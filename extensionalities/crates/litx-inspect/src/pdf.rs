use crate::{Claim, InspectOptions, InspectReport, Preflight, binary::Source, xml};
use liteparse::{LiteParse, LiteParseConfig, stages, types::PdfInput};
use serde::Serialize;
use std::path::Path;

const MAX_PAGES: usize = 4096;
const MAX_PREFLIGHT: usize = 64;
const MAX_TEXT_BYTES: usize = 64 << 20;

#[derive(Debug, Serialize)]
pub struct PdfAnalysis {
    pub basis: &'static str,
    pub metadata: liteparse::DocumentMetadata,
    pub geometry: Vec<PdfGeometry>,
    pub complexity: Vec<stages::PageComplexityStats>,
    pub text_complete: bool,
}

#[derive(Debug, Serialize)]
pub struct PdfGeometry {
    pub page_number: usize,
    pub width_points: f32,
    pub height_points: f32,
}

pub(crate) fn inspect(path: &Path, report: &mut InspectReport, options: &InspectOptions) {
    // Keep the synchronous API usable from callers already inside a Tokio runtime.
    let result = std::thread::scope(|scope| {
        scope
            .spawn(|| inner(path, report, options).map_err(|e| e.to_string()))
            .join()
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => report.error("pdf", error),
        Err(_) => report.error("pdf", "upstream PDF inspection panicked"),
    }
}

fn inner(path: &Path, report: &mut InspectReport, options: &InspectOptions) -> xml::Result<()> {
    let mut source = Source::open(path)?;
    // Byte input prevents extension-driven conversion or subprocesses upstream.
    let input = PdfInput::Bytes(source.range(0, source.len as usize)?);
    {
        let library = stages::Library::init();
        let document = stages::open(&library, &input, None, &[])?;
        let count = document.page_count().max(0) as usize;
        report.counts.pages = Some(count);
        for (tag, key) in [
            ("Title", "title"),
            ("Author", "author"),
            ("Creator", "creator"),
            ("Producer", "producer"),
            ("Subject", "subject"),
            ("Keywords", "keywords"),
        ] {
            if let Some(value) = document.meta_text(tag).filter(|v| !v.is_empty()) {
                report.claims.push(Claim {
                    key: key.into(),
                    value,
                    source: "container_metadata",
                });
            }
        }
        report.pdf = Some(PdfAnalysis {
            basis: "upstream",
            metadata: stages::document_metadata(&input, &document),
            geometry: vec![],
            complexity: vec![],
            text_complete: count <= MAX_PAGES,
        });
        if count > MAX_PAGES {
            report.error(
                "pdf_text",
                "PDF page scan cap exceeded; character counts are partial",
            );
        }
        let mut text_bytes = 0usize;
        for number in 1..=count.min(MAX_PAGES) {
            let extracted = stages::extract(
                &document,
                &stages::ExtractRequest {
                    target_pages: Some(&[number as u32]),
                    max_pages: 1,
                    ..Default::default()
                },
            );
            let pages = match extracted {
                Ok(extracted) => extracted.pages,
                Err(error) => {
                    report.pdf.as_mut().unwrap().text_complete = false;
                    report.error("pdf_text", format!("page {number}: {error}"));
                    continue;
                }
            };
            for page in stages::project(pages) {
                report.pdf.as_mut().unwrap().geometry.push(PdfGeometry {
                    page_number: number,
                    width_points: page.page_width,
                    height_points: page.page_height,
                });
                text_bytes = text_bytes.saturating_add(page.text.len());
                if text_bytes > MAX_TEXT_BYTES {
                    report.pdf.as_mut().unwrap().text_complete = false;
                    report.error(
                        "pdf_text",
                        "PDF cumulative text cap exceeded; character counts are partial",
                    );
                    break;
                }
                report.append_text(&page.text, options.max_chars);
            }
            if text_bytes > MAX_TEXT_BYTES {
                break;
            }
        }
        if options.thumbnail.is_some() && count > 0 {
            let rendered = (|| -> xml::Result<Vec<liteparse::ScreenshotResult>> {
                if !(1..=4096).contains(&options.thumbnail_max_edge) {
                    return Err("thumbnail edge must be between 1 and 4096".into());
                }
                let page = document.page(0)?;
                let long_edge = page.width().max(page.height()) * page.user_unit();
                if !long_edge.is_finite() || long_edge <= 0.0 {
                    return Err("invalid PDF page geometry for thumbnail".into());
                }
                let dpi = 72.0 * (options.thumbnail_max_edge as f32 / long_edge).min(1.0);
                Ok(stages::screenshots(
                    &document,
                    Some(&[1]),
                    &stages::ScreenshotOptions {
                        dpi,
                        detect_rects: false,
                        render_form_fields: false,
                        continue_on_page_error: false,
                    },
                )?)
            })();
            match rendered {
                Ok(pages) => {
                    if let Some(first) = pages.into_iter().next() {
                        crate::thumbnail::write(
                            std::io::Cursor::new(first.image_bytes),
                            "rendered",
                            options,
                            report,
                        );
                    }
                }
                Err(error) => report.error("thumbnail", error),
            }
        }
    } // Release the PDFium lock before the public is_complex API reopens the input.
    if options.max_units > 0 {
        if options.max_units > MAX_PREFLIGHT
            && report.counts.pages.is_some_and(|n| n > MAX_PREFLIGHT)
        {
            report.error("pdf_preflight", "PDF preflight capped at 64 pages");
        }
        let parser = LiteParse::new(LiteParseConfig {
            quiet: true,
            ocr_enabled: false,
            max_pages: options.max_units.min(MAX_PREFLIGHT),
            ..Default::default()
        });
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        match runtime.block_on(parser.is_complex(input)) {
            Ok(stats) => {
                for page in &stats {
                    report.preflight.push(Preflight {
                        page_number: page.page_number,
                        unit_kind: "page",
                        needs_ocr: page.needs_ocr,
                        reasons: page.reasons.iter().map(|r| r.as_str().into()).collect(),
                        confidence: "deterministic",
                        coverage_available: true,
                    });
                }
                report.pdf.as_mut().unwrap().complexity = stats;
            }
            Err(error) => report.error("pdf_preflight", error),
        }
    }
    Ok(())
}

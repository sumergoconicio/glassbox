use crate::{Claim, InspectOptions, InspectReport, Preflight, binary::Source, xml};
use liteparse::{LiteParse, LiteParseConfig, stages, types::PdfInput};
use serde::Serialize;
use std::path::Path;

const MAX_PAGES: usize = 4096;
const MAX_PREFLIGHT: usize = 64;
const MAX_TEXT_BYTES: usize = 64 << 20;

#[derive(Debug, Serialize)]
pub struct PdfAnalysis {
    pub digital_signal: &'static str,
    pub identification_pages: Vec<PdfPageSignal>,
    pub preview_complete: bool,
    pub basis: &'static str,
    pub metadata: liteparse::DocumentMetadata,
    pub geometry: Vec<PdfGeometry>,
    pub complexity: Vec<stages::PageComplexityStats>,
    pub text_complete: bool,
}

#[derive(Debug, Serialize)]
pub struct PdfPageSignal {
    pub page_number: usize,
    pub text_layer: &'static str,
    pub native_chars: usize,
    pub needs_ocr: bool,
    pub ocr_status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct PdfGeometry {
    pub page_number: usize,
    pub width_points: f32,
    pub height_points: f32,
}

pub(crate) fn inspect(
    path: &Path,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut crate::images::OcrSession,
) {
    let bytes = (|| -> xml::Result<Vec<u8>> {
        let mut source = Source::open(path)?;
        source.range(0, source.len as usize)
    })();
    match bytes {
        Ok(bytes) => inspect_bytes(bytes, report, options, ocr),
        Err(error) => report.error("pdf", error),
    }
}

pub(crate) fn inspect_bytes(
    bytes: Vec<u8>,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut crate::images::OcrSession,
) {
    // Keep the synchronous API usable from callers already inside a Tokio runtime.
    let result = std::thread::scope(|scope| {
        scope
            .spawn(|| inner(bytes, report, options, ocr).map_err(|e| e.to_string()))
            .join()
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => report.error("pdf", error),
        Err(_) => report.error("pdf", "upstream PDF inspection panicked"),
    }
}

fn inner(
    bytes: Vec<u8>,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut crate::images::OcrSession,
) -> xml::Result<()> {
    // Byte input prevents extension-driven conversion or subprocesses upstream.
    let input = PdfInput::Bytes(bytes);
    let scan_limit = options.max_pages.unwrap_or(MAX_PAGES).min(MAX_PAGES);
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
            digital_signal: "not_sampled",
            identification_pages: vec![],
            preview_complete: true,
            basis: "upstream",
            metadata: stages::document_metadata(&input, &document),
            geometry: vec![],
            complexity: vec![],
            text_complete: count <= scan_limit,
        });
        if count > MAX_PAGES && !options.identify {
            report.error(
                "pdf_text",
                "PDF page scan cap exceeded; character counts are partial",
            );
        }
        let mut text_bytes = 0usize;
        let mut markdown_pages = Vec::new();
        let mut ocr_pages = std::collections::HashMap::new();
        for number in 1..=count.min(scan_limit) {
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
                    report.pdf.as_mut().unwrap().preview_complete = false;
                    report.error("pdf_text", format!("page {number}: {error}"));
                    continue;
                }
            };
            let stats = if options.identify {
                pages
                    .first()
                    .map(|page| stages::page_complexity(&document, page))
                    .transpose()?
            } else {
                None
            };
            markdown_pages.extend(stages::project_for_markdown(pages.clone()));
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
                let mut text = page.text;
                if let Some(stats) = stats.as_ref() {
                    let native_chars = text.chars().filter(|c| !c.is_whitespace()).count();
                    let text_layer = match (native_chars > 0, stats.full_page_image) {
                        (true, false) => "digital",
                        (true, true) => "scan_with_text_layer",
                        (false, true) => "scanned",
                        (false, false) => "no_text_layer",
                    };
                    let mut ocr_status = "not_needed";
                    if stats.needs_ocr {
                        let recognized = (|| -> xml::Result<(crate::OcrProbe, String)> {
                            let source_page = document.page((number - 1) as i32)?;
                            let edge = source_page.width().max(source_page.height())
                                * source_page.user_unit();
                            if !edge.is_finite() || edge <= 0.0 {
                                return Err("invalid PDF OCR geometry".into());
                            }
                            let rendered = stages::screenshots(
                                &document,
                                Some(&[number as u32]),
                                &stages::ScreenshotOptions {
                                    dpi: (72.0 * 2048.0 / edge).min(150.0),
                                    detect_rects: false,
                                    render_form_fields: false,
                                    continue_on_page_error: false,
                                },
                            )?;
                            let image = image::load_from_memory(
                                &rendered.first().ok_or("missing OCR raster")?.image_bytes,
                            )?;
                            crate::images::probe(&image, ocr)
                        })();
                        match recognized {
                            Ok((probe, recognized)) => {
                                ocr_status = if probe.word_count > 0 {
                                    "text_detected"
                                } else {
                                    "no_text_detected"
                                };
                                if probe.word_count > 0 {
                                    text = recognized;
                                    ocr_pages.insert(number, text.clone());
                                    report.text.origin = "native_and_ocr".into();
                                }
                            }
                            Err(error) => {
                                ocr_status = "failed";
                                report.pdf.as_mut().unwrap().preview_complete = false;
                                report.error("pdf_ocr", format!("page {number}: {error}"));
                            }
                        }
                    }
                    report.preflight.push(Preflight {
                        page_number: number,
                        unit_kind: "page",
                        needs_ocr: stats.needs_ocr,
                        reasons: stats.reasons.iter().map(|r| r.as_str().into()).collect(),
                        confidence: "deterministic",
                        coverage_available: true,
                    });
                    let pdf = report.pdf.as_mut().unwrap();
                    pdf.identification_pages.push(PdfPageSignal {
                        page_number: number,
                        text_layer,
                        native_chars,
                        needs_ocr: stats.needs_ocr,
                        ocr_status,
                    });
                    pdf.complexity.push(stats.clone());
                }
                report.append_text(&text, options.max_chars);
            }
            if text_bytes > MAX_TEXT_BYTES {
                break;
            }
        }
        let signals = stages::document_signals(&markdown_pages, true);
        for page in &markdown_pages {
            let (body, basis) = if let Some(text) = ocr_pages.get(&page.page_number) {
                (crate::markdown::escape(text), "ocr_text_fallback")
            } else {
                let blocks = stages::extract_blocks(
                    page,
                    &signals,
                    &stages::BlockOptions {
                        outline: &[],
                        image_mode: liteparse::config::ImageMode::Off,
                        keep_headers_footers: true,
                    },
                );
                (
                    stages::render_page_markdown(page, blocks.as_deref()),
                    "inferred_layout",
                )
            };
            crate::markdown::append(
                report,
                &format!("## Page {}\n\n{body}\n\n", page.page_number),
                options,
                basis,
            );
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
    if options.identify {
        let pdf = report.pdf.as_mut().unwrap();
        let digital = pdf.identification_pages.iter().any(|p| p.native_chars > 0);
        let scanned = pdf
            .identification_pages
            .iter()
            .any(|p| matches!(p.text_layer, "scanned" | "scan_with_text_layer"));
        pdf.digital_signal = match (digital, scanned) {
            (true, true) => "mixed",
            (true, false) => "digital",
            (false, true) => "scanned",
            _ => "unknown",
        };
    } else if options.max_units > 0 {
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
    if report.counts.pages.is_some_and(|n| n > scan_limit) {
        report.text.truncated = true;
    }
    Ok(())
}

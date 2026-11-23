//! Module: glassbox-inspect PDF native signals.
//! File: pdf.rs.
//! Responsibility: Inspect PDF structure and render explicit thumbnails through LiteParse stages.
use crate::{Claim, InspectOptions, InspectReport, Preflight, binary::Source, xml};
use liteparse::{stages, types::PdfInput};
use serde::Serialize;
use std::path::Path;

/// LITX-AUD-010(b): the native page hard cap is always enforced; tests inject
/// a smaller seam so the cap path runs without a 4097-page document.
#[cfg(not(test))]
const MAX_PAGES: usize = 4096;
#[cfg(test)]
const MAX_PAGES: usize = 8;
const MAX_PREFLIGHT: usize = 64;
/// LITX-AUD-014: cumulative cap on decoded native + OCR text bytes. The test
/// seam exercises the cap path without allocating a 64 MiB document.
#[cfg(not(test))]
const MAX_TEXT_BYTES: usize = 64 << 20;
#[cfg(test)]
const MAX_TEXT_BYTES: usize = 32;
/// LITX-AUD-014 follow-up: aggregate budget for OCR rasters retained while
/// the PDFium lock is held so probes can run unlocked. Peak retained memory
/// is this budget plus the single transient render being charged (each
/// render is already bounded individually by the 2048px DPI normalization).
#[cfg(not(test))]
const MAX_OCR_STAGING_BYTES: usize = 128 << 20;
#[cfg(test)]
const MAX_OCR_STAGING_BYTES: usize = 1 << 20;
/// Upstream's built-in ceiling on one OCR raster's long edge in pixels
/// (`MAX_OCR_RENDER_LONG_EDGE_PX` in the liteparse OCR render path). The
/// caller cap cannot raise it, so pre-render sizing bounds use this value.
const BUILTIN_OCR_LONG_EDGE_PX: u32 = 4096;
/// COND-08: retained character cap for one allowlisted document `/Info`
/// value. The Info dictionary is author-supplied, so an oversized value is
/// clipped and disclosed as partial instead of retained whole; pdfium's
/// reader still reads the raw value transiently, bounded by source size.
const MAX_INFO_VALUE_CHARS: usize = 1024;
/// BLK-09: ceiling for a thumbnail raster's long edge in pixels when the
/// caller's requested edge is unavailable. The artifact writer downsamples to
/// the requested edge after decode, and its decoder accepts 16,384-pixel
/// inputs, so 4096 keeps every thumbnail render bounded and decodable.
const MAX_THUMBNAIL_RASTER_EDGE_PX: u32 = 4096;

/// Charge one retained raster against the shared staging budget with checked
/// arithmetic, billing the retained allocation (`Vec::capacity`), never the
/// payload length: the queue keeps the whole buffer alive. The rejected
/// raster is dropped by the caller.
fn charge_ocr_staging(remaining: &mut usize, raster: &Vec<u8>) -> Result<(), String> {
    match remaining.checked_sub(raster.capacity()) {
        Some(next) => {
            *remaining = next;
            Ok(())
        }
        None => Err(format!(
            "PDF OCR raster staging budget of {MAX_OCR_STAGING_BYTES} bytes exhausted"
        )),
    }
}

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

/// Reads PDF geometry, security, bounded object-kind structure, and `/Info`.
///
/// Accepts a PDF path, destination metadata, and optional thumbnail controls.
/// Returns unit after native structural inspection. Library/open failures are
/// returned; page-local failures and traversal caps retain qualified `unknown`
/// signals. Per-page signals stop at the native page-scan cap; the page-count
/// fact stays exact and the report turns partial. Allowlisted `/Info`
/// dictionary values are published as bounded embedded properties, clipped and
/// disclosed past the character cap. No text, extraction, page-complexity, or
/// OCR stage is called.
pub(crate) fn metadata(
    path: &Path,
    metadata: &mut crate::reports::DocumentMetadata,
    thumbnail: Option<&crate::reports::ThumbnailOptions>,
) -> xml::Result<()> {
    use crate::reports::{
        Completeness, DiagnosticSeverity, DiagnosticStage, EmbeddedMetadataField, EncryptionStatus,
        FactSource, NativePageSignal, NativeUnit, OcrRecommendation, PdfDocumentSignals,
        PdfObjectKind, QualifiedCount, SupportStatus, TextLayerStatus, Uncertainty,
    };
    use liteparse::{stages, types::PdfInput};
    use std::collections::HashSet;

    let input = PdfInput::Path(path.to_string_lossy().into_owned());
    let result = std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<(), String> {
                let library = stages::Library::init();
                let document = stages::open(&library, &input, None, &[])
                    .map_err(|error| error.to_string())?;
                let page_count = document.page_count().max(0) as usize;
                let revision = document.security_handler_revision();
                metadata.pdf = Some(PdfDocumentSignals {
                    security_handler_revision: (revision >= 0).then_some(revision),
                    permissions: Some(document.permissions()),
                });
                metadata.encryption = if revision >= 0 {
                    EncryptionStatus::EncryptedAccessible
                } else {
                    EncryptionStatus::NotEncrypted
                };
                crate::metadata::add_count(
                    metadata,
                    NativeUnit::PhysicalPage,
                    Some(page_count as u64),
                    FactSource::NativeStructure,
                    Completeness::Exact,
                );
                // COND-08: publish the allowlisted document `/Info` values as
                // bounded, sourced embedded properties. pdfium returns each
                // value or nothing; an absent or empty value stays absent, and
                // a value past the character cap is clipped and disclosed as
                // partial, so no oversized author-supplied payload is retained.
                // The reads stay inside the PDFium-locked thread with the open
                // document; no page, body, or OCR stage is entered.
                let mut info_clipped = false;
                for (tag, field) in [
                    ("Title", EmbeddedMetadataField::Title),
                    ("Author", EmbeddedMetadataField::Author),
                    ("Subject", EmbeddedMetadataField::Subject),
                    ("Keywords", EmbeddedMetadataField::Keywords),
                    ("Creator", EmbeddedMetadataField::Creator),
                    ("Producer", EmbeddedMetadataField::Producer),
                    ("CreationDate", EmbeddedMetadataField::CreatedAt),
                    ("ModDate", EmbeddedMetadataField::ModifiedAt),
                ] {
                    let Some(value) = document.meta_text(tag).filter(|value| !value.is_empty())
                    else {
                        continue;
                    };
                    let (value, completeness) =
                        if value.chars().count() > MAX_INFO_VALUE_CHARS {
                            info_clipped = true;
                            (
                                value.chars().take(MAX_INFO_VALUE_CHARS).collect::<String>(),
                                Completeness::Partial,
                            )
                        } else {
                            (value, Completeness::Exact)
                        };
                    crate::metadata::add_property(metadata, field, value, completeness);
                }
                if info_clipped {
                    crate::metadata::add_diagnostic(
                        metadata,
                        DiagnosticStage::Metadata,
                        DiagnosticSeverity::Warning,
                        "metadata_cap_exceeded",
                        format!(
                            "PDF /Info value character cap of {MAX_INFO_VALUE_CHARS} exceeded"
                        ),
                        None,
                    );
                    crate::metadata::mark_partial(metadata);
                }
                // BLK-09: cap the per-page signal scan; the page-count fact
                // stays exact because it reads the page tree, not the pages.
                let scanned = page_count.min(MAX_PAGES);
                for page_index in 0..scanned {
                    let mut signal = NativePageSignal {
                        page_index: page_index as u64,
                        text_layer: TextLayerStatus::Unknown,
                        ocr_recommended: OcrRecommendation::Unknown,
                        object_counts: Vec::new(),
                        coverage: Completeness::Unavailable,
                        uncertainties: Vec::new(),
                        width_points: None,
                        height_points: None,
                        rotation_quarter_turns: None,
                    };
                    match document.page(page_index as i32) {
                        Ok(page) => {
                            signal.width_points = Some(page.width());
                            signal.height_points = Some(page.height());
                            signal.rotation_quarter_turns = Some(page.rotation());
                            let mut visited = 0usize;
                            let mut seen = HashSet::new();
                            let mut incomplete = false;
                            let mut kinds = [0_u64; 6];
                            let mut stack = (0..page.object_count())
                                .rev()
                                .filter_map(|index| {
                                    let object = page.object(index);
                                    incomplete |= object.is_none();
                                    object.map(|object| (object, 0usize))
                                })
                                .collect::<Vec<_>>();
                            while let Some((object, depth)) = stack.pop() {
                                if visited == 100_000 {
                                    incomplete = true;
                                    break;
                                }
                                if !seen.insert(object.id()) {
                                    continue;
                                }
                                visited += 1;
                                let kind = match format!("{:?}", object.kind()).as_str() {
                                    "Text" => PdfObjectKind::Text,
                                    "Path" => PdfObjectKind::Path,
                                    "Image" => PdfObjectKind::Image,
                                    "Shading" => PdfObjectKind::Shading,
                                    "Form" => PdfObjectKind::Form,
                                    _ => PdfObjectKind::Unknown,
                                };
                                let kind_index = match kind {
                                    PdfObjectKind::Text => 0,
                                    PdfObjectKind::Path => 1,
                                    PdfObjectKind::Image => 2,
                                    PdfObjectKind::Shading => 3,
                                    PdfObjectKind::Form => 4,
                                    _ => 5,
                                };
                                kinds[kind_index] += 1;
                                if kind == PdfObjectKind::Form {
                                    if depth == 16 {
                                        incomplete = true;
                                        continue;
                                    }
                                    match object.form_object_count() {
                                        Some(count) => {
                                            for index in (0..count).rev() {
                                                match object.form_object(index) {
                                                    Some(child) => stack.push((child, depth + 1)),
                                                    None => incomplete = true,
                                                }
                                            }
                                        }
                                        None => incomplete = true,
                                    }
                                }
                            }
                            if incomplete {
                                signal.object_counts.push(QualifiedCount {
                                    unit: NativeUnit::PdfObject(PdfObjectKind::Unknown),
                                    value: None,
                                    source: FactSource::NativeStructure,
                                    completeness: Completeness::Partial,
                                });
                                signal.coverage = Completeness::Partial;
                                signal.uncertainties.push(Uncertainty {
                                    code: "pdf_object_walk_incomplete".into(),
                                    message: "object totals are unavailable after a depth, object, or Form XObject limit/failure".into(),
                                    source: FactSource::NativeStructure,
                                });
                            } else {
                                signal.object_counts.push(QualifiedCount {
                                    unit: NativeUnit::PdfObject(PdfObjectKind::Unknown),
                                    value: Some(visited as u64),
                                    source: FactSource::NativeStructure,
                                    completeness: Completeness::Exact,
                                });
                                for (kind, value) in [
                                    PdfObjectKind::Text,
                                    PdfObjectKind::Path,
                                    PdfObjectKind::Image,
                                    PdfObjectKind::Shading,
                                    PdfObjectKind::Form,
                                    PdfObjectKind::Unknown,
                                ]
                                .into_iter()
                                .zip(kinds)
                                .filter(|(_, value)| *value > 0)
                                {
                                    signal.object_counts.push(QualifiedCount {
                                        unit: NativeUnit::PdfObject(kind),
                                        value: Some(value),
                                        source: FactSource::NativeStructure,
                                        completeness: Completeness::Exact,
                                    });
                                }
                                signal.coverage = Completeness::Exact;
                            }
                        }
                        Err(error) => {
                            signal.object_counts.push(QualifiedCount {
                                unit: NativeUnit::PdfObject(PdfObjectKind::Unknown),
                                value: None,
                                source: FactSource::NativeStructure,
                                completeness: Completeness::Unavailable,
                            });
                            signal.uncertainties.push(Uncertainty {
                                code: "pdf_page_unavailable".into(),
                                message: error.to_string(),
                                source: FactSource::NativeStructure,
                            });
                        }
                    }
                    if signal.coverage != Completeness::Exact {
                        crate::metadata::add_diagnostic(
                            metadata,
                            DiagnosticStage::NativeSignals,
                            DiagnosticSeverity::Warning,
                            "pdf_page_signals_partial",
                            "native object signals are incomplete; text-layer and OCR advice remain unknown",
                            Some(page_index as u64),
                        );
                        crate::metadata::mark_partial(metadata);
                    }
                    metadata.native_page_signals.push(signal);
                }
                if page_count > scanned {
                    // BLK-09: report the unscanned remainder as a warning and
                    // keep the partial state visible; the exact page count
                    // above already tells consumers how much was not scanned.
                    crate::metadata::add_diagnostic(
                        metadata,
                        DiagnosticStage::NativeSignals,
                        DiagnosticSeverity::Warning,
                        "pdf_page_signals_capped",
                        format!(
                            "page signals capped at {MAX_PAGES} pages; remaining pages are unscanned"
                        ),
                        None,
                    );
                    crate::metadata::mark_partial(metadata);
                }
                metadata.extraction_support = SupportStatus::Supported;
                crate::metadata::add_provenance(
                    metadata,
                    "pdfium-objects",
                    "document-security-page-geometry-and-object-kinds",
                    FactSource::NativeStructure,
                );
                Ok(())
            })
            .join()
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => return Err("upstream PDF metadata inspection panicked".into()),
    }
    if let Some(options) = thumbnail {
        let result = thumbnail_bytes(path, options.max_edge).and_then(|bytes| {
            crate::metadata::write_thumbnail(
                std::io::Cursor::new(bytes),
                options,
                FactSource::GeneratedArtifact,
            )
        });
        crate::metadata::thumbnail_result(metadata, result);
    }
    Ok(())
}

/// Renders the first PDF page only when the caller explicitly requests it.
///
/// Accepts a PDF path and the requested thumbnail edge in pixels. Returns PNG
/// bytes from the native renderer with the raster long edge capped at the
/// requested edge. Open, geometry, and render failures are returned; no OCR
/// or text extraction occurs.
pub(crate) fn thumbnail_bytes(path: &Path, max_edge: u32) -> xml::Result<Vec<u8>> {
    thumbnail_input(
        liteparse::types::PdfInput::Path(path.to_string_lossy().into_owned()),
        Some(max_edge),
    )
}

/// Renders an embedded first-page PDF preview for an explicit thumbnail.
///
/// Accepts owned PDF bytes. Returns PNG bytes from the native renderer with
/// the raster long edge capped at the internal thumbnail edge. Open, geometry,
/// and render failures are returned; no OCR or text extraction occurs.
pub(crate) fn thumbnail_bytes_from(bytes: Vec<u8>) -> xml::Result<Vec<u8>> {
    thumbnail_input(liteparse::types::PdfInput::Bytes(bytes), None)
}

/// Renders one PDF input's first page for the explicit thumbnail path.
///
/// Accepts a path- or byte-backed PDF input and an optional requested edge.
/// Returns PNG bytes. BLK-09: the raster's actual dimensions are capped
/// before pdfium allocates the bitmap — the render dpi is derived from the
/// first page's geometry and `/UserUnit`, so a freakishly large page renders
/// at the thumbnail edge instead of an unbounded 72-dpi bitmap. Library,
/// open, page, geometry, and render failures are returned; the artifact writer
/// applies its own decode bounds.
fn thumbnail_input(
    input: liteparse::types::PdfInput,
    max_edge: Option<u32>,
) -> xml::Result<Vec<u8>> {
    let result = std::thread::scope(|scope| {
        scope
            .spawn(|| -> Result<Vec<u8>, String> {
                let library = liteparse::stages::Library::init();
                let document = liteparse::stages::open(&library, &input, None, &[])
                    .map_err(|error| error.to_string())?;
                let page = document.page(0).map_err(|error| error.to_string())?;
                let long_edge = page.width().max(page.height()) * page.user_unit();
                if !long_edge.is_finite() || long_edge <= 0.0 {
                    return Err("invalid PDF page geometry for thumbnail".into());
                }
                let cap = max_edge.map_or(MAX_THUMBNAIL_RASTER_EDGE_PX, |edge| {
                    edge.clamp(1, MAX_THUMBNAIL_RASTER_EDGE_PX)
                }) as f32;
                let dpi = 72.0 * (cap / long_edge).min(1.0);
                let mut pages = liteparse::stages::screenshots(
                    &document,
                    Some(&[1]),
                    &liteparse::stages::ScreenshotOptions {
                        dpi,
                        detect_rects: false,
                        render_form_fields: false,
                        continue_on_page_error: false,
                    },
                )
                .map_err(|error| error.to_string())?;
                let page = pages.pop().ok_or("PDF has no first page to render")?;
                Ok(page.image_bytes)
            })
            .join()
    });
    match result {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err("upstream PDF thumbnail rendering panicked".into()),
    }
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

/// Inspects PDF bytes with the production page-complexity implementation.
/// Accepts owned PDF bytes, the destination report, inspection limits, and the
/// reusable OCR session. Returns `Ok(())` after updating the report, or a
/// document-open/setup error; page extraction and complexity failures are
/// recorded as partial-output diagnostics in the report.
fn inner(
    bytes: Vec<u8>,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut crate::images::OcrSession,
) -> xml::Result<()> {
    inner_with_complexity(bytes, report, options, ocr, |document, page| {
        stages::page_complexity(document, page).map_err(Into::into)
    })
}

/// Predicts the next page the upstream OCR gate will rasterize.
///
/// Accepts an open document, the extracted pages, and the scan start index.
/// Returns the index of the first page from `start` whose public complexity
/// stage reports `needs_ocr`, `None` when no remaining page does, or the
/// complexity failure. The public stage is the same gate `render_for_ocr`
/// applies internally, so the prediction never changes page selection.
fn next_ocr_page(
    document: &stages::Document<'_>,
    pages: &[liteparse::types::Page],
    start: usize,
) -> Result<Option<usize>, String> {
    for (index, page) in pages.iter().enumerate().skip(start) {
        if stages::page_complexity(document, page)
            .map_err(|error| format!("page {}: {error}", page.page_number))?
            .needs_ocr
        {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// Computes a reliable pre-render allocation bound for one OCR raster.
///
/// Accepts the page's viewport width and height in points and the configured
/// long-edge cap in pixels. Returns the upper bound on the rendered
/// grayscale pixel buffer's bytes, or an error for geometry that cannot be
/// bounded. The bound mirrors the public render stage: the effective dpi is
/// the default dpi lowered by the caller cap and the upstream built-in
/// long-edge ceiling, grayscale pixels cost one byte each, and each edge
/// keeps two pixels of pdfium rounding headroom.
fn ocr_raster_bound(width_pt: f32, height_pt: f32, max_long_edge: u32) -> Result<usize, String> {
    if !width_pt.is_finite() || !height_pt.is_finite() || width_pt <= 0.0 || height_pt <= 0.0 {
        return Err("invalid PDF OCR geometry".into());
    }
    let long_pt = width_pt.max(height_pt);
    let short_pt = width_pt.min(height_pt);
    let cap_px = if max_long_edge > 0 {
        max_long_edge.min(BUILTIN_OCR_LONG_EDGE_PX)
    } else {
        BUILTIN_OCR_LONG_EDGE_PX
    } as f32;
    let effective_dpi = liteparse::config::DEFAULT_DPI.min(cap_px * 72.0 / long_pt);
    let long_px = long_pt * effective_dpi / 72.0;
    let short_px = long_px * short_pt / long_pt;
    let edge = |px: f32| px.ceil() as usize + 2;
    Ok(edge(long_px).saturating_mul(edge(short_px)))
}

/// Extracts selected physical PDF pages through the approved public-stage OCR recipe.
///
/// Accepts owned PDF bytes, a destination report, v2-mapped legacy controls,
/// and the worker-owned OCR session with its active file lease.
/// Returns unit after native extraction, optional local OCR, and projection. Open,
/// selection, extraction, model, staging, recognition, and merge failures retain
/// available native content and become diagnostics. No model download is attempted.
fn extract_v2(
    bytes: Vec<u8>,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut crate::images::OcrSession,
) -> xml::Result<()> {
    use liteparse::extract::ExtractionOutputOptions;
    use liteparse::ocr::OcrImage;
    use liteparse::ocr::tesseract::TesseractOcrEngine;
    use liteparse::ocr_merge::OcrRaster;
    use std::sync::Arc;

    let selection = options
        .native_selection
        .as_ref()
        .expect("v2 PDF extraction requires selection")
        .clone();
    let input = PdfInput::Bytes(bytes);
    let (mut pages, rasters, tessdata, language) = {
        let library = stages::Library::init();
        let document = stages::open(&library, &input, None, &[])?;
        let count = document.page_count().max(0) as usize;
        report.counts.pages = Some(count);
        if count > MAX_PAGES {
            return Err("PDF page cap exceeded".into());
        }
        let plan = crate::selection::begin(
            report,
            &selection,
            crate::reports::NativeUnit::PhysicalPage,
            count as u64,
        )?;
        let selected: Vec<u32> = plan
            .indices()
            .into_iter()
            .map(|index| index as u32)
            .collect();
        let mut extracted = stages::extract(
            &document,
            &stages::ExtractRequest {
                target_pages: Some(&selected),
                max_pages: selected.len(),
                output: ExtractionOutputOptions {
                    continue_on_page_error: true,
                    emit_word_boxes: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )?;
        for error in &extracted.page_errors {
            report.error(
                "pdf_text",
                format!("page {}: {}", error.page_number, error.message),
            );
        }
        report.pdf = Some(PdfAnalysis {
            digital_signal: "selected_pages",
            identification_pages: Vec::new(),
            preview_complete: extracted.page_errors.is_empty(),
            basis: "upstream-public-stages",
            metadata: stages::document_metadata(&input, &document),
            geometry: Vec::new(),
            complexity: Vec::new(),
            text_complete: extracted.page_errors.is_empty(),
        });
        let mut rasters: Vec<OcrRaster> = Vec::new();
        let mut tessdata = None;
        let mut language = None;
        if options.identify && !extracted.pages.is_empty() {
            // COND-10: normalize the requested language once. The recognizer
            // and the reported OCR provenance then share one canonical model
            // expression, so an alias such as `en` initializes `eng` and is
            // reported as `eng`. An invalid expression or a missing model
            // fails here and keeps the established partial semantics below:
            // native text is retained and no OCR claim is fabricated.
            let resolved =
                crate::images::canonical_language(&options.ocr_language).and_then(|canonical| {
                    crate::images::local_tessdata(&canonical)
                        .map(|directory| (canonical, directory))
                });
            match resolved {
                Ok((canonical, directory)) => {
                    // BLK-09: stage OCR rasters one page per round. Each round
                    // reserves the shared staging budget from a geometry-derived
                    // bound BEFORE the render allocates, so a page that cannot
                    // fit is never rendered and peak retained raster memory
                    // stays inside the budget plus the single in-flight render.
                    // Prediction reuses the same public complexity stage that
                    // upstream's internal gate applies, so page selection,
                    // ordering, and merge policies are unchanged.
                    let round = stages::OcrRenderOptions {
                        max_rasters: 1,
                        max_long_edge: Some(options.ocr_raster_long_edge),
                        grayscale: true,
                        continue_on_page_error: false,
                        reflatten_pages: extracted.flattened_page_numbers.iter().copied().collect(),
                        ..Default::default()
                    };
                    let mut start = 0usize;
                    while start < extracted.pages.len() {
                        let target = match next_ocr_page(&document, &extracted.pages, start) {
                            Ok(Some(index)) => index,
                            Ok(None) => break,
                            Err(error) => {
                                report.pdf.as_mut().unwrap().text_complete = false;
                                report.error("pdf_ocr", format!("{error}; native text retained"));
                                break;
                            }
                        };
                        let bound = match ocr_raster_bound(
                            extracted.pages[target].page_width,
                            extracted.pages[target].page_height,
                            options.ocr_raster_long_edge,
                        ) {
                            Ok(bound) => bound,
                            Err(error) => {
                                report.pdf.as_mut().unwrap().text_complete = false;
                                report.error(
                                    "pdf_ocr",
                                    format!(
                                        "page {}: {error}; native text retained",
                                        extracted.pages[target].page_number
                                    ),
                                );
                                break;
                            }
                        };
                        if !ocr.reserve_staging(bound) {
                            report.pdf.as_mut().unwrap().text_complete = false;
                            report.error(
                                "pdf_ocr",
                                format!(
                                    "page {}: PDF OCR raster staging budget of {MAX_OCR_STAGING_BYTES} bytes exhausted; native text retained",
                                    extracted.pages[target].page_number
                                ),
                            );
                            break;
                        }
                        match stages::render_for_ocr(&document, &extracted.pages, target, &round) {
                            Ok((rendered, resume)) => {
                                // The bound came from the same geometry the
                                // renderer uses, so this top-up only fires when
                                // the actual allocation exceeded it; a raster
                                // that still cannot be charged is dropped rather
                                // than retained unaccounted. A charge above the
                                // actual capacity stays reserved until the final
                                // release, which is always safe.
                                for raster in rendered {
                                    let capacity = match &raster.image {
                                        OcrImage::Pixels { data, .. } => data.capacity(),
                                        OcrImage::Encoded { image, .. } => image.data.capacity(),
                                    };
                                    if capacity > bound && !ocr.reserve_staging(capacity - bound) {
                                        report.pdf.as_mut().unwrap().text_complete = false;
                                        report.error(
                                            "pdf_ocr",
                                            format!(
                                                "page {}: PDF OCR raster staging budget of {MAX_OCR_STAGING_BYTES} bytes exhausted; native text retained",
                                                raster.page_number
                                            ),
                                        );
                                        continue;
                                    }
                                    rasters.push(raster);
                                }
                                start = resume.max(target + 1);
                            }
                            Err(error) => {
                                report.pdf.as_mut().unwrap().text_complete = false;
                                report.error("pdf_ocr", format!("{error}; native text retained"));
                                break;
                            }
                        }
                    }
                    tessdata = Some(directory);
                    language = Some(canonical);
                }
                Err(error) => {
                    report.pdf.as_mut().unwrap().text_complete = false;
                    report.error("pdf_ocr", format!("{error}; native text retained"));
                }
            }
        }
        // Release the process-global PDFium lock before async recognition.
        drop(document);
        (
            std::mem::take(&mut extracted.pages),
            rasters,
            tessdata,
            language,
        )
    };
    let recognition = if rasters.is_empty() {
        None
    } else {
        ocr.recognition()
    };
    if !rasters.is_empty() && recognition.is_none() {
        report.pdf.as_mut().unwrap().text_complete = false;
        report.error(
            "pdf_ocr",
            "OCR cancelled before recognition; native text retained",
        );
    }
    if recognition.is_some() {
        let directory = tessdata.expect("rendered OCR rasters require a model directory");
        // COND-10: the recognizer receives the canonical language the same
        // resolution produced, so a raw alias such as `en` initializes `eng`
        // instead of failing against a missing non-canonical model.
        let canonical = language
            .as_deref()
            .expect("rendered OCR rasters require a canonical OCR language");
        let engine: Arc<dyn liteparse::ocr::OcrEngine> = Arc::new(TesseractOcrEngine::new(Some(
            directory.to_string_lossy().into_owned(),
        )));
        let outcomes = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(stages::recognize(rasters, engine, canonical, 1));
        for outcome in &outcomes {
            if let Some(error) = &outcome.error {
                report.pdf.as_mut().unwrap().text_complete = false;
                report.error(
                    "pdf_ocr",
                    format!(
                        "page {}: {error}; native text retained",
                        outcome.page_number
                    ),
                );
            }
        }
        let native_pages = pages.clone();
        if let Err(error) = stages::merge_ocr(&mut pages, outcomes, false, true) {
            pages = native_pages;
            report.pdf.as_mut().unwrap().text_complete = false;
            report.error("pdf_ocr", format!("{error}; native text retained"));
        }
    }
    ocr.release_staging();
    let origins: std::collections::HashMap<usize, crate::reports::ContentOrigin> = pages
        .iter()
        .map(|page| {
            let native = page.text_items.iter().any(|item| item.confidence.is_none());
            let ocr = page.text_items.iter().any(|item| item.confidence.is_some());
            (
                page.page_number,
                match (native, ocr) {
                    (true, true) => crate::reports::ContentOrigin::Mixed,
                    (false, true) => crate::reports::ContentOrigin::Ocr,
                    _ => crate::reports::ContentOrigin::Native,
                },
            )
        })
        .collect();
    for page in stages::project(pages) {
        let origin = origins
            .get(&page.page_number)
            .copied()
            .unwrap_or(crate::reports::ContentOrigin::Native);
        let mut unit = crate::selection::text_unit(
            page.page_number as u64,
            crate::reports::NativeUnit::PhysicalPage,
            Some(format!("page {}", page.page_number)),
            page.text,
            origin,
        );
        if matches!(
            origin,
            crate::reports::ContentOrigin::Ocr | crate::reports::ContentOrigin::Mixed
        ) {
            unit.provenance[0].ocr = Some(crate::reports::OcrProvenance {
                engine: "tesseract".into(),
                // COND-10: provenance reports the same canonical language
                // the recognizer used, never the raw requested spelling.
                language: language
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| options.ocr_language.clone()),
                mean_confidence: None,
                raster_long_edge: Some(options.ocr_raster_long_edge),
            });
        }
        report.native_units.push(unit);
    }
    Ok(())
}

/// Inspects PDF bytes using `complexity` for per-page OCR and geometry signals.
/// Accepts the same inputs as [`inner`] plus a callback receiving an open
/// PDFium document and extracted page. Returns `Ok(())` after report assembly
/// or a document-open/setup error; page callback failures become partial-output
/// diagnostics. Tests inject a page-local failure without changing public options.
fn inner_with_complexity<F>(
    bytes: Vec<u8>,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut crate::images::OcrSession,
    complexity: F,
) -> xml::Result<()>
where
    F: Fn(
        &stages::Document<'_>,
        &liteparse::types::Page,
    ) -> xml::Result<stages::PageComplexityStats>,
{
    if options.native_selection.is_some() {
        return extract_v2(bytes, report, options, ocr);
    }
    // Byte input prevents extension-driven conversion or subprocesses upstream.
    let input = PdfInput::Bytes(bytes);
    let scan_limit = options.max_pages.unwrap_or(MAX_PAGES).min(MAX_PAGES);
    let locked = {
        let library = stages::Library::init();
        let document = stages::open(&library, &input, None, &[])?;
        let count = document.page_count().max(0) as usize;
        report.counts.pages = Some(count);
        // LITX-AUD-014: the native default-path preflight keeps the 64-page
        // window of the upstream probe it replaces.
        if !options.identify && options.max_units > MAX_PREFLIGHT && count > MAX_PREFLIGHT {
            report.error("pdf_preflight", "PDF preflight capped at 64 pages");
        }
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
        // LITX-AUD-010(b): the native hard cap is reported during previews
        // and full extraction alike.
        if count > MAX_PAGES {
            report.error(
                "pdf_text",
                "PDF page scan cap exceeded; character counts are partial",
            );
        }
        let mut text_bytes = 0usize;
        let mut text_cap_exhausted = false;
        let mut extract_failed = false;
        let mut markdown_pages = Vec::new();
        // Native text per page, kept for the unlocked phase where OCR may
        // replace it before anything is appended to the report.
        let mut page_texts: Vec<(usize, String)> = Vec::new();
        // Rasters buffered while the PDFium lock is held; the OCR probes
        // themselves run after the lock is released (LITX-AUD-014). Retained
        // capacity is bounded by a shared aggregate budget; once exhausted,
        // no further rasters are rendered at all.
        let mut ocr_jobs: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut staging_budget = MAX_OCR_STAGING_BYTES;
        let mut staging_exhausted = false;
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
                    extract_failed = true;
                    report.error("pdf_text", format!("page {number}: {error}"));
                    continue;
                }
            };
            // LITX-AUD-014: the default (!identify, max_units > 0) preflight
            // computes complexity from the document this inspection already
            // parsed — no second parse, no conversion fallback.
            let preflight_window = !options.identify
                && options.max_units > 0
                && number <= options.max_units.min(MAX_PREFLIGHT);
            let stats = if options.identify || preflight_window {
                match pages
                    .first()
                    .map(|page| complexity(&document, page))
                    .transpose()
                {
                    Ok(stats) => stats,
                    Err(error) => {
                        let pdf = report.pdf.as_mut().unwrap();
                        pdf.text_complete = false;
                        pdf.preview_complete = false;
                        extract_failed = true;
                        report.error("pdf_text", format!("page {number}: {error}"));
                        None
                    }
                }
            } else {
                None
            };
            // Markdown projection has the same plain text as grid projection.
            // Project once, then charge each page before cloning or retaining it.
            let projected_pages = stages::project_for_markdown(pages);
            let mut stop = false;
            for page in projected_pages {
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
                    text_cap_exhausted = true;
                    stop = true;
                    break;
                }
                let text = page.text.clone();
                if let Some(stats) = stats.as_ref() {
                    let native_chars = text.chars().filter(|c| !c.is_whitespace()).count();
                    let text_layer = match (native_chars > 0, stats.full_page_image) {
                        (true, false) => "digital",
                        (true, true) => "scan_with_text_layer",
                        (false, true) => "scanned",
                        (false, false) => "no_text_layer",
                    };
                    let mut ocr_status = "not_needed";
                    if options.identify {
                        if stats.needs_ocr && staging_exhausted {
                            // The shared staging budget is spent: do not render
                            // further rasters that would only be rejected.
                            ocr_status = "failed";
                        } else if stats.needs_ocr {
                            let raster = (|| -> xml::Result<Vec<u8>> {
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
                                Ok(rendered
                                    .into_iter()
                                    .next()
                                    .ok_or("missing OCR raster")?
                                    .image_bytes)
                            })();
                            match raster {
                                Ok(png) => match charge_ocr_staging(&mut staging_budget, &png) {
                                    Ok(()) => {
                                        ocr_status = "pending";
                                        ocr_jobs.push((number, png));
                                    }
                                    Err(message) => {
                                        staging_exhausted = true;
                                        ocr_status = "failed";
                                        let pdf = report.pdf.as_mut().unwrap();
                                        pdf.preview_complete = false;
                                        // Resource-limit rejection leaves the
                                        // extraction incomplete; the rejected
                                        // raster is dropped here.
                                        pdf.text_complete = false;
                                        report
                                            .error("pdf_ocr", format!("page {number}: {message}"));
                                    }
                                },
                                Err(error) => {
                                    ocr_status = "failed";
                                    let pdf = report.pdf.as_mut().unwrap();
                                    pdf.preview_complete = false;
                                    // LITX-AUD-010: a failed OCR attempt leaves the
                                    // extraction incomplete.
                                    pdf.text_complete = false;
                                    report.error("pdf_ocr", format!("page {number}: {error}"));
                                }
                            }
                        }
                        report
                            .pdf
                            .as_mut()
                            .unwrap()
                            .identification_pages
                            .push(PdfPageSignal {
                                page_number: number,
                                text_layer,
                                native_chars,
                                needs_ocr: stats.needs_ocr,
                                ocr_status,
                            });
                    }
                    report.preflight.push(Preflight {
                        page_number: number,
                        unit_kind: "page",
                        needs_ocr: stats.needs_ocr,
                        reasons: stats.reasons.iter().map(|r| r.as_str().into()).collect(),
                        confidence: "deterministic",
                        coverage_available: true,
                    });
                    report.pdf.as_mut().unwrap().complexity.push(stats.clone());
                }
                page_texts.push((number, text));
                markdown_pages.push(page);
            }
            if stop || text_bytes > MAX_TEXT_BYTES {
                break;
            }
        }
        let thumbnail_png = if options.thumbnail.is_some() && count > 0 {
            Some((|| -> xml::Result<Vec<u8>> {
                if !(1..=4096).contains(&options.thumbnail_max_edge) {
                    return Err("thumbnail edge must be between 1 and 4096".into());
                }
                let page = document.page(0)?;
                let long_edge = page.width().max(page.height()) * page.user_unit();
                if !long_edge.is_finite() || long_edge <= 0.0 {
                    return Err("invalid PDF page geometry for thumbnail".into());
                }
                let dpi = 72.0 * (options.thumbnail_max_edge as f32 / long_edge).min(1.0);
                let rendered = stages::screenshots(
                    &document,
                    Some(&[1]),
                    &stages::ScreenshotOptions {
                        dpi,
                        detect_rects: false,
                        render_form_fields: false,
                        continue_on_page_error: false,
                    },
                )?;
                Ok(rendered
                    .into_iter()
                    .next()
                    .ok_or("missing thumbnail raster")?
                    .image_bytes)
            })())
        } else {
            None
        };
        // Release the PDFium lock before OCR probes and thumbnail disk writes
        // (LITX-AUD-014); the whole inspection shares this single parse.
        (
            page_texts,
            ocr_jobs,
            markdown_pages,
            text_bytes,
            text_cap_exhausted,
            extract_failed,
            thumbnail_png,
        )
    };
    // Unlocked phase: OCR probes, text assembly, and thumbnail disk writes run
    // without holding the process-global PDFium lock.
    let (
        mut page_texts,
        ocr_jobs,
        markdown_pages,
        mut text_bytes,
        mut text_cap_exhausted,
        extract_failed,
        thumbnail_png,
    ) = locked;
    let mut ocr_pages = std::collections::HashMap::new();
    let saw_native_text = page_texts.iter().any(|(_, text)| !text.trim().is_empty());
    let mut saw_ocr_text = false;
    for (number, png) in ocr_jobs {
        if text_cap_exhausted {
            // LITX-AUD-014: once the cumulative text cap is spent, stop the
            // queued probes immediately: no OCR work whose output cannot be
            // kept, no duplicate cap errors, and no page left pending. The
            // single cap error was already reported by the phase that
            // exhausted the budget.
            if let Some(entry) = report
                .pdf
                .as_mut()
                .unwrap()
                .identification_pages
                .iter_mut()
                .find(|p| p.page_number == number)
            {
                entry.ocr_status = "failed";
            }
            continue;
        }
        let recognized = (|| -> xml::Result<(crate::OcrProbe, String)> {
            let image = image::load_from_memory(&png)?;
            crate::images::probe(&image, ocr, options)
        })();
        match recognized {
            Ok((probe, recognized)) => {
                if let Some(entry) = report
                    .pdf
                    .as_mut()
                    .unwrap()
                    .identification_pages
                    .iter_mut()
                    .find(|p| p.page_number == number)
                {
                    entry.ocr_status = if probe.word_count > 0 {
                        "text_detected"
                    } else {
                        "no_text_detected"
                    };
                }
                if probe.word_count > 0 {
                    // LITX-AUD-014: OCR output counts towards the cumulative
                    // text cap just like native text.
                    text_bytes = text_bytes.saturating_add(recognized.len());
                    if text_bytes > MAX_TEXT_BYTES {
                        report.pdf.as_mut().unwrap().text_complete = false;
                        report.error(
                            "pdf_text",
                            "PDF cumulative text cap exceeded; character counts are partial",
                        );
                        text_cap_exhausted = true;
                    }
                    if let Some((_, text)) = page_texts.iter_mut().find(|(page, _)| *page == number)
                    {
                        *text = recognized.clone();
                    }
                    ocr_pages.insert(number, recognized);
                    saw_ocr_text = true;
                }
            }
            Err(error) => {
                if let Some(entry) = report
                    .pdf
                    .as_mut()
                    .unwrap()
                    .identification_pages
                    .iter_mut()
                    .find(|p| p.page_number == number)
                {
                    entry.ocr_status = "failed";
                }
                let pdf = report.pdf.as_mut().unwrap();
                pdf.preview_complete = false;
                // LITX-AUD-010: a failed OCR attempt leaves the extraction
                // incomplete even when every page fits the scan limit.
                pdf.text_complete = false;
                report.error("pdf_ocr", format!("page {number}: {error}"));
            }
        }
    }
    // LITX-AUD-019: a pure OCR recovery is "ocr"; only a document with native
    // text alongside OCR text is "native_and_ocr".
    if saw_ocr_text {
        report.text.origin = if saw_native_text {
            "native_and_ocr"
        } else {
            "ocr"
        }
        .into();
    }
    for (_, text) in &page_texts {
        report.append_text(text, options.max_chars);
    }
    // LITX-AUD-010/014: a failed OCR attempt, a failed page extraction, or
    // cumulative text-cap exhaustion also marks the text payload partial.
    // Applied after the appends, which recompute text.truncated from the
    // character budget on every append.
    if text_cap_exhausted
        || extract_failed
        || report
            .pdf
            .as_ref()
            .unwrap()
            .identification_pages
            .iter()
            .any(|p| p.ocr_status == "failed")
    {
        report.text.truncated = true;
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
    if let Some(result) = thumbnail_png {
        match result {
            Ok(png) => {
                crate::thumbnail::write(std::io::Cursor::new(png), "rendered", options, report)
            }
            Err(error) => report.error("thumbnail", error),
        }
    }
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
    }
    if report.counts.pages.is_some_and(|n| n > scan_limit) {
        report.text.truncated = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum PageSpec<'a> {
        Empty,
        /// Empty page with a custom MediaBox for geometry-sensitive tests.
        EmptySized {
            width: f32,
            height: f32,
        },
        Text(&'a str),
        Scan {
            jpeg: &'a [u8],
            width: u32,
            height: u32,
        },
    }

    fn synthetic_pdf(pages: &[PageSpec]) -> Vec<u8> {
        synthetic_pdf_sized(600.0, 800.0, pages)
    }

    /// Same assembly as [`synthetic_pdf`] with a custom default MediaBox;
    /// [`PageSpec::EmptySized`] overrides the box for a single page.
    fn synthetic_pdf_sized(box_width: f32, box_height: f32, pages: &[PageSpec]) -> Vec<u8> {
        let mut objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            Vec::new(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        let mut kids = Vec::new();
        for page in pages {
            let (media_width, media_height) = match page {
                PageSpec::EmptySized { width, height } => (*width, *height),
                _ => (box_width, box_height),
            };
            let (content, resources): (Vec<u8>, String) = match page {
                PageSpec::Empty | PageSpec::EmptySized { .. } => (Vec::new(), "<< >>".to_string()),
                PageSpec::Text(text) => (
                    format!("BT /F1 20 Tf 50 700 Td ({text}) Tj ET").into_bytes(),
                    "<< /Font << /F1 3 0 R >> >>".to_string(),
                ),
                PageSpec::Scan {
                    jpeg,
                    width,
                    height,
                } => {
                    let mut image = format!(
                        "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
                        jpeg.len()
                    )
                    .into_bytes();
                    image.extend_from_slice(jpeg);
                    image.extend_from_slice(b"\nendstream");
                    objects.push(image);
                    let image_object = objects.len();
                    (
                        b"q 600 0 0 800 0 0 cm /Scan Do Q".to_vec(),
                        format!("<< /XObject << /Scan {image_object} 0 R >> >>"),
                    )
                }
            };
            let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
            stream.extend_from_slice(&content);
            stream.extend_from_slice(b"\nendstream");
            objects.push(stream);
            let content_object = objects.len();
            objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {media_width} {media_height}] /Resources {resources} /Contents {content_object} 0 R >>").into_bytes());
            kids.push(objects.len());
        }
        objects[1] = format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.iter()
                .map(|n| format!("{n} 0 R"))
                .collect::<Vec<_>>()
                .join(" "),
            pages.len()
        )
        .into_bytes();
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let start = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in &offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    fn write_temp_pdf(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "litx-pdf-caps-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn noisy_jpeg(width: u32, height: u32) -> (Vec<u8>, u32, u32) {
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut pixels = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..width.saturating_mul(height).saturating_mul(3) {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            pixels.push((seed >> 33) as u8);
        }
        let image = image::RgbImage::from_raw(width, height, pixels).unwrap();
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        (jpeg.into_inner(), width, height)
    }

    #[test]
    fn page_count_hard_cap_emits_a_pdf_text_error_during_full_extraction() {
        // LITX-AUD-010(b): the native page cap must be reported even when no
        // explicit max_pages limit is set. The test seam lowers the cap to 8.
        let path = write_temp_pdf(
            "many.pdf",
            &synthetic_pdf(&vec![PageSpec::Empty; MAX_PAGES + 1]),
        );
        let options = InspectOptions {
            max_pages: None,
            max_units: 0,
            ..Default::default()
        };
        let report = crate::extract_transitional(&path, &options);
        assert_eq!(report.counts.pages, Some(MAX_PAGES + 1));
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.stage == "pdf_text" && e.message.contains("cap")),
            "{:?}",
            report.errors
        );
        assert!(!report.pdf.as_ref().unwrap().text_complete);
        assert!(report.text.truncated);
    }

    #[test]
    fn native_text_under_the_cumulative_cap_keeps_the_report_complete() {
        // Under the 32-byte test seam nothing is marked partial.
        let path = write_temp_pdf(
            "under-cap.pdf",
            &synthetic_pdf(&[PageSpec::Text("Tiny text")]),
        );
        let report = crate::extract_transitional(
            &path,
            &InspectOptions {
                max_pages: None,
                max_units: 0,
                ..Default::default()
            },
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.pdf.as_ref().unwrap().text_complete);
        assert!(!report.text.truncated);
        assert!(report.text.content.contains("Tiny text"));
    }

    #[test]
    fn native_text_tripping_the_cumulative_cap_marks_the_report_partial() {
        // LITX-AUD-014 follow-up: cumulative text-cap exhaustion must also
        // mark the text payload partial, not only pdf.text_complete. The test
        // seam caps cumulative decoded text at 32 bytes.
        let path = write_temp_pdf(
            "over-cap.pdf",
            &synthetic_pdf(&[PageSpec::Text(
                "Native text beyond the tiny test cap of thirty-two bytes.",
            )]),
        );
        let report = crate::extract_transitional(
            &path,
            &InspectOptions {
                max_pages: None,
                max_units: 0,
                ..Default::default()
            },
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.stage == "pdf_text" && e.message.contains("cap")),
            "{:?}",
            report.errors
        );
        assert!(!report.pdf.as_ref().unwrap().text_complete);
        assert!(report.text.truncated);
    }

    #[test]
    fn staging_budget_charges_with_checked_arithmetic() {
        // Exact fit succeeds and exhausts the budget; the next byte is
        // refused without wrapping.
        let mut budget = MAX_OCR_STAGING_BYTES;
        charge_ocr_staging(&mut budget, &vec![0u8; MAX_OCR_STAGING_BYTES]).unwrap();
        assert_eq!(budget, 0);
        charge_ocr_staging(&mut budget, &vec![0u8]).unwrap_err();
        assert_eq!(budget, 0);
        let mut full = MAX_OCR_STAGING_BYTES;
        charge_ocr_staging(&mut full, &vec![0u8; MAX_OCR_STAGING_BYTES + 1]).unwrap_err();
        assert_eq!(full, MAX_OCR_STAGING_BYTES);
    }

    #[test]
    fn staging_budget_charges_retained_capacity_not_payload_length() {
        // The queue charges the retained allocation, not the payload: a
        // small-payload raster whose Vec capacity exceeds the remaining
        // budget must be refused with the budget unchanged, while a raster
        // whose capacity exactly fits is accepted.
        let mut budget = MAX_OCR_STAGING_BYTES;
        let mut spare = Vec::with_capacity(MAX_OCR_STAGING_BYTES + 1);
        spare.extend_from_slice(&[0u8; 8]);
        assert!(spare.len() <= budget);
        assert!(spare.capacity() > budget);
        charge_ocr_staging(&mut budget, &spare).unwrap_err();
        assert_eq!(budget, MAX_OCR_STAGING_BYTES);
        let exact: Vec<u8> = Vec::with_capacity(MAX_OCR_STAGING_BYTES);
        assert_eq!(exact.capacity(), MAX_OCR_STAGING_BYTES);
        charge_ocr_staging(&mut budget, &exact).unwrap();
        assert_eq!(budget, 0);
    }

    #[test]
    fn ocr_staging_budget_rejects_unbounded_raster_retention() {
        // LITX-AUD-014 follow-up: the aggregate retained-raster budget (test
        // seam: 1 MiB) rejects the first oversized noisy render, skips
        // further renders, reports the exhaustion once, and leaves no
        // pending OCR statuses. Runs before any OCR probe, so no tessdata
        // is required.
        let jpegs: Vec<(Vec<u8>, u32, u32)> = (0..3).map(|_| noisy_jpeg(900, 1200)).collect();
        let specs: Vec<PageSpec> = jpegs
            .iter()
            .map(|(jpeg, width, height)| PageSpec::Scan {
                jpeg,
                width: *width,
                height: *height,
            })
            .collect();
        let path = write_temp_pdf("staging.pdf", &synthetic_pdf(&specs));
        let report = crate::extract_transitional(
            &path,
            &InspectOptions {
                identify: true,
                max_pages: None,
                max_units: 0,
                ..Default::default()
            },
        );
        let ocr_errors = report
            .errors
            .iter()
            .filter(|e| e.stage == "pdf_ocr")
            .count();
        assert_eq!(ocr_errors, 1, "{:?}", report.errors);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.stage == "pdf_ocr" && e.message.contains("staging")),
            "{:?}",
            report.errors
        );
        let pdf = report.pdf.as_ref().unwrap();
        assert!(!pdf.text_complete);
        assert!(report.text.truncated);
        assert_eq!(report.counts.pages, Some(3));
        assert_eq!(pdf.geometry.len(), 3);
        assert_eq!(pdf.identification_pages.len(), 3);
        assert!(pdf.identification_pages.iter().all(|p| p.needs_ocr));
        assert!(
            pdf.identification_pages
                .iter()
                .all(|p| p.ocr_status == "failed"),
            "{:?}",
            pdf.identification_pages
        );
    }

    fn solid_jpeg(width: u32, height: u32, shade: u8) -> (Vec<u8>, u32, u32) {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([shade, shade, shade]));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        (jpeg.into_inner(), width, height)
    }

    #[test]
    fn ocr_probes_stop_after_native_text_cap_without_extra_errors() {
        // LITX-AUD-014: once the native phase exhausts the cumulative text
        // cap, the unlocked phase must not run queued OCR probes at all:
        // no pdf_ocr errors, no duplicate cap errors, and no pending status.
        let (jpeg, width, height) = solid_jpeg(500, 650, 200);
        let path = write_temp_pdf(
            "native-cap-stop.pdf",
            &synthetic_pdf(&[
                PageSpec::Scan {
                    jpeg: &jpeg,
                    width,
                    height,
                },
                PageSpec::Text("This native page trips the tiny cumulative text cap."),
            ]),
        );
        let report = crate::extract_transitional(
            &path,
            &InspectOptions {
                identify: true,
                max_pages: None,
                max_units: 0,
                ..Default::default()
            },
        );
        let cap_errors = report
            .errors
            .iter()
            .filter(|e| e.stage == "pdf_text" && e.message.contains("cap"))
            .count();
        assert_eq!(cap_errors, 1, "{:?}", report.errors);
        assert!(
            !report.errors.iter().any(|e| e.stage == "pdf_ocr"),
            "{:?}",
            report.errors
        );
        let pdf = report.pdf.as_ref().unwrap();
        assert!(!pdf.text_complete);
        assert!(report.text.truncated);
        assert_eq!(pdf.identification_pages.len(), 1);
        assert_eq!(pdf.identification_pages[0].ocr_status, "failed");
    }

    #[test]
    #[ignore = "requires local eng.traineddata, set TESSDATA_PREFIX"]
    fn ocr_probes_stop_immediately_when_ocr_text_trips_the_cap() {
        // LITX-AUD-014: when OCR output trips the cumulative cap (test seam:
        // 32 bytes), the first tripping probe emits the single cap error and
        // every still-queued probe is abandoned, never left pending.
        let fixture =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/native.pdf");
        let directory = std::env::temp_dir().join(format!(
            "litx-pdf-ocr-stop-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let thumbnail = directory.join("ocr-source.png");
        let rendered = crate::extract_transitional(
            &fixture,
            &InspectOptions {
                thumbnail: Some(thumbnail.clone()),
                thumbnail_max_edge: 1200,
                max_units: 0,
                ..Default::default()
            },
        );
        assert!(thumbnail.exists(), "{:?}", rendered.errors);
        let image = image::open(&thumbnail).unwrap().to_rgb8();
        let (width, height) = (image.width(), image.height());
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let specs: Vec<PageSpec> = (0..3)
            .map(|_| PageSpec::Scan {
                jpeg: jpeg.get_ref(),
                width,
                height,
            })
            .collect();
        let path = directory.join("scans.pdf");
        std::fs::write(&path, synthetic_pdf(&specs)).unwrap();
        let report = crate::extract_transitional(
            &path,
            &InspectOptions {
                identify: true,
                max_pages: None,
                max_units: 0,
                ..Default::default()
            },
        );
        let cap_errors = report
            .errors
            .iter()
            .filter(|e| e.stage == "pdf_text" && e.message.contains("cap"))
            .count();
        assert_eq!(cap_errors, 1, "{:?}", report.errors);
        let pdf = report.pdf.as_ref().unwrap();
        assert!(!pdf.text_complete);
        assert!(report.text.truncated);
        assert_eq!(pdf.identification_pages.len(), 3);
        assert_eq!(pdf.identification_pages[0].ocr_status, "text_detected");
        assert_eq!(pdf.identification_pages[1].ocr_status, "failed");
        assert_eq!(pdf.identification_pages[2].ocr_status, "failed");
    }

    #[test]
    #[ignore = "requires local eng.traineddata, set TESSDATA_PREFIX"]
    fn ocr_text_tripping_the_cumulative_cap_marks_the_report_partial() {
        // OCR output charges the same cumulative cap: a small native page
        // plus an OCR'd scan exceeds the 32-byte test seam.
        let fixture =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/native.pdf");
        let directory = std::env::temp_dir().join(format!(
            "litx-pdf-ocr-cap-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let thumbnail = directory.join("ocr-source.png");
        let rendered = crate::extract_transitional(
            &fixture,
            &InspectOptions {
                thumbnail: Some(thumbnail.clone()),
                thumbnail_max_edge: 1200,
                max_units: 0,
                ..Default::default()
            },
        );
        assert!(thumbnail.exists(), "{:?}", rendered.errors);
        let image = image::open(&thumbnail).unwrap().to_rgb8();
        let (width, height) = (image.width(), image.height());
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let path = directory.join("mixed.pdf");
        std::fs::write(
            &path,
            synthetic_pdf(&[
                PageSpec::Text("Native text"),
                PageSpec::Scan {
                    jpeg: jpeg.get_ref(),
                    width,
                    height,
                },
            ]),
        )
        .unwrap();
        let report = crate::extract_transitional(
            &path,
            &InspectOptions {
                identify: true,
                max_pages: None,
                max_units: 0,
                ..Default::default()
            },
        );
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.stage == "pdf_text" && e.message.contains("cap")),
            "{:?}",
            report.errors
        );
        assert!(!report.pdf.as_ref().unwrap().text_complete);
        assert!(report.text.truncated);
    }

    /// W3-SYS-PDF-001: a page-local complexity failure must retain every
    /// recoverable native and Markdown page while marking the report partial.
    /// Accepts no arguments and returns unit; assertion failures identify lost
    /// text/layout, a missing diagnostic, or incorrect completeness flags.
    #[test]
    fn complexity_failure_preserves_recovered_pages() {
        let bytes = synthetic_pdf(&[
            PageSpec::Text("first page"),
            PageSpec::Text("second page"),
            PageSpec::Text("third page"),
        ]);
        let path = write_temp_pdf("complexity-failure.pdf", &bytes);
        let mut report = crate::extract_transitional(
            &path,
            &InspectOptions {
                max_pages: Some(0),
                max_units: 0,
                ..Default::default()
            },
        );
        report.text = crate::Text::default();
        report.markdown = crate::Markdown::default();
        report.errors.clear();
        let options = InspectOptions {
            identify: true,
            max_pages: None,
            max_units: 0,
            ..Default::default()
        };
        let result = inner_with_complexity(
            bytes,
            &mut report,
            &options,
            &mut crate::images::OcrSession::default(),
            |document, page| {
                if page.page_number == 2 {
                    Err("injected page complexity failure".into())
                } else {
                    stages::page_complexity(document, page).map_err(Into::into)
                }
            },
        );
        if let Err(error) = result {
            report.error("pdf", error);
        }
        for expected in ["first page", "second page", "third page"] {
            assert!(
                report.text.content.contains(expected),
                "missing recovered text {expected:?}: {:?}",
                report.text
            );
            assert!(
                report.markdown.content.contains(expected),
                "missing recovered Markdown {expected:?}: {:?}",
                report.markdown.content
            );
        }
        assert!(report.errors.iter().any(|error| {
            error.stage == "pdf_text" && error.message.contains("page 2: injected")
        }));
        assert!(!report.pdf.as_ref().unwrap().text_complete);
        assert!(!report.pdf.as_ref().unwrap().preview_complete);
        assert!(report.text.truncated);
    }

    /// W3-SYS-PDF-001: an over-budget native page must not leak into Markdown
    /// after the cumulative text budget rejects it. Accepts no arguments and
    /// returns unit; assertion failures identify budget or truncation regressions.
    #[test]
    fn cumulative_native_cap_also_bounds_markdown_pages() {
        let overflowing = "This page exceeds the tiny cumulative native-text budget.";
        let path = write_temp_pdf(
            "markdown-cumulative-cap.pdf",
            &synthetic_pdf(&[PageSpec::Text("Kept"), PageSpec::Text(overflowing)]),
        );
        let report = crate::extract_transitional(
            &path,
            &InspectOptions {
                max_pages: None,
                max_units: 0,
                ..Default::default()
            },
        );
        assert!(report.text.content.contains("Kept"));
        assert!(!report.text.content.contains(overflowing));
        assert!(report.markdown.content.contains("Kept"));
        assert!(
            !report.markdown.content.contains(overflowing),
            "over-budget page was retained in Markdown: {}",
            report.markdown.content
        );
        assert!(report.text.truncated);
        assert!(report.markdown.truncated);
    }

    /// BLK-09: the metadata page-scan cap keeps the page-count fact exact,
    /// stops per-page signals at the cap, and reports the partial state.
    /// Accepts no arguments and returns unit; assertion failures identify a
    /// missing cap, a wrong count completeness, or a missing diagnostic. The
    /// reader is called directly so the test stays scoped to this module.
    #[test]
    fn metadata_scan_caps_page_signals_and_reports_partial_state() {
        let path = write_temp_pdf(
            "metadata-scan-cap.pdf",
            &synthetic_pdf(&vec![PageSpec::Empty; MAX_PAGES + 1]),
        );
        let mut metadata = crate::reports::DocumentMetadata::default();
        crate::pdf::metadata(&path, &mut metadata, None).unwrap();
        let page_count = metadata
            .counts
            .iter()
            .find(|count| count.unit == crate::reports::NativeUnit::PhysicalPage);
        assert!(
            page_count.is_some(),
            "physical page count fact missing: counts={:?} diagnostics={:?}",
            metadata.counts,
            metadata.diagnostics
        );
        let page_count = page_count.unwrap();
        assert_eq!(page_count.value, Some(MAX_PAGES as u64 + 1));
        assert_eq!(page_count.completeness, crate::reports::Completeness::Exact);
        assert_eq!(metadata.native_page_signals.len(), MAX_PAGES);
        assert!(
            metadata
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "pdf_page_signals_capped"),
            "{:?}",
            metadata.diagnostics
        );
        assert_eq!(metadata.completeness, crate::reports::Completeness::Partial);
        assert_eq!(metadata.status, crate::reports::OperationStatus::Partial);
    }

    /// BLK-09: a freakishly large first page renders a bounded thumbnail
    /// raster at the requested edge instead of an unbounded 72-dpi bitmap.
    /// Accepts no arguments and returns unit; assertion failures identify an
    /// oversized raster, a failed artifact, or wrong output dimensions. The
    /// reader is called directly so the test stays scoped to this module.
    #[test]
    fn metadata_thumbnail_raster_is_capped_for_huge_pages() {
        let bytes = synthetic_pdf_sized(30_000.0, 30_000.0, &[PageSpec::Empty]);
        let path = write_temp_pdf("huge-page.pdf", &bytes);
        let directory = std::env::temp_dir().join(format!(
            "litx-pdf-thumb-cap-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let thumbnail_path = directory.join("thumb.png");
        let mut metadata = crate::reports::DocumentMetadata::default();
        crate::pdf::metadata(
            &path,
            &mut metadata,
            Some(&crate::reports::ThumbnailOptions {
                path: thumbnail_path.clone(),
                max_edge: 320,
            }),
        )
        .unwrap();
        assert!(
            !metadata
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "thumbnail_failed"),
            "{:?}",
            metadata.diagnostics
        );
        assert!(
            thumbnail_path.exists(),
            "thumbnail artifact missing: diagnostics={:?} artifacts={:?}",
            metadata.diagnostics,
            metadata.artifacts
        );
        let image = image::open(&thumbnail_path).unwrap();
        assert!(
            image.width().max(image.height()) <= 320,
            "{}x{}",
            image.width(),
            image.height()
        );
    }

    /// BLK-09: the pre-render OCR bound mirrors the render stage: default dpi
    /// lowered by the caller cap and the built-in 4096-px ceiling, one byte
    /// per grayscale pixel, two pixels of rounding headroom per edge.
    /// Accepts no arguments and returns unit; assertion failures identify a
    /// bound that could undercharge the raster staging budget.
    #[test]
    fn ocr_raster_bound_reflects_geometry_and_caps() {
        // 612x792 pt at the 150 default dpi: 1275x1650 px.
        assert_eq!(
            ocr_raster_bound(612.0, 792.0, 3072).unwrap(),
            (1275 + 2) * (1650 + 2)
        );
        // Caller cap 512 px lowers the dpi so the long edge is the cap;
        // short edge: 612 * 512 / 792 = 395.63... -> 396 px + headroom.
        assert_eq!(
            ocr_raster_bound(612.0, 792.0, 512).unwrap(),
            (396 + 2) * (512 + 2)
        );
        // The built-in 4096-px ceiling lowers the dpi below the default on a
        // huge page even without a caller cap; short edge scales by ratio.
        // 20000x10000 pt: dpi = 4096 * 72 / 20000 = 14.7456.
        assert_eq!(
            ocr_raster_bound(20_000.0, 10_000.0, 0).unwrap(),
            (4096 + 2) * (2048 + 2)
        );
        // Geometry that cannot be bounded is refused instead of guessed.
        assert!(ocr_raster_bound(0.0, 800.0, 3072).is_err());
        assert!(ocr_raster_bound(f32::NAN, 800.0, 3072).is_err());
    }

    /// BLK-09: the shared staging budget is reserved from the geometry bound
    /// BEFORE each render; the first page that cannot fit stops staging with
    /// a single error, later pages are never rendered, and native content
    /// stays. Accepts no arguments and returns unit; assertion failures
    /// identify unbounded rendering or missing partial state.
    #[test]
    #[ignore = "requires local eng.traineddata, set TESSDATA_PREFIX"]
    fn v2_ocr_staging_stops_at_the_first_unfitting_page() {
        // Page 1 (200x200 pt) bounds at (417 + 2)^2 = 175,561 bytes and fits
        // the 1 MiB test seam; page 2 (600x800 pt) bounds at
        // (1667 + 2) * (1250 + 2) = 2,089,588 bytes and is refused, which
        // stops staging before page 3 is ever rendered.
        let path = write_temp_pdf(
            "v2-staging.pdf",
            &synthetic_pdf(&[
                PageSpec::EmptySized {
                    width: 200.0,
                    height: 200.0,
                },
                PageSpec::Empty,
                PageSpec::Empty,
            ]),
        );
        let options = InspectOptions {
            identify: true,
            native_selection: Some(crate::reports::NativeSelection::Ranges {
                ranges: vec![crate::reports::NativeRange { start: 1, end: 3 }],
            }),
            ..Default::default()
        };
        let report = crate::extract_transitional(&path, &options);
        let staging_errors: Vec<&crate::StageError> = report
            .errors
            .iter()
            .filter(|error| error.stage == "pdf_ocr" && error.message.contains("staging"))
            .collect();
        assert_eq!(staging_errors.len(), 1, "{:?}", report.errors);
        assert!(
            staging_errors[0].message.contains("page 2"),
            "{}",
            staging_errors[0].message
        );
        assert!(!report.pdf.as_ref().unwrap().text_complete);
        assert_eq!(report.counts.pages, Some(3));
        assert!(
            !staging_errors[0].message.contains("page 3"),
            "staging continued past the first unaffordable page: {}",
            staging_errors[0].message
        );
    }
}

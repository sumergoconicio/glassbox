//! Module: glassbox-inspect Apple iWork embedded-preview reader.
//! File: iwork.rs.
//! Responsibility: Extract bounded QuickLook derived previews for iWork packages without emitting native document pages.
use crate::{
    InspectOptions, InspectReport,
    archive::Archive,
    images::OcrSession,
    reports::{
        Completeness, ContentOrigin, ContentUnit, ExtractionProvenance, FactSource, NativeUnit,
        OcrProvenance,
    },
};
use serde::Serialize;

/// Maximum Unicode scalar characters retained from one embedded iWork preview.
const IWORK_PREVIEW_MAX_CHARS: usize = 4_000;

/// Metadata describing an embedded QuickLook preview inside an iWork package.
#[derive(Debug, Serialize)]
pub struct DerivedPreview {
    pub source_part: String,
    pub content_type: &'static str,
    pub partial: bool,
}

/// Decodes one bounded embedded preview payload into an isolated scratch report.
///
/// Accepts the archive part name, raw preview bytes, isolated preview options
/// (with `native_selection == None`, `max_pages <= 1`, and `max_chars <= 4000`),
/// and the worker's OCR session. Returns an isolated [`InspectReport`] holding
/// bounded preview text, text-origin provenance, and any reader diagnostics;
/// invalid PDFs, corrupt images, or invalid UTF-8 text record errors in the
/// returned report without touching the caller's native-unit state.
fn decode_isolated_preview(
    name: &str,
    bytes: Vec<u8>,
    preview_options: &InspectOptions,
    ocr: &mut OcrSession,
) -> InspectReport {
    let mut preview_report = InspectReport::default();
    if name.ends_with(".pdf") {
        crate::pdf::inspect_bytes(bytes, &mut preview_report, preview_options, ocr);
    } else if name.ends_with(".txt") {
        match std::str::from_utf8(&bytes) {
            Ok(text) => preview_report.append_text(text, preview_options.max_chars),
            Err(error) => preview_report.error("iwork_preview", error),
        }
    } else {
        crate::images::inspect_bytes(bytes, &mut preview_report, preview_options, ocr);
    }
    preview_report
}

/// Reports whether a ZIP central directory contains a structural Apple iWork marker.
///
/// Accepts the archive entry names collected from the ZIP central directory.
/// Returns `true` when at least one entry matches an Apple iWork package layout
/// marker (`Index/*.iwa`, `index.xml`, `index.apxl`, `QuickLook/*`,
/// `preview.png`, or `preview.jpg`), and `false` for generic ZIP archives
/// lacking iWork structure. Performs no I/O and cannot fail.
pub(crate) fn has_iwork_marker(names: &[String]) -> bool {
    names.iter().any(|name| {
        (name.starts_with("Index/") && name.ends_with(".iwa"))
            || matches!(
                name.as_str(),
                "index.xml" | "index.apxl" | "preview.png" | "preview.jpg"
            )
            || name.starts_with("QuickLook/")
    })
}

/// Extracts a bounded QuickLook preview for `.pages`, `.numbers`, and `.key` packages.
///
/// Accepts an open ZIP archive, destination report, extraction options, and the
/// worker OCR session. Returns `false` when the extension is not `pages`,
/// `numbers`, or `key`, or when the archive lacks a structural iWork marker
/// (without emitting `iwork_preview` diagnostics). Otherwise records derived-preview
/// metadata and either one bounded [`NativeUnit::DatasetPreview`] unit (when
/// `native_selection` is set) or legacy bounded preview text/Markdown (when
/// `native_selection` is `None`), and returns `true`. Preview decoding is always
/// isolated from original-document selection so embedded multi-page PDFs never
/// emit native [`NativeUnit::PhysicalPage`] units. Archive read failures,
/// missing previews, out-of-range selections, and preview-decoder errors are
/// recorded in `report.errors`.
pub(crate) fn inspect(
    archive: &mut Archive,
    report: &mut InspectReport,
    options: &InspectOptions,
    ocr: &mut OcrSession,
) -> bool {
    if !matches!(
        report.format.extension.as_str(),
        "pages" | "numbers" | "key"
    ) || !has_iwork_marker(&archive.names)
    {
        return false;
    }
    for name in [
        "QuickLook/Preview.pdf",
        "QuickLook/Preview.txt",
        "QuickLook/Preview.png",
        "QuickLook/Preview.jpg",
        "preview.png",
        "preview.jpg",
        "QuickLook/Thumbnail.jpg",
    ] {
        match archive.read(name) {
            Ok(Some(bytes)) => {
                let pdf = name.ends_with(".pdf");
                let text = name.ends_with(".txt");
                report.derived_preview = Some(DerivedPreview {
                    source_part: name.into(),
                    content_type: if pdf {
                        "application/pdf"
                    } else if text {
                        "text/plain"
                    } else {
                        "image"
                    },
                    partial: true,
                });
                let mut preview_options = options.clone();
                preview_options.max_pages = Some(options.max_pages.unwrap_or(1).min(1));
                preview_options.max_units = options.max_units.min(1);
                preview_options.max_chars = options.max_chars.min(IWORK_PREVIEW_MAX_CHARS);
                // Isolate embedded preview decoding from original-document
                // native-unit selection so QuickLook PDFs/images never emit
                // native PhysicalPage or Image units for the outer package.
                preview_options.native_selection = None;

                if let Some(selection) = &options.native_selection {
                    let plan_result =
                        crate::selection::begin(report, selection, NativeUnit::DatasetPreview, 1);
                    report.native_available.source = FactSource::DerivedPreview;
                    report.native_available.completeness = Completeness::Partial;
                    report.text.origin = "embedded_preview".into();
                    report.text.truncated = true;
                    let Ok(plan) = plan_result else {
                        return true;
                    };
                    if !plan.contains(1) {
                        return true;
                    }
                    preview_options.thumbnail = None;
                    let preview_report =
                        decode_isolated_preview(name, bytes, &preview_options, ocr);
                    let had_errors = !preview_report.errors.is_empty();
                    report.errors.extend(preview_report.errors);
                    if had_errors && preview_report.text.content.is_empty() {
                        return true;
                    }
                    let used_ocr = matches!(
                        preview_report.text.origin.as_str(),
                        "ocr" | "native_and_ocr"
                    );
                    let origin = if used_ocr {
                        ContentOrigin::Ocr
                    } else {
                        ContentOrigin::DerivedPreview
                    };
                    let ocr_provenance = if used_ocr {
                        if let Some(probe) = preview_report
                            .image
                            .as_ref()
                            .and_then(|image| image.probe.as_ref())
                        {
                            Some(OcrProvenance {
                                engine: probe.engine.into(),
                                language: probe.language.clone(),
                                mean_confidence: Some(probe.mean_confidence),
                                raster_long_edge: Some(preview_options.ocr_raster_long_edge),
                            })
                        } else {
                            Some(OcrProvenance {
                                engine: "tesseract".into(),
                                language: preview_options.ocr_language.clone(),
                                mean_confidence: None,
                                raster_long_edge: Some(preview_options.ocr_raster_long_edge),
                            })
                        }
                    } else {
                        None
                    };
                    let bounded_preview_text: String = preview_report
                        .text
                        .content
                        .chars()
                        .take(preview_options.max_chars)
                        .collect();
                    report.native_units.push(ContentUnit {
                        unit_index: 1,
                        unit: NativeUnit::DatasetPreview,
                        source_part: Some(name.into()),
                        label: None,
                        marker_index: None,
                        start_ms: None,
                        end_ms: None,
                        text: Some(bounded_preview_text),
                        rows: Vec::new(),
                        provenance: vec![ExtractionProvenance {
                            reader: "iwork-preview".into(),
                            stage: "embedded-preview".into(),
                            source: FactSource::DerivedPreview,
                            origin,
                            ocr: ocr_provenance,
                        }],
                        completeness: Completeness::Partial,
                    });
                    return true;
                }

                // LITX-AUD-019: the notice is provenance-neutral; the actual
                // preview outcome (native, OCR, or mixed) sets the basis.
                crate::markdown::append(
                    report,
                    &format!(
                        "> Partial iWork preview from {name}. Original document structure was not decoded.\n\n> Bounded embedded preview: first page/image, at most 4000 characters.\n\n"
                    ),
                    &preview_options,
                    "",
                );
                if pdf {
                    crate::pdf::inspect_bytes(bytes, report, &preview_options, ocr);
                } else if text {
                    match std::str::from_utf8(&bytes) {
                        Ok(text) => {
                            report.append_text(text, preview_options.max_chars);
                            crate::markdown::append(
                                report,
                                &crate::markdown::escape(&report.text.content),
                                &preview_options,
                                "text_fallback",
                            );
                        }
                        Err(error) => report.error("iwork_preview", error),
                    }
                } else {
                    preview_options.thumbnail = None;
                    // OCR'd image text appends its own ocr_text_fallback
                    // Markdown (LITX-AUD-009); nothing more to add here.
                    crate::images::inspect_bytes(bytes.clone(), report, &preview_options, ocr);
                    crate::thumbnail::write(
                        std::io::Cursor::new(bytes),
                        "embedded",
                        options,
                        report,
                    );
                }
                if report.markdown.basis.is_empty() {
                    report.markdown.basis = "text_fallback";
                }
                report.text.origin = "embedded_preview".into();
                report.text.truncated = true;
                return true;
            }
            Ok(None) => {}
            Err(error) => {
                report.error("iwork_preview", error);
                return true;
            }
        }
    }
    report.error(
        "iwork_preview",
        "no supported embedded QuickLook preview; native IWA extraction is unavailable",
    );
    true
}

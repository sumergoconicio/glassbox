//! Bounded, offline Quick Look. Claims retain their source; they are not verified identities.
use serde::Serialize;
use std::path::{Path, PathBuf};
mod archive;
mod detect;
mod thumbnail;

#[derive(Debug, Clone)]
pub struct InspectOptions {
    pub max_chars: usize,
    pub max_units: usize,
    pub thumbnail: Option<PathBuf>,
    pub thumbnail_max_edge: u32,
    pub packet: bool,
    pub probe_ocr: bool,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            max_chars: 16_000,
            max_units: 5,
            thumbnail: None,
            thumbnail_max_edge: 320,
            packet: false,
            probe_ocr: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Format {
    pub extension: String,
    pub detected_mime: Option<String>,
    pub detected_name: Option<String>,
    pub bucket: String,
    pub basis: String,
    pub disagreement: bool,
    pub detector: String,
}

#[derive(Debug, Serialize)]
pub struct Claim {
    pub key: String,
    pub value: String,
    pub source: &'static str,
}

#[derive(Debug, Default, Serialize)]
pub struct Counts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheets: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slides: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paragraphs: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<usize>,
    pub chars: usize,
    pub bytes: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Text {
    pub content: String,
    pub truncated: bool,
    pub chars: usize,
    pub origin: String,
}

#[derive(Debug, Serialize)]
pub struct StageError {
    pub stage: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Thumbnail {
    pub written_to: PathBuf,
    pub width: u32,
    pub height: u32,
    pub basis: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ArchiveEntry {
    pub name: String,
    pub size: u64,
    pub compressed_size: u64,
    pub timestamp: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Preflight {
    pub page_number: usize,
    pub unit_kind: &'static str,
    pub needs_ocr: bool,
    pub reasons: Vec<String>,
    pub confidence: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Column {
    pub index: usize,
    pub header: Option<String>,
    pub inferred_type: String,
    pub non_empty_count: usize,
    pub distinct_count: usize,
    pub total_bytes: usize,
    pub max_bytes: usize,
    pub mean_bytes: f64,
    pub avg_prose_score: f64,
    pub is_prose_candidate: bool,
    pub sample_values: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Sheet {
    pub name: String,
    pub index: usize,
    pub used_range: Option<String>,
    pub rows: usize,
    pub cols: usize,
    pub cell_count: usize,
    pub non_empty_cells: usize,
    pub formula_count: usize,
    pub merged_ranges: Vec<String>,
    pub is_longest: bool,
    pub columns: Vec<Column>,
}

#[derive(Debug, Serialize)]
pub struct InspectReport {
    pub format: Format,
    pub claims: Vec<Claim>,
    pub counts: Counts,
    pub entries: Vec<ArchiveEntry>,
    pub sheets: Vec<Sheet>,
    pub preflight: Vec<Preflight>,
    pub text: Text,
    pub thumbnail: Option<Thumbnail>,
    pub errors: Vec<StageError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packet: Option<serde_json::Value>,
}

impl InspectReport {
    pub(crate) fn error(&mut self, stage: &'static str, message: impl ToString) {
        self.errors.push(StageError {
            stage,
            message: message.to_string(),
        });
    }

    pub fn quick_look(&self) -> String {
        let mut out = format!(
            "{} ({})\n{} bytes, {} chars\n",
            self.format.bucket,
            self.format
                .detected_name
                .as_deref()
                .unwrap_or("extension hint"),
            self.counts.bytes,
            self.counts.chars
        );
        for claim in &self.claims {
            out.push_str(&format!(
                "{}: {} [{}]\n",
                claim.key, claim.value, claim.source
            ));
        }
        out.push_str(&self.text.content);
        for error in &self.errors {
            out.push_str(&format!("\n[{}] {}", error.stage, error.message));
        }
        out
    }
}

pub fn inspect(path: &Path, options: &InspectOptions) -> InspectReport {
    let extension = detect::extension(path);
    let mut report = InspectReport {
        format: Format {
            extension: extension.clone(),
            detected_mime: None,
            detected_name: None,
            bucket: if extension == "txt" { "Text" } else { "Other" }.into(),
            basis: "extension".into(),
            disagreement: false,
            detector: "extension".into(),
        },
        claims: vec![],
        counts: Counts::default(),
        entries: vec![],
        sheets: vec![],
        preflight: vec![],
        text: Text {
            origin: "native".into(),
            ..Text::default()
        },
        thumbnail: None,
        errors: vec![],
        packet: None,
    };
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => report.counts.bytes = meta.len(),
        Ok(_) => report.error("input", "input is not a regular file"),
        Err(error) => report.error("input", error),
    }
    if report.errors.is_empty() {
        if let Err(error) = detect::detect(path, &mut report) {
            report.error("detection", error);
        } else if report.format.detected_mime.as_deref() == Some("application/zip")
            || matches!(
                report.format.extension.as_str(),
                "zip" | "cbz" | "docx" | "xlsx" | "xlsm" | "pptx" | "odt" | "ods" | "odp"
            )
        {
            match archive::Archive::open(path, &mut report) {
                Ok(mut archive) => {
                    // Only allowlisted embedded preview entries may be read.
                    if options.thumbnail.is_some() {
                        for name in [
                            "docProps/thumbnail.jpeg",
                            "docProps/thumbnail.png",
                            "Thumbnails/thumbnail.png",
                        ] {
                            match archive.read(name) {
                                Ok(Some(bytes)) => {
                                    thumbnail::write(
                                        std::io::Cursor::new(bytes),
                                        "embedded",
                                        options,
                                        &mut report,
                                    );
                                    break;
                                }
                                Ok(None) => {}
                                Err(error) => report.error("thumbnail", error),
                            }
                        }
                    }
                }
                Err(error) => report.error("archive", error),
            }
        } else if report.format.bucket == "Images" && options.thumbnail.is_some() {
            match std::fs::File::open(path) {
                Ok(file) => thumbnail::write(
                    std::io::BufReader::new(file),
                    "downscaled",
                    options,
                    &mut report,
                ),
                Err(error) => report.error("thumbnail", error),
            }
        }
    }
    if options.probe_ocr {
        report.error("probe_ocr", "image OCR probing is deferred to Phase 4");
    }
    if options.packet {
        report.packet = Some(serde_json::json!({
            "Format": report.format.bucket, "Language": "N/A",
            "counts": report.counts, "excerpt": report.text.content,
        }));
    }
    report
}

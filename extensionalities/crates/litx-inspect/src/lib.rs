//! Module: glassbox-inspect native file inspection library.
//! File: lib.rs.
//! Responsibility: Expose canonical v2 inspection/extraction and the explicitly transitional content API.
//!
//! Legacy claims retain their source; they are not verified identities.
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

#[cfg(not(test))]
const SHARED_OCR_STAGING_BYTES: usize = 128 << 20;
#[cfg(test)]
const SHARED_OCR_STAGING_BYTES: usize = 1 << 20;
mod archive;
mod binary;
pub mod bodymatter;
mod comics;
pub mod detect;
mod djvu;
mod epub;
pub mod hashing;
mod images;
mod iwork;
mod json_profile;
mod markdown;
pub mod metadata;
mod mobi;
mod odf;
mod office;
mod parquet;
mod pdf;
mod profile;
mod rar;
pub mod reports;
pub mod reports_v3;
pub mod selection;
mod spreadsheet;
mod tables;
mod tabular;
mod text;
mod thumbnail;
mod xml;
mod xml_profile;
pub use djvu::{DjvuAnalysis, DjvuPageSignal};
pub use images::{ImageAnalysis, OcrProbe};
pub use iwork::DerivedPreview;
pub use json_profile::JsonProfile;
pub use markdown::Markdown;
pub use parquet::ParquetProfile;
pub use pdf::{PdfAnalysis, PdfGeometry};
pub use tables::{DocumentTable, TableCell};
pub use tabular::DatasetPreview;
pub use xml_profile::{XmlName, XmlPath, XmlProfile};

/// Explicitly linked libav bindings, never part of the default binary.
#[cfg(feature = "ffmpeg-link")]
pub use ffmpeg_next as linked_ffmpeg;

/// Observable counters for one owned file/OCR work budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkBudgetSnapshot {
    /// Maximum simultaneously owned file leases.
    pub limit: usize,
    /// Currently owned file leases.
    pub active: usize,
    /// Highest observed simultaneous file/OCR lease count.
    pub peak_active: usize,
    /// File operations admitted under this budget.
    pub files_started: u64,
    /// File leases released after completion or cancellation.
    pub files_finished: u64,
    /// Recognition operations entered inside a file lease.
    pub ocr_started: u64,
    /// Recognition operations released without unsafe FFI interruption.
    pub ocr_finished: u64,
    /// Whether admission has been permanently cancelled for this session.
    pub cancelled: bool,
}

#[derive(Debug)]
struct WorkBudgetState {
    active: usize,
    peak_active: usize,
    files_started: u64,
    files_finished: u64,
    ocr_started: u64,
    ocr_finished: u64,
    staging_remaining: usize,
}

#[derive(Debug)]
struct WorkBudgetInner {
    limit: usize,
    cancelled: AtomicBool,
    state: Mutex<WorkBudgetState>,
    available: Condvar,
}

/// One clonable concurrency budget shared by every file worker and OCR operation.
#[derive(Debug, Clone)]
pub struct SharedWorkBudget {
    inner: Arc<WorkBudgetInner>,
}

impl SharedWorkBudget {
    /// Creates a budget whose file and nested recognition work shares N slots.
    ///
    /// Accepts a positive total job count. Returns an owned shareable budget,
    /// or an error for zero before any thread, native library, or file is touched.
    pub fn new(limit: usize) -> Result<Self, String> {
        if limit == 0 {
            return Err("shared file/OCR job budget must be positive".into());
        }
        Ok(Self {
            inner: Arc::new(WorkBudgetInner {
                limit,
                cancelled: AtomicBool::new(false),
                state: Mutex::new(WorkBudgetState {
                    active: 0,
                    peak_active: 0,
                    files_started: 0,
                    files_finished: 0,
                    ocr_started: 0,
                    ocr_finished: 0,
                    staging_remaining: SHARED_OCR_STAGING_BYTES,
                }),
                available: Condvar::new(),
            }),
        })
    }

    /// Acquires one file lease that also authorizes recognition within that file.
    ///
    /// Accepts no parameters. Returns a permit after a slot becomes available,
    /// or None after cancellation. Waiting is condvar-bounded and wakes on release
    /// or cancellation; no nested permit is acquired by OCR.
    pub fn acquire(&self) -> Option<WorkPermit> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while state.active >= self.inner.limit && !self.is_cancelled() {
            state = self
                .inner
                .available
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        if self.is_cancelled() {
            return None;
        }
        state.active += 1;
        state.peak_active = state.peak_active.max(state.active);
        state.files_started = state.files_started.saturating_add(1);
        Some(WorkPermit {
            budget: self.clone(),
            released: false,
        })
    }

    /// Stops new file/recognition admission and wakes every waiting file worker.
    ///
    /// Accepts no parameters. Returns unit after marking the shared session
    /// cancelled while holding the budget mutex so waiting threads cannot miss
    /// the wakeup. Active native work owns its lease until ordinary completion;
    /// no unsafe asynchronous FFI interruption is attempted.
    pub fn cancel(&self) {
        let _state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.available.notify_all();
    }

    /// Reserves retained raw/encoded raster allocation capacity across all workers.
    ///
    /// Accepts the actual Vec capacity, not payload length. Returns a permit that
    /// releases bytes on Drop, or None without changing the remaining budget when
    /// capacity would exceed the shared 128 MiB production (small test) limit.
    pub(crate) fn reserve_staging(&self, capacity: usize) -> Option<OcrStagingPermit> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.staging_remaining = state.staging_remaining.checked_sub(capacity)?;
        Some(OcrStagingPermit {
            budget: self.clone(),
            capacity,
            released: false,
        })
    }

    /// Reports whether this session has stopped admitting new work.
    ///
    /// Accepts no parameters. Returns true after cancellation. This atomic read
    /// performs no allocation, blocking, native calls, or failure states.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Captures actual file/OCR ownership counters for resource verification.
    ///
    /// Accepts no parameters. Returns a consistent counter snapshot, recovering
    /// poisoned mutex state so instrumentation cannot panic a completed worker.
    pub fn snapshot(&self) -> WorkBudgetSnapshot {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        WorkBudgetSnapshot {
            limit: self.inner.limit,
            active: state.active,
            peak_active: state.peak_active,
            files_started: state.files_started,
            files_finished: state.files_finished,
            ocr_started: state.ocr_started,
            ocr_finished: state.ocr_finished,
            cancelled: self.is_cancelled(),
        }
    }
}

/// Owned file lease; recognition is a nested activity, not another acquired slot.
#[derive(Debug)]
pub struct WorkPermit {
    budget: SharedWorkBudget,
    released: bool,
}

impl WorkPermit {
    /// Enters local recognition while retaining the parent file's one slot.
    ///
    /// Accepts no parameters. Returns a nested recognition lease, or None when
    /// cancellation arrived before recognition began. An active recognition is
    /// never interrupted and releases its marker during ordinary Drop.
    pub fn recognition(&self) -> Option<OcrPermit> {
        if self.budget.is_cancelled() {
            return None;
        }
        let mut state = self
            .budget
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.ocr_started = state.ocr_started.saturating_add(1);
        Some(OcrPermit {
            budget: self.budget.clone(),
            released: false,
        })
    }
}

impl Drop for WorkPermit {
    /// Releases the file slot and wakes one waiting worker.
    ///
    /// Accepts the owned permit implicitly. Returns unit after updating actual
    /// ownership counters. Poisoned instrumentation state is recovered; Drop
    /// cannot panic or leave the active count permanently charged.
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut state = self
            .budget
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active = state.active.saturating_sub(1);
        state.files_finished = state.files_finished.saturating_add(1);
        drop(state);
        self.budget.inner.available.notify_one();
    }
}

/// Retained raster-capacity permit shared by every worker in one session.
#[derive(Debug)]
pub(crate) struct OcrStagingPermit {
    budget: SharedWorkBudget,
    capacity: usize,
    released: bool,
}

impl Drop for OcrStagingPermit {
    /// Returns retained raster capacity after recognition or report failure.
    ///
    /// Accepts the owned permit implicitly. Returns unit after restoring the exact
    /// Vec capacity. Poisoned counters recover without panicking during cleanup.
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut state = self
            .budget
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.staging_remaining = state
            .staging_remaining
            .saturating_add(self.capacity)
            .min(SHARED_OCR_STAGING_BYTES);
    }
}

/// Recognition marker held inside an existing [`WorkPermit`].
#[derive(Debug)]
pub struct OcrPermit {
    budget: SharedWorkBudget,
    released: bool,
}

impl Drop for OcrPermit {
    /// Marks ordinary recognition completion without releasing the file lease.
    ///
    /// Accepts the owned marker implicitly. Returns unit after updating the
    /// recognition counter. No thread, model, or native context is destroyed
    /// early, and poisoned instrumentation cannot cause a Drop panic.
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut state = self
            .budget
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.ocr_finished = state.ocr_finished.saturating_add(1);
    }
}
/// Transitional body-producing options retained until legacy readers migrate to [`reports::InspectOptions`].
#[derive(Debug, Clone)]
pub struct InspectOptions {
    pub identify: bool,
    pub max_tokens: Option<usize>,
    pub max_pages: Option<usize>,
    pub max_chars: usize,
    pub max_units: usize,
    pub thumbnail: Option<PathBuf>,
    pub thumbnail_max_edge: u32,
    pub packet: bool,
    pub probe_ocr: bool,
    #[doc(hidden)]
    pub native_selection: Option<reports::NativeSelection>,
    #[doc(hidden)]
    pub ocr_raster_long_edge: u32,
    #[doc(hidden)]
    pub ocr_language: String,
}

impl Default for InspectOptions {
    /// Creates legacy preview controls without a native-selection override.
    ///
    /// Accepts no parameters. Returns the historical five-unit, 16000-character
    /// defaults and the approved OCR raster edge. No reader or OCR engine runs.
    fn default() -> Self {
        Self {
            identify: false,
            max_tokens: None,
            max_pages: Some(5),
            max_chars: 16_000,
            max_units: 5,
            thumbnail: None,
            thumbnail_max_edge: 320,
            packet: false,
            probe_ocr: false,
            native_selection: None,
            ocr_raster_long_edge: reports::DEFAULT_OCR_RASTER_LONG_EDGE,
            ocr_language: "eng".into(),
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
    pub spine_items: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<usize>,
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
    pub tokens: usize,
    pub token_basis: &'static str,
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
pub struct Page {
    pub index: usize,
    pub source_part: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub dpi: Option<u16>,
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Preflight {
    pub page_number: usize,
    pub unit_kind: &'static str,
    pub needs_ocr: bool,
    pub reasons: Vec<String>,
    pub confidence: &'static str,
    pub coverage_available: bool,
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
    #[serde(skip)]
    pub(crate) first_row: Option<Vec<String>>,
    #[serde(skip)]
    pub(crate) first_row_truncated: bool,
    pub preview_rows: Vec<Vec<String>>,
    pub preview_truncated: bool,
    pub headers_preview: Vec<Option<String>>,
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

/// Transitional body-producing report; it is never projected into [`reports::MetadataReport`].
#[derive(Debug, Serialize)]
pub struct InspectReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub djvu: Option<DjvuAnalysis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_preview: Option<DerivedPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset: Option<DatasetPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parquet: Option<ParquetProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json: Option<JsonProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xml: Option<XmlProfile>,
    pub source_path: PathBuf,
    pub artifacts: Vec<Artifact>,
    pub markdown: Markdown,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfAnalysis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageAnalysis>,
    pub pages: Vec<Page>,
    pub format: Format,
    pub claims: Vec<Claim>,
    pub counts: Counts,
    pub entries: Vec<ArchiveEntry>,
    pub sheets: Vec<Sheet>,
    pub tables: Vec<DocumentTable>,
    pub preflight: Vec<Preflight>,
    pub text: Text,
    pub thumbnail: Option<Thumbnail>,
    pub errors: Vec<StageError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packet: Option<serde_json::Value>,
    #[serde(skip)]
    pub(crate) native_headers: Vec<String>,
    #[serde(skip)]
    pub(crate) native_units: Vec<reports::ContentUnit>,
    #[serde(skip)]
    pub(crate) native_unit: reports::NativeUnit,
    #[serde(skip)]
    pub(crate) native_available: reports::QualifiedCount,
}

#[derive(Debug, Serialize)]
pub struct Artifact {
    pub kind: &'static str,
    pub path: PathBuf,
}

/// Empty transitional report used by header-only structural readers.
///
/// Accepts no parameters. Returns a report with no content, counts, claims, or
/// artifacts. This constructor performs no I/O and has no failure states.
impl Default for InspectReport {
    fn default() -> Self {
        Self {
            djvu: None,
            derived_preview: None,
            dataset: None,
            parquet: None,
            json: None,
            xml: None,
            source_path: PathBuf::new(),
            artifacts: vec![],
            markdown: Markdown::default(),
            pdf: None,
            image: None,
            pages: vec![],
            format: Format {
                extension: String::new(),
                detected_mime: None,
                detected_name: None,
                bucket: "Other".into(),
                basis: "extension".into(),
                disagreement: false,
                detector: "extension".into(),
            },
            claims: vec![],
            counts: Counts::default(),
            entries: vec![],
            sheets: vec![],
            tables: vec![],
            preflight: vec![],
            text: Text {
                origin: "native".into(),
                token_basis: "whitespace_words",
                ..Text::default()
            },
            thumbnail: None,
            errors: vec![],
            packet: None,
            native_headers: Vec::new(),
            native_units: Vec::new(),
            native_unit: reports::NativeUnit::Unknown,
            native_available: reports::QualifiedCount::default(),
        }
    }
}
impl InspectReport {
    pub(crate) fn append_text(&mut self, value: &str, cap: usize) {
        let chars = value.chars().count();
        self.append_counted(value, chars, 1, cap);
    }

    pub(crate) fn append_counted(
        &mut self,
        prefix: &str,
        chars: usize,
        repeats: usize,
        cap: usize,
    ) {
        self.counts.chars = self
            .counts
            .chars
            .saturating_add(chars.saturating_mul(repeats));
        if let Some(copies) = cap.saturating_sub(self.text.chars).checked_div(chars) {
            let copies = repeats.min(copies.saturating_add(1));
            for _ in 0..copies {
                let remaining = cap.saturating_sub(self.text.chars);
                let taken = chars.min(remaining);
                self.text.content.extend(prefix.chars().take(taken));
                self.text.chars += taken;
            }
        }
        self.text.truncated = self.counts.chars > self.text.chars;
    }
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
        if let Some(pages) = self.counts.pages {
            out.push_str(&format!("{pages} pages\n"));
        }
        for claim in &self.claims {
            out.push_str(&format!(
                "{}: {} [{}]\n",
                claim.key, claim.value, claim.source
            ));
        }
        if let Some(image) = &self.image {
            out.push_str(&format!(
                "image: {}x{}, {} [{}]\n",
                image.width, image.height, image.classification, image.basis
            ));
            if let Some(probe) = &image.probe {
                out.push_str(&format!(
                    "OCR: {} words, mean confidence {:.2} [{}]\n",
                    probe.word_count, probe.mean_confidence, probe.basis
                ));
            }
        }
        out.push_str(&self.text.content);
        for error in &self.errors {
            out.push_str(&format!("\n[{}] {}", error.stage, error.message));
        }
        out
    }
}

/// Inspects identity and structure without body extraction or OCR.
///
/// Accepts a source path and metadata-only options. Returns a
/// [`reports::MetadataReport`] using [`hashing::HashPolicy::SampledAboveThreshold`].
/// Input, hashing, structural, and artifact failures are qualified inside the
/// report rather than returned as Rust errors.
pub fn inspect(path: &Path, options: &reports::InspectOptions) -> reports::MetadataReport {
    inspect_with_policy(path, options, hashing::HashPolicy::SampledAboveThreshold)
}

/// Inspects identity and structure with an explicit [`hashing::HashPolicy`].
///
/// Accepts a source path, metadata-only options, and a hash policy controlling
/// full SHA-256 versus sampled fingerprinting. Returns a [`reports::MetadataReport`].
/// Budget, input, hashing, structural, and artifact failures are qualified in diagnostics.
pub fn inspect_with_policy(
    path: &Path,
    options: &reports::InspectOptions,
    policy: hashing::HashPolicy,
) -> reports::MetadataReport {
    match Inspector::with_budget(&options.budget) {
        Ok(inspector) => inspector.with_hash_policy(policy).inspect(path, options),
        Err(error) => finalize_public_inspection(unadmitted_metadata(path, &error)),
    }
}

/// Finalizes inspection-only support semantics and structured inert advice.
///
/// Accepts a metadata report from the dedicated no-body reader. Returns the
/// report with successful identification-only inspection distinguished from
/// unsupported extraction and with exact argument-vector advice where useful.
/// Missing Unicode path representation suppresses advice rather than lossy
/// reconstruction; no command is executed and no credentials are introduced.
fn finalize_public_inspection(mut report: reports::MetadataReport) -> reports::MetadataReport {
    if report.metadata.extraction_support == reports::SupportStatus::IdentificationOnly
        && report.metadata.status == reports::OperationStatus::Unsupported
        && report.metadata.completeness == reports::Completeness::Exact
    {
        report.metadata.status = reports::OperationStatus::Complete;
    }
    if !report.metadata.advice.is_empty() {
        return report;
    }
    let Some(path) = report.metadata.identity.path.to_str() else {
        return report;
    };
    let advice = match report.metadata.extraction_support {
        reports::SupportStatus::Supported
            if report.metadata.encryption != reports::EncryptionStatus::EncryptedInaccessible =>
        {
            Some(reports::Advice {
                operation: reports::AdviceOperation::Extract,
                arguments: vec!["extract".to_owned(), path.to_owned()],
                reason: "Native extraction is supported; encryption, OCR need, safety limits, and cost remain separate observations.".to_owned(),
                cost: reports::AdviceCost::Unknown,
            })
        }
        reports::SupportStatus::IdentificationOnly => Some(reports::Advice {
            operation: reports::AdviceOperation::Inspect,
            arguments: vec!["inspect".to_owned(), path.to_owned()],
            reason: "Identification is available, but content extraction is unsupported; retain this metadata-only operation.".to_owned(),
            cost: reports::AdviceCost::Unknown,
        }),
        _ => None,
    };
    report.metadata.advice.extend(advice);
    report
}

/// Extracts exactly the requested native units into a canonical report.
///
/// Accepts a source path and extraction controls. Returns separate metadata,
/// selected content, provenance, and coverage using
/// [`hashing::HashPolicy::SampledAboveThreshold`]. Input, parser, selection,
/// safety, and local OCR failures are qualified in report diagnostics.
pub fn extract(path: &Path, options: &reports::ExtractOptions) -> reports::ExtractReport {
    extract_with_policy(path, options, hashing::HashPolicy::SampledAboveThreshold)
}

/// Extracts requested native units using an explicit [`hashing::HashPolicy`].
///
/// Accepts a source path, extraction controls, and a hash policy controlling
/// full SHA-256 versus sampled fingerprinting for the embedded metadata block.
/// Returns a canonical [`reports::ExtractReport`]; all operational failures remain
/// qualified in report diagnostics.
pub fn extract_with_policy(
    path: &Path,
    options: &reports::ExtractOptions,
    policy: hashing::HashPolicy,
) -> reports::ExtractReport {
    match Inspector::with_budget(&options.budget) {
        Ok(inspector) => inspector.with_hash_policy(policy).extract(path, options),
        Err(error) => unadmitted_extract(path, options, &error),
    }
}

/// Runs the transitional body-producing reader for explicit compatibility callers.
///
/// Accepts a source path and legacy content controls. Returns the legacy report
/// with bounded native/OCR content. Filesystem, parser, renderer, and OCR
/// failures are recorded in `report.errors`; canonical callers use `extract`.
pub fn extract_transitional(path: &Path, options: &InspectOptions) -> InspectReport {
    Inspector::default().extract_transitional(path, options)
}

/// Per-worker session with content-free inspection and explicitly owned native/OCR work.
///
/// Each operation owns one shared file lease. The lazy OCR session lives inside
/// that lease and never creates a second nested worker pool or cross-request model.
pub struct Inspector {
    ocr: images::OcrSession,
    hash_policy: hashing::HashPolicy,
}

impl Default for Inspector {
    /// Creates a sequential inspector with no loaded OCR model.
    ///
    /// Accepts no parameters. Returns an inspector owning one file/OCR slot and
    /// using [`hashing::HashPolicy::SampledAboveThreshold`]. No filesystem,
    /// PDFium, Tesseract, thread, or allocation-heavy work runs.
    fn default() -> Self {
        Self {
            ocr: images::OcrSession::default(),
            hash_policy: hashing::HashPolicy::SampledAboveThreshold,
        }
    }
}

/// Builds a failed metadata report without entering the file reader.
///
/// Accepts a source path and explanatory diagnostic. Returns a v2 failed report
/// with the path preserved. No source bytes, PDFium, OCR, or artifact I/O runs.
fn unadmitted_metadata(path: &Path, message: &str) -> reports::MetadataReport {
    let mut report = reports::MetadataReport::default();
    report.metadata.identity.path = path.to_path_buf();
    report.metadata.status = reports::OperationStatus::Failed;
    report.metadata.completeness = reports::Completeness::Partial;
    report.metadata.diagnostics.push(reports::Diagnostic {
        stage: reports::DiagnosticStage::Internal,
        severity: reports::DiagnosticSeverity::Error,
        code: "file_work_not_admitted".into(),
        message: message.to_owned(),
        unit_index: None,
    });
    report
}

/// Builds a failed extraction report without entering the file reader.
///
/// Accepts a source path, extraction controls, and an admission diagnostic.
/// Returns a canonical failed v2 extraction with no fabricated content. Input,
/// hashing, PDFium, OCR, and artifact stages remain unentered after rejection.
fn unadmitted_extract(
    path: &Path,
    options: &reports::ExtractOptions,
    message: &str,
) -> reports::ExtractReport {
    let metadata = unadmitted_metadata(path, message);
    let mut legacy = InspectReport {
        source_path: path.to_path_buf(),
        ..InspectReport::default()
    };
    legacy.error("work_budget", message);
    selection::assemble(metadata, legacy, options)
}

impl Inspector {
    /// Creates one worker session from public queue and job controls.
    ///
    /// Accepts a session budget. Returns an inspector sharing the declared
    /// positive file/OCR slots, or an error for zero before any file is opened.
    pub fn with_budget(budget: &reports::SessionBudget) -> Result<Self, String> {
        Ok(Self::with_shared_budget(SharedWorkBudget::new(
            budget.shared_job_slots,
        )?))
    }

    /// Creates one worker session attached to the batch's actual shared budget.
    ///
    /// Accepts a clone of the owned budget. Returns an inspector whose lazy OCR
    /// model and active file lease stay within this worker/session, defaulting to
    /// [`hashing::HashPolicy::SampledAboveThreshold`]. No model loads.
    pub fn with_shared_budget(budget: SharedWorkBudget) -> Self {
        Self {
            ocr: images::OcrSession::with_budget(budget),
            hash_policy: hashing::HashPolicy::SampledAboveThreshold,
        }
    }

    /// Configures whether this inspector streams full SHA-256 digests above the default size threshold.
    ///
    /// Accepts `full_hash` (`true` selects [`hashing::HashPolicy::Full`]; `false`
    /// selects [`hashing::HashPolicy::SampledAboveThreshold`]). Returns `self`
    /// for builder chaining without performing I/O or loading models.
    pub fn with_full_hash(mut self, full_hash: bool) -> Self {
        self.hash_policy = if full_hash {
            hashing::HashPolicy::Full
        } else {
            hashing::HashPolicy::SampledAboveThreshold
        };
        self
    }

    /// Configures an explicit [`hashing::HashPolicy`] on this inspector session.
    ///
    /// Accepts a hash policy. Returns `self` for builder chaining without
    /// performing I/O or changing the shared work budget.
    pub fn with_hash_policy(mut self, policy: hashing::HashPolicy) -> Self {
        self.hash_policy = policy;
        self
    }

    /// Returns the active [`hashing::HashPolicy`] configured on this inspector.
    ///
    /// Accepts `&self`. Returns the current hash policy. Performs no I/O.
    pub fn hash_policy(&self) -> hashing::HashPolicy {
        self.hash_policy
    }

    /// Inspects one file under the shared lease without body extraction or OCR.
    ///
    /// Accepts a source path and metadata options. Returns a typed metadata report
    /// honoring this inspector's configured [`hashing::HashPolicy`]; cancellation,
    /// input, hashing, and structural failures remain diagnostics.
    pub fn inspect(
        &mut self,
        path: &Path,
        options: &reports::InspectOptions,
    ) -> reports::MetadataReport {
        if !self.ocr.begin_file() {
            return finalize_public_inspection(unadmitted_metadata(
                path,
                "operation cancelled before file admission",
            ));
        }
        let report = if self.hash_policy == hashing::HashPolicy::SampledAboveThreshold {
            metadata::inspect(path, options)
        } else {
            metadata::inspect_with_policy(path, options, self.hash_policy)
        };
        self.ocr.end_file();
        finalize_public_inspection(report)
    }

    /// Extracts native units through the canonical reader and owned OCR session.
    ///
    /// Accepts a source path and extraction controls. Returns a typed report
    /// with source-order units, honest omissions, and metadata hashed under this
    /// inspector's configured [`hashing::HashPolicy`]. Rejected inputs (bad
    /// paths, link components, unopenable or non-regular sources) fail at the
    /// metadata stage and never reach the native readers. Cancellation, reader,
    /// model, safety, and input failures remain diagnostics instead of Rust
    /// exceptions.
    pub fn extract(
        &mut self,
        path: &Path,
        options: &reports::ExtractOptions,
    ) -> reports::ExtractReport {
        if !self.ocr.begin_file() {
            return unadmitted_extract(path, options, "operation cancelled before file admission");
        }
        let (mut metadata, expected_observation) = metadata::inspect_with_policy_and_observation(
            path,
            &reports::InspectOptions {
                thumbnail: options.thumbnail.clone(),
                ..reports::InspectOptions::default()
            },
            self.hash_policy,
        );
        let validated_path = metadata.metadata.identity.path.clone();
        // A rejected input (bad path, link component, unopenable or
        // non-regular source) already failed the metadata stage; it must not
        // reach the native readers, whose own path opens follow links.
        let input_rejected = metadata.metadata.diagnostics.iter().any(|diagnostic| {
            diagnostic.stage == reports::DiagnosticStage::Input
                && diagnostic.severity == reports::DiagnosticSeverity::Error
        });
        let mut legacy = if input_rejected {
            let mut rejected = InspectReport {
                source_path: validated_path.clone(),
                ..InspectReport::default()
            };
            rejected.error("input", "input was rejected before native extraction");
            rejected
        } else {
            let legacy_options = InspectOptions {
                identify: options.ocr.enabled,
                max_tokens: options.max_tokens,
                max_pages: None,
                max_chars: options.max_chars,
                max_units: options.max_units,
                native_selection: Some(options.selection.clone()),
                ocr_raster_long_edge: options.ocr.raster_long_edge,
                ocr_language: options.ocr.language.clone(),
                ..InspectOptions::default()
            };
            inspect_with_session(&validated_path, &legacy_options, &mut self.ocr)
        };
        if !input_rejected
            && let Some(ref expected) = expected_observation
            && !hashing::verify_identity(&validated_path, expected)
        {
            metadata::record_source_changed_during_read(&mut metadata.metadata);
        }
        self.ocr.end_file();
        if metadata.metadata.extraction_support == reports::SupportStatus::IdentificationOnly {
            legacy.errors.clear();
            selection::unsupported(&mut legacy);
        } else if legacy.dataset.is_some() || legacy.json.is_some() || legacy.xml.is_some() {
            selection::dataset_fallback(&mut legacy, &options.selection);
        }
        selection::assemble(metadata, legacy, options)
    }

    /// Extracts through the explicitly transitional legacy content adapter.
    ///
    /// Accepts a source path and legacy content controls. Returns the legacy
    /// body-producing report; cancellation, parser, filesystem, renderer, and
    /// OCR failures remain in `report.errors` for compatibility callers.
    pub fn extract_transitional(&mut self, path: &Path, options: &InspectOptions) -> InspectReport {
        if !self.ocr.begin_file() {
            let mut report = InspectReport {
                source_path: path.to_path_buf(),
                ..InspectReport::default()
            };
            report.error("work_budget", "operation cancelled before file admission");
            return report;
        }
        let report = inspect_with_session(path, options, &mut self.ocr);
        self.ocr.end_file();
        report
    }
}
/// Inspect one file while reusing the caller's lazy OCR session when supported.
///
/// Accepts a source path, inspection limits and output options, and a mutable
/// session. Returns a report with bounded identification and any supported
/// extraction. Filesystem, parser, renderer, and OCR failures are recorded in
/// `report.errors`; unsupported formats retain their static support boundary.
fn inspect_with_session(
    path: &Path,
    options: &InspectOptions,
    ocr: &mut images::OcrSession,
) -> InspectReport {
    let extension = detect::extension(path);
    let mut report = InspectReport {
        djvu: None,
        derived_preview: None,
        dataset: None,
        parquet: None,
        json: None,
        xml: None,
        source_path: path.to_path_buf(),
        artifacts: vec![],
        markdown: Markdown::default(),
        pdf: None,
        image: None,
        pages: vec![],
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
        tables: vec![],
        preflight: vec![],
        text: Text {
            origin: "native".into(),
            token_basis: "whitespace_words",
            ..Text::default()
        },
        thumbnail: None,
        errors: vec![],
        packet: None,
        native_headers: Vec::new(),
        native_units: Vec::new(),
        native_unit: reports::NativeUnit::Unknown,
        native_available: reports::QualifiedCount::default(),
    };
    // Enforce the validated no-follow regular-file boundary before any reader
    // opens the path; symbolic links and non-regular inputs are rejected with
    // the same precedence as the v2 hashing stage, so no reader can follow a
    // rejected link or block on a FIFO behind this gate.
    let validated_path = match crate::hashing::validate_regular_file(path) {
        Ok((absolute, bytes)) => {
            report.counts.bytes = bytes;
            report.source_path = absolute.clone();
            Some(absolute)
        }
        Err(error) => {
            report.error("input", error);
            None
        }
    };
    if let Some(path) = validated_path.as_deref()
        && report.errors.is_empty()
    {
        if let Err(error) = detect::detect(path, &mut report) {
            report.error("detection", error);
        } else if report.format.detected_mime.as_deref().is_some_and(|mime| {
            mime == "application/zip"
                || mime == "application/epub+zip"
                || mime.starts_with("application/vnd.openxmlformats-officedocument.")
                || mime.starts_with("application/vnd.oasis.opendocument.")
                || mime == "application/vnd.ms-excel.sheet.macroEnabled.12"
                || mime.starts_with("application/vnd.apple.")
        }) || (report.format.basis == "extension"
            && matches!(
                report.format.extension.as_str(),
                "zip"
                    | "cbz"
                    | "epub"
                    | "docx"
                    | "xlsx"
                    | "xlsm"
                    | "pptx"
                    | "odt"
                    | "ods"
                    | "odp"
                    | "pages"
                    | "numbers"
                    | "key"
            ))
        {
            match archive::Archive::open(path, &mut report) {
                Ok(mut archive) => {
                    office::inspect(&mut archive, &mut report, options);
                    epub::inspect(&mut archive, &mut report, options);
                    comics::cbz(&mut archive, &mut report, options);
                    iwork::inspect(&mut archive, &mut report, options, ocr);
                    archive::summarize(&mut report, options);
                    // Only allowlisted embedded preview entries may be read.
                    // LITX-AUD-012: fall back only when no thumbnail exists.
                    if options.thumbnail.is_some() && report.thumbnail.is_none() {
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
        } else if report
            .format
            .detected_mime
            .as_deref()
            .is_some_and(|m| m.contains("mobipocket") || m == "application/vnd.amazon.ebook")
            || (report.format.basis == "extension"
                && matches!(extension.as_str(), "mobi" | "azw" | "azw3" | "prc"))
        {
            mobi::inspect(path, &mut report, options);
        } else if report
            .format
            .detected_mime
            .as_deref()
            .is_some_and(|m| m.contains("djvu"))
            || (report.format.basis == "extension" && matches!(extension.as_str(), "djvu" | "djv"))
        {
            djvu::inspect(path, &mut report, options);
        } else if report
            .format
            .detected_mime
            .as_deref()
            .is_some_and(|m| m == "application/vnd.rar" || m == "application/x-rar-compressed")
            || (report.format.basis == "extension" && matches!(extension.as_str(), "cbr" | "rar"))
        {
            rar::inspect(path, &mut report);
        } else if report.format.detected_mime.as_deref() == Some("application/pdf")
            || (report.format.basis == "extension" && extension == "pdf")
        {
            pdf::inspect(path, &mut report, options, ocr);
        } else if report.format.bucket == "Images" {
            let mime = report.format.detected_mime.as_deref().unwrap_or("");
            if metadata::is_supported_raster_format(
                mime,
                &report.format.extension,
                &report.format.basis,
            ) {
                images::inspect(path, &mut report, options, ocr);
            } else if options.native_selection.is_some() {
                selection::unsupported(&mut report);
            }
        } else if report.format.detected_mime.as_deref() == Some("application/vnd.apache.parquet")
            || (report.format.basis == "extension" && extension == "parquet")
        {
            parquet::inspect(path, &mut report, options);
        } else if report.format.basis == "extension" && matches!(extension.as_str(), "csv" | "tsv")
            || matches!(
                report.format.detected_mime.as_deref(),
                Some("text/csv" | "text/tab-separated-values")
            )
        {
            tabular::csv(path, &mut report, options);
        } else if report.format.detected_mime.as_deref() == Some("application/json")
            || (report.format.basis == "extension" && extension == "json")
        {
            json_profile::inspect(path, &mut report, options);
        } else if matches!(
            report.format.detected_mime.as_deref(),
            Some("application/xml" | "text/xml")
        ) || (report.format.basis == "extension" && extension == "xml")
        {
            xml_profile::inspect(path, &mut report, options);
        } else if report.format.basis == "extension"
            && matches!(report.format.bucket.as_str(), "Text" | "Source Code")
        {
            text::inspect(path, &mut report, options);
        }
    }
    if options.probe_ocr && report.format.bucket != "Images" && report.image.is_none() {
        report.error(
            "probe_ocr",
            "OCR probing is supported for raster images only",
        );
    }
    if report.sheets.iter().any(|s| s.preview_truncated) {
        report.text.truncated = true;
    }
    // LITX-AUD-015: dataset preview limits are partial output too.
    if let Some(dataset) = &report.dataset {
        report.text.truncated |= dataset.has_more || dataset.values_truncated;
    }
    if let Some(budget) = options.token_budget()
        && truncate_words(&mut report.text.content, budget)
    {
        report.text.chars = report.text.content.chars().count();
        report.text.truncated = true;
    }
    report.text.tokens = report.text.content.split_whitespace().count();
    markdown::finish(&mut report, options);
    if options.packet {
        report.packet = Some(serde_json::json!({
            "Format": report.format.bucket, "Language": office::language(&report),
            "counts": report.counts, "excerpt": report.text.content,
        }));
    }
    report
}

impl InspectOptions {
    /// Pages where pagination exists, otherwise slides, sections, or spine items.
    pub(crate) fn text_unit_limit(&self) -> usize {
        self.max_pages.unwrap_or(usize::MAX)
    }

    fn token_budget(&self) -> Option<usize> {
        // Only an explicit word-token budget truncates; page limits never
        // imply one (LITX-AUD-008).
        self.max_tokens
    }
}

fn truncate_words(value: &mut String, budget: usize) -> bool {
    let mut words = value.split_whitespace();
    let mut end = 0;
    for word in words.by_ref().take(budget) {
        end = word.as_ptr() as usize - value.as_ptr() as usize + word.len();
    }
    if words.next().is_some() {
        value.truncate(end);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    /// Creates a unique ignored fixture directory for session boundary tests.
    ///
    /// Accepts a label. Returns a new target path. Creation failures panic; no
    /// prior evidence or user file is removed or overwritten.
    fn workdir(label: &str) -> std::path::PathBuf {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tests")
            .join(format!(
                "session-{label}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Creates extraction controls with OCR disabled for deterministic tests.
    ///
    /// Accepts a native selection. Returns default extraction options with the
    /// requested selection and no model, file, or network access.
    fn options(selection: reports::NativeSelection) -> reports::ExtractOptions {
        reports::ExtractOptions {
            selection,
            ocr: reports::OcrOptions {
                enabled: false,
                ..reports::OcrOptions::default()
            },
            ..reports::ExtractOptions::default()
        }
    }

    /// Proves a symlinked input is rejected before any native reader runs.
    ///
    /// Accepts no parameters. Returns unit after extracting through a symbolic
    /// link to a two-line text fixture. Any emitted unit, reader-owned count,
    /// or non-failed status panics; fixture I/O can panic.
    #[cfg(unix)]
    #[test]
    fn rejected_symlink_never_reaches_native_extraction() {
        let directory = workdir("symlink");
        let target = directory.join("target.txt");
        let link = directory.join("link.txt");
        std::fs::write(&target, "zero\none\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let report = extract(&link, &options(reports::NativeSelection::All));
        assert!(report.content.units.is_empty(), "reader followed the link");
        assert_eq!(report.metadata.status, reports::OperationStatus::Failed);
        assert!(
            report.selection.available.value.is_none(),
            "reader-owned line count was fabricated for a rejected input"
        );
        assert!(
            report
                .metadata
                .diagnostics
                .iter()
                .any(
                    |diagnostic| diagnostic.stage == reports::DiagnosticStage::Input
                        && diagnostic.severity == reports::DiagnosticSeverity::Error,
                )
        );
        assert_eq!(
            report.selection.omitted,
            reports::OmissionStatus::PartialFailure
        );
    }

    /// Proves a directory input is rejected before any native reader runs.
    ///
    /// Accepts no parameters. Returns unit after extracting a directory path.
    /// Any emitted unit, reader-owned count, or non-failed status panics.
    #[test]
    fn rejected_directory_never_reaches_native_extraction() {
        let directory = workdir("directory");
        let report = extract(&directory, &options(reports::NativeSelection::All));
        assert!(report.content.units.is_empty());
        assert_eq!(report.metadata.status, reports::OperationStatus::Failed);
        assert!(
            report
                .metadata
                .diagnostics
                .iter()
                .any(
                    |diagnostic| diagnostic.stage == reports::DiagnosticStage::Input
                        && diagnostic.severity == reports::DiagnosticSeverity::Error,
                )
        );
    }

    /// Proves the transitional adapter rejects symlinked inputs before readers.
    ///
    /// Accepts no parameters. Returns unit after a legacy extraction through a
    /// symbolic link. Any body content, native unit, or missing input error
    /// panics; fixture I/O can panic.
    #[cfg(unix)]
    #[test]
    fn transitional_symlink_input_is_rejected_before_readers() {
        let directory = workdir("transitional-symlink");
        let target = directory.join("target.txt");
        let link = directory.join("link.txt");
        std::fs::write(&target, "zero\none\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let report = extract_transitional(&link, &InspectOptions::default());
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.stage == "input" && error.message.contains("symbolic-link")),
            "{:?}",
            report.errors
        );
        assert!(report.text.content.is_empty());
        assert!(report.native_units.is_empty());
    }

    /// Explicit page limits must never imply a word-token budget; only an
    /// explicit `max_tokens` caps words (LITX-AUD-008).
    #[test]
    fn explicit_pages_without_max_tokens_means_no_token_budget() {
        let options = InspectOptions {
            identify: true,
            max_tokens: None,
            max_pages: Some(2),
            ..Default::default()
        };
        assert_eq!(options.token_budget(), None);
        let explicit = InspectOptions {
            max_tokens: Some(500),
            ..Default::default()
        };
        assert_eq!(explicit.token_budget(), Some(500));
    }

    #[test]
    #[ignore = "requires local eng.traineddata, set TESSDATA_PREFIX"]
    fn one_inspector_reuses_its_ocr_engine_between_files() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/image.png");
        let options = InspectOptions {
            probe_ocr: true,
            ..Default::default()
        };
        let mut inspector = Inspector::default();
        let first = inspector.extract_transitional(&path, &options);
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        let handle = inspector.ocr.api.as_ref().unwrap().handle.clone();
        let second = inspector.extract_transitional(&path, &options);
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        assert!(std::sync::Arc::ptr_eq(
            &handle,
            &inspector.ocr.api.as_ref().unwrap().handle
        ));
        assert_eq!(second.image.unwrap().probe.unwrap().word_count, 0);
    }
}

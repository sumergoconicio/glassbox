//! Module: glassbox-inspect versioned report contracts.
//! File: reports.rs.
//! Responsibility: Define typed inspection and extraction models without coupling metadata to body extraction.
//!
//! The crate-root legacy options and report remain operational while readers migrate.
//! No adapter projects a legacy body-producing report into [`MetadataReport`]: such a
//! projection would discard content without proving metadata-only execution.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Current public canonical schema v3 wire version (used by `InspectReportV3` and `ExtractReportV3`).
pub const REPORT_SCHEMA_VERSION: u32 = 3;
/// Legacy v2 wire version used when serializing [`MetadataReport`] and [`ExtractReport`] directly.
pub const LEGACY_REPORT_SCHEMA_VERSION: u32 = 2;
/// Default maximum thumbnail edge in pixels.
pub const DEFAULT_THUMBNAIL_MAX_EDGE: u32 = 320;
/// Approved default OCR raster long edge in pixels for later extraction readers.
pub const DEFAULT_OCR_RASTER_LONG_EDGE: u32 = 3072;

/// Distinguishes metadata inspection from explicit content extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Inspect,
    Extract,
}

/// Requested-operation outcome used by exit-status adapters; unknown is not final.
/// Intentional selection omissions can leave source coverage partial without making
/// a completely emitted requested operation partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Complete,
    Partial,
    Unsupported,
    Failed,
    #[default]
    Unknown,
}

/// Qualifies how completely a fact or payload was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Exact,
    Claimed,
    Partial,
    #[default]
    Unavailable,
}

/// Identifies the native unit used by counts, selection, and extracted content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NativeUnit {
    PhysicalPage,
    Slide,
    DocumentBlock,
    Paragraph,
    Table,
    PageBreak,
    SpineItem,
    MobiTextRecord,
    LogicalLine,
    DatasetPreview,
    DatasetRow,
    JsonScalar,
    XmlElement,
    Worksheet,
    Image,
    ArchiveEntry,
    DerivedPreviewUnit,
    Byte,
    PdfObject(PdfObjectKind),
    #[default]
    Unknown,
}

/// Native PDF object kinds that can be counted without extracting text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfObjectKind {
    Text,
    Image,
    Path,
    Shading,
    Form,
    Other,
    Unknown,
}

/// Identifies the mechanism that supplied a fact without embedding arbitrary payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    NativeStructure,
    EmbeddedProperty,
    ContainerIndex,
    DecoderMetadata,
    ByteScan,
    FileSystem,
    GeneratedArtifact,
    DerivedPreview,
    #[default]
    Unknown,
}

/// A count with an explicit native unit, source, and completeness qualification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct QualifiedCount {
    pub unit: NativeUnit,
    pub value: Option<u64>,
    pub source: FactSource,
    pub completeness: Completeness,
}

/// Supported digest algorithm for source identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DigestAlgorithm {
    #[default]
    Sha256,
}

/// Consistency between identity observations or between a digest and its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConsistencyStatus {
    Consistent,
    SourceChanged,
    Mismatched,
    #[default]
    Unavailable,
}

/// Explicitly states that before/after checks do not create an immutable snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotGuarantee {
    #[default]
    NotImmutable,
}

/// Domain-separated sampled file fingerprint used above the full-hash size threshold.
///
/// Fingerprints are comparable only against other fingerprints with the same
/// `scheme` and never substitute for a full-file `sha256` digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampledFingerprint {
    pub scheme: String,
    pub value: String,
}

/// Streamed digest facts and their relationship to the observed source identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DigestReport {
    pub algorithm: DigestAlgorithm,
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<SampledFingerprint>,
    pub bytes_hashed: QualifiedCount,
    pub consistency: ConsistencyStatus,
    pub source_identity: ConsistencyStatus,
    pub snapshot_guarantee: SnapshotGuarantee,
}

/// Source path and file identity facts; the opaque ID never contains file content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceIdentity {
    pub path: PathBuf,
    pub byte_size: Option<u64>,
    pub opaque_file_id: Option<String>,
    pub consistency: ConsistencyStatus,
}

/// Container identification facts without extracted text or preview values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FormatIdentity {
    pub extension: String,
    pub detected_mime: Option<String>,
    pub detected_name: Option<String>,
    pub bucket: String,
    pub basis: String,
    pub disagreement: bool,
    pub detector: String,
}

/// Permitted schema-identifier classes, including user-defined keys and column names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaKind {
    Document,
    Embedded,
    JsonPointer,
    XmlPath,
    Column,
    Worksheet,
    ArchiveEntry,
}

/// A schema name and its source, never a value from the identified location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaIdentifier {
    pub kind: SchemaKind,
    pub name: String,
    pub source: FactSource,
}

/// Allowlisted embedded-property fields that cannot address body or row payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddedMetadataField {
    Title,
    Subject,
    Author,
    Keywords,
    Creator,
    Producer,
    Language,
    CreatedAt,
    ModifiedAt,
    Application,
}

/// A sourced embedded property; body text and table cells have no field variant here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcedMetadata {
    pub field: EmbeddedMetadataField,
    pub value: String,
    pub source: FactSource,
    pub completeness: Completeness,
}

/// Encryption observation kept separate from extraction support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionStatus {
    NotEncrypted,
    EncryptedAccessible,
    EncryptedInaccessible,
    #[default]
    Unknown,
}

/// Extraction capability, which is independent of operation success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SupportStatus {
    Supported,
    IdentificationOnly,
    Unsupported,
    #[default]
    Unknown,
}

/// Native text-layer signal; presence does not claim readability or completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TextLayerStatus {
    Present,
    Absent,
    Mixed,
    #[default]
    Unknown,
}

/// Qualified OCR advice without OCR output or confidence payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OcrRecommendation {
    Recommended,
    NotRecommended,
    #[default]
    Unknown,
}

/// Explains why native evidence is partial or cannot support a stronger inference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uncertainty {
    pub code: String,
    pub message: String,
    pub source: FactSource,
}

/// PDF document security facts that do not imply body extraction support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PdfDocumentSignals {
    pub security_handler_revision: Option<i32>,
    pub permissions: Option<u64>,
}

/// Decoded raster pixel geometry for image-only documents.
///
/// COND-09: values are decoded pixel counts, never PDF-style points, and no
/// DPI is inferred, so these numbers cannot support a physical-size claim.
/// Raster pixels never enter the `width_points`/`height_points` fields of
/// [`NativePageSignal`], which stay reserved for PDF-style page geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RasterDimensions {
    pub width_pixels: u32,
    pub height_pixels: u32,
}

/// Page-local native object evidence with explicit coverage and uncertainty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativePageSignal {
    pub page_index: u64,
    pub text_layer: TextLayerStatus,
    pub ocr_recommended: OcrRecommendation,
    pub object_counts: Vec<QualifiedCount>,
    pub coverage: Completeness,
    pub uncertainties: Vec<Uncertainty>,
    pub width_points: Option<f32>,
    pub height_points: Option<f32>,
    pub rotation_quarter_turns: Option<i32>,
}

/// Stage identifier for a structured diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStage {
    Input,
    Detection,
    Hashing,
    Metadata,
    NativeSignals,
    Thumbnail,
    Selection,
    Extraction,
    Ocr,
    Artifact,
    Internal,
}

/// Diagnostic severity; intentionally unavailable optional metadata need not be an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// A structured stage diagnostic with an optional native-unit location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub stage: DiagnosticStage,
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
    pub unit_index: Option<u64>,
}

/// Report artifact classes; artifact records contain paths, not embedded file bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Thumbnail,
    MetadataJson,
    MetadataSummary,
    ExtractionJson,
    MarkdownContent,
}

/// A requested or generated artifact with source and dimensional metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub path: PathBuf,
    pub source: FactSource,
    pub completeness: Completeness,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Operation targeted by inert migration or follow-up advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdviceOperation {
    Inspect,
    Extract,
}

/// Cost remains unknown until a measured calibration supplies a stronger value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdviceCost {
    #[default]
    Unknown,
}

/// Structured argument-list advice; it is never a shell command and is never executed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advice {
    pub operation: AdviceOperation,
    pub arguments: Vec<String>,
    pub reason: String,
    pub cost: AdviceCost,
}

/// Metadata reader provenance with no OCR or extracted-content payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataProvenance {
    pub reader: String,
    pub stage: String,
    pub source: FactSource,
}

/// Shared typed metadata embedded in both v2 report envelopes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DocumentMetadata {
    pub identity: SourceIdentity,
    pub format: FormatIdentity,
    pub digest: DigestReport,
    pub counts: Vec<QualifiedCount>,
    pub schema_identifiers: Vec<SchemaIdentifier>,
    /// Namespace prefix-to-URI map (`ns0`, `ns1`, ...) for XML schema paths; omitted when empty.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub xml_namespaces: std::collections::BTreeMap<String, String>,
    pub embedded_metadata: Vec<SourcedMetadata>,
    pub text_layer: TextLayerStatus,
    pub ocr_recommended: OcrRecommendation,
    pub encryption: EncryptionStatus,
    pub extraction_support: SupportStatus,
    pub native_page_signals: Vec<NativePageSignal>,
    pub pdf: Option<PdfDocumentSignals>,
    /// Decoded raster pixel dimensions for image-only documents; the field is
    /// absent for every other format, so other v2 JSON outputs are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raster: Option<RasterDimensions>,
    pub status: OperationStatus,
    pub completeness: Completeness,
    pub diagnostics: Vec<Diagnostic>,
    pub artifacts: Vec<Artifact>,
    pub advice: Vec<Advice>,
    pub provenance: Vec<MetadataProvenance>,
}

/// Metadata-only report; it has no legacy report, body, rows, or OCR output field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetadataReport {
    pub schema_version: u32,
    pub kind: ReportKind,
    pub metadata: DocumentMetadata,
}

impl Default for MetadataReport {
    /// Creates an inspect-marked legacy-envelope report whose facts start unknown or unavailable.
    ///
    /// Accepts no parameters. Returns a metadata-only report stamped with
    /// [`LEGACY_REPORT_SCHEMA_VERSION`] (`2`) for its `metadata`-wrapper layout;
    /// project with [`MetadataReport::to_v3`] to obtain canonical schema v3 (`3`).
    /// This constructor performs no I/O and has no failure states.
    fn default() -> Self {
        Self {
            schema_version: LEGACY_REPORT_SCHEMA_VERSION,
            kind: ReportKind::Inspect,
            metadata: DocumentMetadata::default(),
        }
    }
}

/// Requested thumbnail output; rendering never implies OCR or body extraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailOptions {
    pub path: PathBuf,
    pub max_edge: u32,
}

/// One shared file/OCR concurrency budget for later sequential or batched sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBudget {
    /// Total file and OCR job slots, not a multiplier for each subsystem.
    pub shared_job_slots: usize,
    /// Maximum admitted requests waiting for a worker.
    pub queue_capacity: usize,
    /// Maximum completed out-of-order results retained before flushing.
    pub result_buffer_capacity: usize,
}

impl Default for SessionBudget {
    /// Creates the conservative sequential shared-session budget.
    ///
    /// Accepts no parameters. Returns one job slot and single-item queue/result bounds.
    /// This constructor performs no allocation or I/O and has no failure states.
    fn default() -> Self {
        Self {
            shared_job_slots: 1,
            queue_capacity: 1,
            result_buffer_capacity: 1,
        }
    }
}

/// Metadata inspection controls; content, selection, and OCR controls are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InspectOptions {
    pub thumbnail: Option<ThumbnailOptions>,
    pub budget: SessionBudget,
}

/// One-based inclusive native-unit range; readers reject zero or descending bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRange {
    pub start: u64,
    pub end: u64,
}

/// Full, first-N, or explicit-range selection in native source units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NativeSelection {
    #[default]
    All,
    FirstN {
        count: u64,
    },
    Ranges {
        ranges: Vec<NativeRange>,
    },
}

/// OCR controls retained only for explicit extraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrOptions {
    pub enabled: bool,
    pub language: String,
    pub raster_long_edge: u32,
}

impl Default for OcrOptions {
    /// Creates local OCR controls using the approved 3072-pixel raster edge.
    ///
    /// Accepts no parameters. Returns enabled English OCR options for extraction.
    /// This constructor loads no model and has no failure states.
    fn default() -> Self {
        Self {
            enabled: true,
            language: "eng".to_owned(),
            raster_long_edge: DEFAULT_OCR_RASTER_LONG_EDGE,
        }
    }
}

/// Extraction controls for later readers, including native selection and shared budgets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractOptions {
    pub selection: NativeSelection,
    pub max_tokens: Option<usize>,
    pub max_chars: usize,
    pub max_units: usize,
    pub thumbnail: Option<ThumbnailOptions>,
    pub ocr: OcrOptions,
    pub budget: SessionBudget,
}

impl Default for ExtractOptions {
    /// Creates full-document extraction defaults with existing preview safeguards.
    ///
    /// Accepts no parameters. Returns all-unit selection, a 64 MiB character cap,
    /// five dataset units, default OCR, and one shared job slot. No reader runs.
    fn default() -> Self {
        Self {
            selection: NativeSelection::All,
            max_tokens: None,
            max_chars: 64 << 20,
            max_units: 5,
            thumbnail: None,
            ocr: OcrOptions::default(),
            budget: SessionBudget::default(),
        }
    }
}

/// Reason that requested extraction coverage was not returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OmissionStatus {
    None,
    Selection,
    SafetyLimit,
    PartialFailure,
    Unsupported,
    #[default]
    Unknown,
}

/// Requested and observed native-unit selection with returned source-order indices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SelectionReport {
    pub requested: NativeSelection,
    pub unit: NativeUnit,
    pub available: QualifiedCount,
    pub returned_indices: Vec<u64>,
    pub omitted: OmissionStatus,
    pub completeness: Completeness,
}

/// Origin of extracted content, kept outside metadata reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentOrigin {
    Native,
    Ocr,
    Mixed,
    DerivedPreview,
}

/// OCR process provenance without OCR text; text belongs in [`ContentUnit`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrProvenance {
    pub engine: String,
    pub language: String,
    pub mean_confidence: Option<f64>,
    pub raster_long_edge: Option<u32>,
}

/// Extraction provenance with optional OCR process facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractionProvenance {
    pub reader: String,
    pub stage: String,
    pub source: FactSource,
    pub origin: ContentOrigin,
    pub ocr: Option<OcrProvenance>,
}

/// One extracted dataset row; values exist only in the explicit extraction content model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRow {
    pub row_index: u64,
    pub values: Vec<String>,
}

/// A selected native unit with explicit text/row payloads and provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentUnit {
    pub unit_index: u64,
    pub unit: NativeUnit,
    pub source_part: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional one-based source-order ordinal for typed structural markers
    /// (such as document tables and explicit page breaks) preserved across
    /// partial native-unit selections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_index: Option<u64>,
    pub text: Option<String>,
    pub rows: Vec<ContentRow>,
    pub provenance: Vec<ExtractionProvenance>,
    pub completeness: Completeness,
}

/// Explicit extracted payload kept separate from [`DocumentMetadata`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ExtractedContent {
    /// Budgeted column headers for row-stream tabular extractions (such as CSV/TSV).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    pub units: Vec<ContentUnit>,
}

/// Extraction report with content, provenance, source coverage, and selection.
/// Top-level `status` and `diagnostics` describe the extraction operation
/// independently of the embedded standalone inspection result in `metadata`.
/// Top-level `completeness` describes returned source coverage, independently of
/// whether the requested operation in `status` completed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractReport {
    pub schema_version: u32,
    pub kind: ReportKind,
    /// Extraction operation outcome, kept independent of `metadata.status`.
    #[serde(default)]
    pub status: OperationStatus,
    /// Extraction-stage diagnostics (selection, limits, reader/OCR, and artifact errors).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    pub metadata: DocumentMetadata,
    pub content: ExtractedContent,
    pub provenance: Vec<ExtractionProvenance>,
    pub completeness: Completeness,
    pub selection: SelectionReport,
}

impl ExtractReport {
    /// Returns the effective extraction operation status, falling back to
    /// `metadata.status` only for legacy deserialized reports whose top-level
    /// `status` defaulted to [`OperationStatus::Unknown`].
    ///
    /// Accepts `&self`. Returns the resolved [`OperationStatus`]. Performs no
    /// I/O and cannot fail.
    pub fn extraction_status(&self) -> OperationStatus {
        if self.status == OperationStatus::Unknown {
            self.metadata.status
        } else {
            self.status
        }
    }
}

impl Default for ExtractReport {
    /// Creates an extract-marked legacy-envelope report with empty content and unavailable facts.
    ///
    /// Accepts no parameters. Returns [`LEGACY_REPORT_SCHEMA_VERSION`] (`2`) for the
    /// legacy `metadata`-wrapper layout and an all-unit unknown selection; project with
    /// [`ExtractReport::to_v3`] to obtain canonical schema v3 (`3`).
    /// This constructor performs no extraction or I/O and has no failure states.
    fn default() -> Self {
        Self {
            schema_version: LEGACY_REPORT_SCHEMA_VERSION,
            kind: ReportKind::Extract,
            status: OperationStatus::Unknown,
            diagnostics: Vec::new(),
            metadata: DocumentMetadata::default(),
            content: ExtractedContent::default(),
            provenance: Vec::new(),
            completeness: Completeness::Unavailable,
            selection: SelectionReport::default(),
        }
    }
}

//! Module: glassbox-inspect schema v3 report projections.
//! File: reports_v3.rs.
//! Responsibility: Project internal [`crate::reports::MetadataReport`] and [`crate::reports::ExtractReport`] records into the clean, keyed schema v3 JSON contract with RFC 3339 date normalization and strict content-free guarantees.

pub use crate::detect::{
    ALL_FORMAT_TYPES, format_type_for_extension, format_type_for_file_format, normalize_extension,
};
use crate::reports::{
    Artifact, Completeness, Diagnostic, DiagnosticSeverity, DiagnosticStage, DocumentMetadata,
    EmbeddedMetadataField, EncryptionStatus, ExtractReport, ExtractedContent, ExtractionProvenance,
    MetadataProvenance, MetadataReport, NativeUnit, OcrRecommendation, OperationStatus,
    RasterDimensions, SampledFingerprint, SchemaKind, SelectionReport, SupportStatus,
    TextLayerStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// Schema version emitted by all v3 inspection and extraction JSON projections.
pub const REPORT_SCHEMA_VERSION_V3: u32 = 3;

/// Top-level operational status in schema v3 (`complete | partial | unsupported | failed | unknown`).
pub type ReportStatus = OperationStatus;

/// Artifact record emitted in schema v3 when files are written to disk.
pub type ArtifactRecord = Artifact;

/// Reader provenance list emitted in schema v3 inspect reports only under `--verbose`.
pub type ProvenanceV3 = Vec<MetadataProvenance>;

/// Helper for serde to omit `extension_mismatch` unless it is explicitly `Some(true)`.
///
/// Accepts a reference to an optional boolean flag. Returns `true` when the
/// field should be skipped during serialization (`None` or `Some(false)`).
/// Performs no allocation or I/O and cannot fail.
fn is_not_true(value: &Option<bool>) -> bool {
    *value != Some(true)
}

/// File identity, size, and digest/fingerprint facts in schema v3.
///
/// Carries either full-file `sha256` or sampled `fingerprint`, never both, and
/// omits `consistent` (source mutations withhold both fields and emit a diagnostic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBlockV3 {
    pub path: String,
    pub name: String,
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<SampledFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
}

/// Format classification and literal filename suffix facts in schema v3.
///
/// `format_type` serializes as `"type"` using one of the 23 lowercase identifiers
/// in [`ALL_FORMAT_TYPES`]. `extension_mismatch` is serialized only when `Some(true)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatBlockV3 {
    #[serde(rename = "type")]
    pub format_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
    #[serde(default, skip_serializing_if = "is_not_true")]
    pub extension_mismatch: Option<bool>,
}

/// Embedded authoring/producer software properties kept strictly separate from human authors.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SoftwareBlockV3 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
}

impl SoftwareBlockV3 {
    /// Reports whether every software property is absent.
    ///
    /// Accepts `&self`. Returns `true` when `application`, `creator_tool`, and
    /// `producer` are all `None`. Performs no I/O and cannot fail.
    pub fn is_empty(&self) -> bool {
        self.application.is_none() && self.creator_tool.is_none() && self.producer.is_none()
    }
}

/// Verbatim embedded header properties in schema v3; body/row/OCR values never appear here.
///
/// Always present on [`InspectReportV3`] and serializes as `{}` when all optional fields are empty.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DescriptionBlockV3 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub software: Option<SoftwareBlockV3>,
}

impl DescriptionBlockV3 {
    /// Reports whether every descriptive metadata field is absent or empty.
    ///
    /// Accepts `&self`. Returns `true` when serializing this block produces `{}`.
    /// Performs no I/O and cannot fail.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.authors.is_empty()
            && self.subject.is_none()
            && self.keywords.is_empty()
            && self.language.is_none()
            && self.created_at.is_none()
            && self.modified_at.is_none()
            && self.software.as_ref().is_none_or(SoftwareBlockV3::is_empty)
    }
}

/// Grouped schema and container identifiers without cell, scalar, or body values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StructureNamesV3 {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sheets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub xml_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub xml_namespaces: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub json_pointers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedded: Vec<String>,
}

impl StructureNamesV3 {
    /// Reports whether every schema identifier group is empty.
    ///
    /// Accepts `&self`. Returns `true` when no names were collected in any group.
    /// Performs no I/O and cannot fail.
    pub fn is_empty(&self) -> bool {
        self.sheets.is_empty()
            && self.columns.is_empty()
            && self.entries.is_empty()
            && self.xml_paths.is_empty()
            && self.xml_namespaces.is_empty()
            && self.json_pointers.is_empty()
            && self.documents.is_empty()
            && self.embedded.is_empty()
    }
}

/// Optional PDF structural geometry and security signals in schema v3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PdfStructureV3 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_points: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_points: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uniform_page_size: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_handler_revision: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<u64>,
}

/// Keyed native-unit counts, optional grouped identifier names, and optional format geometry.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StructureBlockV3 {
    #[serde(default)]
    pub counts: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<StructureNamesV3>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfStructureV3>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<RasterDimensions>,
}

/// Single-hop extraction readiness summary in schema v3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessBlockV3 {
    pub extractable: String,
    pub encrypted: String,
    pub text_layer: String,
    pub ocr: String,
}

/// Canonical schema v3 metadata-only inspection report.
///
/// Omits `next_step`/`advice` completely, omits `diagnostics` and `artifacts`
/// when empty, and emits `provenance` only when projected with `verbose == true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectReportV3 {
    pub schema_version: u32,
    pub kind: String,
    pub status: ReportStatus,
    pub file: FileBlockV3,
    pub format: FormatBlockV3,
    pub description: DescriptionBlockV3,
    pub structure: StructureBlockV3,
    pub readiness: ReadinessBlockV3,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceV3>,
}

/// Canonical schema v3 extraction report embedding the v3 inspection metadata projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractReportV3 {
    pub schema_version: u32,
    pub kind: String,
    pub status: ReportStatus,
    pub completeness: Completeness,
    pub metadata: InspectReportV3,
    pub selection: SelectionReport,
    pub content: ExtractedContent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Vec<ExtractionProvenance>>,
}

impl MetadataReport {
    /// Projects this internal metadata report into the canonical schema v3 envelope.
    ///
    /// Accepts `verbose`, which controls whether reader `provenance` is included.
    /// Returns [`InspectReportV3`] without performing any I/O or modifying `self`.
    pub fn to_v3(&self, verbose: bool) -> InspectReportV3 {
        inspect_to_v3(self, verbose)
    }
}

impl ExtractReport {
    /// Projects this internal extraction report into the canonical schema v3 envelope.
    ///
    /// Accepts `verbose`, which controls whether reader and extraction `provenance`
    /// blocks are included. Returns [`ExtractReportV3`] without performing any I/O.
    pub fn to_v3(&self, verbose: bool) -> ExtractReportV3 {
        extract_to_v3(self, verbose)
    }
}

/// Projects a [`MetadataReport`] into the canonical schema v3 inspection contract.
///
/// Accepts a metadata report and `verbose` flag (`true` includes reader provenance).
/// Returns [`InspectReportV3`] with keyed `file`, `format`, `description`, `structure`,
/// and `readiness` blocks, normalized RFC 3339 timestamps, and conditional diagnostics,
/// artifacts, and provenance. Performs no I/O and cannot fail.
pub fn inspect_to_v3(report: &MetadataReport, verbose: bool) -> InspectReportV3 {
    document_metadata_to_v3(&report.metadata, verbose)
}

/// Projects an [`ExtractReport`] into the canonical schema v3 extraction contract.
///
/// Accepts an extraction report and `verbose` flag (`true` includes reader and
/// extraction provenance). Returns [`ExtractReportV3`] embedding the independent
/// standalone v3 metadata projection alongside `selection`, `content`, top-level
/// extraction `status`, combined top-level `diagnostics`, `artifacts`, and
/// optional `provenance`. Performs no I/O and cannot fail.
pub fn extract_to_v3(report: &ExtractReport, verbose: bool) -> ExtractReportV3 {
    let metadata = inspect_to_v3(
        &MetadataReport {
            schema_version: crate::reports::LEGACY_REPORT_SCHEMA_VERSION,
            kind: crate::reports::ReportKind::Inspect,
            metadata: report.metadata.clone(),
        },
        verbose,
    );
    let mut diagnostics = metadata.diagnostics.clone();
    diagnostics.extend(report.diagnostics.clone());
    let artifacts = metadata.artifacts.clone();
    ExtractReportV3 {
        schema_version: REPORT_SCHEMA_VERSION_V3,
        kind: "extract".to_owned(),
        status: report.extraction_status(),
        completeness: report.completeness,
        metadata,
        selection: report.selection.clone(),
        content: report.content.clone(),
        diagnostics,
        artifacts,
        provenance: verbose.then(|| report.provenance.clone()),
    }
}

/// Projects a [`DocumentMetadata`] payload into [`InspectReportV3`].
///
/// Accepts internal document metadata and `verbose`. Returns the schema v3 inspect
/// envelope, appending any `date_unnormalized` info diagnostics emitted during
/// embedded timestamp normalization. Performs no I/O and cannot fail.
fn document_metadata_to_v3(metadata: &DocumentMetadata, verbose: bool) -> InspectReportV3 {
    let mut diagnostics = metadata.diagnostics.clone();

    let file = FileBlockV3 {
        path: metadata.identity.path.to_string_lossy().into_owned(),
        name: metadata
            .identity
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        bytes: metadata.identity.byte_size,
        sha256: metadata.digest.digest.clone(),
        fingerprint: metadata.digest.fingerprint.clone(),
        file_id: metadata.identity.opaque_file_id.clone(),
    };

    let literal_extension = if metadata.format.extension.is_empty() {
        crate::detect::normalize_extension(&metadata.identity.path)
    } else {
        Some(metadata.format.extension.clone())
    };
    let mut format_type = crate::detect::bucket_to_format_type(&metadata.format.bucket);
    if format_type == "other"
        && metadata.format.basis != "bytes"
        && let Some(ext) = literal_extension.as_deref()
    {
        format_type = crate::detect::format_type_for_extension(ext);
    }
    let format = FormatBlockV3 {
        format_type: format_type.to_owned(),
        mime: metadata
            .format
            .detected_mime
            .clone()
            .filter(|mime| !mime.is_empty()),
        extension: literal_extension,
        extension_mismatch: metadata.format.disagreement.then_some(true),
    };

    let description = build_description(metadata, &mut diagnostics);
    let structure = build_structure(metadata);
    let readiness = build_readiness(metadata, format_type);

    InspectReportV3 {
        schema_version: REPORT_SCHEMA_VERSION_V3,
        kind: "inspect".to_owned(),
        status: metadata.status,
        file,
        format,
        description,
        structure,
        readiness,
        diagnostics,
        artifacts: metadata.artifacts.clone(),
        provenance: verbose.then(|| metadata.provenance.clone()),
    }
}

/// Builds the keyed v3 description block from allowlisted embedded metadata properties.
///
/// Accepts internal metadata and a mutable diagnostics vector for recording
/// `date_unnormalized` info diagnostics when embedded timestamps cannot be parsed.
/// Returns [`DescriptionBlockV3`] with software tools separated from human authors.
/// Performs no I/O and cannot fail.
fn build_description(
    metadata: &DocumentMetadata,
    diagnostics: &mut Vec<Diagnostic>,
) -> DescriptionBlockV3 {
    let mut description = DescriptionBlockV3::default();
    let mut software = SoftwareBlockV3::default();
    let mut seen_authors: HashSet<&str> = HashSet::new();
    let mut seen_keywords: HashSet<&str> = HashSet::new();

    for property in &metadata.embedded_metadata {
        let value = property.value.trim();
        if value.is_empty() {
            continue;
        }
        match property.field {
            EmbeddedMetadataField::Title => {
                if description.title.is_none() {
                    description.title = Some(value.to_owned());
                }
            }
            EmbeddedMetadataField::Subject => {
                if description.subject.is_none() {
                    description.subject = Some(value.to_owned());
                }
            }
            EmbeddedMetadataField::Author => {
                if seen_authors.insert(value) {
                    description.authors.push(value.to_owned());
                }
            }
            EmbeddedMetadataField::Keywords => {
                for keyword in value
                    .split([',', ';'])
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                {
                    if seen_keywords.insert(keyword) {
                        description.keywords.push(keyword.to_owned());
                    }
                }
            }
            EmbeddedMetadataField::Language => {
                if description.language.is_none() {
                    description.language = Some(value.to_owned());
                }
            }
            EmbeddedMetadataField::CreatedAt => {
                if description.created_at.is_none() {
                    description.created_at =
                        Some(normalize_or_record("created_at", value, diagnostics));
                }
            }
            EmbeddedMetadataField::ModifiedAt => {
                if description.modified_at.is_none() {
                    description.modified_at =
                        Some(normalize_or_record("modified_at", value, diagnostics));
                }
            }
            EmbeddedMetadataField::Application => {
                if software.application.is_none() {
                    software.application = Some(value.to_owned());
                }
            }
            EmbeddedMetadataField::Creator => {
                if software.creator_tool.is_none() {
                    software.creator_tool = Some(value.to_owned());
                }
            }
            EmbeddedMetadataField::Producer => {
                if software.producer.is_none() {
                    software.producer = Some(value.to_owned());
                }
            }
        }
    }

    if !software.is_empty() {
        description.software = Some(software);
    }
    description
}

/// Normalizes an embedded timestamp field or records an info `date_unnormalized` diagnostic.
///
/// Accepts the logical field name (`"created_at"` or `"modified_at"`), the trimmed
/// raw value, and the mutable diagnostic list. Returns the normalized RFC 3339 or
/// valid date-only string, or the raw input verbatim after pushing a `date_unnormalized`
/// info diagnostic. Performs no I/O and cannot fail.
fn normalize_or_record(field_name: &str, raw: &str, diagnostics: &mut Vec<Diagnostic>) -> String {
    match normalize_metadata_date(raw) {
        Some(normalized) => normalized,
        None => {
            diagnostics.push(Diagnostic {
                stage: DiagnosticStage::Metadata,
                severity: DiagnosticSeverity::Info,
                code: "date_unnormalized".to_owned(),
                message: format!(
                    "embedded {field_name} timestamp could not be normalized to RFC 3339; retained raw value"
                ),
                unit_index: None,
            });
            raw.to_owned()
        }
    }
}

/// Maps one [`NativeUnit`] to its plural schema v3 `structure.counts` key.
///
/// Accepts a native unit enum value. Returns `Some(plural_key)` for countable
/// document units and `None` for byte counts, PDF object counters, and unknown units.
/// Performs no I/O and cannot fail.
fn native_unit_count_key(unit: &NativeUnit) -> Option<&'static str> {
    match unit {
        NativeUnit::PhysicalPage => Some("pages"),
        NativeUnit::Worksheet => Some("sheets"),
        NativeUnit::Slide => Some("slides"),
        NativeUnit::Table => Some("tables"),
        NativeUnit::SpineItem => Some("chapters"),
        NativeUnit::Paragraph => Some("paragraphs"),
        NativeUnit::DocumentBlock => Some("blocks"),
        NativeUnit::LogicalLine => Some("lines"),
        NativeUnit::ArchiveEntry => Some("entries"),
        NativeUnit::DatasetRow => Some("rows"),
        NativeUnit::DatasetPreview | NativeUnit::DerivedPreviewUnit => Some("previews"),
        NativeUnit::Image => Some("images"),
        NativeUnit::JsonScalar => Some("scalars"),
        NativeUnit::XmlElement => Some("elements"),
        NativeUnit::MobiTextRecord => Some("records"),
        NativeUnit::PageBreak => Some("page_breaks"),
        NativeUnit::Byte | NativeUnit::PdfObject(_) | NativeUnit::Unknown => None,
    }
}

/// Escapes one JSON object key token according to RFC 6901 §3 (`~` -> `~0`, `/` -> `~1`).
///
/// Accepts a raw JSON property key. Returns the escaped RFC 6901 reference
/// token. Performs no I/O and cannot fail.
fn escape_json_pointer_token(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    for ch in token.chars() {
        match ch {
            '~' => out.push_str("~0"),
            '/' => out.push_str("~1"),
            _ => out.push(ch),
        }
    }
    out
}

/// Converts an internal `json_profile` schema path into a slash-delimited
/// RFC 6901-escaped schema pointer using `*` as the array-element wildcard.
///
/// Accepts a raw schema path from `json_profile` (`"a.b"`, `"[\"a.b\"]"`,
/// `"items[].id"`, `"[\"k/s~t\"]"`) or an already-formatted leading-slash
/// pointer. Returns `Some(pointer)` with `~` escaped as `~0`, `/` escaped as
/// `~1`, and every `[]` array step emitted as `/*`, or `None` for the empty
/// root-scalar path `""`. Performs no I/O and never panics on malformed input.
fn json_schema_path_to_pointer(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with('/') {
        return Some(raw.to_owned());
    }
    let mut out = String::with_capacity(raw.len() + 4);
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(after_dot) = rest.strip_prefix('.') {
            rest = after_dot;
            continue;
        }
        if let Some(after_array) = rest.strip_prefix("[]") {
            out.push_str("/*");
            rest = after_array;
            continue;
        }
        if let Some(after_bracket) = rest.strip_prefix('[') {
            let mut end = None;
            let bytes = after_bracket.as_bytes();
            if bytes.first() == Some(&b'"') {
                let mut i = 1;
                let mut escaped = false;
                while i < bytes.len() {
                    if escaped {
                        escaped = false;
                    } else if bytes[i] == b'\\' {
                        escaped = true;
                    } else if bytes[i] == b'"' {
                        end = Some(i + 1);
                        break;
                    }
                    i += 1;
                }
            }
            if let Some(quote_end) = end
                && after_bracket[quote_end..].starts_with(']')
                && let Ok(key) = serde_json::from_str::<String>(&after_bracket[..quote_end])
            {
                out.push('/');
                out.push_str(&escape_json_pointer_token(&key));
                rest = &after_bracket[quote_end + 1..];
                continue;
            }
            let stop = after_bracket.find(']').unwrap_or(after_bracket.len());
            out.push('/');
            out.push_str(&escape_json_pointer_token(&after_bracket[..stop]));
            rest = after_bracket.get(stop + 1..).unwrap_or("");
            continue;
        }
        let stop = rest.find(['.', '[']).unwrap_or(rest.len());
        let segment = &rest[..stop];
        out.push('/');
        out.push_str(&escape_json_pointer_token(segment));
        rest = &rest[stop..];
    }
    Some(out)
}

/// Builds the keyed v3 structure block from internal counts, schema identifiers, and geometry.
///
/// Accepts internal metadata. Returns [`StructureBlockV3`] with plural keyed counts
/// (including `columns` when column names are reported), grouped identifier names
/// omitted when empty (including RFC 6901-escaped `json_pointers` and any
/// `xml_namespaces` prefix-to-URI map), and optional `pdf` / `image` geometry
/// blocks. Performs no I/O.
fn build_structure(metadata: &DocumentMetadata) -> StructureBlockV3 {
    let mut counts = BTreeMap::new();
    for count in &metadata.counts {
        if let (Some(key), Some(value)) = (native_unit_count_key(&count.unit), count.value) {
            counts.insert(key.to_owned(), value);
        }
    }

    let mut names = StructureNamesV3::default();
    for identifier in &metadata.schema_identifiers {
        match identifier.kind {
            SchemaKind::Worksheet => names.sheets.push(identifier.name.clone()),
            SchemaKind::Column => names.columns.push(identifier.name.clone()),
            SchemaKind::ArchiveEntry => names.entries.push(identifier.name.clone()),
            SchemaKind::XmlPath => names.xml_paths.push(identifier.name.clone()),
            SchemaKind::JsonPointer => {
                if let Some(pointer) = json_schema_path_to_pointer(&identifier.name) {
                    names.json_pointers.push(pointer);
                }
            }
            SchemaKind::Document => names.documents.push(identifier.name.clone()),
            SchemaKind::Embedded => names.embedded.push(identifier.name.clone()),
        }
    }
    names.xml_namespaces = metadata.xml_namespaces.clone();
    if !names.columns.is_empty() {
        counts
            .entry("columns".to_owned())
            .or_insert(names.columns.len() as u64);
    }

    let pdf = build_pdf_structure(metadata, counts.get("pages").copied());

    StructureBlockV3 {
        counts,
        names: (!names.is_empty()).then_some(names),
        pdf,
        image: metadata.raster,
    }
}

/// Builds optional PDF geometry and security details from native PDF signals.
///
/// Accepts internal metadata and the observed physical page count if available.
/// Returns `Some(PdfStructureV3)` when PDF signals or native page signals exist,
/// or `None` for non-PDF files. `uniform_page_size` is `Some(false)` whenever
/// observed pages differ, `Some(true)` only when every page in the document has
/// comparable observed geometry, and `None` (omitted) when scan coverage is
/// incomplete (such as a capped scan or missing page geometry). Performs no I/O
/// and cannot fail.
fn build_pdf_structure(
    metadata: &DocumentMetadata,
    page_count: Option<u64>,
) -> Option<PdfStructureV3> {
    if metadata.pdf.is_none() && metadata.native_page_signals.is_empty() {
        return None;
    }
    let dims: Vec<(f32, f32)> = metadata
        .native_page_signals
        .iter()
        .filter_map(|signal| Some((signal.width_points?, signal.height_points?)))
        .collect();
    let observed_count = dims.len() as u64;
    let scan_incomplete = metadata
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "pdf_page_signals_capped")
        || dims.len() < metadata.native_page_signals.len()
        || page_count.is_some_and(|total| total > 0 && observed_count < total);
    let (width_points, height_points, uniform_page_size) = match dims.first().copied() {
        Some((first_w, first_h)) => {
            let all_match = dims[1..]
                .iter()
                .all(|&(w, h)| (w - first_w).abs() < 0.01 && (h - first_h).abs() < 0.01);
            let uniform = if !all_match {
                Some(false)
            } else if scan_incomplete {
                None
            } else {
                Some(true)
            };
            (Some(first_w), Some(first_h), uniform)
        }
        None => (None, None, None),
    };
    let (security_handler_revision, permissions) = match &metadata.pdf {
        Some(pdf) => (pdf.security_handler_revision, pdf.permissions),
        None => (None, None),
    };
    Some(PdfStructureV3 {
        pages: page_count,
        width_points,
        height_points,
        uniform_page_size,
        security_handler_revision,
        permissions,
    })
}

/// Maps internal support, encryption, text-layer, and OCR signals into [`ReadinessBlockV3`].
///
/// Accepts internal metadata and the resolved lowercase `format.type` identifier.
/// Returns the exact v3 readiness strings (`extractable`, `encrypted`, `text_layer`,
/// `ocr`), mapping non-textual containers to `text_layer: "not_applicable"` and
/// `ocr: "not_needed"` when OCR recommendation is unknown. Performs no I/O.
fn build_readiness(metadata: &DocumentMetadata, format_type: &str) -> ReadinessBlockV3 {
    let extractable = match metadata.extraction_support {
        SupportStatus::Supported => "supported",
        SupportStatus::IdentificationOnly => "identification_only",
        SupportStatus::Unsupported | SupportStatus::Unknown => "unsupported",
    };

    let encrypted = match metadata.encryption {
        EncryptionStatus::NotEncrypted => "none",
        EncryptionStatus::EncryptedAccessible => "accessible",
        EncryptionStatus::EncryptedInaccessible => "locked",
        EncryptionStatus::Unknown => "unknown",
    };

    let text_layer_not_applicable = matches!(
        format_type,
        "audio" | "video" | "archives" | "databases" | "fonts" | "3d_cad" | "applications"
    ) || (format_type == "other"
        && metadata.text_layer == TextLayerStatus::Unknown
        && metadata.native_page_signals.is_empty()
        && metadata.raster.is_none());

    let text_layer = if text_layer_not_applicable {
        "not_applicable"
    } else {
        match metadata.text_layer {
            TextLayerStatus::Present => "present",
            TextLayerStatus::Mixed => "mixed",
            TextLayerStatus::Absent => "absent",
            TextLayerStatus::Unknown => "unknown",
        }
    };

    let ocr = match metadata.ocr_recommended {
        OcrRecommendation::NotRecommended => "not_needed",
        OcrRecommendation::Recommended => {
            if metadata.text_layer == TextLayerStatus::Absent {
                "required"
            } else {
                "recommended"
            }
        }
        OcrRecommendation::Unknown => {
            if text_layer == "not_applicable" {
                "not_needed"
            } else {
                "unknown"
            }
        }
    };

    ReadinessBlockV3 {
        extractable: extractable.to_owned(),
        encrypted: encrypted.to_owned(),
        text_layer: text_layer.to_owned(),
        ocr: ocr.to_owned(),
    }
}

/// Normalizes an embedded PDF or ISO-8601/W3CDTF metadata date without external dependencies.
///
/// Accepts a raw embedded date string.
/// - Non-ASCII or empty strings are rejected immediately without panicking.
/// - Valid full timestamps in PDF format (`D:YYYY[MM[DD[HH[MM[SS[Z|±HH'MM'|±HH]]]]]]`,
///   tolerating optional trailing `'` and optional `D:` prefix) and ISO-8601 / W3CDTF
///   format (`YYYY-MM-DD[T| ]HH:MM[:SS[.frac]][Z|±HH:MM|±HHMM|±HH]`, allowing fractional
///   digits only when seconds `:SS` are present) are validated against calendar bounds
///   (including leap years) and returned as `Some(rfc3339)`.
/// - Valid date-only strings (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`) are validated and
///   returned verbatim as `Some(raw)`.
/// - Malformed or out-of-range strings return `None` so callers can retain the
///   raw string verbatim and emit a `date_unnormalized` info diagnostic.
pub fn normalize_metadata_date(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.is_ascii() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    if let Some(date_only) = parse_date_only(bytes) {
        return date_only;
    }
    if bytes.starts_with(b"D:") || looks_like_compact_pdf_date(bytes) {
        return parse_pdf_date(bytes);
    }
    parse_iso_timestamp(bytes)
}

/// Returns whether a Gregorian calendar year is a leap year.
///
/// Accepts a four-digit year. Returns `true` for leap years. Cannot fail.
fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// Returns the number of calendar days in `(year, month)`.
///
/// Accepts year and 1-based month. Returns `Some(28..=31)` for months `1..=12`,
/// or `None` for out-of-range months. Cannot fail.
fn days_in_month(year: u32, month: u32) -> Option<u32> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if is_leap_year(year) { 29 } else { 28 }),
        _ => None,
    }
}

/// Parses an exact-length ASCII decimal integer from a byte slice.
///
/// Accepts a byte slice whose bytes must all be ASCII digits. Returns `Some(u32)`
/// or `None` on empty, non-ASCII-digit, or overflowing input. Performs no I/O
/// and cannot panic on arbitrary byte slices.
fn parse_ascii_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut acc: u32 = 0;
    for &b in bytes {
        acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(acc)
}

/// Validates and parses a date-only byte slice (`YYYY`, `YYYY-MM`, or `YYYY-MM-DD`).
///
/// Accepts a trimmed ASCII byte slice. Returns `Some(Some(verbatim))` for valid
/// date-only inputs, `Some(None)` when the shape matches date-only syntax with
/// invalid calendar bounds (such as `"2023-02-30"`), or `None` when the input
/// does not match a date-only shape. Cannot panic on arbitrary byte slices.
fn parse_date_only(bytes: &[u8]) -> Option<Option<String>> {
    match bytes.len() {
        4 if bytes.iter().all(u8::is_ascii_digit) => {
            let year = parse_ascii_digits(bytes)?;
            Some(Some(format!("{year:04}")))
        }
        7 if bytes[4] == b'-' => {
            let year = parse_ascii_digits(&bytes[0..4])?;
            let month = parse_ascii_digits(&bytes[5..7])?;
            if days_in_month(year, month).is_some() {
                Some(Some(format!("{year:04}-{month:02}")))
            } else {
                Some(None)
            }
        }
        10 if bytes[4] == b'-' && bytes[7] == b'-' => {
            let year = parse_ascii_digits(&bytes[0..4])?;
            let month = parse_ascii_digits(&bytes[5..7])?;
            let day = parse_ascii_digits(&bytes[8..10])?;
            let Some(max_day) = days_in_month(year, month) else {
                return Some(None);
            };
            if (1..=max_day).contains(&day) {
                Some(Some(format!("{year:04}-{month:02}-{day:02}")))
            } else {
                Some(None)
            }
        }
        _ => None,
    }
}

/// Reports whether a byte slice without `"D:"` starts with 8+ ASCII digits like a compact PDF timestamp.
///
/// Accepts a trimmed byte slice. Returns `true` when at least 8 leading bytes are ASCII digits.
fn looks_like_compact_pdf_date(bytes: &[u8]) -> bool {
    let leading_digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    leading_digits >= 8
}

/// Parses a PDF `D:YYYY[MM[DD[HH[MM[SS[O]]]]]]` byte slice into canonical RFC 3339 or ISO date.
///
/// Accepts a trimmed byte slice starting with `b"D:"` or 8+ compact ASCII digits.
/// Returns the normalized RFC 3339 timestamp (or validated `YYYY`/`YYYY-MM`/`YYYY-MM-DD`
/// when time components are absent), or `None` for invalid syntax or calendar bounds.
fn parse_pdf_date(bytes: &[u8]) -> Option<String> {
    let (has_prefix, rest) = match bytes.strip_prefix(b"D:") {
        Some(stripped) => (true, stripped),
        None => (false, bytes),
    };
    let digit_len = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if !matches!(digit_len, 4 | 6 | 8 | 10 | 12 | 14) {
        return None;
    }
    if !has_prefix && digit_len < 8 {
        return None;
    }

    let year = parse_ascii_digits(&rest[0..4])?;
    let month = if digit_len >= 6 {
        let m = parse_ascii_digits(&rest[4..6])?;
        days_in_month(year, m)?;
        Some(m)
    } else {
        None
    };
    let day = if digit_len >= 8 {
        let m = month?;
        let d = parse_ascii_digits(&rest[6..8])?;
        let max_day = days_in_month(year, m)?;
        if !(1..=max_day).contains(&d) {
            return None;
        }
        Some(d)
    } else {
        None
    };
    let hour = if digit_len >= 10 {
        let h = parse_ascii_digits(&rest[8..10])?;
        if h > 23 {
            return None;
        }
        Some(h)
    } else {
        None
    };
    let minute = if digit_len >= 12 {
        let m = parse_ascii_digits(&rest[10..12])?;
        if m > 59 {
            return None;
        }
        Some(m)
    } else {
        None
    };
    let second = if digit_len == 14 {
        let s = parse_ascii_digits(&rest[12..14])?;
        if s > 59 {
            return None;
        }
        Some(s)
    } else {
        None
    };

    let tz_part = &rest[digit_len..];
    if tz_part.is_empty() {
        return match (month, day, hour) {
            (None, _, _) => Some(format!("{year:04}")),
            (Some(m), None, _) => Some(format!("{year:04}-{m:02}")),
            (Some(m), Some(d), None) => Some(format!("{year:04}-{m:02}-{d:02}")),
            (Some(m), Some(d), Some(h)) => Some(format!(
                "{year:04}-{m:02}-{d:02}T{h:02}:{:02}:{:02}Z",
                minute.unwrap_or(0),
                second.unwrap_or(0),
            )),
        };
    }

    let (Some(m), Some(d)) = (month, day) else {
        return None;
    };
    let tz = parse_pdf_timezone(tz_part)?;
    Some(format!(
        "{year:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{tz}",
        hour.unwrap_or(0),
        minute.unwrap_or(0),
        second.unwrap_or(0),
    ))
}

/// Parses a PDF timezone suffix (`Z`, `Z00'00'`, `+HH'MM'`, `-HH'MM`, `+HH:MM`, `+HHMM`, `+HH`).
///
/// Accepts the non-empty timezone byte slice after PDF date/time digits. Returns
/// `Some("Z")` or `Some("±HH:MM")`, or `None` for invalid syntax or out-of-range offsets.
fn parse_pdf_timezone(tz: &[u8]) -> Option<String> {
    if matches!(
        tz,
        b"Z" | b"z" | b"Z00'00'" | b"Z00'00" | b"Z00:00" | b"Z0000"
    ) {
        return Some("Z".to_owned());
    }
    let (sign, rest) = match tz.split_first()? {
        (b'+', rest) => ('+', rest),
        (b'-', rest) => ('-', rest),
        _ => return None,
    };
    let rest = rest.strip_suffix(b"'").unwrap_or(rest);
    let (tz_h, tz_m) = match rest.len() {
        2 => (parse_ascii_digits(rest)?, 0),
        4 => (
            parse_ascii_digits(&rest[0..2])?,
            parse_ascii_digits(&rest[2..4])?,
        ),
        5 if matches!(rest[2], b'\'' | b':') => (
            parse_ascii_digits(&rest[0..2])?,
            parse_ascii_digits(&rest[3..5])?,
        ),
        _ => return None,
    };
    if tz_h > 23 || tz_m > 59 {
        return None;
    }
    Some(format!("{sign}{tz_h:02}:{tz_m:02}"))
}

/// Parses an ISO-8601 / W3CDTF timestamp byte slice into canonical RFC 3339.
///
/// Accepts bytes shaped `YYYY-MM-DD[T|t| ]HH:MM[:SS[.frac]][Z|z|±HH:MM|±HHMM|±HH]`,
/// accepting fractional `.digits` only when explicit `:SS` seconds are present.
/// Returns canonical RFC 3339 (defaulting missing timezone suffixes to `Z`), or
/// `None` for malformed syntax or out-of-range calendar/time/offset values.
fn parse_iso_timestamp(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 16 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') || bytes[13] != b':' {
        return None;
    }
    let year = parse_ascii_digits(&bytes[0..4])?;
    let month = parse_ascii_digits(&bytes[5..7])?;
    let day = parse_ascii_digits(&bytes[8..10])?;
    let max_day = days_in_month(year, month)?;
    if !(1..=max_day).contains(&day) {
        return None;
    }

    let hour = parse_ascii_digits(&bytes[11..13])?;
    let minute = parse_ascii_digits(&bytes[14..16])?;
    if hour > 23 || minute > 59 {
        return None;
    }

    let mut pos = 16;
    let (second, has_seconds) = if pos + 3 <= bytes.len() && bytes[pos] == b':' {
        let s = parse_ascii_digits(&bytes[pos + 1..pos + 3])?;
        if s > 59 {
            return None;
        }
        pos += 3;
        (s, true)
    } else {
        (0, false)
    };

    let frac = if pos < bytes.len() && bytes[pos] == b'.' {
        if !has_seconds {
            return None;
        }
        pos += 1;
        let start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        let digits = &bytes[start..pos];
        if digits.is_empty() || digits.len() > 9 {
            return None;
        }
        Some(std::str::from_utf8(digits).ok()?)
    } else {
        None
    };

    let tz_str = parse_iso_timezone(&bytes[pos..])?;
    match frac {
        Some(digits) => Some(format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{digits}{tz_str}"
        )),
        None => Some(format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{tz_str}"
        )),
    }
}

/// Parses an ISO-8601 / RFC 3339 timezone suffix (`""`, `Z`, `z`, `±HH:MM`, `±HHMM`, `±HH`).
///
/// Accepts the trailing timezone byte slice of an ISO-8601 timestamp. Returns
/// `Some("Z")` (including when no timezone suffix was embedded) or `Some("±HH:MM")`,
/// or `None` for malformed or out-of-range offsets.
fn parse_iso_timezone(tz: &[u8]) -> Option<String> {
    if tz.is_empty() || matches!(tz, b"Z" | b"z") {
        return Some("Z".to_owned());
    }
    let (sign, rest) = match tz.split_first()? {
        (b'+', rest) => ('+', rest),
        (b'-', rest) => ('-', rest),
        _ => return None,
    };
    let (tz_h, tz_m) = match rest.len() {
        2 => (parse_ascii_digits(rest)?, 0),
        4 => (
            parse_ascii_digits(&rest[0..2])?,
            parse_ascii_digits(&rest[2..4])?,
        ),
        5 if rest[2] == b':' => (
            parse_ascii_digits(&rest[0..2])?,
            parse_ascii_digits(&rest[3..5])?,
        ),
        _ => return None,
    };
    if tz_h > 23 || tz_m > 59 {
        return None;
    }
    Some(format!("{sign}{tz_h:02}:{tz_m:02}"))
}

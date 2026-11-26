//! Module: glassbox-inspect native-unit selection.
//! File: selection.rs.
//! Responsibility: Parse selectors, plan source-order native units, and assemble honest v2 extraction reports.

use crate::bodymatter::{BodymatterMarker, format_marker, render_gfm_table, sanitize_body};
use crate::reports::{
    Completeness, ContentOrigin, ContentRow, ContentUnit, Diagnostic, DiagnosticSeverity,
    DiagnosticStage, ExtractReport, ExtractedContent, ExtractionProvenance, FactSource,
    MetadataReport, NativeRange, NativeSelection, NativeUnit, OmissionStatus, OperationStatus,
    QualifiedCount, SchemaKind, SelectionReport,
};
use crate::{InspectReport, reports::ExtractOptions};

/// One checked selection plan for a reader-owned native-unit count.
#[derive(Debug, Clone)]
pub(crate) struct SelectionPlan {
    requested: NativeSelection,
    available: u64,
    ranges: Vec<NativeRange>,
}

impl SelectionPlan {
    /// Resolves a requested selection against an exact reader-owned unit count.
    ///
    /// Accepts the requested selection and available one-based unit count. Returns
    /// a plan that filters source-order indices without splitting flattened text.
    /// Zero or descending bounds are rejected, and explicit ranges are normalized
    /// into sorted disjoint order so membership stays a bounded binary search.
    /// Explicit ranges with no existent index return an error; first-N beyond the
    /// document is valid and returns only existent units.
    pub(crate) fn new(requested: &NativeSelection, available: u64) -> Result<Self, String> {
        let ranges = match requested {
            NativeSelection::Ranges { ranges } => {
                if ranges
                    .iter()
                    .any(|range| range.start == 0 || range.start > range.end)
                {
                    return Err(
                        "native-unit indices are one-based and must not be zero or descending"
                            .into(),
                    );
                }
                normalize_ranges(ranges)
            }
            _ => Vec::new(),
        };
        if matches!(requested, NativeSelection::Ranges { .. })
            && !ranges.iter().any(|range| range.start <= available)
        {
            return Err(format!(
                "every selected native-unit index is outside the available count {available}"
            ));
        }
        Ok(Self {
            requested: requested.clone(),
            available,
            ranges,
        })
    }

    /// Tests whether a one-based native source index belongs to the request.
    ///
    /// Accepts a positive source index. Returns true for selected existing units
    /// and false for unrequested or nonexistent indices. Explicit ranges are
    /// answered by one binary search over normalized ranges, so line-wise
    /// membership stays linear in the text and logarithmic in the request.
    /// This check cannot fail.
    pub(crate) fn contains(&self, index: u64) -> bool {
        if index == 0 || index > self.available {
            return false;
        }
        match &self.requested {
            NativeSelection::All => true,
            NativeSelection::FirstN { count } => index <= *count,
            NativeSelection::Ranges { .. } => {
                let position = self.ranges.partition_point(|range| range.end < index);
                self.ranges
                    .get(position)
                    .is_some_and(|range| range.start <= index)
            }
        }
    }

    /// Counts selected existent units arithmetically without scanning indices.
    ///
    /// Accepts the resolved plan. Returns how many one-based existent indices
    /// the request selects. First-N saturation and clipped ranges are computed
    /// in bounded time, so coverage never walks the available count.
    pub(crate) fn selected_existing_count(&self) -> u64 {
        match &self.requested {
            NativeSelection::All => self.available,
            NativeSelection::FirstN { count } => (*count).min(self.available),
            NativeSelection::Ranges { .. } => self
                .ranges
                .iter()
                .map(|range| {
                    let end = range.end.min(self.available);
                    if range.start > end {
                        0
                    } else {
                        end - range.start + 1
                    }
                })
                .fold(0_u64, |total, length| total.saturating_add(length)),
        }
    }

    /// Reports whether the request intentionally excludes existent native units.
    ///
    /// Accepts the resolved plan. Returns true when at least one available unit
    /// is outside a first-N or range selection; nonexistent indices are ignored.
    /// The answer is computed arithmetically from the normalized selection.
    pub(crate) fn omits_existent_units(&self) -> bool {
        match &self.requested {
            NativeSelection::All => false,
            NativeSelection::FirstN { count } => *count < self.available,
            NativeSelection::Ranges { .. } => self.selected_existing_count() < self.available,
        }
    }

    /// Lists selected existent indices in ascending source order.
    ///
    /// Accepts the resolved plan. Returns one-based indices, bounded by the
    /// available count. The enumeration is proportional to the selected units;
    /// callers that only need membership or coverage use `contains` and
    /// `selected_existing_count` so large documents are never scanned.
    pub(crate) fn indices(&self) -> Vec<u64> {
        let mut indices = Vec::new();
        let last = match &self.requested {
            NativeSelection::All => self.available,
            NativeSelection::FirstN { count } => (*count).min(self.available),
            NativeSelection::Ranges { .. } => {
                for range in &self.ranges {
                    let end = range.end.min(self.available);
                    if range.start <= end {
                        indices.extend(range.start..=end);
                    }
                }
                return indices;
            }
        };
        indices.extend(1..=last);
        indices
    }
}

/// Sorts and merges explicit ranges into disjoint ascending order.
///
/// Accepts validated one-based ranges. Returns normalized ranges so membership
/// stays a bounded binary search and coverage arithmetic never double-counts an
/// overlapping request. Empty selections normalize to no ranges.
fn normalize_ranges(ranges: &[NativeRange]) -> Vec<NativeRange> {
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|range| (range.start, range.end));
    let mut normalized: Vec<NativeRange> = Vec::with_capacity(sorted.len());
    for range in sorted {
        if let Some(previous) = normalized.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
            continue;
        }
        normalized.push(range);
    }
    normalized
}

/// Parses a `--pages` value into a checked native selection.
///
/// Accepts a count or comma-separated inclusive one-based ranges such as
/// `3-7,10`. Returns full first-N semantics for a bare integer and deduplicated
/// ascending ranges otherwise. Zero, negative, descending, empty, malformed,
/// and overflowing values return a human-readable invocation error.
pub fn parse_pages(value: &str) -> Result<NativeSelection, String> {
    if value.is_empty() || value.trim() != value {
        return Err("page selection must not be empty or surrounded by whitespace".into());
    }
    if value.chars().all(|character| character.is_ascii_digit()) {
        let count = value
            .parse::<u64>()
            .map_err(|_| "page count overflows an unsigned 64-bit index".to_string())?;
        if count == 0 {
            return Err("page count must be a positive one-based index".into());
        }
        return Ok(NativeSelection::FirstN { count });
    }
    let mut ranges = Vec::new();
    for item in value.split(',') {
        if item.is_empty() || item.trim() != item {
            return Err("page ranges must not contain empty or whitespace-padded items".into());
        }
        let (start, end) = match item.split_once('-') {
            Some((start, end)) if !end.contains('-') => (start, end),
            Some(_) => {
                return Err(format!(
                    "malformed descending or negative page range `{item}`"
                ));
            }
            None => (item, item),
        };
        if start.is_empty()
            || end.is_empty()
            || !start.chars().all(|character| character.is_ascii_digit())
            || !end.chars().all(|character| character.is_ascii_digit())
        {
            return Err(format!("malformed page index or range `{item}`"));
        }
        let start = start
            .parse::<u64>()
            .map_err(|_| format!("page index overflows an unsigned 64-bit value in `{item}`"))?;
        let end = end
            .parse::<u64>()
            .map_err(|_| format!("page index overflows an unsigned 64-bit value in `{item}`"))?;
        if start == 0 || end == 0 {
            return Err("page indices are one-based and must be positive".into());
        }
        if start > end {
            return Err(format!("descending page range `{item}` is not supported"));
        }
        ranges.push(NativeRange { start, end });
    }
    if ranges.is_empty() {
        return Err("page selection must contain at least one range".into());
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut normalized: Vec<NativeRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = normalized.last_mut()
            && range.start <= previous.end
        {
            previous.end = previous.end.max(range.end);
            continue;
        }
        normalized.push(range);
    }
    Ok(NativeSelection::Ranges { ranges: normalized })
}

/// Initializes reader-owned selection facts and returns its checked plan.
///
/// Accepts a transitional extraction report, requested selection, native unit
/// kind, and exact available count. Returns a source-index plan. An unsatisfiable
/// explicit range records a selection diagnostic and returns an empty plan error.
pub(crate) fn begin(
    report: &mut InspectReport,
    requested: &NativeSelection,
    unit: NativeUnit,
    available: u64,
) -> Result<SelectionPlan, String> {
    report.native_unit = unit.clone();
    report.native_available = QualifiedCount {
        unit,
        value: Some(available),
        source: FactSource::NativeStructure,
        completeness: Completeness::Exact,
    };
    SelectionPlan::new(requested, available)
        .inspect_err(|message| report.error("selection", message))
}

/// Creates one native text unit with source-order identity and provenance.
///
/// Accepts a one-based index, actual unit kind, optional source part, text, and
/// content origin. Returns a complete unit with native-structure provenance;
/// readers may add OCR provenance or downgrade completeness after failures.
pub(crate) fn text_unit(
    unit_index: u64,
    unit: NativeUnit,
    source_part: Option<String>,
    text: String,
    origin: ContentOrigin,
) -> ContentUnit {
    ContentUnit {
        unit_index,
        unit,
        source_part,
        label: None,
        marker_index: None,
        start_ms: None,
        end_ms: None,
        text: Some(text),
        rows: Vec::new(),
        provenance: vec![ExtractionProvenance {
            reader: "native-reader".into(),
            stage: "native-unit-extract".into(),
            source: FactSource::NativeStructure,
            origin,
            ocr: None,
        }],
        completeness: Completeness::Exact,
    }
}

/// Creates one bounded dataset worksheet or preview unit with row payloads.
///
/// Accepts a one-based unit index, worksheet/preview kind, optional source part,
/// text and preview rows. Returns a native unit marked partial because dataset
/// extraction retains only the established bounded preview, never a full export.
pub(crate) fn dataset_unit(
    unit_index: u64,
    unit: NativeUnit,
    source_part: Option<String>,
    text: String,
    rows: Vec<ContentRow>,
) -> ContentUnit {
    let mut unit = text_unit(
        unit_index,
        unit,
        source_part,
        text,
        ContentOrigin::DerivedPreview,
    );
    unit.provenance[0].source = FactSource::DerivedPreview;
    unit.provenance[0].stage = "bounded-dataset-preview".into();
    unit.rows = rows;
    unit.completeness = Completeness::Partial;
    unit
}

/// Assembles one worksheet native unit from a bounded sheet preview.
///
/// Accepts the zero-based worksheet index, package source part identifier,
/// mutable sheet summary whose owned `first_row` is consumed, and one-based
/// source row numbers aligned to the retained preview rows. Returns a
/// `ContentUnit` carrying tab-delimited text, structured `ContentRow` values,
/// and `Completeness::Partial` when either the header or preview rows were
/// truncated (otherwise `Completeness::Exact`). Cannot fail; shorter row
/// number lists truncate zipped rows without panicking.
pub(crate) fn worksheet_unit(
    index: usize,
    source_part: String,
    sheet: &mut crate::Sheet,
    row_numbers: Vec<u64>,
) -> ContentUnit {
    let first_row = sheet.first_row.take();
    let text = first_row
        .iter()
        .chain(sheet.preview_rows.iter())
        .map(|row| row.join("\t"))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = first_row
        .into_iter()
        .chain(sheet.preview_rows.iter().cloned())
        .zip(row_numbers)
        .map(|(values, row_index)| ContentRow { row_index, values })
        .collect::<Vec<_>>();
    let mut unit = dataset_unit(
        index as u64 + 1,
        NativeUnit::Worksheet,
        Some(source_part),
        text,
        rows,
    );
    unit.completeness = if sheet.preview_truncated || sheet.first_row_truncated {
        Completeness::Partial
    } else {
        Completeness::Exact
    };
    unit
}

/// Records that native boundaries are unknown for an explicit selection.
///
/// Accepts a report. Returns unit after recording unavailable native coverage.
/// The existing format diagnostic explains the unsupported operation; no
/// duplicate diagnostic, content, or split boundary is fabricated.
pub(crate) fn unsupported(report: &mut InspectReport) {
    report.native_unit = NativeUnit::Unknown;
    report.native_available = QualifiedCount {
        unit: NativeUnit::Unknown,
        value: None,
        source: FactSource::Unknown,
        completeness: Completeness::Unavailable,
    };
}

fn diagnostic_stage(stage: &str) -> DiagnosticStage {
    match stage {
        "selection" => DiagnosticStage::Selection,
        "pdf_ocr" | "probe_ocr" | "ocr" => DiagnosticStage::Ocr,
        "media" => DiagnosticStage::Media,
        "output" => DiagnosticStage::Artifact,
        "input" => DiagnosticStage::Input,
        "detection" => DiagnosticStage::Detection,
        _ => DiagnosticStage::Extraction,
    }
}

/// Truncates one string to a remaining Unicode-character budget.
///
/// Accepts mutable text and maximum retained characters. Returns true when
/// content was removed. Invalid UTF-8 cannot occur because Rust strings are valid.
fn truncate_chars(value: &mut String, cap: usize) -> bool {
    let count = value.chars().count();
    if count <= cap {
        return false;
    }
    *value = value.chars().take(cap).collect();
    true
}

/// Truncates one string to a whitespace-word budget.
///
/// Accepts mutable text and maximum retained words. Returns true when another
/// word was omitted. Empty and whitespace-only strings need no truncation.
fn truncate_tokens(value: &mut String, cap: usize) -> bool {
    let mut words = value.split_whitespace();
    let mut end = 0usize;
    for word in words.by_ref().take(cap) {
        end = word.as_ptr() as usize - value.as_ptr() as usize + word.len();
    }
    if words.next().is_none() {
        return false;
    }
    value.truncate(end);
    true
}

/// Applies shared character/token controls to v2 headers and units and reports omissions.
///
/// Accepts mutable tabular headers, mutable units, character cap, and optional
/// token cap. Returns true when any retained representation was clipped. Tabular
/// headers (when present) and structured rows consume one shared rows budget
/// value-by-value so header text obeys the same limits as cell values, while
/// every unit's text consumes an independent text budget across units. A
/// row-bearing unit keeps its text whole exactly when it fits the remaining text
/// budget; otherwise the redundant rendering is withheld beside complete rows
/// instead of being shown clipped. Text-only units keep a prefix of the
/// remaining text budget. Every unit reports its own truth: only a unit that
/// was actually clipped or withheld is marked partial.
fn apply_content_limits(
    headers: &mut [String],
    units: &mut [ContentUnit],
    max_chars: usize,
    max_tokens: Option<usize>,
) -> bool {
    let mut truncated = false;
    // Independent aggregate budgets: one per parallel projection.
    let mut rows_chars = max_chars;
    let mut rows_tokens = max_tokens;
    let mut text_chars = max_chars;
    let mut text_tokens = max_tokens;
    for header in headers.iter_mut() {
        let mut header_truncated = truncate_chars(header, rows_chars);
        rows_chars = rows_chars.saturating_sub(header.chars().count());
        if let Some(remaining) = &mut rows_tokens {
            header_truncated |= truncate_tokens(header, *remaining);
            *remaining = remaining.saturating_sub(header.split_whitespace().count());
        }
        truncated |= header_truncated;
    }
    for unit in units {
        let mut unit_truncated = false;
        if unit.rows.is_empty() {
            if let Some(text) = &mut unit.text {
                unit_truncated |= truncate_chars(text, text_chars);
                text_chars = text_chars.saturating_sub(text.chars().count());
                if let Some(remaining) = &mut text_tokens {
                    unit_truncated |= truncate_tokens(text, *remaining);
                    *remaining = remaining.saturating_sub(text.split_whitespace().count());
                }
            }
        } else {
            // Rows are the structured representation: prefix-clip each value
            // against the rows budget so retained row data is never wiped by
            // the parallel text rendering.
            for row in &mut unit.rows {
                for value in &mut row.values {
                    unit_truncated |= truncate_chars(value, rows_chars);
                    rows_chars = rows_chars.saturating_sub(value.chars().count());
                    if let Some(remaining) = &mut rows_tokens {
                        unit_truncated |= truncate_tokens(value, *remaining);
                        *remaining = remaining.saturating_sub(value.split_whitespace().count());
                    }
                }
            }
            // A row-bearing unit keeps its text whole exactly when it fits the
            // remaining text budget; otherwise the redundant rendering is
            // withheld beside the structured rows instead of shown clipped.
            // The fit decision is made through a shared borrow so clearing
            // the Option field below never conflicts with a live text borrow.
            let keep_text = unit
                .text
                .as_ref()
                .is_some_and(|text| text_fits(text, text_chars, text_tokens));
            if keep_text {
                if let Some(text) = &mut unit.text {
                    text_chars = text_chars.saturating_sub(text.chars().count());
                    if let Some(remaining) = &mut text_tokens {
                        *remaining = remaining.saturating_sub(text.split_whitespace().count());
                    }
                }
            } else if unit.text.is_some() {
                unit.text = None;
                unit_truncated = true;
            }
        }
        truncated |= unit_truncated;
        if unit_truncated {
            unit.completeness = Completeness::Partial;
        }
    }
    truncated
}

/// Reports whether one whole text fits both remaining text budgets.
///
/// Accepts the candidate text and remaining character and token budgets.
/// Returns true when the text can be retained whole. This check cannot fail.
fn text_fits(text: &str, chars: usize, tokens: Option<usize>) -> bool {
    text.chars().count() <= chars
        && tokens.is_none_or(|remaining| text.split_whitespace().count() <= remaining)
}

/// Converts the established JSON/XML/profile preview into one selectable dataset unit.
///
/// Accepts a transitional report and requested selection. Returns unit after one
/// native preview unit is attached. An out-of-range explicit selector is recorded
/// as a selection diagnostic; existing reader-owned units are never replaced.
pub(crate) fn dataset_fallback(report: &mut InspectReport, requested: &NativeSelection) {
    if !report.native_units.is_empty() {
        return;
    }
    let plan = match begin(report, requested, NativeUnit::DatasetRow, 1) {
        Ok(plan) => plan,
        Err(error) => {
            report.error("selection", error);
            return;
        }
    };
    if !plan.contains(1) {
        return;
    }
    let rows = report
        .dataset
        .as_ref()
        .map(|preview| {
            preview
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| ContentRow {
                    row_index: index as u64 + 1,
                    values: row
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| value.to_string())
                        })
                        .collect(),
                })
                .collect()
        })
        .unwrap_or_default();
    report.native_units.push(dataset_unit(
        1,
        NativeUnit::DatasetRow,
        Some("dataset/profile preview".into()),
        report.text.content.clone(),
        rows,
    ));
}

/// Converts reader-owned native units and diagnostics into a canonical v2 report.
///
/// Accepts v2 metadata, the transitional reader result, and extraction options.
/// Returns selected source-order content, exact returned indices, requested and
/// omitted coverage, provenance, extraction-stage diagnostics, and extraction
/// operation status while preserving `metadata` as the independent standalone
/// inspection result. Selection-only omissions retain partial source coverage
/// while a fully emitted request completes; parser, OCR, safety, and
/// missing-selected-unit losses produce partial reports. Reader or input
/// failures classify as failures with the same precedence as the metadata
/// stage; the unsupported boundary is reserved for identification-only formats
/// and genuinely unsupported operations, never for a broken read, so a failed
/// identification-only document reports failure rather than unsupported.
pub(crate) fn assemble(
    metadata: MetadataReport,
    mut legacy: InspectReport,
    options: &ExtractOptions,
) -> ExtractReport {
    let mut diagnostics = Vec::new();
    for error in &legacy.errors {
        diagnostics.push(Diagnostic {
            stage: diagnostic_stage(error.stage),
            severity: DiagnosticSeverity::Error,
            code: error.stage.into(),
            message: error.message.clone(),
            unit_index: None,
        });
    }
    if legacy.native_units.is_empty()
        && legacy.native_unit == NativeUnit::Unknown
        && !legacy.text.content.is_empty()
    {
        let plan = SelectionPlan::new(&options.selection, 1);
        match plan {
            Ok(plan) if plan.contains(1) => {
                legacy.native_unit = NativeUnit::DerivedPreviewUnit;
                legacy.native_available = QualifiedCount {
                    unit: NativeUnit::DerivedPreviewUnit,
                    value: Some(1),
                    source: FactSource::DerivedPreview,
                    completeness: Completeness::Exact,
                };
                legacy.native_units.push(dataset_unit(
                    1,
                    NativeUnit::DerivedPreviewUnit,
                    None,
                    std::mem::take(&mut legacy.text.content),
                    Vec::new(),
                ));
            }
            Ok(_) => {}
            Err(message) => {
                legacy.error("selection", message.clone());
                diagnostics.push(Diagnostic {
                    stage: DiagnosticStage::Selection,
                    severity: DiagnosticSeverity::Error,
                    code: "selection".into(),
                    message,
                    unit_index: None,
                });
            }
        }
    }
    legacy.native_units.sort_by_key(|unit| unit.unit_index);
    let safety_truncated = apply_content_limits(
        &mut legacy.native_headers,
        &mut legacy.native_units,
        options.max_chars,
        options.max_tokens,
    );
    if safety_truncated {
        diagnostics.push(Diagnostic {
            stage: DiagnosticStage::Extraction,
            severity: DiagnosticSeverity::Warning,
            code: "content_limit".into(),
            message: "requested character or token control truncated extracted content".into(),
            unit_index: None,
        });
    }
    let available = legacy.native_available.clone();
    let plan = SelectionPlan::new(&options.selection, available.value.unwrap_or(0)).ok();
    let returned_indices: Vec<u64> = legacy
        .native_units
        .iter()
        .map(|unit| unit.unit_index)
        .collect();
    let selection_omission = plan
        .as_ref()
        .is_some_and(|plan| plan.omits_existent_units());
    // Bounded selected-membership check: counting returned indices that the
    // plan selects never walks the available count of a large document.
    let missing_selected = plan.as_ref().is_some_and(|plan| {
        let returned_selected = returned_indices
            .iter()
            .filter(|index| plan.contains(**index))
            .count() as u64;
        returned_selected < plan.selected_existing_count()
    });
    let has_errors = !legacy.errors.is_empty();
    let unsupported = metadata.metadata.extraction_support
        == crate::reports::SupportStatus::IdentificationOnly
        || (legacy.native_unit == NativeUnit::Unknown
            && legacy.native_units.is_empty()
            && legacy.errors.is_empty());
    let selection_failed = legacy.errors.iter().any(|error| error.stage == "selection");
    // COND-06: a preexisting metadata-stage failure keeps its failure
    // classification ahead of the unsupported boundary. An identification-only
    // format whose header parse failed at the metadata stage (for example an
    // encrypted MOBI with a broken EXTH block) must never be overwritten with
    // `Unsupported`, which is reserved for formats that were identified
    // cleanly and simply have no extraction path. Legacy reader errors below
    // the metadata stage keep their established partial classification.
    let preexisting_failure =
        selection_failed || metadata.metadata.status == OperationStatus::Failed;
    let omitted = if preexisting_failure || missing_selected || has_errors {
        OmissionStatus::PartialFailure
    } else if unsupported {
        OmissionStatus::Unsupported
    } else if safety_truncated
        || legacy.text.truncated
        || legacy.native_units.iter().any(|unit| {
            unit.completeness == Completeness::Partial
                && unit
                    .provenance
                    .iter()
                    .any(|provenance| provenance.origin == ContentOrigin::DerivedPreview)
        })
    {
        OmissionStatus::SafetyLimit
    } else if selection_omission {
        OmissionStatus::Selection
    } else {
        OmissionStatus::None
    };
    let completeness = if omitted == OmissionStatus::None {
        Completeness::Exact
    } else if selection_failed && legacy.native_units.is_empty() {
        Completeness::Unavailable
    } else {
        Completeness::Partial
    };
    let content_incomplete = legacy
        .native_units
        .iter()
        .any(|unit| unit.completeness != Completeness::Exact);
    let mut status = if preexisting_failure {
        OperationStatus::Failed
    } else if unsupported {
        OperationStatus::Unsupported
    } else if matches!(omitted, OmissionStatus::None | OmissionStatus::Selection)
        && !content_incomplete
        && metadata.metadata.status == OperationStatus::Complete
    {
        // Source coverage can be partial while the requested selection completes.
        OperationStatus::Complete
    } else {
        OperationStatus::Partial
    };
    if metadata.metadata.status == OperationStatus::Unsupported && !preexisting_failure {
        // A metadata stage that reports `Unsupported` keeps the unsupported
        // classification, but the guard keeps this conditional boundary from
        // overwriting a failure (COND-06): a legacy selection failure or a
        // metadata-stage failure always wins first. Warning-only legacy
        // errors keep their established partial classification because the
        // metadata stage still completed or reported partial, never
        // `Unsupported`, on their paths.
        status = OperationStatus::Unsupported;
    }
    let provenance = legacy
        .native_units
        .iter()
        .flat_map(|unit| unit.provenance.clone())
        .collect();
    ExtractReport {
        schema_version: crate::reports::LEGACY_REPORT_SCHEMA_VERSION,
        kind: crate::reports::ReportKind::Extract,
        status,
        diagnostics,
        metadata: metadata.metadata,
        content: ExtractedContent {
            headers: legacy.native_headers,
            units: legacy.native_units,
        },
        provenance,
        completeness,
        selection: SelectionReport {
            requested: options.selection.clone(),
            unit: legacy.native_unit,
            available,
            returned_indices,
            omitted,
            completeness,
        },
        media: None,
    }
}

/// Renders one canonical v2 report as content-first plain text with coverage.
///
/// Accepts an extraction report. Returns unit labels, available/returned source
/// indices, omissions, diagnostics, and each selected text/row payload. This
/// function performs no I/O and has no failure states.
pub fn render_text(report: &ExtractReport) -> String {
    let mut out = coverage_text(report);
    for unit in &report.content.units {
        out.push_str(&format!("\n[unit {} {:?}]\n", unit.unit_index, unit.unit));
        if let Some(text) = &unit.text {
            out.push_str(text);
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
        for row in &unit.rows {
            out.push_str(&row.values.join("\t"));
            out.push('\n');
        }
    }
    out
}

/// Renders one extraction report as Markdown with `bodymatter:v1` structure markers.
///
/// Accepts an [`ExtractReport`]. Returns blank-line-delimited Markdown with
/// forward-scoped `<!-- bodymatter:v1 <.../> -->` markers enabled, GFM pipe
/// tables for row-bearing units (suppressing duplicate TSV `unit.text`),
/// neutralized HTML comments, minimal line-leading block escaping, and zero
/// synthetic preamble or unit headings. No source file is read and no output
/// artifact is written; this function cannot fail.
pub fn render_markdown(report: &ExtractReport) -> String {
    render_markdown_with_options(report, true)
}

/// Renders one extraction report as Markdown with configurable `bodymatter:v1` markers.
///
/// Accepts an [`ExtractReport`] and `include_markers` (`true` to include
/// `bodymatter:v1` structure comments, `false` to omit all marker lines while
/// keeping identical content blocks and GFM pipe tables). Delegates to
/// [`render_markdown_full`] with `include_frontmatter = true` (which applies
/// `glassbox:v1` frontmatter only to ASR transcript reports while keeping
/// non-transcript documents frontmatter-free). Returns blank-line-delimited
/// Markdown ending with a single trailing newline when non-empty, or an empty
/// string when no markers or content blocks are produced. Cannot fail.
pub fn render_markdown_with_options(report: &ExtractReport, include_markers: bool) -> String {
    render_markdown_full(report, include_markers, true)
}

/// Renders one extraction report as Markdown with independent marker and frontmatter controls.
///
/// Accepts an [`ExtractReport`], `include_markers` (`true` to emit
/// `bodymatter:v1` structure comments, `false` to omit them), and
/// `include_frontmatter` (`true` to emit `schema: "glassbox:v1"` YAML
/// frontmatter on ASR transcript reports where `report.media.asr.is_some()`,
/// `false` to omit frontmatter). Non-transcript document extractions always
/// remain frontmatter-free regardless of `include_frontmatter` so existing
/// document Markdown output stays byte-identical. Returns blank-line-delimited
/// Markdown ending with a single trailing newline when non-empty, or an empty
/// string when no output blocks are produced. Cannot fail.
pub fn render_markdown_full(
    report: &ExtractReport,
    include_markers: bool,
    include_frontmatter: bool,
) -> String {
    if report
        .media
        .as_ref()
        .and_then(|media| media.asr.as_ref())
        .is_some()
    {
        return crate::transcript_markdown::render_transcript_markdown(
            report,
            &crate::transcript_markdown::TranscriptRenderOptions {
                include_markers,
                include_frontmatter,
                ..crate::transcript_markdown::TranscriptRenderOptions::default()
            },
        );
    }

    let worksheet_names: Vec<&str> = report
        .metadata
        .schema_identifiers
        .iter()
        .filter(|identifier| identifier.kind == SchemaKind::Worksheet)
        .map(|identifier| identifier.name.as_str())
        .collect();

    let mut blocks: Vec<String> = Vec::new();
    let mut table_counter: u64 = 0;
    let mut page_break_counter: u64 = 0;
    let mut dataset_sheet_counter: u64 = 0;

    let units = &report.content.units;
    let mut index = 0usize;
    while index < units.len() {
        if units[index].unit == NativeUnit::DatasetRow && !units[index].rows.is_empty() {
            dataset_sheet_counter += 1;
            if include_markers {
                blocks.push(format_marker(&BodymatterMarker::Sheet {
                    number: dataset_sheet_counter,
                    name: None,
                }));
            }
            let mut combined_rows: Vec<ContentRow> = Vec::new();
            if !report.content.headers.is_empty() {
                combined_rows.push(ContentRow {
                    row_index: 0,
                    values: report.content.headers.clone(),
                });
            }
            while index < units.len()
                && units[index].unit == NativeUnit::DatasetRow
                && !units[index].rows.is_empty()
            {
                combined_rows.extend(units[index].rows.iter().cloned());
                index += 1;
            }
            let table = render_gfm_table(&combined_rows);
            if !table.is_empty() {
                blocks.push(table);
            }
            continue;
        }

        let unit = &units[index];
        index += 1;

        let marker = match unit.unit {
            NativeUnit::PhysicalPage => Some(BodymatterMarker::Page {
                number: unit.unit_index,
                label: unit
                    .label
                    .as_deref()
                    .filter(|value| !value.trim().is_empty()),
                origin: page_origin(&unit.provenance),
                partial: unit.completeness == Completeness::Partial,
            }),
            NativeUnit::Worksheet => Some(BodymatterMarker::Sheet {
                number: unit.unit_index,
                name: worksheet_name(&worksheet_names, unit),
            }),
            NativeUnit::DatasetPreview => Some(BodymatterMarker::Sheet {
                number: unit.unit_index,
                name: None,
            }),
            NativeUnit::Slide => Some(BodymatterMarker::Slide {
                number: unit.unit_index,
            }),
            NativeUnit::Table => {
                let number = unit.marker_index.unwrap_or_else(|| {
                    table_counter += 1;
                    table_counter
                });
                Some(BodymatterMarker::Table { number })
            }
            NativeUnit::SpineItem => Some(BodymatterMarker::Chapter {
                number: unit.unit_index,
                href: unit
                    .source_part
                    .as_deref()
                    .filter(|value| !value.is_empty()),
            }),
            NativeUnit::PageBreak => {
                let number = unit.marker_index.unwrap_or_else(|| {
                    page_break_counter += 1;
                    page_break_counter
                });
                Some(BodymatterMarker::PageBreak { number })
            }
            NativeUnit::TimeInterval => unit
                .start_ms
                .zip(unit.end_ms)
                .and_then(|(start_ms, end_ms)| BodymatterMarker::time(start_ms, end_ms)),
            _ => None,
        };

        if include_markers && let Some(marker) = marker {
            blocks.push(format_marker(&marker));
        }

        if !unit.rows.is_empty() {
            let table = render_gfm_table(&unit.rows);
            if !table.is_empty() {
                blocks.push(table);
            }
        } else if let Some(text) = &unit.text {
            let sanitized = sanitize_body(text);
            let trimmed = sanitized.trim_end_matches(['\r', '\n']);
            if !trimmed.trim().is_empty() {
                blocks.push(trimmed.to_owned());
            }
        }
    }

    if blocks.is_empty() {
        String::new()
    } else {
        let mut out = blocks.join("\n\n");
        out.push('\n');
        out
    }
}

/// Derives the `bodymatter:v1` page `origin` attribute from unit provenance records.
///
/// Accepts a slice of [`ExtractionProvenance`] entries attached to a PDF page unit.
/// Returns `"mixed"` when any provenance is [`ContentOrigin::Mixed`] or when both
/// native and OCR origins are present, `"ocr"` when only OCR provenance is present,
/// and `"native"` otherwise. Performs no I/O and cannot fail.
fn page_origin(provenance: &[ExtractionProvenance]) -> &'static str {
    let mut has_native = false;
    let mut has_ocr = false;
    for item in provenance {
        match item.origin {
            ContentOrigin::Mixed => return "mixed",
            ContentOrigin::Ocr => has_ocr = true,
            ContentOrigin::Native => has_native = true,
            ContentOrigin::DerivedPreview => {}
        }
    }
    match (has_native, has_ocr) {
        (true, true) => "mixed",
        (false, true) => "ocr",
        _ => "native",
    }
}

/// Resolves the optional worksheet display name for a [`NativeUnit::Worksheet`] unit.
///
/// Accepts the ordered worksheet names collected from report schema identifiers
/// and the target [`ContentUnit`]. Returns the matching one-based worksheet name
/// from schema identifiers when present, or falls back to `unit.source_part`
/// (stripping any `content.xml#sheet/` prefix) when no worksheet schema
/// identifiers exist. Empty names return `None`. Performs no I/O and cannot fail.
fn worksheet_name<'a>(worksheet_names: &[&'a str], unit: &'a ContentUnit) -> Option<&'a str> {
    if !worksheet_names.is_empty() {
        let index = usize::try_from(unit.unit_index.checked_sub(1)?).ok()?;
        return worksheet_names
            .get(index)
            .copied()
            .filter(|name| !name.is_empty());
    }
    let part = unit.source_part.as_deref()?;
    let stripped = part.strip_prefix("content.xml#sheet/").unwrap_or(part);
    (!stripped.is_empty()).then_some(stripped)
}

/// Formats requested/available/returned coverage for text and Markdown outputs.
///
/// Accepts an extraction report. Returns one stable coverage line with qualified
/// availability and source-order returned indices. It has no failure states.
fn coverage_text(report: &ExtractReport) -> String {
    let available = report
        .selection
        .available
        .value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".into());
    let returned = report
        .selection
        .returned_indices
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "Selection: unit={:?}; available={} ({}); returned=[{}]; omitted={:?}; completeness={:?}\n",
        report.selection.unit,
        available,
        format!("{:?}", report.selection.available.completeness).to_lowercase(),
        returned,
        report.selection.omitted,
        report.selection.completeness
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks count/range parsing, deduplication, ordering, and malformed rejection.
    #[test]
    fn parser_enforces_checked_native_indices() {
        assert_eq!(
            parse_pages("5").unwrap(),
            NativeSelection::FirstN { count: 5 }
        );
        let NativeSelection::Ranges { ranges } = parse_pages("3-4,2,2,10").unwrap() else {
            panic!("ranges expected")
        };
        assert_eq!(ranges.len(), 3);
        let plan = SelectionPlan::new(&NativeSelection::Ranges { ranges }, 10).unwrap();
        assert_eq!(plan.indices(), vec![2, 3, 4, 10]);
        for invalid in ["", "0", "-1", "7-3", "1,,2", "18446744073709551616"] {
            assert!(parse_pages(invalid).is_err(), "accepted {invalid}");
        }
    }

    /// Checks first-N saturation and explicit all-out-of-range failure.
    #[test]
    fn plans_distinguish_nonexistent_counts_from_unsatisfiable_ranges() {
        let first = SelectionPlan::new(&NativeSelection::FirstN { count: 20 }, 6).unwrap();
        assert_eq!(first.indices(), vec![1, 2, 3, 4, 5, 6]);
        assert!(!first.omits_existent_units());
        let ranges = NativeSelection::Ranges {
            ranges: vec![NativeRange { start: 8, end: 9 }],
        };
        assert!(SelectionPlan::new(&ranges, 6).is_err());
    }

    /// Checks programmatic ranges are normalized and stay membership-bounded.
    #[test]
    fn programmatic_ranges_are_normalized_for_bounded_membership() {
        let unsorted = NativeSelection::Ranges {
            ranges: vec![
                NativeRange { start: 5, end: 7 },
                NativeRange { start: 2, end: 4 },
                NativeRange { start: 1, end: 3 },
            ],
        };
        let plan = SelectionPlan::new(&unsorted, 10).unwrap();
        assert_eq!(plan.indices(), [1, 2, 3, 4, 5, 6, 7]);
        assert!(plan.contains(4) && plan.contains(7));
        assert!(!plan.contains(8));
        assert_eq!(plan.selected_existing_count(), 7);
        assert!(plan.omits_existent_units());
        let full = NativeSelection::Ranges {
            ranges: vec![
                NativeRange { start: 1, end: 3 },
                NativeRange { start: 4, end: 10 },
            ],
        };
        let plan = SelectionPlan::new(&full, 10).unwrap();
        assert_eq!(plan.selected_existing_count(), 10);
        assert!(!plan.omits_existent_units());
    }

    /// Checks zero and descending bounds are rejected at plan construction.
    #[test]
    fn zero_and_descending_ranges_are_rejected_at_construction() {
        for ranges in [
            vec![NativeRange { start: 0, end: 3 }],
            vec![NativeRange { start: 4, end: 2 }],
        ] {
            let selection = NativeSelection::Ranges { ranges };
            assert!(SelectionPlan::new(&selection, 10).is_err());
        }
    }

    /// Checks arithmetic coverage equals enumerated membership, clipped or not.
    #[test]
    fn arithmetic_coverage_matches_enumerated_membership() {
        let selection = parse_pages("2-4,7,9-12,20-20").unwrap();
        let plan = SelectionPlan::new(&selection, 25).unwrap();
        let enumerated = plan.indices();
        assert_eq!(plan.selected_existing_count(), enumerated.len() as u64);
        for index in 1..=25 {
            assert_eq!(
                plan.contains(index),
                enumerated.contains(&index),
                "index {index}"
            );
        }
        assert_eq!(
            plan.omits_existent_units(),
            plan.selected_existing_count() < 25
        );
        let clipped = SelectionPlan::new(&selection, 10).unwrap();
        assert_eq!(
            clipped.selected_existing_count() as usize,
            clipped.indices().len()
        );
        assert!(clipped.omits_existent_units());
    }

    /// Checks parallel text and rows projections draw independent budgets.
    #[test]
    fn parallel_projections_draw_independent_aggregate_budgets() {
        let row = |index: u64, value: &str| ContentRow {
            row_index: index,
            values: vec![value.into()],
        };
        let first = dataset_unit(
            1,
            NativeUnit::DatasetRow,
            None,
            "aa".into(),
            vec![row(1, "x")],
        );
        let second = dataset_unit(
            2,
            NativeUnit::DatasetRow,
            None,
            "bb".into(),
            vec![row(2, "y")],
        );
        let third = dataset_unit(
            3,
            NativeUnit::DatasetRow,
            None,
            "ccc".into(),
            vec![row(3, "z")],
        );
        let mut units = vec![first, second, third];
        // The text budget (4 chars) covers the first two renderings and
        // withholds the third; the rows budget is independent and never
        // charged by text, so every structured value retains its data.
        let truncated = apply_content_limits(&mut [], &mut units, 4, None);
        assert!(truncated);
        assert_eq!(units[0].text.as_deref(), Some("aa"));
        assert_eq!(units[1].text.as_deref(), Some("bb"));
        assert_eq!(units[2].text, None);
        assert_eq!(units[0].rows[0].values, ["x"]);
        assert_eq!(units[1].rows[0].values, ["y"]);
        assert_eq!(units[2].rows[0].values, ["z"]);
        assert_eq!(units[0].completeness, Completeness::Partial);
        assert_eq!(units[2].completeness, Completeness::Partial);
    }

    /// Checks a clipped row-unit rendering is withheld while rows keep data.
    #[test]
    fn clipped_row_unit_text_is_withheld_and_rows_retain_a_prefix() {
        let first = dataset_unit(
            1,
            NativeUnit::DatasetRow,
            None,
            "count\t7".into(),
            vec![ContentRow {
                row_index: 1,
                values: vec!["count".into(), "7".into()],
            }],
        );
        let second = dataset_unit(
            2,
            NativeUnit::DatasetRow,
            None,
            "one\ttwo three four".into(),
            vec![ContentRow {
                row_index: 2,
                values: vec!["one".into(), "two".into(), "three".into()],
            }],
        );
        let mut units = vec![first, second];
        let truncated = apply_content_limits(&mut [], &mut units, 1 << 30, Some(3));
        assert!(truncated);
        // The first rendering fits the remaining word budget and is kept
        // whole; the second would clip, so its text is withheld while its
        // rows keep the structured prefix the rows budget allows.
        assert_eq!(units[0].text.as_deref(), Some("count\t7"));
        assert_eq!(units[0].rows[0].values, ["count", "7"]);
        assert_eq!(units[1].text, None);
        assert_eq!(units[1].rows[0].values, ["one", "", ""]);
        assert_eq!(units[1].completeness, Completeness::Partial);
    }

    /// Checks only units that were actually clipped are marked partial.
    #[test]
    fn untouched_units_are_not_marked_partial_by_earlier_truncation() {
        let first = text_unit(
            1,
            NativeUnit::LogicalLine,
            None,
            "abc".into(),
            ContentOrigin::Native,
        );
        let second = text_unit(
            2,
            NativeUnit::LogicalLine,
            None,
            String::new(),
            ContentOrigin::Native,
        );
        let mut units = vec![first, second];
        assert!(apply_content_limits(&mut [], &mut units, 2, None));
        assert_eq!(units[0].text.as_deref(), Some("ab"));
        assert_eq!(units[1].text.as_deref(), Some(""));
        assert_eq!(units[0].completeness, Completeness::Partial);
        assert_eq!(units[1].completeness, Completeness::Exact);
    }

    /// Checks broken reads classify as failures with metadata-stage precedence.
    #[test]
    fn failed_reads_classify_as_failures_not_unsupported() {
        let mut metadata = MetadataReport::default();
        metadata.metadata.status = OperationStatus::Failed;
        let mut legacy = crate::InspectReport::default();
        legacy.error("csv", "CSV preview exceeds 1 MiB read cap");
        let report = assemble(metadata, legacy, &crate::reports::ExtractOptions::default());
        assert!(report.content.units.is_empty());
        assert_eq!(report.status, OperationStatus::Failed);
        assert_eq!(report.metadata.status, OperationStatus::Failed);
        assert_eq!(report.selection.omitted, OmissionStatus::PartialFailure);
        assert_eq!(report.completeness, Completeness::Partial);
    }

    /// Checks the identification-only boundary still classifies as unsupported
    /// without mutating the nested standalone inspection status.
    #[test]
    fn identification_only_boundary_stays_unsupported() {
        let mut metadata = MetadataReport::default();
        metadata.metadata.status = OperationStatus::Complete;
        metadata.metadata.extraction_support = crate::reports::SupportStatus::IdentificationOnly;
        let legacy = crate::InspectReport::default();
        let report = assemble(metadata, legacy, &crate::reports::ExtractOptions::default());
        assert_eq!(report.selection.omitted, OmissionStatus::Unsupported);
        assert_eq!(report.status, OperationStatus::Unsupported);
        assert_eq!(report.metadata.status, OperationStatus::Complete);
    }

    /// Checks a failed identification-only document keeps its failure status.
    #[test]
    fn failed_identification_only_document_stays_failed_not_unsupported() {
        // COND-06: an identification-only format whose header parse failed at
        // the metadata stage (for example an encrypted MOBI with a broken EXTH
        // block) must keep the failed classification; `Unsupported` never
        // overwrites a preexisting failure.
        let mut metadata = MetadataReport::default();
        metadata.metadata.extraction_support = crate::reports::SupportStatus::IdentificationOnly;
        metadata.metadata.status = OperationStatus::Failed;
        metadata.metadata.completeness = Completeness::Partial;
        let legacy = crate::InspectReport::default();
        let report = assemble(metadata, legacy, &crate::reports::ExtractOptions::default());
        assert!(report.content.units.is_empty());
        assert_eq!(report.selection.omitted, OmissionStatus::PartialFailure);
        assert_eq!(report.status, OperationStatus::Failed);
        assert_eq!(report.metadata.status, OperationStatus::Failed);
        assert_eq!(report.completeness, Completeness::Partial);
    }

    /// Checks an unsupported metadata stage cannot overwrite a selection failure
    /// while preserving the nested metadata status independently.
    ///
    /// Accepts no parameters. Returns unit after assembling a report whose
    /// metadata stage reported `Unsupported` while the legacy reader failed
    /// during selection. A failure status relabeled to unsupported or a mutated
    /// nested metadata status panics.
    #[test]
    fn unsupported_metadata_stage_cannot_overwrite_selection_failure() {
        // COND-06 follow-up: the final metadata-`Unsupported` override is
        // conditional, so it can never mask a selection or metadata failure.
        // Warning-only legacy errors keep their partial classification
        // because no OCR path reports metadata `Unsupported`.
        let mut metadata = MetadataReport::default();
        metadata.metadata.status = OperationStatus::Unsupported;
        let mut legacy = crate::InspectReport::default();
        legacy.error("selection", "native selection failed before extraction");
        let report = assemble(metadata, legacy, &crate::reports::ExtractOptions::default());
        assert!(report.content.units.is_empty());
        assert_eq!(report.selection.omitted, OmissionStatus::PartialFailure);
        assert_eq!(report.status, OperationStatus::Failed);
        assert_eq!(report.completeness, Completeness::Unavailable);
        assert_eq!(report.metadata.status, OperationStatus::Unsupported);
        assert!(report.metadata.diagnostics.is_empty());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "selection")
        );
    }

    /// Checks `render_markdown` emits bodymatter:v1 page markers with labels,
    /// origins, and partial flags while omitting synthetic headings and preamble.
    #[test]
    fn render_markdown_emits_bodymatter_page_markers_and_no_preamble() {
        let mut page1 = text_unit(
            1,
            NativeUnit::PhysicalPage,
            Some("page 1".into()),
            "Quarterly results improved.\n".into(),
            ContentOrigin::Native,
        );
        page1.label = Some("iv".into());
        let mut page2 = text_unit(
            2,
            NativeUnit::PhysicalPage,
            Some("page 2".into()),
            "Appendix A".into(),
            ContentOrigin::Ocr,
        );
        page2.completeness = Completeness::Partial;
        let page3 = text_unit(
            3,
            NativeUnit::PhysicalPage,
            Some("page 3".into()),
            "Mixed text".into(),
            ContentOrigin::Mixed,
        );
        let report = ExtractReport {
            content: ExtractedContent {
                headers: Vec::new(),
                units: vec![page1, page2, page3],
            },
            ..ExtractReport::default()
        };

        let with_markers = render_markdown(&report);
        assert!(!with_markers.contains("# Glassbox extraction"));
        assert!(!with_markers.contains("## Unit"));
        assert!(!with_markers.contains("> Partial output:"));
        assert_eq!(
            with_markers,
            concat!(
                "<!-- bodymatter:v1 <page number=\"1\" label=\"iv\" origin=\"native\"/> -->\n\n",
                "Quarterly results improved.\n\n",
                "<!-- bodymatter:v1 <page number=\"2\" origin=\"ocr\" completeness=\"partial\"/> -->\n\n",
                "Appendix A\n\n",
                "<!-- bodymatter:v1 <page number=\"3\" origin=\"mixed\"/> -->\n\n",
                "Mixed text\n",
            )
        );

        let without_markers = render_markdown_with_options(&report, false);
        assert!(!without_markers.contains("bodymatter:v1"));
        assert_eq!(
            without_markers,
            "Quarterly results improved.\n\nAppendix A\n\nMixed text\n"
        );
    }

    /// Checks `render_markdown` groups row-bearing `DatasetRow` units under one
    /// unnamed `<sheet number="1"/>` GFM table using budgeted content headers
    /// while leaving rowless JSON/XML fallback previews marker-free.
    #[test]
    fn render_markdown_handles_tabular_grouping_and_independent_counters() {
        let mut tabular = ExtractReport::default();
        tabular.content.headers = vec!["k".into(), "v".into()];
        tabular.content.units = vec![
            dataset_unit(
                1,
                NativeUnit::DatasetRow,
                Some("row 1".into()),
                "a\t1".into(),
                vec![ContentRow {
                    row_index: 1,
                    values: vec!["a".into(), "1".into()],
                }],
            ),
            dataset_unit(
                2,
                NativeUnit::DatasetRow,
                Some("row 2".into()),
                "b\t2".into(),
                vec![ContentRow {
                    row_index: 2,
                    values: vec!["b".into(), "2".into()],
                }],
            ),
        ];
        let rendered = render_markdown(&tabular);
        assert_eq!(
            rendered,
            concat!(
                "<!-- bodymatter:v1 <sheet number=\"1\"/> -->\n\n",
                "| k | v |\n",
                "| --- | --- |\n",
                "| a | 1 |\n",
                "| b | 2 |\n",
            )
        );
        assert!(!rendered.contains("a\t1"));
        assert_eq!(
            render_markdown_with_options(&tabular, false),
            "| k | v |\n| --- | --- |\n| a | 1 |\n| b | 2 |\n"
        );

        let fallback = ExtractReport {
            content: ExtractedContent {
                headers: Vec::new(),
                units: vec![dataset_unit(
                    1,
                    NativeUnit::DatasetRow,
                    Some("dataset/profile preview".into()),
                    "{\"ok\":true}".into(),
                    Vec::new(),
                )],
            },
            ..ExtractReport::default()
        };
        assert_eq!(render_markdown(&fallback), "{\"ok\":true}\n");
        assert_eq!(render_markdown(&ExtractReport::default()), "");
    }
}

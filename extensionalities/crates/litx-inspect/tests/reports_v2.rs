// Module: glassbox-inspect v2 report contract tests.
// File: reports_v2.rs.
// Responsibility: Prove typed metadata isolation, extraction separation, and public option boundaries.

use glassbox_inspect::InspectOptions as LegacyInspectOptions;
use glassbox_inspect::reports::*;
use serde_json::Value;
use std::collections::BTreeSet;

/// Builds a fully typed metadata fixture without body, row, or OCR output.
///
/// Accepts no parameters. Returns synthetic metadata for serialization checks.
/// This helper performs no I/O and has no failure states.
fn metadata_fixture() -> DocumentMetadata {
    DocumentMetadata {
        identity: SourceIdentity {
            path: "/sandbox/settling-in-test-reports/input.pdf".into(),
            byte_size: Some(4_096),
            opaque_file_id: Some("synthetic-file-id".to_owned()),
            consistency: ConsistencyStatus::Consistent,
        },
        format: FormatIdentity {
            extension: "pdf".to_owned(),
            detected_mime: Some("application/pdf".to_owned()),
            detected_name: Some("PDF document".to_owned()),
            bucket: "Documents".to_owned(),
            basis: "bytes".to_owned(),
            disagreement: false,
            detector: "file-format".to_owned(),
        },
        digest: DigestReport {
            algorithm: DigestAlgorithm::Sha256,
            digest: Some("a".repeat(64)),
            bytes_hashed: QualifiedCount {
                unit: NativeUnit::Byte,
                value: Some(4_096),
                source: FactSource::ByteScan,
                completeness: Completeness::Exact,
            },
            consistency: ConsistencyStatus::Consistent,
            source_identity: ConsistencyStatus::Consistent,
            snapshot_guarantee: SnapshotGuarantee::NotImmutable,
        },
        counts: vec![QualifiedCount {
            unit: NativeUnit::PhysicalPage,
            value: Some(1),
            source: FactSource::NativeStructure,
            completeness: Completeness::Exact,
        }],
        schema_identifiers: vec![SchemaIdentifier {
            kind: SchemaKind::Column,
            name: "account_id".to_owned(),
            source: FactSource::NativeStructure,
        }],
        embedded_metadata: vec![SourcedMetadata {
            field: EmbeddedMetadataField::Title,
            value: "Synthetic metadata fixture".to_owned(),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Claimed,
        }],
        text_layer: TextLayerStatus::Present,
        ocr_recommended: OcrRecommendation::NotRecommended,
        encryption: EncryptionStatus::NotEncrypted,
        extraction_support: SupportStatus::Supported,
        native_page_signals: vec![],
        pdf: None,
        raster: None,
        status: OperationStatus::Complete,
        completeness: Completeness::Exact,
        diagnostics: vec![],
        artifacts: vec![Artifact {
            kind: ArtifactKind::Thumbnail,
            path: "/sandbox/settling-in-test-reports/thumbnail.png".into(),
            source: FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: Some(320),
            height: Some(240),
        }],
        advice: vec![Advice {
            operation: AdviceOperation::Extract,
            arguments: vec!["extract".to_owned(), "--pages".to_owned(), "1-3".to_owned()],
            reason: "Select native pages explicitly".to_owned(),
            cost: AdviceCost::Unknown,
        }],
        provenance: vec![MetadataProvenance {
            reader: "pdf-native-v2".to_owned(),
            stage: "native-object-walk".to_owned(),
            source: FactSource::NativeStructure,
        }],
    }
}

/// Builds an extraction fixture whose sentinels must never enter metadata.
///
/// Accepts no parameters. Returns one selected unit with text, a row, and OCR provenance.
/// This helper performs no OCR or I/O and has no failure states.
fn extract_fixture() -> ExtractReport {
    let provenance = ExtractionProvenance {
        reader: "synthetic-extractor".to_owned(),
        stage: "selected-unit".to_owned(),
        source: FactSource::NativeStructure,
        origin: ContentOrigin::Ocr,
        ocr: Some(OcrProvenance {
            engine: "tesseract".to_owned(),
            language: "eng".to_owned(),
            mean_confidence: Some(0.75),
            raster_long_edge: Some(DEFAULT_OCR_RASTER_LONG_EDGE),
        }),
    };
    ExtractReport {
        schema_version: REPORT_SCHEMA_VERSION,
        kind: ReportKind::Extract,
        metadata: metadata_fixture(),
        content: ExtractedContent {
            units: vec![ContentUnit {
                unit_index: 1,
                unit: NativeUnit::PhysicalPage,
                source_part: Some("page-1".to_owned()),
                text: Some("SENTINEL_BODY SENTINEL_OCR".to_owned()),
                rows: vec![ContentRow {
                    row_index: 1,
                    values: vec!["SENTINEL_ROW".to_owned()],
                }],
                provenance: vec![provenance.clone()],
                completeness: Completeness::Exact,
            }],
        },
        provenance: vec![provenance],
        completeness: Completeness::Exact,
        selection: SelectionReport {
            requested: NativeSelection::Ranges {
                ranges: vec![NativeRange { start: 1, end: 1 }],
            },
            unit: NativeUnit::PhysicalPage,
            available: QualifiedCount {
                unit: NativeUnit::PhysicalPage,
                value: Some(2),
                source: FactSource::NativeStructure,
                completeness: Completeness::Exact,
            },
            returned_indices: vec![1],
            omitted: OmissionStatus::None,
            completeness: Completeness::Exact,
        },
    }
}

/// Collects every object key recursively from serialized JSON.
///
/// Accepts a JSON value and a mutable key set. Returns `()` after updating the set.
/// This helper has no failure states; arrays recurse without adding synthetic keys.
fn collect_keys(value: &Value, keys: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, nested) in object {
                keys.insert(key.clone());
                collect_keys(nested, keys);
            }
        }
        Value::Array(array) => {
            for nested in array {
                collect_keys(nested, keys);
            }
        }
        _ => {}
    }
}

/// Asserts that no forbidden payload key occurs at any serialized depth.
///
/// Accepts a JSON value and forbidden key names. Returns `()` when all keys are absent.
/// Panics with the serialized value when a forbidden key is present.
fn assert_forbidden_keys(value: &Value, forbidden: &[&str]) {
    let mut keys = BTreeSet::new();
    collect_keys(value, &mut keys);
    for key in forbidden {
        assert!(!keys.contains(*key), "forbidden key {key}: {value}");
    }
}

/// Verifies exact v2 markers and conservative default fact states.
///
/// Accepts no parameters. Returns `()` after checking inspect and extract defaults.
/// Panics if markers, unknown states, or unavailable optional facts differ.
#[test]
fn default_reports_emit_exact_markers_and_qualified_unknowns() {
    let inspect = serde_json::to_value(MetadataReport::default()).unwrap();
    assert_eq!(inspect["schema_version"], 2);
    assert_eq!(inspect["kind"], "inspect");
    assert_eq!(inspect["metadata"]["status"], "unknown");
    assert_eq!(inspect["metadata"]["completeness"], "unavailable");
    assert_eq!(inspect["metadata"]["digest"]["algorithm"], "sha256");
    assert_eq!(inspect["metadata"]["digest"]["digest"], Value::Null);
    assert_eq!(
        inspect["metadata"]["digest"]["bytes_hashed"]["value"],
        Value::Null
    );
    assert_eq!(
        inspect["metadata"]["digest"]["bytes_hashed"]["completeness"],
        "unavailable"
    );
    assert_eq!(inspect["metadata"]["text_layer"], "unknown");
    assert_eq!(inspect["metadata"]["ocr_recommended"], "unknown");
    assert_eq!(inspect["metadata"]["encryption"], "unknown");
    assert_eq!(inspect["metadata"]["extraction_support"], "unknown");

    let extract = serde_json::to_value(ExtractReport::default()).unwrap();
    assert_eq!(extract["schema_version"], 2);
    assert_eq!(extract["kind"], "extract");
    assert_eq!(extract["content"]["units"], serde_json::json!([]));
    assert_eq!(extract["selection"]["requested"]["mode"], "all");
    assert_eq!(extract["selection"]["completeness"], "unavailable");
}

/// Verifies the metadata-only option type exposes no content or OCR controls.
///
/// Accepts no parameters. Returns `()` after exhaustive destructuring and JSON key checks.
/// Panics on an unexpected default; adding a field fails this test at compile time.
#[test]
fn inspect_options_compile_boundary_contains_only_metadata_controls() {
    let InspectOptions { thumbnail, budget } = InspectOptions::default();
    assert!(thumbnail.is_none());
    assert_eq!(budget.shared_job_slots, 1);

    let options = InspectOptions {
        thumbnail: Some(ThumbnailOptions {
            path: "/sandbox/settling-in-test-reports/custom.png".into(),
            max_edge: DEFAULT_THUMBNAIL_MAX_EDGE,
        }),
        budget: SessionBudget::default(),
    };
    let value = serde_json::to_value(options).unwrap();
    let keys: BTreeSet<_> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, BTreeSet::from(["budget", "thumbnail"]));
    assert_forbidden_keys(
        &value,
        &[
            "selection",
            "max_tokens",
            "max_pages",
            "max_chars",
            "max_units",
            "ocr",
            "probe_ocr",
            "identify",
            "packet",
        ],
    );
}

/// Verifies extraction sentinels and legacy payload keys cannot appear in metadata.
///
/// Accepts no parameters. Returns `()` after recursive key and sentinel assertions.
/// Panics if body text, row values, OCR payload, or legacy fields leak into metadata.
#[test]
fn metadata_report_excludes_body_rows_ocr_and_legacy_payloads() {
    let extract = extract_fixture();
    let report = MetadataReport {
        metadata: extract.metadata.clone(),
        ..MetadataReport::default()
    };
    let serialized = serde_json::to_string(&report).unwrap();
    for sentinel in ["SENTINEL_BODY", "SENTINEL_ROW", "SENTINEL_OCR"] {
        assert!(!serialized.contains(sentinel), "{sentinel}: {serialized}");
    }
    let value = serde_json::to_value(report).unwrap();
    assert_forbidden_keys(
        &value,
        &[
            "body",
            "content",
            "text",
            "rows",
            "values",
            "ocr",
            "markdown",
            "packet",
            "preview_rows",
            "sample_values",
            "formulas",
        ],
    );
}

/// Verifies extraction keeps explicit content outside metadata with full provenance.
///
/// Accepts no parameters. Returns `()` after serialization and round-trip checks.
/// Panics if sentinels move, selection facts change, or deserialization differs.
#[test]
fn extract_report_separates_content_provenance_completeness_and_selection() {
    let report = extract_fixture();
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["kind"], "extract");
    assert_eq!(
        value["content"]["units"][0]["text"],
        "SENTINEL_BODY SENTINEL_OCR"
    );
    assert_eq!(
        value["content"]["units"][0]["rows"][0]["values"][0],
        "SENTINEL_ROW"
    );
    assert!(value["metadata"].get("content").is_none());
    assert!(value["metadata"].get("text").is_none());
    assert!(value["metadata"].get("rows").is_none());
    assert_eq!(value["provenance"][0]["origin"], "ocr");
    assert_eq!(value["provenance"][0]["ocr"]["raster_long_edge"], 3072);
    assert_eq!(value["completeness"], "exact");
    assert_eq!(value["selection"]["requested"]["mode"], "ranges");
    assert_eq!(
        value["selection"]["returned_indices"],
        serde_json::json!([1])
    );

    let decoded: ExtractReport = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, report);
}

/// Verifies exact, claimed, partial, and unavailable counts retain units and sources.
///
/// Accepts no parameters. Returns `()` after comparing the complete serialized counts.
/// Panics if any qualification or unavailable null value is normalized away.
#[test]
fn qualified_counts_preserve_all_completeness_states() {
    let counts = vec![
        QualifiedCount {
            unit: NativeUnit::PhysicalPage,
            value: Some(12),
            source: FactSource::NativeStructure,
            completeness: Completeness::Exact,
        },
        QualifiedCount {
            unit: NativeUnit::Slide,
            value: Some(8),
            source: FactSource::EmbeddedProperty,
            completeness: Completeness::Claimed,
        },
        QualifiedCount {
            unit: NativeUnit::Worksheet,
            value: Some(2),
            source: FactSource::ContainerIndex,
            completeness: Completeness::Partial,
        },
        QualifiedCount {
            unit: NativeUnit::LogicalLine,
            value: None,
            source: FactSource::Unknown,
            completeness: Completeness::Unavailable,
        },
    ];
    assert_eq!(
        serde_json::to_value(counts).unwrap(),
        serde_json::json!([
            {"unit":"physical_page","value":12,"source":"native_structure","completeness":"exact"},
            {"unit":"slide","value":8,"source":"embedded_property","completeness":"claimed"},
            {"unit":"worksheet","value":2,"source":"container_index","completeness":"partial"},
            {"unit":"logical_line","value":null,"source":"unknown","completeness":"unavailable"}
        ])
    );
}

/// Verifies page object evidence does not imply readable text or OCR usefulness.
///
/// Accepts no parameters. Returns `()` after checking signals and uncertainty serialization.
/// Panics if unknown assessments or bounded-walk uncertainty are strengthened.
#[test]
fn native_page_signals_preserve_object_evidence_and_uncertainty() {
    let metadata = DocumentMetadata {
        native_page_signals: vec![NativePageSignal {
            page_index: 1,
            text_layer: TextLayerStatus::Unknown,
            ocr_recommended: OcrRecommendation::Unknown,
            object_counts: vec![QualifiedCount {
                unit: NativeUnit::PdfObject(PdfObjectKind::Text),
                value: Some(0),
                source: FactSource::NativeStructure,
                completeness: Completeness::Exact,
            }],
            coverage: Completeness::Partial,
            uncertainties: vec![Uncertainty {
                code: "form_depth_limit".to_owned(),
                message: "Nested form traversal stopped at the approved depth".to_owned(),
                source: FactSource::NativeStructure,
            }],
            width_points: None,
            height_points: None,
            rotation_quarter_turns: None,
        }],
        ..DocumentMetadata::default()
    };
    let value = serde_json::to_value(metadata).unwrap();
    let page = &value["native_page_signals"][0];
    assert_eq!(page["text_layer"], "unknown");
    assert_eq!(page["ocr_recommended"], "unknown");
    assert_eq!(page["object_counts"][0]["unit"]["pdf_object"], "text");
    assert_eq!(page["coverage"], "partial");
    assert_eq!(page["uncertainties"][0]["code"], "form_depth_limit");
}

/// Verifies digest identity failures remain visible and no immutable snapshot is claimed.
///
/// Accepts no parameters. Returns `()` after inspecting changed-source digest facts.
/// Panics if an unavailable digest becomes trusted or consistency qualifications disappear.
#[test]
fn digest_and_identity_expose_consistency_and_snapshot_limits() {
    let mut metadata = metadata_fixture();
    metadata.identity.consistency = ConsistencyStatus::SourceChanged;
    metadata.digest.digest = None;
    metadata.digest.bytes_hashed.completeness = Completeness::Partial;
    metadata.digest.consistency = ConsistencyStatus::SourceChanged;
    metadata.digest.source_identity = ConsistencyStatus::Mismatched;
    let value = serde_json::to_value(metadata).unwrap();
    assert_eq!(value["identity"]["consistency"], "source_changed");
    assert_eq!(value["digest"]["digest"], Value::Null);
    assert_eq!(value["digest"]["bytes_hashed"]["completeness"], "partial");
    assert_eq!(value["digest"]["consistency"], "source_changed");
    assert_eq!(value["digest"]["source_identity"], "mismatched");
    assert_eq!(value["digest"]["snapshot_guarantee"], "not_immutable");
}

/// Verifies sourced properties, schema names, artifacts, and inert advice stay typed.
///
/// Accepts no parameters. Returns `()` after checking their serialized representations.
/// Panics if schema identifiers gain values or advice becomes a shell command.
#[test]
fn sourced_metadata_artifacts_and_advice_use_dedicated_structures() {
    let value = serde_json::to_value(metadata_fixture()).unwrap();
    assert_eq!(value["embedded_metadata"][0]["field"], "title");
    assert_eq!(value["embedded_metadata"][0]["source"], "embedded_property");
    assert_eq!(value["schema_identifiers"][0]["name"], "account_id");
    assert!(value["schema_identifiers"][0].get("value").is_none());
    assert_eq!(value["artifacts"][0]["kind"], "thumbnail");
    assert_eq!(value["artifacts"][0]["width"], 320);
    assert_eq!(
        value["advice"][0]["arguments"],
        serde_json::json!(["extract", "--pages", "1-3"])
    );
    assert_eq!(value["advice"][0]["cost"], "unknown");
    assert_forbidden_keys(&value, &["command", "shell_command"]);
}

/// Verifies extraction defaults and all native-unit selection request shapes.
///
/// Accepts no parameters. Returns `()` after checking all, first-N, and range selections.
/// Panics if defaults drift or tagged selection serialization changes.
#[test]
fn extract_options_defaults_and_selection_shapes_are_explicit() {
    let defaults = ExtractOptions::default();
    assert_eq!(defaults.selection, NativeSelection::All);
    assert_eq!(defaults.max_tokens, None);
    assert_eq!(defaults.max_chars, 64 << 20);
    assert_eq!(defaults.max_units, 5);
    assert!(defaults.ocr.enabled);
    assert_eq!(defaults.ocr.language, "eng");
    assert_eq!(defaults.ocr.raster_long_edge, 3072);
    assert_eq!(defaults.budget.shared_job_slots, 1);

    let first = serde_json::to_value(ExtractOptions {
        selection: NativeSelection::FirstN { count: 5 },
        ..ExtractOptions::default()
    })
    .unwrap();
    assert_eq!(
        first["selection"],
        serde_json::json!({"mode":"first_n","count":5})
    );
    let ranges = serde_json::to_value(ExtractOptions {
        selection: NativeSelection::Ranges {
            ranges: vec![
                NativeRange { start: 3, end: 7 },
                NativeRange { start: 10, end: 10 },
            ],
        },
        ..ExtractOptions::default()
    })
    .unwrap();
    assert_eq!(
        ranges["selection"],
        serde_json::json!({
            "mode":"ranges",
            "ranges":[{"start":3,"end":7},{"start":10,"end":10}]
        })
    );
}

/// Verifies v2 status variants retain the later CLI exit-status foundation.
///
/// Accepts no parameters. Returns `()` after checking every operation status marker.
/// Panics if complete, partial, unsupported, failed, or unknown markers change.
#[test]
fn operation_statuses_remain_explicit_for_later_exit_mapping() {
    let statuses = vec![
        OperationStatus::Complete,
        OperationStatus::Partial,
        OperationStatus::Unsupported,
        OperationStatus::Failed,
        OperationStatus::Unknown,
    ];
    assert_eq!(
        serde_json::to_value(statuses).unwrap(),
        serde_json::json!(["complete", "partial", "unsupported", "failed", "unknown"])
    );
}

/// Verifies the legacy body-producing options remain compilable and unchanged.
///
/// Accepts no parameters. Returns `()` after inspecting and mutating legacy controls.
/// Panics if legacy defaults differ; v2 migration does not alter legacy behavior.
#[test]
fn legacy_options_remain_available_during_reader_migration() {
    let mut legacy = LegacyInspectOptions::default();
    assert_eq!(legacy.max_pages, Some(5));
    assert_eq!(legacy.max_chars, 16_000);
    assert!(!legacy.probe_ocr);
    legacy.probe_ocr = true;
    legacy.max_tokens = Some(7);
    assert!(legacy.probe_ocr);
    assert_eq!(legacy.max_tokens, Some(7));
}

// Module: glassbox-inspect metadata-only behavior tests.
// File: metadata_only.rs.
// Responsibility: Prove family coverage, streamed identity, content isolation, thumbnails, and legacy adapter separation.

use glassbox_inspect::reports::{
    InspectOptions, NativeUnit, OperationStatus, ReportKind, SupportStatus, ThumbnailOptions,
};
use glassbox_inspect::{extract_transitional, inspect};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Returns the repository fixture directory without accessing external data.
///
/// Accepts no parameters. Returns the checked-in fixture path. No I/O or failure
/// states occur until a caller opens one of the fixtures.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Creates an isolated ignored directory for generated test inputs and artifacts.
///
/// Accepts no parameters. Returns a unique target-directory path. Directory
/// creation failures panic with the path; no production or source data is touched.
fn scratch() -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/settling-in-tests")
        .join(format!("metadata-only-{}-{unique}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Writes one generated fixture below the test-owned target directory.
///
/// Accepts a directory, file name, and bytes. Returns the written path. Open/write
/// failures panic; the helper never writes into the checked-in fixture directory.
fn fixture(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Creates a minimal ZIP-family fixture with caller-selected entry names and bytes.
///
/// Accepts a directory, file name, and entries. Returns the finished archive path.
/// ZIP creation, entry writing, and finalization failures panic; entries are test
/// metadata fixtures rather than private source documents.
fn zip_fixture(directory: &Path, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
    let path = directory.join(name);
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for (name, bytes) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
    path
}

/// Recursively rejects legacy content fields anywhere in a serialized v2 report.
///
/// Accepts a JSON value. Returns unit when no body/content/row/OCR-output fields
/// exist. Assertion failures identify the forbidden key; metadata's OCR advice
/// enum remains legal because it contains no OCR result payload.
fn assert_no_content_fields(value: &Value) {
    const FORBIDDEN: &[&str] = &[
        "text",
        "content",
        "markdown",
        "rows",
        "cells",
        "ocr_text",
        "sample_values",
        "preview",
    ];
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                assert!(
                    !FORBIDDEN.contains(&key.as_str()),
                    "forbidden content key: {key}"
                );
                assert_no_content_fields(value);
            }
        }
        Value::Array(values) => values.iter().for_each(assert_no_content_fields),
        _ => {}
    }
}

/// Checks the legacy v2 metadata-wrapper envelope, streamed digest, and content-free serialization.
///
/// Accepts an input path and report. Returns the serialized JSON for further
/// family-specific assertions. Mismatched schema, kind, digest, byte count, or
/// forbidden body fields panic with the source path.
fn assert_common(path: &Path, report: &glassbox_inspect::reports::MetadataReport) -> Value {
    assert_eq!(
        report.schema_version,
        glassbox_inspect::reports::LEGACY_REPORT_SCHEMA_VERSION,
        "{}",
        path.display()
    );
    assert_eq!(report.kind, ReportKind::Inspect, "{}", path.display());
    assert_ne!(
        report.metadata.status,
        OperationStatus::Failed,
        "{}: {report:#?}",
        path.display()
    );
    let bytes = std::fs::read(path).unwrap();
    let expected = format!("{:x}", Sha256::digest(&bytes));
    assert_eq!(
        report.metadata.digest.digest.as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(
        report.metadata.digest.bytes_hashed.value,
        Some(bytes.len() as u64)
    );
    assert_eq!(report.metadata.digest.bytes_hashed.unit, NativeUnit::Byte);
    let value = serde_json::to_value(report).unwrap();
    assert_no_content_fields(&value);
    value
}

/// Exercises every supported metadata family with body sentinels where practical.
///
/// Accepts no parameters. Returns unit after checking reports and summary strings.
/// Missing fixtures, reader failures, sentinel leakage, or content fields panic.
#[test]
fn supported_families_publish_structure_without_body_values() {
    let directory = scratch();
    let sentinel = "SENTINEL_BODY_VALUE_6f27a9";
    let text = fixture(
        &directory,
        "body.txt",
        format!("first line\n{sentinel}\n").as_bytes(),
    );
    let csv = fixture(
        &directory,
        "table.csv",
        format!("name,value\nrow,{sentinel}\n").as_bytes(),
    );
    let json = fixture(
        &directory,
        "tree.json",
        format!("{{\"name\":\"{sentinel}\",\"nested\":{{\"count\":2}}}}").as_bytes(),
    );
    let xml = fixture(
        &directory,
        "tree.xml",
        format!("<root attribute=\"{sentinel}\"><child>{sentinel}</child></root>").as_bytes(),
    );
    let parquet = fixture(&directory, "identity.parquet", b"PAR1body-sentinelPAR1");
    let pages = zip_fixture(
        &directory,
        "sample.pages",
        &[(
            "QuickLook/Preview.png",
            &std::fs::read(fixtures().join("image.png")).unwrap(),
        )],
    );

    let mut inputs = vec![
        fixtures().join("native.pdf"),
        fixtures().join("notes.docx"),
        fixtures().join("deck.pptx"),
        fixtures().join("grid.xlsx"),
        fixtures().join("notes.odt"),
        fixtures().join("deck.odp"),
        fixtures().join("grid.ods"),
        fixtures().join("native.epub"),
        fixtures().join("native.mobi"),
        fixtures().join("listing.zip"),
        fixtures().join("pages.cbz"),
        fixtures().join("pages4.cbr"),
        fixtures().join("image.png"),
        fixtures().join("single.djvu"),
        text.clone(),
        csv.clone(),
        json.clone(),
        xml.clone(),
        parquet,
        pages,
    ];
    let xlsm = fixture(
        &directory,
        "workbook.xlsm",
        &std::fs::read(fixtures().join("grid.xlsx")).unwrap(),
    );
    inputs.push(xlsm);

    for path in inputs {
        let report = inspect(&path, &InspectOptions::default());
        assert_common(&path, &report);
        let summary = glassbox_inspect::metadata::summary(&report);
        assert!(
            summary.contains("- Schema: 3"),
            "{} summary missing v3 schema marker: {summary}",
            path.display()
        );
        assert!(
            !summary.contains(sentinel),
            "{} leaked its body sentinel",
            path.display()
        );
        let plain = glassbox_inspect::metadata::summary_text(&report, false);
        assert!(
            plain.contains("Schema: 3\n"),
            "{} plain-text summary missing v3 schema marker: {plain}",
            path.display()
        );
        assert!(
            !plain.contains(sentinel),
            "{} plain-text summary leaked its body sentinel",
            path.display()
        );
        assert!(!serde_json::to_string(&report).unwrap().contains(sentinel));
    }

    // Literal filename suffix is preserved on misnamed files and family aliases.
    let xlsm_path = directory.join("workbook.xlsm");
    let xlsm_report = inspect(&xlsm_path, &InspectOptions::default());
    assert_eq!(xlsm_report.metadata.format.extension, "xlsm");
    assert!(!xlsm_report.metadata.format.disagreement);

    let misnamed = fixture(
        &directory,
        "report.pdf",
        &std::fs::read(fixtures().join("notes.docx")).unwrap(),
    );
    let misnamed_report = inspect(&misnamed, &InspectOptions::default());
    assert_eq!(misnamed_report.metadata.format.extension, "pdf");
    assert!(misnamed_report.metadata.format.disagreement);
    assert_eq!(misnamed_report.metadata.format.bucket, "Office Docs");
}

/// Verifies generated JSON, XML, CSV, and plain-text schema/count metadata.
///
/// Accepts no parameters. Returns unit after checking native units and identifiers.
/// Missing identifiers, incorrect counts, or retained scalar/cell/text values panic.
#[test]
fn structured_readers_expose_names_and_counts_not_values() {
    let directory = scratch();
    let sentinel = "SENTINEL_BODY_VALUE_91c2";
    let json = fixture(
        &directory,
        "tree.json",
        format!("{{\"account_id\":\"{sentinel}\",\"items\":[1,true,null]}}").as_bytes(),
    );
    let report = inspect(&json, &InspectOptions::default());
    assert_common(&json, &report);
    assert!(
        report
            .metadata
            .counts
            .iter()
            .any(|count| count.unit == NativeUnit::JsonScalar && count.value == Some(4))
    );
    assert!(
        report
            .metadata
            .schema_identifiers
            .iter()
            .any(|identifier| identifier.name.contains("account_id"))
    );

    let xml = fixture(
        &directory,
        "tree.xml",
        format!("<root><item code=\"{sentinel}\">{sentinel}</item><item/></root>").as_bytes(),
    );
    let report = inspect(&xml, &InspectOptions::default());
    assert_common(&xml, &report);
    assert!(
        report
            .metadata
            .counts
            .iter()
            .any(|count| count.unit == NativeUnit::XmlElement && count.value == Some(3))
    );
    assert!(
        report
            .metadata
            .schema_identifiers
            .iter()
            .any(|identifier| identifier.name == "/root/item")
    );

    let csv = fixture(
        &directory,
        "table.tsv",
        format!("account_id\tvalue\n1\t{sentinel}\n2\tother\n").as_bytes(),
    );
    let report = inspect(&csv, &InspectOptions::default());
    assert_common(&csv, &report);
    assert!(
        report
            .metadata
            .counts
            .iter()
            .any(|count| count.unit == NativeUnit::DatasetRow && count.value == Some(2))
    );
    assert!(
        report
            .metadata
            .schema_identifiers
            .iter()
            .any(|identifier| identifier.name == "account_id")
    );

    let text = fixture(
        &directory,
        "body.txt",
        format!("one\n{sentinel}\nthree").as_bytes(),
    );
    let report = inspect(&text, &InspectOptions::default());
    assert_common(&text, &report);
    assert!(
        report
            .metadata
            .counts
            .iter()
            .any(|count| count.unit == NativeUnit::LogicalLine && count.value == Some(3))
    );
}

/// Verifies PDF security, page geometry, and object signals without text inference.
///
/// Accepts no parameters. Returns unit after inspecting the native PDF fixture.
/// Missing page/object facts or strengthened text/OCR assessments panic.
#[test]
fn pdf_signals_use_native_geometry_and_bounded_object_kinds() {
    let path = fixtures().join("native.pdf");
    let report = inspect(&path, &InspectOptions::default());
    assert_common(&path, &report);
    assert!(report.metadata.pdf.is_some());
    assert!(!report.metadata.native_page_signals.is_empty());
    let page = &report.metadata.native_page_signals[0];
    assert!(page.width_points.is_some() && page.height_points.is_some());
    assert_eq!(
        page.text_layer,
        glassbox_inspect::reports::TextLayerStatus::Unknown
    );
    assert_eq!(
        page.ocr_recommended,
        glassbox_inspect::reports::OcrRecommendation::Unknown
    );
    assert!(page.object_counts.iter().any(|count| count.value.is_some()));
}

/// Verifies DjVu and Parquet remain explicit identification-only boundaries.
///
/// Accepts no parameters. Returns unit after checking support and operation status.
/// Accidental extraction support, content fields, or a successful body parse panic.
#[test]
fn identification_only_formats_do_not_gain_extraction_support() {
    for path in [fixtures().join("single.djvu"), {
        let directory = scratch();
        fixture(&directory, "identity.parquet", b"PAR1unknown-bodyPAR1")
    }] {
        let report = inspect(&path, &InspectOptions::default());
        assert_common(&path, &report);
        assert_eq!(
            report.metadata.extraction_support,
            SupportStatus::IdentificationOnly
        );
        assert_eq!(report.metadata.status, OperationStatus::Complete);
    }
}

/// Verifies corrupt input retains identity/digest evidence and a qualified failure.
///
/// Accepts no parameters. Returns unit after inspecting a malformed PDF fixture.
/// Missing diagnostics, fabricated content, or a trusted successful status panic.
#[test]
fn corrupt_pdf_is_failed_without_legacy_content_fields() {
    let path = fixtures().join("broken.pdf");
    let report = inspect(&path, &InspectOptions::default());
    let value = serde_json::to_value(&report).unwrap();
    assert_no_content_fields(&value);
    assert_eq!(report.metadata.status, OperationStatus::Failed);
    assert!(report.metadata.digest.digest.is_some());
    assert!(!report.metadata.diagnostics.is_empty());
}

/// Verifies thumbnails are created only by explicit options and respect the edge cap.
///
/// Accepts no parameters. Returns unit after default and explicit PDF inspections.
/// Default rendering, missing artifacts, invalid dimensions, or sentinel content panic.
#[test]
fn thumbnail_is_explicit_bounded_and_content_free() {
    let path = fixtures().join("native.pdf");
    let ordinary = inspect(&path, &InspectOptions::default());
    assert!(ordinary.metadata.artifacts.is_empty());
    let directory = scratch();
    let thumbnail = directory.join("first-page.png");
    let options = InspectOptions {
        thumbnail: Some(ThumbnailOptions {
            path: thumbnail.clone(),
            max_edge: 64,
        }),
        ..InspectOptions::default()
    };
    let report = inspect(&path, &options);
    assert_common(&path, &report);
    assert_eq!(report.metadata.artifacts.len(), 1);
    let artifact = &report.metadata.artifacts[0];
    assert_eq!(artifact.path, thumbnail);
    assert!(artifact.width.unwrap() <= 64 && artifact.height.unwrap() <= 64);
    assert!(std::fs::metadata(&thumbnail).unwrap().len() > 0);
}

/// Verifies explicit transitional extraction still returns the original body.
///
/// Accepts no parameters. Returns unit after extracting the plain-text fixture.
/// This guards migration coverage: content assertions moved to the named adapter,
/// while `inspect` remains metadata-only and never supplies this body field.
#[test]
fn transitional_adapter_retains_legacy_content_behavior() {
    let path = fixtures().join("hello.txt");
    let report = extract_transitional(&path, &glassbox_inspect::InspectOptions::default());
    assert!(
        report
            .text
            .content
            .contains("small self-authored inspection fixture")
    );
    let metadata = inspect(&path, &InspectOptions::default());
    let value = serde_json::to_value(metadata).unwrap();
    assert_no_content_fields(&value);
}

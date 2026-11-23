// Module: glassbox-inspect integration tests.
// File: support_precedence.rs.
// Responsibility: Verify failure-over-unsupported precedence for identification-only documents and raster dimension publication.
use glassbox_inspect::reports::{
    EmbeddedMetadataField, ExtractOptions, NativeSelection, OcrOptions, OmissionStatus,
    OperationStatus, RasterDimensions, SupportStatus,
};
use glassbox_inspect::{extract, inspect};
use std::path::{Path, PathBuf};

/// Returns a checked-in fixture path without reading or mutating its contents.
///
/// Accepts a fixture file name. Returns its absolute path. Missing fixtures
/// fail in the subsequent reader call with a structured report diagnostic.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Creates a unique test-owned directory below the ignored target tree.
///
/// Accepts a test label. Returns the created directory path. Filesystem
/// failures panic; checked-in fixtures and production data are untouched.
fn scratch(label: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/support-precedence")
        .join(format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Builds v2 extraction controls with OCR disabled for deterministic native tests.
///
/// Accepts a native selection. Returns extraction options with the requested
/// selection. No file, model, or network access occurs.
fn options(selection: NativeSelection) -> ExtractOptions {
    ExtractOptions {
        selection,
        ocr: OcrOptions {
            enabled: false,
            ..OcrOptions::default()
        },
        ..ExtractOptions::default()
    }
}

/// Writes one encrypted MOBI whose EXTH block signature is missing.
///
/// Accepts the output path. Returns unit. The PalmDB and MOBI headers are
/// valid, encryption marks the format identification-only, and the EXTH flag
/// points at a non-EXTH block so the header reader fails. Write failures panic.
fn broken_exth_mobi(path: &Path) {
    let mut head = vec![0u8; 148];
    head[0..2].copy_from_slice(&1u16.to_be_bytes()); // PalmDOC compression
    head[8..10].copy_from_slice(&1u16.to_be_bytes()); // text record count
    head[12..14].copy_from_slice(&1u16.to_be_bytes()); // encryption type
    head[16..20].copy_from_slice(b"MOBI");
    head[20..24].copy_from_slice(&116u32.to_be_bytes()); // MOBI header size
    head[28..32].copy_from_slice(&65001u32.to_be_bytes()); // UTF-8 encoding
    head[128..132].copy_from_slice(&0x40u32.to_be_bytes()); // EXTH flag
    head[132..144].copy_from_slice(b"NOT_AN_EXTH!"); // broken EXTH signature
    let mut bytes = vec![0u8; 94];
    bytes[60..68].copy_from_slice(b"BOOKMOBI");
    bytes[76..78].copy_from_slice(&2u16.to_be_bytes()); // record count
    bytes[78..82].copy_from_slice(&94u32.to_be_bytes()); // record 0 offset
    bytes[86..90].copy_from_slice(&242u32.to_be_bytes()); // record 1 offset
    bytes.extend_from_slice(&head);
    bytes.extend_from_slice(&[0u8; 16]); // record 1 payload
    std::fs::write(path, bytes).unwrap();
}

/// Verifies a failing identification-only document keeps failure precedence.
///
/// Accepts no parameters. Returns unit after v2 inspect/extract of a crafted
/// encrypted MOBI with a broken EXTH block. Inspect or extract downgrading the
/// preexisting reader failure to the unsupported boundary panics.
#[test]
fn failing_identification_only_document_reports_failure_over_unsupported() {
    let directory = scratch("broken-exth");
    let path = directory.join("broken.mobi");
    broken_exth_mobi(&path);

    let inspected = inspect(&path, &Default::default());
    // The format was identified and stays identification-only, but the header
    // parse failed at the metadata stage, so inspection reports failure.
    assert_eq!(
        inspected.metadata.extraction_support,
        SupportStatus::IdentificationOnly
    );
    assert_eq!(inspected.metadata.status, OperationStatus::Failed);
    assert!(
        inspected
            .metadata
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "metadata_reader_failed")
    );

    // Extraction must not overwrite the preexisting failure with Unsupported.
    let extracted = extract(&path, &options(NativeSelection::All));
    assert_eq!(extracted.metadata.status, OperationStatus::Failed);
    assert_eq!(extracted.selection.omitted, OmissionStatus::PartialFailure);
    assert!(extracted.content.units.is_empty());
}

/// Verifies a cleanly identified encrypted MOBI keeps the unsupported boundary.
///
/// Accepts no parameters. Returns unit after v2 extract of the checked-in
/// encrypted MOBI fixture. A failed status, fabricated content, or a
/// different omission reason than unsupported panics.
#[test]
fn clean_identification_only_document_keeps_the_unsupported_boundary() {
    let path = fixture("encrypted.azw");
    let inspected = inspect(&path, &Default::default());
    assert_eq!(
        inspected.metadata.extraction_support,
        SupportStatus::IdentificationOnly
    );
    assert_eq!(inspected.metadata.status, OperationStatus::Complete);
    let title = inspected
        .metadata
        .embedded_metadata
        .iter()
        .find(|property| property.field == EmbeddedMetadataField::Title)
        .map(|property| property.value.clone());
    assert_eq!(title, Some("Native Pocket Book".to_owned()));

    let extracted = extract(&path, &options(NativeSelection::All));
    assert_eq!(extracted.metadata.status, OperationStatus::Unsupported);
    assert_eq!(extracted.selection.omitted, OmissionStatus::Unsupported);
    assert!(extracted.content.units.is_empty());
}

/// Verifies raster inspection publishes decoded dimensions in the raster field.
///
/// Accepts no parameters. Returns unit after v2 inspection of the checked-in
/// raster fixture. Discarded dimensions, pixel values inside PDF-style point
/// fields, a missing JSON disclosure, or a failed status panics.
#[test]
fn raster_inspection_publishes_decoded_dimensions_with_pixel_disclosure() {
    let path = fixture("image.png");
    let inspected = inspect(&path, &Default::default());
    assert_eq!(inspected.metadata.status, OperationStatus::Complete);
    assert_eq!(
        inspected.metadata.extraction_support,
        SupportStatus::Supported
    );
    // COND-09: decoded pixel geometry lives in the dedicated raster field with
    // pixel-named members. It never enters the PDF-style width_points or
    // height_points members of native page signals, and no DPI is inferred.
    assert_eq!(
        inspected.metadata.raster,
        Some(RasterDimensions {
            width_pixels: 640,
            height_pixels: 400,
        })
    );
    assert!(inspected.metadata.native_page_signals.is_empty());

    // The field serializes with pixel-named keys, non-raster reports omit the
    // key entirely, and older JSON without the key still deserializes.
    let value = serde_json::to_value(&inspected.metadata).unwrap();
    assert_eq!(value["raster"]["width_pixels"], 640);
    assert_eq!(value["raster"]["height_pixels"], 400);
    let encrypted = inspect(&fixture("encrypted.azw"), &Default::default());
    assert!(
        serde_json::to_value(&encrypted.metadata)
            .unwrap()
            .get("raster")
            .is_none()
    );
    let mut stripped = value;
    stripped.as_object_mut().unwrap().remove("raster");
    let backward: glassbox_inspect::reports::DocumentMetadata =
        serde_json::from_value(stripped).unwrap();
    assert_eq!(backward.raster, None);
}

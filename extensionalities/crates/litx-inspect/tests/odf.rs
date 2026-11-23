// Module: glassbox-inspect integration tests.
// File: odf.rs.
// Responsibility: Exercise OpenDocument repeated-cell bounds without changing reader behavior.
use glassbox_inspect::{InspectOptions, extract_transitional as inspect};
use std::path::PathBuf;
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

#[test]
fn ods_repeated_cells_are_profiled_without_expanding_blank_tail() {
    let r = inspect(&fixture("grid.ods"), &InspectOptions::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.counts.sheets, Some(1));
    let sheet = &r.sheets[0];
    assert_eq!(sheet.name, "Notes");
    assert_eq!(sheet.used_range.as_deref(), Some("A1:B4"));
    assert_eq!((sheet.rows, sheet.cols), (4, 2));
    assert_eq!(sheet.non_empty_cells, 8);
    assert_eq!(sheet.formula_count, 1);
    assert_eq!(sheet.merged_ranges, ["B4:C4"]);
    assert_eq!(sheet.columns[0].inferred_type, "number");
    assert_eq!(sheet.columns[1].non_empty_count, 3);
    assert_eq!(sheet.columns[1].distinct_count, 2);
    assert!(sheet.columns[1].is_prose_candidate);
    assert!(sheet.is_longest);
    assert_eq!(r.preflight[0].unit_kind, "sheet");
    assert!(!r.preflight[0].needs_ocr);
    assert_eq!(r.text.content.matches("We can inspect").count(), 2);
}

#[test]
fn odp_native_slides_and_images_are_counted() {
    let r = inspect(&fixture("deck.odp"), &InspectOptions::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.counts.slides, Some(2));
    assert_eq!(r.preflight.len(), 2);
    assert!(!r.preflight[0].needs_ocr);
    assert!(r.preflight[1].needs_ocr);
    assert_eq!(
        r.text.content,
        "This is the first native ODF presentation slide.\nScan\n"
    );
}

#[test]
fn odf_text_preserves_mixed_content_and_explicit_language() {
    let r = inspect(
        &fixture("notes.odt"),
        &InspectOptions {
            packet: true,
            ..Default::default()
        },
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.format.bucket, "Office Docs");
    assert_eq!(r.format.basis, "bytes");
    assert_eq!(
        r.text.content,
        "Café et preuves ici.\tTexte  natif.\nScan\n"
    );
    assert_eq!(r.counts.paragraphs, Some(2));
    assert_eq!(r.preflight.len(), 2);
    assert!(!r.preflight[0].needs_ocr);
    assert!(r.preflight[1].needs_ocr);
    assert_eq!(r.packet.unwrap()["Language"], "French");
}

/// Verifies the ODT table unit keeps exact joined separators and clips early.
///
/// Accepts no parameters. Returns unit after two canonical extractions of the
/// tables fixture. A table unit that loses separators, a clipped unit that is
/// not the exact prefix of the whole text, or a missing partial disclosure
/// panics.
#[test]
fn odt_table_unit_keeps_separators_and_clips_early_under_character_caps() {
    use glassbox_inspect::reports::{
        Completeness, ExtractOptions, NativeSelection, OcrOptions, OmissionStatus, OperationStatus,
    };
    let options = |max_chars: usize| ExtractOptions {
        selection: NativeSelection::All,
        max_chars,
        ocr: OcrOptions {
            enabled: false,
            ..OcrOptions::default()
        },
        ..ExtractOptions::default()
    };

    // Full request: the atomic table unit carries the exact joined text with
    // row, cell, and paragraph separators, including nested-table rows.
    let full = glassbox_inspect::extract(&fixture("tables.odt"), &options(usize::MAX));
    assert_eq!(full.metadata.status, OperationStatus::Complete);
    assert_eq!(full.content.units.len(), 2);
    let table_text = full.content.units[1].text.as_deref().unwrap();
    assert!(table_text.starts_with("Name\tAnalysis\tStatus"));
    assert!(table_text.contains('\n'), "{table_text}");
    assert!(table_text.contains("Nested evidence."), "{table_text}");
    assert_eq!(full.content.units[1].completeness, Completeness::Exact);

    // Small character cap: the first paragraph consumes 23 characters, the
    // table unit retains exactly the next 17 characters of the same text —
    // the early-clipped buffer equals the prefix the full text would clip to,
    // and the unit is disclosed as partial instead of silently shortened.
    let clipped = glassbox_inspect::extract(&fixture("tables.odt"), &options(40));
    assert_eq!(clipped.metadata.status, OperationStatus::Partial);
    assert_eq!(clipped.selection.omitted, OmissionStatus::SafetyLimit);
    assert_eq!(
        clipped.content.units[0].text.as_deref(),
        Some("A structured ODF table.")
    );
    let expected_prefix: String = table_text.chars().take(17).collect();
    assert_eq!(
        clipped.content.units[1].text.as_deref(),
        Some(expected_prefix.as_str()),
        "clipped unit must equal the exact prefix of the whole table text"
    );
    assert_eq!(clipped.content.units[1].completeness, Completeness::Partial);
    assert!(
        clipped
            .metadata
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "content_limit")
    );
}

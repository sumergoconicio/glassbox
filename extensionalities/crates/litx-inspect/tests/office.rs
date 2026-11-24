//! Module: glassbox-inspect integration tests.
//! File: office.rs.
//! Responsibility: Exercise office document integration behavior, including DOCX tables and explicit page breaks.
use glassbox_inspect::reports::{
    Completeness, ExtractOptions, NativeSelection, NativeUnit, OcrOptions, OperationStatus,
};
use glassbox_inspect::{InspectOptions, extract, extract_transitional as inspect};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

#[test]
fn pptx_follows_relationship_order_and_limits_only_preflight() {
    let options = InspectOptions {
        max_units: 1,
        max_chars: 9,
        ..Default::default()
    };
    let r = inspect(&fixture("deck.pptx"), &options);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.counts.slides, Some(2));
    assert_eq!(r.text.content, "This is t");
    assert!(r.text.truncated);
    assert_eq!(
        r.counts.chars,
        "This is the first slide in presentation order.\nScan\n"
            .chars()
            .count()
    );
    assert_eq!(r.preflight.len(), 1);
    assert!(!r.preflight[0].needs_ocr);
    assert_eq!(r.preflight[0].unit_kind, "slide");
}

#[test]
fn docx_native_text_metadata_sections_and_provenance() {
    let options = InspectOptions {
        packet: true,
        ..Default::default()
    };
    let r = inspect(&fixture("notes.docx"), &options);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.format.bucket, "Office Docs");
    assert_eq!(r.format.basis, "bytes");
    assert!(!r.format.disagreement);
    assert!(r.claims.iter().any(|c| c.key == "author"
        && c.value == "Ada Example"
        && c.source == "container_metadata"));
    assert!(
        r.claims
            .iter()
            .any(|c| c.key == "language" && c.value == "en-GB")
    );
    assert_eq!(r.counts.paragraphs, Some(3));
    assert_eq!(r.counts.pages, None); // app.xml's unverified Pages=999 is only a claim.
    assert_eq!(
        r.text.content,
        "This is a native text layer with an ampersand & evidence.\nSection one ends here.\nScan\n"
    );
    assert_eq!(r.preflight.len(), 2);
    assert!(!r.preflight[0].needs_ocr);
    assert!(r.preflight[1].needs_ocr);
    assert_eq!(r.preflight[1].unit_kind, "section");
    assert_eq!(r.preflight[1].confidence, "deterministic");
    assert_eq!(r.packet.unwrap()["Language"], "English");
}

/// Ordinary deck text keeps presentation order and renders each DrawingML
/// table-cell paragraph exactly once, with merged empty cells as empty lines.
#[test]
fn pptx_table_cell_paragraphs_are_emitted_once_in_order() {
    let r = inspect(&fixture("tables.pptx"), &InspectOptions::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.counts.slides, Some(2));
    assert_eq!(
        r.text.content,
        "Wide\n\nRight\nTall\nB\nC\n\nBottom\n\nScan\n"
    );
    assert_eq!(r.counts.chars, 35);
    assert!(!r.text.truncated);
    assert_eq!(r.preflight.len(), 2);
    assert!(!r.preflight[0].needs_ocr, "table slide has 20+ chars");
    assert!(r.preflight[1].needs_ocr, "Scan is short and has images");
}

/// A full native selection of an ordinary document emits each body block unit
/// once, in order, with exact completeness.
#[test]
fn docx_native_selection_all_emits_blocks_once_in_order() {
    let options = ExtractOptions {
        selection: NativeSelection::All,
        ocr: OcrOptions {
            enabled: false,
            ..OcrOptions::default()
        },
        ..ExtractOptions::default()
    };
    let r = extract(&fixture("notes.docx"), &options);
    assert_eq!(r.metadata.status, OperationStatus::Complete);
    let texts: Vec<_> = r.content.units.iter().map(|u| u.text.as_deref()).collect();
    assert_eq!(
        texts,
        [
            Some("This is a native text layer with an ampersand & evidence."),
            Some("Section one ends here."),
            Some("Scan"),
        ]
    );
    assert!(
        r.content
            .units
            .iter()
            .all(|u| u.completeness == Completeness::Exact)
    );
}

/// Extracting `tables.docx` tags top-level `<w:tbl>` blocks as `NativeUnit::Table`.
#[test]
fn docx_native_selection_tags_tables_as_table_units() {
    let options = ExtractOptions {
        selection: NativeSelection::All,
        ocr: OcrOptions {
            enabled: false,
            ..OcrOptions::default()
        },
        ..ExtractOptions::default()
    };
    let report = extract(&fixture("tables.docx"), &options);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert!(
        report
            .content
            .units
            .iter()
            .any(|unit| unit.unit == NativeUnit::Table),
        "expected at least one NativeUnit::Table in tables.docx: {:#?}",
        report.content.units
    );
}

/// Extracting a DOCX whose paragraphs and tables live inside `<w:sdt>/<w:sdtContent>`
/// and `<w:ins>` wrappers emits them in exact source order while excluding `<w:del>`.
#[test]
fn docx_native_selection_traverses_content_controls_and_insertions() {
    use std::io::Write;

    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "office-sdt-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("content_controls.docx");

    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let zip_options = zip::write::SimpleFileOptions::default();
    zip.start_file("[Content_Types].xml", zip_options).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
    )
    .unwrap();
    zip.start_file("word/document.xml", zip_options).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:sdt>
      <w:sdtPr><w:alias w:val="HeaderControl"/></w:sdtPr>
      <w:sdtContent>
        <w:p><w:r><w:t>Control paragraph</w:t></w:r><w:ins><w:r><w:t> with insertion</w:t></w:r></w:ins><w:del><w:r><w:t> deleted text</w:t></w:r></w:del></w:p>
        <w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Cell B</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
      </w:sdtContent>
    </w:sdt>
    <w:ins><w:p><w:r><w:t>Inserted block</w:t></w:r></w:p></w:ins>
  </w:body>
</w:document>"#,
    )
    .unwrap();
    zip.finish().unwrap();

    let options = ExtractOptions {
        selection: NativeSelection::All,
        ocr: OcrOptions {
            enabled: false,
            ..OcrOptions::default()
        },
        ..ExtractOptions::default()
    };
    let report = extract(&path, &options);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(report.completeness, Completeness::Exact);
    let summary: Vec<_> = report
        .content
        .units
        .iter()
        .map(|u| (u.unit.clone(), u.text.as_deref()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                NativeUnit::DocumentBlock,
                Some("Control paragraph with insertion"),
            ),
            (NativeUnit::Table, Some("Cell A\tCell B")),
            (NativeUnit::DocumentBlock, Some("Inserted block")),
        ]
    );
}

/// Verifies that a DOCX paragraph with `<w:outlineLvl w:val="usize::MAX"/>`
/// renders cleanly as an H6 heading without an integer-overflow panic on the
/// transitional Markdown path (`T02-SYS-001`).
///
/// Accepts no parameters and returns unit. Panics if creating the temporary
/// synthetic DOCX fails, if transitional inspection panics or reports errors,
/// or if the rendered Markdown omits `"###### Overflow heading"`.
#[test]
fn docx_outline_level_usize_max_clamps_without_overflow_panic() {
    use std::io::Write;

    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "office-outline-lvl-max-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("outline_lvl_max.docx");

    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let zip_options = zip::write::SimpleFileOptions::default();
    zip.start_file("[Content_Types].xml", zip_options).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
    )
    .unwrap();
    zip.start_file("word/document.xml", zip_options).unwrap();
    let document_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:pPr><w:outlineLvl w:val="{}"/></w:pPr>
      <w:r><w:t>Overflow heading</w:t></w:r>
    </w:p>
  </w:body>
</w:document>"#,
        usize::MAX
    );
    zip.write_all(document_xml.as_bytes()).unwrap();
    zip.finish().unwrap();

    let options = InspectOptions {
        native_selection: None,
        ..InspectOptions::default()
    };
    let report = inspect(&path, &options);
    let _ = std::fs::remove_dir_all(&directory);

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        report.markdown.content.contains("###### Overflow heading"),
        "expected clamped H6 heading in markdown, got: {:?}",
        report.markdown.content
    );
}

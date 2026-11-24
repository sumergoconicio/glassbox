//! Integration tests for end-to-end `bodymatter:v1` Markdown structure markers
//! (`V-IO-05`, `V-IO-06`).
//!
//! Exercises PDF `/PageLabels` extraction, multi-sheet XLSX (including hostile
//! sheet names), CSV tabular grouping, PPTX slides, DOCX tables and explicit
//! `<w:br w:type="page"/>` page breaks (ignoring `<w:lastRenderedPageBreak/>`),
//! EPUB spine chapters, anti-forgery `<!--` neutralization in real extracted
//! documents, XML round-trip verification through `quick_xml::Reader`, and
//! `render_markdown_with_options(report, false)` marker-stripping parity.

use glassbox_inspect::bodymatter::BODYMATTER_PREFIX;
use glassbox_inspect::extract;
use glassbox_inspect::reports::{
    ExtractOptions, ExtractReport, NativeRange, NativeSelection, OcrOptions, OperationStatus,
};
use glassbox_inspect::selection::{render_markdown, render_markdown_with_options};
use quick_xml::{Reader, XmlVersion, events::Event};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Returns the checked-in fixture path for `name`.
///
/// Accepts a fixture filename under `extensionalities/fixtures/`. Returns its
/// absolute path. Cannot fail.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Creates a unique test-owned scratch directory inside `extensionalities/target/tests/`.
///
/// Accepts a short tag identifying the test case. Returns the created directory
/// path. Panics on filesystem errors.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/extract-markers")
        .join(format!(
            "{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes UTF-8 entries into a stored ZIP archive fixture.
///
/// Accepts a target directory, archive filename, and `(entry_name, xml_body)`
/// pairs. Returns the written file path. Panics on I/O or ZIP errors.
fn write_zip_fixture(dir: &Path, name: &str, entries: &[(&str, &str)]) -> PathBuf {
    let path = dir.join(name);
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for (entry_name, body) in entries {
        zip.start_file(
            *entry_name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

/// Returns deterministic extraction options with OCR disabled.
///
/// Accepts a native unit selection. Returns [`ExtractOptions`] with OCR off so
/// tests remain offline and deterministic. Cannot fail.
fn extract_options(selection: NativeSelection) -> ExtractOptions {
    ExtractOptions {
        selection,
        ocr: OcrOptions {
            enabled: false,
            ..OcrOptions::default()
        },
        ..ExtractOptions::default()
    }
}

/// Builds a valid synthetic PDF with optional `/PageLabels` tree and per-page text.
///
/// Accepts per-page ASCII text lines and an optional raw `/PageLabels` PDF
/// dictionary string (for example `"<< /Nums [ 0 << /S /r /St 4 >> 1 << /S /D /St 1 >> ] >>"`).
/// Returns valid PDF 1.7 bytes with exact xref offsets. Cannot fail.
fn build_labeled_pdf(pages: &[&str], page_labels_dict: Option<&str>) -> Vec<u8> {
    let catalog = match page_labels_dict {
        Some(labels) => format!("<< /Type /Catalog /Pages 2 0 R /PageLabels {labels} >>"),
        None => "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
    };
    let mut objects: Vec<Vec<u8>> = vec![
        catalog.into_bytes(),
        Vec::new(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut kids = Vec::new();
    for text in pages {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        let content = format!("BT /F1 16 Tf 50 700 Td ({escaped}) Tj ET").into_bytes();
        objects.push(format!("<< /Length {} >>\nstream\n", content.len()).into_bytes());
        let content_obj = objects.len();
        objects[content_obj - 1].extend_from_slice(&content);
        objects[content_obj - 1].extend_from_slice(b"\nendstream");
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << /F1 3 0 R >> >> /Contents {content_obj} 0 R >>"
            )
            .into_bytes(),
        );
        kids.push(objects.len());
    }
    objects[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.iter()
            .map(|n| format!("{n} 0 R"))
            .collect::<Vec<_>>()
            .join(" "),
        kids.len()
    )
    .into_bytes();

    let mut data = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = vec![0usize];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(data.len());
        data.extend(format!("{} 0 obj\n", index + 1).into_bytes());
        data.extend(object);
        data.extend(b"\nendobj\n");
    }
    let xref = data.len();
    data.extend(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).into_bytes());
    for offset in &offsets[1..] {
        data.extend(format!("{offset:010} 00000 n \n").into_bytes());
    }
    data.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len()
        )
        .into_bytes(),
    );
    data
}

/// Extracts the inner XML element from one line matching `^<!-- bodymatter:v1 (<.*/>) -->$`.
///
/// Accepts a single line. Returns `Some(xml_element)` when the line matches the
/// `bodymatter:v1` consumer recipe, else `None`. Cannot fail.
fn match_marker_line(line: &str) -> Option<&str> {
    let inner = line
        .strip_prefix("<!-- ")?
        .strip_prefix(BODYMATTER_PREFIX)?
        .strip_prefix(' ')?
        .strip_suffix(" -->")?;
    (inner.starts_with('<') && inner.ends_with("/>")).then_some(inner)
}

/// Parses every `bodymatter:v1` marker line in `markdown` via `quick_xml::Reader`.
///
/// Accepts rendered Markdown output. Returns a vector of `(tag_name, attributes)`
/// with XML-decoded attribute values for every matched marker line. Panics if
/// any marker line contains `--` inside its HTML comment payload or fails
/// strict XML parsing.
fn parse_all_markers(markdown: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut parsed = Vec::new();
    for line in markdown.lines() {
        let Some(xml) = match_marker_line(line) else {
            continue;
        };
        let comment_body = line
            .strip_prefix("<!-- ")
            .and_then(|rest| rest.strip_suffix(" -->"))
            .unwrap();
        assert!(
            !comment_body.contains("--"),
            "marker comment payload must never contain `--`: {line:?}"
        );
        let mut reader = Reader::from_str(xml);
        let event = reader
            .read_event()
            .unwrap_or_else(|error| panic!("quick_xml failed on {xml:?}: {error}"));
        let Event::Empty(element) = event else {
            panic!("expected self-closing marker element in {xml:?}, got {event:?}");
        };
        let tag = element.name().as_ref().to_owned();
        let mut attrs = Vec::new();
        for attr in element.attributes() {
            let attr = attr.unwrap_or_else(|error| panic!("invalid attribute in {xml:?}: {error}"));
            let key = attr.key.as_ref().to_owned();
            let value = attr
                .normalized_value(XmlVersion::default())
                .unwrap_or_else(|error| panic!("failed decoding attr `{key}` in {xml:?}: {error}"))
                .into_owned();
            attrs.push((key, value));
        }
        assert!(matches!(reader.read_event().unwrap(), Event::Eof));
        parsed.push((tag, attrs));
    }
    parsed
}

/// Verifies that stripping marker lines from `render_markdown(report)` yields
/// identical content blocks to `render_markdown_with_options(report, false)`
/// and that neither contains synthetic legacy headings or preamble.
///
/// Accepts an [`ExtractReport`]. Panics if marker stripping and `--no-markers`
/// diverge or if legacy preamble/headings appear.
fn assert_no_markers_parity(report: &ExtractReport) {
    let with_markers = render_markdown(report);
    let without_markers = render_markdown_with_options(report, false);
    assert!(
        !with_markers.contains("# Glassbox extraction"),
        "synthetic title must not appear: {with_markers}"
    );
    assert!(
        !with_markers.contains("## Unit "),
        "synthetic unit headings must not appear: {with_markers}"
    );
    assert!(
        parse_all_markers(&without_markers).is_empty()
            && !without_markers.contains("<!-- bodymatter:v1"),
        "--no-markers output must contain zero bodymatter marker comments: {without_markers}"
    );

    let stripped_blocks: Vec<&str> = with_markers
        .trim_end_matches('\n')
        .split("\n\n")
        .filter(|block| !block.is_empty() && match_marker_line(block).is_none())
        .collect();
    let no_marker_blocks: Vec<&str> = if without_markers.is_empty() {
        Vec::new()
    } else {
        without_markers
            .trim_end_matches('\n')
            .split("\n\n")
            .collect()
    };
    assert_eq!(
        stripped_blocks, no_marker_blocks,
        "stripping marker lines from default output must match --no-markers"
    );
}

/// Verifies PDF `/PageLabels` (`iv`, `1`, `2`), partial `--pages 2` coordinate
/// preservation, `--max-chars` `completeness="partial"`, and unlabeled PDFs.
#[test]
fn pdf_markers_preserve_page_labels_physical_numbers_and_partial_flag() {
    let dir = scratch("pdf-labels");
    let labeled_path = dir.join("labeled.pdf");
    std::fs::write(
        &labeled_path,
        build_labeled_pdf(
            &[
                "Preface Roman page four",
                "Chapter one decimal page one",
                "Chapter one decimal page two",
            ],
            Some("<< /Nums [ 0 << /S /r /St 4 >> 1 << /S /D /St 1 >> ] >>"),
        ),
    )
    .unwrap();

    let full = extract(&labeled_path, &extract_options(NativeSelection::All));
    assert_eq!(full.metadata.status, OperationStatus::Complete);
    let md = render_markdown(&full);
    let markers = parse_all_markers(&md);
    assert_eq!(
        markers,
        vec![
            (
                "page".into(),
                vec![
                    ("number".into(), "1".into()),
                    ("label".into(), "iv".into()),
                    ("origin".into(), "native".into()),
                ],
            ),
            (
                "page".into(),
                vec![
                    ("number".into(), "2".into()),
                    ("label".into(), "1".into()),
                    ("origin".into(), "native".into()),
                ],
            ),
            (
                "page".into(),
                vec![
                    ("number".into(), "3".into()),
                    ("label".into(), "2".into()),
                    ("origin".into(), "native".into()),
                ],
            ),
        ]
    );
    assert!(
        md.contains("<!-- bodymatter:v1 <page number=\"1\" label=\"iv\" origin=\"native\"/> -->")
    );
    assert!(
        md.contains("<!-- bodymatter:v1 <page number=\"2\" label=\"1\" origin=\"native\"/> -->")
    );
    assert!(md.contains("Preface Roman page four"));
    assert_no_markers_parity(&full);

    // Partial selection `--pages 2` preserves true physical page number 2 and label "1".
    let page_two = extract(
        &labeled_path,
        &extract_options(NativeSelection::Ranges {
            ranges: vec![NativeRange { start: 2, end: 2 }],
        }),
    );
    assert_eq!(page_two.metadata.status, OperationStatus::Complete);
    let page_two_md = render_markdown(&page_two);
    assert_eq!(
        parse_all_markers(&page_two_md),
        vec![(
            "page".into(),
            vec![
                ("number".into(), "2".into()),
                ("label".into(), "1".into()),
                ("origin".into(), "native".into()),
            ],
        )]
    );
    assert!(page_two_md.contains("Chapter one decimal page one"));
    assert_no_markers_parity(&page_two);

    // Truncation via `--max-chars` emits `completeness="partial"` on truncated page markers.
    let mut truncated_opts = extract_options(NativeSelection::Ranges {
        ranges: vec![NativeRange { start: 2, end: 2 }],
    });
    truncated_opts.max_chars = 7;
    let truncated = extract(&labeled_path, &truncated_opts);
    let truncated_md = render_markdown(&truncated);
    assert_eq!(
        parse_all_markers(&truncated_md),
        vec![(
            "page".into(),
            vec![
                ("number".into(), "2".into()),
                ("label".into(), "1".into()),
                ("origin".into(), "native".into()),
                ("completeness".into(), "partial".into()),
            ],
        )]
    );
    assert_no_markers_parity(&truncated);

    // Unlabeled PDF omits the `label` attribute cleanly.
    let unlabeled_path = dir.join("unlabeled.pdf");
    std::fs::write(&unlabeled_path, build_labeled_pdf(&["Plain page"], None)).unwrap();
    let unlabeled = extract(&unlabeled_path, &extract_options(NativeSelection::All));
    assert_eq!(
        parse_all_markers(&render_markdown(&unlabeled)),
        vec![(
            "page".into(),
            vec![
                ("number".into(), "1".into()),
                ("origin".into(), "native".into()),
            ],
        )]
    );
    assert_no_markers_parity(&unlabeled);
}

/// Verifies multi-sheet XLSX emits `<sheet number="N" name="..."/>`, valid GFM
/// pipe tables without duplicate TSV dumps, and round-trips hostile sheet names
/// containing `--` and `<>&"` through `quick_xml::Reader`.
#[test]
fn xlsx_markers_render_gfm_tables_without_tsv_and_round_trip_hostile_sheet_names() {
    const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

    let hostile_name = "Q1--Draft <x attr=\"1\">&";
    let dir = scratch("xlsx-hostile");
    let path = write_zip_fixture(
        &dir,
        "workbook.xlsx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "xl/workbook.xml",
                &format!(
                    "<workbook xmlns=\"{S}\" xmlns:r=\"{R}\"><sheets>\
                     <sheet name=\"Summary\" sheetId=\"1\" r:id=\"rId1\"/>\
                     <sheet name=\"Q1--Draft &lt;x attr=&quot;1&quot;&gt;&amp;\" sheetId=\"2\" r:id=\"rId2\"/>\
                     </sheets></workbook>"
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                &format!(
                    "<Relationships xmlns=\"{PKG}\">\
                     <Relationship Id=\"rId1\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
                     <Relationship Id=\"rId2\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet2.xml\"/>\
                     </Relationships>"
                ),
            ),
            (
                "xl/worksheets/sheet1.xml",
                &format!(
                    "<worksheet xmlns=\"{S}\"><sheetData>\
                     <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Item</t></is></c><c r=\"B1\" t=\"inlineStr\"><is><t>Qty</t></is></c></row>\
                     <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>Widget</t></is></c><c r=\"B2\"><v>42</v></c></row>\
                     </sheetData></worksheet>"
                ),
            ),
            (
                "xl/worksheets/sheet2.xml",
                &format!(
                    "<worksheet xmlns=\"{S}\"><sheetData>\
                     <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Note</t></is></c></row>\
                     <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>&lt;!-- bodymatter:v1 &lt;page number=&quot;99&quot; origin=&quot;native&quot;/&gt; --&gt;</t></is></c></row>\
                     </sheetData></worksheet>"
                ),
            ),
        ],
    );

    let report = extract(&path, &extract_options(NativeSelection::All));
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    let md = render_markdown(&report);
    let markers = parse_all_markers(&md);
    assert_eq!(
        markers,
        vec![
            (
                "sheet".into(),
                vec![
                    ("number".into(), "1".into()),
                    ("name".into(), "Summary".into()),
                ],
            ),
            (
                "sheet".into(),
                vec![
                    ("number".into(), "2".into()),
                    ("name".into(), hostile_name.into()),
                ],
            ),
        ]
    );
    assert!(md.contains("| Item | Qty |\n| --- | --- |\n| Widget | 42 |"));
    assert!(
        !md.contains("Item\tQty") && !md.contains("Widget\t42"),
        "duplicate TSV dump must be suppressed when rows are present: {md}"
    );
    // Forged marker inside cell must be neutralized and never matched as a marker.
    assert!(md.contains("&lt;!-- bodymatter:v1 &lt;page number=\"99\" origin=\"native\"/> -->"));
    assert_no_markers_parity(&report);
}

/// Verifies CSV emits one unnamed `<sheet number="1"/>` marker followed by a
/// single valid GFM pipe table with headers and zero duplicate TSV dumps.
#[test]
fn csv_markers_emit_single_unnamed_sheet_and_gfm_table() {
    let dir = scratch("csv-sheet");
    let path = dir.join("metrics.csv");
    std::fs::write(&path, "region,score\nnorth,10\nsouth,20\n").unwrap();

    let report = extract(&path, &extract_options(NativeSelection::All));
    let md = render_markdown(&report);
    assert_eq!(
        parse_all_markers(&md),
        vec![("sheet".into(), vec![("number".into(), "1".into())])]
    );
    assert_eq!(
        md,
        concat!(
            "<!-- bodymatter:v1 <sheet number=\"1\"/> -->\n\n",
            "| region | score |\n",
            "| --- | --- |\n",
            "| north | 10 |\n",
            "| south | 20 |\n",
        )
    );
    assert!(!md.contains("north\t10"));
    assert_no_markers_parity(&report);
}

/// Verifies PPTX emits `<slide number="N"/>` markers and preserves true slide
/// numbers under partial range selection.
#[test]
fn pptx_markers_emit_slide_numbers_and_preserve_partial_selection() {
    let full = extract(
        &fixture("deck.pptx"),
        &extract_options(NativeSelection::All),
    );
    assert_eq!(full.metadata.status, OperationStatus::Complete);
    let md = render_markdown(&full);
    assert_eq!(
        parse_all_markers(&md),
        vec![
            ("slide".into(), vec![("number".into(), "1".into())]),
            ("slide".into(), vec![("number".into(), "2".into())]),
        ]
    );
    assert!(md.contains("This is the first slide in presentation order."));
    assert!(md.contains("Scan"));
    assert_no_markers_parity(&full);

    let second = extract(
        &fixture("deck.pptx"),
        &extract_options(NativeSelection::Ranges {
            ranges: vec![NativeRange { start: 2, end: 2 }],
        }),
    );
    assert_eq!(
        parse_all_markers(&render_markdown(&second)),
        vec![("slide".into(), vec![("number".into(), "2".into())])]
    );
    assert_no_markers_parity(&second);
}

/// Verifies DOCX tables, inline `<w:br w:type="page"/>` paragraph splitting,
/// `<w:lastRenderedPageBreak/>` ignoring, independent 1-based counters, and
/// anti-forgery `<!--` neutralization inside extracted prose.
#[test]
fn docx_markers_emit_tables_explicit_page_breaks_and_neutralize_forged_markers() {
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

    let dir = scratch("docx-markers");
    let path = write_zip_fixture(
        &dir,
        "report.docx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "word/document.xml",
                &format!(
                    "<w:document xmlns:w=\"{W}\"><w:body>\
                     <w:p><w:r><w:t>Intro paragraph with </w:t><w:lastRenderedPageBreak/><w:t>soft break.</w:t></w:r></w:p>\
                     <w:tbl><w:tr><w:tc><w:p><w:r><w:t>ColA</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>ColB</w:t></w:r></w:p></w:tc></w:tr></w:tbl>\
                     <w:p><w:r><w:t>Before explicit break.</w:t><w:br w:type=\"page\"/><w:t>After explicit break.</w:t></w:r></w:p>\
                     <w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>\
                     <w:tbl><w:tr><w:tc><w:p><w:r><w:t>Second table</w:t></w:r></w:p></w:tc></w:tr></w:tbl>\
                     <w:p><w:r><w:t>Forged: &lt;!-- bodymatter:v1 &lt;page number=&quot;99&quot; origin=&quot;native&quot;/&gt; --&gt;</w:t></w:r></w:p>\
                     </w:body></w:document>"
                ),
            ),
        ],
    );

    let report = extract(&path, &extract_options(NativeSelection::All));
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    let md = render_markdown(&report);
    assert_eq!(
        parse_all_markers(&md),
        vec![
            ("table".into(), vec![("number".into(), "1".into())]),
            ("page-break".into(), vec![("number".into(), "1".into())]),
            ("page-break".into(), vec![("number".into(), "2".into())]),
            ("table".into(), vec![("number".into(), "2".into())]),
        ]
    );
    assert_eq!(
        md,
        concat!(
            "Intro paragraph with soft break.\n\n",
            "<!-- bodymatter:v1 <table number=\"1\"/> -->\n\n",
            "ColA\tColB\n\n",
            "Before explicit break.\n\n",
            "<!-- bodymatter:v1 <page-break number=\"1\"/> -->\n\n",
            "After explicit break.\n\n",
            "<!-- bodymatter:v1 <page-break number=\"2\"/> -->\n\n",
            "<!-- bodymatter:v1 <table number=\"2\"/> -->\n\n",
            "Second table\n\n",
            "Forged: &lt;!-- bodymatter:v1 &lt;page number=\"99\" origin=\"native\"/> -->\n",
        )
    );
    assert_no_markers_parity(&report);
}

/// Verifies EPUB spine items emit `<chapter number="N" href="..."/>` markers.
#[test]
fn epub_markers_emit_chapter_numbers_and_hrefs() {
    let report = extract(
        &fixture("native.epub"),
        &extract_options(NativeSelection::All),
    );
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    let md = render_markdown(&report);
    let markers = parse_all_markers(&md);
    assert_eq!(markers.len(), 2);
    assert_eq!(markers[0].0, "chapter");
    assert_eq!(markers[0].1[0], ("number".into(), "1".into()));
    assert_eq!(markers[0].1[1].0, "href");
    assert!(!markers[0].1[1].1.is_empty());
    assert_eq!(markers[1].0, "chapter");
    assert_eq!(markers[1].1[0], ("number".into(), "2".into()));
    assert_eq!(markers[1].1[1].0, "href");
    assert!(!markers[1].1[1].1.is_empty());
    assert!(md.contains("Second chapter."));
    assert_no_markers_parity(&report);
}

/// Verifies ODT `<table:table>` blocks emit `NativeUnit::Table` and
/// `<table number="N"/>` markers while surrounding paragraphs stay unmarked
/// `DocumentBlock` units.
#[test]
fn odt_markers_emit_table_numbers_and_keep_paragraphs_unmarked() {
    use glassbox_inspect::reports::NativeUnit;

    let report = extract(
        &fixture("tables.odt"),
        &extract_options(NativeSelection::All),
    );
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(report.content.units.len(), 2);
    assert_eq!(report.content.units[0].unit, NativeUnit::DocumentBlock);
    assert_eq!(report.content.units[1].unit, NativeUnit::Table);
    let md = render_markdown(&report);
    assert_eq!(
        parse_all_markers(&md),
        vec![("table".into(), vec![("number".into(), "1".into())])]
    );
    assert!(md.contains("A structured ODF table.\n\n<!-- bodymatter:v1 <table number=\"1\"/> -->\n\nName\tAnalysis\tStatus"));
    assert_no_markers_parity(&report);

    // Multi-table ODT verifies sequential 1-based table numbering alongside unmarked prose.
    const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
    const T: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
    const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
    let dir = scratch("odt-tables");
    let path = write_zip_fixture(
        &dir,
        "multi.odt",
        &[
            ("mimetype", "application/vnd.oasis.opendocument.text"),
            (
                "content.xml",
                &format!(
                    "<office:document-content xmlns:office=\"{O}\" xmlns:text=\"{T}\" xmlns:table=\"{TABLE}\">\
                     <office:body><office:text>\
                     <text:p>Opening paragraph.</text:p>\
                     <table:table table:name=\"T1\"><table:table-row><table:table-cell><text:p>A1</text:p></table:table-cell><table:table-cell><text:p>B1</text:p></table:table-cell></table:table-row></table:table>\
                     <text:p>Middle paragraph.</text:p>\
                     <table:table table:name=\"T2\"><table:table-row><table:table-cell><text:p>C1</text:p></table:table-cell></table:table-row></table:table>\
                     </office:text></office:body></office:document-content>"
                ),
            ),
        ],
    );
    let multi = extract(&path, &extract_options(NativeSelection::All));
    assert_eq!(multi.metadata.status, OperationStatus::Complete);
    let multi_md = render_markdown(&multi);
    assert_eq!(
        parse_all_markers(&multi_md),
        vec![
            ("table".into(), vec![("number".into(), "1".into())]),
            ("table".into(), vec![("number".into(), "2".into())]),
        ]
    );
    assert_eq!(
        multi_md,
        concat!(
            "Opening paragraph.\n\n",
            "<!-- bodymatter:v1 <table number=\"1\"/> -->\n\n",
            "A1\tB1\n\n",
            "Middle paragraph.\n\n",
            "<!-- bodymatter:v1 <table number=\"2\"/> -->\n\n",
            "C1\n",
        )
    );
    assert_no_markers_parity(&multi);
}

/// Verifies worksheet extraction preserves genuine first-row cells (including
/// labels beyond 60 bytes and numeric/formula values) in both JSON and Markdown,
/// and surfaces partial status when a first-row cell exceeds the preview cap
/// (GB-AUD-01).
#[test]
fn worksheet_extraction_preserves_genuine_first_row_cells_and_surfaces_clipping() {
    use glassbox_inspect::reports::Completeness;

    const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

    let long_header =
        "This genuine source header goes well beyond sixty bytes and must remain intact";
    assert!(long_header.len() > 60);

    let dir = scratch("xlsx-first-row");
    let path = write_zip_fixture(
        &dir,
        "genuine-headers.xlsx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "xl/workbook.xml",
                &format!(
                    "<workbook xmlns=\"{S}\" xmlns:r=\"{R}\"><sheets>\
                     <sheet name=\"Summary\" sheetId=\"1\" r:id=\"rId1\"/>\
                     </sheets></workbook>"
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                &format!(
                    "<Relationships xmlns=\"{PKG}\">\
                     <Relationship Id=\"rId1\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
                     </Relationships>"
                ),
            ),
            (
                "xl/worksheets/sheet1.xml",
                &format!(
                    "<worksheet xmlns=\"{S}\"><sheetData>\
                     <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>{long_header}</t></is></c><c r=\"B1\"><v>2026</v></c></row>\
                     <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>Widget</t></is></c><c r=\"B2\"><v>42</v></c></row>\
                     </sheetData></worksheet>"
                ),
            ),
        ],
    );

    let report = extract(&path, &extract_options(NativeSelection::All));
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(report.content.units.len(), 1);
    let unit = &report.content.units[0];
    assert_eq!(unit.completeness, Completeness::Exact);
    assert_eq!(unit.rows.len(), 2);
    assert_eq!(unit.rows[0].row_index, 1);
    assert_eq!(unit.rows[0].values, vec![long_header, "2026"]);
    assert_eq!(unit.rows[1].row_index, 2);
    assert_eq!(unit.rows[1].values, vec!["Widget", "42"]);

    let md = render_markdown(&report);
    assert_eq!(
        md,
        format!(
            "<!-- bodymatter:v1 <sheet number=\"1\" name=\"Summary\"/> -->\n\n\
             | {long_header} | 2026 |\n\
             | --- | --- |\n\
             | Widget | 42 |\n"
        )
    );
    assert_no_markers_parity(&report);

    // An oversized (>512-char) first-row cell must downgrade completeness and status to partial.
    let huge_header = "H".repeat(600);
    let clipped_path = write_zip_fixture(
        &dir,
        "clipped-header.xlsx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "xl/workbook.xml",
                &format!(
                    "<workbook xmlns=\"{S}\" xmlns:r=\"{R}\"><sheets>\
                     <sheet name=\"Clipped\" sheetId=\"1\" r:id=\"rId1\"/>\
                     </sheets></workbook>"
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                &format!(
                    "<Relationships xmlns=\"{PKG}\">\
                     <Relationship Id=\"rId1\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
                     </Relationships>"
                ),
            ),
            (
                "xl/worksheets/sheet1.xml",
                &format!(
                    "<worksheet xmlns=\"{S}\"><sheetData>\
                     <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>{huge_header}</t></is></c></row>\
                     <row r=\"2\"><c r=\"A2\"><v>1</v></c></row>\
                     </sheetData></worksheet>"
                ),
            ),
        ],
    );
    let clipped = extract(&clipped_path, &extract_options(NativeSelection::All));
    assert_eq!(clipped.status, OperationStatus::Partial);
    assert_eq!(clipped.metadata.status, OperationStatus::Complete);
    assert_eq!(clipped.content.units[0].completeness, Completeness::Partial);
    assert_eq!(
        clipped.content.units[0].rows[0].values[0].chars().count(),
        512
    );

    // Verify ODS preserves genuine row-1 long labels and numeric cells identically.
    const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
    const T: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
    const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
    let ods_path = write_zip_fixture(
        &dir,
        "genuine-headers.ods",
        &[
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            (
                "content.xml",
                &format!(
                    "<office:document-content xmlns:office=\"{O}\" xmlns:text=\"{T}\" xmlns:table=\"{TABLE}\">\
                     <office:body><office:spreadsheet>\
                     <table:table table:name=\"Summary\">\
                     <table:table-row><table:table-cell office:value-type=\"string\"><text:p>{long_header}</text:p></table:table-cell><table:table-cell office:value-type=\"float\" office:value=\"2026\"><text:p>2026</text:p></table:table-cell></table:table-row>\
                     <table:table-row><table:table-cell office:value-type=\"string\"><text:p>Widget</text:p></table:table-cell><table:table-cell office:value-type=\"float\" office:value=\"42\"><text:p>42</text:p></table:table-cell></table:table-row>\
                     </table:table>\
                     </office:spreadsheet></office:body></office:document-content>"
                ),
            ),
        ],
    );
    let ods = extract(&ods_path, &extract_options(NativeSelection::All));
    assert_eq!(ods.metadata.status, OperationStatus::Complete);
    assert_eq!(ods.content.units[0].completeness, Completeness::Exact);
    assert_eq!(
        ods.content.units[0].rows[0].values,
        vec![long_header, "2026"]
    );
    assert_eq!(ods.content.units[0].rows[1].values, vec!["Widget", "42"]);
    assert!(render_markdown(&ods).contains(&format!("| {long_header} | 2026 |")));
}

/// Verifies partial selections retain source element ordinals for DOCX page
/// breaks, DOCX tables, and ODT tables (GB-AUD-02).
#[test]
fn selected_docx_breaks_and_docx_odt_tables_retain_source_element_numbers() {
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
    const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
    const T: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
    const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";

    let dir = scratch("source-ordinals");

    // 1. DOCX with 2 explicit page breaks (`One`, break 1, `Two`, break 2, `Three`).
    //    Unit indices: 1=One, 2=break 1, 3=Two, 4=break 2, 5=Three.
    let breaks_docx = write_zip_fixture(
        &dir,
        "breaks.docx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "word/document.xml",
                &format!(
                    "<w:document xmlns:w=\"{W}\"><w:body>\
                     <w:p><w:r><w:t>One</w:t><w:br w:type=\"page\"/><w:t>Two</w:t><w:br w:type=\"page\"/><w:t>Three</w:t></w:r></w:p>\
                     </w:body></w:document>"
                ),
            ),
        ],
    );
    let full_breaks = extract(&breaks_docx, &extract_options(NativeSelection::All));
    assert_eq!(
        parse_all_markers(&render_markdown(&full_breaks)),
        vec![
            ("page-break".into(), vec![("number".into(), "1".into())]),
            ("page-break".into(), vec![("number".into(), "2".into())]),
        ]
    );
    let selected_break_2 = extract(
        &breaks_docx,
        &extract_options(NativeSelection::Ranges {
            ranges: vec![NativeRange { start: 4, end: 4 }],
        }),
    );
    assert_eq!(
        render_markdown(&selected_break_2),
        "<!-- bodymatter:v1 <page-break number=\"2\"/> -->\n"
    );

    // 2. DOCX with 2 tables: unit 1=Intro, unit 2=Table 1, unit 3=Mid, unit 4=Table 2.
    let tables_docx = write_zip_fixture(
        &dir,
        "tables.docx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "word/document.xml",
                &format!(
                    "<w:document xmlns:w=\"{W}\"><w:body>\
                     <w:p><w:r><w:t>Intro</w:t></w:r></w:p>\
                     <w:tbl><w:tr><w:tc><w:p><w:r><w:t>First</w:t></w:r></w:p></w:tc></w:tr></w:tbl>\
                     <w:p><w:r><w:t>Mid</w:t></w:r></w:p>\
                     <w:tbl><w:tr><w:tc><w:p><w:r><w:t>Second</w:t></w:r></w:p></w:tc></w:tr></w:tbl>\
                     </w:body></w:document>"
                ),
            ),
        ],
    );
    let selected_docx_table_2 = extract(
        &tables_docx,
        &extract_options(NativeSelection::Ranges {
            ranges: vec![NativeRange { start: 4, end: 4 }],
        }),
    );
    assert_eq!(
        render_markdown(&selected_docx_table_2),
        "<!-- bodymatter:v1 <table number=\"2\"/> -->\n\nSecond\n"
    );

    // 3. ODT with 2 tables: unit 1=Opening, unit 2=Table 1, unit 3=Middle, unit 4=Table 2.
    let tables_odt = write_zip_fixture(
        &dir,
        "tables.odt",
        &[
            ("mimetype", "application/vnd.oasis.opendocument.text"),
            (
                "content.xml",
                &format!(
                    "<office:document-content xmlns:office=\"{O}\" xmlns:text=\"{T}\" xmlns:table=\"{TABLE}\">\
                     <office:body><office:text>\
                     <text:p>Opening</text:p>\
                     <table:table table:name=\"T1\"><table:table-row><table:table-cell><text:p>First</text:p></table:table-cell></table:table-row></table:table>\
                     <text:p>Middle</text:p>\
                     <table:table table:name=\"T2\"><table:table-row><table:table-cell><text:p>Second</text:p></table:table-cell></table:table-row></table:table>\
                     </office:text></office:body></office:document-content>"
                ),
            ),
        ],
    );
    let selected_odt_table_2 = extract(
        &tables_odt,
        &extract_options(NativeSelection::Ranges {
            ranges: vec![NativeRange { start: 4, end: 4 }],
        }),
    );
    assert_eq!(
        render_markdown(&selected_odt_table_2),
        "<!-- bodymatter:v1 <table number=\"2\"/> -->\n\nSecond\n"
    );
}

/// Verifies CSV/TSV headers obey `--max-chars` and `--max-tokens` limits in
/// both JSON extracted content and rendered Markdown (GB-AUD-04).
#[test]
fn csv_markdown_and_json_headers_obey_char_and_token_limits() {
    use glassbox_inspect::reports::OmissionStatus;

    let dir = scratch("csv-limits");
    let path = dir.join("table.csv");
    std::fs::write(
        &path,
        "Alpha Beta Gamma Delta,Long descriptive column header\nA,B\n",
    )
    .unwrap();

    let mut opts = extract_options(NativeSelection::All);
    opts.max_chars = 5;
    opts.max_tokens = Some(1);
    let report = extract(&path, &opts);
    assert_eq!(report.status, OperationStatus::Partial);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(report.selection.omitted, OmissionStatus::SafetyLimit);
    assert_eq!(report.content.headers, vec!["Alpha", ""]);

    let md = render_markdown(&report);
    assert!(
        !md.contains("Alpha Beta Gamma Delta"),
        "leaked header: {md}"
    );
    assert!(
        !md.contains("Long descriptive column header"),
        "leaked second header: {md}"
    );
    assert_eq!(
        md,
        concat!(
            "<!-- bodymatter:v1 <sheet number=\"1\"/> -->\n\n",
            "| Alpha |  |\n",
            "| --- | --- |\n",
            "|  |  |\n",
        )
    );
    assert_no_markers_parity(&report);
}

/// Verifies raw source HTML in extracted prose and CSV table cells is escaped
/// in rendered Markdown without corrupting generated markers or table `<br>`
/// breaks (GB-AUD-08).
#[test]
fn extracted_markdown_escapes_raw_source_html_in_prose_and_tables() {
    let dir = scratch("raw-html");
    let txt_path = dir.join("raw.txt");
    std::fs::write(
        &txt_path,
        "<script>alert(1)</script>\n<img src=x onerror=1>\nNormal prose a_b.\n",
    )
    .unwrap();
    let txt_report = extract(&txt_path, &extract_options(NativeSelection::All));
    let txt_md = render_markdown(&txt_report);
    assert!(!txt_md.contains("<script>"));
    assert!(!txt_md.contains("<img "));
    assert!(txt_md.contains("&lt;script>alert(1)&lt;/script>"));
    assert!(txt_md.contains("&lt;img src=x onerror=1>"));
    assert!(txt_md.contains("Normal prose a_b."));

    let csv_path = dir.join("raw.csv");
    std::fs::write(
        &csv_path,
        "Header,Payload\n\"<script>x</script>\",\"line1\n<img onerror=1>\"\n",
    )
    .unwrap();
    let csv_report = extract(&csv_path, &extract_options(NativeSelection::All));
    let csv_md = render_markdown(&csv_report);
    assert!(csv_md.contains("<!-- bodymatter:v1 <sheet number=\"1\"/> -->"));
    assert!(!csv_md.contains("<script>"));
    assert!(!csv_md.contains("<img "));
    assert!(csv_md.contains("| &lt;script>x&lt;/script> | line1<br>&lt;img onerror=1> |"));
    assert_no_markers_parity(&csv_report);
}

/// Verifies a sparse, wide multi-sheet XLSX (100 sheets with cells at column
/// `XFD` = 16,384) bounds preview column width to `<= 256` and cumulative
/// materialized preview cells to `<= 65_536`, marking clipped sheets partial
/// without OOM or quadratic slowdown.
#[test]
fn sparse_wide_workbook_preview_is_column_and_cell_bounded() {
    use glassbox_inspect::reports::Completeness;

    const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
    const SHEET_COUNT: usize = 100;

    let dir = scratch("xlsx-wide-sparse");
    let mut wb_sheets = String::new();
    let mut wb_rels = String::new();
    let mut owned_entries: Vec<(String, String)> = Vec::with_capacity(SHEET_COUNT + 3);
    for i in 1..=SHEET_COUNT {
        wb_sheets.push_str(&format!(
            "<sheet name=\"Wide{i}\" sheetId=\"{i}\" r:id=\"rId{i}\"/>"
        ));
        wb_rels.push_str(&format!(
            "<Relationship Id=\"rId{i}\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet{i}.xml\"/>"
        ));
        let mut sheet_rows = String::new();
        for row in 1..=6 {
            sheet_rows.push_str(&format!(
                "<row r=\"{row}\">\
                 <c r=\"A{row}\" t=\"inlineStr\"><is><t>S{i}R{row}</t></is></c>\
                 <c r=\"XFD{row}\" t=\"inlineStr\"><is><t>Far{i}</t></is></c>\
                 </row>"
            ));
        }
        owned_entries.push((
            format!("xl/worksheets/sheet{i}.xml"),
            format!("<worksheet xmlns=\"{S}\"><sheetData>{sheet_rows}</sheetData></worksheet>"),
        ));
    }
    owned_entries.push((
        "[Content_Types].xml".into(),
        format!(
            "<Types xmlns=\"{CT}\">\
             <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
             </Types>"
        ),
    ));
    owned_entries.push((
        "xl/workbook.xml".into(),
        format!("<workbook xmlns=\"{S}\" xmlns:r=\"{R}\"><sheets>{wb_sheets}</sheets></workbook>"),
    ));
    owned_entries.push((
        "xl/_rels/workbook.xml.rels".into(),
        format!("<Relationships xmlns=\"{PKG}\">{wb_rels}</Relationships>"),
    ));
    let entry_refs: Vec<(&str, &str)> = owned_entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let path = write_zip_fixture(&dir, "wide-sparse.xlsx", &entry_refs);

    let report = extract(&path, &extract_options(NativeSelection::All));
    assert_eq!(report.status, OperationStatus::Partial);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(report.content.units.len(), SHEET_COUNT);

    let mut total_cells = 0usize;
    for unit in &report.content.units {
        assert_eq!(
            unit.completeness,
            Completeness::Partial,
            "wide sheet {} must report Partial completeness",
            unit.unit_index
        );
        for row in &unit.rows {
            assert!(
                row.values.len() <= 256,
                "row width {} exceeded MAX_PREVIEW_COLS (256)",
                row.values.len()
            );
            total_cells += row.values.len();
        }
    }
    assert!(
        total_cells <= 65_536,
        "workbook preview cells {total_cells} exceeded MAX_WORKBOOK_PREVIEW_CELLS (65536)"
    );
    assert_eq!(report.content.units[0].rows[0].values[0], "S1R1");
    assert_eq!(report.content.units[0].rows[0].values.len(), 256);

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies a long sheet name under a small `--max-chars` budget does not let
/// legacy Markdown header truncation abort v3 extraction `preview_rows`.
#[test]
fn long_sheet_name_under_small_max_chars_preserves_extract_preview_rows() {
    const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

    let sheet_name = "SummarySheetWithLongName";
    assert_eq!(sheet_name.len(), 24);
    let dir = scratch("xlsx-long-sheet-name");
    let path = write_zip_fixture(
        &dir,
        "long-name.xlsx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "xl/workbook.xml",
                &format!(
                    "<workbook xmlns=\"{S}\" xmlns:r=\"{R}\"><sheets>\
                     <sheet name=\"{sheet_name}\" sheetId=\"1\" r:id=\"rId1\"/>\
                     </sheets></workbook>"
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                &format!(
                    "<Relationships xmlns=\"{PKG}\">\
                     <Relationship Id=\"rId1\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
                     </Relationships>"
                ),
            ),
            (
                "xl/worksheets/sheet1.xml",
                &format!(
                    "<worksheet xmlns=\"{S}\"><sheetData>\
                     <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>a</t></is></c><c r=\"B1\" t=\"inlineStr\"><is><t>b</t></is></c></row>\
                     <row r=\"2\"><c r=\"A2\"><v>1</v></c><c r=\"B2\"><v>2</v></c></row>\
                     </sheetData></worksheet>"
                ),
            ),
        ],
    );

    let mut opts = extract_options(NativeSelection::All);
    opts.max_chars = 20;
    let report = extract(&path, &opts);
    assert_eq!(report.content.units.len(), 1);
    let unit = &report.content.units[0];
    assert_eq!(unit.rows.len(), 2, "expected both rows: {:?}", unit.rows);
    assert_eq!(unit.rows[0].values, vec!["a", "b"]);
    assert_eq!(unit.rows[1].values, vec!["1", "2"]);

    let v3_json = serde_json::to_value(report.to_v3(false)).unwrap();
    let json_rows = v3_json["content"]["units"][0]["rows"].as_array().unwrap();
    assert_eq!(json_rows.len(), 2);
    assert_eq!(json_rows[0]["values"], serde_json::json!(["a", "b"]));
    assert_eq!(json_rows[1]["values"], serde_json::json!(["1", "2"]));

    let md = render_markdown(&report);
    assert!(
        md.contains("| a | b |\n| --- | --- |\n| 1 | 2 |"),
        "both rows must render in Markdown: {md}"
    );
    assert_no_markers_parity(&report);

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies ODS cells containing literal tabs (`\t` and `<text:tab/>`) stay
/// intact without splitting into extra columns or dropping trailing cells, and
/// escape tabs to `<br>` in Markdown tables (GB-AUD-01).
#[test]
fn ods_cells_with_literal_tabs_preserve_columns_and_escape_markdown() {
    use glassbox_inspect::reports::Completeness;

    const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
    const T: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
    const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";

    let dir = scratch("ods-literal-tabs");
    let ods_path = write_zip_fixture(
        &dir,
        "tabs.ods",
        &[
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            (
                "content.xml",
                &format!(
                    "<office:document-content xmlns:office=\"{O}\" xmlns:text=\"{T}\" xmlns:table=\"{TABLE}\">\
                     <office:body><office:spreadsheet>\
                     <table:table table:name=\"Tabs\">\
                     <table:table-row>\
                       <table:table-cell office:value-type=\"string\"><text:p>Left&#9;Right</text:p></table:table-cell>\
                       <table:table-cell office:value-type=\"string\"><text:p>Tail</text:p></table:table-cell>\
                     </table:table-row>\
                     <table:table-row>\
                       <table:table-cell office:value-type=\"string\"><text:p>Lower<text:tab/>Mid</text:p></table:table-cell>\
                       <table:table-cell office:value-type=\"string\"><text:p>Final</text:p></table:table-cell>\
                     </table:table-row>\
                     </table:table>\
                     </office:spreadsheet></office:body></office:document-content>"
                ),
            ),
        ],
    );

    let report = extract(&ods_path, &extract_options(NativeSelection::All));
    assert_eq!(report.status, OperationStatus::Complete);
    assert_eq!(report.content.units.len(), 1);
    let unit = &report.content.units[0];
    assert_eq!(unit.completeness, Completeness::Exact);
    assert_eq!(unit.rows.len(), 2);
    assert_eq!(unit.rows[0].row_index, 1);
    assert_eq!(unit.rows[0].values, vec!["Left\tRight", "Tail"]);
    assert_eq!(unit.rows[1].row_index, 2);
    assert_eq!(unit.rows[1].values, vec!["Lower\tMid", "Final"]);

    let v3_json = serde_json::to_value(report.to_v3(false)).unwrap();
    assert_eq!(
        v3_json["content"]["units"][0]["rows"][0]["values"],
        serde_json::json!(["Left\tRight", "Tail"])
    );
    assert_eq!(
        v3_json["content"]["units"][0]["rows"][1]["values"],
        serde_json::json!(["Lower\tMid", "Final"])
    );

    let md = render_markdown(&report);
    assert!(
        md.contains("| Left<br>Right | Tail |\n| --- | --- |\n| Lower<br>Mid | Final |"),
        "expected literal tab escaped to <br> with Tail preserved, got:\n{md}"
    );
    assert_no_markers_parity(&report);
    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies sparse ODS and XLSX worksheets with empty leading and intermediate
/// rows emit true one-based physical `ContentRow.row_index` values (GB-AUD-01).
#[test]
fn sparse_ods_and_xlsx_preserve_physical_row_numbers() {
    const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
    const T: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
    const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
    const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

    let dir = scratch("sparse-row-numbers");

    // ODS: row 1 empty, row 2 has data, rows 3-4 empty (repeated=2), row 5 has data.
    let ods_path = write_zip_fixture(
        &dir,
        "sparse.ods",
        &[
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            (
                "content.xml",
                &format!(
                    "<office:document-content xmlns:office=\"{O}\" xmlns:text=\"{T}\" xmlns:table=\"{TABLE}\">\
                     <office:body><office:spreadsheet>\
                     <table:table table:name=\"SparseOds\">\
                     <table:table-row><table:table-cell/></table:table-row>\
                     <table:table-row>\
                       <table:table-cell office:value-type=\"string\"><text:p>R2A</text:p></table:table-cell>\
                       <table:table-cell office:value-type=\"string\"><text:p>R2B</text:p></table:table-cell>\
                     </table:table-row>\
                     <table:table-row table:number-rows-repeated=\"2\"><table:table-cell/></table:table-row>\
                     <table:table-row>\
                       <table:table-cell office:value-type=\"string\"><text:p>R5A</text:p></table:table-cell>\
                       <table:table-cell office:value-type=\"string\"><text:p>R5B</text:p></table:table-cell>\
                     </table:table-row>\
                     </table:table>\
                     </office:spreadsheet></office:body></office:document-content>"
                ),
            ),
        ],
    );
    let ods = extract(&ods_path, &extract_options(NativeSelection::All));
    assert_eq!(ods.status, OperationStatus::Complete);
    let ods_rows = &ods.content.units[0].rows;
    assert_eq!(ods_rows.len(), 2);
    assert_eq!(ods_rows[0].row_index, 2);
    assert_eq!(ods_rows[0].values, vec!["R2A", "R2B"]);
    assert_eq!(ods_rows[1].row_index, 5);
    assert_eq!(ods_rows[1].values, vec!["R5A", "R5B"]);

    // XLSX: row 1 empty element, row 2 has data, rows 3-4 absent, row 5 has data.
    let xlsx_path = write_zip_fixture(
        &dir,
        "sparse.xlsx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "xl/workbook.xml",
                &format!(
                    "<workbook xmlns=\"{S}\" xmlns:r=\"{R}\"><sheets>\
                     <sheet name=\"SparseXlsx\" sheetId=\"1\" r:id=\"rId1\"/>\
                     </sheets></workbook>"
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                &format!(
                    "<Relationships xmlns=\"{PKG}\">\
                     <Relationship Id=\"rId1\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
                     </Relationships>"
                ),
            ),
            (
                "xl/worksheets/sheet1.xml",
                &format!(
                    "<worksheet xmlns=\"{S}\"><sheetData>\
                     <row r=\"1\"/>\
                     <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>X2A</t></is></c><c r=\"B2\" t=\"inlineStr\"><is><t>X2B</t></is></c></row>\
                     <row r=\"5\"><c r=\"A5\" t=\"inlineStr\"><is><t>X5A</t></is></c><c r=\"B5\" t=\"inlineStr\"><is><t>X5B</t></is></c></row>\
                     </sheetData></worksheet>"
                ),
            ),
        ],
    );
    let xlsx = extract(&xlsx_path, &extract_options(NativeSelection::All));
    assert_eq!(xlsx.status, OperationStatus::Complete);
    let xlsx_rows = &xlsx.content.units[0].rows;
    assert_eq!(xlsx_rows.len(), 2);
    assert_eq!(xlsx_rows[0].row_index, 2);
    assert_eq!(xlsx_rows[0].values, vec!["X2A", "X2B"]);
    assert_eq!(xlsx_rows[1].row_index, 5);
    assert_eq!(xlsx_rows[1].values, vec!["X5A", "X5B"]);

    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies end-to-end `.txt`, `.csv`, and `.xlsx` extraction neutralizes raw
/// Unicode line terminators (`U+0085`, `U+2028`, `U+2029`), vertical tab/form
/// feed (`\x0b`, `\x0c`), and forbidden C0 controls including ANSI ESC (`0x1B`)
/// in rendered Markdown (`T02-SEC-002`).
#[test]
fn extracted_markdown_neutralizes_unicode_line_terminators_and_control_chars() {
    const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";

    let forbidden_raw = [
        '\u{0085}', '\u{2028}', '\u{2029}', '\u{000B}', '\u{000C}', '\u{001B}', '\u{0007}',
        '\u{001F}', '\u{FFFE}', '\u{FFFF}',
    ];

    let dir = scratch("controls-and-unicode-breaks");

    // 1. Plain text (`.txt`): normalizes Unicode/VT/FF line breaks (escaping smuggled
    //    Markdown block triggers), replaces ANSI ESC and forbidden C0 controls with U+FFFD,
    //    and preserves tabs in prose.
    let txt_path = dir.join("controls.txt");
    std::fs::write(
        &txt_path,
        "Plain\tkeep\u{0085}> quote\u{2028}# heading\u{2029}---\u{000B}- item\u{000C}\x1b[31mred\x1b[0m\u{0007}\u{001F}\u{FFFE}\u{FFFF}\n",
    )
    .unwrap();
    let txt_report = extract(&txt_path, &extract_options(NativeSelection::All));
    assert_eq!(txt_report.metadata.status, OperationStatus::Complete);
    let txt_md = render_markdown(&txt_report);
    for ch in forbidden_raw {
        assert!(
            !txt_md.contains(ch),
            ".txt rendered Markdown leaked raw U+{:04X}: {txt_md:?}",
            ch as u32
        );
    }
    assert!(txt_md.contains("Plain\tkeep\n\\> quote\n\\# heading\n\\---\n\\- item\n\u{FFFD}[31mred\u{FFFD}[0m\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}"));
    assert_no_markers_parity(&txt_report);

    // 2. CSV (`.csv`): table cells stay strictly single-line with all breaks mapped to `<br>`
    //    and ANSI ESC / forbidden C0 controls replaced with U+FFFD.
    let csv_path = dir.join("controls.csv");
    std::fs::write(
        &csv_path,
        "Header,Payload\n\"a\u{0085}b\u{2028}c\u{2029}d\u{000B}e\u{000C}f\",\"\x1b[31mred\x1b[0m\u{0007}\u{001F}\u{FFFE}\u{FFFF}\"\n",
    )
    .unwrap();
    let csv_report = extract(&csv_path, &extract_options(NativeSelection::All));
    assert_eq!(csv_report.metadata.status, OperationStatus::Complete);
    let csv_md = render_markdown(&csv_report);
    for ch in forbidden_raw {
        assert!(
            !csv_md.contains(ch),
            ".csv rendered Markdown leaked raw U+{:04X}: {csv_md:?}",
            ch as u32
        );
    }
    assert!(
        !csv_md.chars().any(|ch| ch <= '\u{001F}' && ch != '\n'),
        ".csv rendered Markdown leaked raw non-LF C0 control: {csv_md:?}"
    );
    assert!(
        csv_md.contains(
            "| a<br>b<br>c<br>d<br>e<br>f | \u{FFFD}[31mred\u{FFFD}[0m\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD} |"
        ),
        "unexpected .csv rendered Markdown:\n{csv_md}"
    );
    assert_no_markers_parity(&csv_report);

    // 3. XLSX (`.xlsx`): valid XML 1.0 cells containing Unicode line terminators
    //    (`U+0085`, `U+2028`, `U+2029`) render strictly single-line with `<br>`.
    let xlsx_path = write_zip_fixture(
        &dir,
        "unicode-breaks.xlsx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "xl/workbook.xml",
                &format!(
                    "<workbook xmlns=\"{S}\" xmlns:r=\"{R}\"><sheets>\
                     <sheet name=\"Breaks\" sheetId=\"1\" r:id=\"rId1\"/>\
                     </sheets></workbook>"
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                &format!(
                    "<Relationships xmlns=\"{PKG}\">\
                     <Relationship Id=\"rId1\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
                     </Relationships>"
                ),
            ),
            (
                "xl/worksheets/sheet1.xml",
                &format!(
                    "<worksheet xmlns=\"{S}\"><sheetData>\
                     <row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Col</t></is></c><c r=\"B1\" t=\"inlineStr\"><is><t>Val</t></is></c></row>\
                     <row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>nel\u{0085}ls\u{2028}ps\u{2029}end</t></is></c><c r=\"B2\" t=\"inlineStr\"><is><t>ok</t></is></c></row>\
                     </sheetData></worksheet>"
                ),
            ),
        ],
    );
    let xlsx_report = extract(&xlsx_path, &extract_options(NativeSelection::All));
    assert_eq!(xlsx_report.metadata.status, OperationStatus::Complete);
    let xlsx_md = render_markdown(&xlsx_report);
    for ch in forbidden_raw {
        assert!(
            !xlsx_md.contains(ch),
            ".xlsx rendered Markdown leaked raw U+{:04X}: {xlsx_md:?}",
            ch as u32
        );
    }
    assert!(
        !xlsx_md.chars().any(|ch| ch <= '\u{001F}' && ch != '\n'),
        ".xlsx rendered Markdown leaked raw non-LF C0 control: {xlsx_md:?}"
    );
    assert!(
        xlsx_md.contains("| nel<br>ls<br>ps<br>end | ok |"),
        "unexpected .xlsx rendered Markdown:\n{xlsx_md}"
    );
    assert_no_markers_parity(&xlsx_report);

    std::fs::remove_dir_all(&dir).ok();
}

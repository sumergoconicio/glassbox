//! Module: glassbox-inspect Wave-2 reader audit regression tests.
//! File: audit_wave2_readers.rs.
//! Responsibility: Verify fixes for iWork preview provenance (GB-AUD-12), DOCX
//! content-control wrappers (GB-AUD-13), MOBI cross-record UTF-8/HTML decoding
//! (GB-AUD-14), text unit-cap selection boundary (GB-AUD-16), and EPUB block
//! and table whitespace preservation (GB-AUD-21).
use glassbox_inspect::extract;
use glassbox_inspect::reports::{
    Completeness, ContentOrigin, ExtractOptions, FactSource, NativeRange, NativeSelection,
    NativeUnit, OcrOptions, OmissionStatus, OperationStatus,
};
use std::io::Write;
use std::path::PathBuf;

/// Creates a unique writable scratch directory under the shared test target.
///
/// Accepts a short test label. Returns the newly created directory path.
/// Panics if directory creation fails.
fn scratch(label: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "audit-wave2-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Builds extraction options with OCR disabled for deterministic structural tests.
///
/// Accepts a native selection. Returns [`ExtractOptions`] with OCR disabled.
/// Performs no I/O and cannot fail.
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

/// Generates a minimal valid PDF 1.4 byte stream containing one page per entry in `pages`.
///
/// Accepts a slice of plain ASCII strings (one per page). Returns valid PDF
/// bytes with exact xref offsets and Type1 Helvetica text streams. Performs no
/// I/O and cannot fail.
fn build_multipage_pdf(pages: &[String]) -> Vec<u8> {
    let mut pdf = Vec::new();
    let mut offsets = vec![0usize];
    pdf.extend_from_slice(b"%PDF-1.4\n");

    let push_obj = |pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, id: usize, body: &[u8]| {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    };

    // Object 1: Catalog, Object 2: Pages, Object 3: Font.
    // For each page i in 0..pages.len():
    //   Page obj = 4 + 2*i
    //   Content stream obj = 5 + 2*i
    let kids: Vec<String> = (0..pages.len())
        .map(|i| format!("{} 0 R", 4 + 2 * i))
        .collect();
    push_obj(
        &mut pdf,
        &mut offsets,
        1,
        b"<< /Type /Catalog /Pages 2 0 R >>",
    );
    push_obj(
        &mut pdf,
        &mut offsets,
        2,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            pages.len()
        )
        .as_bytes(),
    );
    push_obj(
        &mut pdf,
        &mut offsets,
        3,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    );

    for (i, text) in pages.iter().enumerate() {
        let page_id = 4 + 2 * i;
        let content_id = 5 + 2 * i;
        let escaped = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        let stream = format!("BT /F1 12 Tf 72 720 Td ({escaped}) Tj ET");
        push_obj(
            &mut pdf,
            &mut offsets,
            page_id,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
            )
            .as_bytes(),
        );
        push_obj(
            &mut pdf,
            &mut offsets,
            content_id,
            format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            )
            .as_bytes(),
        );
    }

    let xref_offset = pdf.len();
    let total_objects = offsets.len();
    pdf.extend_from_slice(format!("xref\n0 {total_objects}\n").as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {total_objects} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
        )
        .as_bytes(),
    );
    pdf
}

/// Builds a synthetic ZIP archive from `(name, bytes)` entries at `path`.
///
/// Accepts destination path and slice of entries. Panics on I/O or ZIP errors.
fn write_zip(path: &PathBuf, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, data) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap();
}

/// Builds a valid uncompressed PalmDB/MOBI file from raw text record slices.
///
/// Accepts text record byte slices and MOBI encoding (`65001` UTF-8 or `1252`
/// Windows-1252). Returns complete `.mobi` file bytes with valid PalmDB header,
/// offset table, MOBI header (version 6, no EXTH), and sequential text records.
fn build_mobi(records: &[&[u8]], encoding: u32) -> Vec<u8> {
    let text_records = records.len();
    let total_records = 1 + text_records;
    let text_length: usize = records.iter().map(|r| r.len()).sum();

    let mut head = vec![0u8; 248];
    // PalmDOC header (bytes 0..16)
    head[0..2].copy_from_slice(&1u16.to_be_bytes()); // compression = 1 (uncompressed)
    head[4..8].copy_from_slice(&(text_length as u32).to_be_bytes());
    head[8..10].copy_from_slice(&(text_records as u16).to_be_bytes());
    head[10..12].copy_from_slice(&4096u16.to_be_bytes()); // record size
    head[12..14].copy_from_slice(&0u16.to_be_bytes()); // encryption = none
    // MOBI header (bytes 16..248, header_size = 232)
    head[16..20].copy_from_slice(b"MOBI");
    head[20..24].copy_from_slice(&232u32.to_be_bytes());
    head[28..32].copy_from_slice(&encoding.to_be_bytes());
    head[36..40].copy_from_slice(&6u32.to_be_bytes()); // version 6
    head[92..96].copy_from_slice(&9u32.to_be_bytes()); // locale English
    head[128..132].copy_from_slice(&0u32.to_be_bytes()); // no EXTH
    head[240..244].copy_from_slice(&0u32.to_be_bytes()); // extra flags = 0

    let mut out = vec![0u8; 78 + total_records * 8];
    out[0..10].copy_from_slice(b"Wave2Book\0");
    out[60..68].copy_from_slice(b"BOOKMOBI");
    out[76..78].copy_from_slice(&(total_records as u16).to_be_bytes());

    let mut offset = out.len() as u32;
    let mut table_pos = 78;
    out[table_pos..table_pos + 4].copy_from_slice(&offset.to_be_bytes());
    table_pos += 8;
    offset += head.len() as u32;

    for record in records {
        out[table_pos..table_pos + 4].copy_from_slice(&offset.to_be_bytes());
        table_pos += 8;
        offset += record.len() as u32;
    }

    out.extend_from_slice(&head);
    for record in records {
        out.extend_from_slice(record);
    }
    out
}

/// GB-AUD-12: Extracting an iWork `.pages` package with a 6-page embedded
/// `QuickLook/Preview.pdf` must never emit native PDF `PhysicalPage` units;
/// it emits at most one bounded `DatasetPreview` unit from page 1 only
/// (capped at 4,000 chars) with `DerivedPreview` provenance and `Partial`
/// completeness.
#[test]
fn gb_aud_12_iwork_pages_preview_emits_single_bounded_derived_preview_unit() {
    let dir = scratch("gb-aud-12");
    let path = dir.join("six_page.pages");

    let page1 = format!("PAGE_ONE_HEADER {}", "A".repeat(4_500));
    let pages = vec![
        page1,
        "PAGE_TWO_SECRET".to_string(),
        "PAGE_THREE_SECRET".to_string(),
        "PAGE_FOUR_SECRET".to_string(),
        "PAGE_FIVE_SECRET".to_string(),
        "PAGE_SIX_SECRET".to_string(),
    ];
    let pdf_bytes = build_multipage_pdf(&pages);
    write_zip(
        &path,
        &[
            ("Index/Document.iwa", b"synthetic-iwa-placeholder"),
            ("QuickLook/Preview.pdf", &pdf_bytes),
        ],
    );

    let report = extract(&path, &extract_options(NativeSelection::All));
    assert_eq!(report.selection.unit, NativeUnit::DatasetPreview);
    assert_eq!(report.selection.available.value, Some(1));
    assert_eq!(
        report.selection.available.source,
        FactSource::DerivedPreview
    );
    assert_eq!(
        report.selection.available.completeness,
        Completeness::Partial
    );
    assert_eq!(report.content.units.len(), 1);
    let unit = &report.content.units[0];
    assert_eq!(unit.unit_index, 1);
    assert_eq!(unit.unit, NativeUnit::DatasetPreview);
    assert_eq!(unit.source_part.as_deref(), Some("QuickLook/Preview.pdf"));
    assert_eq!(unit.completeness, Completeness::Partial);
    assert_eq!(unit.provenance.len(), 1);
    assert_eq!(unit.provenance[0].reader, "iwork-preview");
    assert_eq!(unit.provenance[0].stage, "embedded-preview");
    assert_eq!(unit.provenance[0].source, FactSource::DerivedPreview);
    assert_ne!(unit.provenance[0].origin, ContentOrigin::Native);
    assert_eq!(unit.provenance[0].origin, ContentOrigin::DerivedPreview);

    let text = unit.text.as_deref().expect("preview unit must carry text");
    assert!(text.contains("PAGE_ONE_HEADER"), "got: {text}");
    assert!(
        text.chars().count() <= 4_000,
        "preview text must be capped at 4000 chars, got {}",
        text.chars().count()
    );
    for secret in [
        "PAGE_TWO_SECRET",
        "PAGE_THREE_SECRET",
        "PAGE_FOUR_SECRET",
        "PAGE_FIVE_SECRET",
        "PAGE_SIX_SECRET",
    ] {
        assert!(!text.contains(secret), "leaked later preview page {secret}");
    }
    assert_eq!(report.completeness, Completeness::Partial);
    assert_eq!(report.status, OperationStatus::Partial);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
}

/// GB-AUD-13: DOCX extraction traverses `<w:sdt>/<w:sdtContent>`, `<w:ins>`,
/// `<w:smartTag>`, and `<w:customXml>` in exact source order, and discloses
/// partial status whenever an unknown wrapper hides nested Word content.
#[test]
fn gb_aud_13_docx_traverses_content_controls_and_discloses_unknown_wrappers() {
    let dir = scratch("gb-aud-13");
    let content_types = br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;

    // 1. Supported wrappers emit paragraphs and tables in exact source order.
    let wrapped_path = dir.join("wrapped.docx");
    let wrapped_doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:sdt>
      <w:sdtPr><w:tag w:val="intro"/></w:sdtPr>
      <w:sdtContent>
        <w:p><w:r><w:t>First </w:t></w:r><w:ins><w:r><w:t>SDT paragraph</w:t></w:r></w:ins></w:p>
        <w:tbl>
          <w:tr>
            <w:tc><w:p><w:r><w:t>H1</w:t></w:r></w:p></w:tc>
            <w:tc><w:p><w:r><w:t>H2</w:t></w:r></w:p></w:tc>
          </w:tr>
          <w:tr>
            <w:tc><w:p><w:r><w:t>V1</w:t></w:r></w:p></w:tc>
            <w:tc><w:p><w:r><w:t>V2</w:t></w:r></w:p></w:tc>
          </w:tr>
        </w:tbl>
      </w:sdtContent>
    </w:sdt>
    <w:smartTag><w:p><w:r><w:t>Smart tag block</w:t></w:r></w:p></w:smartTag>
    <w:customXml><w:ins><w:p><w:r><w:t>Custom XML inserted</w:t></w:r></w:p></w:ins></w:customXml>
  </w:body>
</w:document>"#;
    write_zip(
        &wrapped_path,
        &[
            ("[Content_Types].xml", content_types),
            ("word/document.xml", wrapped_doc),
        ],
    );

    let report = extract(&wrapped_path, &extract_options(NativeSelection::All));
    assert_eq!(report.status, OperationStatus::Complete);
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
            (NativeUnit::DocumentBlock, Some("First SDT paragraph")),
            (NativeUnit::Table, Some("H1\tH2\nV1\tV2")),
            (NativeUnit::DocumentBlock, Some("Smart tag block")),
            (NativeUnit::DocumentBlock, Some("Custom XML inserted")),
        ]
    );

    // 2. An unknown wrapper hiding nested content must never claim complete/exact.
    let unknown_path = dir.join("unknown_wrapper.docx");
    let unknown_doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:altChunk><w:p><w:r><w:t>Hidden inside unsupported wrapper</w:t></w:r></w:p></w:altChunk>
  </w:body>
</w:document>"#;
    write_zip(
        &unknown_path,
        &[
            ("[Content_Types].xml", content_types),
            ("word/document.xml", unknown_doc),
        ],
    );
    let unknown_report = extract(&unknown_path, &extract_options(NativeSelection::All));
    assert_ne!(unknown_report.status, OperationStatus::Complete);
    assert_eq!(unknown_report.metadata.status, OperationStatus::Complete);
    assert_ne!(unknown_report.completeness, Completeness::Exact);
}

/// GB-AUD-14: MOBI extraction preserves UTF-8 multi-byte decoding and HTML
/// open-tag/blocked-section state across PalmDB text record boundaries for both
/// full extraction and partial `--pages 2-2` selection.
#[test]
fn gb_aud_14_mobi_preserves_utf8_and_html_state_across_record_boundaries() {
    let dir = scratch("gb-aud-14");

    // Case 1: 2-record MOBI where `<title>Split Title</title>` spans records 1 & 2,
    // followed by `<p>Chapter Body Text</p>` in record 2.
    let two_rec_path = dir.join("split_title.mobi");
    let rec1 = b"<html><head><title>Split ";
    let rec2 = b"Title</title></head><body><p>Chapter Body Text</p></body></html>";
    std::fs::write(&two_rec_path, build_mobi(&[rec1, rec2], 65001)).unwrap();

    let full = extract(&two_rec_path, &extract_options(NativeSelection::All));
    assert!(
        !full.diagnostics.iter().any(|d| d.code == "mobi_text"),
        "unexpected mobi_text diagnostics: {:?}",
        full.diagnostics
    );
    assert_eq!(full.status, OperationStatus::Complete);
    assert_eq!(full.metadata.status, OperationStatus::Complete);
    assert_eq!(full.content.units.len(), 2);
    let combined: String = full
        .content
        .units
        .iter()
        .filter_map(|u| u.text.as_deref())
        .collect();
    assert!(combined.contains("Chapter Body Text"), "got: {combined:?}");
    assert!(
        !combined.contains("Split Title"),
        "head/title text must stay omitted: {combined:?}"
    );

    let partial = extract(
        &two_rec_path,
        &extract_options(NativeSelection::Ranges {
            ranges: vec![NativeRange { start: 2, end: 2 }],
        }),
    );
    assert!(
        !partial.diagnostics.iter().any(|d| d.code == "mobi_text"),
        "unexpected mobi_text diagnostics on partial selection: {:?}",
        partial.diagnostics
    );
    assert_eq!(partial.status, OperationStatus::Complete);
    assert_eq!(partial.metadata.status, OperationStatus::Complete);
    assert_eq!(partial.content.units.len(), 1);
    assert_eq!(partial.content.units[0].unit_index, 2);
    assert_eq!(
        partial.content.units[0].text.as_deref(),
        Some("Chapter Body Text\n")
    );

    // Case 2: 3-record MOBI where both `<title>...</title>` AND a 2-byte UTF-8
    // character (`é` = 0xC3 0xA9) span record boundaries.
    let utf8_path = dir.join("split_utf8.mobi");
    let r1 = b"<html><head><title>Split ";
    let r2 = b"Title</title></head><body><p>Caf\xC3";
    let r3 = b"\xA9 Chapter Body Text</p></body></html>";
    std::fs::write(&utf8_path, build_mobi(&[r1, r2, r3], 65001)).unwrap();

    let utf8_full = extract(&utf8_path, &extract_options(NativeSelection::All));
    assert!(
        !utf8_full.diagnostics.iter().any(|d| d.code == "mobi_text"),
        "unexpected mobi_text diagnostics on split UTF-8: {:?}",
        utf8_full.diagnostics
    );
    assert_eq!(utf8_full.status, OperationStatus::Complete);
    assert_eq!(utf8_full.metadata.status, OperationStatus::Complete);
    assert_eq!(utf8_full.content.units.len(), 3);
    let utf8_combined: String = utf8_full
        .content
        .units
        .iter()
        .filter_map(|u| u.text.as_deref())
        .collect();
    assert_eq!(utf8_combined, "Café Chapter Body Text\n");
}

/// UTF-8 BOM regression: a leading `\u{feff}` in a multi-record UTF-8 MOBI must
/// never desynchronize `quick-xml` byte offsets, panic on UTF-8 char
/// boundaries, leak markup tags, or drop trailing characters.
#[test]
fn gb_aud_14_mobi_leading_utf8_bom_preserves_offsets_and_text_across_records() {
    let dir = scratch("gb-aud-14-bom");

    // Trigger 1: Record 1 = b"\xEF\xBB\xBFa", Record 2 = b"\xC3\xA9bc".
    // Without explicit BOM stripping, `quick-xml` shifts positions by -3 and
    // slicing into the middle of `é` panics on a UTF-8 char boundary.
    let multibyte_path = dir.join("bom_multibyte.mobi");
    let rec1 = b"\xEF\xBB\xBFa";
    let rec2 = b"\xC3\xA9bc";
    std::fs::write(&multibyte_path, build_mobi(&[rec1, rec2], 65001)).unwrap();

    let mb_report = extract(&multibyte_path, &extract_options(NativeSelection::All));
    assert!(
        !mb_report.diagnostics.iter().any(|d| d.code == "mobi_text"),
        "unexpected mobi_text diagnostics on BOM multibyte MOBI: {:?}",
        mb_report.diagnostics
    );
    assert_eq!(mb_report.status, OperationStatus::Complete);
    assert_eq!(mb_report.metadata.status, OperationStatus::Complete);
    assert_eq!(mb_report.content.units.len(), 2);
    assert_eq!(mb_report.content.units[0].text.as_deref(), Some("a"));
    assert_eq!(mb_report.content.units[1].text.as_deref(), Some("ébc"));

    // Trigger 2: UTF-8 BOM followed by `<p>hello world</p>` split across 2 records.
    // Must not leak `<p>` or drop `"rld"`.
    let markup_path = dir.join("bom_markup.mobi");
    let m1 = b"\xEF\xBB\xBF<p>hello ";
    let m2 = b"world</p>";
    std::fs::write(&markup_path, build_mobi(&[m1, m2], 65001)).unwrap();

    let markup_report = extract(&markup_path, &extract_options(NativeSelection::All));
    assert!(
        !markup_report
            .diagnostics
            .iter()
            .any(|d| d.code == "mobi_text"),
        "unexpected mobi_text diagnostics on BOM markup MOBI: {:?}",
        markup_report.diagnostics
    );
    assert_eq!(markup_report.status, OperationStatus::Complete);
    assert_eq!(markup_report.metadata.status, OperationStatus::Complete);
    assert_eq!(markup_report.content.units.len(), 2);
    assert_eq!(
        markup_report.content.units[0].text.as_deref(),
        Some("hello ")
    );
    assert_eq!(
        markup_report.content.units[1].text.as_deref(),
        Some("world\n")
    );
    let combined: String = markup_report
        .content
        .units
        .iter()
        .filter_map(|u| u.text.as_deref())
        .collect();
    assert_eq!(combined, "hello world\n");
}

/// GB-AUD-16: Requesting exactly `65_536` lines (`FirstN { count: 65_536 }`)
/// from a `65_537`-line text file must complete with `OmissionStatus::Selection`
/// and exact available/unit completeness (never false-triggering `SafetyLimit`),
/// while requesting `All` on the same `65_537`-line file still caps at `65_536`
/// and reports partial status.
#[test]
fn gb_aud_16_text_unit_cap_exact_first_n_completes_without_safety_limit() {
    let dir = scratch("gb-aud-16");
    let path = dir.join("lines_65537.txt");
    std::fs::write(&path, "x\n".repeat(65_537)).unwrap();

    let exact_cap = extract(
        &path,
        &extract_options(NativeSelection::FirstN { count: 65_536 }),
    );
    assert_eq!(exact_cap.status, OperationStatus::Complete);
    assert_eq!(exact_cap.metadata.status, OperationStatus::Complete);
    assert_eq!(exact_cap.selection.omitted, OmissionStatus::Selection);
    assert_eq!(exact_cap.selection.available.value, Some(65_537));
    assert_eq!(
        exact_cap.selection.available.completeness,
        Completeness::Exact
    );
    assert_eq!(exact_cap.content.units.len(), 65_536);
    assert!(
        exact_cap
            .content
            .units
            .iter()
            .all(|u| u.completeness == Completeness::Exact)
    );

    let all = extract(&path, &extract_options(NativeSelection::All));
    assert_eq!(all.content.units.len(), 65_536);
    assert_eq!(all.status, OperationStatus::Partial);
    assert_eq!(all.metadata.status, OperationStatus::Complete);
    assert_ne!(all.selection.omitted, OmissionStatus::None);
    assert_ne!(all.selection.omitted, OmissionStatus::Selection);
}

/// GB-AUD-21: EPUB text extraction preserves readable boundary whitespace across
/// headings `h4`-`h6`, tables (`tr`/`td`/`th`), blockquotes, definition lists,
/// and preformatted blocks so adjacent tokens are never jammed together.
#[test]
fn gb_aud_21_epub_preserves_block_and_table_whitespace() {
    let dir = scratch("gb-aud-21");
    let path = dir.join("blocks_and_tables.epub");
    let container = br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
    let opf = br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Whitespace Regression</dc:title>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
  </spine>
</package>"#;
    let xhtml = br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Ignored Title</title></head><body><h4>Heading</h4><table><tr><td>Alpha</td><td>Beta</td></tr></table><blockquote>Quote</blockquote><dl><dt>Term</dt><dd>Def</dd></dl><pre>Code</pre><h5>Sub</h5></body></html>"#;
    write_zip(
        &path,
        &[
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/chapter1.xhtml", xhtml),
        ],
    );

    let report = extract(&path, &extract_options(NativeSelection::All));
    assert_eq!(report.status, OperationStatus::Complete);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(report.content.units.len(), 1);
    let text = report.content.units[0]
        .text
        .as_deref()
        .expect("spine item must carry text");
    assert!(
        !text.contains("HeadingAlphaBeta"),
        "adjacent heading/cells must not jam together: {text:?}"
    );
    assert!(
        !text.contains("AlphaBeta"),
        "adjacent table cells must not jam together: {text:?}"
    );
    assert_eq!(text, "Heading\nAlpha\tBeta\nQuote\nTerm\nDef\nCode\nSub\n");
}

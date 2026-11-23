//! Module: glassbox-inspect canonical extraction integration tests.
//! File: extract_selection.rs.
//! Responsibility: Verify native-unit selection, independent content, limits, and coverage across readers.

use glassbox_inspect::reports::{
    Completeness, ExtractOptions, ExtractReport, NativeRange, NativeSelection, OcrOptions,
    OmissionStatus, OperationStatus,
};
use glassbox_inspect::{InspectOptions, extract, extract_transitional, selection::parse_pages};
use std::path::{Path, PathBuf};

/// Returns a checked-in fixture without reading or mutating its contents.
///
/// Accepts a fixture file name. Returns its absolute path. Missing fixtures fail
/// in the subsequent extraction call with a structured report diagnostic.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Creates extraction controls with OCR disabled for deterministic native tests.
///
/// Accepts a native selection. Returns full character/token defaults with the
/// requested selection. No file, model, or network access occurs.
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

/// Extracts one checked-in fixture without OCR.
///
/// Accepts a fixture name and selection. Returns the canonical v2 report. Reader
/// failures stay inside the report so each test can assert status explicitly.
fn selected(name: &str, selection: NativeSelection) -> ExtractReport {
    extract(&fixture(name), &options(selection))
}

/// Creates a unique test-owned file under the ignored target tree.
///
/// Accepts a test label, file name, and UTF-8 contents. Returns the written path.
/// Filesystem failures panic; checked-in fixtures and production data are untouched.
fn input(label: &str, name: &str, contents: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/extract-selection")
        .join(format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

/// Creates a one-page image-only PDF from the checked-in text raster.
///
/// Accepts a test-owned output path. Returns unit after writing a minimal PDF
/// with a JPEG XObject and checked cross-reference offsets. Image encoding and
/// filesystem failures panic; no source fixture is modified.
fn scanned_pdf(path: &Path) {
    let raster = path.with_extension("png");
    let rendered = extract_transitional(
        &fixture("native.pdf"),
        &InspectOptions {
            thumbnail: Some(raster.clone()),
            thumbnail_max_edge: 800,
            max_units: 0,
            ..InspectOptions::default()
        },
    );
    assert!(rendered.errors.is_empty(), "{:?}", rendered.errors);
    let image = image::open(raster).unwrap().to_rgb8();
    let (width, height) = image.dimensions();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&image)
        .unwrap();
    let stream = |body: &[u8]| {
        let mut value = format!("<< /Length {} >>\nstream\n", body.len()).into_bytes();
        value.extend_from_slice(body);
        value.extend_from_slice(b"\nendstream");
        value
    };
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 640 400] /Resources << /XObject << /Im1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        stream(b"q 640 0 0 400 0 0 cm /Im1 Do Q"),
        {
            let mut object = format!("<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n", jpeg.len()).into_bytes();
            object.extend_from_slice(&jpeg);
            object.extend_from_slice(b"\nendstream");
            object
        },
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(object);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    std::fs::write(path, pdf).unwrap();
}
/// Verifies bare text extraction returns every logical line, including a final unterminated line.
#[test]
fn text_full_request_preserves_all_logical_lines() {
    let path = input(
        "full",
        "lines.txt",
        "zero\none\ntwo\nthree\nfour\nfive\nsix",
    );
    let report = extract(&path, &options(NativeSelection::All));
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(report.selection.available.value, Some(7));
    assert_eq!(
        report.selection.returned_indices,
        (1..=7).collect::<Vec<_>>()
    );
    assert_eq!(
        report
            .content
            .units
            .iter()
            .map(|unit| unit.text.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["zero", "one", "two", "three", "four", "five", "six"]
    );
    assert_eq!(report.selection.omitted, OmissionStatus::None);
}

/// Verifies valueless-five semantics, first-N beyond total, and source-order range deduplication.
#[test]
fn text_first_n_and_ranges_have_exact_coverage() {
    let path = input("selection", "lines.txt", "0\n1\n2\n3\n4\n5\n6\n");
    let first = extract(&path, &options(parse_pages("5").unwrap()));
    assert_eq!(first.selection.returned_indices, [1, 2, 3, 4, 5]);
    assert_eq!(first.metadata.status, OperationStatus::Complete);
    assert_eq!(first.completeness, Completeness::Partial);
    assert_eq!(first.selection.omitted, OmissionStatus::Selection);

    let beyond = extract(&path, &options(parse_pages("99").unwrap()));
    assert_eq!(beyond.metadata.status, OperationStatus::Complete);
    assert_eq!(
        beyond.selection.returned_indices,
        (1..=7).collect::<Vec<_>>()
    );
    assert_eq!(beyond.selection.omitted, OmissionStatus::None);

    let ranges = extract(&path, &options(parse_pages("2-4,6,3-4").unwrap()));
    assert_eq!(ranges.selection.returned_indices, [2, 3, 4, 6]);
    assert_eq!(
        ranges
            .content
            .units
            .iter()
            .map(|unit| unit.text.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["1", "2", "3", "5"]
    );
    assert_eq!(ranges.metadata.status, OperationStatus::Complete);
    assert_eq!(ranges.completeness, Completeness::Partial);
    assert_eq!(ranges.selection.omitted, OmissionStatus::Selection);
}

/// Verifies malformed selectors fail before reader dispatch and out-of-range selections fail clearly.
#[test]
fn selectors_reject_invalid_and_wholly_unavailable_indices() {
    for invalid in ["", "0", "-1", "4-2", "1,,2", " 1", "18446744073709551616"] {
        assert!(parse_pages(invalid).is_err(), "accepted {invalid:?}");
    }
    let path = input("out-of-range", "lines.txt", "only\nlines\n");
    let report = extract(
        &path,
        &options(NativeSelection::Ranges {
            ranges: vec![NativeRange { start: 3, end: 4 }],
        }),
    );
    assert_eq!(report.metadata.status, OperationStatus::Failed);
    assert!(report.content.units.is_empty());
    assert_eq!(report.completeness, Completeness::Unavailable);
    assert!(
        report
            .metadata
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("outside the available count"))
    );
}

/// Verifies character and token controls retain prefixes and report safety omissions.
#[test]
fn content_limits_are_partial_and_explicit() {
    let path = input("limits", "lines.txt", "alpha beta\ngamma delta\n");
    let chars = extract(
        &path,
        &ExtractOptions {
            max_chars: 7,
            ..options(NativeSelection::All)
        },
    );
    assert_eq!(chars.metadata.status, OperationStatus::Partial);
    assert_eq!(chars.selection.omitted, OmissionStatus::SafetyLimit);
    assert_eq!(chars.content.units[0].text.as_deref(), Some("alpha b"));

    let tokens = extract(
        &path,
        &ExtractOptions {
            max_tokens: Some(2),
            ..options(NativeSelection::All)
        },
    );
    assert_eq!(tokens.selection.omitted, OmissionStatus::SafetyLimit);
    assert_eq!(tokens.content.units[0].text.as_deref(), Some("alpha beta"));
}

/// Verifies EPUB selection uses native spine items rather than chapters or flattened text.
#[test]
fn epub_spine_items_keep_independent_expected_text() {
    let report = selected("native.epub", NativeSelection::All);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(report.selection.available.value, Some(2));
    assert_eq!(
        report.content.units[0].text.as_deref(),
        Some("First chapter & evidence.\n")
    );
    assert_eq!(
        report.content.units[1].text.as_deref(),
        Some("Second chapter.\n")
    );

    let second = selected("native.epub", parse_pages("2-2").unwrap());
    assert_eq!(second.selection.returned_indices, [2]);
    assert_eq!(
        second.content.units[0].text.as_deref(),
        Some("Second chapter.\n")
    );
}

/// Verifies MOBI selection remains a decoded PalmDB text record.
#[test]
fn mobi_decoded_record_is_not_relabelled_as_a_chapter() {
    let report = selected("native.mobi", NativeSelection::All);
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    assert_eq!(
        report.selection.unit,
        glassbox_inspect::reports::NativeUnit::MobiTextRecord
    );
    assert_eq!(report.selection.available.value, Some(1));
    assert_eq!(
        report.content.units[0].text.as_deref(),
        Some("Native MOBI abcabc & evidence.\n")
    );
}

/// Verifies DOCX top-level blocks and PPTX slides preserve their independent text.
#[test]
fn office_blocks_and_slides_match_native_oracles() {
    let document = selected("notes.docx", NativeSelection::All);
    assert_eq!(document.selection.available.value, Some(3));
    assert_eq!(
        document
            .content
            .units
            .iter()
            .map(|unit| unit.text.as_deref().unwrap())
            .collect::<Vec<_>>(),
        [
            "This is a native text layer with an ampersand & evidence.",
            "Section one ends here.",
            "Scan",
        ]
    );

    let slides = selected("deck.pptx", parse_pages("2-2").unwrap());
    assert_eq!(slides.selection.returned_indices, [2]);
    assert_eq!(slides.content.units[0].text.as_deref(), Some("Scan\n"));
}

/// Verifies ODT section blocks and ODP slides use the same native-unit contract.
#[test]
fn open_document_blocks_and_slides_match_native_oracles() {
    let document = selected("notes.odt", NativeSelection::All);
    assert_eq!(document.selection.available.value, Some(2));
    assert_eq!(
        document.content.units[0].text.as_deref(),
        Some("Café et preuves ici.\tTexte  natif.")
    );
    assert_eq!(document.content.units[1].text.as_deref(), Some("Scan"));

    let slides = selected("deck.odp", parse_pages("1-1").unwrap());
    assert_eq!(slides.selection.returned_indices, [1]);
    assert_eq!(
        slides.content.units[0].text.as_deref(),
        Some("This is the first native ODF presentation slide.")
    );
}

/// Verifies PDF selection extracts physical pages and preserves native page text.
#[test]
fn pdf_ranges_select_physical_pages() {
    let full = selected("native.pdf", NativeSelection::All);
    assert_eq!(full.metadata.status, OperationStatus::Complete);
    assert_eq!(full.selection.available.value, Some(2));
    assert!(
        full.content.units[0]
            .text
            .as_deref()
            .unwrap()
            .contains("Native PDF evidence")
    );

    let second = selected("native.pdf", parse_pages("2-2").unwrap());
    assert_eq!(second.selection.returned_indices, [2]);
    assert_eq!(second.content.units.len(), 1);
    assert_eq!(second.content.units[0].unit_index, 2);
}

/// Verifies CSV/TSV remains a bounded preview with selectable dataset rows.
#[test]
fn csv_preview_ranges_preserve_rows_and_values() {
    let path = input(
        "csv",
        "rows.csv",
        "name,value\nalpha,1\nbeta,2\ngamma,3\ndelta,4\nepsilon,5\nzeta,6\n",
    );
    let report = extract(&path, &options(parse_pages("2-4,3").unwrap()));
    assert_eq!(report.selection.available.value, Some(5));
    assert_eq!(report.selection.returned_indices, [2, 3, 4]);
    assert_eq!(report.metadata.status, OperationStatus::Partial);
    assert_eq!(report.selection.omitted, OmissionStatus::SafetyLimit);
    assert_eq!(report.content.units[0].text.as_deref(), Some("beta\t2"));
    assert_eq!(report.content.units[0].rows[0].values, ["beta", "2"]);
    assert_eq!(
        report.selection.available.completeness,
        Completeness::Partial
    );
}

/// Verifies worksheet previews emit selected sheet units with their native values.
#[test]
fn worksheet_previews_are_not_empty_or_flattened_across_sheets() {
    for name in ["grid.xlsx", "grid.ods"] {
        let report = selected(name, NativeSelection::All);
        assert!(!report.content.units.is_empty(), "{name}");
        assert!(
            report
                .content
                .units
                .iter()
                .flat_map(|unit| unit.rows.iter())
                .flat_map(|row| row.values.iter())
                .any(|value| value.contains("We can inspect")),
            "{name}: {:?}",
            report.content.units
        );
    }
}
/// Verifies the public stage OCR recipe emits native-layout OCR provenance.
///
/// Runs only with a local English Tesseract model. The scanned page must contain
/// its independent raster text and OCR engine plus 3072-pixel provenance.
#[test]
#[ignore = "requires local eng.traineddata, set TESSDATA_PREFIX"]
fn public_pdf_ocr_emits_native_layout_provenance() {
    let directory = input("ocr", "placeholder.txt", "")
        .parent()
        .unwrap()
        .to_path_buf();
    let path = directory.join("scan.pdf");
    scanned_pdf(&path);
    let report = extract(&path, &ExtractOptions::default());
    assert_eq!(report.metadata.status, OperationStatus::Complete);
    let scanned = &report.content.units[0];
    assert!(
        scanned.text.as_deref().unwrap().contains("Native PDF"),
        "{:?}",
        report.content.units
    );
    assert!(scanned.provenance.iter().any(|provenance| {
        provenance
            .ocr
            .as_ref()
            .is_some_and(|ocr| ocr.engine == "tesseract" && ocr.raster_long_edge == Some(3072))
    }));
}

// Module: glassbox-inspect OOXML text-ownership integration tests.
// File: office_text_ownership.rs.
// Responsibility: Prove nested OOXML constructs emit each owned fragment
// exactly once through the public extraction APIs, with bounded accumulation
// and explicit partials on clipping.
use glassbox_inspect::reports::{
    Completeness, ExtractOptions, NativeSelection, NativeUnit, OcrOptions,
};
use glassbox_inspect::{InspectOptions, extract, extract_transitional};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The payload size of the nesting probes, matching the audited 30x
/// multiplication shapes at a compact scale.
const PAYLOAD_CHARS: usize = 2_000;
/// How many nested wrappers each probe stacks around the payload.
const NESTING_DEPTH: usize = 30;

/// Creates one unique test-owned directory under the ignored target tree.
///
/// Accepts a test label. Returns the created directory path. Filesystem
/// failures panic; checked-in fixtures and production data are untouched.
fn directory(label: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/office-text-ownership")
        .join(format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Writes string entries into a stored ZIP fixture.
///
/// Accepts a directory, file name, and entry name/body pairs. Returns the
/// fixture path; filesystem or ZIP encoding failures panic.
fn zip_fixture(directory: &Path, name: &str, entries: &[(&str, &str)]) -> PathBuf {
    let path = directory.join(name);
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for (name, body) in entries {
        zip.start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    path
}

/// Wraps `inner` in `depth` layers of the given element.
///
/// Accepts the element name and the innermost markup. Returns the nested
/// markup string; never fails.
fn nested(element: &str, depth: usize, inner: String) -> String {
    let mut value = inner;
    for _ in 0..depth {
        value = format!("<{element}>{value}</{element}>");
    }
    value
}

/// Builds a minimal single-slide PPTX whose slide nests `depth` `a:p`
/// paragraphs around one `a:t` payload.
///
/// Accepts a directory and nesting depth. Returns the fixture path.
fn nested_ap_pptx(directory: &Path, depth: usize) -> PathBuf {
    let inner = nested("a:p", depth, format!("<a:r><a:t>{}</a:t></a:r>", payload()));
    slide_fixture(directory, "nested-ap.pptx", &inner)
}

/// Builds a minimal single-slide PPTX whose slide nests `depth` `a:t` text
/// elements around one payload.
///
/// Accepts a directory and nesting depth. Returns the fixture path.
fn nested_at_pptx(directory: &Path, depth: usize) -> PathBuf {
    let inner = nested("a:t", depth, payload());
    slide_fixture(
        directory,
        "nested-at.pptx",
        &format!("<a:p><a:r>{inner}</a:r></a:p>"),
    )
}

/// Assembles the shared PPTX package parts around slide body markup.
///
/// Accepts a directory, file name, and the slide's `spTree` markup. Returns
/// the written fixture path; ZIP failures panic.
fn slide_fixture(directory: &Path, name: &str, sp_tree: &str) -> PathBuf {
    const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
    const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
    zip_fixture(
        directory,
        name,
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/\
                     vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "ppt/presentation.xml",
                &format!(
                    "<p:presentation xmlns:p=\"{P}\" xmlns:r=\"{R}\">\
                     <p:sldIdLst><p:sldId r:id=\"rId1\"/></p:sldIdLst></p:presentation>"
                ),
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                &format!(
                    "<Relationships xmlns=\"{PKG}\">\
                     <Relationship Id=\"rId1\" Type=\"{R}/slide\" Target=\"slides/slide1.xml\"/>\
                     </Relationships>"
                ),
            ),
            (
                "ppt/slides/slide1.xml",
                &format!(
                    "<p:sld xmlns:p=\"{P}\" xmlns:a=\"{A}\">\
                     <p:cSld><p:spTree>{sp_tree}</p:spTree></p:cSld></p:sld>"
                ),
            ),
        ],
    )
}

/// Builds a DOCX whose body nests `depth` `w:p` paragraphs and `depth` `w:t`
/// text elements around separate payloads.
///
/// Accepts a directory. Returns the fixture path with three body blocks:
/// a paragraph nesting `w:t` elements, a paragraph nesting `w:p` elements,
/// and a plain trailing paragraph.
fn nested_paragraphs_docx(directory: &Path) -> PathBuf {
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
    let nested_text = nested("w:t", NESTING_DEPTH, payload());
    let nested_p = nested(
        "w:p",
        NESTING_DEPTH,
        format!("<w:r><w:t>{}</w:t></w:r>", payload()),
    );
    zip_fixture(
        directory,
        "nested.docx",
        &[
            (
                "[Content_Types].xml",
                &format!(
                    "<Types xmlns=\"{CT}\">\
                     <Override PartName=\"/word/document.xml\" ContentType=\"application/\
                     vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
                     </Types>"
                ),
            ),
            (
                "word/document.xml",
                &format!(
                    "<w:document xmlns:w=\"{W}\"><w:body>\
                     <w:p><w:r>{nested_text}</w:r></w:p>\
                     <w:p>{nested_p}</w:p>\
                     <w:p><w:r><w:t>tail</w:t></w:r></w:p>\
                     </w:body></w:document>"
                ),
            ),
        ],
    )
}

/// Returns the probe payload: `PAYLOAD_CHARS` ASCII `A` characters.
fn payload() -> String {
    "A".repeat(PAYLOAD_CHARS)
}

/// Creates extraction controls with OCR disabled for deterministic tests.
///
/// Accepts a native selection. Returns default character/token controls with
/// the requested selection. No file, model, or network access occurs.
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

/// Counts how many times `needle` occurs in `haystack`.
fn count(haystack: &str, needle: char) -> usize {
    haystack.matches(needle).count()
}

/// Nested `a:p` paragraphs each emit only their own fragments: the slide unit
/// holds exactly one payload, not one per nesting level.
#[test]
fn pptx_nested_paragraphs_extract_payload_once() {
    let path = nested_ap_pptx(&directory("nested-ap"), NESTING_DEPTH);
    let report = extract(&path, &options(NativeSelection::All));
    assert_eq!(report.selection.available.value, Some(1));
    assert_eq!(report.content.units.len(), 1);
    let unit = &report.content.units[0];
    assert_eq!(unit.unit, NativeUnit::Slide);
    let text = unit.text.as_deref().expect("slide keeps its text");
    assert_eq!(count(text, 'A'), PAYLOAD_CHARS);
    assert_eq!(unit.completeness, Completeness::Exact);
}

/// Nested `a:t` text elements each contribute only their direct character
/// data: the slide unit holds exactly one payload, not one per level.
#[test]
fn pptx_nested_text_elements_extract_payload_once() {
    let path = nested_at_pptx(&directory("nested-at"), NESTING_DEPTH);
    let report = extract(&path, &options(NativeSelection::All));
    assert_eq!(report.content.units.len(), 1);
    let unit = &report.content.units[0];
    let text = unit.text.as_deref().expect("slide keeps its text");
    assert_eq!(count(text, 'A'), PAYLOAD_CHARS);
    assert_eq!(unit.completeness, Completeness::Exact);
}

/// A paragraph block nesting `w:t` elements extracts its payload exactly
/// once; a paragraph block nesting `w:p` blocks skips the nested content,
/// reports the omission as an explicit partial, and never multiplies it.
#[test]
fn docx_nested_paragraph_units_own_their_fragments_once() {
    let path = nested_paragraphs_docx(&directory("nested-docx"));
    let report = extract(&path, &options(NativeSelection::All));
    assert_eq!(report.selection.available.value, Some(3));
    let units = &report.content.units;
    assert_eq!(units.len(), 3);

    let nested_text = units[0].text.as_deref().expect("block keeps text");
    assert_eq!(count(nested_text, 'A'), PAYLOAD_CHARS);
    assert_eq!(units[0].completeness, Completeness::Exact);

    let nested_block = units[1].text.as_deref().expect("block keeps text");
    assert_eq!(count(nested_block, 'A'), 0);
    assert!(nested_block.ends_with("[unit text truncated]"));
    assert_eq!(units[1].completeness, Completeness::Partial);

    assert_eq!(units[2].text.as_deref(), Some("tail"));
    assert_eq!(units[2].completeness, Completeness::Exact);
}

/// The legacy Word text body streams each nested fragment exactly once: both
/// payloads survive, counts stay linear, and no clip is reported.
#[test]
fn docx_legacy_text_emits_nested_fragments_once() {
    let path = nested_paragraphs_docx(&directory("nested-docx-legacy"));
    let report = extract_transitional(&path, &InspectOptions::default());
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let text = &report.text.content;
    assert_eq!(count(text, 'A'), 2 * PAYLOAD_CHARS);
    assert_eq!(report.counts.chars, text.chars().count());
    assert!(!report.text.truncated);
    assert_eq!(report.counts.paragraphs, Some(NESTING_DEPTH + 3));
}

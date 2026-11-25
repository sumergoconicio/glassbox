// Module: glassbox-inspect integration tests.
// File: comics.rs.
// Responsibility: Exercise comic archive preview ordering without changing reader behavior.
use glassbox_inspect::{InspectOptions, extract_transitional as inspect};
use std::{io::Write, path::PathBuf};

/// Returns the checked-in fixture path for `name`.
///
/// Accepts a fixture filename relative to `extensionalities/fixtures`.
/// Returns its path without reading the file; missing fixtures fail in
/// downstream reader calls.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Creates a unique scratch directory under `target/tests` for one test case.
///
/// Accepts a short descriptive prefix. Returns the newly created directory path
/// or panics if directory creation fails.
fn scratch_dir(prefix: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes an uncompressed ZIP archive at `path` with the given `(name, bytes)` entries.
///
/// Accepts the target file path and entry slice. Returns unit after finishing
/// the ZIP central directory, or panics on I/O or ZIP writer errors.
fn write_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, bytes) in entries {
        writer
            .start_file(
                *name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn cbz_dimensions_follow_archive_order_not_filename_sorting() {
    let r = inspect(
        &fixture("pages.cbz"),
        &InspectOptions {
            max_units: 1,
            ..Default::default()
        },
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let v = serde_json::to_value(r).unwrap();
    assert_eq!(v["format"]["bucket"], "Comics");
    assert_eq!(v["counts"]["pages"], 2);
    assert_eq!(v["pages"][0]["source_part"], "02.png");
    assert_eq!(v["pages"][0]["width"], 64);
    assert_eq!(v["pages"][1]["height"], 80);
}

#[test]
fn cbr_walks_rar4_and_rar5_without_decompressing_pages() {
    for name in ["pages4.cbr", "pages5.cbr"] {
        let r = inspect(&fixture(name), &Default::default());
        assert_eq!(r.counts.pages, Some(2), "{name}: {:?}", r.errors);
        assert_eq!(r.entries[0].name, "02.png");
        let v = serde_json::to_value(r).unwrap();
        assert_eq!(v["format"]["bucket"], "Comics");
        assert_eq!(v["pages"][0]["width"], serde_json::Value::Null);
        assert!(v["thumbnail"].is_null());
        assert!(
            v["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["message"].as_str().unwrap().contains("no decompression"))
        );
    }
    for name in ["truncated.cbr", "encrypted.cbr"] {
        let r = inspect(&fixture(name), &Default::default());
        assert!(
            r.errors.iter().any(|e| e.stage == "rar"),
            "{name}: {:?}",
            r.errors
        );
    }
}

/// Verifies a generic ZIP renamed `.cbz` identifies as a generic ZIP without comic errors.
///
/// Accepts no parameters. Returns unit after inspecting a stored ZIP containing
/// only `readme.txt` named `generic.cbz` through `extract_transitional`. Panics
/// if format bucket, byte basis, extension disagreement, or error stages diverge.
#[test]
fn generic_zip_named_cbz_falls_back_to_generic_zip_without_comic_errors() {
    let dir = scratch_dir("comics-generic-cbz");
    let path = dir.join("generic.cbz");
    write_zip(&path, &[("readme.txt", b"plain archive text")]);

    let report = inspect(&path, &InspectOptions::default());
    assert_eq!(report.format.bucket, "Archives");
    assert_eq!(report.format.basis, "bytes");
    assert!(report.format.disagreement);
    assert!(
        !report
            .errors
            .iter()
            .any(|e| matches!(e.stage, "comic" | "comic_page" | "iwork_preview")),
        "unexpected specialized archive error: {:?}",
        report.errors
    );
}

/// Verifies spoofed DOCX and EPUB packages named `.cbz` short-circuit before CBZ inspection.
///
/// Accepts no parameters. Returns unit after copying `notes.docx` and
/// `native.epub` to `spoof.cbz` paths and inspecting both through
/// `extract_transitional`. Panics if format bucket, extension disagreement,
/// extracted DOCX text, or error stages diverge.
#[test]
fn spoofed_docx_and_epub_named_cbz_short_circuit_before_cbz_reader() {
    let docx_dir = scratch_dir("comics-spoof-docx");
    let spoof_docx = docx_dir.join("spoof.cbz");
    std::fs::copy(fixture("notes.docx"), &spoof_docx).unwrap();
    let docx_report = inspect(&spoof_docx, &InspectOptions::default());
    assert_eq!(docx_report.format.bucket, "Office Docs");
    assert_eq!(docx_report.format.basis, "bytes");
    assert!(docx_report.format.disagreement);
    assert!(
        docx_report
            .text
            .content
            .contains("This is a native text layer with an ampersand & evidence."),
        "expected DOCX text, got: {:?}",
        docx_report.text.content
    );
    assert!(
        !docx_report
            .errors
            .iter()
            .any(|e| matches!(e.stage, "comic" | "comic_page" | "iwork_preview")),
        "unexpected specialized archive error: {:?}",
        docx_report.errors
    );

    let epub_dir = scratch_dir("comics-spoof-epub");
    let spoof_epub = epub_dir.join("spoof.cbz");
    std::fs::copy(fixture("native.epub"), &spoof_epub).unwrap();
    let epub_report = inspect(&spoof_epub, &InspectOptions::default());
    assert_eq!(epub_report.format.bucket, "Ebooks");
    assert_eq!(epub_report.format.basis, "bytes");
    assert!(epub_report.format.disagreement);
    assert!(
        !epub_report
            .errors
            .iter()
            .any(|e| matches!(e.stage, "comic" | "comic_page")),
        "unexpected comic error: {:?}",
        epub_report.errors
    );
}

/// Verifies a `.cbz` with unreadable candidate image entries still emits a comic diagnostic.
///
/// Accepts no parameters. Returns unit after inspecting a `.cbz` containing a
/// corrupt `.png` entry through `extract_transitional`. Panics if the reader
/// fails to record the expected `"comic"` diagnostic.
#[test]
fn cbz_with_unreadable_image_entry_retains_comic_error() {
    let dir = scratch_dir("comics-corrupt-cbz");
    let path = dir.join("broken.cbz");
    write_zip(&path, &[("01.png", b"not a valid png")]);

    let report = inspect(&path, &InspectOptions::default());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.stage == "comic" && e.message.contains("no readable image pages")),
        "expected comic error, got: {:?}",
        report.errors
    );
}

use litx_inspect::{InspectOptions, inspect};
use std::{io::Write, path::PathBuf};

fn archive(extension: &str, entries: &[(&str, &[u8])]) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "archive-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("sample.{extension}"));
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
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
    path
}

#[test]
fn zip_markdown_lists_file_count_and_names_without_reading_contents() {
    let path = archive(
        "zip",
        &[
            ("one.txt", b"PRIVATE_CONTENT"),
            ("folder/two.md", b"ALSO_PRIVATE"),
        ],
    );
    let r = inspect(&path, &InspectOptions::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.counts.entries, Some(2));
    assert!(
        r.markdown.content.contains("Entries: 2"),
        "{}",
        r.markdown.content
    );
    assert!(r.markdown.content.contains("one.txt"));
    assert!(r.markdown.content.contains("folder/two.md"));
    assert!(!r.markdown.content.contains("PRIVATE"));
    assert!(r.text.content.is_empty());
}

#[test]
fn iwork_pdf_is_extracted_as_an_explicitly_partial_derived_preview() {
    let bytes =
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/native.pdf"))
            .unwrap();
    for extension in ["pages", "key"] {
        let path = archive(
            extension,
            &[
                ("Index/Document.iwa", b"NOT_DECODED"),
                ("QuickLook/Preview.pdf", &bytes),
            ],
        );
        let r = inspect(&path, &InspectOptions::default());
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.text.content.contains("Native PDF"), "{}", r.text.content);
        let value = serde_json::to_value(&r).unwrap();
        assert_eq!(
            value["derived_preview"]["source_part"],
            "QuickLook/Preview.pdf"
        );
        assert_eq!(value["derived_preview"]["partial"], true);
        assert!(
            r.markdown.content.contains("Partial iWork preview"),
            "{}",
            r.markdown.content
        );
        assert!(r.markdown.truncated);
    }
}

#[test]
fn numbers_pdf_preview_extracts_text_but_stays_within_one_page() {
    let bytes =
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/native.pdf"))
            .unwrap();
    let path = archive(
        "numbers",
        &[
            ("Index/Document.iwa", b"NOT_DECODED"),
            ("QuickLook/Preview.pdf", &bytes),
        ],
    );
    let r = inspect(
        &path,
        &InspectOptions {
            max_pages: None,
            max_chars: 64 << 20,
            ..Default::default()
        },
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(
        r.text.content.contains("Native PDF"),
        "Numbers should expose bounded preview text: {}",
        r.text.content
    );
    assert!(
        r.markdown.content.contains("Bounded embedded preview"),
        "{}",
        r.markdown.content
    );
    assert_eq!(r.pdf.unwrap().geometry.len(), 1);
    assert!(r.text.chars <= 4000);
    assert!(r.derived_preview.unwrap().partial);
}

#[test]
fn iwork_image_preview_retains_provenance_and_preserves_thumbnail_bytes() {
    let bytes =
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/image.png"))
            .unwrap();
    let path = archive(
        "pages",
        &[
            ("Index/Document.iwa", b"NOT_DECODED"),
            ("preview.png", &bytes),
        ],
    );
    let output = path.with_extension("thumbnail.png");
    let r = inspect(
        &path,
        &InspectOptions {
            thumbnail: Some(output.clone()),
            thumbnail_max_edge: 1024,
            ..Default::default()
        },
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let value = serde_json::to_value(&r).unwrap();
    assert_eq!(value["derived_preview"]["source_part"], "preview.png");
    assert_eq!(value["derived_preview"]["partial"], true);
    assert_eq!(r.image.unwrap().width, 640);
    assert_eq!(r.thumbnail.unwrap().basis, "embedded");
    assert_eq!(std::fs::read(output).unwrap(), bytes);
    assert!(r.markdown.content.contains("Partial iWork preview"));
}

#[test]
fn iwork_text_preview_is_bounded_and_labeled_as_derived_text() {
    let bytes = "preview words ".repeat(1000);
    let path = archive("pages", &[("QuickLook/Preview.txt", bytes.as_bytes())]);
    let r = inspect(
        &path,
        &InspectOptions {
            max_chars: 64 << 20,
            max_pages: None,
            ..Default::default()
        },
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(r.text.content.starts_with("preview words"));
    assert_eq!(r.text.chars, 4000);
    assert_eq!(r.derived_preview.unwrap().content_type, "text/plain");
    assert!(r.markdown.truncated);
}

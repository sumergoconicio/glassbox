use litx_inspect::{InspectOptions, inspect};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

#[test]
fn native_pdf_keeps_full_page_counts_and_normalizes_upstream_preflight() {
    let r = inspect(
        &fixture("native.pdf"),
        &InspectOptions {
            max_chars: 12,
            max_units: 2,
            ..Default::default()
        },
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.counts.pages, Some(2));
    assert!(r.quick_look().contains("2 pages"));
    assert_eq!(r.text.chars, 12);
    assert!(r.text.content.starts_with("Native PDF"));
    assert!(r.text.truncated);
    assert!(r.counts.chars > 12);
    assert_eq!(r.preflight.len(), 2);
    assert!(r.preflight[0].needs_ocr);
    assert_eq!(r.preflight[0].reasons, ["sparse-text"]);
    assert!(r.preflight[1].needs_ocr);
    assert!(r.preflight[1].coverage_available);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["pdf"]["complexity"][1]["full_page_image"], true);
    assert_eq!(v["pdf"]["text_complete"], true);
    assert!(
        r.claims.iter().any(|c| c.key == "title"
            && c.value == "Native PDF"
            && c.source == "container_metadata")
    );
}

#[test]
fn pdf_thumbnail_is_opt_in_bounded_and_refuses_to_overwrite() {
    let path = fixture("../target/tests").join(format!(
        "pdf-preview-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let options = InspectOptions {
        thumbnail: Some(path.clone()),
        thumbnail_max_edge: 160,
        max_units: 0,
        ..Default::default()
    };
    let r = inspect(&fixture("native.pdf"), &options);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let thumb = r.thumbnail.unwrap();
    assert_eq!((thumb.width, thumb.height), (120, 160));
    assert_eq!(thumb.basis, "rendered");
    assert_eq!(r.counts.pages, Some(2));
    assert!(r.preflight.is_empty());
    let first = std::fs::read(&path).unwrap();
    let repeated = inspect(&fixture("native.pdf"), &options);
    assert!(repeated.errors.iter().any(|e| e.stage == "thumbnail"));
    assert_eq!(first, std::fs::read(path).unwrap());
}

#[test]
fn byte_verified_pdf_works_inside_an_existing_async_runtime() {
    let path = fixture("../target/tests").join(format!(
        "renamed-pdf-{}.bin",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, std::fs::read(fixture("native.pdf")).unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let r = runtime.block_on(async {
        inspect(
            &path,
            &InspectOptions {
                max_units: 0,
                ..Default::default()
            },
        )
    });
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.counts.pages, Some(2));
    assert!(r.preflight.is_empty());
    assert!(r.pdf.unwrap().text_complete);
    assert!(r.text.content.contains("Native PDF"));
    let broken = inspect(&fixture("broken.pdf"), &Default::default());
    assert!(broken.errors.iter().any(|e| e.stage == "pdf"));
}

#[test]
fn at_least_five_pages_are_checked_while_full_six_page_counts_survive_preview_limits() {
    let r = inspect(
        &fixture("multipage.pdf"),
        &InspectOptions {
            max_chars: 12,
            ..Default::default()
        },
    );
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.counts.pages, Some(6));
    assert_eq!(r.preflight.len(), 5);
    for (index, page) in r.preflight.iter().enumerate() {
        assert_eq!(page.page_number, index + 1);
        assert!(page.coverage_available);
    }
    assert_eq!(r.preflight[1].reasons, ["scanned"]);
    assert!(!r.preflight[2].needs_ocr);
    assert_eq!(r.preflight[3].reasons, ["no-text"]);
    assert_eq!(r.text.chars, 12);
    assert!(r.text.truncated);
    let pdf = r.pdf.as_ref().unwrap();
    assert!(pdf.text_complete);
    assert_eq!(pdf.geometry.len(), 6);
    assert_eq!(pdf.complexity.len(), 5);
    assert_eq!(pdf.geometry[5].width_points, 800.0);
    assert_eq!(pdf.geometry[5].height_points, 600.0);
    let full = inspect(
        &fixture("multipage.pdf"),
        &InspectOptions {
            max_chars: 4096,
            max_units: usize::MAX,
            ..Default::default()
        },
    );
    assert!(full.errors.is_empty(), "{:?}", full.errors);
    assert_eq!(full.preflight.len(), 6);
    assert_eq!(r.counts.chars, full.counts.chars);
    assert!(
        full.text
            .content
            .contains("Final page confirms full-document extraction.")
    );
}

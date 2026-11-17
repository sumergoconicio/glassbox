use litx_inspect::inspect;
use std::path::PathBuf;
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

#[test]
fn djvu_shared_dictionary_is_not_counted_as_a_page() {
    let r = inspect(&fixture("bundled.djvu"), &Default::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let v = serde_json::to_value(r).unwrap();
    assert_eq!(v["counts"]["pages"], 2);
    assert_eq!(v["pages"][0]["width"], 600);
    assert_eq!(v["pages"][1]["height"], 1200);
    assert_eq!(v["pages"][0]["dpi"], 300);
    assert_eq!(v["pages"][0]["version"], "0.26");
    let single = inspect(&fixture("single.djvu"), &Default::default());
    assert_eq!(single.counts.pages, Some(1));
    let indirect = inspect(&fixture("indirect.djvu"), &Default::default());
    assert_eq!(indirect.counts.pages, None);
    assert!(indirect.errors.iter().any(|e| e.message.contains("BZZ")));
    let truncated = inspect(&fixture("truncated.djvu"), &Default::default());
    assert!(truncated.errors.iter().any(|e| e.stage == "djvu"));
}

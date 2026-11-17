use std::process::Command;

#[test]
fn inspect_emits_a_report_and_optional_packet() {
    let output = Command::new(env!("CARGO_BIN_EXE_litx"))
        .args([
            "inspect",
            "../../fixtures/hello.txt",
            "--packet",
            "--pretty",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["counts"]["bytes"],
        include_bytes!("../../../fixtures/hello.txt").len()
    );
    assert_eq!(report["packet"]["Format"], "Text");
    assert!(report["claims"].is_array());
    assert!(report["errors"].is_array());
    assert!(report["packet"].get("Domains").is_none());
}

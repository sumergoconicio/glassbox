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

#[test]
fn missing_local_ocr_model_keeps_native_metrics_without_downloads() {
    let missing = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "missing-tessdata-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    assert!(!missing.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_litx"))
        .args([
            "inspect",
            "../../fixtures/document-image.png",
            "--probe-ocr",
        ])
        .env("TESSDATA_PREFIX", &missing)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["image"]["classification"], "document_scan_like");
    assert!(report["image"]["probe"].is_null());
    assert!(report["errors"].as_array().unwrap().iter().any(|e| {
        e["stage"] == "probe_ocr"
            && e["message"]
                .as_str()
                .unwrap()
                .contains("local eng.traineddata")
    }));
    assert!(!missing.exists());
}

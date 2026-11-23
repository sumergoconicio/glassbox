// Module: glassbox-inspect integration tests.
// File: optional.rs.
// Responsibility: Exercise optional Magika runtime degradation without changing reader behavior.
#[cfg(feature = "magika")]
#[test]
fn missing_onnx_runtime_degrades_the_report_without_downloading() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return;
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hello.txt");
    let report = glassbox_inspect::extract_transitional(&path, &Default::default());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.stage == "magika" && e.message.contains("ORT_DYLIB_PATH"))
    );
    assert_eq!(report.format.detector, "file-format");
}

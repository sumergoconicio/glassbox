// Module: glassbox-inspect integration tests.
// File: optional.rs.
// Responsibility: Verify Magika 2 CPU detection and coexistence with OCRia.

/// Check repeated and concurrent requests without a Magika ONNX dependency.
#[cfg(feature = "magika")]
#[test]
fn magika_cpu_requests_preserve_verified_identity() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hello.txt");
    let check = || {
        let report = glassbox_inspect::extract_transitional(&path, &Default::default());
        assert!(
            report.errors.iter().all(|error| error.stage != "magika"),
            "{:?}",
            report.errors
        );
        assert_eq!(report.format.detector, "file-format");
        assert!(
            report
                .claims
                .iter()
                .any(|claim| claim.key == "magika_detection"
                    && claim.value.contains("engine=magika-2.0.0-rc.0"))
        );
    };
    check();
    check();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(check);
        }
    });
}

/// Check Magika before and after OCRia loads and performs local inference.
#[cfg(feature = "magika")]
#[test]
#[ignore = "requires local OCRia models and ONNX Runtime; no downloads"]
fn magika_and_ocria_coexist_in_one_process() {
    use liteparse::ocr::ocria::{OcriaEngine, OcriaOptions};
    let models = std::path::PathBuf::from(
        std::env::var_os("GLASSBOX_TEST_OCRIA_MODEL_DIR")
            .expect("set GLASSBOX_TEST_OCRIA_MODEL_DIR to local models"),
    );
    let runtime = std::path::PathBuf::from(
        std::env::var_os("ORT_DYLIB_PATH")
            .expect("set ORT_DYLIB_PATH to an existing local runtime"),
    );
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hello.txt");
    let check = || {
        let report = glassbox_inspect::extract_transitional(&path, &Default::default());
        assert!(
            report.errors.iter().all(|error| error.stage != "magika"),
            "{:?}",
            report.errors
        );
        assert!(
            report
                .claims
                .iter()
                .any(|claim| claim.key == "magika_detection")
        );
    };
    check();
    let engine = OcriaEngine::load(
        &models,
        Some(&runtime),
        OcriaOptions {
            threads: 2,
            ..OcriaOptions::default()
        },
    )
    .expect("OCRia shares the process with Magika");
    let pixels = vec![255; 128 * 128 * 3];
    let result = engine
        .run(&pixels, 128, 128)
        .expect("OCRia inference succeeds");
    assert!(result.is_empty(), "a blank image contains no text");
    check();
}

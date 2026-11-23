// Module: glassbox-cli OCR language integration tests.
// File: ocr_language_cli.rs.
// Responsibility: Verify --ocr-language parsing, defaults, and local OCR flow through both binaries.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BINARIES: [&str; 2] = [env!("CARGO_BIN_EXE_glassbox"), env!("CARGO_BIN_EXE_litx")];

/// Creates one unique private test directory under the ignored target tree.
///
/// Accepts a test label. Returns a new mode-0700 directory path. Creation or
/// permission failures panic; no checked-in fixture or prior file is removed.
fn workdir(label: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/ocr-language-cli")
        .join(format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&path).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    path
}

/// Runs one Glassbox binary with an optional isolated tessdata override.
///
/// Accepts the binary path, argument strings, and a tessdata directory that
/// may not exist; `None` keeps the environment's normal model search. Returns
/// captured process output. Launch failures panic.
fn run(binary: &str, args: &[&str], tessdata: Option<&Path>) -> Output {
    let mut command = Command::new(binary);
    command.args(args).current_dir(env!("CARGO_MANIFEST_DIR"));
    if let Some(path) = tessdata {
        command.env("TESSDATA_PREFIX", path);
    }
    command.output().unwrap()
}

/// Parses a canonical report and includes stderr when JSON decoding fails.
///
/// Accepts captured output. Returns a JSON value. Malformed stdout panics
/// with the process diagnostic instead of hiding the failed command.
fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: exit={:?} stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Verifies the OCR language default and spellings reach local model checks.
///
/// Accepts no parameters and exercises the tracked image fixture against an
/// empty isolated tessdata directory. Returns unit after checking the default
/// `eng` and alias `en` spellings both fail on the canonical `eng.traineddata`
/// file, the explicit `zz` spelling fails on `zz.traineddata`, each report
/// keeps its native image unit unclaimed with a `probe_ocr` diagnostic and
/// exit 2, and both binary names emit the same report. A wrong model file in
/// a diagnostic, a claimed OCR provenance, a different exit, or divergent
/// alias output panics.
#[test]
fn ocr_language_spellings_reach_local_model_checks_offline() {
    let tessdata = workdir("offline").join("tessdata");
    std::fs::create_dir_all(&tessdata).unwrap();
    let fixture = "../../fixtures/image.png";
    let mut aliased_report = None;
    for (label, arguments, expected_model) in [
        (
            "default eng",
            vec!["extract", fixture] as Vec<&str>,
            "eng.traineddata",
        ),
        (
            "alias en",
            vec!["extract", fixture, "--ocr-language", "en"],
            "eng.traineddata",
        ),
        (
            "explicit eng",
            vec!["extract", fixture, "--ocr-language", "eng"],
            "eng.traineddata",
        ),
        (
            "explicit zz",
            vec!["extract", fixture, "--ocr-language", "zz"],
            "zz.traineddata",
        ),
    ] {
        let output = run(BINARIES[0], &arguments, Some(&tessdata));
        assert_eq!(output.status.code(), Some(2), "{label}: {output:?}");
        let report = json(&output);
        assert_eq!(report["metadata"]["status"], "partial", "{label}");
        assert!(
            report["metadata"]["diagnostics"]
                .as_array()
                .expect("diagnostics array")
                .iter()
                .any(|diagnostic| diagnostic["code"] == "probe_ocr"
                    && diagnostic["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(expected_model))),
            "{label}: {}",
            report["metadata"]["diagnostics"]
        );
        let unit = &report["content"]["units"][0];
        assert_eq!(unit["provenance"][0]["origin"], "native", "{label}");
        assert!(unit["provenance"][0]["ocr"].is_null(), "{label}");
        if label == "alias en" {
            aliased_report = Some(report);
        }
    }
    let alias = run(
        BINARIES[1],
        &["extract", fixture, "--ocr-language", "en"],
        Some(&tessdata),
    );
    assert_eq!(alias.status.code(), Some(2));
    assert_eq!(aliased_report.expect("alias report"), json(&alias));
}

/// Verifies an invalid OCR language fails closed without losing native content.
///
/// Accepts no parameters and exercises the tracked image fixture with one
/// non-ASCII language spelling. Returns unit after checking the report keeps
/// its native image unit unclaimed, records a `probe_ocr` diagnostic naming
/// the invalid model name, and exits 2. A claimed OCR provenance, a lost
/// diagnostic, or a different exit panics.
#[test]
fn invalid_ocr_language_fails_closed_without_losing_content() {
    let tessdata = workdir("invalid").join("tessdata");
    std::fs::create_dir_all(&tessdata).unwrap();
    let output = run(
        BINARIES[0],
        &[
            "extract",
            "../../fixtures/image.png",
            "--ocr-language",
            "eñ",
        ],
        Some(&tessdata),
    );
    assert_eq!(output.status.code(), Some(2));
    let report = json(&output);
    assert_eq!(report["metadata"]["status"], "partial");
    assert!(
        report["metadata"]["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "probe_ocr"
                && diagnostic["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("invalid local OCR language"))),
        "{}",
        report["metadata"]["diagnostics"]
    );
    let unit = &report["content"]["units"][0];
    assert_eq!(unit["provenance"][0]["origin"], "native");
    assert!(unit["provenance"][0]["ocr"].is_null());
}

/// Verifies the OCR language leaves extraction without OCR untouched.
///
/// Accepts no parameters and exercises the tracked text fixture. Returns unit
/// after checking one enabled and one disabled OCR request both complete with
/// exit 0 while the disabled request claims no OCR provenance. A broken
/// ordinary extraction or a claimed provenance panics.
#[test]
fn ocr_language_leaves_non_ocr_extraction_untouched() {
    let tessdata = workdir("text").join("tessdata");
    std::fs::create_dir_all(&tessdata).unwrap();
    let enabled = run(
        BINARIES[0],
        &[
            "extract",
            "../../fixtures/hello.txt",
            "--ocr-language",
            "en",
        ],
        Some(&tessdata),
    );
    assert_eq!(enabled.status.code(), Some(0));
    let enabled = json(&enabled);
    assert_eq!(enabled["metadata"]["status"], "complete");
    let disabled = run(
        BINARIES[0],
        &[
            "extract",
            "../../fixtures/hello.txt",
            "--no-ocr",
            "--ocr-language",
            "en",
        ],
        Some(&tessdata),
    );
    assert_eq!(disabled.status.code(), Some(0));
    let disabled = json(&disabled);
    assert_eq!(disabled["metadata"]["status"], "complete");
    assert!(disabled["content"]["units"][0]["provenance"][0]["ocr"].is_null());
}

/// Verifies the alias spelling recognizes text and reports canonical facts.
///
/// Accepts no parameters and renders the tracked native PDF page to a raster,
/// then extracts it with `--ocr-language en`. Returns unit after checking the
/// image unit origin turns OCR, the provenance reports the canonical `eng`
/// language with a Tesseract engine, observed confidence, and raster edge,
/// and the recognized text contains the rendered marker. A missing OCR claim,
/// a non-canonical language, or lost recognized text panics.
#[test]
#[ignore = "requires local eng.traineddata, set TESSDATA_PREFIX"]
fn english_alias_ocr_uses_local_model_and_reports_canonical_language() {
    let directory = workdir("canonical");
    let raster = directory.join("text.png");
    let rendered = run(
        BINARIES[0],
        &[
            "inspect",
            "../../fixtures/native.pdf",
            "--thumbnail",
            raster.to_str().unwrap(),
            "--thumbnail-max-edge",
            "800",
        ],
        None,
    );
    assert_eq!(rendered.status.code(), Some(0), "{rendered:?}");
    let extracted = run(
        BINARIES[0],
        &["extract", raster.to_str().unwrap(), "--ocr-language", "en"],
        None,
    );
    assert_eq!(extracted.status.code(), Some(0), "{extracted:?}");
    let report = json(&extracted);
    assert_eq!(report["metadata"]["status"], "complete");
    assert!(
        !report["metadata"]["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "probe_ocr"),
        "{}",
        report["metadata"]["diagnostics"]
    );
    let unit = &report["content"]["units"][0];
    assert_eq!(unit["provenance"][0]["origin"], "ocr");
    let facts = &unit["provenance"][0]["ocr"];
    assert_eq!(facts["engine"], "tesseract");
    assert_eq!(facts["language"], "eng");
    assert!(facts["mean_confidence"].as_f64().unwrap() > 0.5);
    assert_eq!(facts["raster_long_edge"], 3072);
    assert!(
        unit["text"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("native pdf"),
        "{}",
        unit["text"]
    );
}

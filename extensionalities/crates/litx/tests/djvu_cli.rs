// Module: glassbox-cli DjVu boundary tests.
// File: djvu_cli.rs.
// Responsibility: Prove v2 identification-only inspection and unchanged explicit legacy extraction boundaries.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const DIAGNOSTIC: &str = "DjVu extraction is unsupported; identification only";

/// Returns an IFF chunk with a big-endian length and required padding byte.
///
/// Accepts a four-byte identifier and payload. Returns encoded bytes. The small
/// deterministic fixture cannot overflow the length conversion.
fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = id.to_vec();
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        bytes.push(0);
    }
    bytes
}

/// Builds a standalone DjVu fixture containing forbidden native text.
///
/// Accepts no parameters. Returns valid DjVu bytes with a TXTa marker. No file
/// I/O occurs; the hidden marker must never appear in either CLI report family.
fn native_text_document() -> Vec<u8> {
    let text = b"HIDDEN_CLI_NATIVE_TEXT";
    let mut text_payload = (text.len() as u32).to_be_bytes()[1..].to_vec();
    text_payload.extend_from_slice(text);
    text_payload.push(0);
    let info: &[u8] = &[2, 88, 3, 32, 26, 0, 44, 1, 22, 1];
    let mut page = b"DJVU".to_vec();
    page.extend(chunk(b"INFO", info));
    page.extend(chunk(b"TXTa", &text_payload));
    let mut bytes = b"AT&T".to_vec();
    bytes.extend(chunk(b"FORM", &page));
    bytes
}

/// Returns the stable ignored fixture directory for DjVu CLI checks.
///
/// Accepts no parameters. Returns the target path. No mutation occurs until the
/// caller creates the directory or its deterministic test fixture.
fn fixture_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/verification/djvu-identification-only/fixtures/cli")
}

/// Writes the native-text DjVu fixture under the authorized target directory.
///
/// Accepts no parameters. Returns the fixture path. Filesystem errors panic;
/// only the deterministic test-owned fixture is replaced on repeated runs.
fn native_text_fixture() -> PathBuf {
    let directory = fixture_directory();
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("native-text.djvu");
    std::fs::write(&path, native_text_document()).unwrap();
    path
}

/// Runs Glassbox with an optional isolated tessdata override.
///
/// Accepts arguments and an optional path. Returns captured process output.
/// Spawn failures panic; assertions explicitly validate each expected exit code.
fn run_litx(args: &[&str], tessdata: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_glassbox"));
    command
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = tessdata {
        command.env("TESSDATA_PREFIX", path);
    }
    command.output().unwrap()
}

/// Parses a report while preserving process diagnostics on malformed output.
///
/// Accepts captured output. Returns a JSON value. This helper does not require
/// exit 0 because v2 identification-only inspection deliberately exits 2.
fn report_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)))
}

/// Asserts canonical extraction retains its static DjVu boundary.
///
/// Accepts a v2 extraction JSON report. Returns unit after checking empty native
/// units and the static diagnostic. Native text leakage or OCR stages panic.
fn assert_extract_boundary(report: &Value) {
    assert_eq!(report["kind"], "extract");
    assert_eq!(report["metadata"]["format"]["bucket"], "Ebooks");
    assert_eq!(
        report["metadata"]["format"]["detected_mime"],
        "image/vnd.djvu"
    );
    assert_eq!(report["metadata"]["status"], "unsupported");
    let errors = report["metadata"]["diagnostics"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0]["stage"], "metadata");
    assert_eq!(errors[0]["message"], DIAGNOSTIC);
    assert!(report["metadata"]["djvu"].is_null());
    assert_eq!(report["content"]["units"], serde_json::json!([]));
    assert_eq!(report["selection"]["omitted"], "unsupported");
    assert!(
        !serde_json::to_string(report)
            .unwrap()
            .contains("HIDDEN_CLI_NATIVE_TEXT")
    );
}

/// Asserts the v2 report identifies DjVu without native analysis or content.
///
/// Accepts a metadata JSON report. Returns unit after checking schema, support,
/// status, diagnostics, digest, and hidden marker absence. Body fields panic.
fn assert_metadata_boundary(report: &Value) {
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["kind"], "inspect");
    assert_eq!(report["metadata"]["format"]["bucket"], "Ebooks");
    assert_eq!(
        report["metadata"]["format"]["detected_mime"],
        "image/vnd.djvu"
    );
    assert_eq!(
        report["metadata"]["extraction_support"],
        "identification_only"
    );
    assert_eq!(report["metadata"]["status"], "complete");
    assert_eq!(
        report["metadata"]["diagnostics"][0]["code"],
        "djvu_identification_only"
    );
    assert_eq!(
        report["metadata"]["digest"]["digest"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(report.get("text").is_none());
    assert!(report.get("markdown").is_none());
    assert!(
        !serde_json::to_string(report)
            .unwrap()
            .contains("HIDDEN_CLI_NATIVE_TEXT")
    );
}

/// Verifies inspect v2, legacy extract, rejected flags, and thumbnails share the boundary.
///
/// Accepts no parameters. Returns unit after running every CLI route. DjVu text
/// decoding, OCR initialization, incorrect exit codes, or thumbnail creation panics.
#[test]
fn all_single_document_cli_modes_are_identification_only() {
    let path = native_text_fixture();
    let path = path.to_str().unwrap();
    let missing = fixture_directory().join("missing-tessdata");
    assert!(!missing.exists());

    let inspected = run_litx(&["inspect", path], Some(&missing));
    assert_eq!(inspected.status.code(), Some(0));
    assert_metadata_boundary(&report_json(&inspected));

    for args in [vec!["extract", "--no-ocr", path], vec!["extract", path]] {
        let output = run_litx(&args, Some(&missing));
        assert_eq!(output.status.code(), Some(2));
        assert_extract_boundary(&report_json(&output));
    }
    for flag in ["--identify", "--probe-ocr"] {
        let output = run_litx(&["inspect", flag, path], Some(&missing));
        assert_eq!(output.status.code(), Some(64));
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("glassbox extract PATH")
        );
    }

    let thumbnail = fixture_directory().join("forbidden-cli-thumbnail.png");
    let output = run_litx(
        &["inspect", "--thumbnail", thumbnail.to_str().unwrap(), path],
        Some(&missing),
    );
    assert_eq!(output.status.code(), Some(0));
    assert_metadata_boundary(&report_json(&output));
    assert!(!thumbnail.exists(), "DjVu thumbnail must not be created");
    assert!(!missing.exists(), "DjVu must not initialize an OCR model");
}

/// Verifies persistent legacy extraction retains its boundary and recovers.
///
/// Accepts no parameters. Returns unit after DjVu then plain-text batch responses.
/// Session poisoning, native text leakage, or ordinary-text loss causes a panic.
#[test]
fn batch_preserves_the_boundary_and_continues_with_the_next_file() {
    use std::io::Write;
    let path = native_text_fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["extract", "--batch", "--no-ocr"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"id":"djvu", "path":path})).unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"id":"next", "path":"../../fixtures/hello.txt"})
    )
    .unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_extract_boundary(&responses[0]["report"]);
    assert_eq!(
        responses[1]["report"]["metadata"]["format"]["bucket"],
        "Text"
    );
    assert_eq!(
        responses[1]["report"]["metadata"]["diagnostics"],
        serde_json::json!([])
    );
    assert!(
        !responses[1]["report"]["content"]["units"][0]["text"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}

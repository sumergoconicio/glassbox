// Module: glassbox-cli Parquet boundary tests.
// File: parquet_cli.rs.
// Responsibility: Prove v2 identification-only inspection and unchanged explicit legacy Parquet deferral.
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const DEFERRAL: &str = "Parquet extraction is deferred; identification only";

/// Builds a valid empty Parquet footer with two primitive schema leaves.
///
/// Accepts no parameters. Returns deterministic Compact-Thrift test bytes.
/// No row groups or data pages exist; v2 inspect must not parse even this footer.
fn valid_parquet_bytes() -> Vec<u8> {
    let footer: &[u8] = b"\x15\x02\x19\x3c\x48\x06schema\x15\x04\x00\x15\x04\x25\x00\x18\x02id\x00\x15\x0c\x25\x02\x18\x04name\x00\x16\x00\x19\x0c\x00";
    let mut bytes = b"PAR1".to_vec();
    bytes.extend_from_slice(footer);
    bytes.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"PAR1");
    bytes
}

/// Creates an isolated ignored Parquet test directory.
///
/// Accepts a label. Returns a unique target-directory path. Directory creation
/// failures panic; no source fixture or production data is modified.
fn workdir(tag: &str) -> PathBuf {
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "parquet-cli-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Writes one deterministic Parquet or plain-text fixture.
///
/// Accepts a test directory, file name, and bytes. Returns the written path.
/// Filesystem failures panic; writes remain under the test-owned target tree.
fn write(directory: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Creates a Glassbox command rooted at the CLI crate directory.
///
/// Accepts argument strings. Returns an unlaunched command. No process, file,
/// or environment mutation occurs until the caller launches it.
fn litx(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_glassbox"));
    command.args(args).current_dir(env!("CARGO_MANIFEST_DIR"));
    command
}

/// Asserts canonical extraction keeps its static Parquet deferral.
///
/// Accepts a v2 extraction report. Returns unit after checking empty content,
/// absent footer profile, and one static diagnostic. Any row preview panics.
fn assert_extract_deferred(report: &Value) {
    assert_eq!(report["kind"], "extract");
    assert_eq!(report["metadata"]["status"], "unsupported");
    let errors = report["metadata"]["diagnostics"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "one static diagnostic: {errors:?}");
    assert_eq!(errors[0]["stage"], "metadata");
    assert_eq!(errors[0]["message"], DEFERRAL);
    assert!(report["metadata"]["parquet"].is_null());
    assert!(report["metadata"]["dataset"].is_null());
    assert_eq!(report["content"]["units"], serde_json::json!([]));
    assert_eq!(report["selection"]["omitted"], "unsupported");
    assert_eq!(
        report["metadata"]["format"]["detected_mime"],
        "application/vnd.apache.parquet"
    );
}

/// Asserts v2 Parquet inspection identifies bytes without footer or row parsing.
///
/// Accepts a metadata JSON report. Returns unit after checking schema, support,
/// complete status, diagnostic, digest, and absence of legacy body/profile keys.
fn assert_metadata_deferred(report: &Value) {
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["kind"], "inspect");
    assert_eq!(
        report["metadata"]["format"]["detected_mime"],
        "application/vnd.apache.parquet"
    );
    assert_eq!(
        report["metadata"]["extraction_support"],
        "identification_only"
    );
    assert_eq!(report["metadata"]["status"], "complete");
    assert_eq!(
        report["metadata"]["diagnostics"][0]["code"],
        "parquet_identification_only"
    );
    assert_eq!(
        report["metadata"]["digest"]["digest"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(report.get("parquet").is_none());
    assert!(report.get("dataset").is_none());
    assert!(report.get("text").is_none());
    assert!(report.get("markdown").is_none());
}

/// Verifies inspect v2 and explicit extract retain their separate Parquet contracts.
///
/// Accepts no parameters. Returns unit after one report from each operation.
/// Inspect exits 0; extract exits 2. Footer profiles, rows, or wrong statuses panic.
#[test]
fn parquet_inspect_and_extract_keep_separate_boundaries() {
    let directory = workdir("parity");
    let path = write(&directory, "empty.parquet", &valid_parquet_bytes());
    let path = path.to_str().unwrap();
    let inspected = litx(&["inspect", path]).output().unwrap();
    assert_eq!(inspected.status.code(), Some(0));
    let extracted = litx(&["extract", path, "--no-ocr"]).output().unwrap();
    assert_eq!(extracted.status.code(), Some(2));
    assert_metadata_deferred(&serde_json::from_slice(&inspected.stdout).unwrap());
    assert_extract_deferred(&serde_json::from_slice(&extracted.stdout).unwrap());
}

/// Verifies malformed Parquet is not footer-parsed and remains identification-only.
///
/// Accepts no parameters. Returns unit after inspecting hostile trailing bytes.
/// The process exits 0 with the same v2 boundary and a trustworthy streamed digest.
#[test]
fn malformed_parquet_defers_without_footer_parsing() {
    let directory = workdir("malformed");
    let mut bytes = b"PAR1".to_vec();
    bytes.extend_from_slice(&[0xff; 200]);
    let path = write(&directory, "broken.parquet", &bytes);
    let output = litx(&["inspect", path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_metadata_deferred(&serde_json::from_slice(&output.stdout).unwrap());
}

/// Verifies metadata batch recovers across valid, malformed, and text inputs.
///
/// Accepts no parameters. Returns unit after three ordered JSONL responses.
/// Aggregate status is 0 for complete inspection; the text report is complete and
/// content-free, proving that deferred Parquet inputs do not poison the session.
#[test]
fn parquet_batch_recovers_across_valid_malformed_and_text_inputs() {
    let directory = workdir("batch");
    let valid = write(&directory, "valid.parquet", &valid_parquet_bytes());
    let mut malformed = b"PAR1".to_vec();
    malformed.extend_from_slice(&[0xff; 200]);
    let broken = write(&directory, "broken.parquet", &malformed);
    let text = write(&directory, "notes.txt", b"ordinary text survives the batch");
    let mut child = litx(&["inspect", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"id": "p1", "path": valid})).unwrap();
    writeln!(input, "{}", serde_json::json!({"id": 2, "path": broken})).unwrap();
    writeln!(input, "{}", serde_json::json!({"id": "t3", "path": text})).unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    assert_metadata_deferred(&responses[0]["report"]);
    assert_metadata_deferred(&responses[1]["report"]);
    assert_eq!(responses[2]["report"]["metadata"]["status"], "complete");
    assert_eq!(
        responses[2]["report"]["metadata"]["counts"][0]["unit"],
        "logical_line"
    );
    assert!(responses[2]["report"].get("text").is_none());
}

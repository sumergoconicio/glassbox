// Module: glassbox-cli XML metadata transport tests.
// File: xml_cli.rs.
// Responsibility: Prove v2 value-free XML inspection, persistent recovery, and explicit legacy profile parity.
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const VALID: &[u8] = b"<catalog><book id=\"SENTINEL-V1\">rust</book><book>xml</book></catalog>";
const INVALID: &[u8] = b"<a>SENTINEL-BAD1&undefined;</a>";

/// Creates an isolated ignored XML CLI test directory.
///
/// Accepts a label. Returns a unique target-directory path. Directory creation
/// failures panic; source fixtures and production data remain untouched.
fn workdir(tag: &str) -> PathBuf {
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "xml-cli-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

/// Writes one deterministic XML fixture under the test target directory.
///
/// Accepts a directory, file name, and bytes. Returns the written path.
/// Filesystem failures panic; no checked-in fixture is overwritten.
fn write(directory: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Creates a Glassbox command rooted at the CLI crate directory.
///
/// Accepts argument strings. Returns an unlaunched command. No process or file
/// mutation occurs until the caller launches it.
fn litx(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_glassbox"));
    command.args(args).current_dir(env!("CARGO_MANIFEST_DIR"));
    command
}

/// Asserts the v2 XML report contains element paths/counts but no values/profile body.
///
/// Accepts a metadata JSON report. Returns unit after checking three elements,
/// two paths, digest, and value absence. Legacy `xml`/text fields cause a panic.
fn assert_xml_metadata(report: &Value) {
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["kind"], "inspect");
    assert_eq!(report["metadata"]["status"], "complete");
    assert_eq!(
        report["metadata"]["digest"]["digest"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(
        report["metadata"]["counts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|count| count["unit"] == "xml_element" && count["value"] == 3)
    );
    let identifiers = report["metadata"]["schema_identifiers"].as_array().unwrap();
    assert_eq!(identifiers.len(), 2);
    assert!(
        identifiers
            .iter()
            .any(|identifier| identifier["name"] == "/catalog/book")
    );
    assert!(report.get("xml").is_none());
    assert!(report.get("text").is_none());
    assert!(!serde_json::to_string(report).unwrap().contains("SENTINEL"));
    assert!(
        !serde_json::to_string(report)
            .unwrap()
            .contains("rust</book>")
    );
}

/// Verifies v2 inspect and explicit extract publish separate value-free XML models.
///
/// Accepts no parameters. Returns unit after comparing schema identifiers with
/// the legacy structural profile. Attribute/text value leakage or wrong exits panic.
#[test]
fn xml_inspect_and_extract_publish_value_free_structure() {
    let directory = workdir("parity");
    let path = write(&directory, "catalog.xml", VALID);
    let path = path.to_str().unwrap();
    let inspected = litx(&["inspect", path]).output().unwrap();
    assert!(
        inspected.status.success(),
        "{}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let extracted = litx(&["extract", path, "--no-ocr"]).output().unwrap();
    assert_eq!(extracted.status.code(), Some(2));
    let fresh: Value = serde_json::from_slice(&inspected.stdout).unwrap();
    let full: Value = serde_json::from_slice(&extracted.stdout).unwrap();
    assert_xml_metadata(&fresh);
    assert_eq!(full["kind"], "extract");
    assert_eq!(full["metadata"]["diagnostics"], serde_json::json!([]));
    assert!(
        full["metadata"]["counts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|count| count["unit"] == "xml_element" && count["value"] == 3)
    );
    assert_eq!(
        full["metadata"]["schema_identifiers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(full["content"]["units"].as_array().unwrap().len(), 1);
    assert_eq!(full["content"]["units"][0]["rows"], serde_json::json!([]));
    assert!(
        full["content"]["units"][0]["text"]
            .as_str()
            .unwrap()
            .contains("# XML structure")
    );
    assert!(!String::from_utf8_lossy(&extracted.stdout).contains("SENTINEL"));
}

/// Verifies malformed XML fails closed with exit 1 and content-free diagnostics.
///
/// Accepts no parameters. Returns unit after inspecting an undefined entity.
/// No partial schema profile or sentinel body text may appear in the v2 report.
#[test]
fn xml_fresh_cli_fails_closed_on_invalid_input_with_exit_one() {
    let directory = workdir("invalid");
    let path = write(&directory, "bad.xml", INVALID);
    let output = litx(&["inspect", path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["metadata"]["status"], "failed");
    assert_eq!(report["metadata"]["diagnostics"][0]["stage"], "metadata");
    assert!(report.get("xml").is_none());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !text.contains("SENTINEL"),
        "content-free diagnostics: {text}"
    );
}

/// Verifies valid-invalid-valid batch recovery and fresh transport parity.
///
/// Accepts no parameters. Returns unit after three ordered metadata responses.
/// Aggregate status is 1 for the invalid item; valid reports remain identical,
/// value-free, and complete after the failure.
#[test]
fn xml_batch_recovers_valid_invalid_valid_and_matches_fresh_transport() {
    let directory = workdir("batch");
    let good = write(&directory, "good.xml", VALID);
    let bad = write(&directory, "bad.xml", INVALID);
    let mut child = litx(&["inspect", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"id": "v1", "path": good})).unwrap();
    writeln!(input, "{}", serde_json::json!({"id": 2, "path": bad})).unwrap();
    writeln!(input, "{}", serde_json::json!({"id": "v3", "path": good})).unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    assert_xml_metadata(&responses[0]["report"]);
    assert_eq!(responses[1]["report"]["metadata"]["status"], "failed");
    assert_xml_metadata(&responses[2]["report"]);
    assert_eq!(
        responses[0]["report"]["metadata"]["schema_identifiers"],
        responses[2]["report"]["metadata"]["schema_identifiers"]
    );

    let fresh = litx(&["inspect", good.to_str().unwrap()]).output().unwrap();
    let fresh: Value = serde_json::from_slice(&fresh.stdout).unwrap();
    assert_eq!(
        responses[0]["report"]["metadata"]["schema_identifiers"],
        fresh["metadata"]["schema_identifiers"]
    );
    let text: String = responses
        .iter()
        .map(|response| serde_json::to_string(response).unwrap())
        .collect();
    assert!(!text.contains("SENTINEL"));
}

/// Verifies a persistent XML worker flushes metadata before stdin closes.
///
/// Accepts no parameters. Returns unit after reading one JSONL response.
/// Timeout, malformed JSON, wrong element counts, or body fields panic.
#[test]
fn xml_batch_flushes_structural_response_before_eof() {
    use std::io::BufRead;
    let directory = workdir("flush");
    let good = write(&directory, "good.xml", VALID);
    let mut child = litx(&["inspect", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .unwrap();
        let _ = sender.send(line);
    });
    writeln!(
        input,
        "{}",
        serde_json::json!({"id": "xml-flush", "path": good})
    )
    .unwrap();
    input.flush().unwrap();
    let response = receiver.recv_timeout(std::time::Duration::from_secs(10));
    drop(input);
    if response.is_err() {
        let _ = child.kill();
    }
    child.wait().unwrap();
    reader.join().unwrap();
    let response: Value =
        serde_json::from_str(&response.expect("XML batch response must arrive before EOF"))
            .unwrap();
    assert_eq!(response["id"], "xml-flush");
    assert_xml_metadata(&response["report"]);
}

/// Verifies declaration errors do not leak declaration names through v2 diagnostics.
///
/// Accepts no parameters. Returns unit after inspecting an invalid declaration.
/// The operation exits 1 and the serialized report must omit `SECRETATTR`.
#[test]
fn xml_declaration_errors_stay_content_free_through_cli_transport() {
    let directory = workdir("decl-leak");
    let path = write(
        &directory,
        "decl.xml",
        b"<?xml version=\"1.0\" SECRETATTR=\"x\"?><a/>",
    );
    let output = litx(&["inspect", path.to_str().unwrap()]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["metadata"]["status"], "failed");
    assert!(report.get("xml").is_none());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !text.contains("SECRETATTR"),
        "no declaration attribute names leak: {text}"
    );
}

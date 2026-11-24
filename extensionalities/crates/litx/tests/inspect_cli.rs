//! Module: glassbox-cli metadata and extraction integration tests.
//! File: inspect_cli.rs.
//! Responsibility: Prove schema v3 inspect transport, legacy flag migration, exit codes, sampled hashing, and extraction behavior.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Runs the compiled Glassbox binary from the CLI crate directory.
///
/// Accepts argument strings. Returns captured process output. Spawn failures
/// panic; test assertions own all exit-code and output validation.
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap()
}

/// Parses a JSON process response regardless of its documented inspect exit code.
///
/// Accepts captured output. Returns a JSON value. Empty/non-JSON stdout panics
/// with stderr and the process status; this helper does not assert success.
fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: status={:?}, stderr={}, stdout={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

/// Creates an isolated ignored directory for generated CLI inputs and outputs.
///
/// Accepts a test label. Returns a unique target-directory path. Directory
/// creation failures panic; no checked-in fixture or production data is changed.
fn workdir(label: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&directory).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        &directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    directory
}

/// Recursively rejects body fields from a serialized metadata report.
///
/// Accepts a JSON value. Returns unit if no legacy text/Markdown/row/cell/OCR
/// result field exists. A forbidden key panics with its name.
fn assert_metadata_only(value: &Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                assert!(
                    !["text", "content", "markdown", "rows", "cells", "ocr_text"]
                        .contains(&key.as_str()),
                    "unexpected body key {key}"
                );
                assert_metadata_only(child);
            }
        }
        Value::Array(values) => values.iter().for_each(assert_metadata_only),
        _ => {}
    }
}

/// Checks the common schema v3 CLI envelope and complete plain-text metadata result.
///
/// Accepts a report. Returns unit after schema/kind/format/digest assertions.
/// Missing v3 fields, body fields, legacy fields, or a failed operation panic.
fn assert_text_metadata(report: &Value) {
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["kind"], "inspect");
    assert_eq!(report["status"], "complete");
    assert_eq!(report["format"]["type"], "text");
    assert_eq!(report["format"]["extension"], "txt");
    assert_eq!(report["file"]["sha256"].as_str().unwrap().len(), 64);
    assert!(report["file"].get("consistent").is_none());
    assert!(report.get("advice").is_none());
    assert!(report.get("next_step").is_none());
    assert!(report.get("diagnostics").is_none());
    assert!(report.get("provenance").is_none());
    assert_metadata_only(report);
}

/// Verifies JSON, pretty JSON, text, and Markdown all carry only schema v3 metadata.
///
/// Accepts no parameters. Returns unit after running every output format.
/// Legacy body keys, incorrect envelopes, or unexpected process failures panic.
#[test]
fn inspect_all_formats_emit_metadata_only_reports() {
    for args in [
        vec!["inspect", "../../fixtures/hello.txt"],
        vec!["inspect", "../../fixtures/hello.txt", "--pretty"],
        vec!["inspect", "../../fixtures/hello.txt", "--format", "text"],
        vec![
            "inspect",
            "../../fixtures/hello.txt",
            "--format",
            "markdown",
        ],
    ] {
        let output = run(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains("small self-authored inspection fixture"));
        if args.contains(&"markdown") {
            assert!(stdout.contains("# Glassbox inspection"), "{stdout}");
            assert!(stdout.contains("- Schema: 3"), "{stdout}");
            assert!(stdout.contains("- SHA-256:"), "{stdout}");
            assert!(!stdout.contains("## Provenance"), "{stdout}");
            assert!(!stdout.contains("\"text\""));
        } else if args.contains(&"text") {
            assert!(stdout.contains("Schema: 3\n"), "{stdout}");
            assert!(stdout.contains("SHA-256:"), "{stdout}");
            assert!(!stdout.contains("# Glassbox inspection"), "{stdout}");
            assert!(!stdout.contains("Provenance:"), "{stdout}");
            assert!(!stdout.contains("\"text\""));
        } else {
            assert_text_metadata(&serde_json::from_str(&stdout).unwrap());
        }
    }
}

/// Verifies every extraction-only content flag exits 64 with explicit migration advice.
///
/// Accepts no parameters. Returns unit after testing all rejected inspect flags.
/// Any accepted flag, missing advice, or file processing before rejection panics.
#[test]
fn inspect_rejects_legacy_content_flags_with_exit_64() {
    for flag in [
        "--identify",
        "--max-pages",
        "--max-tokens",
        "--max-chars",
        "--probe-ocr",
        "--no-ocr",
        "--ocr-language",
        "--packet",
        "--no-markers",
    ] {
        let output = run(&["inspect", "../../fixtures/hello.txt", flag]);
        assert_eq!(output.status.code(), Some(64), "{flag}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("glassbox extract PATH"), "{flag}: {stderr}");
        assert!(output.stdout.is_empty());
    }
}

/// Verifies explicit extract retains full text and native page-limit behavior.
///
/// Accepts no parameters. Returns unit after checking 600 words and six PDF pages.
/// Missing content, unintended truncation, or incomplete page geometry panics.
#[test]
fn extract_preserves_full_text_and_page_limits() {
    let directory = workdir("extract-full-text");
    let path = directory.join("words.txt");
    std::fs::write(&path, "word ".repeat(600)).unwrap();
    let output = run(&["extract", path.to_str().unwrap(), "--no-ocr"]);
    assert!(output.status.success());
    let report = json(&output);
    assert_eq!(report["content"]["units"].as_array().unwrap().len(), 1);
    assert_eq!(
        report["content"]["units"][0]["text"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .count(),
        600
    );
    assert_eq!(report["completeness"], "exact");

    for (args, expected_pages, complete) in [
        (
            vec![
                "extract",
                "../../fixtures/multipage.pdf",
                "--no-ocr",
                "--max-pages",
                "5",
            ],
            5,
            false,
        ),
        (
            vec!["extract", "../../fixtures/multipage.pdf", "--no-ocr"],
            6,
            true,
        ),
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(0));
        let report = json(&output);
        assert_eq!(report["schema_version"], 3);
        assert_eq!(report["status"], "complete");
        assert_eq!(report["metadata"]["status"], "complete");
        assert_eq!(
            report["selection"]["omitted"],
            if complete { "none" } else { "selection" }
        );
        assert_eq!(report["metadata"]["structure"]["counts"]["pages"], 6);
        assert_eq!(report["metadata"]["structure"]["pdf"]["pages"], 6);
        assert_eq!(
            report["completeness"],
            if complete { "exact" } else { "partial" }
        );
        assert_eq!(
            report["content"]["units"].as_array().unwrap().len(),
            expected_pages
        );
        assert_eq!(
            report["content"]["units"]
                .as_array()
                .unwrap()
                .iter()
                .any(|unit| unit["text"]
                    .as_str()
                    .unwrap()
                    .contains("Final page confirms full-document extraction.")),
            complete
        );
    }
}

/// Verifies batch extract still persists Markdown and legacy JSON per document.
///
/// Accepts no parameters. Returns unit after three mixed-document responses.
/// Missing artifacts, overwritten files, content mismatch, or process failure panics.
#[test]
fn batch_extract_persists_markdown_and_reports_for_mixed_documents() {
    use std::io::Write;
    use std::process::Stdio;
    let directory = workdir("batch-extract-output");
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["extract", "--batch", "--no-ocr", "--output-dir"])
        .arg(&directory)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for name in ["notes.docx", "native.pdf", "grid.xlsx"] {
        writeln!(
            input,
            "{}",
            serde_json::json!({"path":format!("../../fixtures/{name}")})
        )
        .unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reports: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(reports.len(), 3);
    for response in &reports {
        let report = &response["report"];
        assert_eq!(report["schema_version"], 3);
        assert!(report.get("diagnostics").is_none());
        assert!(report["metadata"].get("diagnostics").is_none());
        assert_eq!(report["artifacts"].as_array().unwrap().len(), 2);
        assert_eq!(report["metadata"]["artifacts"].as_array().unwrap().len(), 2);
        for artifact in report["artifacts"].as_array().unwrap() {
            let bytes = std::fs::read(artifact["path"].as_str().unwrap()).unwrap();
            if artifact["kind"] == "markdown_content" {
                let markdown = String::from_utf8(bytes).unwrap();
                assert!(!markdown.is_empty());
                if report["content"]["units"][0]["text"].is_string() {
                    let expected = report["content"]["units"][0]["text"]
                        .as_str()
                        .unwrap()
                        .split_whitespace()
                        .next()
                        .unwrap();
                    assert!(markdown.contains(expected), "{markdown}");
                }
            } else {
                let saved: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(saved["schema_version"], 3);
                assert_eq!(
                    saved["metadata"]["file"]["path"],
                    report["metadata"]["file"]["path"]
                );
            }
        }
    }
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 6);
}

/// Verifies explicit page limits never imply a token budget during extraction.
///
/// Accepts no parameters. Returns unit after bounded and unbounded extract runs.
/// Any result below the original 600 word tokens panics.
#[test]
fn extract_with_explicit_max_pages_keeps_every_word_token() {
    let directory = workdir("extract-pages-tokens");
    let path = directory.join("words.txt");
    std::fs::write(&path, "word ".repeat(600)).unwrap();
    for args in [
        vec!["extract", "--no-ocr"],
        vec!["extract", "--no-ocr", "--max-pages", "6"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_glassbox"))
            .args(&args)
            .arg(&path)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let report = json(&output);
        assert_eq!(
            report["content"]["units"][0]["text"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .count(),
            600
        );
    }
}

/// Verifies Office content and packet assertions remain on explicit extract.
///
/// Accepts no parameters. Returns unit after bounded DOCX/PPTX/XLSX extraction.
/// Missing token bounds, withheld over-budget workbook TSV, row-1-inclusive cell
/// truncation, or original body wording panics.
#[test]
fn identify_office_text_obeys_an_explicit_word_token_budget() {
    for name in ["notes.docx", "deck.pptx", "grid.xlsx"] {
        let output = run(&[
            "extract",
            &format!("../../fixtures/{name}"),
            "--identify",
            "--max-tokens",
            "3",
            "--packet",
        ]);
        assert_eq!(output.status.code(), Some(2));
        let report = json(&output);
        assert_eq!(report["selection"]["omitted"], "safety_limit");
        let content = report["content"]["units"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|unit| unit["text"].as_str())
            .collect::<Vec<_>>()
            .join(" ");
        match name {
            "notes.docx" => assert_eq!(content.trim_end(), "This is a"),
            "deck.pptx" => assert_eq!(content.trim_end(), "This is the"),
            "grid.xlsx" => {
                assert!(content.is_empty());
                let unit = &report["content"]["units"][0];
                assert!(unit["text"].is_null());
                assert_eq!(unit["completeness"], "partial");
                assert_eq!(report["completeness"], "partial");
                let rows = unit["rows"].as_array().unwrap();
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0]["row_index"], 1);
                assert_eq!(rows[0]["values"], serde_json::json!(["Label", "Total"]));
                assert_eq!(rows[1]["row_index"], 2);
                assert_eq!(rows[1]["values"], serde_json::json!(["count", ""]));
            }
            _ => unreachable!(),
        }
    }
}

/// Verifies metadata batch isolates oversized lines and processes the next request.
///
/// Accepts no parameters. Returns unit after one invalid and one valid JSONL item.
/// The aggregate exits 64; a lost response or legacy body field fails the test.
#[test]
fn batch_rejects_oversized_lines_without_losing_the_next_request() {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "--batch"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"path":"x".repeat(70_000)})).unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"id":9,"path":"../../fixtures/hello.txt"})
    )
    .unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(64));
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert!(
        responses[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("65536")
    );
    assert_eq!(responses[1]["id"], 9);
    assert_text_metadata(&responses[1]["report"]);
}

/// Verifies a persistent metadata worker flushes a complete response before EOF.
///
/// Accepts no parameters. Returns unit after reading one JSONL response.
/// Timeout, malformed JSON, or a legacy body report panics; a stuck child is killed.
#[test]
fn batch_flushes_each_response_before_stdin_closes() {
    use std::io::{BufRead, Write};
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "--batch"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
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
        serde_json::json!({"path":"../../fixtures/hello.txt"})
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
        serde_json::from_str(&response.expect("worker must respond before EOF")).unwrap();
    assert_text_metadata(&response["report"]);
}

/// Verifies request correlation, operational failure aggregation, and recovery.
///
/// Accepts no parameters. Returns unit after valid, malformed, missing, valid items.
/// Invalid-plus-failed precedence exits 64 with all four ordered v2 responses.
#[test]
fn batch_correlates_requests_and_continues_after_bad_input() {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "--batch"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"id":"first", "path":"../../fixtures/hello.txt"})
    )
    .unwrap();
    writeln!(input, "not json").unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"id":2, "path":"missing-test-file"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        serde_json::json!({"id":3, "path":"../../fixtures/hello.txt"})
    )
    .unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(64));
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 4);
    assert_text_metadata(&responses[0]["report"]);
    assert_eq!(responses[1]["error"]["stage"], "batch");
    assert_eq!(responses[2]["report"]["diagnostics"][0]["stage"], "input");
    assert_eq!(responses[2]["report"]["status"], "failed");
    assert_text_metadata(&responses[3]["report"]);
}

/// Verifies single-file output-dir writes a v3 JSON report and content-free summary.
///
/// Accepts no parameters. Returns unit after reading both generated artifacts.
/// Incorrect artifact kinds, body leakage, or non-v3 saved JSON panics.
#[test]
fn inspect_output_dir_persists_metadata_summary_and_json() {
    let directory = workdir("metadata-output");
    let output = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "../../fixtures/hello.txt", "--output-dir"])
        .arg(&directory)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let report = json(&output);
    assert_text_metadata(&report);
    assert_eq!(report["artifacts"].as_array().unwrap().len(), 2);
    for artifact in report["artifacts"].as_array().unwrap() {
        let bytes = std::fs::read(artifact["path"].as_str().unwrap()).unwrap();
        if artifact["kind"] == "metadata_json" {
            let saved: Value = serde_json::from_slice(&bytes).unwrap();
            assert_text_metadata(&saved);
            assert_eq!(saved["artifacts"].as_array().unwrap().len(), 2);
        } else {
            let summary = String::from_utf8(bytes).unwrap();
            assert!(summary.contains("# Glassbox inspection"), "{summary}");
            assert!(summary.contains("- Schema: 3"), "{summary}");
            assert!(!summary.contains("small self-authored inspection fixture"));
        }
    }
}

/// Verifies inspect's public 0/1 exits and identification-only completion.
///
/// Accepts no parameters. Returns unit after text, DjVu, and corrupt PDF runs.
/// Any unexpected exit code, missing digest, or content-producing report panics.
#[test]
fn inspect_exit_codes_distinguish_complete_identification_and_failed() {
    let complete = run(&["inspect", "../../fixtures/hello.txt"]);
    assert_eq!(complete.status.code(), Some(0));
    assert_text_metadata(&json(&complete));

    let identification = run(&["inspect", "../../fixtures/single.djvu"]);
    assert_eq!(identification.status.code(), Some(0));
    let report = json(&identification);
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["status"], "complete");
    assert_eq!(report["format"]["type"], "ebooks");
    assert_eq!(report["readiness"]["extractable"], "identification_only");
    assert_metadata_only(&report);

    let failed = run(&["inspect", "../../fixtures/broken.pdf"]);
    assert_eq!(failed.status.code(), Some(1));
    let report = json(&failed);
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["status"], "failed");
    assert_eq!(report["file"]["sha256"].as_str().unwrap().len(), 64);
    assert_metadata_only(&report);
}

/// Verifies a closed stdout pipe becomes an operational error, not a panic.
///
/// Accepts no parameters. Returns unit after closing the child's stdout reader.
/// Exit 101, a signal exit, or a missing I/O diagnostic fails this regression.
#[test]
fn closed_stdout_returns_operational_exit_without_panicking() {
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "../../fixtures/hello.txt"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Broken pipe"));
}

/// Verifies CLI thumbnail generation is opt-in, bounded, and recorded in v3.
///
/// Accepts no parameters. Returns unit after ordinary and explicit PDF runs.
/// Default rendering, oversized output, missing artifact, or body fields panic.
#[test]
fn inspect_thumbnail_is_opt_in_and_bounded() {
    let ordinary = run(&["inspect", "../../fixtures/native.pdf"]);
    assert!(ordinary.status.success());
    assert!(json(&ordinary).get("artifacts").is_none());
    let directory = workdir("metadata-thumbnail");
    let thumbnail = directory.join("page.png");
    let output = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "../../fixtures/native.pdf", "--thumbnail"])
        .arg(&thumbnail)
        .args(["--thumbnail-max-edge", "64"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json(&output);
    assert_metadata_only(&report);
    let artifact = &report["artifacts"][0];
    assert!(artifact["width"].as_u64().unwrap() <= 64);
    assert!(artifact["height"].as_u64().unwrap() <= 64);
    assert!(std::fs::metadata(thumbnail).unwrap().len() > 0);
}

/// Verifies missing OCR data remains a qualified explicit-extraction failure.
///
/// Accepts no parameters. Returns unit after extracting the native PDF with an
/// isolated missing tessdata path. Lost OCR provenance or incomplete flags panic.
#[test]
fn missing_ocr_model_marks_pdf_extraction_incomplete() {
    let missing = workdir("missing-tessdata-prov").join("absent");
    assert!(!missing.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["extract", "--identify", "../../fixtures/native.pdf"])
        .env("TESSDATA_PREFIX", &missing)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let report = json(&output);
    assert_eq!(report["schema_version"], 3);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["code"] == "pdf_ocr")
    );
    assert_eq!(report["status"], "partial");
    assert_eq!(report["metadata"]["status"], "complete");
    assert_eq!(report["completeness"], "partial");
    assert!(
        report["content"]["units"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Native PDF evidence")
    );
    assert!(!missing.exists());
}

/// Verifies raster OCR probing remains available only through explicit extract.
///
/// Accepts no parameters. Returns unit after a missing-model extraction probe.
/// Lost native classification, missing error provenance, or model creation panics.
#[test]
fn missing_local_ocr_model_keeps_native_metrics_without_downloads() {
    let missing = workdir("missing-tessdata").join("absent");
    assert!(!missing.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args([
            "extract",
            "../../fixtures/document-image.png",
            "--probe-ocr",
        ])
        .env("TESSDATA_PREFIX", &missing)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let report = json(&output);
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["metadata"]["format"]["type"], "images");
    assert_eq!(report["selection"]["unit"], "image");
    assert!(report["diagnostics"][0]["unit_index"].is_null());
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| {
                error["code"] == "probe_ocr"
                    && error["message"]
                        .as_str()
                        .unwrap()
                        .contains("local eng.traineddata")
            })
    );
    assert!(!missing.exists());
}

/// Verifies `--verbose` controls reader provenance across JSON, text, and Markdown.
///
/// Accepts no parameters. Returns unit after running default and `--verbose`
/// inspect invocations on the tracked text fixture. Leaked provenance by default
/// or missing provenance under `--verbose` panics.
#[test]
fn inspect_verbose_controls_provenance_across_formats() {
    let default_json = json(&run(&["inspect", "../../fixtures/hello.txt"]));
    assert!(default_json.get("provenance").is_none());

    let verbose_json = json(&run(&["inspect", "../../fixtures/hello.txt", "--verbose"]));
    assert!(
        verbose_json["provenance"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "{verbose_json}"
    );

    let verbose_text = String::from_utf8(
        run(&[
            "inspect",
            "../../fixtures/hello.txt",
            "--format",
            "text",
            "--verbose",
        ])
        .stdout,
    )
    .unwrap();
    assert!(verbose_text.contains("Provenance:\n"), "{verbose_text}");

    let verbose_md = String::from_utf8(
        run(&[
            "inspect",
            "../../fixtures/hello.txt",
            "--format",
            "markdown",
            "--verbose",
        ])
        .stdout,
    )
    .unwrap();
    assert!(verbose_md.contains("## Provenance\n"), "{verbose_md}");
}

/// Verifies misnamed files keep literal extensions and report `extension_mismatch: true`.
///
/// Accepts no parameters. Returns unit after inspecting HTML bytes saved with a
/// `.pdf` suffix next to a genuine PDF. Overwritten suffixes, missing mismatch
/// flags, or false positives on matching files panic.
#[test]
fn inspect_misnamed_file_preserves_literal_extension_and_flags_mismatch() {
    let directory = workdir("misnamed");
    let misnamed = directory.join("report.pdf");
    std::fs::write(
        &misnamed,
        b"<!DOCTYPE html><html><head><title>Web Page</title></head><body>Hello</body></html>",
    )
    .unwrap();
    let output = run(&["inspect", misnamed.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    let report = json(&output);
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["format"]["type"], "web");
    assert_eq!(report["format"]["extension"], "pdf");
    assert_eq!(report["format"]["extension_mismatch"], true);

    let misnamed_docx = directory.join("contract.pdf");
    std::fs::copy("../../fixtures/notes.docx", &misnamed_docx).unwrap();
    let docx_output = run(&["inspect", misnamed_docx.to_str().unwrap()]);
    assert_eq!(docx_output.status.code(), Some(0));
    let docx_report = json(&docx_output);
    assert_eq!(docx_report["format"]["type"], "office_docs");
    assert_eq!(docx_report["format"]["extension"], "pdf");
    assert_eq!(docx_report["format"]["extension_mismatch"], true);

    let honest = json(&run(&["inspect", "../../fixtures/native.pdf"]));
    assert_eq!(honest["format"]["type"], "pdf");
    assert_eq!(honest["format"]["extension"], "pdf");
    assert!(honest["format"].get("extension_mismatch").is_none());
}

/// Verifies files above `DEFAULT_FULL_HASH_MAX_BYTES` sample by default, use `fp-` stems, and honor `--full-hash`.
///
/// Accepts no parameters and writes one isolated sparse file (`128 MiB + 1` byte)
/// under the ignored target directory, removing it before returning. Missing
/// sampled fingerprints, missing `fp-` stems, leaked `sha256` on sampled runs,
/// or ignored `--full-hash` overrides panic.
#[test]
fn inspect_large_sparse_file_samples_by_default_and_honors_full_hash_and_fp_stem() {
    use glassbox_inspect::hashing::{DEFAULT_FULL_HASH_MAX_BYTES, SAMPLED_FINGERPRINT_SCHEME};
    use std::io::{Seek, SeekFrom, Write};

    let directory = workdir("sparse-hash");
    let large = directory.join("archive-dump.bin");
    let out_dir = directory.join("out");
    let total_len = DEFAULT_FULL_HASH_MAX_BYTES + 1;
    {
        let mut file = std::fs::File::create(&large).unwrap();
        file.set_len(total_len).unwrap();
        file.write_all(b"GLASSBOX-SPARSE-HEAD").unwrap();
        file.seek(SeekFrom::Start(total_len - 16)).unwrap();
        file.write_all(b"SPARSE-TAIL-END!").unwrap();
        file.sync_all().unwrap();
    }

    let sampled_output = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", large.to_str().unwrap(), "--output-dir"])
        .arg(&out_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert_eq!(sampled_output.status.code(), Some(2));
    let sampled = json(&sampled_output);
    assert_eq!(sampled["schema_version"], 3);
    assert_eq!(sampled["status"], "unsupported");
    assert_eq!(sampled["file"]["bytes"], total_len);
    assert!(sampled["file"].get("sha256").is_none(), "{sampled}");
    let fp_value = sampled["file"]["fingerprint"]["value"].as_str().unwrap();
    assert_eq!(
        sampled["file"]["fingerprint"]["scheme"],
        SAMPLED_FINGERPRINT_SCHEME
    );
    assert_eq!(fp_value.len(), 64);
    assert!(
        sampled["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "fingerprint_sampled" && item["severity"] == "info"),
        "{sampled}"
    );
    let expected_stem = format!("fp-{}", &fp_value[..12]);
    let artifacts = sampled["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2);
    for artifact in artifacts {
        let name = Path::new(artifact["path"].as_str().unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(
            name.contains(&expected_stem),
            "expected {expected_stem} in {name}"
        );
    }

    let full_output = run(&["inspect", large.to_str().unwrap(), "--full-hash"]);
    assert_eq!(full_output.status.code(), Some(2));
    let full = json(&full_output);
    assert_eq!(full["file"]["sha256"].as_str().unwrap().len(), 64);
    assert!(full["file"].get("fingerprint").is_none(), "{full}");
    assert!(
        full.get("diagnostics").is_none_or(|items| !items
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "fingerprint_sampled")),
        "{full}"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

/// Verifies `inspect` accepts a flag-like filename after `--` while still rejecting prior content flags.
///
/// Accepts no parameters and writes a temporary file literally named
/// `--no-markers` under the ignored target directory, removing it afterward.
/// Non-zero exit for `inspect -- --no-markers`, mismatched `file.name`, or
/// missing exit-64 rejection for `inspect --no-markers file.txt` panics.
#[test]
fn inspect_allows_flag_like_filename_after_option_terminator() {
    let directory = workdir("inspect-dash-filename");
    let literal = directory.join("--no-markers");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/native.pdf");
    std::fs::copy(&source, &literal).unwrap();

    let accepted = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "--", "--no-markers"])
        .current_dir(&directory)
        .output()
        .unwrap();
    assert_eq!(
        accepted.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let report = json(&accepted);
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["kind"], "inspect");
    assert_eq!(report["status"], "complete");
    assert_eq!(report["file"]["name"], "--no-markers");
    assert_metadata_only(&report);

    let rejected = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "--no-markers", "file.txt"])
        .current_dir(&directory)
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(64));
    let stderr = String::from_utf8(rejected.stderr).unwrap();
    assert!(
        stderr.contains("glassbox extract PATH --no-markers"),
        "{stderr}"
    );
    assert!(rejected.stdout.is_empty());

    let _ = std::fs::remove_dir_all(&directory);
}

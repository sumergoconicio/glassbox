//! Module: Glassbox canonical extraction CLI integration tests.
//! File: extract_cli.rs.
//! Responsibility: Verify selector parsing, exits, JSONL, persistence, and metadata separation.

use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// Creates a unique test-owned working directory under the ignored target tree.
///
/// Accepts a test label. Returns the new directory path. Filesystem failures
/// panic; no checked-in fixture or production datastore is modified.
fn workdir(label: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/extract-cli")
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

/// Writes seven independently identifiable logical lines for selection tests.
///
/// Accepts a test label. Returns the text fixture path. Filesystem failures
/// panic; the file is isolated beneath the ignored target directory.
fn lines(label: &str) -> PathBuf {
    let path = workdir(label).join("lines.txt");
    std::fs::write(&path, "zero\none\ntwo\nthree\nfour\nfive\nsix").unwrap();
    path
}

/// Runs the real Glassbox executable from the CLI crate directory.
///
/// Accepts argument strings. Returns captured process output. Launch failures
/// panic; stdout remains JSON for completed extraction requests.
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap()
}

/// Parses a canonical report and includes stderr when JSON decoding fails.
///
/// Accepts captured output. Returns a JSON value. Malformed stdout panics with
/// the process diagnostic instead of hiding the failed command.
fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Verifies bare extraction, valueless pages, explicit ranges, and oversized first-N.
#[test]
fn pages_cover_default_first_n_ranges_and_beyond_total() {
    let path = lines("pages");
    let path = path.to_str().unwrap();

    let full = run(&["extract", path, "--no-ocr"]);
    assert_eq!(full.status.code(), Some(0));
    let full = json(&full);
    assert_eq!(full["kind"], "extract");
    assert_eq!(full["selection"]["requested"]["mode"], "all");
    assert_eq!(full["selection"]["available"]["value"], 7);
    assert_eq!(
        full["selection"]["returned_indices"],
        serde_json::json!([1, 2, 3, 4, 5, 6, 7])
    );
    assert_eq!(full["content"]["units"][6]["text"], "six");

    let bare = run(&["extract", path, "--pages", "--no-ocr"]);
    assert_eq!(bare.status.code(), Some(0));
    let bare = json(&bare);
    assert_eq!(
        bare["selection"]["requested"],
        serde_json::json!({"mode":"first_n","count":5})
    );
    assert_eq!(
        bare["selection"]["returned_indices"],
        serde_json::json!([1, 2, 3, 4, 5])
    );
    assert_eq!(bare["metadata"]["status"], "complete");
    assert_eq!(bare["completeness"], "partial");
    assert_eq!(bare["selection"]["omitted"], "selection");

    let ranges = run(&["extract", path, "--pages", "2-4,6,3", "--no-ocr"]);
    assert_eq!(ranges.status.code(), Some(0));
    let ranges = json(&ranges);
    assert_eq!(
        ranges["selection"]["returned_indices"],
        serde_json::json!([2, 3, 4, 6])
    );
    let values: Vec<&str> = ranges["content"]["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|unit| unit["text"].as_str().unwrap())
        .collect();
    assert_eq!(values, ["one", "two", "three", "five"]);
    assert_eq!(ranges["metadata"]["status"], "complete");
    assert_eq!(ranges["completeness"], "partial");

    let beyond = run(&["extract", path, "--pages", "99", "--no-ocr"]);
    assert_eq!(beyond.status.code(), Some(0));
    let beyond = json(&beyond);
    assert_eq!(
        beyond["selection"]["returned_indices"],
        serde_json::json!([1, 2, 3, 4, 5, 6, 7])
    );
    assert_eq!(beyond["selection"]["omitted"], "none");
}

/// Verifies complete request outcomes remain distinct from partial source coverage.
///
/// Accepts no parameters and exercises one independent six-line fixture. Returns
/// unit after checking default-all, bare-five, first-two, and range selections.
/// Panics if selected content, omission disclosure, status, or exit zero regresses.
#[test]
fn six_line_requests_complete_with_truthful_source_coverage() {
    let path = workdir("six-line-completion").join("six-lines.txt");
    std::fs::write(&path, "zero\none\ntwo\nthree\nfour\nfive\n").unwrap();
    let path = path.to_str().unwrap();
    type Case<'a> = (&'a [&'a str], &'a [u64], &'a [&'a str], &'a str, &'a str);
    let cases: [Case<'_>; 4] = [
        (
            &[],
            &[1, 2, 3, 4, 5, 6],
            &["zero", "one", "two", "three", "four", "five"],
            "none",
            "exact",
        ),
        (
            &["--pages"],
            &[1, 2, 3, 4, 5],
            &["zero", "one", "two", "three", "four"],
            "selection",
            "partial",
        ),
        (
            &["--pages", "2"],
            &[1, 2],
            &["zero", "one"],
            "selection",
            "partial",
        ),
        (
            &["--pages", "3-4,6"],
            &[3, 4, 6],
            &["two", "three", "five"],
            "selection",
            "partial",
        ),
    ];
    for (selectors, indices, texts, omitted, completeness) in cases {
        let mut arguments = vec!["extract", path, "--no-ocr"];
        arguments.extend_from_slice(selectors);
        let output = run(&arguments);
        assert_eq!(output.status.code(), Some(0), "{selectors:?}: {output:?}");
        let report = json(&output);
        assert_eq!(report["metadata"]["status"], "complete", "{selectors:?}");
        assert_eq!(report["completeness"], completeness, "{selectors:?}");
        assert_eq!(
            report["selection"]["completeness"], completeness,
            "{selectors:?}"
        );
        assert_eq!(report["selection"]["omitted"], omitted, "{selectors:?}");
        assert_eq!(
            report["selection"]["returned_indices"],
            serde_json::json!(indices)
        );
        let actual: Vec<&str> = report["content"]["units"]
            .as_array()
            .unwrap()
            .iter()
            .map(|unit| unit["text"].as_str().unwrap())
            .collect();
        assert_eq!(actual.as_slice(), texts, "{selectors:?}");
    }
}

/// Verifies malformed, conflicting, deprecated-zero, and out-of-range selector exits.
#[test]
fn selector_usage_and_coverage_errors_have_distinct_exits() {
    let path = lines("selector-errors");
    let path = path.to_str().unwrap();
    for pages in ["0", "-1", "4-2", "1,,2", "not-a-range"] {
        let output = run(&["extract", path, "--pages", pages, "--no-ocr"]);
        assert_eq!(output.status.code(), Some(64), "{pages}: {output:?}");
        assert!(!output.stderr.is_empty(), "{pages}");
    }

    let conflict = run(&["extract", path, "--pages", "2", "--max-pages", "2"]);
    assert_eq!(conflict.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("cannot be used with"));

    let zero = run(&["extract", path, "--max-pages", "0"]);
    assert_eq!(zero.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&zero.stderr).contains("positive integer"));

    let missing = run(&["extract", path, "--pages", "8-9", "--no-ocr"]);
    assert_eq!(missing.status.code(), Some(1));
    let missing = json(&missing);
    assert_eq!(missing["metadata"]["status"], "failed");
    assert_eq!(missing["completeness"], "unavailable");
    assert_eq!(missing["content"]["units"], serde_json::json!([]));
}

/// Verifies deprecated page and packet flags retain behavior while warning on stderr.
#[test]
fn deprecated_flags_warn_without_changing_full_extraction() {
    let path = lines("deprecated");
    let output = run(&[
        "extract",
        path.to_str().unwrap(),
        "--max-pages",
        "99",
        "--packet",
        "--no-ocr",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--max-pages is deprecated"));
    assert!(stderr.contains("--packet is deprecated"));
    assert_eq!(
        json(&output)["content"]["units"].as_array().unwrap().len(),
        7
    );
}

/// Verifies canonical extraction persists Markdown and the identical v2 JSON report.
#[test]
fn output_persists_selected_markdown_and_v2_json() {
    let path = lines("output");
    let directory = workdir("output-artifacts");
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args([
            "extract",
            "--batch",
            "--pages",
            "2-3",
            "--no-ocr",
            "--output-dir",
        ])
        .arg(&directory)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        serde_json::json!({"id":"selected","path":path})
    )
    .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let response = json(&output);
    let report = &response["report"];
    assert_eq!(report["metadata"]["status"], "complete");
    assert_eq!(report["completeness"], "partial");
    assert_eq!(report["selection"]["omitted"], "selection");
    assert_eq!(report["metadata"]["artifacts"].as_array().unwrap().len(), 2);
    let markdown_artifact = report["metadata"]["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "markdown_content")
        .unwrap();
    let json_artifact = report["metadata"]["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "extraction_json")
        .unwrap();
    let markdown = std::fs::read_to_string(markdown_artifact["path"].as_str().unwrap()).unwrap();
    assert!(markdown.contains("one"));
    assert!(markdown.contains("two"));
    assert!(!markdown.contains("zero"));
    let saved: Value =
        serde_json::from_slice(&std::fs::read(json_artifact["path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(saved["selection"], report["selection"]);
    assert_eq!(
        saved["metadata"]["identity"]["path"],
        report["metadata"]["identity"]["path"]
    );
}

/// Verifies extraction batch emits ordered reports and preserves later work after a failure.
#[test]
fn batch_emits_v2_reports_and_recovers_after_out_of_range() {
    let path = lines("batch");
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["extract", "--batch", "--no-ocr", "--pages", "99"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"id":"first","path":path})).unwrap();
    writeln!(input, "{}", serde_json::json!({"id":"second","path":path})).unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], "first");
    assert_eq!(responses[1]["id"], "second");
    for response in responses {
        assert_eq!(response["report"]["kind"], "extract");
        assert_eq!(response["report"]["metadata"]["status"], "complete");
        assert_eq!(
            response["report"]["content"]["units"]
                .as_array()
                .unwrap()
                .len(),
            7
        );
    }
}

/// Verifies inspect stays content-free while extract emits content for the same file.
#[test]
fn inspect_remains_metadata_only_beside_canonical_extract() {
    let path = lines("inspection-boundary");
    let inspected = run(&["inspect", path.to_str().unwrap()]);
    assert_eq!(inspected.status.code(), Some(0));
    let inspected = json(&inspected);
    assert_eq!(inspected["kind"], "inspect");
    assert!(inspected.get("content").is_none());
    assert!(inspected.get("text").is_none());
    assert!(
        !String::from_utf8_lossy(&run(&["inspect", path.to_str().unwrap()]).stdout)
            .contains("zero")
    );

    let extracted = run(&["extract", path.to_str().unwrap(), "--no-ocr"]);
    assert_eq!(extracted.status.code(), Some(0));
    assert_eq!(json(&extracted)["content"]["units"][0]["text"], "zero");
}

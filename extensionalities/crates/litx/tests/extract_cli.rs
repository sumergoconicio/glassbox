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
    assert_eq!(missing["status"], "failed");
    assert_eq!(missing["metadata"]["status"], "complete");
    assert!(missing["metadata"].get("diagnostics").is_none());
    assert!(
        missing["diagnostics"]
            .as_array()
            .is_some_and(|items| items.iter().any(|d| d["stage"] == "selection"))
    );
    assert_eq!(missing["completeness"], "unavailable");
    assert_eq!(missing["content"]["units"], serde_json::json!([]));

    // GB-AUD-17: extracting an out-of-range page from a valid PDF fails the
    // extraction request while keeping nested inspection metadata complete.
    let pdf_missing = run(&[
        "extract",
        "../../fixtures/native.pdf",
        "--pages",
        "99-99",
        "--no-ocr",
    ]);
    assert_eq!(pdf_missing.status.code(), Some(1));
    let pdf_missing = json(&pdf_missing);
    assert_eq!(pdf_missing["status"], "failed");
    assert_eq!(pdf_missing["metadata"]["status"], "complete");
    assert!(pdf_missing["metadata"].get("diagnostics").is_none());
    assert!(
        pdf_missing["diagnostics"]
            .as_array()
            .is_some_and(|items| items.iter().any(|d| d["stage"] == "selection"))
    );
    assert_eq!(pdf_missing["completeness"], "unavailable");
    assert_eq!(pdf_missing["content"]["units"], serde_json::json!([]));
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

/// Verifies canonical extraction persists Markdown and the identical v3 JSON report.
#[test]
fn output_persists_selected_markdown_and_v3_json() {
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
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["status"], "complete");
    assert_eq!(report["metadata"]["status"], "complete");
    assert_eq!(report["completeness"], "partial");
    assert_eq!(report["selection"]["omitted"], "selection");
    assert_eq!(report["artifacts"].as_array().unwrap().len(), 2);
    let markdown_artifact = report["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "markdown_content")
        .unwrap();
    let json_artifact = report["artifacts"]
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
    assert_eq!(saved["schema_version"], 3);
    assert_eq!(saved["selection"], report["selection"]);
    assert_eq!(
        saved["metadata"]["file"]["path"],
        report["metadata"]["file"]["path"]
    );
}

/// Verifies extraction batch emits ordered v3 reports and preserves later work.
#[test]
fn batch_emits_v3_reports_and_recovers_after_out_of_range() {
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
        assert_eq!(response["report"]["schema_version"], 3);
        assert_eq!(response["report"]["kind"], "extract");
        assert_eq!(response["report"]["status"], "complete");
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
    assert_eq!(inspected["schema_version"], 3);
    assert_eq!(inspected["kind"], "inspect");
    assert!(inspected.get("content").is_none());
    assert!(inspected.get("text").is_none());
    assert!(
        !String::from_utf8_lossy(&run(&["inspect", path.to_str().unwrap()]).stdout)
            .contains("zero")
    );

    let extracted = run(&["extract", path.to_str().unwrap(), "--no-ocr"]);
    assert_eq!(extracted.status.code(), Some(0));
    let extracted = json(&extracted);
    assert_eq!(extracted["schema_version"], 3);
    assert_eq!(extracted["content"]["units"][0]["text"], "zero");
}

/// Verifies `bodymatter:v1` markers emit by default and `--no-markers` strips stdout and `--output-dir` `.md`.
///
/// Accepts no parameters. Returns unit after checking default PDF/XLSX Markdown
/// markers, `--no-markers` stdout stripping, and `--no-markers` combined with
/// `--format json --output-dir`. Synthetic headings, leaked markers, or missing
/// GFM pipe tables panic.
#[test]
fn extract_markdown_markers_default_on_and_no_markers_strips_stdout_and_saved_md() {
    let default_pdf = run(&[
        "extract",
        "../../fixtures/multipage.pdf",
        "--no-ocr",
        "--format",
        "markdown",
    ]);
    assert_eq!(default_pdf.status.code(), Some(0));
    let pdf_md = String::from_utf8(default_pdf.stdout).unwrap();
    assert!(
        pdf_md.contains("<!-- bodymatter:v1 <page number=\"1\" origin=\"native\"/> -->"),
        "{pdf_md}"
    );
    assert!(!pdf_md.contains("# Glassbox extraction"), "{pdf_md}");
    assert!(!pdf_md.contains("## Unit "), "{pdf_md}");

    let stripped_pdf = run(&[
        "extract",
        "../../fixtures/multipage.pdf",
        "--no-ocr",
        "--format",
        "markdown",
        "--no-markers",
    ]);
    assert_eq!(stripped_pdf.status.code(), Some(0));
    let stripped_md = String::from_utf8(stripped_pdf.stdout).unwrap();
    assert!(!stripped_md.contains("<!-- bodymatter:v1"), "{stripped_md}");
    assert!(
        stripped_md.contains("Final page confirms full-document extraction."),
        "{stripped_md}"
    );

    let out_dir = workdir("no-markers-output");
    let json_with_out = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args([
            "extract",
            "../../fixtures/multipage.pdf",
            "--no-ocr",
            "--no-markers",
            "--format",
            "json",
            "--output-dir",
        ])
        .arg(&out_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert_eq!(json_with_out.status.code(), Some(0));
    let report = json(&json_with_out);
    assert_eq!(report["schema_version"], 3);
    let md_path = report["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["kind"] == "markdown_content")
        .unwrap()["path"]
        .as_str()
        .unwrap();
    let saved_md = std::fs::read_to_string(md_path).unwrap();
    assert_eq!(saved_md, stripped_md);
    assert!(!saved_md.contains("<!-- bodymatter:v1"), "{saved_md}");

    let xlsx_md = String::from_utf8(
        run(&[
            "extract",
            "../../fixtures/grid.xlsx",
            "--no-ocr",
            "--format",
            "markdown",
        ])
        .stdout,
    )
    .unwrap();
    assert!(
        xlsx_md.contains("<!-- bodymatter:v1 <sheet number=\"1\" name=\"Summary\"/> -->"),
        "{xlsx_md}"
    );
    assert!(xlsx_md.contains("| ---"), "{xlsx_md}");
}

/// Verifies `--verbose` and `--full-hash` on single-file extract including `fp-` output stems.
///
/// Accepts no parameters and creates a temporary sparse file (`128 MiB + 1` byte)
/// under the ignored target directory, cleaning it up before returning. Missing
/// provenance under `--verbose`, unexpected provenance by default, missing
/// `fp-` stems, or ignored `--full-hash` panics.
#[test]
fn extract_verbose_and_full_hash_behave_consistently_with_inspect() {
    use glassbox_inspect::hashing::{DEFAULT_FULL_HASH_MAX_BYTES, SAMPLED_FINGERPRINT_SCHEME};
    use std::io::{Seek, SeekFrom};

    let quiet = json(&run(&["extract", "../../fixtures/hello.txt", "--no-ocr"]));
    assert!(quiet.get("provenance").is_none());
    assert!(quiet["metadata"].get("provenance").is_none());

    let verbose = json(&run(&[
        "extract",
        "../../fixtures/hello.txt",
        "--no-ocr",
        "--verbose",
    ]));
    assert!(
        verbose["provenance"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "{verbose}"
    );
    assert!(
        verbose["metadata"]["provenance"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "{verbose}"
    );

    let directory = workdir("extract-sparse-hash");
    let large = directory.join("large-media.bin");
    let out_dir = directory.join("out");
    let total_len = DEFAULT_FULL_HASH_MAX_BYTES + 1;
    {
        let mut file = std::fs::File::create(&large).unwrap();
        file.set_len(total_len).unwrap();
        file.write_all(b"SPARSE-EXTRACT-HEAD").unwrap();
        file.seek(SeekFrom::Start(total_len - 16)).unwrap();
        file.write_all(b"SPARSE-EXT-TAIL!").unwrap();
        file.sync_all().unwrap();
    }

    let sampled_out = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args([
            "extract",
            large.to_str().unwrap(),
            "--no-ocr",
            "--output-dir",
        ])
        .arg(&out_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(matches!(sampled_out.status.code(), Some(0) | Some(2)));
    let sampled = json(&sampled_out);
    assert_eq!(sampled["schema_version"], 3);
    assert!(sampled["metadata"]["file"].get("sha256").is_none());
    let fp = sampled["metadata"]["file"]["fingerprint"]["value"]
        .as_str()
        .unwrap();
    assert_eq!(
        sampled["metadata"]["file"]["fingerprint"]["scheme"],
        SAMPLED_FINGERPRINT_SCHEME
    );
    let expected_stem = format!("fp-{}", &fp[..12]);
    assert!(
        sampled["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|artifact| artifact["path"].as_str().unwrap().contains(&expected_stem)),
        "{sampled}"
    );

    let full_out = run(&[
        "extract",
        large.to_str().unwrap(),
        "--no-ocr",
        "--full-hash",
    ]);
    assert!(matches!(full_out.status.code(), Some(0) | Some(2)));
    let full = json(&full_out);
    assert_eq!(
        full["metadata"]["file"]["sha256"].as_str().unwrap().len(),
        64
    );
    assert!(full["metadata"]["file"].get("fingerprint").is_none());

    let _ = std::fs::remove_dir_all(&directory);
}

// Module: glassbox-cli shared jobs integration tests.
// File: batch_jobs.rs.
// Responsibility: Exercise real file/OCR budgets, bounded JSONL behavior, directory ordering, and output cancellation.

use glassbox_inspect::reports::{ExtractOptions, InspectOptions, NativeSelection, OperationStatus};
use glassbox_inspect::{Inspector, SharedWorkBudget};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Resolves one existing synthetic fixture without reading private or remote data.
///
/// Accepts a fixture filename. Returns its absolute repository path. No file is
/// opened by this helper; missing fixtures fail in the actual operation under test.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Creates one unique private test directory under the ignored target tree.
///
/// Accepts a test label. Returns a new directory path. Creation/permission
/// failures panic; no old evidence is removed, truncated, or traversed.
fn workdir(label: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "batch-jobs-{label}-{}",
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

/// Runs the actual Glassbox binary with bounded supplied stdin.
///
/// Accepts CLI arguments and raw JSONL bytes. Returns captured output. Spawn,
/// stdin write, and process wait failures panic; expected nonzero statuses remain
/// available for assertions and are not hidden by this helper.
fn run(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(input).unwrap();
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

/// Decodes strict JSONL without accepting blank or informational output lines.
///
/// Accepts captured process output. Returns ordered JSON records. Invalid UTF-8,
/// malformed JSON, or blank lines panic with process stderr for diagnosis.
fn jsonl(output: &Output) -> Vec<Value> {
    let text = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(!text.lines().any(|line| line.trim().is_empty()));
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Verifies actual mixed file/OCR operations never exceed the shared N-slot lease.
///
/// Accepts no parameters. Runs jobs 1, 2, and 4 against repeated native text,
/// native PDF, and raster OCR inputs. Returns unit after join; counter mismatch,
/// unfinished recognition, or peak>N panics with the actual snapshot.
#[test]
fn instrumented_real_file_and_ocr_budget_peaks_stay_within_jobs() {
    for jobs in [1, 2, 4] {
        let budget = SharedWorkBudget::new(jobs).unwrap();
        let text = fixture("hello.txt");
        let pdf = fixture("multipage.pdf");
        let image = fixture("document-image.png");
        std::thread::scope(|scope| {
            for worker in 0..(jobs * 2) {
                let budget = budget.clone();
                let text = text.clone();
                let pdf = pdf.clone();
                let image = image.clone();
                scope.spawn(move || {
                    let mut inspector = Inspector::with_shared_budget(budget);
                    for round in 0..3 {
                        let mut options = ExtractOptions {
                            selection: NativeSelection::All,
                            ..ExtractOptions::default()
                        };
                        let source = match (worker + round) % 3 {
                            0 => &text,
                            1 => &pdf,
                            _ => &image,
                        };
                        if source == &image && round == 1 {
                            options.ocr.language = "eng".into();
                            options.ocr.raster_long_edge = 1024;
                        }
                        let report = inspector.extract(source, &options);
                        assert_ne!(report.metadata.status, OperationStatus::Unknown);
                        let inspected = inspector.inspect(source, &InspectOptions::default());
                        assert!(inspected.metadata.digest.bytes_hashed.value.is_some());
                    }
                });
            }
        });
        let snapshot = budget.snapshot();
        assert!(snapshot.peak_active <= jobs, "{snapshot:?}");
        assert_eq!(snapshot.active, 0, "{snapshot:?}");
        assert_eq!(snapshot.files_started, snapshot.files_finished);
        assert_eq!(snapshot.ocr_started, snapshot.ocr_finished);
        assert!(snapshot.ocr_started > 0, "{snapshot:?}");
        assert_eq!(snapshot.files_started, (jobs * 2 * 3 * 2) as u64);
    }
}

/// Verifies repeated overlapping operations isolate OCR options and recover models.
///
/// Accepts no parameters. Returns unit after an unavailable-language request and
/// ordinary English retry on the same image/session. Cross-request model reuse,
/// missing-model success, or wrong reported language panics.
#[test]
fn repeated_same_file_does_not_contaminate_model_or_option_sessions() {
    let image = fixture("document-image.png");
    let budget = SharedWorkBudget::new(2).unwrap();
    let mut inspector = Inspector::with_shared_budget(budget.clone());
    let missing = ExtractOptions {
        ocr: glassbox_inspect::reports::OcrOptions {
            language: "definitely_missing_glassbox_model".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let failed = inspector.extract(&image, &missing);
    assert!(failed.metadata.diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("unavailable") && diagnostic.message.contains("no downloads")
    }));
    let before_ordinary = budget.snapshot().ocr_started;
    let ordinary = inspector.extract(&image, &ExtractOptions::default());
    assert!(!ordinary.metadata.diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("definitely_missing_glassbox_model")
    }));
    assert_eq!(budget.snapshot().ocr_started, before_ordinary + 1);
    let no_ocr = inspector.extract(
        &image,
        &ExtractOptions {
            ocr: glassbox_inspect::reports::OcrOptions {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert!(no_ocr.content.units[0].provenance[0].ocr.is_none());
    assert_eq!(budget.snapshot().ocr_started, before_ordinary + 1);
}

/// Verifies public jobs validation and ordered severity across real JSONL entrypoints.
///
/// Accepts no parameters. Returns unit after invalid 0/negative/overflow checks
/// and jobs 1/2/4 mixed batches. Wrong status, IDs, sequence, continuation, or
/// per-request error classification panics without filtering failed requests.
#[test]
fn cli_jobs_matrix_preserves_order_ids_line_errors_and_status_severity() {
    for invalid in ["0", "-1", "999999999999999999999999999999999999"] {
        let output = run(
            &["inspect", "../../fixtures/hello.txt", "--jobs", invalid],
            b"",
        );
        assert_eq!(output.status.code(), Some(64), "{invalid}");
        assert!(output.stdout.is_empty());
    }
    let valid = |id: &str| {
        format!(
            "{{\"id\":{},\"path\":\"{}\"}}\n",
            serde_json::to_string(id).unwrap(),
            fixture("hello.txt").display()
        )
        .into_bytes()
    };
    let missing = format!(
        "{{\"id\":\"same\",\"path\":\"{}\"}}\n",
        fixture("absent-does-not-exist.txt").display()
    )
    .into_bytes();
    let oversized = format!("{{\"id\":\"large\",\"path\":\"{}\"}}\n", "x".repeat(70_000));
    for jobs in ["1", "2", "4"] {
        let mut input = valid("same");
        input.extend(missing.clone());
        input.extend(b"not-json\n");
        input.extend(oversized.as_bytes());
        input.extend(valid("same"));
        let output = run(&["inspect", "--batch", "--jobs", jobs], &input);
        assert_eq!(output.status.code(), Some(64), "{jobs}");
        let records = jsonl(&output);
        assert_eq!(records.len(), 5);
        assert_eq!(records[0]["id"], "same");
        assert_eq!(records[1]["id"], "same");
        assert_eq!(records[2]["status"], "invalid");
        assert_eq!(records[3]["status"], "invalid");
        assert_eq!(records[3]["id"], Value::Null);
        assert_eq!(records[4]["id"], "same");
        assert_eq!(records[4]["report"]["metadata"]["status"], "complete");
        assert!(
            records
                .iter()
                .enumerate()
                .all(|(index, record)| { record["sequence"] == index + 1 })
        );
    }
}

/// Verifies a completed response flushes while stdin remains open at jobs=4.
///
/// Accepts no parameters. Returns unit after reading one ordered response before
/// EOF and a second after later input. Missing early output, wrong ID, or process
/// failure panics; stdin is closed explicitly so workers can join.
#[test]
fn jobs_four_flushes_before_eof_with_external_ids() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "--batch", "--jobs", "4"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let request = format!(
        "{{\"id\":\"early\",\"path\":\"{}\"}}\n",
        fixture("hello.txt").display()
    );
    input.write_all(request.as_bytes()).unwrap();
    input.flush().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let first: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(first["id"], "early");
    input
        .write_all(request.replace("early", "late").as_bytes())
        .unwrap();
    input.flush().unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    let second: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(second["id"], "late");
    drop(input);
    let status = child.wait().unwrap();
    assert!(status.success());
}

/// Verifies immediate and recursive directory results are byte-identical across jobs.
///
/// Accepts no parameters. Returns unit after jobs=1 and jobs=4 inventories.
/// Reordered records, lost nested files, or status drift panics. All fixtures
/// are ordinary local files in a unique private target directory.
#[test]
fn directory_jobs_preserve_deterministic_streaming_order() {
    let root = workdir("directory");
    std::fs::write(root.join("b.txt"), b"bravo").unwrap();
    std::fs::write(root.join("a.txt"), b"alpha").unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/c.txt"), b"charlie").unwrap();
    for recursive in [false, true] {
        let mut args = vec!["inspect", root.to_str().unwrap(), "--jobs", "1"];
        if recursive {
            args.push("--recursive");
        }
        let sequential = run(&args, b"");
        args[3] = "4";
        let concurrent = run(&args, b"");
        assert_eq!(sequential.status.code(), concurrent.status.code());
        assert_eq!(sequential.stdout, concurrent.stdout);
        let records = jsonl(&concurrent);
        assert_eq!(records.len(), if recursive { 3 } else { 2 });
    }
}

/// Verifies broken stdout cancels admission and joins despite stdin staying open.
///
/// Accepts no parameters. Returns unit after dropping the child's stdout reader
/// while retaining stdin. A non-operational exit or five-second deadlock panics;
/// the child is killed only as test cleanup after a failed bounded wait.
#[test]
fn broken_stdout_with_open_stdin_returns_one_without_worker_deadlock() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "--batch", "--jobs", "4"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let request = format!(
        "{{\"id\":\"broken\",\"path\":\"{}\"}}\n",
        fixture("hello.txt").display()
    );
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(request.as_bytes())
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            panic!("broken stdout deadlocked with open stdin");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
    drop(child.stdin.take());
}

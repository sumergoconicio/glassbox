// Module: glassbox-cli safe inventory and artifact integration tests.
// File: inventory_cli.rs.
// Responsibility: Exercise real directory JSONL, status severity, private artifacts, and collision handling.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Runs the compiled Glassbox binary from the CLI crate directory.
///
/// Accepts argument strings. Returns captured stdout, stderr, and exit status.
/// Spawn failures panic; assertions own all outcome validation.
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap()
}

/// Creates a unique private ignored test directory.
///
/// Accepts a test label. Returns a new mode-0700 target path. Directory or
/// permission failures panic; no prior file is removed or truncated.
fn workdir(label: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "inventory-cli-{label}-{}",
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

/// Decodes one JSON value per stdout line without accepting informational junk.
///
/// Accepts captured process output. Returns ordered JSON records. Invalid UTF-8
/// or JSON panics with process status and stderr for diagnosis.
fn jsonl(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Decodes one canonical JSON report from a single-file command.
///
/// Accepts captured process output. Returns the report. Empty, non-UTF-8, or
/// non-JSON output panics with stderr and status.
fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: status={:?}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Builds an isolated directory with hidden files, nested content, and link loops.
///
/// Accepts no parameters. Returns the private fixture root. Creation failures
/// panic. Every link points only inside this test root; no external path is read.
#[cfg(unix)]
fn linked_tree() -> PathBuf {
    let root = workdir("tree").canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), b"alpha").unwrap();
    std::fs::write(root.join(".hidden.txt"), b"hidden").unwrap();
    std::fs::write(root.join("space ;$(not-run).txt"), b"metacharacters").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/nested.txt"), b"nested").unwrap();
    std::os::unix::fs::symlink(root.join("a.txt"), root.join("file-link")).unwrap();
    std::os::unix::fs::symlink(root.join("sub"), root.join("dir-link")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
    root
}

/// Verifies immediate inventory is deterministic and recursive inspection is opt-in.
///
/// Accepts no parameters. Returns unit after two immediate runs and one recursive
/// run. Missing hidden/metacharacter files, followed links, sequence gaps,
/// nondeterminism, or loop traversal panic.
#[cfg(unix)]
#[test]
fn immediate_and_recursive_inventory_are_deterministic_and_no_follow() {
    let root = linked_tree();
    let root_text = root.to_str().unwrap();
    let first = run(&["inspect", root_text]);
    let second = run(&["inspect", root_text]);
    assert_eq!(first.status.code(), Some(2));
    assert_eq!(second.status.code(), Some(2));
    assert_eq!(first.stdout, second.stdout);
    let records = jsonl(&first);
    assert_eq!(records.len(), 6);
    let paths: Vec<&str> = records
        .iter()
        .map(|record| record["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec![
            root.join(".hidden.txt").to_str().unwrap(),
            root.join("a.txt").to_str().unwrap(),
            root.join("dir-link").to_str().unwrap(),
            root.join("file-link").to_str().unwrap(),
            root.join("loop").to_str().unwrap(),
            root.join("space ;$(not-run).txt").to_str().unwrap(),
        ]
    );
    assert!(
        records
            .iter()
            .enumerate()
            .all(|(index, record)| record["sequence"] == index + 1 && record["id"] == index + 1)
    );
    assert!(
        records
            .iter()
            .filter(|record| record["report"].is_object())
            .all(|record| {
                record["status"] == "complete"
                    && record["report"]["schema_version"] == 3
                    && record["report"]["file"]["sha256"].as_str().unwrap().len() == 64
            })
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["error"]["code"] == "symlink_skipped")
            .count(),
        3
    );
    assert!(!paths.contains(&root.join("sub/nested.txt").to_str().unwrap()));

    let recursive = run(&["inspect", root_text, "--recursive"]);
    assert_eq!(recursive.status.code(), Some(2));
    let records = jsonl(&recursive);
    assert_eq!(records.len(), 7);
    assert!(records.iter().any(|record| {
        record["path"].as_str().unwrap() == root.join("sub/nested.txt").to_str().unwrap()
            && record["report"]["schema_version"] == 3
            && record["report"]["file"]["path"] == root.join("sub/nested.txt").to_str().unwrap()
    }));
    assert_eq!(
        records
            .iter()
            .filter(|record| record["error"]["code"] == "symlink_skipped")
            .count(),
        3
    );
}

/// Verifies identification-only inspection completes while extraction is unsupported.
///
/// Accepts no parameters. Returns unit after DjVu inspect/extract calls. Status
/// confusion, leaked advice fields, guessed OCR states, or schema drift panics.
#[test]
fn identification_only_status_and_readiness_are_distinct() {
    for fixture in ["../../fixtures/single.djvu", "../../fixtures/bundled.djvu"] {
        let inspected = run(&["inspect", fixture]);
        assert_eq!(inspected.status.code(), Some(0), "{fixture}");
        let report = json(&inspected);
        assert_eq!(report["schema_version"], 3);
        assert_eq!(report["status"], "complete");
        assert_eq!(report["format"]["type"], "ebooks");
        assert_eq!(report["readiness"]["extractable"], "identification_only");
        assert_eq!(report["readiness"]["ocr"], "unknown");
        assert!(report.get("advice").is_none());
        assert!(report.get("next_step").is_none());
        let extracted = run(&["extract", fixture, "--no-ocr"]);
        assert_eq!(extracted.status.code(), Some(2), "{fixture}");
        let extracted_json = json(&extracted);
        assert_eq!(extracted_json["schema_version"], 3);
        assert_eq!(extracted_json["status"], "unsupported");
        assert_eq!(extracted_json["metadata"]["status"], "unsupported");
    }
}

/// Verifies directory mode errors and accepts bounded jobs=2 inspection.
///
/// Accepts no parameters. Returns unit after jobs=2, directory extract, and
/// directory text-format requests. Unexpected extraction or nested content panics.
#[test]
fn invalid_directory_modes_return_64_and_extract_rejects_directories() {
    let root = workdir("invalid-modes");
    std::fs::write(root.join("nested.txt"), b"must not be extracted").unwrap();
    let path = root.to_str().unwrap();
    let concurrent = run(&["inspect", path, "--jobs", "2"]);
    assert_eq!(concurrent.status.code(), Some(0));
    assert_eq!(jsonl(&concurrent).len(), 1);
    let args = ["inspect", path, "--format", "text"];
    let output = run(&args);
    assert_eq!(output.status.code(), Some(64), "{args:?}");
    assert!(output.stdout.is_empty());
    let extracted = run(&["extract", path, "--no-ocr"]);
    assert_eq!(extracted.status.code(), Some(1));
    let report = json(&extracted);
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["status"], "failed");
    assert_eq!(report["metadata"]["status"], "failed");
    assert_eq!(report["content"]["units"].as_array().map(Vec::len), Some(0));
    assert!(!String::from_utf8_lossy(&extracted.stdout).contains("must not be extracted"));
}

/// Verifies non-Unicode filenames produce valid diagnostics and later files continue.
///
/// Accepts no parameters. Returns unit after one ordered directory run on Linux
/// (macOS APFS rejects non-UTF-8 filenames at creation). A panic, invalid JSONL
/// record, lost later file, or non-operational aggregate panics.
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_filename_does_not_panic_or_stop_inventory() {
    use std::os::unix::ffi::OsStringExt;
    let root = workdir("non-utf8");
    let non_utf8 = std::ffi::OsString::from_vec(vec![b'a', 0xff]);
    std::fs::write(root.join(&non_utf8), b"opaque").unwrap();
    std::fs::write(root.join("z.txt"), b"valid").unwrap();
    let output = run(&["inspect", root.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let records = jsonl(&output);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["error"]["code"], "non_utf8_path_unsupported");
    assert_eq!(records[0]["status"], "failed");
    assert_eq!(records[1]["report"]["schema_version"], 3);
    assert_eq!(records[1]["report"]["status"], "complete");
}

/// Verifies private no-overwrite metadata artifacts and metadata-only summaries.
///
/// Accepts no parameters. Returns unit after a successful save and exact retry.
/// Public permissions, symlink paths, changed existing bytes, body leakage, or
/// successful collision handling panics.
#[cfg(unix)]
#[test]
fn metadata_artifacts_are_private_complete_and_collision_safe() {
    use std::os::unix::fs::PermissionsExt;
    let root = workdir("artifacts");
    let source = root.join("space ;$(not-run).txt");
    let output_dir = root.join("saved");
    std::fs::write(&source, b"UNIQUE BODY SENTINEL").unwrap();
    let args = ["inspect", source.to_str().unwrap(), "--output-dir"];
    let first = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(args)
        .arg(&output_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report = json(&first);
    assert_eq!(report["schema_version"], 3);
    let artifacts = report["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2);
    assert_eq!(
        std::fs::metadata(&output_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let mut before = Vec::new();
    for artifact in artifacts {
        let path = Path::new(artifact["path"].as_str().unwrap());
        assert!(path.is_absolute());
        assert!(!path.file_name().unwrap().to_string_lossy().contains(' '));
        assert!(!path.file_name().unwrap().to_string_lossy().contains(';'));
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let bytes = std::fs::read(path).unwrap();
        if artifact["kind"] == "metadata_summary" {
            let summary = String::from_utf8(bytes.clone()).unwrap();
            assert!(summary.contains("# Glassbox inspection"), "{summary}");
            assert!(summary.contains("- Schema: 3"), "{summary}");
            assert!(!summary.contains("UNIQUE BODY SENTINEL"));
        } else {
            let saved: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(saved["schema_version"], 3);
            assert_eq!(saved["file"]["path"], report["file"]["path"]);
        }
        before.push((path.to_path_buf(), bytes));
    }
    let second = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(args)
        .arg(&output_dir)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(1));
    assert_eq!(json(&second)["status"], "failed");
    for (path, bytes) in before {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(std::fs::read_dir(&output_dir).unwrap().count(), 2);
}

/// Verifies output-directory symlinks and public existing directories are rejected.
///
/// Accepts no parameters. Returns unit after link and mode-0755 attempts. Any
/// write through the link, changed destination, or successful public save panics.
#[cfg(unix)]
#[test]
fn output_rejects_symlink_components_and_public_directories() {
    let root = workdir("unsafe-output");
    let real = root.join("real");
    let link = root.join("link");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let linked = run(&[
        "inspect",
        "../../fixtures/hello.txt",
        "--output-dir",
        link.to_str().unwrap(),
    ]);
    assert_eq!(linked.status.code(), Some(1));
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
    let public = root.join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::set_permissions(&public, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let rejected = run(&[
        "inspect",
        "../../fixtures/hello.txt",
        "--output-dir",
        public.to_str().unwrap(),
    ]);
    assert_eq!(rejected.status.code(), Some(1));
    assert_eq!(std::fs::read_dir(public).unwrap().count(), 0);
}

/// Verifies concurrent identical saves have one winner and never truncate bytes.
///
/// Accepts no parameters. Returns unit after two simultaneous inspect processes.
/// More than one success, changed artifact bytes, temporary names, or extra files panic.
#[cfg(unix)]
#[test]
fn concurrent_artifact_writers_are_exclusive() {
    let root = workdir("concurrent");
    let output = root.join("saved");
    std::fs::create_dir(&output).unwrap();
    std::fs::set_permissions(&output, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let command = || {
        Command::new(env!("CARGO_BIN_EXE_glassbox"))
            .args(["inspect", "../../fixtures/hello.txt", "--output-dir"])
            .arg(&output)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = command();
    let second = command();
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    let statuses = [first.status.code(), second.status.code()];
    assert!(statuses.contains(&Some(0)), "{statuses:?}");
    assert!(statuses.contains(&Some(1)), "{statuses:?}");
    let files: Vec<PathBuf> = std::fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2);
    assert!(
        files
            .iter()
            .all(|path| !path.file_name().unwrap().to_string_lossy().starts_with('.'))
    );
}

/// Verifies deep recursive inventory preserves sorted order, pinned identity, and depth limits.
///
/// Accepts no parameters. Returns unit after running `glassbox inspect --recursive --jobs 2`
/// on a tree spanning sibling branches and a chain exceeding the 256-level depth cap.
/// Out-of-order records, missing digests, unguarded depth traversal, or wrong exit status
/// panics; the temporary tree is cleaned up afterward.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn deep_recursive_inventory_preserves_order_and_guards_depth() {
    let root = workdir("deep-recursive").canonicalize().unwrap();
    std::fs::write(root.join("00_first.txt"), b"first").unwrap();
    let mut deepest = root.join("branch");
    for _ in 1..256 {
        deepest.push("d");
    }
    let overflow_dir = deepest.join("m_overflow");
    std::fs::create_dir_all(&overflow_dir).unwrap();
    std::fs::write(deepest.join("a_leaf.txt"), b"leaf").unwrap();
    std::fs::write(overflow_dir.join("hidden.txt"), b"must-not-visit").unwrap();
    std::fs::write(root.join("zz_last.txt"), b"last").unwrap();

    let output = run(&[
        "inspect",
        root.to_str().unwrap(),
        "--recursive",
        "--jobs",
        "2",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records = jsonl(&output);
    assert_eq!(records.len(), 4);
    let paths: Vec<&str> = records
        .iter()
        .map(|record| record["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec![
            root.join("00_first.txt").to_str().unwrap(),
            deepest.join("a_leaf.txt").to_str().unwrap(),
            overflow_dir.to_str().unwrap(),
            root.join("zz_last.txt").to_str().unwrap(),
        ]
    );
    assert_eq!(records[0]["report"]["status"], "complete");
    assert_eq!(records[1]["report"]["status"], "complete");
    assert_eq!(
        records[1]["report"]["file"]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(records[2]["status"], "partial");
    assert_eq!(records[2]["error"]["code"], "directory_depth_exceeded");
    assert_eq!(records[3]["report"]["status"], "complete");
    assert!(
        records
            .iter()
            .enumerate()
            .all(|(index, record)| record["sequence"] == index + 1 && record["id"] == index + 1)
    );

    let _ = std::fs::remove_dir_all(&root);
}

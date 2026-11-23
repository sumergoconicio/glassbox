// Module: glassbox-cli CLI hardening regression tests.
// File: hardening_cli.rs.
// Responsibility: Verify documented --jobs bounds and explicit directory-mode --pretty rejection.

use std::path::PathBuf;
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

/// Creates one unique private test directory under the ignored target tree.
///
/// Accepts a test label. Returns a new mode-0700 directory path. Creation or
/// permission failures panic; no prior file is removed or truncated.
fn workdir(label: &str) -> PathBuf {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "hardening-cli-{label}-{}",
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

/// Verifies out-of-bounds `--jobs` values are rejected at CLI parse.
///
/// Accepts no parameters. Returns unit after jobs 0 and 33 attempts on inspect
/// and extract plus one jobs=32 inspection. An accepted out-of-range value, a
/// missing range message, or a rejected maximum value panics.
#[test]
fn jobs_out_of_bounds_is_rejected_with_the_documented_range() {
    for command in ["inspect", "extract"] {
        for jobs in ["0", "33"] {
            let output = run(&[command, "../../fixtures/hello.txt", "--jobs", jobs]);
            assert_eq!(output.status.code(), Some(64), "{command} {jobs}");
            assert!(output.stdout.is_empty(), "{command} {jobs}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("1..=32"), "{command} {jobs}: {stderr}");
        }
    }
    let accepted = run(&["inspect", "../../fixtures/hello.txt", "--jobs", "32"]);
    assert_eq!(accepted.status.code(), Some(0));
}

/// Verifies `--pretty` on a directory is an explicit usage error.
///
/// Accepts no parameters. Returns unit after a rejected directory `--pretty`
/// request and the still-supported plain directory, single-file `--pretty`,
/// and rejected directory text-format requests. A silent success, lost
/// JSONL contract message, or a broken ordinary invocation panics.
#[test]
fn pretty_directory_inspect_is_rejected_as_a_usage_error() {
    let root = workdir("pretty");
    std::fs::write(root.join("a.txt"), b"alpha").unwrap();
    let rejected = run(&["inspect", root.to_str().unwrap(), "--pretty"]);
    assert_eq!(rejected.status.code(), Some(64));
    assert!(rejected.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("--pretty"), "{stderr}");
    assert!(stderr.contains("JSONL"), "{stderr}");
    let ordinary = run(&["inspect", root.to_str().unwrap()]);
    assert_eq!(ordinary.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&ordinary.stdout).lines().count(), 1);
    let single = run(&["inspect", root.join("a.txt").to_str().unwrap(), "--pretty"]);
    assert_eq!(single.status.code(), Some(0));
    let rendered = String::from_utf8_lossy(&single.stdout);
    assert!(
        rendered.contains("\n  "),
        "single-file --pretty stays indented"
    );
    let text = run(&["inspect", root.to_str().unwrap(), "--format", "text"]);
    assert_eq!(text.status.code(), Some(64));
}

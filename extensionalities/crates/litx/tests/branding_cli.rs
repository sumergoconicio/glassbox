// Module: glassbox-cli integration tests.
// File: branding_cli.rs.
// Responsibility: Prove Glassbox branding, upstream parse routing, downstream routes, and litx compatibility.
use std::io::Write;
use std::process::{Command, Stdio};

const BINARIES: [&str; 2] = [env!("CARGO_BIN_EXE_glassbox"), env!("CARGO_BIN_EXE_litx")];

/// Check both executable identities, operation discovery, and upstream version identity.
///
/// Accepts no parameters and launches each compiled executable with help and version arguments.
/// Returns unit after asserting Glassbox branding and identical versions.
/// Panics when a process cannot launch, returns failure, emits non-UTF8 help, or loses a required operation.
#[test]
fn both_commands_expose_glassbox_branding_and_the_same_version() {
    let mut versions = Vec::new();
    for binary in BINARIES {
        let help = Command::new(binary).arg("--help").output().unwrap();
        assert!(help.status.success());
        let help = String::from_utf8(help.stdout).unwrap();
        assert!(help.contains("What's in the box?!"));
        assert!(help.contains("Usage: glassbox"));
        for command in ["parse", "inspect", "extract", "defaults"] {
            assert!(help.contains(command));
        }
        let version = Command::new(binary).arg("--version").output().unwrap();
        assert!(version.status.success());
        let version = String::from_utf8(version.stdout).unwrap();
        assert_eq!(
            version,
            format!("glassbox {}\n", env!("GLASSBOX_UPSTREAM_VERSION"))
        );
        versions.push(version);
    }
    assert_eq!(versions[0], versions[1]);
}

/// Exercise direct, nested, and hidden upstream parse routes through both executable names.
///
/// Accepts no parameters and uses the tracked native PDF fixture with OCR disabled.
/// Returns unit after asserting generated upstream help and byte-identical alias output.
/// Panics on process failures, missing generated flags, unsuccessful parsing, or output divergence.
#[test]
fn upstream_parse_routes_include_generated_flags_hidden_commands_and_alias_parity() {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/native.pdf");
    let mut parsed = Vec::new();
    for binary in BINARIES {
        let parse_help = Command::new(binary)
            .args(["parse", "--help"])
            .output()
            .unwrap();
        assert!(parse_help.status.success());
        let parse_help = String::from_utf8(parse_help.stdout).unwrap();
        assert!(parse_help.contains("Usage: glassbox parse"));
        assert!(parse_help.contains("--target-pages"));
        assert!(parse_help.contains("--extract-structure-tree"));

        let hidden_help = Command::new(binary)
            .args(["parse", "image-bounds", "--help"])
            .output()
            .unwrap();
        assert!(hidden_help.status.success());
        assert!(
            String::from_utf8(hidden_help.stdout)
                .unwrap()
                .contains("Usage: glassbox image-bounds")
        );

        let output = Command::new(binary)
            .args(["parse", fixture.to_str().unwrap(), "--no-ocr", "--quiet"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(!output.stdout.is_empty());
        parsed.push(output);
    }
    assert_eq!(parsed[0].stdout, parsed[1].stdout);
    assert_eq!(parsed[0].stderr, parsed[1].stderr);
    assert_eq!(parsed[0].status, parsed[1].status);
}

/// Checks both aliases preserve metadata inspect, explicit extract, and defaults.
///
/// Accepts no parameters. Returns unit after byte-equivalent JSON comparisons.
/// Process failures or divergent alias reports panic.
#[test]
fn compatibility_command_preserves_inspection_extraction_and_defaults() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hello.txt");
    for arguments in [
        vec!["inspect".as_ref(), fixture.as_os_str()],
        vec!["extract".as_ref(), fixture.as_os_str()],
        vec!["defaults".as_ref()],
    ] {
        let reports: Vec<serde_json::Value> = BINARIES
            .iter()
            .map(|binary| {
                let output = Command::new(binary).args(&arguments).output().unwrap();
                assert!(output.status.success());
                serde_json::from_slice(&output.stdout).unwrap()
            })
            .collect();
        assert_eq!(reports[0], reports[1]);
        if arguments[0] != "defaults" {
            assert_eq!(reports[0]["schema_version"], 3);
        }
    }
}

/// Checks both aliases preserve schema v3 JSONL responses and request errors.
///
/// Accepts no parameters. Returns unit after malformed and repeated requests.
/// Aggregate status is 64 for malformed input; response divergence panics.
#[test]
fn compatibility_command_preserves_batch_responses_and_request_errors() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hello.txt");
    let mut responses = Vec::new();
    for binary in BINARIES {
        let mut child = Command::new(binary)
            .args(["inspect", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        writeln!(input, "{{not-json").unwrap();
        for id in ["first", "repeat"] {
            writeln!(input, "{}", serde_json::json!({"id": id, "path": fixture})).unwrap();
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(64));
        let lines: Vec<serde_json::Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0]["error"].is_object());
        assert_eq!(lines[1]["id"], "first");
        assert_eq!(lines[2]["id"], "repeat");
        assert_eq!(lines[1]["report"]["schema_version"], 3);
        assert_eq!(lines[1]["report"], lines[2]["report"]);
        responses.push(lines);
    }
    assert_eq!(responses[0], responses[1]);
}

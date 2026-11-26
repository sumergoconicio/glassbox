//! Module: Glassbox media contact-sheet CLI integration tests.
//! File: media_contact_sheet_cli.rs.
//! Responsibility: Verify `glassbox extract --imagegrid` flag validation, safe publication, v3 receipts, batch mode, error exits, and `glassbox inspect` media-flag rejection.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter keeping concurrent test working directories distinct.
static WORKDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates a unique test-owned working directory under the ignored target tree.
///
/// Accepts a descriptive test label. Returns the newly created private `0700`
/// directory path under `extensionalities/target/tests/media-contact-sheet-cli`.
/// Panics if directory creation or Unix permission setup fails.
fn workdir(label: &str) -> PathBuf {
    let seq = WORKDIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/media-contact-sheet-cli")
        .join(format!(
            "{label}-{}-{}-{seq}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&path).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::canonicalize(path).unwrap()
}

/// Runs the compiled `glassbox` binary with optional environment overrides.
///
/// Accepts CLI argument slices and `(key, value)` environment pairs to inject.
/// Returns the captured process [`Output`]. Panics if launching the test binary
/// fails.
fn run_with_env(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_glassbox"));
    cmd.args(args).current_dir(env!("CARGO_MANIFEST_DIR"));
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

/// Runs the compiled `glassbox` binary with inherited environment variables.
///
/// Accepts CLI argument slices. Returns the captured process [`Output`]. Panics
/// if launching the test binary fails.
fn run(args: &[&str]) -> Output {
    run_with_env(args, &[])
}

/// Decodes stdout as JSON, panicking with stdout and stderr on parse failure.
///
/// Accepts captured process output. Returns the parsed [`serde_json::Value`].
/// Panics with full stdout/stderr context if stdout is not valid JSON.
fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected JSON stdout ({error}): stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Reports whether `ffmpeg` and `ffprobe` are runnable on this host.
///
/// Accepts no arguments. Returns `true` when both `ffmpeg -version` and
/// `ffprobe -version` exit successfully, or `false` when either tool is absent
/// or fails to launch. Never panics.
fn host_has_ffmpeg() -> bool {
    let ffmpeg_ok = Command::new("ffmpeg")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let ffprobe_ok = Command::new("ffprobe")
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    ffmpeg_ok && ffprobe_ok
}

/// Generates a tiny 4-second synthetic color-ramp MP4 fixture using `ffmpeg`.
///
/// Accepts a destination directory and filename. Returns the created MP4 path.
/// Panics if `ffmpeg` fails to encode the synthetic video stream.
fn generate_synthetic_mp4(dir: &Path, filename: &str) -> PathBuf {
    let dest = dir.join(filename);
    let output = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=10:duration=4",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&dest)
        .output()
        .expect("spawn ffmpeg for synthetic MP4");
    assert!(
        output.status.success(),
        "ffmpeg failed to create synthetic MP4: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dest
}

/// Generates a 2-second audio-only WAV fixture using `ffmpeg`.
///
/// Accepts a destination directory and filename. Returns the created WAV path.
/// Panics if `ffmpeg` fails to encode the synthetic sine-wave audio stream.
fn generate_synthetic_wav(dir: &Path, filename: &str) -> PathBuf {
    let dest = dir.join(filename);
    let output = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&dest)
        .output()
        .expect("spawn ffmpeg for synthetic WAV");
    assert!(
        output.status.success(),
        "ffmpeg failed to create synthetic WAV: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dest
}

/// Asserts that `dir` contains no leftover Glassbox temp files or contact-sheet temp dirs.
///
/// Accepts a directory path to inspect. Returns unit when no entry starts with
/// `.glassbox-tmp-` or `.contact-sheet-`. Panics on directory read failure or
/// if any temporary file/directory leaked.
fn assert_no_temp_leftovers(dir: &Path) {
    let entries: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for name in &entries {
        assert!(
            !name.starts_with(".glassbox-tmp-") && !name.starts_with(".contact-sheet-"),
            "unexpected temporary entry '{name}' left in {}: {entries:?}",
            dir.display()
        );
    }
}

/// Verifies `glassbox inspect` rejects all 16 media flags before `--` with exit `64` and passes literal `--imagegrid` after `--`.
///
/// Accepts no arguments. Returns unit after checking every media flag in bare
/// and `=value` forms against a nonexistent path (confirming no file open),
/// plus inspecting a real file literally named `--imagegrid` after `--`. Panics
/// on any unexpected exit status or missing migration advice.
#[test]
fn inspect_rejects_all_media_flags_and_allows_literal_after_terminator() {
    const MEDIA_FLAGS: &[&str] = &[
        "--imagegrid",
        "--every-seconds",
        "--quality",
        "--performance",
        "--imagegrid-format",
        "--asr",
        "--asr-backend",
        "--asr-url",
        "--asr-model",
        "--asr-api-key-env",
        "--asr-protocol",
        "--asr-model-dir",
        "--asr-device",
        "--asr-language",
        "--extract-audio",
        "--no-frontmatter",
    ];

    for flag in MEDIA_FLAGS {
        let bare = run(&["inspect", flag, "/nonexistent/media-file.mp4"]);
        assert_eq!(bare.status.code(), Some(64), "bare {flag}: {bare:?}");
        let stderr = String::from_utf8_lossy(&bare.stderr);
        assert!(
            stderr.contains("glassbox extract"),
            "expected extract guidance for {flag}, got: {stderr}"
        );
        assert!(bare.stdout.is_empty(), "bare {flag} should not emit stdout");

        let equals = format!("{flag}=sample");
        let with_eq = run(&["inspect", &equals, "/nonexistent/media-file.mp4"]);
        assert_eq!(
            with_eq.status.code(),
            Some(64),
            "equals {flag}: {with_eq:?}"
        );
        let eq_stderr = String::from_utf8_lossy(&with_eq.stderr);
        assert!(
            eq_stderr.contains("glassbox extract"),
            "expected extract guidance for {equals}, got: {eq_stderr}"
        );
    }

    let dir = workdir("inspect-literal-imagegrid");
    let literal_file = dir.join("--imagegrid");
    std::fs::write(
        &literal_file,
        "literal content for inspect terminator test\n",
    )
    .unwrap();
    let passed = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args(["inspect", "--", "--imagegrid"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        matches!(passed.status.code(), Some(0 | 2)),
        "literal --imagegrid after -- should pass through inspect guard (not exit 64), got {:?}: stderr={}",
        passed.status.code(),
        String::from_utf8_lossy(&passed.stderr)
    );
    assert!(
        passed.stderr.is_empty(),
        "literal --imagegrid after -- should not emit guard error: {}",
        String::from_utf8_lossy(&passed.stderr)
    );
    let report = json(&passed);
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["kind"], "inspect");
    assert_eq!(report["file"]["name"], "--imagegrid");
}

/// Verifies invalid contact-sheet CLI flag combinations exit `64` before opening any input file.
///
/// Accepts no arguments. Returns unit after checking missing destinations,
/// orphan `-o`/media flags, `-o` with `--batch`, unsupported `-o` extensions,
/// `--imagegrid-format` vs `-o` suffix conflicts, and invalid `--every-seconds`
/// pairings against a nonexistent path. Panics if any invalid invocation fails
/// to exit `64` or omits stderr diagnostics.
#[test]
fn invalid_contact_sheet_flags_exit_64_before_opening_file() {
    let dir = workdir("invalid-flags");
    let out_dir = dir.join("out");
    let out_jpg = dir.join("grid.jpg");
    let out_webp = dir.join("grid.webp");
    let out_noext = dir.join("grid");
    let out_dir_s = out_dir.to_str().unwrap();
    let out_jpg_s = out_jpg.to_str().unwrap();
    let out_webp_s = out_webp.to_str().unwrap();
    let out_noext_s = out_noext.to_str().unwrap();
    let missing_input = "/nonexistent/does-not-exist.mp4";

    let cases: &[(&str, Vec<&str>)] = &[
        (
            "imagegrid without destination",
            vec!["extract", missing_input, "--imagegrid"],
        ),
        (
            "-o without imagegrid",
            vec!["extract", missing_input, "-o", out_jpg_s],
        ),
        (
            "-o with batch",
            vec!["extract", "--batch", "--imagegrid", "-o", out_jpg_s],
        ),
        (
            "bad -o extension (.webp)",
            vec!["extract", missing_input, "--imagegrid", "-o", out_webp_s],
        ),
        (
            "missing -o extension",
            vec!["extract", missing_input, "--imagegrid", "-o", out_noext_s],
        ),
        (
            "conflicting --imagegrid-format png and -o out.jpg",
            vec![
                "extract",
                missing_input,
                "--imagegrid",
                "--imagegrid-format",
                "png",
                "-o",
                out_jpg_s,
            ],
        ),
        (
            "--every-seconds with adaptive",
            vec![
                "extract",
                missing_input,
                "--imagegrid",
                "adaptive",
                "--every-seconds",
                "10",
                "-o",
                out_jpg_s,
            ],
        ),
        (
            "--every-seconds with explicit default",
            vec![
                "extract",
                missing_input,
                "--imagegrid",
                "default",
                "--every-seconds",
                "10",
                "-o",
                out_jpg_s,
            ],
        ),
        (
            "--every-seconds without --imagegrid",
            vec![
                "extract",
                missing_input,
                "--every-seconds",
                "10",
                "--output-dir",
                out_dir_s,
            ],
        ),
        (
            "--imagegrid custom without --every-seconds",
            vec![
                "extract",
                missing_input,
                "--imagegrid",
                "custom",
                "-o",
                out_jpg_s,
            ],
        ),
        (
            "--quality without --imagegrid",
            vec!["extract", missing_input, "--quality"],
        ),
        (
            "--performance without --imagegrid",
            vec!["extract", missing_input, "--performance"],
        ),
        (
            "--imagegrid-format without --imagegrid",
            vec!["extract", missing_input, "--imagegrid-format", "png"],
        ),
        (
            "--quality conflicting with --performance",
            vec![
                "extract",
                missing_input,
                "--imagegrid",
                "--quality",
                "--performance",
                "-o",
                out_jpg_s,
            ],
        ),
    ];

    for (label, argv) in cases {
        let output = run(argv);
        assert_eq!(
            output.status.code(),
            Some(64),
            "case '{label}' expected exit 64, got {:?}: stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stderr.is_empty(),
            "case '{label}' expected diagnostic on stderr"
        );
    }
    assert!(!out_jpg.exists());
    assert!(!out_dir.exists());
}

/// Verifies missing FFmpeg/ffprobe helpers exit `1` with actionable `DEPENDENCIES.md` diagnostics.
///
/// Accepts no arguments. Returns unit after invoking `--imagegrid` with
/// `GLASSBOX_FFMPEG=/nonexistent/ffmpeg` and `GLASSBOX_FFPROBE=/nonexistent/ffprobe`
/// and checking exit `1`, v3 `status == "failed"`, `stage == "media"`,
/// `code == "media_helper_missing"`, and a reference to `DEPENDENCIES.md`.
/// Panics if the diagnostic or exit status diverges.
#[test]
fn missing_ffmpeg_exits_1_with_dependencies_guidance() {
    let dir = workdir("missing-ffmpeg");
    let dummy_input = dir.join("clip.mp4");
    std::fs::write(&dummy_input, b"not-a-real-video").unwrap();
    let out_jpg = dir.join("grid.jpg");

    for (env_key, bad_bin) in [
        ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg"),
        ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe"),
    ] {
        let output = run_with_env(
            &[
                "extract",
                dummy_input.to_str().unwrap(),
                "--imagegrid",
                "-o",
                out_jpg.to_str().unwrap(),
            ],
            &[(env_key, bad_bin)],
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "{env_key}: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = json(&output);
        assert_eq!(report["schema_version"], 3);
        assert_eq!(report["kind"], "extract");
        assert_eq!(report["status"], "failed");
        let media_diag = report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|diag| diag["stage"] == "media")
            .expect("expected DiagnosticStage::Media diagnostic");
        assert_eq!(media_diag["severity"], "error");
        assert_eq!(media_diag["code"], "media_helper_missing");
        let message = media_diag["message"].as_str().unwrap();
        assert!(
            message.contains("DEPENDENCIES.md"),
            "expected DEPENDENCIES.md guidance for {env_key}, got: {message}"
        );
        assert!(!out_jpg.exists());
    }
}

/// Verifies end-to-end contact-sheet generation, publication safety, caps, audio-only rejection, and batch mode with synthetic FFmpeg fixtures.
///
/// Accepts no arguments. Skips with an explicit diagnostic only when `ffmpeg`
/// or `ffprobe` is absent on the host. Generates synthetic 4 s MP4 and 2 s WAV
/// fixtures and asserts:
/// 1. `--imagegrid default --quality -o <0755-dir>/grid.jpg` exits `0`, writes
///    `0600` valid JPEG (`1920x1130`, `25` tiles), emits v3 `ContactSheet`
///    artifact and receipt, and leaves zero temp files/dirs.
/// 2. Re-running the same `-o` exits `1` without overwriting existing bytes.
/// 3. `--imagegrid adaptive --performance --imagegrid-format png --output-dir <dir>`
///    exits `0`, writes `<stem>-contact-sheet.png` (`1280x150`, valid PNG) plus
///    `.md`/`.json` artifacts.
/// 4. `--imagegrid custom --every-seconds 0.01 -o <path>` (400 tiles > 250 cap)
///    exits `64`.
/// 5. `--imagegrid adaptive -o <path>` on audio-only WAV exits `1` with a
///    `DiagnosticStage::Media` error.
/// 6. `--batch --imagegrid adaptive --performance --output-dir <dir>` exits `0`
///    with ordered JSONL report and `0600` contact-sheet output.
#[test]
fn synthetic_ffmpeg_contact_sheet_end_to_end() {
    if !host_has_ffmpeg() {
        eprintln!(
            "SKIP synthetic_ffmpeg_contact_sheet_end_to_end: host does not have ffmpeg/ffprobe on PATH"
        );
        return;
    }

    let root = workdir("ffmpeg-e2e");
    let mp4 = generate_synthetic_mp4(&root, "color-ramp.mp4");
    let wav = generate_synthetic_wav(&root, "tone-only.wav");

    // 1. Default quality JPEG published via -o into an ordinary 0755 directory.
    let public_dir = root.join("public-0755");
    std::fs::create_dir_all(&public_dir).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        &public_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let grid_jpg = public_dir.join("grid.jpg");

    let run1 = run(&[
        "extract",
        mp4.to_str().unwrap(),
        "--imagegrid",
        "default",
        "--quality",
        "-o",
        grid_jpg.to_str().unwrap(),
    ]);
    assert_eq!(
        run1.status.code(),
        Some(0),
        "default quality -o failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run1.stdout),
        String::from_utf8_lossy(&run1.stderr)
    );
    assert!(grid_jpg.is_file(), "grid.jpg should be published");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&grid_jpg)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "published contact sheet must be mode 0600");
    }
    let jpeg_bytes = std::fs::read(&grid_jpg).unwrap();
    assert!(
        jpeg_bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "grid.jpg must start with valid JPEG SOI marker"
    );

    let report1 = json(&run1);
    assert_eq!(report1["schema_version"], 3);
    assert_eq!(report1["kind"], "extract");
    assert_eq!(report1["status"], "complete");
    assert_eq!(report1["completeness"], "exact");
    let artifacts1 = report1["artifacts"].as_array().unwrap();
    assert_eq!(artifacts1.len(), 1);
    assert_eq!(artifacts1[0]["kind"], "contact_sheet");
    assert_eq!(artifacts1[0]["width"], 1920);
    assert_eq!(artifacts1[0]["height"], 1130);
    assert_eq!(
        PathBuf::from(artifacts1[0]["path"].as_str().unwrap()),
        grid_jpg
    );
    let receipt1 = &report1["media"]["contact_sheet"];
    assert_eq!(receipt1["tiles"], 25);
    assert_eq!(receipt1["rows"], 5);
    assert_eq!(receipt1["columns"], 5);
    assert_eq!(receipt1["mode"], "default");
    assert_eq!(receipt1["preset"], "quality");
    assert_eq!(receipt1["format"], "jpg");
    assert_eq!(receipt1["width_px"], 1920);
    assert_eq!(receipt1["height_px"], 1130);
    assert_eq!(receipt1["cell_width_px"], 360);
    assert_eq!(receipt1["cell_height_px"], 202);
    assert_eq!(receipt1["seek_seconds"].as_array().unwrap().len(), 25);
    assert_no_temp_leftovers(&public_dir);
    assert_no_temp_leftovers(&root);

    // 2. Re-running with the same -o refuses overwrite and preserves original bytes.
    let rerun = run(&[
        "extract",
        mp4.to_str().unwrap(),
        "--imagegrid",
        "default",
        "--quality",
        "-o",
        grid_jpg.to_str().unwrap(),
    ]);
    assert_eq!(rerun.status.code(), Some(1), "re-run collision must exit 1");
    let rerun_report = json(&rerun);
    assert_eq!(rerun_report["status"], "failed");
    assert_eq!(rerun_report["completeness"], "unavailable");
    assert_eq!(rerun_report["selection"]["omitted"], "partial_failure");
    assert_eq!(rerun_report["selection"]["completeness"], "unavailable");
    assert!(rerun_report.get("media").is_none());
    assert!(
        rerun_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diag| diag["stage"] == "media" && diag["code"] == "contact_sheet_exists"),
        "expected contact_sheet_exists media diagnostic: {rerun_report}"
    );
    assert_eq!(
        std::fs::read(&grid_jpg).unwrap(),
        jpeg_bytes,
        "existing grid.jpg bytes must remain unchanged on collision"
    );
    assert_no_temp_leftovers(&public_dir);

    // 3. Adaptive performance PNG published into --output-dir (1 row -> 1280x150).
    let private_out = root.join("adaptive-out");
    let run_png = run(&[
        "extract",
        mp4.to_str().unwrap(),
        "--imagegrid",
        "adaptive",
        "--performance",
        "--imagegrid-format",
        "png",
        "--output-dir",
        private_out.to_str().unwrap(),
    ]);
    assert_eq!(
        run_png.status.code(),
        Some(0),
        "adaptive performance PNG failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run_png.stdout),
        String::from_utf8_lossy(&run_png.stderr)
    );
    let expected_png = private_out.join("color-ramp-contact-sheet.png");
    assert!(
        expected_png.is_file(),
        "expected {}",
        expected_png.display()
    );
    let png_bytes = std::fs::read(&expected_png).unwrap();
    assert!(
        png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "expected valid PNG header"
    );
    let report_png = json(&run_png);
    assert_eq!(report_png["status"], "complete");
    let png_artifact = report_png["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "contact_sheet")
        .expect("expected contact_sheet artifact");
    assert_eq!(png_artifact["width"], 1280);
    assert_eq!(png_artifact["height"], 150);
    let receipt_png = &report_png["media"]["contact_sheet"];
    assert_eq!(receipt_png["rows"], 1);
    assert_eq!(receipt_png["tiles"], 5);
    assert_eq!(receipt_png["width_px"], 1280);
    assert_eq!(receipt_png["height_px"], 150);
    assert_eq!(receipt_png["mode"], "adaptive");
    assert_eq!(receipt_png["preset"], "performance");
    assert_eq!(receipt_png["format"], "png");
    // Ensure persist_extract saved .json containing the contact_sheet receipt without double-publishing.
    let json_artifact = report_png["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "extraction_json")
        .expect("expected extraction_json artifact");
    let saved_json: Value =
        serde_json::from_slice(&std::fs::read(json_artifact["path"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(saved_json["media"]["contact_sheet"]["width_px"], 1280);
    assert_no_temp_leftovers(&private_out);

    // 4. Custom interval exceeding 250-tile cap (4s / 0.01s = 400 tiles) exits 64.
    let over_cap_out = root.join("over-cap.jpg");
    let over_cap = run(&[
        "extract",
        mp4.to_str().unwrap(),
        "--imagegrid",
        "custom",
        "--every-seconds",
        "0.01",
        "-o",
        over_cap_out.to_str().unwrap(),
    ]);
    assert_eq!(
        over_cap.status.code(),
        Some(64),
        "exceeding 250-tile cap must exit 64: stdout={} stderr={}",
        String::from_utf8_lossy(&over_cap.stdout),
        String::from_utf8_lossy(&over_cap.stderr)
    );
    assert!(
        String::from_utf8_lossy(&over_cap.stderr).contains("250"),
        "stderr should mention 250-tile cap"
    );
    assert!(!over_cap_out.exists());

    // 5. Audio-only WAV exits 1 with structured DiagnosticStage::Media diagnostic.
    let audio_grid_out = root.join("audio-grid.jpg");
    let audio_only = run(&[
        "extract",
        wav.to_str().unwrap(),
        "--imagegrid",
        "adaptive",
        "-o",
        audio_grid_out.to_str().unwrap(),
    ]);
    assert_eq!(
        audio_only.status.code(),
        Some(1),
        "audio-only input must exit 1: stderr={}",
        String::from_utf8_lossy(&audio_only.stderr)
    );
    let audio_report = json(&audio_only);
    assert_eq!(audio_report["status"], "failed");
    assert_eq!(audio_report["completeness"], "unavailable");
    assert_eq!(audio_report["selection"]["omitted"], "partial_failure");
    assert_eq!(audio_report["selection"]["completeness"], "unavailable");
    assert!(audio_report.get("media").is_none());
    assert!(
        audio_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diag| diag["stage"] == "media" && diag["severity"] == "error"),
        "expected media error diagnostic on audio-only input: {audio_report}"
    );
    assert!(!audio_grid_out.exists());

    // 6. Batch mode with --imagegrid --output-dir publishes cleanly.
    let batch_out = root.join("batch-out");
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args([
            "extract",
            "--batch",
            "--jobs",
            "2",
            "--imagegrid",
            "adaptive",
            "--performance",
            "--output-dir",
        ])
        .arg(&batch_out)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        serde_json::json!({"id": "vid-1", "path": mp4})
    )
    .unwrap();
    drop(child.stdin.take());
    let batch_res = child.wait_with_output().unwrap();
    assert_eq!(
        batch_res.status.code(),
        Some(0),
        "batch --imagegrid failed: stdout={} stderr={}",
        String::from_utf8_lossy(&batch_res.stdout),
        String::from_utf8_lossy(&batch_res.stderr)
    );
    let batch_line = json(&batch_res);
    assert_eq!(batch_line["id"], "vid-1");
    assert_eq!(batch_line["report"]["status"], "complete");
    assert_eq!(
        batch_line["report"]["media"]["contact_sheet"]["width_px"],
        1280
    );
    assert!(batch_out.join("color-ramp-contact-sheet.jpg").is_file());
    assert_no_temp_leftovers(&batch_out);
}

/// Verifies symlink, `concat:`, and `http://` inputs are rejected before FFmpeg/ffprobe with zero SSRF and zero artifacts.
///
/// Accepts no arguments. Constructs a local MP4 target, a Unix symlink to it,
/// a `concat:` pseudo-URL, and a live loopback TCP listener for an `http://`
/// probe URL, then verifies single-file and `--batch` `--imagegrid` invocations
/// fail closed with exit `1`, `status == "failed"`,
/// `selection.omitted == "partial_failure"`,
/// `selection.completeness == "unavailable"`, `DiagnosticStage::Input` error
/// diagnostics, zero written contact-sheet artifacts, and zero TCP connections.
#[test]
fn rejects_symlink_and_protocol_inputs_without_ssrf_or_artifacts() {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;

    let root = workdir("adversarial-input-guard");
    let real_mp4 = if host_has_ffmpeg() {
        generate_synthetic_mp4(&root, "real.mp4")
    } else {
        let placeholder = root.join("real.mp4");
        std::fs::write(&placeholder, b"placeholder-video").unwrap();
        placeholder
    };

    // 1. Symlink link.mp4 -> real.mp4 must exit 1 with failed selection and zero output artifact.
    #[cfg(unix)]
    let link_mp4 = {
        let link = root.join("link.mp4");
        std::os::unix::fs::symlink(&real_mp4, &link).unwrap();
        let out_jpg = root.join("out.jpg");
        let symlink_res = run(&[
            "extract",
            link.to_str().unwrap(),
            "--imagegrid",
            "-o",
            out_jpg.to_str().unwrap(),
        ]);
        assert_eq!(
            symlink_res.status.code(),
            Some(1),
            "symlink input must exit 1: stdout={} stderr={}",
            String::from_utf8_lossy(&symlink_res.stdout),
            String::from_utf8_lossy(&symlink_res.stderr)
        );
        let report = json(&symlink_res);
        assert_eq!(report["status"], "failed");
        assert_eq!(report["completeness"], "unavailable");
        assert_eq!(report["selection"]["omitted"], "partial_failure");
        assert_eq!(report["selection"]["completeness"], "unavailable");
        assert!(report.get("media").is_none());
        assert!(
            report["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diag| diag["stage"] == "input" && diag["severity"] == "error"),
            "expected input error diagnostic on symlink: {report}"
        );
        assert!(!out_jpg.exists(), "rejected symlink must not write out.jpg");
        link
    };

    // 2. concat:real.mp4|real.mp4 protocol string must exit 1 and write zero artifacts.
    let concat_input = format!("concat:{}|{}", real_mp4.display(), real_mp4.display());
    let ssrf_concat = root.join("ssrf_concat.jpg");
    let concat_res = run(&[
        "extract",
        &concat_input,
        "--imagegrid",
        "-o",
        ssrf_concat.to_str().unwrap(),
    ]);
    assert_eq!(
        concat_res.status.code(),
        Some(1),
        "concat: protocol input must exit 1: stdout={} stderr={}",
        String::from_utf8_lossy(&concat_res.stdout),
        String::from_utf8_lossy(&concat_res.stderr)
    );
    let concat_report = json(&concat_res);
    assert_eq!(concat_report["status"], "failed");
    assert_eq!(concat_report["completeness"], "unavailable");
    assert_eq!(concat_report["selection"]["omitted"], "partial_failure");
    assert_eq!(concat_report["selection"]["completeness"], "unavailable");
    assert!(concat_report.get("media").is_none());
    assert!(
        concat_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diag| diag["stage"] == "input" && diag["severity"] == "error"),
        "expected input error diagnostic on concat: {concat_report}"
    );
    assert!(
        !ssrf_concat.exists(),
        "rejected concat: input must not write ssrf_concat.jpg"
    );

    // 3. http://127.0.0.1:<port>/test.mp4 against a live listener must exit 1 with 0 TCP connections (zero SSRF).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind SSRF sentinel listener");
    let addr = listener.local_addr().expect("listener local addr");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let stop = Arc::new(AtomicBool::new(false));
    let connections = Arc::new(AtomicUsize::new(0));
    let stop_bg = Arc::clone(&stop);
    let connections_bg = Arc::clone(&connections);
    let accept_thread = std::thread::spawn(move || {
        while !stop_bg.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    connections_bg.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        if let Ok((stream, _)) = listener.accept() {
            connections_bg.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });

    let http_url = format!("http://{addr}/test.mp4");
    let ssrf_http = root.join("ssrf_http.jpg");
    let http_res = run(&[
        "extract",
        &http_url,
        "--imagegrid",
        "-o",
        ssrf_http.to_str().unwrap(),
    ]);
    stop.store(true, Ordering::SeqCst);
    accept_thread.join().expect("join SSRF sentinel thread");

    assert_eq!(
        http_res.status.code(),
        Some(1),
        "http:// protocol input must exit 1: stdout={} stderr={}",
        String::from_utf8_lossy(&http_res.stdout),
        String::from_utf8_lossy(&http_res.stderr)
    );
    let http_report = json(&http_res);
    assert_eq!(http_report["status"], "failed");
    assert_eq!(http_report["completeness"], "unavailable");
    assert_eq!(http_report["selection"]["omitted"], "partial_failure");
    assert_eq!(http_report["selection"]["completeness"], "unavailable");
    assert!(http_report.get("media").is_none());
    assert!(
        http_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diag| diag["stage"] == "input" && diag["severity"] == "error"),
        "expected input error diagnostic on http://: {http_report}"
    );
    assert!(
        !ssrf_http.exists(),
        "rejected http:// input must not write ssrf_http.jpg"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "http:// input must never trigger an outbound TCP connection (SSRF)"
    );

    // 4. Symlink rejection in --batch --imagegrid --output-dir mode writes zero contact sheets.
    #[cfg(unix)]
    {
        let batch_out = root.join("batch-symlink-out");
        let mut child = Command::new(env!("CARGO_BIN_EXE_glassbox"))
            .args([
                "extract",
                "--batch",
                "--imagegrid",
                "adaptive",
                "--performance",
                "--output-dir",
            ])
            .arg(&batch_out)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.as_mut().unwrap(),
            "{}",
            serde_json::json!({"id": "symlink-1", "path": link_mp4})
        )
        .unwrap();
        drop(child.stdin.take());
        let batch_res = child.wait_with_output().unwrap();
        assert_eq!(
            batch_res.status.code(),
            Some(1),
            "batch symlink input must exit 1: stdout={} stderr={}",
            String::from_utf8_lossy(&batch_res.stdout),
            String::from_utf8_lossy(&batch_res.stderr)
        );
        let batch_line = json(&batch_res);
        assert_eq!(batch_line["id"], "symlink-1");
        assert_eq!(batch_line["report"]["status"], "failed");
        assert_eq!(batch_line["report"]["completeness"], "unavailable");
        assert_eq!(
            batch_line["report"]["selection"]["omitted"],
            "partial_failure"
        );
        assert_eq!(
            batch_line["report"]["selection"]["completeness"],
            "unavailable"
        );
        assert!(batch_line["report"].get("media").is_none());
        assert!(
            batch_line["report"]["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diag| diag["stage"] == "input" && diag["severity"] == "error"),
            "expected input error diagnostic in batch report: {batch_line}"
        );
        assert!(
            !batch_out.join("link-contact-sheet.jpg").exists(),
            "batch symlink rejection must not write link-contact-sheet.jpg"
        );
        for entry in std::fs::read_dir(&batch_out).unwrap() {
            let path = entry.unwrap().path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();
            assert!(
                ext != "jpg" && ext != "png",
                "batch symlink rejection must write zero contact-sheet images, found {}",
                path.display()
            );
        }
        assert_no_temp_leftovers(&batch_out);
    }

    assert_no_temp_leftovers(&root);
}

/// Verifies dash-prefixed filenames after `--` succeed without FFmpeg/ffprobe option injection.
///
/// Accepts no arguments. Skips with an explicit diagnostic only when `ffmpeg`
/// or `ffprobe` is absent on the host. Generates `-clip.mp4` inside a test
/// scratch directory and runs both absolute (`-- <dir>/-clip.mp4`) and
/// relative (`-- -clip.mp4`) `--imagegrid adaptive` invocations, asserting
/// exit `0`, valid `0600` JPEG output, and complete schema v3 receipts.
#[test]
fn accepts_dash_prefixed_filename_after_double_dash_terminator() {
    if !host_has_ffmpeg() {
        eprintln!(
            "SKIP accepts_dash_prefixed_filename_after_double_dash_terminator: host does not have ffmpeg/ffprobe on PATH"
        );
        return;
    }

    let root = workdir("dash-filename");
    let dash_mp4 = generate_synthetic_mp4(&root, "-clip.mp4");
    let minus_jpg = root.join("minus.jpg");

    // 1. Absolute path ending in `-clip.mp4` passed after `--`.
    let abs_run = run(&[
        "extract",
        "--imagegrid",
        "adaptive",
        "-o",
        minus_jpg.to_str().unwrap(),
        "--",
        dash_mp4.to_str().unwrap(),
    ]);
    assert_eq!(
        abs_run.status.code(),
        Some(0),
        "dash-prefixed absolute path failed: stdout={} stderr={}",
        String::from_utf8_lossy(&abs_run.stdout),
        String::from_utf8_lossy(&abs_run.stderr)
    );
    assert!(minus_jpg.is_file(), "expected {}", minus_jpg.display());
    let jpeg_bytes = std::fs::read(&minus_jpg).unwrap();
    assert!(
        jpeg_bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "minus.jpg must be a valid JPEG"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&minus_jpg)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "minus.jpg must be mode 0600");
    }
    let abs_report = json(&abs_run);
    assert_eq!(abs_report["status"], "complete");
    assert_eq!(abs_report["completeness"], "exact");
    assert_eq!(abs_report["selection"]["omitted"], "none");
    assert_eq!(abs_report["selection"]["completeness"], "exact");
    assert_eq!(abs_report["media"]["contact_sheet"]["tiles"], 5);

    // 2. Bare relative `-clip.mp4` passed after `--` from its parent directory.
    let minus_rel_jpg = root.join("minus-rel.jpg");
    let rel_run = Command::new(env!("CARGO_BIN_EXE_glassbox"))
        .args([
            "extract",
            "--imagegrid",
            "adaptive",
            "-o",
            minus_rel_jpg.to_str().unwrap(),
            "--",
            "-clip.mp4",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(
        rel_run.status.code(),
        Some(0),
        "bare relative -clip.mp4 after -- failed: stdout={} stderr={}",
        String::from_utf8_lossy(&rel_run.stdout),
        String::from_utf8_lossy(&rel_run.stderr)
    );
    assert!(minus_rel_jpg.is_file());
    let rel_bytes = std::fs::read(&minus_rel_jpg).unwrap();
    assert!(
        rel_bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "minus-rel.jpg must be a valid JPEG"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(&minus_rel_jpg)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "minus-rel.jpg must be mode 0600");
    }
    let rel_report = json(&rel_run);
    assert_eq!(rel_report["status"], "complete");
    assert_eq!(rel_report["media"]["contact_sheet"]["tiles"], 5);

    assert_no_temp_leftovers(&root);
}

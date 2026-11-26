//! Module: Glassbox combined media triage and local Whisper ONNX CLI integration tests.
//! File: media_combined_and_whisper_cli.rs.
//! Responsibility: Verify fail-closed local Whisper ONNX validation (`V-MED-09`), combined `--imagegrid` + `--asr` (+ `--extract-audio`) single-file and `--batch` triage (`V-MED-10`), `models.json` local-ONNX routing with zero OpenAI egress fallback, empty `media` receipt omission, and the ignored offline Whisper ONNX JFK smoke test (`V-MED-ONNX`).

use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Monotonic counter keeping concurrent test working directories distinct.
static WORKDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates a unique test-owned working directory under `extensionalities/target/tests/media-combined-and-whisper-cli`.
///
/// Accepts a descriptive test label. Returns the newly created canonicalized
/// `0700` directory path inside the ignored target tree (never `/tmp`).
/// Panics if directory creation or Unix permission setup fails.
fn workdir(label: &str) -> PathBuf {
    let seq = WORKDIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/media-combined-and-whisper-cli")
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

/// Runs the compiled `glassbox` binary with scrubbed ASR/config/helper env vars plus explicit overrides.
///
/// Accepts CLI argument slices and `(key, value)` environment pairs to inject.
/// Scrubs ambient `GLASSBOX_*` and `ORT_DYLIB_PATH` variables (unless overridden)
/// so each test runs hermetically. Returns the captured process [`Output`].
/// Panics if spawning the binary fails.
fn run_clean_env(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_glassbox"));
    cmd.args(args).current_dir(env!("CARGO_MANIFEST_DIR"));
    for key in [
        "GLASSBOX_CONFIG_DIR",
        "GLASSBOX_ASR_BACKEND",
        "GLASSBOX_ASR_URL",
        "GLASSBOX_ASR_MODEL",
        "GLASSBOX_ASR_API_KEY_ENV",
        "GLASSBOX_ASR_PROTOCOL",
        "GLASSBOX_ASR_MODEL_DIR",
        "GLASSBOX_ASR_DEVICE",
        "GLASSBOX_ASR_TIMEOUT_SECS",
        "GLASSBOX_FFMPEG",
        "GLASSBOX_FFPROBE",
    ] {
        cmd.env_remove(key);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

/// Runs the compiled `glassbox` binary with scrubbed ASR env vars and supplied stdin bytes.
///
/// Accepts CLI argument slices, `(key, value)` environment overrides, and raw
/// stdin payload bytes (for `--batch` JSONL tests). Returns the captured
/// process [`Output`]. Panics if spawning or writing stdin fails.
fn run_clean_env_with_stdin(args: &[&str], envs: &[(&str, &str)], stdin_bytes: &[u8]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_glassbox"));
    cmd.args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in [
        "GLASSBOX_CONFIG_DIR",
        "GLASSBOX_ASR_BACKEND",
        "GLASSBOX_ASR_URL",
        "GLASSBOX_ASR_MODEL",
        "GLASSBOX_ASR_API_KEY_ENV",
        "GLASSBOX_ASR_PROTOCOL",
        "GLASSBOX_ASR_MODEL_DIR",
        "GLASSBOX_ASR_DEVICE",
        "GLASSBOX_ASR_TIMEOUT_SECS",
        "GLASSBOX_FFMPEG",
        "GLASSBOX_FFPROBE",
    ] {
        cmd.env_remove(key);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn glassbox batch process");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(stdin_bytes)
        .expect("write batch stdin");
    child.wait_with_output().expect("wait for glassbox batch")
}

/// Decodes process stdout as JSON, panicking with full stdout/stderr context on failure.
///
/// Accepts captured process output. Returns the parsed [`serde_json::Value`].
/// Panics if stdout is not valid JSON.
fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "expected JSON stdout ({error}): status={:?} stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Reports whether both `ffmpeg` and `ffprobe` are runnable on this host.
///
/// Accepts no arguments. Returns `true` when `ffmpeg -version` and
/// `ffprobe -version` both exit with status `0`, or `false` otherwise.
/// Never panics.
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

/// Generates a synthetic MP4 containing both video and audio plus an optional container title tag.
///
/// Accepts a destination directory, filename, duration in seconds, and optional
/// title metadata string. Returns the created MP4 path. Panics if `ffmpeg` fails.
fn generate_av_mp4(dir: &Path, filename: &str, duration_secs: u32, title: Option<&str>) -> PathBuf {
    let dest = dir.join(filename);
    let video_src = format!("testsrc2=size=160x120:rate=5:duration={duration_secs}");
    let audio_src = format!("sine=frequency=520:sample_rate=16000:duration={duration_secs}");
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-y",
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        &video_src,
        "-f",
        "lavfi",
        "-i",
        &audio_src,
        "-pix_fmt",
        "yuv420p",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-c:a",
        "aac",
        "-shortest",
    ]);
    if let Some(title) = title {
        cmd.args(["-metadata", &format!("title={title}")]);
    }
    cmd.arg(&dest);
    let output = cmd.output().expect("spawn ffmpeg for synthetic AV MP4");
    assert!(
        output.status.success(),
        "ffmpeg failed to create AV MP4: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dest
}

/// Generates a synthetic 16 kHz mono WAV fixture of `duration_secs` seconds via `ffmpeg`.
///
/// Accepts a destination directory, filename, and duration in seconds. Returns
/// the created WAV path. Panics if `ffmpeg` fails.
fn generate_synthetic_wav(dir: &Path, filename: &str, duration_secs: u32) -> PathBuf {
    let dest = dir.join(filename);
    let filter = format!("sine=frequency=440:sample_rate=16000:duration={duration_secs}");
    let output = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &filter,
            "-ac",
            "1",
            "-ar",
            "16000",
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

/// Asserts that `dir` contains no leftover Glassbox temporary files or scratch directories.
///
/// Accepts a directory path to inspect. Returns unit when clean; panics on I/O
/// failure or if any `.glassbox-tmp-`, `.models.json.tmp.`, or `.contact-sheet-`
/// entry remains.
fn assert_no_temp_leftovers(dir: &Path) {
    let entries: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    for name in &entries {
        assert!(
            !name.starts_with(".glassbox-tmp-")
                && !name.starts_with(".models.json.tmp.")
                && !name.starts_with(".contact-sheet-"),
            "unexpected temporary entry '{name}' left in {}: {entries:?}",
            dir.display()
        );
    }
}

/// Canned HTTP response served by the loopback mock server for one incoming request.
#[derive(Debug, Clone)]
struct MockResponse {
    /// HTTP status line (for example `"200 OK"`, `"500 Internal Server Error"`).
    status_line: &'static str,
    /// Value of the `Content-Type` response header.
    content_type: &'static str,
    /// UTF-8 response body text.
    body: String,
}

/// Handle to a background loopback HTTP mock server running on a dedicated Tokio thread.
struct MockServerHandle {
    /// Normalized loopback base URL (`http://127.0.0.1:<port>/v1`).
    base_url: String,
    /// Total HTTP requests served.
    hits: Arc<AtomicUsize>,
}

/// Spawns a local loopback HTTP/1.1 mock server serving a scripted response queue.
///
/// Accepts a vector of [`MockResponse`] items served sequentially to incoming
/// connections. Returns a [`MockServerHandle`] once the TCP listener is bound
/// and ready to accept connections. Panics on loopback bind failure.
fn spawn_mock_server(responses: Vec<MockResponse>) -> MockServerHandle {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_bg = Arc::clone(&hits);
    let _captured = Arc::new(Mutex::new(Vec::<String>::new()));

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build mock HTTP Tokio runtime");
        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind loopback TCP listener");
            let addr = listener.local_addr().expect("local addr");
            addr_tx.send(addr).expect("send bound addr");

            let mut queue = VecDeque::from(responses);
            while let Some(resp) = queue.pop_front() {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                hits_bg.fetch_add(1, Ordering::SeqCst);
                drain_http_request(&mut stream).await;

                let body_bytes = resp.body.as_bytes();
                let header = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp.status_line,
                    resp.content_type,
                    body_bytes.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(body_bytes).await;
                let _ = stream.shutdown().await;
            }
        });
    });

    let addr = addr_rx.recv().expect("receive mock server addr");
    MockServerHandle {
        base_url: format!("http://{addr}/v1"),
        hits,
    }
}

/// Drains one complete HTTP/1.1 request (headers plus fixed or chunked body) from a TCP stream.
///
/// Accepts a mutable `tokio::net::TcpStream`. Reads until headers and full
/// request body have been consumed so the client never sees an early connection
/// reset. Panics if socket reads fail unexpectedly.
async fn drain_http_request(stream: &mut tokio::net::TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.expect("read request headers");
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end - 4]);
    let mut headers = HashMap::new();
    for line in header_text.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let is_chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let mut rest = buf[header_end..].to_vec();

    if is_chunked {
        while !rest.windows(5).any(|w| w == b"0\r\n\r\n") {
            let n = stream.read(&mut chunk).await.expect("read chunked body");
            if n == 0 {
                break;
            }
            rest.extend_from_slice(&chunk[..n]);
        }
    } else {
        let content_length = headers
            .get("content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        while rest.len() < content_length {
            let n = stream.read(&mut chunk).await.expect("read fixed body");
            if n == 0 {
                break;
            }
            rest.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Parses a flat JSON-in-YAML `glassbox:v1` frontmatter block into a key-to-JSON-Value map and body slice.
///
/// Accepts rendered Markdown starting with `---\n`. Returns `(frontmatter_map, body)`
/// where every `key: <json>` line is parsed via `serde_json::from_str`. Panics
/// if fences are missing or any frontmatter line is malformed.
fn parse_frontmatter(markdown: &str) -> (BTreeMap<String, Value>, &str) {
    let rest = markdown
        .strip_prefix("---\n")
        .expect("markdown must start with YAML frontmatter opening fence");
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.find("\n---"))
        .expect("markdown must contain YAML frontmatter closing fence");
    let yaml_block = &rest[..end];
    let body_start = end + "\n---".len();
    let body = rest[body_start..].trim_start_matches('\n');

    let mut fields = BTreeMap::new();
    for line in yaml_block.lines() {
        let (key, raw_value) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("expected `key: value` frontmatter line, got: {line}"));
        let parsed: Value = serde_json::from_str(raw_value).unwrap_or_else(|err| {
            panic!("frontmatter key '{key}' value is not valid JSON ({err}): {raw_value}")
        });
        fields.insert(key.to_owned(), parsed);
    }
    (fields, body)
}

/// Normalizes transcript text to lowercase single-spaced ASCII words for fuzzy phrase checks.
///
/// Accepts a transcript string slice. Returns lowercase alphanumeric words
/// separated by single spaces. Cannot fail.
fn normalize_transcript_words(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Verifies missing or empty `--asr-model-dir` fails closed before FFmpeg spawns with exit `1`.
///
/// Accepts no arguments. Tests both a nonexistent model directory and an empty
/// model directory with `GLASSBOX_FFMPEG=/nonexistent/ffmpeg` and
/// `GLASSBOX_FFPROBE=/nonexistent/ffprobe` so any premature FFmpeg spawn would
/// surface a helper error instead of `asr_asset_missing`. Asserts exit `1`,
/// missing-asset diagnostics naming required model files, and omitted empty
/// `"media"` object in v3 JSON. Panics on mismatch.
#[test]
fn whisper_onnx_missing_or_empty_model_dir_fails_closed_before_ffmpeg() {
    let dir = workdir("whisper-missing-model-dir");
    let dummy_audio = dir.join("sample.wav");
    std::fs::write(&dummy_audio, b"not-even-probed-before-validate").unwrap();
    let empty_model_dir = dir.join("empty-models");
    std::fs::create_dir_all(&empty_model_dir).unwrap();
    let missing_model_dir = dir.join("does-not-exist-models");

    // 1. Empty model directory fails closed before FFmpeg/ffprobe spawns.
    let empty_out = run_clean_env(
        &[
            "extract",
            dummy_audio.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-backend",
            "whisper-onnx",
            "--asr-model-dir",
            empty_model_dir.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg"),
            ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe"),
            ("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap()),
        ],
    );
    assert_eq!(
        empty_out.status.code(),
        Some(1),
        "empty model dir must exit 1: stdout={} stderr={}",
        String::from_utf8_lossy(&empty_out.stdout),
        String::from_utf8_lossy(&empty_out.stderr)
    );
    let report = json(&empty_out);
    assert_eq!(report["status"], "failed");
    assert!(
        report.get("media").is_none(),
        "failed media run must omit empty media object: {report}"
    );
    let diagnostics = report["diagnostics"].as_array().expect("diagnostics array");
    let diag = diagnostics
        .iter()
        .find(|item| item["stage"] == "media" && item["code"] == "asr_asset_missing")
        .unwrap_or_else(|| panic!("expected asr_asset_missing diagnostic, got: {diagnostics:?}"));
    let msg = diag["message"].as_str().unwrap();
    assert!(msg.contains("whisper-onnx"), "got: {msg}");
    assert!(msg.contains("encoder_model_int8.onnx"), "got: {msg}");
    assert!(msg.contains("decoder_model_merged_int8.onnx"), "got: {msg}");
    assert!(msg.contains("generation_config.json"), "got: {msg}");
    assert!(msg.contains("vocab.json"), "got: {msg}");
    assert!(
        !msg.contains("GLASSBOX_FFMPEG"),
        "validate() must fail BEFORE FFmpeg resolution: {msg}"
    );

    // 2. Nonexistent model directory also fails closed before FFmpeg spawns.
    let missing_out = run_clean_env(
        &[
            "extract",
            dummy_audio.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-model-dir",
            missing_model_dir.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg"),
            ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe"),
            ("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap()),
        ],
    );
    assert_eq!(missing_out.status.code(), Some(1));
    let missing_report = json(&missing_out);
    assert_eq!(missing_report["status"], "failed");
    assert!(missing_report.get("media").is_none());
    let missing_msg = missing_report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["code"] == "asr_asset_missing")
        .and_then(|item| item["message"].as_str())
        .expect("missing model dir diagnostic");
    assert!(missing_msg.contains("does not exist"), "got: {missing_msg}");
}

/// Verifies invalid explicit `ORT_DYLIB_PATH` fails closed before FFmpeg spawns with exit `1`.
///
/// Accepts no arguments. Populates valid dummy Whisper filenames in a scratch
/// model directory, sets `ORT_DYLIB_PATH=/nonexistent/libonnxruntime.so` and
/// broken `GLASSBOX_FFMPEG`/`GLASSBOX_FFPROBE`, and checks both CLI
/// `--asr-model-dir` and `GLASSBOX_ASR_MODEL_DIR`. Asserts exit `1` and an
/// actionable `asr_asset_missing` diagnostic mentioning `ORT_DYLIB_PATH`.
/// Panics on mismatch.
#[test]
fn whisper_onnx_invalid_ort_dylib_path_fails_closed_before_ffmpeg() {
    let dir = workdir("whisper-invalid-ort-dylib");
    let dummy_audio = dir.join("clip.wav");
    std::fs::write(&dummy_audio, b"dummy").unwrap();
    let model_dir = dir.join("fake-whisper-tiny");
    std::fs::create_dir_all(&model_dir).unwrap();
    std::fs::write(model_dir.join("encoder_model_int8.onnx"), b"stub-enc").unwrap();
    std::fs::write(
        model_dir.join("decoder_model_merged_int8.onnx"),
        b"stub-dec",
    )
    .unwrap();
    std::fs::write(
        model_dir.join("generation_config.json"),
        r#"{"decoder_start_token_id":50258,"no_timestamps_token_id":50363,"eos_token_id":50257}"#,
    )
    .unwrap();
    std::fs::write(model_dir.join("vocab.json"), r#"{"hello":1}"#).unwrap();

    // Check via CLI --asr-model-dir AND via GLASSBOX_ASR_MODEL_DIR env var.
    let cli_out = run_clean_env(
        &[
            "extract",
            dummy_audio.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-backend",
            "whisper-onnx",
            "--asr-model-dir",
            model_dir.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[
            ("ORT_DYLIB_PATH", "/nonexistent/libonnxruntime.so"),
            ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg"),
            ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe"),
            ("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap()),
        ],
    );
    assert_eq!(
        cli_out.status.code(),
        Some(1),
        "invalid ORT_DYLIB_PATH must exit 1: stdout={} stderr={}",
        String::from_utf8_lossy(&cli_out.stdout),
        String::from_utf8_lossy(&cli_out.stderr)
    );
    let cli_report = json(&cli_out);
    assert_eq!(cli_report["status"], "failed");
    assert!(cli_report.get("media").is_none());
    let cli_msg = cli_report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["stage"] == "media" && item["code"] == "asr_asset_missing")
        .and_then(|item| item["message"].as_str())
        .expect("expected ORT_DYLIB_PATH diagnostic");
    assert!(cli_msg.contains("ORT_DYLIB_PATH"), "got: {cli_msg}");
    assert!(
        !cli_msg.contains("GLASSBOX_FFMPEG"),
        "ORT_DYLIB_PATH validation must happen before FFmpeg spawns: {cli_msg}"
    );

    let env_out = run_clean_env(
        &[
            "extract",
            dummy_audio.to_str().unwrap(),
            "--asr",
            "full",
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_ASR_MODEL_DIR", model_dir.to_str().unwrap()),
            ("ORT_DYLIB_PATH", "/nonexistent/libonnxruntime.so"),
            ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg"),
            ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe"),
            ("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap()),
        ],
    );
    assert_eq!(env_out.status.code(), Some(1));
    let env_report = json(&env_out);
    let env_msg = env_report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["code"] == "asr_asset_missing")
        .and_then(|item| item["message"].as_str())
        .expect("expected env ORT_DYLIB_PATH diagnostic");
    assert!(env_msg.contains("ORT_DYLIB_PATH"), "got: {env_msg}");
}

/// Verifies explicit `--asr-device cuda` (and `GLASSBOX_ASR_DEVICE=cuda`) fails closed without CPU fallback.
///
/// Accepts no arguments. Runs against a scratch directory with broken
/// `GLASSBOX_FFMPEG`/`GLASSBOX_FFPROBE` so failure is proven to happen inside
/// `WhisperOnnxAsrBackend::validate()`. Asserts exit `1` and an
/// `asr_config_failed` diagnostic refusing silent CPU fallback. Panics on mismatch.
#[test]
fn whisper_onnx_cuda_without_feature_fails_closed_without_cpu_fallback() {
    let dir = workdir("whisper-cuda-fail-closed");
    let dummy_audio = dir.join("clip.wav");
    std::fs::write(&dummy_audio, b"dummy").unwrap();
    let model_dir = dir.join("models");
    std::fs::create_dir_all(&model_dir).unwrap();

    let cli_out = run_clean_env(
        &[
            "extract",
            dummy_audio.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-model-dir",
            model_dir.to_str().unwrap(),
            "--asr-device",
            "cuda",
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg"),
            ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe"),
            ("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap()),
        ],
    );
    assert_eq!(
        cli_out.status.code(),
        Some(1),
        "CUDA without feature must exit 1: stdout={} stderr={}",
        String::from_utf8_lossy(&cli_out.stdout),
        String::from_utf8_lossy(&cli_out.stderr)
    );
    let report = json(&cli_out);
    assert_eq!(report["status"], "failed");
    let msg = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["stage"] == "media" && item["code"] == "asr_config_failed")
        .and_then(|item| item["message"].as_str())
        .expect("expected CUDA asr_config_failed diagnostic");
    assert!(msg.contains("whisper-onnx-cuda"), "got: {msg}");
    assert!(msg.contains("refusing silent CPU fallback"), "got: {msg}");

    let env_out = run_clean_env(
        &[
            "extract",
            dummy_audio.to_str().unwrap(),
            "--asr",
            "full",
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_ASR_MODEL_DIR", model_dir.to_str().unwrap()),
            ("GLASSBOX_ASR_DEVICE", "cuda"),
            ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg"),
            ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe"),
            ("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap()),
        ],
    );
    assert_eq!(env_out.status.code(), Some(1));
    let env_report = json(&env_out);
    let env_msg = env_report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["code"] == "asr_config_failed")
        .and_then(|item| item["message"].as_str())
        .expect("expected env CUDA diagnostic");
    assert!(env_msg.contains("whisper-onnx-cuda"), "got: {env_msg}");
}

/// Verifies `glassbox models bind asr --local-onnx` routes to `whisper-onnx` with zero OpenAI fallback.
///
/// Accepts no arguments. Registers an OpenAI endpoint pointing at a live
/// loopback mock server inside an isolated `GLASSBOX_CONFIG_DIR/models.json`,
/// binds `asr` to `--local-onnx --model-dir <empty_dir> --device cpu`, verifies
/// `models show asr --format json`, and runs `glassbox extract --asr full`.
/// Asserts exit `1`, diagnostic routing to `whisper-onnx` (`models.json`), and
/// `0` HTTP hits on the OpenAI mock server. Panics on mismatch.
#[test]
fn whisper_onnx_models_bind_routes_without_openai_fallback() {
    let dir = workdir("whisper-models-bind");
    let cfg_dir = dir.join("config");
    let empty_model_dir = dir.join("empty-onnx");
    std::fs::create_dir_all(&empty_model_dir).unwrap();
    let dummy_audio = dir.join("private.wav");
    std::fs::write(&dummy_audio, b"private-local-audio").unwrap();

    let mock = spawn_mock_server(vec![MockResponse {
        status_line: "200 OK",
        content_type: "application/json",
        body: r#"{"text":"MUST NEVER BE CALLED"}"#.to_owned(),
    }]);

    let cfg_env = [("GLASSBOX_CONFIG_DIR", cfg_dir.to_str().unwrap())];

    let add_ep = run_clean_env(
        &[
            "models",
            "endpoint",
            "add",
            "cloud",
            "--base-url",
            &mock.base_url,
        ],
        &cfg_env,
    );
    assert_eq!(
        add_ep.status.code(),
        Some(0),
        "endpoint add failed: {}",
        String::from_utf8_lossy(&add_ep.stderr)
    );

    let bind = run_clean_env(
        &[
            "models",
            "bind",
            "asr",
            "--local-onnx",
            "--model-dir",
            empty_model_dir.to_str().unwrap(),
            "--device",
            "cpu",
        ],
        &cfg_env,
    );
    assert_eq!(
        bind.status.code(),
        Some(0),
        "models bind --local-onnx failed: {}",
        String::from_utf8_lossy(&bind.stderr)
    );

    let show = run_clean_env(&["models", "show", "asr", "--format", "json"], &cfg_env);
    assert_eq!(show.status.code(), Some(0));
    let shown = json(&show);
    assert_eq!(shown["binding"]["kind"], "local_onnx");
    assert_eq!(shown["binding"]["device"], "cpu");
    assert_eq!(
        shown["binding"]["model_dir"],
        empty_model_dir.to_str().unwrap()
    );

    let out = run_clean_env(
        &[
            "extract",
            dummy_audio.to_str().unwrap(),
            "--asr",
            "full",
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir.to_str().unwrap()),
            ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg"),
            ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe"),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "missing local ONNX assets must fail closed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json(&out);
    assert_eq!(report["status"], "failed");
    let msg = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["stage"] == "media" && item["code"] == "asr_asset_missing")
        .and_then(|item| item["message"].as_str())
        .expect("expected whisper-onnx missing asset diagnostic");
    assert!(msg.contains("whisper-onnx"), "got: {msg}");
    assert!(msg.contains("models.json"), "got: {msg}");
    assert_eq!(
        mock.hits.load(Ordering::SeqCst),
        0,
        "local ONNX failure must NEVER fall back to an OpenAI endpoint in models.json"
    );
}

/// Verifies combined `--imagegrid` + `--asr full` + `--extract-audio wav` succeeds in one pass.
///
/// Accepts no arguments. Skips cleanly if `ffmpeg`/`ffprobe` are absent on the
/// host. Generates a synthetic AV MP4, creates a `0755` parent directory for
/// `-o sheet.png` plus a private `--output-dir`, runs against a loopback mock
/// OpenAI server with `--format markdown`, and asserts exit `0`, `0600`
/// permissions on `sheet.png` and `<stem>-extract.wav`, `glassbox:v1`
/// frontmatter linking both `extraction.contact_sheet` and
/// `extraction.audio_extract`, valid `bodymatter:v1` `<time>` markers, and v3
/// JSON carrying both artifacts and `media.{contact_sheet,asr,audio_extract}`.
/// Panics on mismatch.
#[test]
fn combined_imagegrid_asr_and_extract_audio_succeeds() {
    if !host_has_ffmpeg() {
        eprintln!("skipping combined_imagegrid_asr_and_extract_audio_succeeds: ffmpeg unavailable");
        return;
    }

    let dir = workdir("combined-success");
    let mp4 = generate_av_mp4(&dir, "lecture-demo.mp4", 4, Some("Combined Media Triage"));

    let sheet_parent = dir.join("public-parent");
    std::fs::create_dir_all(&sheet_parent).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        &sheet_parent,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let sheet_path = sheet_parent.join("sheet.png");
    let out_dir = dir.join("artifacts");

    let mock = spawn_mock_server(vec![MockResponse {
        status_line: "200 OK",
        content_type: "application/json",
        body: r#"{"text":"Hello from combined media triage."}"#.to_owned(),
    }]);

    let out = run_clean_env(
        &[
            "extract",
            mp4.to_str().unwrap(),
            "--imagegrid",
            "adaptive",
            "-o",
            sheet_path.to_str().unwrap(),
            "--asr",
            "full",
            "--extract-audio",
            "wav",
            "--output-dir",
            out_dir.to_str().unwrap(),
            "--format",
            "markdown",
            "--asr-url",
            &mock.base_url,
            "--asr-model",
            "whisper-1",
        ],
        &[("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap())],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "combined extraction must exit 0: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);

    // 1. Contact sheet published with 0600 permissions inside 0755 parent.
    assert!(sheet_path.is_file(), "sheet.png must exist");
    let png_bytes = std::fs::read(&sheet_path).unwrap();
    assert!(
        png_bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "sheet.png must have valid PNG magic"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let sheet_mode = std::fs::metadata(&sheet_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(sheet_mode, 0o600, "sheet.png must be 0600");
    }

    // 2. Extracted WAV published with 0600 permissions inside --output-dir.
    let wav_path = out_dir.join("lecture-demo-extract.wav");
    assert!(wav_path.is_file(), "lecture-demo-extract.wav must exist");
    let wav_bytes = std::fs::read(&wav_path).unwrap();
    assert!(wav_bytes.len() > 44, "WAV must contain PCM payload");
    assert_eq!(&wav_bytes[0..4], b"RIFF");
    assert_eq!(&wav_bytes[8..12], b"WAVE");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let wav_mode = std::fs::metadata(&wav_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(wav_mode, 0o600, "extracted WAV must be 0600");
    }

    // 3. Saved .md and .json exist inside --output-dir.
    let saved_files: Vec<PathBuf> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let md_path = saved_files
        .iter()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .expect("saved .md artifact expected")
        .clone();
    let json_path = saved_files
        .iter()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .expect("saved .json artifact expected")
        .clone();

    // 4. Verify stdout and saved .md have glassbox:v1 frontmatter linking both media artifacts + bodymatter:v1 <time>.
    let stdout_md = String::from_utf8(out.stdout).unwrap();
    let saved_md = std::fs::read_to_string(&md_path).unwrap();
    for (label, md) in [
        ("stdout", stdout_md.as_str()),
        ("saved .md", saved_md.as_str()),
    ] {
        let (fm, body) = parse_frontmatter(md);
        assert_eq!(fm["schema"], "glassbox:v1", "{label}");
        assert_eq!(fm["type"], "transcript", "{label}");
        assert_eq!(fm["title"], "Combined Media Triage", "{label}");
        let extraction = &fm["extraction"];
        assert_eq!(extraction["mode"], "full", "{label}");
        assert_eq!(extraction["backend"], "openai", "{label}");
        assert_eq!(extraction["model"], "whisper-1", "{label}");
        assert_eq!(extraction["completeness"], "exact", "{label}");
        assert_eq!(
            extraction["contact_sheet"],
            sheet_path.to_str().unwrap(),
            "{label} must link contact_sheet"
        );
        assert_eq!(
            extraction["audio_extract"],
            wav_path.to_str().unwrap(),
            "{label} must link audio_extract"
        );
        assert!(
            body.contains("<!-- bodymatter:v1 <time start=\"00:00:00.000\" end=\""),
            "{label} missing bodymatter:v1 time marker: {body}"
        );
        assert!(
            body.contains("Hello from combined media triage."),
            "{label} missing transcript text: {body}"
        );
    }

    // 5. Verify saved v3 JSON has both artifacts and all three media sub-receipts.
    let v3: Value = serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert_eq!(v3["schema_version"], 3);
    assert_eq!(v3["status"], "complete");
    assert_eq!(v3["completeness"], "exact");
    let artifact_kinds: Vec<&str> = v3["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["kind"].as_str())
        .collect();
    assert!(
        artifact_kinds.contains(&"contact_sheet"),
        "artifacts missing contact_sheet: {artifact_kinds:?}"
    );
    assert!(
        artifact_kinds.contains(&"audio_extract"),
        "artifacts missing audio_extract: {artifact_kinds:?}"
    );
    assert!(
        artifact_kinds.contains(&"markdown_content"),
        "artifacts missing markdown_content: {artifact_kinds:?}"
    );
    assert!(
        artifact_kinds.contains(&"extraction_json"),
        "artifacts missing extraction_json: {artifact_kinds:?}"
    );
    assert_eq!(
        v3["media"]["contact_sheet"]["output"],
        sheet_path.to_str().unwrap()
    );
    assert_eq!(v3["media"]["asr"]["backend"], "openai");
    assert_eq!(v3["media"]["asr"]["model"], "whisper-1");
    assert_eq!(
        v3["media"]["audio_extract"]["path"],
        wav_path.to_str().unwrap()
    );
    assert_eq!(v3["media"]["audio_extract"]["format"], "wav");

    assert_no_temp_leftovers(&sheet_parent);
    assert_no_temp_leftovers(&out_dir);
}

/// Verifies combined triage exits `2` (`partial`) when `--imagegrid` succeeds and `--asr` fails with HTTP 500.
///
/// Accepts no arguments. Skips cleanly if `ffmpeg`/`ffprobe` are absent.
/// Generates a synthetic AV MP4, serves HTTP 500 responses from the loopback
/// mock server (including retry), and runs `--imagegrid adaptive -o sheet.png --asr full`.
/// Asserts exit `2`, `status == "partial"`, `sheet.png` published on disk and
/// listed in `artifacts` + `media.contact_sheet`, `media.asr` omitted, and
/// `DiagnosticStage::Media` recording the ASR HTTP 500 failure. Panics on mismatch.
#[test]
fn combined_imagegrid_succeeds_and_asr_fails_exits_2_partial() {
    if !host_has_ffmpeg() {
        eprintln!(
            "skipping combined_imagegrid_succeeds_and_asr_fails_exits_2_partial: ffmpeg unavailable"
        );
        return;
    }

    let dir = workdir("combined-grid-ok-asr-fail");
    let mp4 = generate_av_mp4(&dir, "clip.mp4", 4, None);
    let sheet_path = dir.join("sheet.png");

    // Two 500 responses cover initial attempt + 1 allowed transient retry.
    let mock = spawn_mock_server(vec![
        MockResponse {
            status_line: "500 Internal Server Error",
            content_type: "application/json",
            body: r#"{"error":{"message":"upstream boom 1"}}"#.to_owned(),
        },
        MockResponse {
            status_line: "500 Internal Server Error",
            content_type: "application/json",
            body: r#"{"error":{"message":"upstream boom 2"}}"#.to_owned(),
        },
    ]);

    let out = run_clean_env(
        &[
            "extract",
            mp4.to_str().unwrap(),
            "--imagegrid",
            "adaptive",
            "-o",
            sheet_path.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-url",
            &mock.base_url,
            "--asr-model",
            "whisper-1",
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap())],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "grid ok + ASR 500 must exit 2 (partial): stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        mock.hits.load(Ordering::SeqCst),
        2,
        "expected 1 retry on 500"
    );
    assert!(
        sheet_path.is_file(),
        "published sheet.png must be preserved"
    );

    let report = json(&out);
    assert_eq!(report["status"], "partial");
    assert_eq!(report["completeness"], "partial");
    assert_eq!(report["selection"]["omitted"], "partial_failure");
    assert!(
        report["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "contact_sheet"
                && item["path"] == sheet_path.to_str().unwrap()),
        "contact_sheet artifact must be preserved: {}",
        report["artifacts"]
    );
    assert_eq!(
        report["media"]["contact_sheet"]["output"],
        sheet_path.to_str().unwrap()
    );
    assert!(
        report["media"].get("asr").is_none(),
        "failed ASR must not emit media.asr receipt: {}",
        report["media"]
    );
    let has_asr_diag = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diag| diag["stage"] == "media" && diag["code"] == "asr_http_failed");
    assert!(
        has_asr_diag,
        "expected media/asr_http_failed diagnostic: {}",
        report["diagnostics"]
    );
    assert_no_temp_leftovers(&dir);
}

/// Verifies combined triage on an audio-only WAV exits `2` (`partial`) with transcript units preserved.
///
/// Accepts no arguments. Skips cleanly if `ffmpeg`/`ffprobe` are absent.
/// Generates a synthetic audio-only WAV and runs `--imagegrid adaptive -o sheet.png --asr full`
/// against a loopback mock OpenAI server. Asserts exit `2`, `status == "partial"`,
/// no `sheet.png` written, transcript unit preserved in `content.units` and
/// `media.asr`, and `DiagnosticStage::Media` recording the missing video stream.
/// Panics on mismatch.
#[test]
fn combined_audio_only_wav_grid_fails_and_asr_succeeds_exits_2_partial() {
    if !host_has_ffmpeg() {
        eprintln!(
            "skipping combined_audio_only_wav_grid_fails_and_asr_succeeds_exits_2_partial: ffmpeg unavailable"
        );
        return;
    }

    let dir = workdir("combined-wav-grid-fail-asr-ok");
    let wav = generate_synthetic_wav(&dir, "voice-note.wav", 3);
    let sheet_path = dir.join("sheet.png");

    let mock = spawn_mock_server(vec![MockResponse {
        status_line: "200 OK",
        content_type: "application/json",
        body: r#"{"text":"Audio only transcript preserved."}"#.to_owned(),
    }]);

    let out = run_clean_env(
        &[
            "extract",
            wav.to_str().unwrap(),
            "--imagegrid",
            "adaptive",
            "-o",
            sheet_path.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-url",
            &mock.base_url,
            "--asr-model",
            "whisper-1",
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap())],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "audio-only grid fail + ASR ok must exit 2 (partial): stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !sheet_path.exists(),
        "sheet.png must not be created for WAV"
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);

    let report = json(&out);
    assert_eq!(report["status"], "partial");
    assert_eq!(report["completeness"], "partial");
    assert_eq!(report["selection"]["omitted"], "partial_failure");
    let units = report["content"]["units"]
        .as_array()
        .expect("content units");
    assert_eq!(units.len(), 1);
    assert_eq!(units[0]["unit"], "time_interval");
    assert_eq!(units[0]["text"], "Audio only transcript preserved.");
    assert_eq!(report["media"]["asr"]["backend"], "openai");
    assert!(
        report["media"].get("contact_sheet").is_none(),
        "failed contact sheet must not emit media.contact_sheet: {}",
        report["media"]
    );
    let has_probe_diag = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diag| diag["stage"] == "media" && diag["code"] == "media_probe_failed");
    assert!(
        has_probe_diag,
        "expected media/media_probe_failed diagnostic for missing video stream: {}",
        report["diagnostics"]
    );
    assert_no_temp_leftovers(&dir);
}

/// Verifies combined total failure exits `1` and omits empty `"media"` from schema v3 JSON.
///
/// Accepts no arguments. Skips cleanly if `ffmpeg`/`ffprobe` are absent.
/// Runs `--imagegrid adaptive -o sheet.png --asr full --asr-backend whisper-onnx --asr-model-dir <empty>`
/// against an audio-only WAV so both contact-sheet generation and Whisper ONNX
/// validation fail. Asserts exit `1`, `status == "failed"`, `"media"` absent
/// from v3 JSON, and both media error diagnostics recorded. Panics on mismatch.
#[test]
fn combined_total_failure_exits_1_and_omits_empty_media_object() {
    if !host_has_ffmpeg() {
        eprintln!(
            "skipping combined_total_failure_exits_1_and_omits_empty_media_object: ffmpeg unavailable"
        );
        return;
    }

    let dir = workdir("combined-total-failure");
    let wav = generate_synthetic_wav(&dir, "audio-only.wav", 2);
    let empty_models = dir.join("empty-models");
    std::fs::create_dir_all(&empty_models).unwrap();
    let sheet_path = dir.join("sheet.png");

    let out = run_clean_env(
        &[
            "extract",
            wav.to_str().unwrap(),
            "--imagegrid",
            "adaptive",
            "-o",
            sheet_path.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-backend",
            "whisper-onnx",
            "--asr-model-dir",
            empty_models.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(1));
    let report = json(&out);
    assert_eq!(report["status"], "failed");
    assert_eq!(report["completeness"], "unavailable");
    assert!(
        report.get("media").is_none(),
        "total failure must omit media field completely: {report}"
    );
    let codes: Vec<&str> = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"media_probe_failed"),
        "missing contact sheet error in diagnostics: {codes:?}"
    );
    assert!(
        codes.contains(&"asr_asset_missing"),
        "missing whisper-onnx error in diagnostics: {codes:?}"
    );
}

/// Verifies `--batch` combined media triage supports `--imagegrid` + `--asr full` + `--extract-audio wav`.
///
/// Accepts no arguments. Skips cleanly if `ffmpeg`/`ffprobe` are absent.
/// Feeds one JSONL request for a synthetic AV MP4 to `glassbox extract --batch`,
/// verifies exit `0`, ordered JSONL output containing `media.{contact_sheet,asr,audio_extract}`,
/// and all published files inside `--output-dir`. Panics on mismatch.
#[test]
fn combined_batch_mode_runs_imagegrid_asr_and_extract_audio() {
    if !host_has_ffmpeg() {
        eprintln!(
            "skipping combined_batch_mode_runs_imagegrid_asr_and_extract_audio: ffmpeg unavailable"
        );
        return;
    }

    let dir = workdir("combined-batch");
    let mp4 = generate_av_mp4(&dir, "batch-clip.mp4", 4, Some("Batch Triage"));
    let out_dir = dir.join("batch-out");

    let mock = spawn_mock_server(vec![MockResponse {
        status_line: "200 OK",
        content_type: "application/json",
        body: r#"{"text":"Batch media transcription ok."}"#.to_owned(),
    }]);

    let req_line = format!(
        "{}\n",
        serde_json::json!({
            "id": "media-1",
            "path": mp4.to_str().unwrap(),
        })
    );
    let out = run_clean_env_with_stdin(
        &[
            "extract",
            "--batch",
            "--imagegrid",
            "adaptive",
            "--asr",
            "full",
            "--extract-audio",
            "wav",
            "--output-dir",
            out_dir.to_str().unwrap(),
            "--asr-url",
            &mock.base_url,
            "--asr-model",
            "whisper-1",
        ],
        &[("GLASSBOX_CONFIG_DIR", dir.join("cfg").to_str().unwrap())],
        req_line.as_bytes(),
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "batch combined extract must exit 0: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout_str = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout_str
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "expected 1 JSONL line: {stdout_str}");
    let envelope: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(envelope["id"], "media-1");
    let report = &envelope["report"];
    assert_eq!(report["status"], "complete");
    assert!(report["media"]["contact_sheet"].is_object());
    assert_eq!(report["media"]["asr"]["backend"], "openai");
    assert_eq!(report["media"]["audio_extract"]["format"], "wav");
    assert!(out_dir.join("batch-clip-contact-sheet.jpg").is_file());
    assert!(out_dir.join("batch-clip-extract.wav").is_file());
    assert_no_temp_leftovers(&out_dir);
}

/// Live offline Whisper Tiny INT8 ONNX verification gate (`V-MED-ONNX`) against the JFK spike fixture.
///
/// Accepts no arguments. Marked `#[ignore]` so CI without the 41 MB local
/// model bundle stays fast; run explicitly on hosts carrying
/// `extensionalities/sandbox/spikes/active/whisper-onnx/{models,fixtures/jfk.wav}`
/// and a discoverable ONNX Runtime library (`ORT_DYLIB_PATH` or LiteParse cache).
/// Clears OpenAI and proxy env vars, runs `glassbox extract <jfk.wav> --asr full
/// --asr-backend whisper-onnx --asr-model-dir <models> --output-dir <out> --format markdown`,
/// and asserts exit `0`, valid `glassbox:v1` frontmatter (`backend == "whisper-onnx"`),
/// v3 JSON receipt (`backend == "whisper-onnx"`, `provider == "cpu"`, no `protocol`),
/// valid `bodymatter:v1` `<time start="00:00:00.000" .../>` marker, and normalized
/// spoken phrases `"fellow americans"` and `"ask not"`. Panics on mismatch.
#[ignore]
#[test]
fn live_whisper_onnx_jfk() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let spike_root = manifest_dir.join("../../sandbox/spikes/active/whisper-onnx");
    let models_dir = spike_root.join("models");
    let jfk_wav = spike_root.join("fixtures/jfk.wav");
    assert!(
        models_dir.is_dir(),
        "V-MED-ONNX requires Whisper ONNX models at {}",
        models_dir.display()
    );
    assert!(
        jfk_wav.is_file(),
        "V-MED-ONNX requires JFK WAV fixture at {}",
        jfk_wav.display()
    );

    let dir = workdir("live-whisper-onnx-jfk");
    let out_dir = dir.join("out");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_glassbox"));
    cmd.args([
        "extract",
        jfk_wav.to_str().unwrap(),
        "--asr",
        "full",
        "--asr-backend",
        "whisper-onnx",
        "--asr-model-dir",
        models_dir.to_str().unwrap(),
        "--output-dir",
        out_dir.to_str().unwrap(),
        "--format",
        "markdown",
    ])
    .current_dir(&manifest_dir);
    for key in [
        "GLASSBOX_CONFIG_DIR",
        "GLASSBOX_ASR_BACKEND",
        "GLASSBOX_ASR_URL",
        "GLASSBOX_ASR_MODEL",
        "GLASSBOX_ASR_API_KEY_ENV",
        "GLASSBOX_ASR_PROTOCOL",
        "GLASSBOX_ASR_MODEL_DIR",
        "GLASSBOX_ASR_DEVICE",
        "GLASSBOX_ASR_TIMEOUT_SECS",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("GLASSBOX_CONFIG_DIR", dir.join("cfg"));

    let out = cmd.output().expect("spawn glassbox live Whisper ONNX");
    assert_eq!(
        out.status.code(),
        Some(0),
        "live Whisper ONNX JFK transcription must exit 0: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout_md = String::from_utf8(out.stdout).expect("UTF-8 markdown stdout");
    let (fm, body) = parse_frontmatter(&stdout_md);
    assert_eq!(fm["schema"], "glassbox:v1");
    assert_eq!(fm["type"], "transcript");
    assert_eq!(fm["extraction"]["mode"], "full");
    assert_eq!(fm["extraction"]["backend"], "whisper-onnx");
    assert_eq!(fm["extraction"]["completeness"], "exact");
    assert!(
        body.contains("<!-- bodymatter:v1 <time start=\"00:00:00.000\" end=\""),
        "expected bodymatter:v1 <time> marker starting at 00:00:00.000, got: {body}"
    );

    let normalized = normalize_transcript_words(body);
    assert!(
        normalized.contains("fellow americans"),
        "expected normalized JFK transcript to contain 'fellow americans', got: {normalized}"
    );
    assert!(
        normalized.contains("ask not"),
        "expected normalized JFK transcript to contain 'ask not', got: {normalized}"
    );

    let json_path = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .expect("saved v3 JSON receipt expected");
    let v3: Value = serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert_eq!(v3["status"], "complete");
    assert_eq!(v3["media"]["asr"]["backend"], "whisper-onnx");
    assert_eq!(v3["media"]["asr"]["provider"], "cpu");
    assert!(
        v3["media"]["asr"].get("protocol").is_none(),
        "local whisper-onnx receipt must omit protocol: {}",
        v3["media"]["asr"]
    );
    assert!(
        !v3["media"]["asr"]["sampled_ranges_ms"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_no_temp_leftovers(&out_dir);
}

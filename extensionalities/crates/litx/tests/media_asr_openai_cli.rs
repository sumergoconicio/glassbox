//! Module: Glassbox media ASR (OpenAI-compatible), `--extract-audio`, and `glassbox models` CLI integration tests.
//! File: media_asr_openai_cli.rs.
//! Responsibility: Verify `glassbox models` CRUD and secret hygiene, `--asr`/`--extract-audio` exit-64 validation rules, live FFmpeg piped OpenAI ASR (`transcriptions` and `chat`), retry/partial/failure exit contracts, `glassbox:v1` + `bodymatter:v1` Markdown rendering, and standalone/teed `--extract-audio` safe publication.

use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Monotonic counter keeping concurrent test working directories distinct.
static WORKDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates a unique test-owned working directory under the ignored target tree.
///
/// Accepts a descriptive test label. Returns the newly created private `0700`
/// directory path under `extensionalities/target/tests/media-asr-openai-cli`.
/// Panics if directory creation or Unix permission setup fails.
fn workdir(label: &str) -> PathBuf {
    let seq = WORKDIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests/media-asr-openai-cli")
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

/// Runs the compiled `glassbox` binary with scrubbed ASR/config env vars plus explicit overrides.
///
/// Accepts CLI arguments and `(key, value)` environment pairs to inject.
/// Removes any ambient `GLASSBOX_ASR_*` or `GLASSBOX_CONFIG_DIR` variables first
/// so every test runs hermetically. Returns the captured process [`Output`].
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
        "GLASSBOX_FFMPEG_IO_TIMEOUT_SECS",
    ] {
        cmd.env_remove(key);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

/// Decodes process stdout as JSON, panicking with full stdout/stderr on failure.
///
/// Accepts captured process output. Returns the parsed [`serde_json::Value`].
/// Panics with context if stdout is not valid JSON.
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

/// Reports whether `ffmpeg` and `ffprobe` are runnable on this host.
///
/// Accepts no arguments. Returns `true` when both `ffmpeg -version` and
/// `ffprobe -version` exit with status `0`, or `false` otherwise. Never panics.
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

/// Generates a synthetic WAV with 2 seconds of digital silence followed by 4 seconds of speech-band modulated audio.
///
/// Accepts a destination directory and filename. Returns the created WAV path
/// whose first 2 seconds are silent and whose remaining 4 seconds carry an
/// amplitude-modulated 500 Hz + 900 Hz signal squarely inside the VAD speech
/// energy and zero-crossing-rate band. Panics if `ffmpeg` fails.
fn generate_silence_then_speech_wav(dir: &Path, filename: &str) -> PathBuf {
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
            "anullsrc=r=16000:cl=mono:d=2",
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.35*sin(2*PI*450*t)+0.25*sin(2*PI*950*t):s=16000:c=mono:d=4",
            "-filter_complex",
            "[0:a][1:a]concat=n=2:v=0:a=1[out]",
            "-map",
            "[out]",
            "-c:a",
            "pcm_s16le",
        ])
        .arg(&dest)
        .output()
        .expect("spawn ffmpeg for silence+speech WAV");
    assert!(
        output.status.success(),
        "ffmpeg failed to create silence+speech WAV: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dest
}

/// Generates a synthetic MP4 containing both motion video and audio plus an optional container title tag.
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
    let output = cmd.output().expect("spawn ffmpeg for AV MP4");
    assert!(
        output.status.success(),
        "ffmpeg failed to create AV MP4: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dest
}

/// Generates a mute video-only MP4 fixture (`-an`) via `ffmpeg`.
///
/// Accepts a destination directory and filename. Returns the created MP4 path.
/// Panics if `ffmpeg` fails.
fn generate_mute_mp4(dir: &Path, filename: &str) -> PathBuf {
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
            "testsrc2=size=160x120:rate=5:duration=3",
            "-an",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&dest)
        .output()
        .expect("spawn ffmpeg for mute MP4");
    assert!(
        output.status.success(),
        "ffmpeg failed to create mute MP4: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    dest
}

/// Asserts that `dir` contains no leftover Glassbox temporary files or audio scratch files.
///
/// Accepts a directory path to inspect. Panics if reading the directory fails
/// or any `.glassbox-tmp-`, `.models.json.tmp.`, or `.contact-sheet-` entry remains.
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

/// Recorded HTTP request captured by the loopback mock OpenAI ASR server.
#[derive(Debug, Clone)]
struct CapturedRequest {
    /// Request line (for example `POST /v1/audio/transcriptions HTTP/1.1`).
    request_line: String,
    /// Lowercased HTTP request headers.
    headers: HashMap<String, String>,
    /// Raw request payload bytes.
    body: Vec<u8>,
}

/// Canned HTTP response served by the loopback mock server for one incoming request.
#[derive(Debug, Clone)]
struct MockResponse {
    /// HTTP status line (for example `"200 OK"`, `"503 Service Unavailable"`).
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
    /// Captured incoming HTTP requests in arrival order.
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
}

/// Spawns a local loopback HTTP/1.1 mock server serving a scripted response queue.
///
/// Accepts a vector of [`MockResponse`] items served sequentially to incoming
/// connections. Returns a [`MockServerHandle`] once the TCP listener is bound
/// and ready to accept connections. Panics on loopback bind failure.
fn spawn_mock_server(responses: Vec<MockResponse>) -> MockServerHandle {
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let hits = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let hits_bg = Arc::clone(&hits);
    let captured_bg = Arc::clone(&captured);

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
                let req = read_http_request(&mut stream).await;
                captured_bg.lock().expect("lock captured").push(req);

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
        captured,
    }
}

/// Reads one complete HTTP/1.1 request (headers plus body) from a TCP stream.
///
/// Accepts a mutable `tokio::net::TcpStream`. Supports both `Content-Length`
/// and `Transfer-Encoding: chunked` request bodies. Returns a [`CapturedRequest`].
/// Panics if the socket closes before headers finish.
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> CapturedRequest {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.expect("read request headers");
        assert!(n > 0, "unexpected EOF before HTTP header delimiter");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end - 4]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let is_chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let mut raw_rest = buf[header_end..].to_vec();

    let body = if is_chunked {
        loop {
            if let Some(decoded) = try_decode_chunked(&raw_rest) {
                break decoded;
            }
            let n = stream.read(&mut chunk).await.expect("read chunked body");
            if n == 0 {
                break raw_rest;
            }
            raw_rest.extend_from_slice(&chunk[..n]);
        }
    } else {
        let content_length = headers
            .get("content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        while raw_rest.len() < content_length {
            let n = stream.read(&mut chunk).await.expect("read fixed body");
            if n == 0 {
                break;
            }
            raw_rest.extend_from_slice(&chunk[..n]);
        }
        raw_rest.truncate(content_length);
        raw_rest
    };

    CapturedRequest {
        request_line,
        headers,
        body,
    }
}

/// Decodes an HTTP/1.1 chunked transfer-encoded buffer once the terminating `0\r\n\r\n` chunk arrives.
///
/// Accepts the buffered raw bytes after the HTTP header delimiter. Returns
/// `Some(decoded_bytes)` once a complete chunked stream is present, or `None`
/// if more bytes must be read from the socket. Cannot fail.
fn try_decode_chunked(buf: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    let mut out = Vec::new();
    loop {
        let rel_crlf = buf[pos..].windows(2).position(|w| w == b"\r\n")?;
        let size_line = std::str::from_utf8(&buf[pos..pos + rel_crlf]).ok()?;
        let size_hex = size_line.split(';').next()?.trim();
        let chunk_len = usize::from_str_radix(size_hex, 16).ok()?;
        pos += rel_crlf + 2;
        if chunk_len == 0 {
            if buf.len() >= pos + 2 && &buf[pos..pos + 2] == b"\r\n" {
                return Some(out);
            }
            return None;
        }
        if buf.len() < pos + chunk_len + 2 {
            return None;
        }
        out.extend_from_slice(&buf[pos..pos + chunk_len]);
        pos += chunk_len + 2;
    }
}

/// Parses a flat JSON-in-YAML `glassbox:v1` frontmatter block into a key-to-JSON-Value map.
///
/// Accepts the full rendered Markdown string. Returns `(frontmatter_map, body_str)`
/// after asserting the string starts with `---\n` and contains a closing `\n---`.
/// Panics if frontmatter fences are missing or any line is not valid `key: <json>`.
fn parse_frontmatter(markdown: &str) -> (HashMap<String, Value>, &str) {
    let rest = markdown
        .strip_prefix("---\n")
        .unwrap_or_else(|| panic!("expected leading YAML fence, got:\n{markdown}"));
    let end = rest
        .find("\n---")
        .unwrap_or_else(|| panic!("expected closing YAML fence, got:\n{markdown}"));
    let fm_text = &rest[..end];
    let body = &rest[end + 4..];

    let mut map = HashMap::new();
    for line in fm_text.lines() {
        let (key, raw_val) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("malformed frontmatter line: {line}"));
        let parsed: Value = serde_json::from_str(raw_val.trim())
            .unwrap_or_else(|err| panic!("non-JSON frontmatter value on '{line}': {err}"));
        map.insert(key.trim().to_owned(), parsed);
    }
    (map, body)
}

/// Checks whether a string is a valid 26-character uppercase Crockford Base32 ULID.
///
/// Accepts a candidate string slice. Returns `true` iff it matches
/// `^[0-7][0-9A-HJKMNP-TV-Z]{25}$`. Performs no I/O and cannot fail.
fn is_valid_ulid(candidate: &str) -> bool {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes = candidate.as_bytes();
    bytes.len() == 26
        && (b'0'..=b'7').contains(&bytes[0])
        && bytes[1..].iter().all(|b| ALPHABET.contains(b))
}

/// Verifies `glassbox models` CRUD lifecycle, `0600`/`0700` perms, raw-key exit `64`, secret redaction, and ambiguous env exit `64`.
///
/// Accepts no arguments. Exercises `endpoint add/list/remove`, `bind`/`unbind`/`show`,
/// raw key rejection, bound-endpoint `--force` guard, secret redaction in `show`,
/// and fail-closed exit `64` on simultaneous `GLASSBOX_ASR_URL` + `GLASSBOX_ASR_MODEL_DIR`
/// without explicit backend selection. Panics on any contract violation.
#[test]
fn models_cli_lifecycle_permissions_secret_hygiene_and_ambiguity_guard() {
    let dir = workdir("models-cli");
    let cfg_dir = dir.join("config-home");
    let cfg_dir_s = cfg_dir.to_str().unwrap();
    let secret_val = "sk-live-SUPER-SECRET-NEVER-PRINT-9999";

    // Reject raw API key on endpoint add (exit 64, never creates models.json).
    let raw_key = run_clean_env(
        &[
            "models",
            "endpoint",
            "add",
            "bad",
            "--base-url",
            "https://api.example.com/v1",
            "--api-key-env",
            "sk-live-1234567890",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(raw_key.status.code(), Some(64), "raw key must exit 64");
    assert!(
        !cfg_dir.join("models.json").exists(),
        "rejected endpoint add must not write models.json"
    );

    // Add valid endpoint with trailing slash and valid env-var name.
    let add = run_clean_env(
        &[
            "models",
            "endpoint",
            "add",
            "primary",
            "--base-url",
            "http://127.0.0.1:8080/v1///",
            "--api-key-env",
            "MY_ASR_SECRET_ENV",
        ],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
            ("MY_ASR_SECRET_ENV", secret_val),
        ],
    );
    assert_eq!(
        add.status.code(),
        Some(0),
        "endpoint add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let models_file = cfg_dir.join("models.json");
    assert!(models_file.exists());
    assert_no_temp_leftovers(&cfg_dir);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = std::fs::metadata(&cfg_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&models_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "config dir must be 0700");
        assert_eq!(file_mode, 0o600, "models.json must be 0600");
    }

    let raw_disk = std::fs::read_to_string(&models_file).unwrap();
    assert!(raw_disk.contains("MY_ASR_SECRET_ENV"));
    assert!(!raw_disk.contains(secret_val));

    // List endpoints in JSON and text formats.
    let list_json = run_clean_env(
        &["models", "endpoint", "list", "--format", "json"],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
            ("MY_ASR_SECRET_ENV", secret_val),
        ],
    );
    assert_eq!(list_json.status.code(), Some(0));
    let listed = json(&list_json);
    assert_eq!(
        listed["endpoints"]["primary"]["base_url"],
        "http://127.0.0.1:8080/v1"
    );
    assert_eq!(
        listed["endpoints"]["primary"]["api_key_env"],
        "MY_ASR_SECRET_ENV"
    );
    assert!(!String::from_utf8_lossy(&list_json.stdout).contains(secret_val));

    // Bind asr capability to primary endpoint.
    let bind = run_clean_env(
        &[
            "models",
            "bind",
            "asr",
            "--endpoint",
            "primary",
            "--model",
            "whisper-large-v3",
            "--protocol",
            "chat",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(
        bind.status.code(),
        Some(0),
        "bind failed: {}",
        String::from_utf8_lossy(&bind.stderr)
    );

    // Show text & JSON must never leak secret value.
    for extra in [
        &["models", "show"][..],
        &["models", "show", "asr", "--format", "json"][..],
    ] {
        let shown = run_clean_env(
            extra,
            &[
                ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
                ("MY_ASR_SECRET_ENV", secret_val),
            ],
        );
        assert_eq!(shown.status.code(), Some(0));
        let out_str = String::from_utf8_lossy(&shown.stdout);
        assert!(out_str.contains("MY_ASR_SECRET_ENV"), "got: {out_str}");
        assert!(
            !out_str.contains(secret_val),
            "secret leaked in show: {out_str}"
        );
    }

    // Removing bound endpoint without --force exits 64.
    let remove_blocked = run_clean_env(
        &["models", "endpoint", "remove", "primary"],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(remove_blocked.status.code(), Some(64));

    // Removing bound endpoint with --force succeeds and removes binding.
    let remove_forced = run_clean_env(
        &["models", "endpoint", "remove", "primary", "--force"],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(remove_forced.status.code(), Some(0));

    // Bind local-onnx then unbind cleanly.
    let bind_onnx = run_clean_env(
        &[
            "models",
            "bind",
            "asr",
            "--local-onnx",
            "--model-dir",
            "/tmp/fake-whisper-dir",
            "--device",
            "cpu",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(bind_onnx.status.code(), Some(0));
    let unbind = run_clean_env(
        &["models", "unbind", "asr"],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(unbind.status.code(), Some(0));
    let unbind_again = run_clean_env(
        &["models", "unbind", "asr"],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(unbind_again.status.code(), Some(64));

    // Ambiguous env vars (GLASSBOX_ASR_URL + GLASSBOX_ASR_MODEL_DIR without backend) exit 64 before opening file.
    let ambiguous = run_clean_env(
        &["extract", "/nonexistent/clip.mp4", "--asr", "short"],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
            ("GLASSBOX_ASR_URL", "http://127.0.0.1:8080/v1"),
            ("GLASSBOX_ASR_MODEL", "whisper-1"),
            ("GLASSBOX_ASR_MODEL_DIR", "/nonexistent/whisper-onnx"),
        ],
    );
    assert_eq!(
        ambiguous.status.code(),
        Some(64),
        "ambiguous env vars must exit 64: stderr={}",
        String::from_utf8_lossy(&ambiguous.stderr)
    );
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr).contains("ambiguous"),
        "expected ambiguity message, got: {}",
        String::from_utf8_lossy(&ambiguous.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Verifies `--extract-audio` and `--asr-*` exit-64 validation rules before opening any input file.
///
/// Accepts no arguments. Checks missing `--output-dir` for `--extract-audio`,
/// Q5 rejection of `--extract-audio` with `--asr short` and `--asr adaptive`,
/// orphan `--asr-*` flags without `--asr`, and raw secret rejected in
/// `--asr-api-key-env`. Panics if any invalid invocation fails to exit `64`.
#[test]
fn extract_audio_and_asr_flag_validation_exits_64_before_opening_file() {
    let dir = workdir("flag-validation");
    let out_dir = dir.join("out");
    let out_dir_s = out_dir.to_str().unwrap();
    let cfg_dir = dir.join("cfg");
    let cfg_dir_s = cfg_dir.to_str().unwrap();
    let missing = "/nonexistent/does-not-exist.mp4";

    let cases: &[(&str, Vec<&str>)] = &[
        (
            "--extract-audio without --output-dir",
            vec!["extract", missing, "--extract-audio", "wav"],
        ),
        (
            "bare --extract-audio without --output-dir",
            vec!["extract", missing, "--extract-audio"],
        ),
        (
            "--extract-audio with --asr short (Q5)",
            vec![
                "extract",
                missing,
                "--extract-audio",
                "wav",
                "--asr",
                "short",
                "--output-dir",
                out_dir_s,
                "--asr-url",
                "http://127.0.0.1:9/v1",
                "--asr-model",
                "whisper-1",
            ],
        ),
        (
            "--extract-audio with bare --asr (default short, Q5)",
            vec![
                "extract",
                missing,
                "--extract-audio",
                "--asr",
                "--output-dir",
                out_dir_s,
                "--asr-url",
                "http://127.0.0.1:9/v1",
                "--asr-model",
                "whisper-1",
            ],
        ),
        (
            "--extract-audio with --asr adaptive (Q5)",
            vec![
                "extract",
                missing,
                "--extract-audio",
                "flac",
                "--asr",
                "adaptive",
                "--output-dir",
                out_dir_s,
                "--asr-url",
                "http://127.0.0.1:9/v1",
                "--asr-model",
                "whisper-1",
            ],
        ),
        (
            "orphan --asr-url without --asr",
            vec!["extract", missing, "--asr-url", "http://127.0.0.1:9/v1"],
        ),
        (
            "orphan --asr-model without --asr",
            vec!["extract", missing, "--asr-model", "whisper-1"],
        ),
        (
            "orphan --asr-language without --asr",
            vec!["extract", missing, "--asr-language", "en"],
        ),
        (
            "raw API key in --asr-api-key-env",
            vec![
                "extract",
                missing,
                "--asr",
                "full",
                "--asr-url",
                "http://127.0.0.1:9/v1",
                "--asr-model",
                "whisper-1",
                "--asr-api-key-env",
                "sk-raw-key-rejected",
            ],
        ),
        (
            "--asr with no configured backend",
            vec!["extract", missing, "--asr", "full"],
        ),
    ];

    for (label, argv) in cases {
        let output = run_clean_env(argv, &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)]);
        assert_eq!(
            output.status.code(),
            Some(64),
            "case '{label}' expected exit 64, got {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stderr.is_empty(),
            "case '{label}' must print error message to stderr"
        );
    }

    // Confirm --extract-audio + --asr short error mentions the Q5 rule clearly.
    let q5 = run_clean_env(
        &[
            "extract",
            missing,
            "--extract-audio",
            "wav",
            "--asr",
            "short",
            "--output-dir",
            out_dir_s,
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    let q5_err = String::from_utf8_lossy(&q5.stderr);
    assert!(
        q5_err.contains("--asr full") && q5_err.contains("--asr short"),
        "expected Q5 guidance, got: {q5_err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Verifies `backend.validate()` fails fast before spawning FFmpeg/ffprobe when `api_key_env` is unset.
///
/// Accepts no arguments. Points `GLASSBOX_FFPROBE` at a nonexistent binary while
/// supplying an unset `--asr-api-key-env`; asserts the CLI exits `1` with
/// `asr_config_failed` naming the env var rather than `media_helper_missing`.
#[test]
fn asr_backend_validate_fails_fast_before_spawning_ffmpeg() {
    let dir = workdir("validate-fail-fast");
    let cfg_dir = dir.join("cfg");
    let dummy_input = dir.join("dummy.wav");
    std::fs::write(&dummy_input, b"not-a-real-wav").unwrap();

    let output = run_clean_env(
        &[
            "extract",
            dummy_input.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-url",
            "http://127.0.0.1:9/v1",
            "--asr-model",
            "whisper-1",
            "--asr-api-key-env",
            "DEFINITELY_UNSET_GLASSBOX_ASR_KEY",
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir.to_str().unwrap()),
            ("GLASSBOX_FFPROBE", "/nonexistent/ffprobe-binary"),
            ("GLASSBOX_FFMPEG", "/nonexistent/ffmpeg-binary"),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let report = json(&output);
    assert_eq!(report["status"], "failed");
    let diags = report["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        diags.iter().any(|d| {
            d["stage"] == "media"
                && d["code"] == "asr_config_failed"
                && d["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("DEFINITELY_UNSET_GLASSBOX_ASR_KEY"))
        }),
        "expected asr_config_failed before ffprobe, got diagnostics: {diags:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Verifies `--asr full --format markdown` (`transcriptions`), `glassbox:v1` frontmatter, `bodymatter:v1` `<time>` markers, zero temp files, and `--no-frontmatter`/`--no-markers` parity across stdout and `--output-dir`.
///
/// Accepts no arguments. Skips cleanly only if `ffmpeg`/`ffprobe` are absent.
/// Panics on any frontmatter, marker, HTTP payload, or temp-file violation.
#[test]
fn asr_full_transcriptions_markdown_frontmatter_and_suppression_flags() {
    if !host_has_ffmpeg() {
        eprintln!("skipping ffmpeg test: ffmpeg/ffprobe unavailable on host");
        return;
    }

    let dir = workdir("asr-full-md");
    let cfg_dir = dir.join("cfg");
    let isolated_tmp = dir.join("tmp");
    std::fs::create_dir_all(&isolated_tmp).unwrap();
    let mp4 = generate_av_mp4(&dir, "lecture.mp4", 4, Some("Briefing: \"Q4\" --- Live"));

    let server = spawn_mock_server(vec![
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"Hello from the quarterly briefing."}"#.to_owned(),
        },
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"Second run without frontmatter."}"#.to_owned(),
        },
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"Third run plain body only."}"#.to_owned(),
        },
    ]);

    // 1. Default Markdown (frontmatter + time markers) with trailing slash URL normalization & --output-dir.
    let out1 = dir.join("out1");
    let trailing_url = format!("{}/", server.base_url);
    let run1 = run_clean_env(
        &[
            "extract",
            mp4.to_str().unwrap(),
            "--asr",
            "full",
            "--format",
            "markdown",
            "--asr-url",
            &trailing_url,
            "--asr-model",
            "whisper-1",
            "--asr-language",
            "en",
            "--output-dir",
            out1.to_str().unwrap(),
        ],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir.to_str().unwrap()),
            ("TMPDIR", isolated_tmp.to_str().unwrap()),
        ],
    );
    assert_eq!(
        run1.status.code(),
        Some(0),
        "asr full markdown failed: stderr={}",
        String::from_utf8_lossy(&run1.stderr)
    );

    let md1 = String::from_utf8(run1.stdout).unwrap();
    let (fm, body) = parse_frontmatter(&md1);
    assert_eq!(
        fm.get("schema").and_then(Value::as_str),
        Some("glassbox:v1")
    );
    let ulid = fm.get("id").and_then(Value::as_str).expect("id present");
    assert!(is_valid_ulid(ulid), "invalid ULID: {ulid}");
    assert_eq!(
        fm.get("title").and_then(Value::as_str),
        Some("Briefing: \"Q4\" --- Live")
    );
    assert_eq!(fm.get("type").and_then(Value::as_str), Some("transcript"));
    assert_eq!(fm.get("tags"), Some(&serde_json::json!([])));
    assert_eq!(fm.get("language").and_then(Value::as_str), Some("en"));
    assert!(fm.contains_key("updated_at"));
    assert!(
        !fm.contains_key("content_type"),
        "content_type must be omitted"
    );
    assert!(
        !fm.contains_key("sha256"),
        "sha256 must be omitted from frontmatter"
    );

    let extraction = fm.get("extraction").expect("extraction block");
    assert_eq!(extraction["mode"], "full");
    assert_eq!(extraction["backend"], "openai");
    assert_eq!(extraction["model"], "whisper-1");
    assert_eq!(extraction["completeness"], "exact");
    assert!(extraction["duration_ms"].as_u64().unwrap_or(0) >= 3_500);
    assert_eq!(extraction["sampled_ranges_ms"].as_array().unwrap().len(), 1);

    assert!(
        body.contains("<!-- bodymatter:v1 <time start=\"00:00:00.000\" end=\""),
        "expected time marker in body:\n{body}"
    );
    assert!(body.contains("Hello from the quarterly briefing."));

    // Verify saved .md in --output-dir matches structure and has zero temp files in source/out/tmp.
    let saved_md1_path = std::fs::read_dir(&out1)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .expect("saved .md artifact");
    let saved_md1 = std::fs::read_to_string(&saved_md1_path).unwrap();
    let (saved_fm1, saved_body1) = parse_frontmatter(&saved_md1);
    assert_eq!(
        saved_fm1.get("schema").and_then(Value::as_str),
        Some("glassbox:v1")
    );
    assert_eq!(saved_body1, body);
    assert_no_temp_leftovers(&dir);
    assert_no_temp_leftovers(&out1);
    assert!(
        std::fs::read_dir(&isolated_tmp).unwrap().next().is_none(),
        "zero temp audio files may be created in TMPDIR"
    );

    // Inspect captured multipart request.
    {
        let reqs = server.captured.lock().unwrap();
        assert_eq!(
            reqs[0].request_line,
            "POST /v1/audio/transcriptions HTTP/1.1"
        );
        assert!(!reqs[0].headers.contains_key("authorization"));
        let ct = reqs[0].headers.get("content-type").expect("content-type");
        assert!(ct.starts_with("multipart/form-data;"), "ct={ct}");
        assert!(
            reqs[0].body.windows(4).any(|w| w == b"RIFF"),
            "multipart payload must include WAV bytes"
        );
    }

    // 2. --no-frontmatter suppresses frontmatter on BOTH stdout and saved .md while keeping <time> markers.
    let out2 = dir.join("out2");
    let run2 = run_clean_env(
        &[
            "extract",
            mp4.to_str().unwrap(),
            "--asr",
            "full",
            "--format",
            "markdown",
            "--no-frontmatter",
            "--asr-url",
            &server.base_url,
            "--asr-model",
            "whisper-1",
            "--output-dir",
            out2.to_str().unwrap(),
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir.to_str().unwrap())],
    );
    assert_eq!(run2.status.code(), Some(0));
    let md2 = String::from_utf8(run2.stdout).unwrap();
    let saved_md2_path = std::fs::read_dir(&out2)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .expect("saved .md artifact 2");
    let saved_md2 = std::fs::read_to_string(&saved_md2_path).unwrap();
    for content in [&md2, &saved_md2] {
        assert!(
            !content.contains("schema: \"glassbox:v1\""),
            "frontmatter leaked: {content}"
        );
        assert!(!content.starts_with("---"), "YAML fence leaked: {content}");
        assert!(
            content.contains("<!-- bodymatter:v1 <time start="),
            "time marker missing: {content}"
        );
        assert!(content.contains("Second run without frontmatter."));
    }

    // 3. --no-frontmatter + --no-markers suppresses both on stdout and saved .md.
    let out3 = dir.join("out3");
    let run3 = run_clean_env(
        &[
            "extract",
            mp4.to_str().unwrap(),
            "--asr",
            "full",
            "--format",
            "markdown",
            "--no-frontmatter",
            "--no-markers",
            "--asr-url",
            &server.base_url,
            "--asr-model",
            "whisper-1",
            "--output-dir",
            out3.to_str().unwrap(),
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir.to_str().unwrap())],
    );
    assert_eq!(run3.status.code(), Some(0));
    let md3 = String::from_utf8(run3.stdout).unwrap();
    let saved_md3_path = std::fs::read_dir(&out3)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .expect("saved .md artifact 3");
    let saved_md3 = std::fs::read_to_string(&saved_md3_path).unwrap();
    for content in [&md3, &saved_md3] {
        assert!(!content.contains("glassbox:v1"));
        assert!(!content.contains("bodymatter:v1"));
        assert_eq!(content.trim(), "Third run plain body only.");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Verifies `--asr short` (VAD skipping leading silence) and `--asr adaptive` (`chat` `input_audio` with Bearer auth & redaction).
///
/// Accepts no arguments. Skips cleanly only if `ffmpeg`/`ffprobe` are absent.
/// Panics on unexpected exit codes, selection/completeness metadata, protocol
/// payload mismatches, or secret leaks.
#[test]
fn asr_short_vad_and_adaptive_chat_protocol_with_auth_redaction() {
    if !host_has_ffmpeg() {
        eprintln!("skipping ffmpeg test: ffmpeg/ffprobe unavailable on host");
        return;
    }

    let dir = workdir("asr-short-adaptive");
    let cfg_dir = dir.join("cfg");
    let cfg_dir_s = cfg_dir.to_str().unwrap();
    let wav = generate_silence_then_speech_wav(&dir, "intro_silence.wav");
    let secret = "chat-bearer-token-secret-4242";

    let server = spawn_mock_server(vec![
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"Speech started after the intro silence."}"#.to_owned(),
        },
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: serde_json::json!({
                "choices": [{
                    "message": {
                        "content": "Adaptive chat sample transcript."
                    }
                }]
            })
            .to_string(),
        },
    ]);

    // Configure persistent models.json for the --asr short run.
    let add = run_clean_env(
        &[
            "models",
            "endpoint",
            "add",
            "mock",
            "--base-url",
            &server.base_url,
            "--api-key-env",
            "TEST_MOCK_API_KEY",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(add.status.code(), Some(0));
    let bind = run_clean_env(
        &[
            "models",
            "bind",
            "asr",
            "--endpoint",
            "mock",
            "--model",
            "whisper-short-v1",
            "--protocol",
            "transcriptions",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(bind.status.code(), Some(0));

    // 1. --asr short resolved purely from models.json!
    let short_out = run_clean_env(
        &[
            "extract",
            wav.to_str().unwrap(),
            "--asr",
            "short",
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
            ("TEST_MOCK_API_KEY", secret),
        ],
    );
    assert_eq!(
        short_out.status.code(),
        Some(0),
        "asr short failed: stderr={}",
        String::from_utf8_lossy(&short_out.stderr)
    );
    let short_report = json(&short_out);
    assert_eq!(short_report["status"], "complete");
    assert_eq!(short_report["completeness"], "partial");
    assert_eq!(short_report["selection"]["omitted"], "selection");
    assert_eq!(short_report["selection"]["completeness"], "partial");
    assert_eq!(short_report["media"]["asr"]["mode"], "short");
    assert_eq!(short_report["media"]["asr"]["model"], "whisper-short-v1");
    let range0 = short_report["media"]["asr"]["sampled_ranges_ms"][0]
        .as_array()
        .expect("sampled range 0");
    assert!(
        range0[0].as_u64().unwrap() >= 1_000,
        "VAD should skip leading 2s silence (with 500ms pre-roll), got start_ms={}",
        range0[0]
    );
    assert!(!String::from_utf8_lossy(&short_out.stdout).contains(secret));

    // 2. --asr adaptive using --asr-protocol chat overriding models.json protocol.
    let adaptive_out = run_clean_env(
        &[
            "extract",
            wav.to_str().unwrap(),
            "--asr",
            "adaptive",
            "--asr-protocol",
            "chat",
            "--asr-model",
            "gemma-audio-v1",
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
            ("TEST_MOCK_API_KEY", secret),
        ],
    );
    assert_eq!(
        adaptive_out.status.code(),
        Some(0),
        "asr adaptive chat failed: stderr={}",
        String::from_utf8_lossy(&adaptive_out.stderr)
    );
    let adaptive_report = json(&adaptive_out);
    assert_eq!(adaptive_report["status"], "complete");
    assert_eq!(adaptive_report["completeness"], "partial");
    assert_eq!(adaptive_report["selection"]["omitted"], "selection");
    assert_eq!(adaptive_report["selection"]["completeness"], "partial");
    assert_eq!(adaptive_report["media"]["asr"]["mode"], "adaptive");
    assert_eq!(adaptive_report["media"]["asr"]["protocol"], "chat");
    assert_eq!(adaptive_report["media"]["asr"]["model"], "gemma-audio-v1");
    assert_eq!(
        adaptive_report["content"]["units"][0]["text"],
        "Adaptive chat sample transcript."
    );
    assert!(!String::from_utf8_lossy(&adaptive_out.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&adaptive_out.stderr).contains(secret));

    // Verify captured chat request sent Bearer auth and base64 input_audio WAV.
    {
        let reqs = server.captured.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(
            reqs[1].headers.get("authorization").map(String::as_str),
            Some("Bearer chat-bearer-token-secret-4242")
        );
        assert_eq!(reqs[1].request_line, "POST /v1/chat/completions HTTP/1.1");
        let payload: Value = serde_json::from_slice(&reqs[1].body).expect("chat JSON body");
        assert_eq!(payload["model"], "gemma-audio-v1");
        let audio_part = &payload["messages"][0]["content"][1];
        assert_eq!(audio_part["type"], "input_audio");
        assert_eq!(audio_part["input_audio"]["format"], "wav");
        assert!(
            !audio_part["input_audio"]["data"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Verifies 5xx retry-once recovery (exit `0`), 401 zero-retry secret redaction (exit `1`), and multi-window partial failure (exit `2`).
///
/// Accepts no arguments. Skips cleanly only if `ffmpeg`/`ffprobe` are absent.
/// Panics if retry counts, exit codes, secret redaction, or preserved window-1
/// text violate the ADR-003 / ADR-004 contracts.
#[test]
fn asr_http_retry_401_redaction_and_multi_window_partial_failure() {
    if !host_has_ffmpeg() {
        eprintln!("skipping ffmpeg test: ffmpeg/ffprobe unavailable on host");
        return;
    }

    let dir = workdir("asr-retries-partial");
    let cfg_dir = dir.join("cfg");
    let cfg_dir_s = cfg_dir.to_str().unwrap();
    let short_wav = generate_synthetic_wav(&dir, "short.wav", 3);
    let long_wav = generate_synthetic_wav(&dir, "two_windows.wav", 35);

    // 1. 5xx retry-once recovery: attempt 1 -> 503, attempt 2 -> 200 -> exit 0.
    let retry_server = spawn_mock_server(vec![
        MockResponse {
            status_line: "503 Service Unavailable",
            content_type: "application/json",
            body: r#"{"error":"temporary overload"}"#.to_owned(),
        },
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"Recovered after single 503 retry."}"#.to_owned(),
        },
    ]);
    let retry_out = run_clean_env(
        &[
            "extract",
            short_wav.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-url",
            &retry_server.base_url,
            "--asr-model",
            "whisper-1",
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(retry_out.status.code(), Some(0));
    assert_eq!(retry_server.hits.load(Ordering::SeqCst), 2);
    let retry_report = json(&retry_out);
    assert_eq!(retry_report["status"], "complete");
    assert_eq!(
        retry_report["content"]["units"][0]["text"],
        "Recovered after single 503 retry."
    );

    // 2. 401 zero retries + secret redaction: exits 1, exactly 1 hit, secret never echoed.
    let secret_401 = "sk-top-secret-401-token-xyz";
    let auth_server = spawn_mock_server(vec![
        MockResponse {
            status_line: "401 Unauthorized",
            content_type: "application/json",
            body: format!(r#"{{"error":"invalid credentials for Bearer {secret_401}"}}"#),
        },
        // Extra response that must NEVER be consumed because 401 must not retry.
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"should never be reached"}"#.to_owned(),
        },
    ]);
    let auth_out = run_clean_env(
        &[
            "extract",
            long_wav.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-url",
            &auth_server.base_url,
            "--asr-model",
            "whisper-1",
            "--asr-api-key-env",
            "SECRET_401_ENV",
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
            ("SECRET_401_ENV", secret_401),
        ],
    );
    assert_eq!(auth_out.status.code(), Some(1));
    assert_eq!(
        auth_server.hits.load(Ordering::SeqCst),
        1,
        "401 must perform zero retries and stop before window 2"
    );
    let auth_stdout = String::from_utf8_lossy(&auth_out.stdout);
    let auth_stderr = String::from_utf8_lossy(&auth_out.stderr);
    assert!(
        !auth_stdout.contains(secret_401),
        "secret leaked in stdout: {auth_stdout}"
    );
    assert!(
        !auth_stderr.contains(secret_401),
        "secret leaked in stderr: {auth_stderr}"
    );
    let auth_report = json(&auth_out);
    assert_eq!(auth_report["status"], "failed");
    assert!(
        auth_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["stage"] == "media" && d["severity"] == "error")
    );

    // 3. Multi-window partial failure: window 1 -> 200, window 2 -> 500 twice -> exit 2, status partial, window 1 preserved!
    let partial_server = spawn_mock_server(vec![
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"Window one succeeded cleanly."}"#.to_owned(),
        },
        MockResponse {
            status_line: "500 Internal Server Error",
            content_type: "application/json",
            body: r#"{"error":"upstream crash 1"}"#.to_owned(),
        },
        MockResponse {
            status_line: "500 Internal Server Error",
            content_type: "application/json",
            body: r#"{"error":"upstream crash 2"}"#.to_owned(),
        },
    ]);
    let partial_out = run_clean_env(
        &[
            "extract",
            long_wav.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-url",
            &partial_server.base_url,
            "--asr-model",
            "whisper-1",
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(
        partial_out.status.code(),
        Some(2),
        "window-2 failure after window-1 success must exit 2: stderr={}",
        String::from_utf8_lossy(&partial_out.stderr)
    );
    assert_eq!(partial_server.hits.load(Ordering::SeqCst), 3);
    let partial_report = json(&partial_out);
    assert_eq!(partial_report["status"], "partial");
    assert_eq!(partial_report["completeness"], "partial");
    assert_eq!(partial_report["selection"]["omitted"], "partial_failure");
    assert_eq!(partial_report["media"]["asr"]["windows_attempted"], 2);
    assert_eq!(partial_report["media"]["asr"]["windows_succeeded"], 1);
    assert_eq!(
        partial_report["content"]["units"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        partial_report["content"]["units"][0]["text"],
        "Window one succeeded cleanly."
    );
    assert!(
        partial_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["stage"] == "media" && d["severity"] == "error" && d["unit_index"] == 2)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Verifies standalone `--extract-audio wav`, collision refusal, combined `--asr full --extract-audio wav` single-pass tee + stitching, and mute-video exit `1`.
///
/// Accepts no arguments. Skips cleanly only if `ffmpeg`/`ffprobe` are absent.
/// Panics if WAV headers, `0600` permissions, no-overwrite guarantees,
/// frontmatter `audio_extract` links, overlap stitching, or mute-video errors fail.
#[test]
fn extract_audio_standalone_teed_collision_and_mute_video() {
    if !host_has_ffmpeg() {
        eprintln!("skipping ffmpeg test: ffmpeg/ffprobe unavailable on host");
        return;
    }

    let dir = workdir("extract-audio-and-mute");
    let cfg_dir = dir.join("cfg");
    let cfg_dir_s = cfg_dir.to_str().unwrap();
    let av_mp4 = generate_av_mp4(&dir, "panel_talk.mp4", 4, Some("Panel Talk"));
    let long_wav = generate_synthetic_wav(&dir, "panel_long.wav", 35);
    let mute_mp4 = generate_mute_mp4(&dir, "silent_cam.mp4");

    // 1. Standalone --extract-audio wav --output-dir <out1>
    let out1 = dir.join("out1");
    let standalone = run_clean_env(
        &[
            "extract",
            av_mp4.to_str().unwrap(),
            "--extract-audio",
            "wav",
            "--output-dir",
            out1.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(
        standalone.status.code(),
        Some(0),
        "standalone extract-audio failed: stderr={}",
        String::from_utf8_lossy(&standalone.stderr)
    );
    let standalone_report = json(&standalone);
    assert_eq!(standalone_report["status"], "complete");
    assert_eq!(standalone_report["media"]["audio_extract"]["format"], "wav");
    let wav_path = out1.join("panel_talk-extract.wav");
    assert!(wav_path.exists(), "expected {}", wav_path.display());
    assert_no_temp_leftovers(&out1);

    let wav_bytes = std::fs::read(&wav_path).unwrap();
    assert!(wav_bytes.len() > 44);
    assert_eq!(&wav_bytes[0..4], b"RIFF");
    assert_eq!(&wav_bytes[8..12], b"WAVE");
    let riff_len = u32::from_le_bytes(wav_bytes[4..8].try_into().unwrap()) as usize;
    let data_len = u32::from_le_bytes(wav_bytes[40..44].try_into().unwrap()) as usize;
    assert_eq!(riff_len + 8, wav_bytes.len());
    assert_eq!(data_len + 44, wav_bytes.len());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&wav_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "extracted audio must be mode 0600");
    }

    // 2. Collision refusal on existing <stem>-extract.wav
    let out2 = dir.join("out2");
    std::fs::create_dir_all(&out2).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&out2, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let existing_wav = out2.join("panel_talk-extract.wav");
    std::fs::write(&existing_wav, b"DO_NOT_OVERWRITE_SENTINEL").unwrap();

    let collide = run_clean_env(
        &[
            "extract",
            av_mp4.to_str().unwrap(),
            "--extract-audio",
            "wav",
            "--output-dir",
            out2.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(collide.status.code(), Some(1));
    assert_eq!(
        std::fs::read(&existing_wav).unwrap(),
        b"DO_NOT_OVERWRITE_SENTINEL",
        "existing file must never be overwritten"
    );
    assert_no_temp_leftovers(&out2);
    let collide_report = json(&collide);
    assert!(
        collide_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["stage"] == "media" && d["code"] == "audio_extract_exists")
    );

    // 3. Combined --asr full --extract-audio wav (single-pass tee + 2-window overlap stitching)
    let out3 = dir.join("out3");
    let tee_server = spawn_mock_server(vec![
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"We discuss usable energy today."}"#.to_owned(),
        },
        MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"Usable energy today, then fuels."}"#.to_owned(),
        },
    ]);
    let combined = run_clean_env(
        &[
            "extract",
            long_wav.to_str().unwrap(),
            "--asr",
            "full",
            "--extract-audio",
            "wav",
            "--output-dir",
            out3.to_str().unwrap(),
            "--asr-url",
            &tee_server.base_url,
            "--asr-model",
            "whisper-1",
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(
        combined.status.code(),
        Some(0),
        "combined asr full + extract-audio wav failed: stderr={}",
        String::from_utf8_lossy(&combined.stderr)
    );
    let combined_report = json(&combined);
    assert_eq!(combined_report["status"], "complete");
    assert_eq!(combined_report["completeness"], "exact");
    assert_eq!(combined_report["media"]["asr"]["windows_attempted"], 2);
    assert_eq!(combined_report["media"]["asr"]["windows_succeeded"], 2);
    assert_eq!(combined_report["media"]["audio_extract"]["format"], "wav");
    assert_eq!(
        combined_report["content"]["units"][0]["text"],
        "We discuss usable energy today."
    );
    assert_eq!(
        combined_report["content"]["units"][1]["text"], "then fuels.",
        "expected >=3-word normalized overlap stitching across 4s window overlap"
    );

    let teed_wav = out3.join("panel_long-extract.wav");
    assert!(teed_wav.exists());
    let teed_bytes = std::fs::read(&teed_wav).unwrap();
    assert_eq!(&teed_bytes[0..4], b"RIFF");
    let teed_data_len = u32::from_le_bytes(teed_bytes[40..44].try_into().unwrap()) as usize;
    assert_eq!(teed_data_len + 44, teed_bytes.len());
    // 35 s * 16_000 Hz * 2 bytes/sample = 1_120_000 bytes (single-pass tee writes each sample ONCE, not duplicated across overlap!).
    assert_eq!(
        teed_data_len,
        35 * 16_000 * 2,
        "single-pass WAV tee must write each decoded sample exactly once"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&teed_wav).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert_no_temp_leftovers(&out3);

    // Saved .md frontmatter links the published audio_extract path.
    let saved_md_path = std::fs::read_dir(&out3)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .expect("saved .md");
    let saved_md = std::fs::read_to_string(&saved_md_path).unwrap();
    let (fm, _) = parse_frontmatter(&saved_md);
    assert!(
        fm["extraction"]["audio_extract"]
            .as_str()
            .is_some_and(|p| p.ends_with("panel_long-extract.wav")),
        "expected audio_extract path in frontmatter: {fm:?}"
    );

    // 4. Mute video (video-only MP4 with -an) passed to --asr full -> exits 1 with DiagnosticStage::Media and 0 HTTP calls.
    let mute_server = spawn_mock_server(vec![MockResponse {
        status_line: "200 OK",
        content_type: "application/json",
        body: r#"{"text":"must never be called"}"#.to_owned(),
    }]);
    let mute_out = run_clean_env(
        &[
            "extract",
            mute_mp4.to_str().unwrap(),
            "--asr",
            "full",
            "--asr-url",
            &mute_server.base_url,
            "--asr-model",
            "whisper-1",
            "--format",
            "json",
        ],
        &[("GLASSBOX_CONFIG_DIR", cfg_dir_s)],
    );
    assert_eq!(mute_out.status.code(), Some(1));
    assert_eq!(mute_server.hits.load(Ordering::SeqCst), 0);
    let mute_report = json(&mute_out);
    assert_eq!(mute_report["status"], "failed");
    assert!(
        mute_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["stage"] == "media" && d["code"] == "media_probe_failed"),
        "expected media_probe_failed on mute MP4: {:?}",
        mute_report["diagnostics"]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Verifies a stalled `GLASSBOX_FFMPEG` wrapper times out cleanly across `--asr full`, `--asr short`, and `--extract-audio wav`.
///
/// Accepts no arguments. Skips with an explicit diagnostic only when `ffmpeg`
/// or `ffprobe` is absent on the host. Generates a real local synthetic WAV
/// fixture so host `ffprobe` succeeds, writes an executable shell wrapper
/// under `extensionalities/target/tests/` that sleeps 15 seconds on stdout,
/// and invokes `--asr full`, `--asr short`, and `--extract-audio wav --output-dir`
/// with `GLASSBOX_FFMPEG=<wrapper>` and `GLASSBOX_FFMPEG_IO_TIMEOUT_SECS=1`.
/// Asserts every invocation terminates in `< 6 s` with exit `1`, failed v3
/// status/selection, a `media_timeout` error diagnostic, zero HTTP calls, and
/// zero leaked audio artifacts or temporary files.
#[cfg(unix)]
#[test]
fn sleeping_ffmpeg_wrapper_times_out_cleanly_across_full_short_and_extract_audio() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    if !host_has_ffmpeg() {
        eprintln!(
            "SKIP sleeping_ffmpeg_wrapper_times_out_cleanly_across_full_short_and_extract_audio: host does not have ffmpeg/ffprobe on PATH"
        );
        return;
    }

    let dir = workdir("ffmpeg-timeout");
    let cfg_dir = dir.join("cfg");
    let cfg_dir_s = cfg_dir.to_str().unwrap();
    let wav = generate_synthetic_wav(&dir, "sample.wav", 3);

    let wrapper = dir.join("sleep-ffmpeg.sh");
    std::fs::write(&wrapper, "#!/bin/sh\nexec sleep 15\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wrapper_s = wrapper.to_str().unwrap();

    let server = spawn_mock_server(vec![]);
    let deadline = Duration::from_secs(6);

    // 1 & 2. `--asr full` and `--asr short` with sleeping FFmpeg wrapper.
    for mode in ["full", "short"] {
        let started = Instant::now();
        let output = run_clean_env(
            &[
                "extract",
                wav.to_str().unwrap(),
                "--asr",
                mode,
                "--asr-url",
                &server.base_url,
                "--asr-model",
                "whisper-1",
                "--format",
                "json",
            ],
            &[
                ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
                ("GLASSBOX_FFMPEG", wrapper_s),
                ("GLASSBOX_FFMPEG_IO_TIMEOUT_SECS", "1"),
            ],
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < deadline,
            "--asr {mode} hung for {:.2}s (expected < 6s): stderr={}",
            elapsed.as_secs_f64(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "--asr {mode} timeout must exit 1: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report = json(&output);
        assert_eq!(report["status"], "failed", "--asr {mode}: {report}");
        assert_eq!(
            report["completeness"], "unavailable",
            "--asr {mode}: {report}"
        );
        assert_eq!(
            report["selection"]["omitted"], "partial_failure",
            "--asr {mode}: {report}"
        );
        assert_eq!(
            report["selection"]["completeness"], "unavailable",
            "--asr {mode}: {report}"
        );
        assert!(report.get("media").is_none(), "--asr {mode}: {report}");
        assert!(
            report["diagnostics"].as_array().unwrap().iter().any(|d| {
                d["stage"] == "media" && d["severity"] == "error" && d["code"] == "media_timeout"
            }),
            "--asr {mode} expected media_timeout diagnostic: {:?}",
            report["diagnostics"]
        );
    }
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        0,
        "timed-out FFmpeg must not invoke ASR HTTP endpoint"
    );

    // 3. `--extract-audio wav --output-dir <dir>` with sleeping FFmpeg wrapper.
    let extract_out = dir.join("extract-out");
    let started = Instant::now();
    let extract_res = run_clean_env(
        &[
            "extract",
            wav.to_str().unwrap(),
            "--extract-audio",
            "wav",
            "--output-dir",
            extract_out.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[
            ("GLASSBOX_CONFIG_DIR", cfg_dir_s),
            ("GLASSBOX_FFMPEG", wrapper_s),
            ("GLASSBOX_FFMPEG_IO_TIMEOUT_SECS", "1"),
        ],
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < deadline,
        "--extract-audio wav hung for {:.2}s (expected < 6s): stderr={}",
        elapsed.as_secs_f64(),
        String::from_utf8_lossy(&extract_res.stderr)
    );
    assert_eq!(
        extract_res.status.code(),
        Some(1),
        "--extract-audio wav timeout must exit 1: stdout={} stderr={}",
        String::from_utf8_lossy(&extract_res.stdout),
        String::from_utf8_lossy(&extract_res.stderr)
    );
    let extract_report = json(&extract_res);
    assert_eq!(extract_report["status"], "failed");
    assert_eq!(extract_report["completeness"], "unavailable");
    assert_eq!(extract_report["selection"]["omitted"], "partial_failure");
    assert_eq!(extract_report["selection"]["completeness"], "unavailable");
    assert!(extract_report.get("media").is_none());
    assert!(
        extract_report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| {
                d["stage"] == "media" && d["severity"] == "error" && d["code"] == "media_timeout"
            }),
        "--extract-audio wav expected media_timeout diagnostic: {:?}",
        extract_report["diagnostics"]
    );
    assert!(
        !extract_out.join("sample-extract.wav").exists(),
        "timed-out --extract-audio must not publish sample-extract.wav"
    );
    assert_no_temp_leftovers(&extract_out);

    let _ = std::fs::remove_dir_all(&dir);
}

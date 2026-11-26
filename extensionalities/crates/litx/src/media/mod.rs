//! Module: glassbox-cli local media processing adapters.
//! File: media/mod.rs.
//! Responsibility: Provide shared media error types, lazy FFmpeg/ffprobe binary resolution, bounded subprocess execution, and ASR + audio-extraction orchestration.

pub mod asr;
pub mod asr_openai;
#[allow(dead_code)]
// Exports public Whisper ONNX helpers and geometry constants verified by unit tests.
pub mod asr_whisper;
pub mod audio_pipe;
pub mod contact_sheet;
pub mod ffprobe;
pub mod stitch;
#[allow(dead_code)]
// Exports public VAD tuning constants (e.g., PRE_ROLL_SAMPLES) verified by unit tests.
pub mod vad;

use self::asr::{AsrBackend, AsrError, AsrMode, AudioExtractFormat, TranscriptChunk};
use self::asr_openai::{OpenAiAsrBackend, OpenAiAsrConfig};
use self::asr_whisper::{WhisperOnnxAsrBackend, WhisperOnnxConfig};
use self::audio_pipe::{AudioWindowStream, WAV_HEADER_BYTES, extract_audio_to_staged};
use crate::models::ResolvedAsrConfig;
use crate::output::{StagedWriter, audio_extract_filename};
use glassbox_inspect::reports::{
    Artifact, ArtifactKind, AsrReceipt, AudioExtractReceipt, Completeness, ContentOrigin,
    ContentUnit, Diagnostic, DiagnosticSeverity, DiagnosticStage, EmbeddedMetadataField,
    ExtractReport, ExtractionProvenance, FactSource, MediaReceipt, NativeSelection, NativeUnit,
    OmissionStatus, OperationStatus, QualifiedCount, SelectionReport, SourcedMetadata,
};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Environment variable overriding the `ffmpeg` executable path.
pub const FFMPEG_ENV_VAR: &str = "GLASSBOX_FFMPEG";

/// Environment variable overriding the `ffprobe` executable path.
pub const FFPROBE_ENV_VAR: &str = "GLASSBOX_FFPROBE";

/// Poll cadence used while waiting for a timeout-bounded media subprocess.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Maximum stdout bytes retained in memory by [`run_command_with_timeout`] (16 MiB).
const MAX_COMMAND_STDOUT_BYTES: usize = 16 * 1024 * 1024;

/// Maximum stderr bytes retained in memory by [`run_command_with_timeout`] (256 KiB).
const MAX_COMMAND_STDERR_BYTES: usize = 256 * 1024;

/// Read buffer chunk size used when draining child stdout/stderr pipes.
const PIPE_DRAIN_CHUNK_BYTES: usize = 16 * 1024;

/// Errors returned by local media probing, frame decoding, compositing, and encoding.
#[derive(Debug)]
pub enum MediaError {
    /// Required external media helper (`ffmpeg` or `ffprobe`) was missing or not executable.
    MissingHelper(String),
    /// Caller supplied invalid media parameters (for example, an interval exceeding 250 tiles).
    InvalidInput(String),
    /// `ffprobe` exited with an error or returned no usable stream or duration metadata.
    ProbeFailed(String),
    /// `ffmpeg` failed to decode a frame or media stream.
    DecodeFailed(String),
    /// A media subprocess exceeded its execution deadline and was terminated.
    Timeout(String),
    /// Local OS I/O error occurred while spawning or reading a media subprocess.
    Io(std::io::Error),
    /// In-memory encoding of a contact sheet or media buffer failed.
    EncodeFailed(String),
}

impl fmt::Display for MediaError {
    /// Formats a human-readable diagnostic message for the media error.
    ///
    /// Accepts a mutable formatter. Returns `fmt::Result` after writing the
    /// structured error description. Fails only if the underlying formatter fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHelper(msg)
            | Self::InvalidInput(msg)
            | Self::ProbeFailed(msg)
            | Self::DecodeFailed(msg)
            | Self::Timeout(msg)
            | Self::EncodeFailed(msg) => f.write_str(msg),
            Self::Io(err) => write!(f, "media subprocess I/O error: {err}"),
        }
    }
}

impl std::error::Error for MediaError {
    /// Returns the underlying OS error when this error wraps an I/O failure.
    ///
    /// Accepts `&self`. Returns `Some` for `MediaError::Io` and `None` for all
    /// other variants. This method cannot fail.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MediaError {
    /// Converts a standard I/O error into `MediaError::Io`.
    ///
    /// Accepts an `std::io::Error`. Returns `MediaError::Io`. This conversion
    /// cannot fail.
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Validated settings for ASR transcription and/or full-track `--extract-audio`.
#[derive(Debug, Clone)]
pub(crate) struct AudioPipelineConfig {
    /// Optional ASR sampling mode (`short`, `adaptive`, or `full`).
    pub asr_mode: Option<AsrMode>,
    /// Resolved ASR backend settings (populated whenever `asr_mode` is `Some`).
    pub asr_config: Option<ResolvedAsrConfig>,
    /// Optional BCP-47 / ISO language hint forwarded to the ASR backend.
    pub asr_language: Option<String>,
    /// Optional full-track audio extraction format (`wav`, `flac`, `m4a`, or `opus`).
    pub extract_audio: Option<AudioExtractFormat>,
}

/// Maps a runtime [`MediaError`] variant to a stable snake_case diagnostic code.
///
/// Accepts a reference to a [`MediaError`]. Returns a static code string suitable
/// for [`Diagnostic::code`]. Performs no I/O and cannot fail.
pub(crate) fn media_error_code(error: &MediaError) -> &'static str {
    match error {
        MediaError::MissingHelper(_) => "media_helper_missing",
        MediaError::InvalidInput(_) => "media_invalid_input",
        MediaError::ProbeFailed(_) => "media_probe_failed",
        MediaError::DecodeFailed(_) => "media_decode_failed",
        MediaError::Timeout(_) => "media_timeout",
        MediaError::Io(_) => "media_io_failed",
        MediaError::EncodeFailed(_) => "media_encode_failed",
    }
}

/// Maps an [`AsrError`] variant to a stable snake_case diagnostic code.
///
/// Accepts a reference to an [`AsrError`]. Returns a static code string suitable
/// for [`Diagnostic::code`]. Performs no I/O and cannot fail.
fn asr_error_code(error: &AsrError) -> &'static str {
    match error {
        AsrError::Config(_) => "asr_config_failed",
        AsrError::MissingAsset(_) => "asr_asset_missing",
        AsrError::Http { .. } => "asr_http_failed",
        AsrError::Decode(_) => "asr_decode_failed",
        AsrError::Runtime(_) => "asr_runtime_failed",
        AsrError::Io(_) => "asr_io_failed",
    }
}

/// Resolves the `ffmpeg` executable lazily from `GLASSBOX_FFMPEG` or `PATH`.
///
/// Accepts no arguments. Returns the resolved executable `PathBuf`. Returns
/// `MediaError::MissingHelper` mentioning `extensionalities/DEPENDENCIES.md`
/// when `GLASSBOX_FFMPEG` is invalid or `ffmpeg` cannot be found on `PATH`.
pub fn resolve_ffmpeg() -> Result<PathBuf, MediaError> {
    resolve_helper_with(
        "ffmpeg",
        FFMPEG_ENV_VAR,
        std::env::var_os(FFMPEG_ENV_VAR).as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

/// Resolves the `ffprobe` executable lazily from `GLASSBOX_FFPROBE` or `PATH`.
///
/// Accepts no arguments. Returns the resolved executable `PathBuf`. Returns
/// `MediaError::MissingHelper` mentioning `extensionalities/DEPENDENCIES.md`
/// when `GLASSBOX_FFPROBE` is invalid or `ffprobe` cannot be found on `PATH`.
pub fn resolve_ffprobe() -> Result<PathBuf, MediaError> {
    resolve_helper_with(
        "ffprobe",
        FFPROBE_ENV_VAR,
        std::env::var_os(FFPROBE_ENV_VAR).as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

/// Resolves a media helper executable using an explicit env override and `PATH`.
///
/// Accepts the helper name, override env-var name, optional override value, and
/// optional `PATH` value. Returns the first verified executable path. Returns
/// `MediaError::MissingHelper` with installation guidance pointing to
/// `extensionalities/DEPENDENCIES.md` if an explicit override is unusable or no
/// executable is found on `PATH`.
fn resolve_helper_with(
    tool: &'static str,
    env_var: &'static str,
    override_val: Option<&OsStr>,
    path_env: Option<&OsStr>,
) -> Result<PathBuf, MediaError> {
    if let Some(raw) = override_val.filter(|val| !val.is_empty()) {
        let candidate = PathBuf::from(raw);
        let has_separator = candidate.is_absolute() || candidate.components().count() > 1;
        if has_separator {
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
            return Err(MediaError::MissingHelper(format!(
                "{env_var} points to '{}' which is not an executable file; install {tool} or update {env_var} (see extensionalities/DEPENDENCIES.md)",
                candidate.display()
            )));
        }
        if let Some(found) = search_path(&candidate, path_env) {
            return Ok(found);
        }
        return Err(MediaError::MissingHelper(format!(
            "{env_var} ('{}') was not found on PATH; install {tool} or set {env_var} to an executable path (see extensionalities/DEPENDENCIES.md)",
            candidate.display()
        )));
    }

    if let Some(found) = search_path(Path::new(tool), path_env) {
        return Ok(found);
    }

    Err(MediaError::MissingHelper(format!(
        "required media helper '{tool}' was not found on PATH; install FFmpeg or set {env_var} (see extensionalities/DEPENDENCIES.md)"
    )))
}

/// Searches a `PATH`-formatted directory list for an executable matching `name`.
///
/// Accepts the relative executable filename and an optional `PATH` value.
/// Returns `Some(PathBuf)` for the first executable regular file found, or
/// `None` if `PATH` is absent or no candidate is executable. This helper cannot
/// panic.
fn search_path(name: &Path, path_env: Option<&OsStr>) -> Option<PathBuf> {
    let path_env = path_env?;
    for dir in std::env::split_paths(path_env) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if candidate.extension().is_none() {
                let mut exe = std::ffi::OsString::from(candidate.as_os_str());
                exe.push(".exe");
                let exe_path = PathBuf::from(exe);
                if is_executable_file(&exe_path) {
                    return Some(exe_path);
                }
            }
        }
    }
    None
}

/// Checks whether `path` refers to an existing executable regular file.
///
/// Accepts a filesystem path (following symbolic links to the target binary).
/// Returns `true` if metadata succeeds, the target is a regular file, and Unix
/// execute permission bits are set (or any regular file on non-Unix). Returns
/// `false` on missing paths, directories, or permission lookup failures.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Validates a local regular non-symlink media file and formats an FFmpeg `file:` input argument.
///
/// Accepts a caller-supplied local filesystem path. Rejects empty paths, bare
/// `"-"` (stdin), missing files, non-local protocol strings (`http://`,
/// `concat:`, `lavfi:`, etc.), symbolic links on the target or any traversed
/// ancestor component, and non-regular files (directories, FIFOs, sockets, and
/// device nodes). Resolves relative paths against the current working directory
/// and returns an `OsString` prefixed with `"file:"` so FFmpeg and `ffprobe`
/// never interpret leading `-` characters as CLI flags or embedded `:`
/// characters as virtual protocols. Returns `MediaError::InvalidInput` when the
/// path violates the local regular-file boundary or `MediaError::Io` on
/// unexpected filesystem lookup failures.
pub fn ffmpeg_local_input_arg(path: &Path) -> Result<OsString, MediaError> {
    if path.as_os_str().is_empty() || path.as_os_str() == OsStr::new("-") {
        return Err(MediaError::InvalidInput(
            "media input must be a local regular file path".to_string(),
        ));
    }

    let supplied = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(MediaError::Io)?.join(path)
    };

    let mut current = PathBuf::new();
    let mut current_is_dir = true;
    for component in supplied.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                current.push(component.as_os_str());
                current_is_dir = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !current_is_dir {
                    return Err(MediaError::InvalidInput(format!(
                        "media input path traverses through a non-directory component: {}",
                        current.display()
                    )));
                }
                if !current.pop() {
                    return Err(MediaError::InvalidInput(
                        "media input path escapes filesystem root".to_string(),
                    ));
                }
                current_is_dir = true;
            }
            Component::Normal(part) => {
                current.push(part);
                match std::fs::symlink_metadata(&current) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(MediaError::InvalidInput(format!(
                            "media input rejects symbolic-link path component: {}",
                            current.display()
                        )));
                    }
                    Ok(meta) => {
                        current_is_dir = meta.is_dir();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return Err(MediaError::InvalidInput(format!(
                            "local media input file not found: {}",
                            path.display()
                        )));
                    }
                    Err(err) => return Err(MediaError::Io(err)),
                }
            }
        }
    }

    let final_meta = match std::fs::symlink_metadata(&current) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(MediaError::InvalidInput(format!(
                "local media input file not found: {}",
                path.display()
            )));
        }
        Err(err) => return Err(MediaError::Io(err)),
    };
    if final_meta.file_type().is_symlink() {
        return Err(MediaError::InvalidInput(format!(
            "media input rejects symbolic link: {}",
            current.display()
        )));
    }
    if !final_meta.file_type().is_file() {
        return Err(MediaError::InvalidInput(format!(
            "media input must be a regular local file: {}",
            current.display()
        )));
    }

    let mut arg = OsString::from("file:");
    arg.push(current.as_os_str());
    Ok(arg)
}

/// Drains a reader completely to EOF while retaining at most `cap` leading bytes.
///
/// Accepts any `Read` stream and a hard memory cap in bytes. Returns the retained
/// byte prefix once EOF is reached, discarding excess bytes to a sink so child
/// processes never block on a full OS pipe buffer. Propagates non-`Interrupted`
/// I/O errors.
fn read_capped_drain<R: Read>(mut reader: R, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut chunk = [0_u8; PIPE_DRAIN_CHUNK_BYTES];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(captured.len());
                if room > 0 {
                    let take = room.min(n);
                    captured.extend_from_slice(&chunk[..take]);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(captured)
}

/// RAII guard that kills and reaps a spawned child process if dropped before clean completion.
struct ChildReapGuard(Option<Child>);

impl Drop for ChildReapGuard {
    /// Kills and reaps the owned child process when the guard drops early.
    ///
    /// Accepts `&mut self`. Ignores secondary cleanup errors so timeouts,
    /// early returns, or panics never leak an orphan or zombie subprocess.
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Executes a configured `Command` with null stdin, capped concurrent pipe draining, and a hard timeout.
///
/// Accepts an owned `Command` and a maximum `Duration`. Configures null stdin
/// and piped stdout/stderr, guards the spawned child with an RAII reap guard
/// before spawning pipe-drainer threads (16 MiB stdout cap, 256 KiB stderr cap,
/// draining excess to sink to prevent pipe deadlocks), and returns the captured
/// `Output` when the process exits within `timeout`. Returns
/// `MediaError::Timeout` (killing and reaping the child and detaching drainer
/// threads) if the deadline expires, or `MediaError::Io` if spawning, waiting,
/// or reading pipes fails.
pub fn run_command_with_timeout(mut cmd: Command, timeout: Duration) -> Result<Output, MediaError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut guard = ChildReapGuard(Some(cmd.spawn().map_err(MediaError::Io)?));
    let (child_stdout, child_stderr) =
        {
            let child = guard
                .0
                .as_mut()
                .expect("child guard must hold spawned child");
            let stdout = child.stdout.take().ok_or_else(|| {
                MediaError::Io(std::io::Error::other("child stdout was not piped"))
            })?;
            let stderr = child.stderr.take().ok_or_else(|| {
                MediaError::Io(std::io::Error::other("child stderr was not piped"))
            })?;
            (stdout, stderr)
        };

    let stdout_handle =
        thread::spawn(move || read_capped_drain(child_stdout, MAX_COMMAND_STDOUT_BYTES));
    let stderr_handle =
        thread::spawn(move || read_capped_drain(child_stderr, MAX_COMMAND_STDERR_BYTES));

    let started = Instant::now();
    let status = loop {
        let child = guard
            .0
            .as_mut()
            .expect("child guard must hold spawned child until wait completes");
        match child.try_wait() {
            Ok(Some(status)) => {
                guard.0 = None;
                break status;
            }
            Ok(None) => {
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    drop(guard);
                    drop(stdout_handle);
                    drop(stderr_handle);
                    return Err(MediaError::Timeout(format!(
                        "media helper exceeded timeout of {:.1}s",
                        timeout.as_secs_f64()
                    )));
                }
                let remaining = timeout.saturating_sub(elapsed);
                thread::sleep(remaining.min(WAIT_POLL_INTERVAL));
            }
            Err(err) => {
                drop(guard);
                drop(stdout_handle);
                drop(stderr_handle);
                return Err(MediaError::Io(err));
            }
        }
    };

    let stdout = stdout_handle
        .join()
        .map_err(|_| MediaError::Io(std::io::Error::other("stdout reader thread panicked")))??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| MediaError::Io(std::io::Error::other("stderr reader thread panicked")))??;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Clears `report.media` back to `None` when every sub-receipt is absent.
///
/// Accepts a mutable [`ExtractReport`]. Prevents schema v3 JSON projections
/// from ever emitting an empty `"media": {}` object when all media stages fail
/// or produce no receipts. Performs no I/O and cannot fail.
pub(crate) fn prune_empty_media_receipt(report: &mut ExtractReport) {
    if report.media.as_ref().is_some_and(MediaReceipt::is_empty) {
        report.media = None;
    }
}

/// Appends a structured `DiagnosticStage::Media` diagnostic to `report`.
///
/// Accepts the mutable [`ExtractReport`], severity, stable error/warning code,
/// human-readable message, and optional one-based unit index. Performs no I/O
/// and cannot fail.
fn push_media_diagnostic(
    report: &mut ExtractReport,
    severity: DiagnosticSeverity,
    code: &str,
    message: String,
    unit_index: Option<u64>,
) {
    report.diagnostics.push(Diagnostic {
        stage: DiagnosticStage::Media,
        severity,
        code: code.to_owned(),
        message,
        unit_index,
    });
}

/// Attaches a container title from `ffprobe` when the report has no embedded title yet.
///
/// Accepts a mutable [`ExtractReport`] and optional trimmed container title.
/// Inserts an [`EmbeddedMetadataField::Title`] entry sourced from
/// [`FactSource::DecoderMetadata`] if absent. Performs no I/O and cannot fail.
fn attach_container_title(report: &mut ExtractReport, title: Option<String>) {
    if let Some(title) = title.filter(|value| !value.trim().is_empty())
        && !report
            .metadata
            .embedded_metadata
            .iter()
            .any(|entry| entry.field == EmbeddedMetadataField::Title)
    {
        report.metadata.embedded_metadata.push(SourcedMetadata {
            field: EmbeddedMetadataField::Title,
            value: title,
            source: FactSource::DecoderMetadata,
            completeness: Completeness::Exact,
        });
    }
}

/// Computes media duration in milliseconds from `ffprobe` metadata or decoded PCM totals.
///
/// Accepts the probed [`ffprobe::AudioProbe`], total decoded 16 kHz PCM samples,
/// total WAV bytes written (including 44-byte header), and sampled ranges.
/// Prefers positive finite `probe.duration_secs`, falling back to decoded sample
/// count, WAV payload bytes, or the highest sampled range end. Cannot fail.
fn resolve_duration_ms(
    probe: &ffprobe::AudioProbe,
    decoded_samples: usize,
    wav_total_bytes: u64,
    sampled_ranges_ms: &[[u64; 2]],
) -> u64 {
    if let Some(secs) = probe.duration_secs.filter(|s| s.is_finite() && *s > 0.0) {
        return (secs * 1_000.0).round() as u64;
    }
    if decoded_samples > 0 {
        return vad::samples_to_ms(decoded_samples);
    }
    if wav_total_bytes > WAV_HEADER_BYTES as u64 {
        let pcm_samples = (wav_total_bytes - WAV_HEADER_BYTES as u64) / 2;
        return ((u128::from(pcm_samples) * 1_000) / asr::SAMPLE_RATE_HZ as u128) as u64;
    }
    sampled_ranges_ms.last().map_or(0, |range| range[1])
}

/// Records a published extracted-audio artifact and typed [`AudioExtractReceipt`] on `report`.
///
/// Accepts the mutable [`ExtractReport`], published path, [`AudioExtractFormat`],
/// and duration in milliseconds. Performs no I/O and cannot fail.
fn record_audio_extract_artifact(
    report: &mut ExtractReport,
    published_path: PathBuf,
    format: AudioExtractFormat,
    duration_ms: u64,
) {
    report.metadata.artifacts.push(Artifact {
        kind: ArtifactKind::AudioExtract,
        path: published_path.clone(),
        source: FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
        width: None,
        height: None,
    });
    report
        .media
        .get_or_insert_with(MediaReceipt::default)
        .audio_extract = Some(AudioExtractReceipt {
        path: published_path,
        format: format.extension().to_owned(),
        duration_ms,
    });
}

/// Streams a full audio track via [`extract_audio_to_staged`] and publishes it inside `output_dir`.
///
/// Accepts the source path, probed [`ffprobe::AudioProbe`], target
/// [`AudioExtractFormat`], required private `output_dir`, and mutable
/// [`ExtractReport`]. Returns `true` after safely publishing the file and
/// attaching both `ArtifactKind::AudioExtract` and [`AudioExtractReceipt`], or
/// `false` after recording a `DiagnosticStage::Media` error (leaving zero
/// temporary files behind).
fn run_audio_extract_sidecar(
    source: &Path,
    probe: &ffprobe::AudioProbe,
    format: AudioExtractFormat,
    output_dir: Option<&Path>,
    report: &mut ExtractReport,
) -> bool {
    let Some(dir) = output_dir else {
        push_media_diagnostic(
            report,
            DiagnosticSeverity::Error,
            "audio_extract_output_failed",
            "--extract-audio requires --output-dir DIR".to_owned(),
            None,
        );
        return false;
    };

    let filename = audio_extract_filename(source, format.extension());
    let mut staged = match StagedWriter::create(dir, &filename) {
        Ok(writer) => writer,
        Err(err) => {
            let code = if err.kind() == std::io::ErrorKind::AlreadyExists {
                "audio_extract_exists"
            } else {
                "audio_extract_output_failed"
            };
            push_media_diagnostic(
                report,
                DiagnosticSeverity::Error,
                code,
                format!(
                    "create staged audio extract '{}': {err}",
                    dir.join(&filename).display()
                ),
                None,
            );
            return false;
        }
    };

    let bytes_written = match extract_audio_to_staged(source, probe, format, &mut staged) {
        Ok(bytes) => bytes,
        Err(err) => {
            drop(staged);
            push_media_diagnostic(
                report,
                DiagnosticSeverity::Error,
                media_error_code(&err),
                err.to_string(),
                None,
            );
            return false;
        }
    };

    match staged.publish() {
        Ok(published_path) => {
            let wav_bytes = if format == AudioExtractFormat::Wav {
                bytes_written
            } else {
                0
            };
            let duration_ms = resolve_duration_ms(probe, 0, wav_bytes, &[]);
            record_audio_extract_artifact(report, published_path, format, duration_ms);
            true
        }
        Err(err) => {
            let code = if err.kind() == std::io::ErrorKind::AlreadyExists {
                "audio_extract_exists"
            } else {
                "audio_extract_output_failed"
            };
            push_media_diagnostic(
                report,
                DiagnosticSeverity::Error,
                code,
                format!(
                    "publish extracted audio to '{}': {err}",
                    dir.join(&filename).display()
                ),
                None,
            );
            false
        }
    }
}

/// Orchestrates standalone `--extract-audio` and/or `--asr` (with optional audio extract) for one media file.
///
/// Accepts the local source path, mutable [`ExtractReport`] (initialized with
/// base inspection metadata), validated [`AudioPipelineConfig`], and optional
/// validated private `--output-dir`. Returns the resulting [`OperationStatus`]
/// (`Complete` exit `0`, `Partial` exit `2`, or `Failed` exit `1`) after
/// updating `report.status`, `report.completeness`, `report.selection`,
/// `report.content`, `report.media`, `report.diagnostics`, `report.provenance`,
/// and `report.metadata.artifacts`.
///
/// Validates the resolved ASR backend (OpenAI-compatible HTTP or local offline
/// Whisper ONNX) before spawning `ffprobe` or `ffmpeg`, uses single-pass WAV
/// teeing for `--asr full --extract-audio wav`, stitches overlapped
/// `full`/`short` chunks via [`stitch::stitch_chunks`], and enforces ADR-004
/// exit-code contracts across healthy sampling (`short`/`adaptive`), partial
/// window or sidecar failures, and total failures.
pub(crate) fn run_asr_and_audio_extract(
    source: &Path,
    report: &mut ExtractReport,
    config: &AudioPipelineConfig,
    output_dir: Option<&Path>,
) -> OperationStatus {
    let Some(mode) = config.asr_mode else {
        let Some(extract_fmt) = config.extract_audio else {
            return report.status;
        };
        if let Err(err) = resolve_ffmpeg().and_then(|_| resolve_ffprobe()) {
            push_media_diagnostic(
                report,
                DiagnosticSeverity::Error,
                media_error_code(&err),
                err.to_string(),
                None,
            );
            report.status = OperationStatus::Failed;
            report.completeness = Completeness::Unavailable;
            return OperationStatus::Failed;
        }
        let probe = match ffprobe::probe_audio_stream(source) {
            Ok(probe) => probe,
            Err(err) => {
                push_media_diagnostic(
                    report,
                    DiagnosticSeverity::Error,
                    media_error_code(&err),
                    err.to_string(),
                    None,
                );
                report.status = OperationStatus::Failed;
                report.completeness = Completeness::Unavailable;
                return OperationStatus::Failed;
            }
        };
        attach_container_title(report, probe.title.clone());
        if run_audio_extract_sidecar(source, &probe, extract_fmt, output_dir, report) {
            report.status = OperationStatus::Complete;
            report.completeness = Completeness::Exact;
            report.selection.omitted = OmissionStatus::None;
            report.selection.completeness = Completeness::Exact;
            return OperationStatus::Complete;
        }
        report.status = OperationStatus::Failed;
        report.completeness = Completeness::Unavailable;
        prune_empty_media_receipt(report);
        return OperationStatus::Failed;
    };

    // Build and validate ASR backend BEFORE spawning FFmpeg or ffprobe.
    let (mut backend, config_source): (Box<dyn AsrBackend>, &'static str) = match &config.asr_config
    {
        Some(ResolvedAsrConfig::OpenAi {
            base_url,
            model,
            api_key_env,
            protocol,
            timeout_secs,
            source,
        }) => {
            let mut openai_cfg = OpenAiAsrConfig::new(base_url.clone(), model.clone());
            openai_cfg.api_key_env = api_key_env.clone();
            openai_cfg.protocol = *protocol;
            openai_cfg.timeout_secs = *timeout_secs;
            match OpenAiAsrBackend::new(openai_cfg) {
                Ok(b) => (Box::new(b), source.as_str()),
                Err(err) => {
                    push_media_diagnostic(
                        report,
                        DiagnosticSeverity::Error,
                        asr_error_code(&err),
                        err.to_string(),
                        None,
                    );
                    report.status = OperationStatus::Failed;
                    report.completeness = Completeness::Unavailable;
                    return OperationStatus::Failed;
                }
            }
        }
        Some(ResolvedAsrConfig::WhisperOnnx {
            model_dir,
            device,
            source,
        }) => {
            let whisper_cfg = WhisperOnnxConfig::new(model_dir.clone(), *device);
            (
                Box::new(WhisperOnnxAsrBackend::new(whisper_cfg)),
                source.as_str(),
            )
        }
        None => {
            push_media_diagnostic(
                report,
                DiagnosticSeverity::Error,
                "asr_config_failed",
                "missing resolved ASR backend configuration".to_owned(),
                None,
            );
            report.status = OperationStatus::Failed;
            report.completeness = Completeness::Unavailable;
            return OperationStatus::Failed;
        }
    };

    if let Err(err) = backend.validate() {
        push_media_diagnostic(
            report,
            DiagnosticSeverity::Error,
            asr_error_code(&err),
            format!(
                "ASR backend '{}' ({}, {config_source}) validation failed: {err}",
                backend.backend_name(),
                backend.provider()
            ),
            None,
        );
        report.status = OperationStatus::Failed;
        report.completeness = Completeness::Unavailable;
        prune_empty_media_receipt(report);
        return OperationStatus::Failed;
    }

    if let Err(err) = resolve_ffmpeg().and_then(|_| resolve_ffprobe()) {
        push_media_diagnostic(
            report,
            DiagnosticSeverity::Error,
            media_error_code(&err),
            err.to_string(),
            None,
        );
        report.status = OperationStatus::Failed;
        report.completeness = Completeness::Unavailable;
        prune_empty_media_receipt(report);
        return OperationStatus::Failed;
    }

    let probe = match ffprobe::probe_audio_stream(source) {
        Ok(probe) => probe,
        Err(err) => {
            push_media_diagnostic(
                report,
                DiagnosticSeverity::Error,
                media_error_code(&err),
                err.to_string(),
                None,
            );
            report.status = OperationStatus::Failed;
            report.completeness = Completeness::Unavailable;
            return OperationStatus::Failed;
        }
    };
    attach_container_title(report, probe.title.clone());

    let use_wav_tee =
        mode == AsrMode::Full && config.extract_audio == Some(AudioExtractFormat::Wav);
    let mut audio_extract_succeeded = false;
    let mut audio_extract_failed = false;

    let mut wav_tee_writer: Option<StagedWriter> = if use_wav_tee {
        match output_dir {
            Some(dir) => {
                let filename = audio_extract_filename(source, "wav");
                match StagedWriter::create(dir, &filename) {
                    Ok(writer) => Some(writer),
                    Err(err) => {
                        let code = if err.kind() == std::io::ErrorKind::AlreadyExists {
                            "audio_extract_exists"
                        } else {
                            "audio_extract_output_failed"
                        };
                        push_media_diagnostic(
                            report,
                            DiagnosticSeverity::Error,
                            code,
                            format!(
                                "create staged audio extract '{}': {err}",
                                dir.join(&filename).display()
                            ),
                            None,
                        );
                        audio_extract_failed = true;
                        None
                    }
                }
            }
            None => {
                push_media_diagnostic(
                    report,
                    DiagnosticSeverity::Error,
                    "audio_extract_output_failed",
                    "--extract-audio requires --output-dir DIR".to_owned(),
                    None,
                );
                audio_extract_failed = true;
                None
            }
        }
    } else {
        None
    };

    let mut windows_attempted: u64 = 0;
    let mut windows_succeeded: u64 = 0;
    let mut windows_failed: u64 = 0;
    let mut stream_error = false;
    let mut sampled_ranges_ms: Vec<[u64; 2]> = Vec::new();
    let mut raw_chunks: Vec<TranscriptChunk> = Vec::new();

    let stream_summary = {
        let mut stream =
            match AudioWindowStream::open(source, &probe, mode, wav_tee_writer.as_mut()) {
                Ok(stream) => stream,
                Err(err) => {
                    drop(wav_tee_writer);
                    push_media_diagnostic(
                        report,
                        DiagnosticSeverity::Error,
                        media_error_code(&err),
                        err.to_string(),
                        None,
                    );
                    report.status = OperationStatus::Failed;
                    report.completeness = Completeness::Unavailable;
                    return OperationStatus::Failed;
                }
            };

        for window_res in &mut stream {
            match window_res {
                Ok(window) => {
                    windows_attempted += 1;
                    match backend.transcribe_window(&window, config.asr_language.as_deref()) {
                        Ok(chunk) => {
                            windows_succeeded += 1;
                            sampled_ranges_ms.push([chunk.start_ms, chunk.end_ms]);
                            raw_chunks.push(chunk);
                        }
                        Err(err) => {
                            windows_failed += 1;
                            let stop_immediately = matches!(
                                err,
                                AsrError::Config(_)
                                    | AsrError::MissingAsset(_)
                                    | AsrError::Http {
                                        status: Some(401 | 403),
                                        ..
                                    }
                            ) || (windows_succeeded == 0
                                && matches!(
                                    err,
                                    AsrError::Runtime(_)
                                        | AsrError::Http {
                                            status: Some(400..=499) | None,
                                            ..
                                        }
                                ));
                            push_media_diagnostic(
                                report,
                                DiagnosticSeverity::Error,
                                asr_error_code(&err),
                                err.to_string(),
                                Some(window.index as u64 + 1),
                            );
                            if stop_immediately {
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    stream_error = true;
                    push_media_diagnostic(
                        report,
                        DiagnosticSeverity::Error,
                        media_error_code(&err),
                        err.to_string(),
                        None,
                    );
                    break;
                }
            }
        }

        stream
            .finish()
            .unwrap_or_else(|_| audio_pipe::AudioStreamSummary {
                total_decoded_samples: 0,
                warnings: Vec::new(),
                vad_no_speech_scanned_ms: None,
                wav_tee_bytes_written: 0,
                wav_tee_error: None,
            })
    };

    for warning in &stream_summary.warnings {
        let code = if stream_summary.vad_no_speech_scanned_ms.is_some() {
            "asr_no_speech_detected"
        } else {
            "asr_window_skipped"
        };
        push_media_diagnostic(
            report,
            DiagnosticSeverity::Warning,
            code,
            warning.clone(),
            None,
        );
    }

    if let Some(staged) = wav_tee_writer {
        if let Some(tee_err) = &stream_summary.wav_tee_error {
            drop(staged);
            push_media_diagnostic(
                report,
                DiagnosticSeverity::Error,
                "audio_extract_output_failed",
                format!("finalize extracted WAV audio: {tee_err}"),
                None,
            );
            audio_extract_failed = true;
        } else if !stream_error && stream_summary.wav_tee_bytes_written > WAV_HEADER_BYTES as u64 {
            match staged.publish() {
                Ok(published_path) => {
                    let duration_ms = resolve_duration_ms(
                        &probe,
                        stream_summary.total_decoded_samples,
                        stream_summary.wav_tee_bytes_written,
                        &sampled_ranges_ms,
                    );
                    record_audio_extract_artifact(
                        report,
                        published_path,
                        AudioExtractFormat::Wav,
                        duration_ms,
                    );
                    audio_extract_succeeded = true;
                }
                Err(err) => {
                    let code = if err.kind() == std::io::ErrorKind::AlreadyExists {
                        "audio_extract_exists"
                    } else {
                        "audio_extract_output_failed"
                    };
                    let dest_hint = output_dir
                        .map(|dir| dir.join(audio_extract_filename(source, "wav")))
                        .unwrap_or_else(|| PathBuf::from(audio_extract_filename(source, "wav")));
                    push_media_diagnostic(
                        report,
                        DiagnosticSeverity::Error,
                        code,
                        format!(
                            "publish extracted audio to '{}': {err}",
                            dest_hint.display()
                        ),
                        None,
                    );
                    audio_extract_failed = true;
                }
            }
        } else {
            drop(staged);
            audio_extract_failed = true;
        }
    } else if let Some(extract_fmt) = config.extract_audio.filter(|_| !use_wav_tee) {
        if run_audio_extract_sidecar(source, &probe, extract_fmt, output_dir, report) {
            audio_extract_succeeded = true;
        } else {
            audio_extract_failed = true;
        }
    }

    let no_speech_detected = mode == AsrMode::Short
        && stream_summary.vad_no_speech_scanned_ms.is_some()
        && !stream_error
        && windows_failed == 0;
    let asr_had_any_success = windows_succeeded > 0 || no_speech_detected;
    let asr_had_any_failure = windows_failed > 0 || stream_error || !asr_had_any_success;

    if !asr_had_any_success && !audio_extract_succeeded {
        if !report
            .diagnostics
            .iter()
            .any(|diag| diag.severity == DiagnosticSeverity::Error)
        {
            push_media_diagnostic(
                report,
                DiagnosticSeverity::Error,
                "asr_no_audio_windows",
                "audio stream produced zero transcribable windows".to_owned(),
                None,
            );
        }
        report.status = OperationStatus::Failed;
        report.completeness = Completeness::Unavailable;
        report.selection = SelectionReport {
            requested: NativeSelection::All,
            unit: NativeUnit::TimeInterval,
            available: QualifiedCount {
                unit: NativeUnit::TimeInterval,
                value: Some(windows_attempted),
                source: FactSource::DecoderMetadata,
                completeness: Completeness::Unavailable,
            },
            returned_indices: Vec::new(),
            omitted: OmissionStatus::PartialFailure,
            completeness: Completeness::Unavailable,
        };
        prune_empty_media_receipt(report);
        return OperationStatus::Failed;
    }

    let stitched =
        stitch::stitch_chunks(&raw_chunks, matches!(mode, AsrMode::Full | AsrMode::Short));
    let unit_provenance = ExtractionProvenance {
        reader: format!("asr-{}", backend.backend_name()),
        stage: format!("asr:{config_source}"),
        source: FactSource::GeneratedArtifact,
        origin: ContentOrigin::DerivedPreview,
        ocr: None,
    };

    for (idx, chunk) in stitched.into_iter().enumerate() {
        report.content.units.push(ContentUnit {
            unit_index: idx as u64 + 1,
            unit: NativeUnit::TimeInterval,
            source_part: None,
            label: None,
            marker_index: None,
            start_ms: Some(chunk.start_ms),
            end_ms: Some(chunk.end_ms),
            text: Some(chunk.text),
            rows: Vec::new(),
            provenance: vec![unit_provenance.clone()],
            completeness: Completeness::Exact,
        });
    }

    if asr_had_any_success {
        report.provenance.push(unit_provenance);
        let duration_ms = resolve_duration_ms(
            &probe,
            stream_summary.total_decoded_samples,
            stream_summary.wav_tee_bytes_written,
            &sampled_ranges_ms,
        );
        report.media.get_or_insert_with(MediaReceipt::default).asr = Some(AsrReceipt {
            mode: mode.as_str().to_owned(),
            backend: backend.backend_name().to_owned(),
            model: backend.model_id().to_owned(),
            provider: backend.provider().to_owned(),
            protocol: backend.protocol().map(str::to_owned),
            language: config.asr_language.clone(),
            duration_ms,
            sampled_ranges_ms,
            windows_attempted,
            windows_succeeded,
        });
    }

    let (status, omitted, completeness) = if asr_had_any_failure || audio_extract_failed {
        (
            OperationStatus::Partial,
            OmissionStatus::PartialFailure,
            Completeness::Partial,
        )
    } else if matches!(mode, AsrMode::Short | AsrMode::Adaptive) {
        (
            OperationStatus::Complete,
            OmissionStatus::Selection,
            Completeness::Partial,
        )
    } else {
        (
            OperationStatus::Complete,
            OmissionStatus::None,
            Completeness::Exact,
        )
    };

    let returned_indices: Vec<u64> = report
        .content
        .units
        .iter()
        .map(|unit| unit.unit_index)
        .collect();

    report.status = status;
    report.completeness = completeness;
    report.selection = SelectionReport {
        requested: NativeSelection::All,
        unit: NativeUnit::TimeInterval,
        available: QualifiedCount {
            unit: NativeUnit::TimeInterval,
            value: Some(windows_attempted),
            source: FactSource::DecoderMetadata,
            completeness,
        },
        returned_indices,
        omitted,
        completeness,
    };

    prune_empty_media_receipt(report);

    status
}

/// Formats an `OsString` path list for platform-independent test assertions.
#[cfg(test)]
fn join_test_paths(paths: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(paths).expect("join test PATH")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Creates an isolated workspace-local scratch directory under `extensionalities/target/tests/media-mod`.
    ///
    /// Accepts a descriptive test label. Returns the newly created directory
    /// `PathBuf` inside the gitignored downstream target tree (never `/tmp`).
    /// Panics if directory creation fails.
    fn unique_temp_dir(label: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tests/media-mod")
            .join(format!("{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[cfg(unix)]
    fn create_fake_executable(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write fake executable");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("chmod fake executable");
        path
    }

    #[test]
    fn missing_helper_mentions_dependencies_doc() {
        let err = resolve_helper_with("ffmpeg", FFMPEG_ENV_VAR, None, Some(OsStr::new("")))
            .expect_err("empty PATH should fail");
        assert!(matches!(err, MediaError::MissingHelper(_)));
        let msg = err.to_string();
        assert!(msg.contains("ffmpeg"), "got: {msg}");
        assert!(
            msg.contains("extensionalities/DEPENDENCIES.md"),
            "got: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_override_takes_precedence_and_rejects_broken_override() {
        let dir = unique_temp_dir("override");
        let custom = create_fake_executable(&dir, "custom-ffmpeg");
        let path_dir = unique_temp_dir("path-bin");
        let _path_ffmpeg = create_fake_executable(&path_dir, "ffmpeg");
        let path_env = join_test_paths(&[path_dir.as_path()]);

        let resolved = resolve_helper_with(
            "ffmpeg",
            FFMPEG_ENV_VAR,
            Some(custom.as_os_str()),
            Some(path_env.as_os_str()),
        )
        .expect("override should resolve");
        assert_eq!(resolved, custom);

        let missing = dir.join("does-not-exist");
        let err = resolve_helper_with(
            "ffmpeg",
            FFMPEG_ENV_VAR,
            Some(missing.as_os_str()),
            Some(path_env.as_os_str()),
        )
        .expect_err("broken override must not fall back to PATH");
        assert!(matches!(err, MediaError::MissingHelper(_)));
        assert!(err.to_string().contains("extensionalities/DEPENDENCIES.md"));

        let fallback =
            resolve_helper_with("ffmpeg", FFMPEG_ENV_VAR, None, Some(path_env.as_os_str()))
                .expect("PATH lookup should succeed");
        assert_eq!(fallback, path_dir.join("ffmpeg"));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&path_dir);
    }

    #[cfg(unix)]
    #[test]
    fn run_command_with_timeout_captures_large_stdout_and_enforces_deadline() {
        // Produce >128 KiB on stdout (larger than Linux default 64 KiB pipe buffer).
        let mut big = Command::new("sh");
        big.args(["-c", "dd if=/dev/zero bs=1024 count=256 2>/dev/null"]);
        let out = run_command_with_timeout(big, Duration::from_secs(5))
            .expect("large stdout should drain without deadlock");
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 256 * 1024);

        // Produce >512 KiB on stderr (larger than 256 KiB cap) to confirm excess drains cleanly.
        let mut noisy_stderr = Command::new("sh");
        noisy_stderr.args(["-c", "dd if=/dev/zero bs=1024 count=512 1>&2 2>/dev/null"]);
        let capped = run_command_with_timeout(noisy_stderr, Duration::from_secs(5))
            .expect("oversized stderr must drain to sink without deadlock");
        assert!(capped.status.success());
        assert_eq!(capped.stderr.len(), MAX_COMMAND_STDERR_BYTES);

        let mut slow = Command::new("sh");
        slow.args(["-c", "exec sleep 5"]);
        let started = Instant::now();
        let err = run_command_with_timeout(slow, Duration::from_millis(80))
            .expect_err("sleeping command should time out");
        assert!(matches!(err, MediaError::Timeout(_)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout must return promptly without waiting for child exit"
        );
    }

    #[test]
    fn read_capped_drain_caps_retention_while_consuming_entire_stream() {
        let payload = vec![b'x'; 100_000];
        let mut cursor = std::io::Cursor::new(payload);
        let retained = read_capped_drain(&mut cursor, 1_024).expect("capped drain succeeds");
        assert_eq!(retained.len(), 1_024);
        assert_eq!(cursor.position(), 100_000);
    }

    #[test]
    fn ffmpeg_local_input_arg_rejects_protocols_dash_dirs_and_symlinks() {
        let dir = unique_temp_dir("input-boundary");
        let real_file = dir.join("clip.mp4");
        fs::write(&real_file, b"placeholder").expect("write real file");

        // Regular local files with tricky names (`-clip.mp4`, `a:b.mp4`) are accepted with `file:`.
        let dash_file = dir.join("-clip.mp4");
        fs::write(&dash_file, b"placeholder").expect("write dash file");
        let dash_arg = ffmpeg_local_input_arg(&dash_file).expect("dash filename must succeed");
        let dash_text = dash_arg.to_string_lossy();
        assert!(dash_text.starts_with("file:"), "got: {dash_text}");
        assert!(dash_text.ends_with("/-clip.mp4"), "got: {dash_text}");

        let colon_file = dir.join("a:b.mp4");
        fs::write(&colon_file, b"placeholder").expect("write colon file");
        let colon_arg = ffmpeg_local_input_arg(&colon_file).expect("colon filename must succeed");
        let colon_text = colon_arg.to_string_lossy();
        assert!(colon_text.starts_with("file:"), "got: {colon_text}");
        assert!(colon_text.ends_with("/a:b.mp4"), "got: {colon_text}");

        // Reject empty path, bare `-`, directories, and non-local protocol strings.
        for bad in [
            Path::new(""),
            Path::new("-"),
            dir.as_path(),
            Path::new("http://127.0.0.1:46069/test.mp4"),
            Path::new("https://example.invalid/clip.mp4"),
            Path::new("concat:a.mp4|b.mp4"),
            Path::new("lavfi:sine"),
        ] {
            let err = ffmpeg_local_input_arg(bad)
                .expect_err(&format!("expected rejection for {}", bad.display()));
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "expected InvalidInput for {}, got: {err:?}",
                bad.display()
            );
        }

        // Reject direct symlinks and symlinked ancestor directories (including via `..`).
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let file_link = dir.join("link.mp4");
            symlink(&real_file, &file_link).expect("create file symlink");
            let err = ffmpeg_local_input_arg(&file_link).expect_err("file symlink must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)));

            let sub_dir = dir.join("sub");
            fs::create_dir_all(&sub_dir).expect("create subdir");
            let nested_file = sub_dir.join("inner.mp4");
            fs::write(&nested_file, b"placeholder").expect("write inner file");
            let dir_link = dir.join("linkdir");
            symlink(&sub_dir, &dir_link).expect("create dir symlink");

            let err = ffmpeg_local_input_arg(&dir_link.join("inner.mp4"))
                .expect_err("symlinked ancestor dir must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)));

            let err = ffmpeg_local_input_arg(&dir_link.join("..").join("clip.mp4"))
                .expect_err("symlinked ancestor before .. must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_ffprobe_and_ffmpeg_accept_dash_and_colon_filenames_and_reject_protocols() {
        let (Ok(ffmpeg), Ok(_ffprobe)) = (resolve_ffmpeg(), resolve_ffprobe()) else {
            return;
        };

        let dir = unique_temp_dir("live-tricky-names");
        let dash_clip = dir.join("-clip.mp4");
        let colon_clip = dir.join("a:b.mp4");

        // Synthesize a tiny 1-second video+audio clip directly onto `-clip.mp4`.
        let mut gen_cmd = Command::new(&ffmpeg);
        gen_cmd
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x64:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(&dash_clip);
        let gen_out = run_command_with_timeout(gen_cmd, Duration::from_secs(15))
            .expect("ffmpeg fixture generation should complete");
        assert!(
            gen_out.status.success(),
            "fixture generation failed: {}",
            String::from_utf8_lossy(&gen_out.stderr)
        );
        fs::copy(&dash_clip, &colon_clip).expect("copy fixture to a:b.mp4");

        for clip in [&dash_clip, &colon_clip] {
            let vprobe = ffprobe::probe_video_stream(clip)
                .expect("ffprobe video probe must succeed on tricky filename");
            assert!(vprobe.duration_secs > 0.5);

            let aprobe = ffprobe::probe_audio_stream(clip)
                .expect("ffprobe audio probe must succeed on tricky filename");
            assert!(aprobe.channels >= 1);
            assert!(aprobe.sample_rate > 0);

            let sheet = contact_sheet::generate_contact_sheet(
                clip,
                contact_sheet::ImageGridMode::Adaptive,
                contact_sheet::ImageGridPreset::Performance,
                contact_sheet::ImageGridFormat::Jpg,
                1,
            )
            .expect("contact sheet must succeed on tricky filename");
            assert_eq!(sheet.tiles, 5);
            assert!(sheet.bytes.len() > 100);
        }

        // Protocol strings must fail before spawning ffprobe/ffmpeg.
        for bad in [
            Path::new("http://127.0.0.1:46069/test.mp4"),
            Path::new("concat:a.mp4|b.mp4"),
        ] {
            let verr = ffprobe::probe_video_stream(bad).expect_err("protocol must be rejected");
            assert!(matches!(verr, MediaError::InvalidInput(_)));
            let aerr = ffprobe::probe_audio_stream(bad).expect_err("protocol must be rejected");
            assert!(matches!(aerr, MediaError::InvalidInput(_)));
        }

        let _ = fs::remove_dir_all(&dir);
    }
}

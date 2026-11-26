//! Module: glassbox-cli piped FFmpeg audio windowing and streaming audio extraction.
//! File: media/audio_pipe.rs.
//! Responsibility: Stream mono 16 kHz PCM from FFmpeg with zero temporary files across `full`, `adaptive`, and `short` ASR modes, optional single-pass WAV teeing, RAII subprocess reaping, and O(1)-RAM `--extract-audio` streaming into `StagedWriter`.

use super::asr::{
    AsrMode, AudioExtractFormat, AudioWindow, OVERLAP_SECONDS, SAMPLE_RATE_HZ, WINDOW_SECONDS,
};
use super::ffprobe::AudioProbe;
use super::vad::{self, VadOutcome, samples_to_ms};
use super::{MediaError, ffmpeg_local_input_arg, resolve_ffmpeg, run_command_with_timeout};
use crate::output::StagedWriter;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Number of 16 kHz PCM samples in one full 30-second ASR window (`480_000`).
pub const WINDOW_SAMPLES: usize = WINDOW_SECONDS * SAMPLE_RATE_HZ;

/// Number of 16 kHz PCM samples overlapped between adjacent sequential windows (`64_000`).
pub const OVERLAP_SAMPLES: usize = OVERLAP_SECONDS * SAMPLE_RATE_HZ;

/// Stride in samples between adjacent overlapped 30 s windows (`416_000` = 26 s).
const STEP_SAMPLES: usize = WINDOW_SAMPLES - OVERLAP_SAMPLES;

/// Byte size of one 30-second `f32le` PCM window (`480_000 * 4`).
const WINDOW_BYTES: usize = WINDOW_SAMPLES * 4;

/// Byte size of one 4-second `f32le` overlap tail (`64_000 * 4`).
const OVERLAP_BYTES: usize = OVERLAP_SAMPLES * 4;

/// Standard RIFF/WAV header length in bytes for uncompressed 16-bit PCM.
pub const WAV_HEADER_BYTES: usize = 44;

/// Maximum PCM payload bytes representable in a 32-bit RIFF/WAV container (`u32::MAX - 36`).
pub const MAX_WAV_DATA_BYTES: u64 = (u32::MAX as u64) - 36;

/// Interval in seconds between independent adaptive ASR sample marks (15 minutes).
pub const ADAPTIVE_INTERVAL_SECS: u64 = 900;

/// Maximum adaptive ASR sample marks permitted for a single media file (62.5 hours).
pub const MAX_ADAPTIVE_MARKS: usize = 250;

/// Duration in seconds decoded for each adaptive ASR sample mark.
pub const ADAPTIVE_WINDOW_SECS: u64 = 30;

/// Hard wall-clock timeout applied to each input-seeked adaptive FFmpeg window.
const ADAPTIVE_WINDOW_TIMEOUT: Duration = Duration::from_secs(30);

/// Default idle-read timeout in seconds for streaming FFmpeg stdout pipes.
pub const DEFAULT_PIPE_IDLE_TIMEOUT_SECS: u64 = 30;

/// Environment variable overriding [`DEFAULT_PIPE_IDLE_TIMEOUT_SECS`] (clamped to `1..=600`).
pub const PIPE_IO_TIMEOUT_ENV_VAR: &str = "GLASSBOX_FFMPEG_IO_TIMEOUT_SECS";

/// Minimum allowed FFmpeg pipe idle-read timeout override in seconds.
const MIN_PIPE_IDLE_TIMEOUT_SECS: u64 = 1;

/// Maximum allowed FFmpeg pipe idle-read timeout override in seconds.
const MAX_PIPE_IDLE_TIMEOUT_SECS: u64 = 600;

/// Maximum post-EOF wait timeout in seconds for a piped FFmpeg child to exit cleanly.
const POST_EOF_WAIT_TIMEOUT_SECS: u64 = 10;

/// Bounded channel depth for background FFmpeg stdout chunks (`4 * 64 KiB = 256 KiB`).
const STDOUT_CHANNEL_CAPACITY: usize = 4;

/// Poll interval while waiting for a piped FFmpeg child to exit after stdout EOF.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Maximum stderr bytes retained from an FFmpeg child while still draining to EOF.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Streaming chunk size in bytes used for O(1)-RAM FFmpeg stdout reads and writes.
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// Summary facts collected across an [`AudioWindowStream`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioStreamSummary {
    /// Total 16 kHz PCM samples decoded across all yielded windows (or scanned in `short` mode).
    pub total_decoded_samples: usize,
    /// Non-fatal operational warnings (such as skipped empty adaptive marks or no-speech VAD scans).
    pub warnings: Vec<String>,
    /// Populated when `--asr short` scanned up to EOF or 600 s without detecting speech.
    pub vad_no_speech_scanned_ms: Option<u64>,
    /// Total bytes written to the single-pass WAV tee (`44 + pcm_bytes`), or `0` when unused.
    pub wav_tee_bytes_written: u64,
    /// Non-fatal WAV tee finalization error captured at EOF after preserving the final ASR window.
    pub wav_tee_error: Option<String>,
}

/// Builds a standard 44-byte RIFF/WAV header for 16 kHz mono signed 16-bit PCM.
///
/// Accepts `data_bytes`, the exact PCM payload byte count following the header.
/// Returns the encoded 44-byte header array. Returns `MediaError::EncodeFailed`
/// if `data_bytes` exceeds [`MAX_WAV_DATA_BYTES`] (`u32::MAX - 36`).
pub fn build_wav_header(data_bytes: u64) -> Result<[u8; WAV_HEADER_BYTES], MediaError> {
    if data_bytes > MAX_WAV_DATA_BYTES {
        return Err(MediaError::EncodeFailed(
            "WAV audio payload exceeds the 4 GiB RIFF size limit".to_string(),
        ));
    }

    let data_size = data_bytes as u32;
    let riff_size = 36_u32 + data_size;
    let sample_rate = SAMPLE_RATE_HZ as u32;
    let channels = 1_u16;
    let bits_per_sample = 16_u16;
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = sample_rate * u32::from(block_align);

    let mut header = [0_u8; WAV_HEADER_BYTES];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&riff_size.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&1_u16.to_le_bytes());
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_size.to_le_bytes());
    Ok(header)
}

/// Seeks to offset 0 on `writer`, writes the finalized 44-byte RIFF/WAV header, and flushes.
///
/// Accepts a mutable seekable writer holding a 44-byte placeholder followed by
/// `data_bytes` of 16 kHz mono `s16le` PCM. Returns the total WAV file length
/// (`44 + data_bytes`). Returns `MediaError::EncodeFailed` if `data_bytes`
/// overflows RIFF limits or `MediaError::Io` on seek, write, or flush failure.
pub fn patch_wav_header<W: Write + Seek>(
    writer: &mut W,
    data_bytes: u64,
) -> Result<u64, MediaError> {
    let header = build_wav_header(data_bytes)?;
    writer.seek(SeekFrom::Start(0)).map_err(MediaError::Io)?;
    writer.write_all(&header).map_err(MediaError::Io)?;
    writer.seek(SeekFrom::End(0)).map_err(MediaError::Io)?;
    writer.flush().map_err(MediaError::Io)?;
    Ok(WAV_HEADER_BYTES as u64 + data_bytes)
}

/// Converts a normalized floating-point PCM sample in `[-1.0, 1.0]` to signed 16-bit PCM.
///
/// Accepts one finite `f32` audio sample. Clamps out-of-range values to
/// `[-1.0, 1.0]` and scales symmetrically to `i16`. Performs no I/O and cannot fail.
pub fn f32_sample_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// Computes the deterministic adaptive-mode sample marks in seconds (`0, 900, 1800, ...`).
///
/// Accepts an optional probed media duration in seconds. Returns `Ok(vec![0])`
/// when duration is unknown (`None`) or non-positive (`<= 0.0`); otherwise
/// returns all multiples of `ADAPTIVE_INTERVAL_SECS` strictly less than
/// `duration_secs`, bounded by [`MAX_ADAPTIVE_MARKS`] (`250`, i.e. 62.5 hours).
/// Returns `MediaError::InvalidInput` without allocating when `duration_secs`
/// is non-finite (`NaN`, `INFINITY`, `NEG_INFINITY`) or requires more than
/// `MAX_ADAPTIVE_MARKS` marks.
pub fn adaptive_marks_secs(duration_secs: Option<f64>) -> Result<Vec<u64>, MediaError> {
    let Some(duration) = duration_secs else {
        return Ok(vec![0]);
    };
    if !duration.is_finite() {
        return Err(MediaError::InvalidInput(
            "media duration must be finite for adaptive ASR scheduling".to_string(),
        ));
    }
    if duration <= 0.0 {
        return Ok(vec![0]);
    }
    let max_duration = (MAX_ADAPTIVE_MARKS as f64) * (ADAPTIVE_INTERVAL_SECS as f64);
    if duration > max_duration {
        return Err(MediaError::InvalidInput(format!(
            "media duration ({duration:.1}s) exceeds maximum of {MAX_ADAPTIVE_MARKS} adaptive ASR marks"
        )));
    }
    let estimated =
        ((duration / ADAPTIVE_INTERVAL_SECS as f64).ceil() as usize).clamp(1, MAX_ADAPTIVE_MARKS);
    let mut marks = Vec::with_capacity(estimated);
    for idx in 0..estimated {
        let mark = (idx as u64) * ADAPTIVE_INTERVAL_SECS;
        if (mark as f64) < duration {
            marks.push(mark);
        }
    }
    if marks.is_empty() {
        marks.push(0);
    }
    Ok(marks)
}

/// Slices captured `--asr short` PCM into at most two overlapped 30-second windows.
///
/// Accepts the zero-based `start_sample` offset (including VAD pre-roll) and
/// the captured mono 16 kHz `f32` PCM buffer. Returns an empty vector if `pcm`
/// is empty, 1 window when `pcm.len() <= WINDOW_SAMPLES`, or 2 windows with
/// exact `OVERLAP_SAMPLES` (4 s) overlap and each window capped at
/// `WINDOW_SAMPLES` (30 s). Guarantees `end_ms > start_ms` for every returned
/// window. Performs no I/O and cannot fail.
pub fn slice_short_windows(start_sample: usize, pcm: Vec<f32>) -> Vec<AudioWindow> {
    if pcm.is_empty() {
        return Vec::new();
    }

    if pcm.len() <= WINDOW_SAMPLES {
        let start_ms = samples_to_ms(start_sample);
        let end_ms = samples_to_ms(start_sample + pcm.len()).max(start_ms + 1);
        return vec![AudioWindow {
            index: 0,
            start_ms,
            end_ms,
            pcm_16k_f32: pcm,
        }];
    }

    let first_pcm = pcm[..WINDOW_SAMPLES].to_vec();
    let first_start_ms = samples_to_ms(start_sample);
    let first_end_ms = samples_to_ms(start_sample + WINDOW_SAMPLES).max(first_start_ms + 1);

    let second_offset = STEP_SAMPLES;
    let second_end = pcm.len().min(second_offset + WINDOW_SAMPLES);
    let second_pcm = pcm[second_offset..second_end].to_vec();
    let second_start_sample = start_sample + second_offset;
    let second_start_ms = samples_to_ms(second_start_sample);
    let second_end_ms =
        samples_to_ms(second_start_sample + second_pcm.len()).max(second_start_ms + 1);

    vec![
        AudioWindow {
            index: 0,
            start_ms: first_start_ms,
            end_ms: first_end_ms,
            pcm_16k_f32: first_pcm,
        },
        AudioWindow {
            index: 1,
            start_ms: second_start_ms,
            end_ms: second_end_ms,
            pcm_16k_f32: second_pcm,
        },
    ]
}

/// Spawns a background thread that drains `stderr` completely while retaining at most 64 KiB.
///
/// Accepts an owned `ChildStderr` pipe handle. Returns a `JoinHandle<Vec<u8>>`
/// yielding the retained prefix of stderr bytes once the child closes stderr.
/// Draining excess bytes past `MAX_STDERR_BYTES` guarantees FFmpeg can never
/// deadlock on a full stderr pipe buffer.
fn spawn_stderr_drainer(mut stderr: ChildStderr) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let room = MAX_STDERR_BYTES.saturating_sub(captured.len());
                    if room > 0 {
                        let take = room.min(n);
                        captured.extend_from_slice(&chunk[..take]);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        captured
    })
}

/// Formats a concise diagnostic message when FFmpeg exits with a non-zero status.
///
/// Accepts an operation label, optional exit status code, and captured stderr
/// bytes. Returns a sanitized error string. Performs no I/O and cannot fail.
fn format_ffmpeg_failure(context: &str, exit_code: Option<i32>, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    if !detail.is_empty() {
        format!("ffmpeg {context} failed: {detail}")
    } else if let Some(code) = exit_code {
        format!("ffmpeg {context} exited with status {code}")
    } else {
        format!("ffmpeg {context} terminated by signal")
    }
}

/// Parses an optional FFmpeg pipe idle-read timeout override in seconds.
///
/// Accepts an optional string from [`PIPE_IO_TIMEOUT_ENV_VAR`]. Parses whole
/// seconds and clamps valid values to `1..=600`; falls back to
/// [`DEFAULT_PIPE_IDLE_TIMEOUT_SECS`] when absent or unparseable. Cannot fail.
fn parse_pipe_idle_timeout(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|value| value.clamp(MIN_PIPE_IDLE_TIMEOUT_SECS, MAX_PIPE_IDLE_TIMEOUT_SECS))
        .unwrap_or(DEFAULT_PIPE_IDLE_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Resolves the configured FFmpeg pipe idle-read timeout from the environment.
///
/// Accepts no arguments. Reads [`PIPE_IO_TIMEOUT_ENV_VAR`] and delegates to
/// [`parse_pipe_idle_timeout`]. Performs no filesystem I/O and cannot fail.
fn resolve_pipe_idle_timeout() -> Duration {
    let raw = std::env::var(PIPE_IO_TIMEOUT_ENV_VAR).ok();
    parse_pipe_idle_timeout(raw.as_deref())
}

/// Converts a timed-out [`MediaError::Io`] returned by a [`Read`] consumer into [`MediaError::Timeout`].
///
/// Accepts a [`MediaError`]. Returns `MediaError::Timeout` when `err` wraps an
/// `std::io::ErrorKind::TimedOut`, or returns `err` unchanged otherwise. Cannot fail.
fn map_pipe_timeout_error(err: MediaError) -> MediaError {
    match err {
        MediaError::Io(io_err) if io_err.kind() == std::io::ErrorKind::TimedOut => {
            MediaError::Timeout(io_err.to_string())
        }
        other => other,
    }
}

/// RAII owner of a piped FFmpeg subprocess with timeout-bounded stdout reads, drained stderr, and non-blocking reaping.
///
/// Reads `stdout` on a dedicated background thread into a bounded channel so
/// callers never block indefinitely on a stalled FFmpeg pipe. Enforces both an
/// idle-read deadline during streaming and a post-EOF exit wait deadline.
/// On timeout, error, or early drop, kills and reaps the child and detaches
/// drainer threads without `.join()` so grandchildren inheriting pipe FDs can
/// never hang cleanup.
struct FfmpegPipeChild {
    /// Bounded channel carrying 64 KiB stdout chunks from the background reader thread.
    stdout_rx: Option<Receiver<std::io::Result<Vec<u8>>>>,
    /// Remainder bytes from the most recently received stdout chunk.
    pending_chunk: Vec<u8>,
    /// Current read cursor inside `pending_chunk`.
    pending_offset: usize,
    /// Set once the stdout reader thread reports clean EOF.
    stdout_eof: bool,
    /// Background thread reading child stdout into `stdout_rx`.
    stdout_thread: Option<JoinHandle<()>>,
    /// Live child handle cleared once waited or reaped.
    child: Option<Child>,
    /// Background thread draining stderr (capped at 64 KiB).
    stderr_thread: Option<JoinHandle<Vec<u8>>>,
    /// Maximum idle duration permitted while waiting for the next stdout chunk.
    idle_timeout: Duration,
    /// Maximum duration permitted while waiting for child exit after stdout EOF.
    wait_timeout: Duration,
}

impl FfmpegPipeChild {
    /// Spawns a configured `Command` with environment-configured idle and post-EOF wait timeouts.
    ///
    /// Accepts an owned `Command`. Uses [`resolve_pipe_idle_timeout`] for the
    /// idle-read timeout and `idle_timeout.min(10s)` for the post-EOF wait
    /// deadline. Returns `MediaError::Io` if spawning or taking pipes fails.
    fn spawn(cmd: Command) -> Result<Self, MediaError> {
        let idle_timeout = resolve_pipe_idle_timeout();
        let wait_timeout = idle_timeout.min(Duration::from_secs(POST_EOF_WAIT_TIMEOUT_SECS));
        Self::spawn_with_timeouts(cmd, idle_timeout, wait_timeout)
    }

    /// Spawns a configured `Command` with explicit idle-read and post-EOF wait timeouts.
    ///
    /// Accepts an owned `Command`, `idle_timeout`, and `wait_timeout`. Wraps
    /// the spawned child in an RAII guard immediately so setup failures never
    /// orphan the process, moves stdout to a bounded channel reader thread, and
    /// drains stderr concurrently. Returns `MediaError::Io` on spawn or pipe
    /// setup failure.
    fn spawn_with_timeouts(
        mut cmd: Command,
        idle_timeout: Duration,
        wait_timeout: Duration,
    ) -> Result<Self, MediaError> {
        struct SpawnReapGuard(Option<Child>);
        impl Drop for SpawnReapGuard {
            /// Kills and reaps the child if setup aborts before ownership transfers.
            ///
            /// Accepts `&mut self`. Ignores cleanup errors and never panics.
            fn drop(&mut self) {
                if let Some(mut child) = self.0.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut guard = SpawnReapGuard(Some(cmd.spawn().map_err(MediaError::Io)?));
        let (mut stdout, stderr) = {
            let child = guard
                .0
                .as_mut()
                .expect("spawn guard must hold spawned child");
            let stdout = child.stdout.take().ok_or_else(|| {
                MediaError::Io(std::io::Error::other("ffmpeg stdout was not piped"))
            })?;
            let stderr = child.stderr.take().ok_or_else(|| {
                MediaError::Io(std::io::Error::other("ffmpeg stderr was not piped"))
            })?;
            (stdout, stderr)
        };

        let (tx, rx) = mpsc::sync_channel::<std::io::Result<Vec<u8>>>(STDOUT_CHANNEL_CAPACITY);
        let stdout_thread = thread::spawn(move || {
            let mut buf = vec![0_u8; STREAM_CHUNK_BYTES];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        break;
                    }
                }
            }
        });
        let stderr_thread = spawn_stderr_drainer(stderr);
        let child = guard.0.take();

        Ok(Self {
            stdout_rx: Some(rx),
            pending_chunk: Vec::new(),
            pending_offset: 0,
            stdout_eof: false,
            stdout_thread: Some(stdout_thread),
            child,
            stderr_thread: Some(stderr_thread),
            idle_timeout,
            wait_timeout,
        })
    }

    /// Waits up to `wait_timeout` for normal child exit after stdout EOF and checks exit status.
    ///
    /// Accepts `&mut self` and an error context label. Polls `child.try_wait()`
    /// with a 10 ms cadence until the process exits or `wait_timeout` expires.
    /// On timeout or wait error, kills and reaps the child and detaches drainer
    /// threads without joining, returning `MediaError::Timeout` or
    /// `MediaError::Io`. Only joins the stderr drainer thread after
    /// `child.try_wait()` returns `Ok(Some(status))`. Returns
    /// `MediaError::DecodeFailed` on non-zero exit status.
    fn wait_checked(&mut self, context: &str) -> Result<(), MediaError> {
        self.stdout_eof = true;
        drop(self.stdout_rx.take());
        self.pending_chunk.clear();
        self.pending_offset = 0;

        let Some(mut child) = self.child.take() else {
            return Ok(());
        };

        let deadline = Instant::now() + self.wait_timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    let now = Instant::now();
                    if now >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        drop(self.stdout_thread.take());
                        drop(self.stderr_thread.take());
                        return Err(MediaError::Timeout(format!(
                            "ffmpeg {context} did not exit within {:.1}s after stdout EOF",
                            self.wait_timeout.as_secs_f64()
                        )));
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    thread::sleep(remaining.min(WAIT_POLL_INTERVAL));
                }
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    drop(self.stdout_thread.take());
                    drop(self.stderr_thread.take());
                    return Err(MediaError::Io(err));
                }
            }
        };

        drop(self.stdout_thread.take());
        let stderr = self
            .stderr_thread
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        if !status.success() {
            return Err(MediaError::DecodeFailed(format_ffmpeg_failure(
                context,
                status.code(),
                &stderr,
            )));
        }
        Ok(())
    }

    /// Immediately terminates and reaps the child process while detaching drainer threads.
    ///
    /// Accepts `&mut self`. Closes the stdout channel receiver, sends kill,
    /// reaps the direct child via `wait()`, and detaches both background
    /// drainer threads without `.join()` so grandchildren holding open pipe FDs
    /// can never block cleanup. Never panics.
    fn kill_and_reap_detached(&mut self) {
        self.stdout_eof = true;
        drop(self.stdout_rx.take());
        self.pending_chunk.clear();
        self.pending_offset = 0;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        drop(self.stdout_thread.take());
        drop(self.stderr_thread.take());
    }
}

impl Read for FfmpegPipeChild {
    /// Reads decoded bytes from the FFmpeg stdout channel with an idle-read timeout.
    ///
    /// Accepts a destination buffer. Serves buffered remainder bytes first,
    /// then waits up to `idle_timeout` for the next 64 KiB chunk from the
    /// background stdout reader thread. Returns `Ok(0)` at clean EOF. On idle
    /// timeout, calls [`Self::kill_and_reap_detached`] immediately and returns
    /// `std::io::ErrorKind::TimedOut`; on pipe read errors, kills and reaps the
    /// child and propagates the OS error.
    fn read(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
        if dst.is_empty() {
            return Ok(0);
        }
        if self.pending_offset < self.pending_chunk.len() {
            let available = &self.pending_chunk[self.pending_offset..];
            let take = available.len().min(dst.len());
            dst[..take].copy_from_slice(&available[..take]);
            self.pending_offset += take;
            if self.pending_offset == self.pending_chunk.len() {
                self.pending_chunk.clear();
                self.pending_offset = 0;
            }
            return Ok(take);
        }
        if self.stdout_eof {
            return Ok(0);
        }
        let Some(rx) = self.stdout_rx.as_ref() else {
            return Ok(0);
        };
        match rx.recv_timeout(self.idle_timeout) {
            Ok(Ok(chunk)) => {
                let take = chunk.len().min(dst.len());
                dst[..take].copy_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    self.pending_chunk = chunk;
                    self.pending_offset = take;
                }
                Ok(take)
            }
            Ok(Err(err)) => {
                self.kill_and_reap_detached();
                Err(err)
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.stdout_eof = true;
                self.stdout_rx = None;
                Ok(0)
            }
            Err(RecvTimeoutError::Timeout) => {
                let secs = self.idle_timeout.as_secs_f64();
                self.kill_and_reap_detached();
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("ffmpeg audio pipe idle read exceeded timeout of {secs:.1}s"),
                ))
            }
        }
    }
}

impl Drop for FfmpegPipeChild {
    /// Guarantees the FFmpeg subprocess is killed and reaped without hanging when dropped early.
    ///
    /// Accepts `&mut self`. Delegates to [`Self::kill_and_reap_detached`]. Never panics.
    fn drop(&mut self) {
        self.kill_and_reap_detached();
    }
}

/// Builds FFmpeg arguments for sequential 16 kHz mono `f32le` PCM piping from a validated `file:` input arg.
///
/// Accepts a pre-validated `file:` input `OsStr` and probed audio stream index.
/// Includes `-protocol_whitelist file` and explicit `-i`. Performs no I/O and
/// cannot fail.
fn sequential_pcm_args_for_input(input_arg: &OsStr, stream_index: usize) -> Vec<OsString> {
    vec![
        OsString::from("-nostdin"),
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-protocol_whitelist"),
        OsString::from("file"),
        OsString::from("-i"),
        input_arg.to_os_string(),
        OsString::from("-map"),
        OsString::from(format!("0:{stream_index}")),
        OsString::from("-ac"),
        OsString::from("1"),
        OsString::from("-ar"),
        OsString::from(SAMPLE_RATE_HZ.to_string()),
        OsString::from("-f"),
        OsString::from("f32le"),
        OsString::from("pipe:1"),
    ]
}

/// Validates `source` and builds FFmpeg arguments for sequential 16 kHz mono `f32le` PCM piping.
///
/// Accepts the local regular media path and probed audio stream index.
/// Validates `source` through [`ffmpeg_local_input_arg`] before building
/// `-protocol_whitelist file -i file:...` arguments. Returns
/// `MediaError::InvalidInput` or `MediaError::Io` when `source` fails local
/// regular-file validation.
fn sequential_pcm_args(source: &Path, stream_index: usize) -> Result<Vec<OsString>, MediaError> {
    let input_arg = ffmpeg_local_input_arg(source)?;
    Ok(sequential_pcm_args_for_input(&input_arg, stream_index))
}

/// Builds FFmpeg arguments for an input-seeked 30-second adaptive PCM window from a validated `file:` input arg.
///
/// Accepts a pre-validated `file:` input `OsStr`, probed audio stream index,
/// and start mark in seconds. Includes `-protocol_whitelist file` and explicit
/// `-i`. Performs no I/O and cannot fail.
fn adaptive_pcm_args_for_input(
    input_arg: &OsStr,
    stream_index: usize,
    mark_secs: u64,
) -> Vec<OsString> {
    vec![
        OsString::from("-nostdin"),
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-protocol_whitelist"),
        OsString::from("file"),
        OsString::from("-ss"),
        OsString::from(mark_secs.to_string()),
        OsString::from("-t"),
        OsString::from(ADAPTIVE_WINDOW_SECS.to_string()),
        OsString::from("-i"),
        input_arg.to_os_string(),
        OsString::from("-map"),
        OsString::from(format!("0:{stream_index}")),
        OsString::from("-ac"),
        OsString::from("1"),
        OsString::from("-ar"),
        OsString::from(SAMPLE_RATE_HZ.to_string()),
        OsString::from("-f"),
        OsString::from("f32le"),
        OsString::from("pipe:1"),
    ]
}

/// Validates `source` and builds FFmpeg arguments for an input-seeked 30-second adaptive PCM window.
///
/// Accepts the local regular media path, probed audio stream index, and start
/// mark in seconds. Validates `source` through [`ffmpeg_local_input_arg`]
/// before building `-protocol_whitelist file` arguments. Returns
/// `MediaError::InvalidInput` or `MediaError::Io` when `source` fails local
/// regular-file validation.
fn adaptive_pcm_args(
    source: &Path,
    stream_index: usize,
    mark_secs: u64,
) -> Result<Vec<OsString>, MediaError> {
    let input_arg = ffmpeg_local_input_arg(source)?;
    Ok(adaptive_pcm_args_for_input(
        &input_arg,
        stream_index,
        mark_secs,
    ))
}

/// Builds FFmpeg arguments for streaming `--extract-audio` into stdout from a validated `file:` input arg.
///
/// Accepts a pre-validated `file:` input `OsStr`, probed audio stream index,
/// and requested [`AudioExtractFormat`]. Includes `-protocol_whitelist file`
/// and explicit `-i`. Performs no I/O and cannot fail.
fn extract_audio_ffmpeg_args_for_input(
    input_arg: &OsStr,
    stream_index: usize,
    format: AudioExtractFormat,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-nostdin"),
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-protocol_whitelist"),
        OsString::from("file"),
        OsString::from("-i"),
        input_arg.to_os_string(),
        OsString::from("-map"),
        OsString::from(format!("0:{stream_index}")),
    ];

    match format {
        AudioExtractFormat::Wav => {
            args.extend([
                OsString::from("-ac"),
                OsString::from("1"),
                OsString::from("-ar"),
                OsString::from(SAMPLE_RATE_HZ.to_string()),
                OsString::from("-f"),
                OsString::from("s16le"),
                OsString::from("pipe:1"),
            ]);
        }
        AudioExtractFormat::Flac => {
            args.extend([
                OsString::from("-c:a"),
                OsString::from("flac"),
                OsString::from("-f"),
                OsString::from("flac"),
                OsString::from("pipe:1"),
            ]);
        }
        AudioExtractFormat::M4a => {
            args.extend([
                OsString::from("-c:a"),
                OsString::from("aac"),
                OsString::from("-b:a"),
                OsString::from("128k"),
                OsString::from("-f"),
                OsString::from("mp4"),
                OsString::from("-movflags"),
                OsString::from("frag_keyframe+empty_moov"),
                OsString::from("pipe:1"),
            ]);
        }
        AudioExtractFormat::Opus => {
            args.extend([
                OsString::from("-c:a"),
                OsString::from("libopus"),
                OsString::from("-b:a"),
                OsString::from("96k"),
                OsString::from("-f"),
                OsString::from("ogg"),
                OsString::from("pipe:1"),
            ]);
        }
    }

    args
}

/// Validates `source` and builds FFmpeg arguments for streaming `--extract-audio` into stdout.
///
/// Accepts the local regular media path, probed audio stream index, and
/// requested [`AudioExtractFormat`]. Validates `source` through
/// [`ffmpeg_local_input_arg`] and includes `-protocol_whitelist file` and
/// explicit `-i`. Returns `MediaError::InvalidInput` or `MediaError::Io` when
/// `source` fails local regular-file validation.
pub fn extract_audio_ffmpeg_args(
    source: &Path,
    stream_index: usize,
    format: AudioExtractFormat,
) -> Result<Vec<OsString>, MediaError> {
    let input_arg = ffmpeg_local_input_arg(source)?;
    Ok(extract_audio_ffmpeg_args_for_input(
        &input_arg,
        stream_index,
        format,
    ))
}

/// Runs one timeout-bounded adaptive FFmpeg window pipe and returns its raw `f32le` stdout bytes.
///
/// Accepts the configured FFmpeg `Command`, maximum wall-clock `timeout`, and
/// maximum stdout byte cap (`WINDOW_BYTES`). Delegates execution to
/// [`run_command_with_timeout`] so stdout/stderr draining is memory-capped,
/// child reaping is RAII-guarded, and drainer threads are detached on timeout
/// or kill without hanging. Returns raw `f32le` bytes (truncated to
/// `max_stdout_bytes`) on success, or `MediaError::Timeout`,
/// `MediaError::DecodeFailed`, or `MediaError::Io`.
fn run_bounded_adaptive_command(
    cmd: Command,
    timeout: Duration,
    max_stdout_bytes: usize,
) -> Result<Vec<u8>, MediaError> {
    let mut output = run_command_with_timeout(cmd, timeout)?;
    if !output.status.success() {
        return Err(MediaError::DecodeFailed(format_ffmpeg_failure(
            "adaptive audio decode",
            output.status.code(),
            &output.stderr,
        )));
    }
    if output.stdout.len() > max_stdout_bytes {
        output.stdout.truncate(max_stdout_bytes);
    }
    Ok(output.stdout)
}

/// Decodes raw little-endian `f32` PCM bytes and verifies alignment and finiteness.
///
/// Accepts raw `f32le` bytes. Returns a vector of finite `f32` samples.
/// Returns `MediaError::DecodeFailed` if `bytes.len()` is not a multiple of 4
/// or if any decoded sample is `NaN` or infinite.
fn decode_f32le_bytes(bytes: &[u8]) -> Result<Vec<f32>, MediaError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(MediaError::DecodeFailed(
            "FFmpeg emitted truncated f32 PCM sample".to_string(),
        ));
    }
    let mut pcm = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !sample.is_finite() {
            return Err(MediaError::DecodeFailed(
                "FFmpeg emitted non-finite f32 PCM sample".to_string(),
            ));
        }
        pcm.push(sample);
    }
    Ok(pcm)
}

/// Core sequential 30 s / 4 s overlap PCM window slicer with optional single-pass WAV tee.
///
/// Separated from subprocess spawning so full-mode window boundaries, in-flight
/// 4 GiB RIFF WAV enforcement, and single-pass WAV header patching are
/// exhaustively unit-testable in pure Rust.
struct SequentialPcmSlicer<'a, W: Write + Seek> {
    /// Rolling `f32le` byte buffer holding up to one 30 s window (with 4 s overlap retained).
    bytes: Vec<u8>,
    /// Zero-based index of the next window to yield.
    next_index: usize,
    /// Total 16 kHz samples read so far from the underlying stream.
    total_samples: usize,
    /// Optional seekable WAV output sink receiving each newly decoded sample once as `s16le`.
    wav_tee: Option<&'a mut W>,
    /// Total `s16le` PCM payload bytes written to `wav_tee` so far.
    wav_data_bytes: u64,
    /// Maximum PCM payload bytes permitted for `wav_tee` (defaults to [`MAX_WAV_DATA_BYTES`]).
    max_wav_data_bytes: u64,
    /// Whether EOF has been reached on the underlying stream.
    eof: bool,
}

impl<'a, W: Write + Seek> SequentialPcmSlicer<'a, W> {
    /// Initializes a sequential slicer with the standard 4 GiB RIFF WAV cap and writes a 44-byte placeholder when `wav_tee` is provided.
    ///
    /// Accepts an optional seekable WAV writer. Returns an initialized slicer.
    /// Returns `MediaError::Io` if writing the placeholder header fails.
    fn new(wav_tee: Option<&'a mut W>) -> Result<Self, MediaError> {
        Self::with_max_wav_data_bytes(wav_tee, MAX_WAV_DATA_BYTES)
    }

    /// Initializes a sequential slicer with an explicit WAV payload byte limit.
    ///
    /// Accepts an optional seekable WAV writer and `max_wav_data_bytes` limit.
    /// Writes the 44-byte placeholder RIFF header immediately when `wav_tee` is
    /// `Some`. Returns `MediaError::Io` if writing the placeholder fails.
    fn with_max_wav_data_bytes(
        mut wav_tee: Option<&'a mut W>,
        max_wav_data_bytes: u64,
    ) -> Result<Self, MediaError> {
        if let Some(writer) = wav_tee.as_deref_mut() {
            let placeholder = build_wav_header(0)?;
            writer.write_all(&placeholder).map_err(MediaError::Io)?;
        }
        Ok(Self {
            bytes: Vec::with_capacity(WINDOW_BYTES),
            next_index: 0,
            total_samples: 0,
            wav_tee,
            wav_data_bytes: 0,
            max_wav_data_bytes,
            eof: false,
        })
    }

    /// Reads the next 30-second overlapped [`AudioWindow`] from `reader` and tees fresh samples to WAV.
    ///
    /// Accepts a readable `f32le` PCM byte stream. Enforces
    /// `max_wav_data_bytes` before writing each fresh WAV chunk so an
    /// oversized stream fails immediately rather than writing past the 4 GiB
    /// RIFF limit. Returns `Ok(Some(window))` while fresh audio arrives, or
    /// `Ok(None)` upon EOF (without yet patching the WAV header so the caller
    /// can verify FFmpeg exit status first). Returns `MediaError::Timeout` on
    /// timed-out pipe reads, `MediaError::DecodeFailed` on truncated or
    /// non-finite PCM, `MediaError::EncodeFailed` on RIFF limit overflow, and
    /// `MediaError::Io` on read or WAV write errors.
    fn next_window<R: Read>(&mut self, reader: &mut R) -> Result<Option<AudioWindow>, MediaError> {
        if self.eof {
            return Ok(None);
        }

        let kept_bytes = self.bytes.len();
        let target_fresh = WINDOW_BYTES - kept_bytes;
        let mut remaining = target_fresh;
        let mut scratch = [0_u8; STREAM_CHUNK_BYTES];

        while remaining > 0 {
            let want = remaining.min(scratch.len());
            let n = match reader.read(&mut scratch[..want]) {
                Ok(n) => n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
                    return Err(MediaError::Timeout(err.to_string()));
                }
                Err(err) => return Err(MediaError::Io(err)),
            };
            if n == 0 {
                break;
            }
            self.bytes.extend_from_slice(&scratch[..n]);
            remaining -= n;
        }

        let fresh_bytes = self.bytes.len() - kept_bytes;
        if fresh_bytes == 0 {
            self.eof = true;
            return Ok(None);
        }

        if !fresh_bytes.is_multiple_of(4) {
            return Err(MediaError::DecodeFailed(
                "FFmpeg emitted truncated f32 PCM sample".to_string(),
            ));
        }

        let fresh_samples_slice = &self.bytes[kept_bytes..];
        let fresh_samples = decode_f32le_bytes(fresh_samples_slice)?;

        if let Some(writer) = self.wav_tee.as_deref_mut() {
            let chunk_bytes = (fresh_samples.len() as u64).checked_mul(2).ok_or_else(|| {
                MediaError::EncodeFailed(
                    "WAV audio payload exceeds the 4 GiB RIFF size limit".to_string(),
                )
            })?;
            let new_total = self
                .wav_data_bytes
                .checked_add(chunk_bytes)
                .ok_or_else(|| {
                    MediaError::EncodeFailed(
                        "WAV audio payload exceeds the 4 GiB RIFF size limit".to_string(),
                    )
                })?;
            if new_total > self.max_wav_data_bytes {
                return Err(MediaError::EncodeFailed(
                    "WAV audio payload exceeds the 4 GiB RIFF size limit".to_string(),
                ));
            }
            let mut i16_bytes = Vec::with_capacity(fresh_samples.len() * 2);
            for &sample in &fresh_samples {
                i16_bytes.extend_from_slice(&f32_sample_to_i16(sample).to_le_bytes());
            }
            writer.write_all(&i16_bytes).map_err(MediaError::Io)?;
            self.wav_data_bytes = new_total;
        }

        let kept_samples = kept_bytes / 4;
        let start_sample = self.total_samples - kept_samples;
        self.total_samples += fresh_samples.len();

        let mut pcm = Vec::with_capacity(self.bytes.len() / 4);
        for chunk in self.bytes[..kept_bytes].chunks_exact(4) {
            pcm.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        pcm.extend_from_slice(&fresh_samples);

        let end_sample = start_sample + pcm.len();
        let start_ms = samples_to_ms(start_sample);
        let end_ms = samples_to_ms(end_sample).max(start_ms + 1);
        let index = self.next_index;
        self.next_index += 1;

        if self.bytes.len() == WINDOW_BYTES {
            self.bytes.copy_within(WINDOW_BYTES - OVERLAP_BYTES.., 0);
            self.bytes.truncate(OVERLAP_BYTES);
        } else {
            self.bytes.clear();
            self.eof = true;
        }

        Ok(Some(AudioWindow {
            index,
            start_ms,
            end_ms,
            pcm_16k_f32: pcm,
        }))
    }

    /// Patches the finalized RIFF/WAV header on the optional tee writer and returns total WAV bytes written.
    ///
    /// Accepts `&mut self`. Returns `Ok(total_wav_bytes)` (or `Ok(0)` if no WAV
    /// tee was configured). Returns `MediaError::EncodeFailed` on RIFF overflow
    /// or `MediaError::Io` on seek/write/flush failure.
    fn finalize_wav_tee(&mut self) -> Result<u64, MediaError> {
        if let Some(writer) = self.wav_tee.as_deref_mut() {
            patch_wav_header(writer, self.wav_data_bytes)
        } else {
            Ok(0)
        }
    }
}

/// Mode-specific state machine owned by [`AudioWindowStream`].
enum WindowStreamMode<'a, W: Write + Seek = StagedWriter> {
    /// Single sequential FFmpeg pipe with 30 s / 4 s overlap and optional single-pass WAV tee.
    Full {
        /// Active FFmpeg subprocess (reaped automatically on drop or verified on EOF).
        child: FfmpegPipeChild,
        /// Rolling window slicer + optional WAV tee sink.
        slicer: SequentialPcmSlicer<'a, W>,
        /// Whether EOF verification and WAV header patching have already run.
        finalized: bool,
    },
    /// Independent input-seeked 30 s FFmpeg pipes scheduled at 0:00, 15:00, 30:00, ...
    Adaptive {
        /// Resolved path to the `ffmpeg` binary.
        ffmpeg_bin: PathBuf,
        /// Source media file path.
        source: PathBuf,
        /// Probed Container stream index (`0:<stream_index>`).
        stream_index: usize,
        /// Remaining sample marks in seconds.
        remaining_marks: VecDeque<u64>,
        /// Zero-based index of the next non-empty yielded window.
        next_index: usize,
    },
    /// Pre-captured VAD speech segment sliced into at most 2 overlapped 30 s windows.
    Short {
        /// Remaining pre-sliced windows (0, 1, or 2 items).
        windows: VecDeque<AudioWindow>,
    },
}

/// Zero-temp-file FFmpeg audio window stream supporting `full`, `adaptive`, and `short` ASR modes.
pub struct AudioWindowStream<'a, W: Write + Seek = StagedWriter> {
    /// Active mode implementation.
    inner: WindowStreamMode<'a, W>,
    /// Non-fatal warnings recorded during probing, VAD, or adaptive seeking.
    warnings: Vec<String>,
    /// Total 16 kHz PCM samples decoded (or scanned during `short` VAD).
    total_decoded_samples: usize,
    /// Populated when `short` mode found no speech before EOF or the 600 s cap.
    vad_no_speech_scanned_ms: Option<u64>,
    /// Total bytes written to the single-pass WAV tee (non-zero only in `full` + WAV tee).
    wav_tee_bytes_written: u64,
    /// Optional non-fatal error captured when finalizing the WAV tee at EOF.
    wav_tee_error: Option<String>,
}

impl<'a> AudioWindowStream<'a, StagedWriter> {
    /// Opens an [`AudioWindowStream`] for `source` using the selected [`AsrMode`].
    ///
    /// Accepts the local regular media path, probed [`AudioProbe`], desired
    /// [`AsrMode`], and optional [`StagedWriter`] for single-pass WAV teeing
    /// (valid only with [`AsrMode::Full`]). Returns a ready stream yielding
    /// `AudioWindow` items with zero temp files on disk. Returns
    /// `MediaError::InvalidInput` if `wav_tee` is paired with `Short` or
    /// `Adaptive`, if `source` fails local regular-file validation, or if
    /// adaptive scheduling exceeds `MAX_ADAPTIVE_MARKS`;
    /// `MediaError::MissingHelper` if `ffmpeg` is missing; or
    /// `MediaError::Timeout` / `MediaError::DecodeFailed` / `MediaError::Io`
    /// if spawning or initial `Short` VAD decoding fails.
    pub(crate) fn open(
        source: &Path,
        probe: &AudioProbe,
        mode: AsrMode,
        wav_tee: Option<&'a mut StagedWriter>,
    ) -> Result<Self, MediaError> {
        if wav_tee.is_some() && mode != AsrMode::Full {
            return Err(MediaError::InvalidInput(
                "single-pass WAV tee is only supported with --asr full".to_string(),
            ));
        }

        let ffmpeg_bin = resolve_ffmpeg()?;

        match mode {
            AsrMode::Full => {
                let args = sequential_pcm_args(source, probe.stream_index)?;
                let mut cmd = Command::new(&ffmpeg_bin);
                cmd.args(args);
                let child = FfmpegPipeChild::spawn(cmd)?;
                let slicer = SequentialPcmSlicer::new(wav_tee)?;
                Ok(Self {
                    inner: WindowStreamMode::Full {
                        child,
                        slicer,
                        finalized: false,
                    },
                    warnings: Vec::new(),
                    total_decoded_samples: 0,
                    vad_no_speech_scanned_ms: None,
                    wav_tee_bytes_written: 0,
                    wav_tee_error: None,
                })
            }
            AsrMode::Adaptive => {
                let _ = ffmpeg_local_input_arg(source)?;
                let marks = adaptive_marks_secs(probe.duration_secs)?;
                Ok(Self {
                    inner: WindowStreamMode::Adaptive {
                        ffmpeg_bin,
                        source: source.to_path_buf(),
                        stream_index: probe.stream_index,
                        remaining_marks: VecDeque::from(marks),
                        next_index: 0,
                    },
                    warnings: Vec::new(),
                    total_decoded_samples: 0,
                    vad_no_speech_scanned_ms: None,
                    wav_tee_bytes_written: 0,
                    wav_tee_error: None,
                })
            }
            AsrMode::Short => Self::open_short(&ffmpeg_bin, source, probe),
        }
    }

    /// Runs the single sequential FFmpeg pipe through [`vad::extract_short_speech`], reaps FFmpeg immediately, and queues `<= 2` windows.
    ///
    /// Accepts the resolved `ffmpeg` path, local regular source media path, and
    /// probed [`AudioProbe`]. Immediately kills and reaps FFmpeg (detaching
    /// drainer threads) when 60 s of speech or the 600 s scan cap is hit, or
    /// checks exit status with a bounded deadline when EOF is reached
    /// naturally. Returns an [`AudioWindowStream`] holding 0, 1, or 2
    /// pre-sliced windows, or `MediaError::Timeout`, `MediaError::InvalidInput`,
    /// `MediaError::DecodeFailed`, or `MediaError::Io`.
    fn open_short(
        ffmpeg_bin: &Path,
        source: &Path,
        probe: &AudioProbe,
    ) -> Result<Self, MediaError> {
        let args = sequential_pcm_args(source, probe.stream_index)?;
        let mut cmd = Command::new(ffmpeg_bin);
        cmd.args(args);
        let mut child = FfmpegPipeChild::spawn(cmd)?;

        let result = vad::extract_short_speech(&mut child).map_err(map_pipe_timeout_error)?;

        if result.reached_eof {
            child.wait_checked("short-mode audio decode")?;
        } else {
            child.kill_and_reap_detached();
        }

        let mut warnings = Vec::new();
        let mut vad_no_speech_scanned_ms = None;
        let windows = match result.outcome {
            VadOutcome::Speech { start_sample, pcm } => slice_short_windows(start_sample, pcm),
            VadOutcome::NoSpeechDetected { scanned_ms } => {
                vad_no_speech_scanned_ms = Some(scanned_ms);
                warnings.push(format!(
                    "no speech detected within first {:.1}s of audio",
                    scanned_ms as f64 / 1_000.0
                ));
                Vec::new()
            }
        };

        Ok(Self {
            inner: WindowStreamMode::Short {
                windows: VecDeque::from(windows),
            },
            warnings,
            total_decoded_samples: result.total_samples_read,
            vad_no_speech_scanned_ms,
            wav_tee_bytes_written: 0,
            wav_tee_error: None,
        })
    }
}

impl<'a, W: Write + Seek> AudioWindowStream<'a, W> {
    /// Yields the next [`AudioWindow`] from this stream, finalizing FFmpeg and any WAV tee on EOF.
    ///
    /// Accepts `&mut self`. Returns `Ok(Some(window))` for each scheduled window
    /// and `Ok(None)` once all windows have been produced and the FFmpeg process
    /// has finalized. If WAV tee header finalization fails at EOF after a final
    /// ASR window was decoded, stores the WAV tee error for [`Self::finish`]
    /// instead of dropping the final `AudioWindow`. Returns
    /// `MediaError::DecodeFailed`, `MediaError::Timeout`, `MediaError::InvalidInput`,
    /// `MediaError::EncodeFailed`, or `MediaError::Io` on stream or decode failure.
    pub fn next_window(&mut self) -> Result<Option<AudioWindow>, MediaError> {
        match &mut self.inner {
            WindowStreamMode::Full {
                child,
                slicer,
                finalized,
            } => {
                if *finalized {
                    return Ok(None);
                }
                let maybe_window = slicer.next_window(child)?;
                self.total_decoded_samples = slicer.total_samples;

                if let Some(window) = maybe_window {
                    if slicer.eof {
                        child.wait_checked("full-mode audio decode")?;
                        match slicer.finalize_wav_tee() {
                            Ok(bytes) => self.wav_tee_bytes_written = bytes,
                            Err(err) => {
                                self.wav_tee_bytes_written = 0;
                                self.wav_tee_error = Some(err.to_string());
                            }
                        }
                        *finalized = true;
                    }
                    return Ok(Some(window));
                }

                child.wait_checked("full-mode audio decode")?;
                match slicer.finalize_wav_tee() {
                    Ok(bytes) => self.wav_tee_bytes_written = bytes,
                    Err(err) => {
                        self.wav_tee_bytes_written = 0;
                        self.wav_tee_error = Some(err.to_string());
                    }
                }
                *finalized = true;
                Ok(None)
            }
            WindowStreamMode::Adaptive {
                ffmpeg_bin,
                source,
                stream_index,
                remaining_marks,
                next_index,
            } => {
                while let Some(mark_secs) = remaining_marks.pop_front() {
                    let args = adaptive_pcm_args(source, *stream_index, mark_secs)?;
                    let mut cmd = Command::new(&*ffmpeg_bin);
                    cmd.args(args);
                    let raw =
                        run_bounded_adaptive_command(cmd, ADAPTIVE_WINDOW_TIMEOUT, WINDOW_BYTES)?;
                    let pcm = decode_f32le_bytes(&raw)?;
                    if pcm.is_empty() {
                        self.warnings.push(format!(
                            "adaptive mark at {mark_secs}s yielded 0 audio samples; skipping"
                        ));
                        continue;
                    }
                    let start_ms = mark_secs.saturating_mul(1_000);
                    let decoded_ms = samples_to_ms(pcm.len()).max(1);
                    let end_ms = start_ms.saturating_add(decoded_ms);
                    let index = *next_index;
                    *next_index += 1;
                    self.total_decoded_samples += pcm.len();
                    return Ok(Some(AudioWindow {
                        index,
                        start_ms,
                        end_ms,
                        pcm_16k_f32: pcm,
                    }));
                }
                Ok(None)
            }
            WindowStreamMode::Short { windows } => Ok(windows.pop_front()),
        }
    }

    /// Consumes the stream after iteration and returns its [`AudioStreamSummary`].
    ///
    /// Accepts ownership of `self`. If `Full` mode has not yet reached EOF
    /// (early stop), the FFmpeg subprocess is killed and reaped cleanly without
    /// patching the incomplete WAV tee. Surfaces any non-fatal WAV tee
    /// finalization error in `AudioStreamSummary::wav_tee_error`. Returns
    /// `Ok(AudioStreamSummary)`.
    pub fn finish(self) -> Result<AudioStreamSummary, MediaError> {
        Ok(AudioStreamSummary {
            total_decoded_samples: self.total_decoded_samples,
            warnings: self.warnings,
            vad_no_speech_scanned_ms: self.vad_no_speech_scanned_ms,
            wav_tee_bytes_written: self.wav_tee_bytes_written,
            wav_tee_error: self.wav_tee_error,
        })
    }
}

impl<W: Write + Seek> Iterator for AudioWindowStream<'_, W> {
    type Item = Result<AudioWindow, MediaError>;

    /// Advances the stream and yields the next `Result<AudioWindow, MediaError>`.
    ///
    /// Accepts `&mut self`. Returns `Some(Ok(window))`, `Some(Err(err))`, or
    /// `None` at end-of-stream.
    fn next(&mut self) -> Option<Self::Item> {
        match self.next_window() {
            Ok(Some(window)) => Some(Ok(window)),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

/// Copies a piped audio stream into `writer` in 64 KiB chunks while enforcing `max_wav_data_bytes` in flight.
///
/// Accepts any readable audio byte stream, the target [`AudioExtractFormat`],
/// a mutable `Write` destination, and the maximum PCM payload byte limit for
/// WAV output. For [`AudioExtractFormat::Wav`], checks `payload_bytes + n <=
/// max_wav_data_bytes` before writing each chunk so oversized streams abort
/// immediately without writing past the 4 GiB RIFF cap. Returns total payload
/// bytes written (excluding any 44-byte WAV header). Returns
/// `MediaError::Timeout` on timed-out reads, `MediaError::EncodeFailed` on WAV
/// cap overflow, or `MediaError::Io` on read/write errors.
fn stream_audio_to_writer<R: Read, W: Write>(
    reader: &mut R,
    format: AudioExtractFormat,
    writer: &mut W,
    max_wav_data_bytes: u64,
) -> Result<u64, MediaError> {
    let mut payload_bytes = 0_u64;
    let mut buf = [0_u8; STREAM_CHUNK_BYTES];

    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
                return Err(MediaError::Timeout(err.to_string()));
            }
            Err(err) => return Err(MediaError::Io(err)),
        };
        if n == 0 {
            break;
        }
        let next_total = payload_bytes.checked_add(n as u64).ok_or_else(|| {
            MediaError::EncodeFailed(
                "WAV audio payload exceeds the 4 GiB RIFF size limit".to_string(),
            )
        })?;
        if format == AudioExtractFormat::Wav && next_total > max_wav_data_bytes {
            return Err(MediaError::EncodeFailed(
                "WAV audio payload exceeds the 4 GiB RIFF size limit".to_string(),
            ));
        }
        writer.write_all(&buf[..n]).map_err(MediaError::Io)?;
        payload_bytes = next_total;
    }

    Ok(payload_bytes)
}

/// Streams the full probed audio track from FFmpeg directly into `writer` in O(1) memory.
///
/// Accepts the local regular source media path, probed [`AudioProbe`], target
/// [`AudioExtractFormat`], and an open [`StagedWriter`]. Validates `source` via
/// [`extract_audio_ffmpeg_args`], spawns FFmpeg mapping `0:<probe.stream_index>`
/// to stdout, and copies in 64 KiB chunks into `writer` with zero intermediate
/// temp files; for [`AudioExtractFormat::Wav`], writes a 44-byte placeholder
/// RIFF/WAV header first, enforces [`MAX_WAV_DATA_BYTES`] before writing every
/// chunk, and seeks back to offset 0 to patch exact RIFF and data chunk sizes
/// after FFmpeg exits cleanly. Returns the total number of bytes written to
/// `writer`. Returns `MediaError::InvalidInput` if `source` fails local-file
/// validation, `MediaError::MissingHelper` if `ffmpeg` is absent,
/// `MediaError::Timeout` if FFmpeg stalls or fails to exit after EOF,
/// `MediaError::DecodeFailed` if FFmpeg exits non-zero or yields zero audio
/// bytes, `MediaError::EncodeFailed` on RIFF size overflow, or `MediaError::Io`
/// on read/write/seek failure (leaving the caller to drop `StagedWriter` so its
/// staged temp file is unlinked automatically).
pub(crate) fn extract_audio_to_staged(
    source: &Path,
    probe: &AudioProbe,
    format: AudioExtractFormat,
    writer: &mut StagedWriter,
) -> Result<u64, MediaError> {
    let args = extract_audio_ffmpeg_args(source, probe.stream_index, format)?;
    let ffmpeg = resolve_ffmpeg()?;
    let mut cmd = Command::new(ffmpeg);
    cmd.args(args);

    if format == AudioExtractFormat::Wav {
        let placeholder = build_wav_header(0)?;
        writer.write_all(&placeholder).map_err(MediaError::Io)?;
    }

    let mut child = FfmpegPipeChild::spawn(cmd)?;
    let payload_bytes = stream_audio_to_writer(&mut child, format, writer, MAX_WAV_DATA_BYTES)?;

    child.wait_checked("audio extraction")?;

    if payload_bytes == 0 {
        return Err(MediaError::DecodeFailed(
            "ffmpeg audio extraction produced zero audio bytes".to_string(),
        ));
    }

    if format == AudioExtractFormat::Wav {
        patch_wav_header(writer, payload_bytes)
    } else {
        writer.flush().map_err(MediaError::Io)?;
        Ok(payload_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn f32_silence_bytes(seconds: usize) -> Vec<u8> {
        vec![0_u8; seconds * SAMPLE_RATE_HZ * 4]
    }

    fn f32_ramp_bytes(samples: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 4);
        for i in 0..samples {
            let val = ((i % 200) as f32 / 100.0) - 1.0;
            out.extend_from_slice(&val.to_le_bytes());
        }
        out
    }

    #[test]
    fn sequential_slicer_matches_window_boundaries_for_1s_30s_31s_56s_57s() {
        let cases: &[(usize, &[(u64, u64)])] = &[
            (1, &[(0, 1_000)]),
            (30, &[(0, 30_000)]),
            (31, &[(0, 30_000), (26_000, 31_000)]),
            (56, &[(0, 30_000), (26_000, 56_000)]),
            (57, &[(0, 30_000), (26_000, 56_000), (52_000, 57_000)]),
        ];

        for &(duration_secs, expected) in cases {
            let raw = f32_silence_bytes(duration_secs);
            let mut reader = Cursor::new(raw);
            let mut wav_sink: Cursor<Vec<u8>> = Cursor::new(Vec::new());
            let mut slicer =
                SequentialPcmSlicer::new(Some(&mut wav_sink)).expect("init slicer with WAV tee");

            let mut actual = Vec::new();
            while let Some(window) = slicer.next_window(&mut reader).expect("slice window") {
                actual.push((window.start_ms, window.end_ms));
            }
            let wav_total = slicer.finalize_wav_tee().expect("patch WAV header");

            assert_eq!(actual, expected, "duration={duration_secs}s");
            assert_eq!(slicer.total_samples, duration_secs * SAMPLE_RATE_HZ);

            // Verify single-pass WAV tee wrote each decoded sample exactly once (no overlap duplication).
            let expected_pcm_bytes = (duration_secs * SAMPLE_RATE_HZ * 2) as u64;
            assert_eq!(wav_total, WAV_HEADER_BYTES as u64 + expected_pcm_bytes);
            let wav_bytes = wav_sink.into_inner();
            assert_eq!(wav_bytes.len() as u64, wav_total);
            assert_eq!(&wav_bytes[0..4], b"RIFF");
            assert_eq!(&wav_bytes[8..12], b"WAVE");
            assert_eq!(
                u32::from_le_bytes(wav_bytes[4..8].try_into().unwrap()),
                36 + expected_pcm_bytes as u32
            );
            assert_eq!(
                u32::from_le_bytes(wav_bytes[40..44].try_into().unwrap()),
                expected_pcm_bytes as u32
            );
        }
    }

    #[test]
    fn wav_header_patching_and_f32_to_i16_conversion_are_exact() {
        assert_eq!(f32_sample_to_i16(-1.5), -32_767);
        assert_eq!(f32_sample_to_i16(-1.0), -32_767);
        assert_eq!(f32_sample_to_i16(0.0), 0);
        assert_eq!(f32_sample_to_i16(0.5), 16_384);
        assert_eq!(f32_sample_to_i16(1.0), 32_767);
        assert_eq!(f32_sample_to_i16(2.5), 32_767);

        // Build via placeholder + PCM write + patch.
        let mut cursor = Cursor::new(Vec::new());
        let placeholder = build_wav_header(0).expect("placeholder");
        cursor.write_all(&placeholder).expect("write placeholder");
        let pcm = f32_ramp_bytes(160);
        let samples = decode_f32le_bytes(&pcm).expect("decode ramp");
        for s in &samples {
            cursor
                .write_all(&f32_sample_to_i16(*s).to_le_bytes())
                .expect("write s16");
        }
        let total = patch_wav_header(&mut cursor, (samples.len() * 2) as u64).expect("patch");
        assert_eq!(total, 44 + 320);

        let bytes = cursor.into_inner();
        assert_eq!(bytes.len(), 364);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 356);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            16_000
        );
        assert_eq!(
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            32_000
        );
        assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 320);

        // Overflow past 4 GiB RIFF cap fails cleanly.
        let err = build_wav_header(u64::from(u32::MAX)).expect_err("RIFF overflow must fail");
        assert!(matches!(err, MediaError::EncodeFailed(_)));
    }

    #[test]
    fn adaptive_mark_scheduling_covers_0s_through_3000s() {
        assert_eq!(adaptive_marks_secs(None).expect("None"), vec![0]);
        assert_eq!(adaptive_marks_secs(Some(0.0)).expect("0s"), vec![0]);
        assert_eq!(adaptive_marks_secs(Some(10.0)).expect("10s"), vec![0]);
        assert_eq!(adaptive_marks_secs(Some(900.0)).expect("900s"), vec![0]);
        assert_eq!(
            adaptive_marks_secs(Some(900.1)).expect("900.1s"),
            vec![0, 900]
        );
        assert_eq!(
            adaptive_marks_secs(Some(1800.0)).expect("1800s"),
            vec![0, 900]
        );
        assert_eq!(
            adaptive_marks_secs(Some(1800.5)).expect("1800.5s"),
            vec![0, 900, 1800]
        );
        assert_eq!(
            adaptive_marks_secs(Some(2700.0)).expect("2700s"),
            vec![0, 900, 1800]
        );
        assert_eq!(
            adaptive_marks_secs(Some(3000.0)).expect("3000s"),
            vec![0, 900, 1800, 2700]
        );
    }

    #[test]
    fn adaptive_marks_rejects_non_finite_and_excessive_durations_without_allocating() {
        let exact_max_secs = (MAX_ADAPTIVE_MARKS as f64) * (ADAPTIVE_INTERVAL_SECS as f64);
        let at_cap = adaptive_marks_secs(Some(exact_max_secs)).expect("exact 250 marks allowed");
        assert_eq!(at_cap.len(), MAX_ADAPTIVE_MARKS);
        assert_eq!(at_cap.first().copied(), Some(0));
        assert_eq!(
            at_cap.last().copied(),
            Some(((MAX_ADAPTIVE_MARKS - 1) as u64) * ADAPTIVE_INTERVAL_SECS)
        );

        for bad in [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            exact_max_secs + 0.1,
            1.0e18,
        ] {
            let err = adaptive_marks_secs(Some(bad))
                .expect_err(&format!("expected rejection for duration={bad:?}"));
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "expected InvalidInput for duration={bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn short_window_slicing_caps_at_two_overlapped_30s_windows() {
        assert!(slice_short_windows(0, Vec::new()).is_empty());

        let one = slice_short_windows(40_000, vec![0.0; 20 * SAMPLE_RATE_HZ]);
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].start_ms, one[0].end_ms), (2_500, 22_500));

        let exact_30 = slice_short_windows(0, vec![0.0; 30 * SAMPLE_RATE_HZ]);
        assert_eq!(exact_30.len(), 1);
        assert_eq!((exact_30[0].start_ms, exact_30[0].end_ms), (0, 30_000));

        let forty = slice_short_windows(40_000, vec![0.0; 40 * SAMPLE_RATE_HZ]);
        assert_eq!(forty.len(), 2);
        assert_eq!((forty[0].start_ms, forty[0].end_ms), (2_500, 32_500));
        assert_eq!((forty[1].start_ms, forty[1].end_ms), (28_500, 42_500));
        assert_eq!(forty[0].end_ms - forty[1].start_ms, 4_000);

        let sixty = slice_short_windows(40_000, vec![0.0; 60 * SAMPLE_RATE_HZ]);
        assert_eq!(sixty.len(), 2);
        assert_eq!(sixty[0].pcm_16k_f32.len(), WINDOW_SAMPLES);
        assert_eq!(sixty[1].pcm_16k_f32.len(), WINDOW_SAMPLES);
        assert_eq!((sixty[0].start_ms, sixty[0].end_ms), (2_500, 32_500));
        assert_eq!((sixty[1].start_ms, sixty[1].end_ms), (28_500, 58_500));
    }

    #[test]
    fn extract_audio_args_match_all_four_formats_and_include_protocol_whitelist() {
        let input_arg = OsStr::new("file:/data/clip.mp4");
        let to_strs = |v: Vec<OsString>| -> Vec<String> {
            v.into_iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect()
        };

        let seq = to_strs(sequential_pcm_args_for_input(input_arg, 1));
        assert!(seq.windows(2).any(|w| w == ["-protocol_whitelist", "file"]));
        assert!(seq.windows(2).any(|w| w == ["-i", "file:/data/clip.mp4"]));

        let adaptive = to_strs(adaptive_pcm_args_for_input(input_arg, 1, 900));
        assert!(
            adaptive
                .windows(2)
                .any(|w| w == ["-protocol_whitelist", "file"])
        );
        assert!(
            adaptive
                .windows(2)
                .any(|w| w == ["-i", "file:/data/clip.mp4"])
        );

        let wav = to_strs(extract_audio_ffmpeg_args_for_input(
            input_arg,
            1,
            AudioExtractFormat::Wav,
        ));
        assert!(wav.windows(2).any(|w| w == ["-protocol_whitelist", "file"]));
        assert!(wav.windows(2).any(|w| w == ["-i", "file:/data/clip.mp4"]));
        assert!(wav.windows(2).any(|w| w == ["-map", "0:1"]));
        assert!(wav.windows(2).any(|w| w == ["-f", "s16le"]));
        assert_eq!(wav.last().map(String::as_str), Some("pipe:1"));

        let flac = to_strs(extract_audio_ffmpeg_args_for_input(
            input_arg,
            2,
            AudioExtractFormat::Flac,
        ));
        assert!(flac.windows(2).any(|w| w == ["-c:a", "flac"]));
        assert!(flac.windows(2).any(|w| w == ["-f", "flac"]));

        let m4a = to_strs(extract_audio_ffmpeg_args_for_input(
            input_arg,
            0,
            AudioExtractFormat::M4a,
        ));
        assert!(m4a.windows(2).any(|w| w == ["-c:a", "aac"]));
        assert!(
            m4a.windows(2)
                .any(|w| w == ["-movflags", "frag_keyframe+empty_moov"])
        );

        let opus = to_strs(extract_audio_ffmpeg_args_for_input(
            input_arg,
            0,
            AudioExtractFormat::Opus,
        ));
        assert!(opus.windows(2).any(|w| w == ["-c:a", "libopus"]));
        assert!(opus.windows(2).any(|w| w == ["-f", "ogg"]));

        // Public validator rejects protocol strings before building args.
        let err = extract_audio_ffmpeg_args(
            Path::new("http://127.0.0.1:46069/clip.mp4"),
            0,
            AudioExtractFormat::Wav,
        )
        .expect_err("protocol string must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }

    #[test]
    fn parse_pipe_idle_timeout_clamps_valid_range_and_defaults_on_invalid() {
        assert_eq!(
            parse_pipe_idle_timeout(None),
            Duration::from_secs(DEFAULT_PIPE_IDLE_TIMEOUT_SECS)
        );
        assert_eq!(
            parse_pipe_idle_timeout(Some("not-a-number")),
            Duration::from_secs(DEFAULT_PIPE_IDLE_TIMEOUT_SECS)
        );
        assert_eq!(parse_pipe_idle_timeout(Some("0")), Duration::from_secs(1));
        assert_eq!(parse_pipe_idle_timeout(Some("5")), Duration::from_secs(5));
        assert_eq!(
            parse_pipe_idle_timeout(Some("9999")),
            Duration::from_secs(MAX_PIPE_IDLE_TIMEOUT_SECS)
        );
    }

    #[cfg(unix)]
    #[test]
    fn raii_drop_kills_and_reaps_child_immediately() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]);
        let started = Instant::now();
        {
            let child = FfmpegPipeChild::spawn(cmd).expect("spawn sleeper");
            assert!(child.child.is_some());
            // Drop immediately without waiting for sleep to finish.
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "RAII drop must kill and reap subprocess immediately"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pipe_idle_timeout_and_grandchild_open_fd_do_not_hang() {
        // Direct sleeping child stalls stdout -> hits TimedOut promptly and maps to MediaError::Timeout.
        let mut slow_cmd = Command::new("sh");
        slow_cmd.args(["-c", "exec sleep 30"]);
        let mut slow_child = FfmpegPipeChild::spawn_with_timeouts(
            slow_cmd,
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .expect("spawn slow child");
        let started = Instant::now();
        let mut slicer = SequentialPcmSlicer::<Cursor<Vec<u8>>>::new(None).expect("init slicer");
        let err = slicer
            .next_window(&mut slow_child)
            .expect_err("stalled stdout must time out");
        assert!(
            matches!(err, MediaError::Timeout(_)),
            "expected MediaError::Timeout, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "idle timeout must return promptly, elapsed={:?}",
            started.elapsed()
        );

        // Child spawns a background grandchild inheriting stdout/stderr; detached drainer threads
        // must allow timeout + kill + drop to return immediately without blocking on the grandchild.
        let mut grandchild_cmd = Command::new("sh");
        grandchild_cmd.args(["-c", "sleep 20 & wait"]);
        let mut grandchild = FfmpegPipeChild::spawn_with_timeouts(
            grandchild_cmd,
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .expect("spawn grandchild holder");
        let gc_started = Instant::now();
        let mut buf = [0_u8; 64];
        let io_err = grandchild
            .read(&mut buf)
            .expect_err("grandchild holding stdout open must time out");
        assert_eq!(io_err.kind(), std::io::ErrorKind::TimedOut);
        drop(grandchild);
        assert!(
            gc_started.elapsed() < Duration::from_secs(2),
            "detached drainer threads must prevent hanging on grandchild FDs, elapsed={:?}",
            gc_started.elapsed()
        );

        // Post-EOF hang: child closes stdout immediately then sleeps; wait_checked must time out.
        let mut post_eof_cmd = Command::new("sh");
        post_eof_cmd.args(["-c", "exec 1>&-; exec sleep 30"]);
        let mut post_eof_child = FfmpegPipeChild::spawn_with_timeouts(
            post_eof_cmd,
            Duration::from_secs(2),
            Duration::from_millis(200),
        )
        .expect("spawn post-eof sleeper");
        let eof_n = post_eof_child.read(&mut buf).expect("should read EOF");
        assert_eq!(eof_n, 0);
        let wait_started = Instant::now();
        let wait_err = post_eof_child
            .wait_checked("post-eof timeout test")
            .expect_err("sleeping child after EOF must hit wait timeout");
        assert!(
            matches!(wait_err, MediaError::Timeout(_)),
            "expected MediaError::Timeout, got {wait_err:?}"
        );
        assert!(
            wait_started.elapsed() < Duration::from_secs(2),
            "post-EOF wait timeout must return promptly, elapsed={:?}",
            wait_started.elapsed()
        );
    }

    #[test]
    fn in_flight_wav_cap_rejects_oversized_chunk_before_writing() {
        // 1) SequentialPcmSlicer WAV tee cap: allow 32,000 bytes (1 s of s16le = 16,000 samples * 2),
        //    then feed 2 s of f32le PCM (which wants 64,000 bytes of s16le) -> must reject BEFORE writing.
        let two_secs = f32_silence_bytes(2);
        let mut reader = Cursor::new(two_secs);
        let mut wav_sink: Cursor<Vec<u8>> = Cursor::new(Vec::new());
        let mut slicer = SequentialPcmSlicer::with_max_wav_data_bytes(Some(&mut wav_sink), 32_000)
            .expect("init capped slicer");
        let err = slicer
            .next_window(&mut reader)
            .expect_err("oversized WAV tee chunk must fail in flight");
        assert!(matches!(err, MediaError::EncodeFailed(_)));
        assert_eq!(
            wav_sink.get_ref().len(),
            WAV_HEADER_BYTES,
            "oversized chunk must be rejected BEFORE writing payload bytes"
        );

        // 2) stream_audio_to_writer WAV cap: allow 100 bytes; feed 120 bytes -> rejects before writing
        //    the second chunk (or the single 120-byte chunk).
        let mut raw_reader = Cursor::new(vec![0xAB_u8; 120]);
        let mut out_sink = Vec::new();
        let err =
            stream_audio_to_writer(&mut raw_reader, AudioExtractFormat::Wav, &mut out_sink, 100)
                .expect_err("oversized WAV extract must fail in flight");
        assert!(matches!(err, MediaError::EncodeFailed(_)));
        assert!(
            out_sink.is_empty(),
            "oversized WAV chunk must not be written before cap check"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wav_tee_finalize_error_preserves_final_asr_window_and_surfaces_in_summary() {
        /// Seekable writer whose streaming writes succeed but whose rewind to offset 0 fails.
        struct FailingHeaderSeekWriter {
            buf: Vec<u8>,
        }

        impl Write for FailingHeaderSeekWriter {
            /// Appends written bytes into the in-memory test buffer.
            ///
            /// Accepts a byte slice. Returns byte count written and cannot fail.
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.buf.extend_from_slice(data);
                Ok(data.len())
            }

            /// Flushes the in-memory buffer as a no-op.
            ///
            /// Accepts `&mut self`. Always returns `Ok(())`.
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl Seek for FailingHeaderSeekWriter {
            /// Simulates an I/O failure when patching the WAV header at EOF.
            ///
            /// Accepts a `SeekFrom` position. Always returns an I/O error.
            fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
                Err(std::io::Error::other("simulated wav header seek failure"))
            }
        }

        // Emit 1 second of f32le silence (64,000 bytes) cleanly onto stdout and exit 0.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "dd if=/dev/zero bs=1000 count=64 2>/dev/null"]);
        let child = FfmpegPipeChild::spawn_with_timeouts(
            cmd,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .expect("spawn 1s pcm emitter");

        let mut failing_writer = FailingHeaderSeekWriter { buf: Vec::new() };
        let slicer =
            SequentialPcmSlicer::new(Some(&mut failing_writer)).expect("write placeholder header");

        let mut stream = AudioWindowStream {
            inner: WindowStreamMode::Full {
                child,
                slicer,
                finalized: false,
            },
            warnings: Vec::new(),
            total_decoded_samples: 0,
            vad_no_speech_scanned_ms: None,
            wav_tee_bytes_written: 0,
            wav_tee_error: None,
        };

        // Final ASR window MUST still be yielded even though finalize_wav_tee failed at EOF.
        let first = stream
            .next_window()
            .expect("finalize_wav_tee failure must not drop final ASR window")
            .expect("expected 1s final window");
        assert_eq!(first.index, 0);
        assert_eq!(first.start_ms, 0);
        assert_eq!(first.end_ms, 1_000);
        assert_eq!(first.pcm_16k_f32.len(), SAMPLE_RATE_HZ);

        assert!(
            stream
                .next_window()
                .expect("post-finalization next_window must return Ok(None)")
                .is_none()
        );

        let summary = stream.finish().expect("finish succeeds");
        assert_eq!(summary.total_decoded_samples, SAMPLE_RATE_HZ);
        assert_eq!(summary.wav_tee_bytes_written, 0);
        let tee_err = summary
            .wav_tee_error
            .as_deref()
            .expect("wav_tee_error must surface in summary");
        assert!(
            tee_err.contains("simulated wav header seek failure"),
            "unexpected wav_tee_error: {tee_err}"
        );
    }
}

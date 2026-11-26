//! Module: glassbox-cli FFmpeg/ffprobe stream metadata probing.
//! File: media/ffprobe.rs.
//! Responsibility: Probe local media containers with `ffprobe` to select primary motion video and decodable audio streams.

use super::{MediaError, ffmpeg_local_input_arg, resolve_ffprobe, run_command_with_timeout};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Timeout enforced on every `ffprobe` metadata probe invocation.
const FFPROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Probed metadata for the selected primary motion video stream.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoProbe {
    /// Zero-based container stream index (`0:<stream_index>`).
    pub stream_index: usize,
    /// Resolved positive, finite duration in seconds.
    pub duration_secs: f64,
    /// Optional trimmed non-empty container title tag.
    pub title: Option<String>,
}

/// Probed metadata for the selected decodable audio stream.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioProbe {
    /// Zero-based container stream index (`0:<stream_index>`).
    pub stream_index: usize,
    /// Codec short name reported by `ffprobe` (for example, `aac`, `opus`, `pcm_s16le`).
    pub codec_name: String,
    /// Positive audio channel count.
    pub channels: u32,
    /// Positive audio sample rate in Hz.
    pub sample_rate: u32,
    /// Optional positive, finite duration in seconds.
    pub duration_secs: Option<f64>,
    /// Optional trimmed non-empty container title tag.
    pub title: Option<String>,
}

/// Probes `path` with `ffprobe` and selects the first motion video stream (`V:0`).
///
/// Accepts a local regular non-symlink media file path. Validates the local
/// file boundary via [`ffmpeg_local_input_arg`] and invokes `ffprobe` with
/// `-protocol_whitelist file` and explicit `-i` so filenames cannot inject
/// options or open remote/virtual protocols. Returns a `VideoProbe` containing
/// the selected stream index, a positive finite duration in seconds, and an
/// optional trimmed container title. Returns `MediaError::InvalidInput` if
/// `path` is not a local regular file, `MediaError::MissingHelper` if `ffprobe`
/// is unavailable, `MediaError::Timeout` if probing exceeds 15 s,
/// `MediaError::Io` on spawn/read failure, or `MediaError::ProbeFailed` when
/// `ffprobe` fails or reports no motion video stream or positive duration.
pub fn probe_video_stream(path: &Path) -> Result<VideoProbe, MediaError> {
    let input_arg = ffmpeg_local_input_arg(path)?;
    let ffprobe = resolve_ffprobe()?;
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
        "-select_streams",
        "V:0",
        "-show_entries",
        "stream=index,duration,duration_ts,time_base:format=duration:format_tags=title",
        "-of",
        "json",
        "-i",
    ])
    .arg(&input_arg);

    let output = run_command_with_timeout(cmd, FFPROBE_TIMEOUT)?;
    if !output.status.success() {
        return Err(MediaError::ProbeFailed(format_probe_failure(
            "video",
            output.status.code(),
            &output.stderr,
        )));
    }
    parse_video_probe_json(&output.stdout)
}

/// Probes `path` with `ffprobe` and selects the first decodable audio stream.
///
/// Accepts a local regular non-symlink media file path. Validates the local
/// file boundary via [`ffmpeg_local_input_arg`] and invokes `ffprobe` with
/// `-protocol_whitelist file` and explicit `-i` so filenames cannot inject
/// options or open remote/virtual protocols. Returns an `AudioProbe` for the
/// first audio stream with a non-empty codec name, positive channel count, and
/// positive sample rate. Returns `MediaError::InvalidInput` if `path` is not a
/// local regular file, `MediaError::MissingHelper` if `ffprobe` is unavailable,
/// `MediaError::Timeout` if probing exceeds 15 s, `MediaError::Io` on
/// spawn/read failure, or `MediaError::ProbeFailed` when `ffprobe` fails or no
/// decodable audio stream exists.
pub fn probe_audio_stream(path: &Path) -> Result<AudioProbe, MediaError> {
    let input_arg = ffmpeg_local_input_arg(path)?;
    let ffprobe = resolve_ffprobe()?;
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
        "-select_streams",
        "a",
        "-show_entries",
        "stream=index,codec_name,channels,sample_rate,duration:format=duration:format_tags=title",
        "-of",
        "json",
        "-i",
    ])
    .arg(&input_arg);

    let output = run_command_with_timeout(cmd, FFPROBE_TIMEOUT)?;
    if !output.status.success() {
        return Err(MediaError::ProbeFailed(format_probe_failure(
            "audio",
            output.status.code(),
            &output.stderr,
        )));
    }
    parse_audio_probe_json(&output.stdout)
}

/// Formats a concise diagnostic message for a non-zero `ffprobe` exit status.
///
/// Accepts the probed stream kind label, optional exit code, and raw stderr
/// bytes. Returns a clean error string without leaking source media data. This
/// helper cannot fail.
fn format_probe_failure(kind: &str, exit_code: Option<i32>, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    if !detail.is_empty() {
        format!("ffprobe {kind} probe failed: {detail}")
    } else if let Some(code) = exit_code {
        format!("ffprobe {kind} probe exited with status {code}")
    } else {
        format!("ffprobe {kind} probe terminated by signal")
    }
}

/// Parses `ffprobe` JSON output into a validated `VideoProbe`.
///
/// Accepts raw JSON stdout bytes from `ffprobe`. Returns a `VideoProbe` with
/// the first motion video stream index, positive finite duration (trying stream
/// `duration`, `duration_ts * time_base`, then `format.duration`), and optional
/// container title. Returns `MediaError::ProbeFailed` on invalid JSON, empty
/// streams, missing index, or missing positive duration.
pub(crate) fn parse_video_probe_json(raw: &[u8]) -> Result<VideoProbe, MediaError> {
    let info: Value = serde_json::from_slice(raw)
        .map_err(|err| MediaError::ProbeFailed(format!("invalid ffprobe JSON output: {err}")))?;
    let streams = info.get("streams").and_then(Value::as_array);
    let stream = streams
        .and_then(|arr| arr.first())
        .ok_or_else(|| MediaError::ProbeFailed("no motion video stream found".to_string()))?;

    let stream_index = parse_stream_index(stream)?;
    let duration_secs = resolve_video_duration(stream, info.get("format")).ok_or_else(|| {
        MediaError::ProbeFailed(
            "a positive video duration is required for even sampling".to_string(),
        )
    })?;
    let title = extract_format_title(info.get("format"));

    Ok(VideoProbe {
        stream_index,
        duration_secs,
        title,
    })
}

/// Parses `ffprobe` JSON output into the first decodable `AudioProbe`.
///
/// Accepts raw JSON stdout bytes from `ffprobe`. Iterates over reported audio
/// streams and returns the first stream with a valid index, non-empty
/// `codec_name`, `channels > 0`, and `sample_rate > 0`. Returns
/// `MediaError::ProbeFailed` on invalid JSON or if no stream is decodable.
pub(crate) fn parse_audio_probe_json(raw: &[u8]) -> Result<AudioProbe, MediaError> {
    let info: Value = serde_json::from_slice(raw)
        .map_err(|err| MediaError::ProbeFailed(format!("invalid ffprobe JSON output: {err}")))?;
    let streams = info
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| MediaError::ProbeFailed("no decodable audio stream found".to_string()))?;

    let format = info.get("format");
    let format_duration = format.and_then(|fmt| parse_positive_f64(fmt.get("duration")));
    let title = extract_format_title(format);

    for stream in streams {
        let Ok(stream_index) = parse_stream_index(stream) else {
            continue;
        };
        let Some(codec_name) = stream
            .get("codec_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let Some(channels) = parse_positive_u32(stream.get("channels")) else {
            continue;
        };
        let Some(sample_rate) = parse_positive_u32(stream.get("sample_rate")) else {
            continue;
        };
        let duration_secs = parse_positive_f64(stream.get("duration")).or(format_duration);

        return Ok(AudioProbe {
            stream_index,
            codec_name: codec_name.to_string(),
            channels,
            sample_rate,
            duration_secs,
            title,
        });
    }

    Err(MediaError::ProbeFailed(
        "no decodable audio stream found".to_string(),
    ))
}

/// Extracts a non-negative container stream index from an `ffprobe` stream object.
///
/// Accepts a JSON stream object. Returns the `usize` stream index, or
/// `MediaError::ProbeFailed` if `index` is missing, negative, or overflows
/// `usize`.
fn parse_stream_index(stream: &Value) -> Result<usize, MediaError> {
    let raw = stream.get("index").and_then(Value::as_u64).ok_or_else(|| {
        MediaError::ProbeFailed("ffprobe stream entry is missing integer index".to_string())
    })?;
    usize::try_from(raw).map_err(|_| {
        MediaError::ProbeFailed("ffprobe stream index exceeds usize range".to_string())
    })
}

/// Resolves a positive finite video duration from stream and container fields.
///
/// Accepts the selected stream JSON object and optional container `format`
/// JSON object. Tries `stream.duration`, then `stream.duration_ts * time_base`,
/// then `format.duration`. Returns `Some(seconds)` for the first finite value
/// strictly greater than zero, or `None` if all candidates are absent or
/// unusable.
fn resolve_video_duration(stream: &Value, format: Option<&Value>) -> Option<f64> {
    if let Some(secs) = parse_positive_f64(stream.get("duration")) {
        return Some(secs);
    }
    if let Some(secs) = parse_duration_ts(stream.get("duration_ts"), stream.get("time_base")) {
        return Some(secs);
    }
    format.and_then(|fmt| parse_positive_f64(fmt.get("duration")))
}

/// Computes duration in seconds from `ffprobe` `duration_ts` and fractional `time_base`.
///
/// Accepts optional JSON values for `duration_ts` (integer or numeric string)
/// and `time_base` (`"numerator/denominator"`). Returns `Some(seconds)` when
/// the denominator is non-zero and the resulting duration is positive and
/// finite; returns `None` otherwise.
fn parse_duration_ts(duration_ts: Option<&Value>, time_base: Option<&Value>) -> Option<f64> {
    let ts = parse_positive_f64(duration_ts)?;
    let tb = time_base?.as_str()?.trim();
    let (num_str, den_str) = tb.split_once('/')?;
    let numerator: f64 = num_str.trim().parse().ok()?;
    let denominator: f64 = den_str.trim().parse().ok()?;
    if !numerator.is_finite() || !denominator.is_finite() || denominator == 0.0 {
        return None;
    }
    let duration = ts * numerator / denominator;
    (duration.is_finite() && duration > 0.0).then_some(duration)
}

/// Parses a JSON number or numeric string into a strictly positive finite `f64`.
///
/// Accepts an optional JSON value. Returns `Some(value)` when the value parses
/// to a finite number greater than `0.0`, and `None` for missing, non-numeric,
/// zero, negative, or non-finite inputs.
fn parse_positive_f64(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(num) => num.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    (parsed.is_finite() && parsed > 0.0).then_some(parsed)
}

/// Parses a JSON unsigned integer or numeric string into a strictly positive `u32`.
///
/// Accepts an optional JSON value. Returns `Some(value)` when the value parses
/// to an integer in `1..=u32::MAX`, and `None` otherwise.
fn parse_positive_u32(value: Option<&Value>) -> Option<u32> {
    let parsed = match value? {
        Value::Number(num) => u32::try_from(num.as_u64()?).ok()?,
        Value::String(text) => text.trim().parse::<u32>().ok()?,
        _ => return None,
    };
    (parsed > 0).then_some(parsed)
}

/// Extracts an optional trimmed non-empty container title from `format.tags`.
///
/// Accepts an optional `format` JSON object. Looks for an exact `"title"` tag
/// first and falls back to any ASCII case-insensitive `"title"` key. Returns
/// `Some(title)` when a non-empty trimmed value exists, or `None` otherwise.
fn extract_format_title(format: Option<&Value>) -> Option<String> {
    let tags = format?.get("tags")?.as_object()?;
    let direct = tags
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(title) = direct {
        return Some(title.to_string());
    }
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("title"))
        .and_then(|(_, v)| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_motion_video_stream_with_direct_duration_and_title() {
        let json = br#"{
            "streams": [
                {
                    "index": 0,
                    "duration": "123.450000",
                    "duration_ts": 123450,
                    "time_base": "1/1000"
                }
            ],
            "format": {
                "duration": "124.000000",
                "tags": {
                    "title": "  Quarterly Review  "
                }
            }
        }"#;

        let probe = parse_video_probe_json(json).expect("valid video probe");
        assert_eq!(probe.stream_index, 0);
        assert!((probe.duration_secs - 123.45).abs() < 1e-9);
        assert_eq!(probe.title.as_deref(), Some("Quarterly Review"));
    }

    #[test]
    fn rejects_empty_video_streams() {
        let json = br#"{ "streams": [], "format": { "duration": "42.0" } }"#;
        let err = parse_video_probe_json(json).expect_err("empty streams must fail");
        assert!(matches!(err, MediaError::ProbeFailed(_)));
        assert!(err.to_string().contains("no motion video stream"));
    }

    #[test]
    fn resolves_video_duration_via_duration_ts_and_format_fallback() {
        let ts_json = br#"{
            "streams": [
                {
                    "index": 1,
                    "duration": "N/A",
                    "duration_ts": 900000,
                    "time_base": "1/90000"
                }
            ],
            "format": {
                "duration": "99.0",
                "tags": { "title": "   " }
            }
        }"#;

        let ts_probe = parse_video_probe_json(ts_json).expect("duration_ts fallback");
        assert_eq!(ts_probe.stream_index, 1);
        assert!((ts_probe.duration_secs - 10.0).abs() < 1e-9);
        assert_eq!(ts_probe.title, None);

        let fmt_json = br#"{
            "streams": [
                {
                    "index": 2,
                    "time_base": "1/0"
                }
            ],
            "format": {
                "duration": "65.5",
                "tags": { "TITLE": "Field Clip" }
            }
        }"#;

        let fmt_probe = parse_video_probe_json(fmt_json).expect("format duration fallback");
        assert_eq!(fmt_probe.stream_index, 2);
        assert!((fmt_probe.duration_secs - 65.5).abs() < 1e-9);
        assert_eq!(fmt_probe.title.as_deref(), Some("Field Clip"));
    }

    #[test]
    fn rejects_missing_or_non_positive_video_duration() {
        let json = br#"{
            "streams": [{ "index": 0, "duration": "0.0" }],
            "format": { "duration": "-5.0" }
        }"#;
        let err = parse_video_probe_json(json).expect_err("non-positive duration must fail");
        assert!(matches!(err, MediaError::ProbeFailed(_)));
    }

    #[test]
    fn audio_probe_skips_unusable_stream_zero_and_selects_stream_one() {
        let json = br#"{
            "streams": [
                {
                    "index": 0,
                    "codec_name": "",
                    "channels": 0,
                    "sample_rate": "0"
                },
                {
                    "index": 1,
                    "codec_name": "aac",
                    "channels": 2,
                    "sample_rate": "48000",
                    "duration": "182.25"
                }
            ],
            "format": {
                "duration": "183.0",
                "tags": { "title": "Board Briefing" }
            }
        }"#;

        let probe = parse_audio_probe_json(json).expect("should pick stream 1");
        assert_eq!(probe.stream_index, 1);
        assert_eq!(probe.codec_name, "aac");
        assert_eq!(probe.channels, 2);
        assert_eq!(probe.sample_rate, 48_000);
        assert_eq!(probe.duration_secs, Some(182.25));
        assert_eq!(probe.title.as_deref(), Some("Board Briefing"));
    }

    #[test]
    fn audio_probe_skips_missing_codec_and_falls_back_to_format_duration() {
        let json = br#"{
            "streams": [
                {
                    "index": 0,
                    "channels": 2,
                    "sample_rate": 44100
                },
                {
                    "index": 2,
                    "codec_name": "opus",
                    "channels": 1,
                    "sample_rate": 16000
                }
            ],
            "format": {
                "duration": "45.0"
            }
        }"#;

        let probe = parse_audio_probe_json(json).expect("should pick stream 2");
        assert_eq!(probe.stream_index, 2);
        assert_eq!(probe.codec_name, "opus");
        assert_eq!(probe.channels, 1);
        assert_eq!(probe.sample_rate, 16_000);
        assert_eq!(probe.duration_secs, Some(45.0));
        assert_eq!(probe.title, None);
    }

    #[test]
    fn audio_probe_rejects_when_no_stream_is_decodable() {
        let json = br#"{
            "streams": [
                { "index": 0, "codec_name": "aac", "channels": 0, "sample_rate": "48000" },
                { "index": 1, "codec_name": "aac", "channels": 2, "sample_rate": "0" }
            ],
            "format": {}
        }"#;

        let err = parse_audio_probe_json(json).expect_err("no usable audio stream");
        assert!(matches!(err, MediaError::ProbeFailed(_)));
        assert!(err.to_string().contains("no decodable audio stream"));
    }
}

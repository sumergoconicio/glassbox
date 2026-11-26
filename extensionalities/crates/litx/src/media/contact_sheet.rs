//! Module: glassbox-cli in-memory video contact-sheet generator.
//! File: media/contact_sheet.rs.
//! Responsibility: Plan frame schedules, grab raw RGB tiles via FFmpeg pipes, composite contact sheets in memory, and encode JPEG or PNG output.

use super::ffprobe::probe_video_stream;
use super::{MediaError, ffmpeg_local_input_arg, resolve_ffmpeg, run_command_with_timeout};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, Rgb, RgbImage};
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Fixed number of columns in every generated contact sheet.
pub const COLUMNS: usize = 5;

/// Row count used by the default fixed 5x5 contact sheet.
pub const FIXED_ROWS: usize = 5;

/// Maximum row count used by adaptive scheduling above `LONG_SECONDS`.
pub const MAX_ADAPTIVE_ROWS: usize = 10;

/// Adaptive threshold below which a single row (5 tiles) is generated (15 minutes).
pub const SHORT_SECONDS: f64 = 900.0;

/// Adaptive threshold above which `MAX_ADAPTIVE_ROWS` (50 tiles) are generated (150 minutes).
pub const LONG_SECONDS: f64 = 9000.0;

/// Hard upper bound on tiles permitted in custom `--every-seconds` mode.
pub const MAX_CUSTOM_TILES: usize = 250;

/// Minimum allowed custom sampling interval (one microsecond).
const MIN_CUSTOM_INTERVAL_SECONDS: f64 = 0.000_001;

/// Hard per-video cap on concurrent FFmpeg frame-grab worker threads.
const MAX_WORKER_CONCURRENCY: usize = 4;

/// Timeout enforced on each individual FFmpeg frame-grab subprocess.
const FRAME_GRAB_TIMEOUT: Duration = Duration::from_secs(30);

/// JPEG encoder quality used for `ImageGridFormat::Jpg`.
const JPEG_QUALITY: u8 = 85;

/// Upper bound in seconds for exact integer microsecond scheduling without overflow.
const MAX_EXACT_MICROSECOND_SECONDS: f64 = 1.0e12;

/// Contact-sheet frame scheduling mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ImageGridMode {
    /// Fixed 5x5 grid sampling 25 evenly spaced midpoints.
    Default,
    /// Duration-scaled 5-column grid: 1 row below 15 min, 2-9 rows in 15-150 min, 10 rows above 150 min.
    Adaptive,
    /// Fixed-interval sampling starting at 0 s, capped at `MAX_CUSTOM_TILES`.
    Custom {
        /// Sampling interval in seconds (must be finite and >= 0.000001).
        every_seconds: f64,
    },
}

/// Resolution and spacing preset for contact-sheet cells and canvas width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageGridPreset {
    /// High-resolution preset: 360x202 cells, 24 px gutter, 12 px margin, 1,920 px sheet width.
    Quality,
    /// Compact preset: 240x134 cells, 16 px gutter, 8 px margin, 1,280 px sheet width.
    Performance,
}

impl ImageGridPreset {
    /// Returns `(cell_width_px, cell_height_px, gutter_px, margin_px)` for this preset.
    ///
    /// Accepts `self`. Returns the pixel geometry tuple matching the ADR-001
    /// Quality (1,920 px) or Performance (1,280 px) layout. This method cannot
    /// fail.
    fn layout(self) -> (u32, u32, u32, u32) {
        match self {
            Self::Quality => (360, 202, 24, 12),
            Self::Performance => (240, 134, 16, 8),
        }
    }
}

/// Output image encoding format for a generated contact sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageGridFormat {
    /// JPEG encoded at quality 85.
    Jpg,
    /// Lossless PNG.
    Png,
}

/// Deterministic layout geometry and sample timestamp plan for a contact sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct ContactSheetPlan {
    /// Number of tile rows in the sheet.
    pub rows: usize,
    /// Number of video frames to sample and place.
    pub tiles: usize,
    /// Final composited sheet width in pixels.
    pub width_px: u32,
    /// Final composited sheet height in pixels.
    pub height_px: u32,
    /// Individual tile cell width in pixels.
    pub cell_width_px: u32,
    /// Individual tile cell height in pixels.
    pub cell_height_px: u32,
    /// White spacing between adjacent tile cells in pixels.
    pub gutter_px: u32,
    /// Outer white border margin around the tile grid in pixels.
    pub margin_px: u32,
    /// Chronological target seek timestamps in seconds.
    pub timestamps: Vec<f64>,
}

/// Completed in-memory contact-sheet image bytes and structured generation metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct ContactSheetOutput {
    /// Complete encoded JPEG or PNG image bytes (never written to temp files).
    pub bytes: Vec<u8>,
    /// Final composited sheet width in pixels.
    pub width_px: u32,
    /// Final composited sheet height in pixels.
    pub height_px: u32,
    /// Number of tile rows in the sheet.
    pub rows: u32,
    /// Number of tile columns in the sheet (always `COLUMNS`).
    pub columns: u32,
    /// Number of tiles populated in the sheet.
    pub tiles: u32,
    /// Individual tile cell width in pixels.
    pub cell_width_px: u32,
    /// Individual tile cell height in pixels.
    pub cell_height_px: u32,
    /// Probed source video duration in seconds.
    pub duration_seconds: f64,
    /// Effective seek timestamp in seconds for each populated tile (rounded to 6 decimals).
    pub seek_seconds: Vec<f64>,
    /// 1-based tile indices whose frame reused the previous decoded tile.
    pub reused_tiles: Vec<u32>,
    /// 1-based tile indices that required slower linear output seeking.
    pub linear_seek_tiles: Vec<u32>,
    /// Optional container title tag extracted by `ffprobe`.
    pub title: Option<String>,
}

/// Outcome of a single worker's attempt to decode one tile at its scheduled timestamp.
#[derive(Debug, Clone)]
enum TileGrabOutcome {
    /// Frame decoded successfully into an exact `rgb24` byte buffer.
    Decoded {
        /// Raw `rgb24` bytes (`cell_width_px * cell_height_px * 3`).
        rgb: Vec<u8>,
        /// Effective seek timestamp in seconds.
        seek_seconds: f64,
        /// Whether linear output seeking (`-i ... -ss ...`) was required.
        used_linear_seek: bool,
    },
    /// No frame could be decoded at or after the requested timestamp (eligible for previous-tile reuse).
    Missing,
}

/// Resolved per-tile RGB frames and fallback disclosures ready for compositing.
#[derive(Debug, Clone, PartialEq)]
struct ResolvedTiles {
    /// Ordered raw `rgb24` buffers, one per planned tile.
    frames: Vec<Vec<u8>>,
    /// Effective seek timestamps rounded to 6 decimal places.
    seek_seconds: Vec<f64>,
    /// Sorted 1-based indices of tiles populated by carrying forward the previous frame.
    reused_tiles: Vec<u32>,
    /// Sorted 1-based indices of tiles that fell back to linear output seeking.
    linear_seek_tiles: Vec<u32>,
}

/// Computes the adaptive row count for a video duration in seconds.
///
/// Accepts a positive finite video duration in seconds. Returns `1` below
/// 900 s, `10` above 9,000 s, and `min(9, 2 + floor((d - 900) / 8100 * 8))`
/// across `[900.0, 9000.0]`. This helper cannot fail.
pub fn adaptive_rows(duration_secs: f64) -> usize {
    if duration_secs < SHORT_SECONDS {
        return 1;
    }
    if duration_secs > LONG_SECONDS {
        return MAX_ADAPTIVE_ROWS;
    }
    let progress = (duration_secs - SHORT_SECONDS) / (LONG_SECONDS - SHORT_SECONDS);
    let step = (progress * (MAX_ADAPTIVE_ROWS - 2) as f64).floor() as usize;
    (2 + step).min(MAX_ADAPTIVE_ROWS - 1)
}

/// Plans contact-sheet dimensions, tile counts, and seek timestamps for a duration.
///
/// Accepts a positive finite video duration in seconds, an `ImageGridMode`,
/// and an `ImageGridPreset`. Returns a `ContactSheetPlan` with exact canvas
/// dimensions and chronological timestamps. Returns `MediaError::InvalidInput`
/// if `duration_secs` is non-positive/non-finite, if a custom interval is
/// non-finite or below 1 microsecond, or if custom scheduling exceeds
/// `MAX_CUSTOM_TILES` (`250`).
pub fn plan_contact_sheet(
    duration_secs: f64,
    mode: ImageGridMode,
    preset: ImageGridPreset,
) -> Result<ContactSheetPlan, MediaError> {
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Err(MediaError::InvalidInput(
            "a positive finite video duration is required for contact-sheet planning".to_string(),
        ));
    }

    let (rows, tiles, timestamps) = match mode {
        ImageGridMode::Default => {
            let rows = FIXED_ROWS;
            let tiles = COLUMNS * rows;
            let timestamps = midpoint_timestamps(duration_secs, tiles);
            (rows, tiles, timestamps)
        }
        ImageGridMode::Adaptive => {
            let rows = adaptive_rows(duration_secs);
            let tiles = COLUMNS * rows;
            let timestamps = midpoint_timestamps(duration_secs, tiles);
            (rows, tiles, timestamps)
        }
        ImageGridMode::Custom { every_seconds } => {
            let timestamps = custom_timestamps(duration_secs, every_seconds)?;
            let tiles = timestamps.len();
            let rows = tiles.div_ceil(COLUMNS);
            (rows, tiles, timestamps)
        }
    };

    let (cell_width_px, cell_height_px, gutter_px, margin_px) = preset.layout();
    let cols_u32 = COLUMNS as u32;
    let rows_u32 = rows as u32;
    let width_px = cols_u32 * (cell_width_px + gutter_px) + 2 * margin_px - gutter_px;
    let height_px = rows_u32 * (cell_height_px + gutter_px) + 2 * margin_px - gutter_px;

    Ok(ContactSheetPlan {
        rows,
        tiles,
        width_px,
        height_px,
        cell_width_px,
        cell_height_px,
        gutter_px,
        margin_px,
        timestamps,
    })
}

/// Generates a complete JPEG or PNG video contact sheet entirely in memory.
///
/// Accepts a local video path, scheduling `mode`, resolution `preset`, output
/// `format`, and requested `max_concurrency` (clamped to `1..=4`). Probes the
/// first motion video stream via `ffprobe`, grabs raw `rgb24` tiles via piped
/// FFmpeg workers with linear-seek and previous-tile fallbacks, composites onto
/// a white `RgbImage`, and encodes JPEG (quality 85) or PNG bytes without
/// creating any temporary files or directories on disk. Returns
/// `MediaError::InvalidInput`, `MediaError::MissingHelper`,
/// `MediaError::ProbeFailed`, `MediaError::DecodeFailed`,
/// `MediaError::Timeout`, `MediaError::Io`, or `MediaError::EncodeFailed`.
pub fn generate_contact_sheet(
    source: &Path,
    mode: ImageGridMode,
    preset: ImageGridPreset,
    format: ImageGridFormat,
    max_concurrency: usize,
) -> Result<ContactSheetOutput, MediaError> {
    if let ImageGridMode::Custom { every_seconds } = mode {
        validate_custom_interval(every_seconds)?;
    }

    let probe = probe_video_stream(source)?;
    let plan = plan_contact_sheet(probe.duration_secs, mode, preset)?;
    let ffmpeg = resolve_ffmpeg()?;
    let filter = thumbnail_filter(plan.cell_width_px, plan.cell_height_px);
    let expected_bytes = (plan.cell_width_px as usize) * (plan.cell_height_px as usize) * 3;
    let workers = max_concurrency
        .clamp(1, MAX_WORKER_CONCURRENCY)
        .min(plan.tiles.max(1));

    let raw_outcomes = grab_tiles_concurrent(
        &ffmpeg,
        source,
        probe.stream_index,
        &plan.timestamps,
        &filter,
        expected_bytes,
        workers,
    )?;

    let resolved = resolve_tile_fallbacks(raw_outcomes)?;
    let canvas = composite_contact_sheet(&plan, &resolved.frames)?;
    let bytes = encode_contact_sheet(&canvas, format)?;

    Ok(ContactSheetOutput {
        bytes,
        width_px: plan.width_px,
        height_px: plan.height_px,
        rows: plan.rows as u32,
        columns: COLUMNS as u32,
        tiles: plan.tiles as u32,
        cell_width_px: plan.cell_width_px,
        cell_height_px: plan.cell_height_px,
        duration_seconds: probe.duration_secs,
        seek_seconds: resolved.seek_seconds,
        reused_tiles: resolved.reused_tiles,
        linear_seek_tiles: resolved.linear_seek_tiles,
        title: probe.title,
    })
}

/// Validates a custom `--every-seconds` interval value.
///
/// Accepts the interval in seconds. Returns `Ok(())` when finite and at least
/// `0.000001` seconds, or `MediaError::InvalidInput` otherwise.
fn validate_custom_interval(every_seconds: f64) -> Result<(), MediaError> {
    if !every_seconds.is_finite() || every_seconds < MIN_CUSTOM_INTERVAL_SECONDS {
        return Err(MediaError::InvalidInput(
            "the screenshot interval must be a finite number of at least 0.000001 seconds"
                .to_string(),
        ));
    }
    Ok(())
}

/// Computes midpoint timestamps `(i + 0.5) / count * duration_secs`.
///
/// Accepts positive duration in seconds and tile count. Returns `count`
/// monotonically increasing timestamps inside `(0, duration_secs)`. This
/// helper cannot fail.
fn midpoint_timestamps(duration_secs: f64, count: usize) -> Vec<f64> {
    (0..count)
        .map(|i| ((i as f64) + 0.5) / (count as f64) * duration_secs)
        .collect()
}

/// Computes custom-interval timestamps using integer microsecond ceiling math.
///
/// Accepts positive finite `duration_secs` and `every_seconds`. Uses integer
/// microsecond `ceil(duration_us / step_us)` to prevent floating-point drift,
/// enforces `MAX_CUSTOM_TILES` (`250`), and returns timestamps starting at
/// `0.0`. Returns `MediaError::InvalidInput` if the interval is invalid or the
/// resulting tile count exceeds `MAX_CUSTOM_TILES`.
fn custom_timestamps(duration_secs: f64, every_seconds: f64) -> Result<Vec<f64>, MediaError> {
    validate_custom_interval(every_seconds)?;

    if duration_secs <= MAX_EXACT_MICROSECOND_SECONDS
        && every_seconds <= MAX_EXACT_MICROSECOND_SECONDS
    {
        let duration_us = ((duration_secs * 1_000_000.0).round() as u128).max(1);
        let step_us = (every_seconds * 1_000_000.0).round() as u128;
        if step_us == 0 {
            return Err(MediaError::InvalidInput(
                "the screenshot interval must be at least 0.000001 seconds".to_string(),
            ));
        }
        let count_u128 = duration_us.div_ceil(step_us);
        if count_u128 > MAX_CUSTOM_TILES as u128 {
            return Err(MediaError::InvalidInput(format!(
                "custom interval of {every_seconds}s over {duration_secs:.3}s requires {count_u128} tiles, exceeding the maximum of {MAX_CUSTOM_TILES} tiles"
            )));
        }
        let count = count_u128 as usize;
        let timestamps = (0..count)
            .map(|i| ((i as u128) * step_us) as f64 / 1_000_000.0)
            .collect();
        return Ok(timestamps);
    }

    if duration_secs <= every_seconds {
        return Ok(vec![0.0]);
    }
    let ratio = (duration_secs / every_seconds).ceil();
    if !ratio.is_finite() || ratio > MAX_CUSTOM_TILES as f64 {
        return Err(MediaError::InvalidInput(format!(
            "custom interval of {every_seconds}s over {duration_secs:.3}s exceeds the maximum of {MAX_CUSTOM_TILES} tiles"
        )));
    }
    let count = ratio as usize;
    Ok((0..count).map(|i| (i as f64) * every_seconds).collect())
}

/// Builds the aspect-preserving FFmpeg scale and black-letterbox filter graph.
///
/// Accepts target cell width and height in pixels. Returns an FFmpeg `-vf`
/// filter expression that scales preserving display aspect ratio (`dar`),
/// resets sample aspect ratio to `1`, and pads with black to `width x height`.
/// This helper cannot fail.
fn thumbnail_filter(width: u32, height: u32) -> String {
    format!(
        "scale=w='max(2,if(gte(dar,{width}/{height}),{width},trunc({height}*dar/2)*2))':h='max(2,if(gte(dar,{width}/{height}),trunc({width}/dar/2)*2,{height}))',setsar=1,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black"
    )
}

/// Grabs all scheduled tiles across up to `workers` scoped OS threads.
///
/// Accepts the resolved `ffmpeg` path, source video path, container video
/// stream index, target timestamps, FFmpeg filter graph, expected `rgb24` byte
/// count per tile, and bounded worker count. Returns one `TileGrabOutcome` per
/// timestamp in original chronological order. Returns the first fatal
/// `MediaError` encountered by any worker and signals remaining workers to stop.
fn grab_tiles_concurrent(
    ffmpeg: &Path,
    source: &Path,
    stream_index: usize,
    timestamps: &[f64],
    filter: &str,
    expected_bytes: usize,
    workers: usize,
) -> Result<Vec<TileGrabOutcome>, MediaError> {
    let total = timestamps.len();
    let next_index = AtomicUsize::new(0);
    let aborted = AtomicBool::new(false);
    let slots: Mutex<Vec<Option<Result<TileGrabOutcome, MediaError>>>> =
        Mutex::new((0..total).map(|_| None).collect());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    if aborted.load(Ordering::Acquire) {
                        break;
                    }
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= total {
                        break;
                    }
                    let outcome = grab_single_tile(
                        ffmpeg,
                        source,
                        stream_index,
                        index,
                        timestamps[index],
                        filter,
                        expected_bytes,
                    );
                    if outcome.is_err() {
                        aborted.store(true, Ordering::Release);
                    }
                    let mut guard = slots.lock().unwrap_or_else(|poison| poison.into_inner());
                    guard[index] = Some(outcome);
                }
            });
        }
    });

    let mut guard = slots
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut outcomes = Vec::with_capacity(total);
    for (index, slot) in guard.drain(..).enumerate() {
        let entry = slot.ok_or_else(|| {
            MediaError::DecodeFailed(format!(
                "tile {} was not populated before worker shutdown",
                index + 1
            ))
        })?;
        outcomes.push(entry?);
    }
    Ok(outcomes)
}

/// Attempts fast seek, linear output seek, and tile-0 `0.0` fallback for one tile.
///
/// Accepts the resolved `ffmpeg` path, source video path, stream index, zero-based
/// tile index, target timestamp, filter graph, and expected byte count. Returns
/// `TileGrabOutcome::Decoded` on success, `TileGrabOutcome::Missing` for tile
/// `> 0` when both fast and linear seeks produce empty/short output, or a fatal
/// `MediaError` if FFmpeg fails, times out, or cannot decode tile `0` even at
/// `0.0`.
fn grab_single_tile(
    ffmpeg: &Path,
    source: &Path,
    stream_index: usize,
    tile_index: usize,
    timestamp: f64,
    filter: &str,
    expected_bytes: usize,
) -> Result<TileGrabOutcome, MediaError> {
    if let Some(rgb) = invoke_ffmpeg_frame(
        ffmpeg,
        source,
        stream_index,
        timestamp,
        filter,
        expected_bytes,
        true,
    )? {
        return Ok(TileGrabOutcome::Decoded {
            rgb,
            seek_seconds: timestamp,
            used_linear_seek: false,
        });
    }

    if let Some(rgb) = invoke_ffmpeg_frame(
        ffmpeg,
        source,
        stream_index,
        timestamp,
        filter,
        expected_bytes,
        false,
    )? {
        return Ok(TileGrabOutcome::Decoded {
            rgb,
            seek_seconds: timestamp,
            used_linear_seek: true,
        });
    }

    if tile_index == 0 {
        if let Some(rgb) = invoke_ffmpeg_frame(
            ffmpeg,
            source,
            stream_index,
            0.0,
            filter,
            expected_bytes,
            true,
        )? {
            return Ok(TileGrabOutcome::Decoded {
                rgb,
                seek_seconds: 0.0,
                used_linear_seek: false,
            });
        }
        return Err(MediaError::DecodeFailed(
            "could not decode a frame from the selected video stream".to_string(),
        ));
    }

    Ok(TileGrabOutcome::Missing)
}

/// Runs one FFmpeg subprocess to decode a single `rgb24` frame onto stdout.
///
/// Accepts the `ffmpeg` binary path, local source path, stream index, seek
/// timestamp in seconds, `-vf` filter graph, expected `rgb24` byte count, and
/// seek mode (`fast_input_seek = true` places `-ss` before `-i`; `false` places
/// `-ss` after `-i`). Validates the source via [`ffmpeg_local_input_arg`] and
/// enforces `-protocol_whitelist file` before `-i` on both fast and linear seek
/// paths. Returns `Ok(Some(bytes))` when FFmpeg exits `0` with exactly
/// `expected_bytes`, `Ok(None)` when FFmpeg exits `0` with empty or short
/// output, or `Err(MediaError)` on invalid input, non-zero exit, oversized
/// output, timeout, or I/O failure.
fn invoke_ffmpeg_frame(
    ffmpeg: &Path,
    source: &Path,
    stream_index: usize,
    timestamp: f64,
    filter: &str,
    expected_bytes: usize,
    fast_input_seek: bool,
) -> Result<Option<Vec<u8>>, MediaError> {
    let input_arg = ffmpeg_local_input_arg(source)?;
    let seek_arg = format!("{timestamp:.6}");
    let map_arg = format!("0:{stream_index}");
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-nostdin", "-v", "error", "-protocol_whitelist", "file"]);
    if fast_input_seek {
        cmd.args(["-ss", &seek_arg]);
    }
    cmd.args(["-threads", "2", "-i"]).arg(&input_arg);
    if !fast_input_seek {
        cmd.args(["-ss", &seek_arg]);
    }
    cmd.args([
        "-map",
        &map_arg,
        "-an",
        "-sn",
        "-dn",
        "-filter_threads",
        "1",
        "-vf",
        filter,
        "-frames:v",
        "1",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "pipe:1",
    ]);

    let output = run_command_with_timeout(cmd, FRAME_GRAB_TIMEOUT)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {:?}", output.status.code())
        } else {
            stderr
        };
        return Err(MediaError::DecodeFailed(format!(
            "ffmpeg frame grab failed at {timestamp:.6}s: {detail}"
        )));
    }

    if output.stdout.len() == expected_bytes {
        Ok(Some(output.stdout))
    } else if output.stdout.len() < expected_bytes {
        Ok(None)
    } else {
        Err(MediaError::DecodeFailed(format!(
            "ffmpeg returned {} raw rgb24 bytes at {timestamp:.6}s, expected {expected_bytes}",
            output.stdout.len()
        )))
    }
}

/// Applies deterministic previous-tile reuse and collects 1-based fallback indices.
///
/// Accepts chronological per-tile `TileGrabOutcome` results. Carries forward
/// the preceding tile's RGB buffer and seek time for any `Missing` tile at
/// index `> 0`, records 1-based indices in `reused_tiles` and
/// `linear_seek_tiles`, and rounds seek timestamps to 6 decimal places. Returns
/// `MediaError::DecodeFailed` if the outcomes list is empty or tile `0` is
/// `Missing`.
fn resolve_tile_fallbacks(outcomes: Vec<TileGrabOutcome>) -> Result<ResolvedTiles, MediaError> {
    if outcomes.is_empty() {
        return Err(MediaError::DecodeFailed(
            "contact sheet requires at least one tile".to_string(),
        ));
    }

    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(outcomes.len());
    let mut seek_seconds: Vec<f64> = Vec::with_capacity(outcomes.len());
    let mut reused_tiles: Vec<u32> = Vec::new();
    let mut linear_seek_tiles: Vec<u32> = Vec::new();

    for (index, outcome) in outcomes.into_iter().enumerate() {
        let one_based = (index + 1) as u32;
        match outcome {
            TileGrabOutcome::Decoded {
                rgb,
                seek_seconds: seek,
                used_linear_seek,
            } => {
                frames.push(rgb);
                seek_seconds.push(round_six_decimals(seek));
                if used_linear_seek {
                    linear_seek_tiles.push(one_based);
                }
            }
            TileGrabOutcome::Missing => {
                if index == 0 {
                    return Err(MediaError::DecodeFailed(
                        "could not decode a frame from the selected video stream".to_string(),
                    ));
                }
                let prev_frame = frames[index - 1].clone();
                let prev_seek = seek_seconds[index - 1];
                frames.push(prev_frame);
                seek_seconds.push(prev_seek);
                reused_tiles.push(one_based);
            }
        }
    }

    Ok(ResolvedTiles {
        frames,
        seek_seconds,
        reused_tiles,
        linear_seek_tiles,
    })
}

/// Rounds a floating-point timestamp to six decimal places.
///
/// Accepts a timestamp in seconds. Returns `0.0` for non-finite (`NaN` or
/// infinite) inputs, returns `value` unchanged if scaling by `1e6` would
/// overflow `f64`, and otherwise returns the timestamp rounded to microsecond
/// precision (`1e-6`). This helper cannot fail.
fn round_six_decimals(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    let scaled = (value * 1_000_000.0).round();
    if !scaled.is_finite() {
        return value;
    }
    scaled / 1_000_000.0
}

/// Composites raw `rgb24` tile buffers onto a white `RgbImage` canvas in memory.
///
/// Accepts a `ContactSheetPlan` and a slice of raw `rgb24` tile buffers (one
/// per planned tile). Places each tile at its exact row/column offset with
/// `margin_px` border and `gutter_px` spacing, leaving margins, gutters, and
/// any unused trailing cells pure white (`Rgb([255, 255, 255])`). Returns
/// `MediaError::EncodeFailed` if the tile count or any tile byte length does
/// not match `plan`.
fn composite_contact_sheet(
    plan: &ContactSheetPlan,
    tiles: &[Vec<u8>],
) -> Result<RgbImage, MediaError> {
    if tiles.len() != plan.tiles {
        return Err(MediaError::EncodeFailed(format!(
            "cannot composite {} tiles into a {}-tile plan",
            tiles.len(),
            plan.tiles
        )));
    }

    let cell_w = plan.cell_width_px as usize;
    let cell_h = plan.cell_height_px as usize;
    let expected_tile_bytes = cell_w * cell_h * 3;
    let canvas_stride = (plan.width_px as usize) * 3;
    let tile_stride = cell_w * 3;

    let mut canvas = RgbImage::from_pixel(plan.width_px, plan.height_px, Rgb([255, 255, 255]));
    let canvas_raw: &mut [u8] = canvas.as_mut();

    for (index, tile_rgb) in tiles.iter().enumerate() {
        if tile_rgb.len() != expected_tile_bytes {
            return Err(MediaError::EncodeFailed(format!(
                "tile {} has {} bytes, expected {}",
                index + 1,
                tile_rgb.len(),
                expected_tile_bytes
            )));
        }
        let row = (index / COLUMNS) as u32;
        let col = (index % COLUMNS) as u32;
        let dst_x = (plan.margin_px + col * (plan.cell_width_px + plan.gutter_px)) as usize;
        let dst_y = (plan.margin_px + row * (plan.cell_height_px + plan.gutter_px)) as usize;

        for y in 0..cell_h {
            let src_start = y * tile_stride;
            let dst_start = (dst_y + y) * canvas_stride + dst_x * 3;
            canvas_raw[dst_start..dst_start + tile_stride]
                .copy_from_slice(&tile_rgb[src_start..src_start + tile_stride]);
        }
    }

    Ok(canvas)
}

/// Encodes a composited `RgbImage` into an in-memory JPEG (quality 85) or PNG buffer.
///
/// Accepts a reference to the composited `RgbImage` and target
/// `ImageGridFormat`. Returns the encoded image bytes without touching the
/// filesystem. Returns `MediaError::EncodeFailed` if the underlying encoder
/// rejects the image dimensions or fails to encode.
fn encode_contact_sheet(canvas: &RgbImage, format: ImageGridFormat) -> Result<Vec<u8>, MediaError> {
    let mut buf = Vec::new();
    match format {
        ImageGridFormat::Jpg => {
            let mut encoder = JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
            encoder
                .encode(
                    canvas.as_raw(),
                    canvas.width(),
                    canvas.height(),
                    ExtendedColorType::Rgb8,
                )
                .map_err(|err| {
                    MediaError::EncodeFailed(format!("failed to encode JPEG contact sheet: {err}"))
                })?;
        }
        ImageGridFormat::Png => {
            let encoder = PngEncoder::new(&mut buf);
            encoder
                .write_image(
                    canvas.as_raw(),
                    canvas.width(),
                    canvas.height(),
                    ExtendedColorType::Rgb8,
                )
                .map_err(|err| {
                    MediaError::EncodeFailed(format!("failed to encode PNG contact sheet: {err}"))
                })?;
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_rows_respects_all_duration_boundaries() {
        assert_eq!(adaptive_rows(1.0), 1);
        assert_eq!(adaptive_rows(899.9), 1);
        assert_eq!(adaptive_rows(900.0), 2);
        assert_eq!(adaptive_rows(1912.4), 2);
        assert_eq!(adaptive_rows(1912.5), 3);
        assert_eq!(adaptive_rows(4950.0), 6);
        assert_eq!(adaptive_rows(9000.0), 9);
        assert_eq!(adaptive_rows(9000.1), 10);
        assert_eq!(adaptive_rows(20_000.0), 10);
    }

    #[test]
    fn default_plan_computes_5x5_midpoints_and_preset_dimensions() {
        let quality = plan_contact_sheet(100.0, ImageGridMode::Default, ImageGridPreset::Quality)
            .expect("quality plan");
        assert_eq!(quality.rows, 5);
        assert_eq!(quality.tiles, 25);
        assert_eq!(quality.width_px, 1920);
        assert_eq!(quality.height_px, 5 * (202 + 24));
        assert_eq!(quality.cell_width_px, 360);
        assert_eq!(quality.cell_height_px, 202);
        assert_eq!(quality.gutter_px, 24);
        assert_eq!(quality.margin_px, 12);
        assert_eq!(quality.timestamps.len(), 25);
        assert!((quality.timestamps[0] - 2.0).abs() < 1e-9);
        assert!((quality.timestamps[24] - 98.0).abs() < 1e-9);

        let perf = plan_contact_sheet(100.0, ImageGridMode::Default, ImageGridPreset::Performance)
            .expect("performance plan");
        assert_eq!(perf.width_px, 1280);
        assert_eq!(perf.height_px, 5 * (134 + 16));
        assert_eq!(perf.cell_width_px, 240);
        assert_eq!(perf.cell_height_px, 134);
        assert_eq!(perf.gutter_px, 16);
        assert_eq!(perf.margin_px, 8);
    }

    #[test]
    fn custom_plan_uses_exact_microsecond_ceil_and_enforces_250_tile_cap() {
        // 2.1 / 0.3 overflows 7.0 in IEEE-754 float (7.000000000000001), so microsecond
        // integer ceil must produce exactly 7 tiles (2 rows), not 8.
        let exact = plan_contact_sheet(
            2.1,
            ImageGridMode::Custom { every_seconds: 0.3 },
            ImageGridPreset::Performance,
        )
        .expect("exact microsecond ceil");
        assert_eq!(exact.tiles, 7);
        assert_eq!(exact.rows, 2);
        assert!((exact.timestamps[0] - 0.0).abs() < 1e-12);
        assert!((exact.timestamps[6] - 1.8).abs() < 1e-12);

        // Exact 250-tile cap is accepted.
        let at_cap = plan_contact_sheet(
            250.0,
            ImageGridMode::Custom { every_seconds: 1.0 },
            ImageGridPreset::Performance,
        )
        .expect("250 tiles must succeed");
        assert_eq!(at_cap.tiles, 250);
        assert_eq!(at_cap.rows, 50);

        // 251 tiles is rejected with InvalidInput.
        let over_cap = plan_contact_sheet(
            250.000_001,
            ImageGridMode::Custom { every_seconds: 1.0 },
            ImageGridPreset::Performance,
        )
        .expect_err("251 tiles must fail");
        assert!(matches!(over_cap, MediaError::InvalidInput(_)));
        assert!(over_cap.to_string().contains("250"));

        // Invalid intervals are rejected.
        for bad in [0.0, -1.0, 0.000_000_5, f64::NAN, f64::INFINITY] {
            let err = plan_contact_sheet(
                10.0,
                ImageGridMode::Custom { every_seconds: bad },
                ImageGridPreset::Quality,
            )
            .expect_err("invalid interval must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)));
        }
    }

    #[test]
    fn compositor_places_tiles_with_exact_offsets_and_white_trailing_cells() {
        let plan = plan_contact_sheet(
            7.0,
            ImageGridMode::Custom { every_seconds: 1.0 },
            ImageGridPreset::Performance,
        )
        .expect("7-tile plan");
        assert_eq!(plan.tiles, 7);
        assert_eq!(plan.rows, 2);
        assert_eq!(plan.width_px, 1280);
        assert_eq!(plan.height_px, 300);

        let pixel_count = (plan.cell_width_px as usize) * (plan.cell_height_px as usize);
        let tiles: Vec<Vec<u8>> = (0..plan.tiles)
            .map(|i| {
                let color = [(i as u8) * 30 + 10, 40, 200 - (i as u8) * 20];
                let mut rgb = Vec::with_capacity(pixel_count * 3);
                for _ in 0..pixel_count {
                    rgb.extend_from_slice(&color);
                }
                rgb
            })
            .collect();

        let canvas = composite_contact_sheet(&plan, &tiles).expect("composite canvas");
        assert_eq!(canvas.width(), 1280);
        assert_eq!(canvas.height(), 300);

        // Top-left outer margin is white.
        assert_eq!(*canvas.get_pixel(0, 0), Rgb([255, 255, 255]));
        assert_eq!(*canvas.get_pixel(7, 7), Rgb([255, 255, 255]));

        // Tile 0 starts at (margin=8, margin=8).
        assert_eq!(*canvas.get_pixel(8, 8), Rgb([10, 40, 200]));

        // Gutter between tile 0 (x in 8..248) and tile 1 (x in 264..504) is white.
        assert_eq!(*canvas.get_pixel(248, 8), Rgb([255, 255, 255]));
        assert_eq!(*canvas.get_pixel(263, 8), Rgb([255, 255, 255]));
        assert_eq!(*canvas.get_pixel(264, 8), Rgb([40, 40, 180]));

        // Tile 6 sits at row 1, col 1 -> x = 8 + 256 = 264, y = 8 + 150 = 158.
        assert_eq!(*canvas.get_pixel(264, 158), Rgb([190, 40, 80]));

        // Unused trailing cell (row 1, col 2 -> x = 8 + 2 * 256 = 520, y = 158) stays white.
        assert_eq!(*canvas.get_pixel(520, 158), Rgb([255, 255, 255]));

        // JPEG magic bytes (`FF D8 FF`).
        let jpg = encode_contact_sheet(&canvas, ImageGridFormat::Jpg).expect("encode jpeg");
        assert!(jpg.len() > 100);
        assert_eq!(&jpg[0..3], &[0xFF, 0xD8, 0xFF]);

        // PNG magic bytes (`89 50 4E 47 0D 0A 1A 0A`).
        let png = encode_contact_sheet(&canvas, ImageGridFormat::Png).expect("encode png");
        assert!(png.len() > 100);
        assert_eq!(
            &png[0..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );
    }

    #[test]
    fn resolve_tile_fallbacks_carries_forward_previous_tiles_and_records_indices() {
        let outcomes = vec![
            TileGrabOutcome::Decoded {
                rgb: vec![1, 2, 3],
                seek_seconds: 1.0,
                used_linear_seek: false,
            },
            TileGrabOutcome::Decoded {
                rgb: vec![4, 5, 6],
                seek_seconds: 2.123_456_7,
                used_linear_seek: true,
            },
            TileGrabOutcome::Missing,
            TileGrabOutcome::Missing,
        ];

        let resolved = resolve_tile_fallbacks(outcomes).expect("carry-forward should succeed");
        assert_eq!(resolved.frames.len(), 4);
        assert_eq!(resolved.frames[0], vec![1, 2, 3]);
        assert_eq!(resolved.frames[1], vec![4, 5, 6]);
        assert_eq!(resolved.frames[2], vec![4, 5, 6]);
        assert_eq!(resolved.frames[3], vec![4, 5, 6]);
        assert_eq!(
            resolved.seek_seconds,
            vec![1.0, 2.123457, 2.123457, 2.123457]
        );
        assert_eq!(resolved.linear_seek_tiles, vec![2]);
        assert_eq!(resolved.reused_tiles, vec![3, 4]);

        // Missing tile 0 cannot reuse anything and must fail.
        let err = resolve_tile_fallbacks(vec![TileGrabOutcome::Missing])
            .expect_err("missing tile 0 must fail");
        assert!(matches!(err, MediaError::DecodeFailed(_)));
    }

    #[test]
    fn round_six_decimals_guards_non_finite_values() {
        assert_eq!(round_six_decimals(f64::NAN), 0.0);
        assert_eq!(round_six_decimals(f64::INFINITY), 0.0);
        assert_eq!(round_six_decimals(f64::NEG_INFINITY), 0.0);
        assert_eq!(round_six_decimals(1.234_567_8), 1.234_568);
    }
}

//! Module: glassbox-cli local offline Whisper ONNX ASR backend.
//! File: media/asr_whisper.rs.
//! Responsibility: Resolve local Whisper ONNX weights and ONNX Runtime shared libraries, compute 80-bin Slaney log-Mel features, infer decoder KV-cache tensor geometry dynamically, and run warm greedy Whisper inference implementing `AsrBackend`.

#[cfg(all(feature = "whisper-onnx", feature = "magika"))]
compile_error!(
    "glassbox-cli features `whisper-onnx` and `magika` cannot be enabled together due to incompatible `ort` versions; disable default features when enabling `magika`"
);

use super::asr::{
    AsrBackend, AsrError, AudioWindow, SAMPLE_RATE_HZ, TranscriptChunk, WINDOW_SECONDS,
};
use crate::models::OnnxDevice;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[cfg(feature = "whisper-onnx")]
use ort::{
    environment::Environment,
    session::{Session, SessionInputValue},
    value::{DynValue, Tensor, ValueType},
};
#[cfg(feature = "whisper-onnx")]
use rustfft::{FftPlanner, num_complex::Complex32};
#[cfg(feature = "whisper-onnx")]
use serde::Deserialize;
#[cfg(feature = "whisper-onnx")]
use std::collections::HashSet;
#[cfg(feature = "whisper-onnx")]
use std::f32::consts::TAU;
#[cfg(feature = "whisper-onnx")]
use std::fs::File;
#[cfg(feature = "whisper-onnx")]
use std::io::Read;
#[cfg(feature = "whisper-onnx")]
use std::sync::{Arc, Mutex, OnceLock};

/// Environment variable overriding the ONNX Runtime dynamic library path.
pub const ORT_DYLIB_ENV_VAR: &str = "ORT_DYLIB_PATH";

/// Environment variable disabling ONNX Runtime's built-in 1DS telemetry client at library load.
pub const ORT_DISABLE_TELEMETRY_ENV_VAR: &str = "ORT_DISABLE_TELEMETRY";

/// Environment variable overriding LiteParse's cached model root directory.
pub const LITEPARSE_MODEL_DIR_ENV_VAR: &str = "LITEPARSE_MODEL_DIR";

/// Number of Slaney Mel filterbank bins expected by Whisper encoders.
pub const WHISPER_MELS: usize = 80;

/// Number of 10 ms STFT frames spanning a padded 30-second Whisper window.
pub const WHISPER_FRAMES: usize = 3_000;

/// STFT window and FFT length in samples (25 ms at 16 kHz).
pub const WHISPER_FFT_SIZE: usize = 400;

/// STFT hop stride in samples (10 ms at 16 kHz).
pub const WHISPER_HOP_LENGTH: usize = 160;

/// Number of non-negative FFT frequency bins (`WHISPER_FFT_SIZE / 2 + 1`).
pub const WHISPER_FFT_BINS: usize = WHISPER_FFT_SIZE / 2 + 1;

/// Number of 16 kHz PCM samples in a full 30-second Whisper input window.
pub const WHISPER_WINDOW_SAMPLES: usize = WINDOW_SECONDS * SAMPLE_RATE_HZ;

/// Default maximum token length used when `generation_config.json` omits `max_length`.
#[cfg(feature = "whisper-onnx")]
const DEFAULT_MAX_LENGTH: usize = 448;

/// Minimum permitted Whisper `generation_config.json` `max_length` (prompt plus at least one token).
#[cfg(feature = "whisper-onnx")]
const MIN_GENERATION_MAX_LENGTH: usize = 4;

/// Maximum permitted Whisper `generation_config.json` `max_length` bounding autoregressive decode steps.
#[cfg(feature = "whisper-onnx")]
const MAX_GENERATION_MAX_LENGTH: usize = 4096;

/// Maximum permitted `generation_config.json` file size in bytes (1 MiB).
#[cfg(feature = "whisper-onnx")]
const MAX_GENERATION_CONFIG_BYTES: u64 = 1024 * 1024;

/// Maximum permitted `vocab.json` file size in bytes (32 MiB).
#[cfg(feature = "whisper-onnx")]
const MAX_VOCAB_JSON_BYTES: u64 = 32 * 1024 * 1024;

/// Platform-specific ONNX Runtime shared-library filenames checked during fallback discovery.
#[cfg(target_os = "macos")]
const ORT_DYLIB_NAMES: &[&str] = &["libonnxruntime.dylib"];

/// Platform-specific ONNX Runtime shared-library filenames checked during fallback discovery.
#[cfg(windows)]
const ORT_DYLIB_NAMES: &[&str] = &["onnxruntime.dll"];

/// Platform-specific ONNX Runtime shared-library filenames checked during fallback discovery.
#[cfg(all(not(target_os = "macos"), not(windows)))]
const ORT_DYLIB_NAMES: &[&str] = &["libonnxruntime.so", "libonnxruntime.so.1"];

/// Computes the default ONNX Runtime intra-op thread count (`available_parallelism.clamp(1, 4)`).
///
/// Accepts no arguments. Returns a thread count between `1` and `4` inclusive,
/// falling back to `1` if the host parallelism cannot be queried. Cannot fail.
pub fn default_onnx_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .clamp(1, 4)
}

/// Ensures `ORT_DISABLE_TELEMETRY=1` is set in the process environment before `libonnxruntime` loads.
///
/// Accepts no arguments. Sets `ORT_DISABLE_TELEMETRY=1` when unset so
/// Microsoft's ONNX Runtime 1DS telemetry client does not initialize on `dlopen`,
/// write `.ses` files to `TMPDIR`, or race static teardown at process exit.
/// Must be called while single-threaded before spawning media batch workers or
/// loading ONNX Runtime. Returns unit and cannot fail.
pub fn ensure_ort_telemetry_disabled() {
    if std::env::var_os(ORT_DISABLE_TELEMETRY_ENV_VAR).is_none() {
        // SAFETY: Called once while single-threaded before spawning media batch
        // workers or loading `libonnxruntime`; nothing in glassbox-cli reads
        // `ORT_DISABLE_TELEMETRY` concurrently, and the only reader is the ONNX
        // Runtime shared library loaded afterwards.
        unsafe {
            std::env::set_var(ORT_DISABLE_TELEMETRY_ENV_VAR, "1");
        }
    }
}

/// Configuration for the local offline Whisper ONNX ASR backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperOnnxConfig {
    /// Local directory containing Whisper ONNX weights, `generation_config.json`, and `vocab.json`.
    pub model_dir: PathBuf,
    /// Requested ONNX Runtime execution device (`cpu` or `cuda`).
    pub device: OnnxDevice,
    /// Intra-op ONNX Runtime thread count (clamped to `>= 1` at session creation).
    pub threads: usize,
}

impl WhisperOnnxConfig {
    /// Creates a [`WhisperOnnxConfig`] with default intra-op thread count (`1..=4`).
    ///
    /// Accepts the local model directory path and desired [`OnnxDevice`].
    /// Returns a populated [`WhisperOnnxConfig`]. Performs no I/O and cannot fail.
    pub fn new(model_dir: PathBuf, device: OnnxDevice) -> Self {
        Self {
            model_dir,
            device,
            threads: default_onnx_threads(),
        }
    }
}

/// Resolved filesystem paths for a local Whisper ONNX model bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperModelAssets {
    /// Path to `encoder_model_int8.onnx` or `encoder_model.onnx`.
    pub encoder_path: PathBuf,
    /// Path to `decoder_model_merged_int8.onnx` or `decoder_model_merged.onnx`.
    pub decoder_path: PathBuf,
    /// Path to `generation_config.json`.
    pub generation_config_path: PathBuf,
    /// Path to `vocab.json`.
    pub vocab_path: PathBuf,
}

/// Resolves required Whisper ONNX model assets inside `model_dir`.
///
/// Accepts the local model directory path. Prefers `_int8.onnx` weights over
/// unquantized `.onnx` weights when both exist, checking both `model_dir/` and
/// Hugging Face Optimum's `model_dir/onnx/` subdirectory. Requires
/// `generation_config.json` and `vocab.json` in `model_dir`. Returns
/// [`WhisperModelAssets`] on success or [`AsrError::MissingAsset`] naming the
/// directory and every missing required file.
pub fn resolve_model_assets(model_dir: &Path) -> Result<WhisperModelAssets, AsrError> {
    if !model_dir.exists() {
        return Err(AsrError::MissingAsset(format!(
            "Whisper ONNX model directory does not exist: {}",
            model_dir.display()
        )));
    }
    if !model_dir.is_dir() {
        return Err(AsrError::MissingAsset(format!(
            "Whisper ONNX model path is not a directory: {}",
            model_dir.display()
        )));
    }

    let encoder_path = find_first_existing_file(
        model_dir,
        &[
            "encoder_model_int8.onnx",
            "encoder_model.onnx",
            "onnx/encoder_model_int8.onnx",
            "onnx/encoder_model.onnx",
        ],
    );
    let decoder_path = find_first_existing_file(
        model_dir,
        &[
            "decoder_model_merged_int8.onnx",
            "decoder_model_merged.onnx",
            "onnx/decoder_model_merged_int8.onnx",
            "onnx/decoder_model_merged.onnx",
        ],
    );
    let generation_config_path = find_first_existing_file(model_dir, &["generation_config.json"]);
    let vocab_path = find_first_existing_file(model_dir, &["vocab.json"]);

    let mut missing = Vec::new();
    if encoder_path.is_none() {
        missing.push("encoder_model_int8.onnx (or encoder_model.onnx)");
    }
    if decoder_path.is_none() {
        missing.push("decoder_model_merged_int8.onnx (or decoder_model_merged.onnx)");
    }
    if generation_config_path.is_none() {
        missing.push("generation_config.json");
    }
    if vocab_path.is_none() {
        missing.push("vocab.json");
    }

    if !missing.is_empty() {
        return Err(AsrError::MissingAsset(format!(
            "missing Whisper ONNX model asset(s) in {}: {}",
            model_dir.display(),
            missing.join(", ")
        )));
    }

    Ok(WhisperModelAssets {
        encoder_path: encoder_path.expect("checked missing list above"),
        decoder_path: decoder_path.expect("checked missing list above"),
        generation_config_path: generation_config_path.expect("checked missing list above"),
        vocab_path: vocab_path.expect("checked missing list above"),
    })
}

/// Returns the first candidate relative path under `root` that exists as a regular file.
///
/// Accepts a base directory and an ordered list of relative file paths.
/// Returns `Some(PathBuf)` for the first matching file, or `None` when none
/// exist. Performs metadata lookups only and cannot fail.
fn find_first_existing_file(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|rel| root.join(rel))
        .find(|candidate| candidate.is_file())
}

/// Resolves the ONNX Runtime dynamic library path from the live process environment and OS paths.
///
/// Accepts no arguments. Checks `ORT_DYLIB_PATH` first (failing closed if set to
/// a missing file), then LiteParse's cached `ocria-nano/runtime/` directory,
/// dynamic loader search paths (`LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH`), and
/// standard OS library directories. Returns the resolved library [`PathBuf`] or
/// [`AsrError::MissingAsset`] with an actionable `ORT_DYLIB_PATH` diagnostic.
pub fn resolve_ort_dylib() -> Result<PathBuf, AsrError> {
    let os_dirs = default_os_library_dirs();
    resolve_ort_dylib_with(|key| std::env::var(key).ok(), &os_dirs)
}

/// Resolves the ONNX Runtime dynamic library using a caller-supplied environment lookup and OS directories.
///
/// Accepts an environment lookup closure `env(name) -> Option<String>` and a
/// slice of fallback OS library directories. Enforces fail-closed semantics when
/// `ORT_DYLIB_PATH` is explicitly set to a missing or non-file path. Returns the
/// discovered library [`PathBuf`] or [`AsrError::MissingAsset`] naming
/// `ORT_DYLIB_PATH` and the LiteParse cache path.
pub fn resolve_ort_dylib_with<F>(env: F, os_search_dirs: &[PathBuf]) -> Result<PathBuf, AsrError>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(raw) = non_empty_env(&env, ORT_DYLIB_ENV_VAR) {
        let explicit = PathBuf::from(&raw);
        if explicit.is_file() {
            return Ok(explicit);
        }
        return Err(AsrError::MissingAsset(format!(
            "{ORT_DYLIB_ENV_VAR} is set to '{}' but that file does not exist or is not a regular file",
            explicit.display()
        )));
    }

    let mut search_dirs = liteparse_runtime_dirs(&env);
    search_dirs.extend(loader_library_dirs(&env));
    search_dirs.extend(os_search_dirs.iter().cloned());

    for dir in &search_dirs {
        for lib_name in ORT_DYLIB_NAMES {
            let candidate = dir.join(lib_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    let primary_lib = ORT_DYLIB_NAMES[0];
    Err(AsrError::MissingAsset(format!(
        "ONNX Runtime shared library ({primary_lib}) not found; set {ORT_DYLIB_ENV_VAR}=/path/to/{primary_lib} or install LiteParse's cached runtime under ~/.cache/liteparse/models/ocria-nano/runtime/{primary_lib} (for example via `glassbox parse models pull ocria-nano`)"
    )))
}

/// Reads an environment variable via `env`, returning `None` when unset or blank.
///
/// Accepts a lookup closure and environment variable name. Returns
/// `Some(String)` with surrounding whitespace trimmed only when non-empty.
/// Cannot fail.
fn non_empty_env<F>(env: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    env(key)
        .map(|val| val.trim().to_string())
        .filter(|val| !val.is_empty())
}

/// Builds LiteParse's cached `ocria-nano/runtime/` candidate directories from environment variables.
///
/// Accepts an environment lookup closure. Returns candidate directories matching
/// `crates/liteparse/src/ocr/ocria/mod.rs` (`$LITEPARSE_MODEL_DIR/ocria-nano/runtime`,
/// `$XDG_CACHE_HOME/liteparse/models/ocria-nano/runtime`, and
/// `$HOME/.cache/liteparse/models/ocria-nano/runtime` on Unix, or
/// `%LOCALAPPDATA%\liteparse\models\ocria-nano\runtime` on Windows). Cannot fail.
fn liteparse_runtime_dirs<F>(env: &F) -> Vec<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let mut dirs = Vec::new();
    if let Some(model_root) = non_empty_env(env, LITEPARSE_MODEL_DIR_ENV_VAR) {
        dirs.push(PathBuf::from(model_root).join("ocria-nano").join("runtime"));
    }
    #[cfg(windows)]
    {
        if let Some(local_app_data) = non_empty_env(env, "LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local_app_data)
                    .join("liteparse")
                    .join("models")
                    .join("ocria-nano")
                    .join("runtime"),
            );
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg_cache) = non_empty_env(env, "XDG_CACHE_HOME") {
            dirs.push(
                PathBuf::from(xdg_cache)
                    .join("liteparse")
                    .join("models")
                    .join("ocria-nano")
                    .join("runtime"),
            );
        }
        if let Some(home) = non_empty_env(env, "HOME") {
            dirs.push(
                PathBuf::from(home)
                    .join(".cache")
                    .join("liteparse")
                    .join("models")
                    .join("ocria-nano")
                    .join("runtime"),
            );
        }
    }
    dirs
}

/// Parses platform dynamic-loader library search paths (`LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH`).
///
/// Accepts an environment lookup closure. Returns non-empty directory entries
/// split with `std::env::split_paths`. Cannot fail.
fn loader_library_dirs<F>(env: &F) -> Vec<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let var_name = if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(windows) {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    };
    match non_empty_env(env, var_name) {
        Some(raw) => std::env::split_paths(&raw)
            .filter(|dir| !dir.as_os_str().is_empty())
            .collect(),
        None => Vec::new(),
    }
}

/// Returns standard platform library directories searched when `ORT_DYLIB_PATH` and LiteParse cache miss.
///
/// Accepts no arguments. Returns a vector of standard OS library directories
/// for Linux, macOS, or Windows. Performs no I/O and cannot fail.
fn default_os_library_dirs() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/opt/homebrew/lib"),
            PathBuf::from("/usr/local/lib"),
            PathBuf::from("/usr/lib"),
        ]
    }
    #[cfg(windows)]
    {
        Vec::new()
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        vec![
            PathBuf::from("/usr/local/lib"),
            PathBuf::from("/usr/lib"),
            PathBuf::from("/usr/lib64"),
            PathBuf::from("/usr/lib/x86_64-linux-gnu"),
            PathBuf::from("/usr/lib/aarch64-linux-gnu"),
            PathBuf::from("/lib/x86_64-linux-gnu"),
            PathBuf::from("/lib/aarch64-linux-gnu"),
        ]
    }
}

/// Dynamic KV-cache tensor geometry discovered from a merged Whisper decoder ONNX graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvCacheSpec {
    /// Number of transformer decoder layers (`0..num_layers`).
    pub num_layers: usize,
    /// Number of attention heads per layer (axis 1 of each KV tensor).
    pub num_heads: usize,
    /// Per-head embedding dimension (axis 3 of each KV tensor).
    pub head_dim: usize,
}

/// Infers Whisper decoder KV-cache geometry from `(input_name, shape)` metadata pairs.
///
/// Accepts a slice of `(name, shape)` tuples describing all ONNX decoder inputs
/// (negative dimensions represent symbolic/dynamic axes such as batch or past
/// sequence length). Parses every `past_key_values.{layer}.{decoder|encoder}.{key|value}`
/// tensor, verifies contiguous layers `0..num_layers`, verifies all four tensors
/// exist per layer with consistent fixed `num_heads` (axis 1) and `head_dim`
/// (axis 3), and returns [`KvCacheSpec`]. Returns [`AsrError::Decode`] when no
/// KV-cache inputs exist or the schema is incomplete, malformed, or inconsistent.
pub fn infer_kv_cache_spec(inputs: &[(&str, &[i64])]) -> Result<KvCacheSpec, AsrError> {
    const ALL_SLOTS_MASK: u8 = 0b1111;

    let mut layer_masks: BTreeMap<usize, u8> = BTreeMap::new();
    let mut expected_heads: Option<usize> = None;
    let mut expected_head_dim: Option<usize> = None;

    for &(name, shape) in inputs {
        let Some(rest) = name.strip_prefix("past_key_values.") else {
            continue;
        };
        let mut parts = rest.split('.');
        let (Some(layer_str), Some(attention), Some(kv), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(AsrError::Decode(format!(
                "malformed Whisper KV-cache input name '{name}': expected past_key_values.<layer>.<decoder|encoder>.<key|value>"
            )));
        };

        let layer_idx = layer_str.parse::<usize>().map_err(|_| {
            AsrError::Decode(format!(
                "malformed Whisper KV-cache layer index in input '{name}'"
            ))
        })?;
        let slot_bit = match (attention, kv) {
            ("decoder", "key") => 1u8 << 0,
            ("decoder", "value") => 1u8 << 1,
            ("encoder", "key") => 1u8 << 2,
            ("encoder", "value") => 1u8 << 3,
            _ => {
                return Err(AsrError::Decode(format!(
                    "unexpected Whisper KV-cache tensor role in input '{name}': expected decoder|encoder and key|value"
                )));
            }
        };

        if shape.len() != 4 {
            return Err(AsrError::Decode(format!(
                "Whisper KV-cache input '{name}' has rank {} (expected 4D [batch, heads, seq, head_dim])",
                shape.len()
            )));
        }
        let heads = usize::try_from(shape[1]).ok().filter(|&v| v > 0).ok_or_else(|| {
            AsrError::Decode(format!(
                "Whisper KV-cache input '{name}' has non-positive or dynamic head count {} at axis 1",
                shape[1]
            ))
        })?;
        let head_dim = usize::try_from(shape[3]).ok().filter(|&v| v > 0).ok_or_else(|| {
            AsrError::Decode(format!(
                "Whisper KV-cache input '{name}' has non-positive or dynamic head dimension {} at axis 3",
                shape[3]
            ))
        })?;

        if let Some(prev) = expected_heads {
            if prev != heads {
                return Err(AsrError::Decode(format!(
                    "inconsistent Whisper KV-cache head count on '{name}': got {heads}, expected {prev}"
                )));
            }
        } else {
            expected_heads = Some(heads);
        }

        if let Some(prev) = expected_head_dim {
            if prev != head_dim {
                return Err(AsrError::Decode(format!(
                    "inconsistent Whisper KV-cache head dimension on '{name}': got {head_dim}, expected {prev}"
                )));
            }
        } else {
            expected_head_dim = Some(head_dim);
        }

        let mask = layer_masks.entry(layer_idx).or_insert(0);
        if (*mask & slot_bit) != 0 {
            return Err(AsrError::Decode(format!(
                "duplicate Whisper KV-cache input '{name}'"
            )));
        }
        *mask |= slot_bit;
    }

    if layer_masks.is_empty() {
        return Err(AsrError::Decode(
            "Whisper decoder ONNX graph contains no `past_key_values.*` inputs; expected a merged decoder export (decoder_model_merged[_int8].onnx)".to_string(),
        ));
    }

    let num_layers = layer_masks.len();
    for expected_layer in 0..num_layers {
        let Some(&mask) = layer_masks.get(&expected_layer) else {
            return Err(AsrError::Decode(format!(
                "Whisper decoder KV-cache layers are non-contiguous: missing layer {expected_layer}"
            )));
        };
        if mask != ALL_SLOTS_MASK {
            return Err(AsrError::Decode(format!(
                "Whisper decoder KV-cache layer {expected_layer} is incomplete (expected decoder/encoder key and value tensors)"
            )));
        }
    }

    Ok(KvCacheSpec {
        num_layers,
        num_heads: expected_heads.expect("non-empty layer_masks guarantees expected_heads"),
        head_dim: expected_head_dim.expect("non-empty layer_masks guarantees expected_head_dim"),
    })
}

/// Inspects a loaded ONNX Runtime decoder [`Session`] and infers its [`KvCacheSpec`].
///
/// Accepts a reference to a loaded merged Whisper decoder [`Session`]. Extracts
/// tensor input names and shapes from `Session::inputs()` and delegates to
/// [`infer_kv_cache_spec`]. Returns [`KvCacheSpec`] or [`AsrError::Decode`] if
/// any KV-cache input is non-tensor, malformed, or inconsistent.
#[cfg(feature = "whisper-onnx")]
fn parse_kv_cache_spec_from_session(decoder: &Session) -> Result<KvCacheSpec, AsrError> {
    let input_shapes: Vec<(&str, &[i64])> = decoder
        .inputs()
        .iter()
        .map(|outlet| {
            let dims: &[i64] = match outlet.dtype() {
                ValueType::Tensor { shape, .. } => shape,
                _ => &[],
            };
            (outlet.name(), dims)
        })
        .collect();
    infer_kv_cache_spec(&input_shapes)
}

/// Builds a GPT-2 / Whisper byte-level BPE lookup table mapping token IDs to raw UTF-8 byte runs.
///
/// Accepts a vocabulary map from BPE symbol strings (as stored in `vocab.json`)
/// to numeric token IDs. Maps GPT-2's printable Unicode byte aliases back to
/// their original `0..=255` byte values and returns `HashMap<usize, Vec<u8>>`.
/// Tokens containing non-byte special markers are skipped cleanly. Cannot fail.
pub fn build_token_byte_table(vocab: &HashMap<String, usize>) -> HashMap<usize, Vec<u8>> {
    let mut aliases = HashMap::with_capacity(256);
    let mut extended = 256u32;
    for byte in 0u32..=255 {
        let codepoint = if matches!(byte, 33..=126 | 161..=172 | 174..=255) {
            byte
        } else {
            let cp = extended;
            extended += 1;
            cp
        };
        if let Some(ch) = char::from_u32(codepoint) {
            aliases.insert(ch, byte as u8);
        }
    }

    vocab
        .iter()
        .filter_map(|(token, &id)| {
            let bytes = token
                .chars()
                .map(|ch| aliases.get(&ch).copied())
                .collect::<Option<Vec<u8>>>()?;
            Some((id, bytes))
        })
        .collect()
}

/// Decodes a sequence of Whisper BPE token IDs into a UTF-8 string.
///
/// Accepts a slice of token IDs and a token-to-bytes table produced by
/// [`build_token_byte_table`]. Returns the concatenated UTF-8 string on success.
/// Returns [`AsrError::Decode`] if any token ID is absent from `table` or if
/// the concatenated byte sequence is not valid UTF-8.
pub fn decode_token_ids(
    ids: &[usize],
    table: &HashMap<usize, Vec<u8>>,
) -> Result<String, AsrError> {
    let mut bytes = Vec::new();
    for &id in ids {
        let piece = table.get(&id).ok_or_else(|| {
            AsrError::Decode(format!(
                "Whisper decoder produced unknown or non-text token ID {id}"
            ))
        })?;
        bytes.extend_from_slice(piece);
    }
    String::from_utf8(bytes).map_err(|err| {
        AsrError::Decode(format!(
            "Whisper token byte sequence is not valid UTF-8: {err}"
        ))
    })
}

/// Derives a concise model identifier label from the basename of `model_dir`.
///
/// Accepts a local model directory path. Returns the trimmed UTF-8 final path
/// component, falling back to `"whisper-onnx"` when the path has no usable
/// filename component. Cannot fail.
fn derive_model_id(model_dir: &Path) -> String {
    model_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("whisper-onnx")
        .to_string()
}

/// Whisper 80-bin Slaney log-Mel spectrogram feature extractor.
#[cfg(feature = "whisper-onnx")]
pub struct Features {
    /// Cached forward 400-point FFT plan.
    fft: Arc<dyn rustfft::Fft<f32>>,
    /// Periodic 400-sample Hann analysis window.
    window: Vec<f32>,
    /// Sparse area-normalized Slaney Mel triangular filterbank weights.
    filters: Vec<Vec<(usize, f32)>>,
}

#[cfg(feature = "whisper-onnx")]
impl Default for Features {
    /// Constructs a default Whisper [`Features`] extractor via [`Features::new`].
    ///
    /// Accepts no arguments. Returns a ready [`Features`] instance. Cannot fail.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "whisper-onnx")]
impl Features {
    /// Precomputes the 400-point FFT plan, Hann window, and 80-bin Slaney Mel filterbank.
    ///
    /// Accepts no arguments. Returns a reusable [`Features`] extractor matching
    /// OpenAI Whisper's reference STFT and Slaney Mel scaling. Cannot fail.
    pub fn new() -> Self {
        let f_sp = 200.0 / 3.0;
        let min_log_mel = 1000.0 / f_sp;
        let log_step = 6.4_f64.ln() / 27.0;
        let max_mel = min_log_mel + 8.0_f64.ln() / log_step;
        let edges: Vec<f64> = (0..WHISPER_MELS + 2)
            .map(|index| {
                let mel = max_mel * index as f64 / (WHISPER_MELS + 1) as f64;
                if mel < min_log_mel {
                    mel * f_sp
                } else {
                    1000.0 * (log_step * (mel - min_log_mel)).exp()
                }
            })
            .collect();
        let filters = edges
            .windows(3)
            .map(|edge| {
                let [lower, center, upper] = [edge[0], edge[1], edge[2]];
                (0..WHISPER_FFT_BINS)
                    .filter_map(|bin| {
                        let hz = (bin * SAMPLE_RATE_HZ / WHISPER_FFT_SIZE) as f64;
                        let weight =
                            ((hz - lower) / (center - lower)).min((upper - hz) / (upper - center));
                        (weight > 0.0).then_some((bin, (weight * 2.0 / (upper - lower)) as f32))
                    })
                    .collect()
            })
            .collect();
        let window = (0..WHISPER_FFT_SIZE)
            .map(|index| 0.5 - 0.5 * (TAU * index as f32 / WHISPER_FFT_SIZE as f32).cos())
            .collect();
        Self {
            fft: FftPlanner::new().plan_fft_forward(WHISPER_FFT_SIZE),
            window,
            filters,
        }
    }

    /// Extracts an `[80, 3000]` row-major normalized log-Mel spectrogram from 16 kHz mono PCM.
    ///
    /// Accepts `&self` and a slice of finite 16 kHz mono `f32` PCM samples (up
    /// to 30 seconds; shorter inputs are zero-padded with reflect edge handling
    /// at the 30-second boundary). Returns a `Vec<f32>` of length
    /// `WHISPER_MELS * WHISPER_FRAMES` (`240_000`) normalized with Whisper's
    /// 8-decade dynamic-range clamp `((x.max(max - 8.0)) + 4.0) / 4.0`. Cannot fail.
    pub fn log_mel(&self, pcm: &[f32]) -> Vec<f32> {
        let mut mel = vec![-10.0f32; WHISPER_MELS * WHISPER_FRAMES];
        let mut complex = vec![Complex32::default(); WHISPER_FFT_SIZE];
        let mut scratch = vec![Complex32::default(); self.fft.get_inplace_scratch_len()];
        let mut power = [0.0f32; WHISPER_FFT_BINS];
        let active = (pcm.len() + WHISPER_FFT_SIZE / 2)
            .div_ceil(WHISPER_HOP_LENGTH)
            .min(WHISPER_FRAMES);

        for frame in 0..active {
            for (index, bin) in complex.iter_mut().enumerate() {
                let position =
                    (frame * WHISPER_HOP_LENGTH + index) as isize - (WHISPER_FFT_SIZE / 2) as isize;
                // Centered STFT reflects the edges of the 30-second padded input.
                let sample = if position < 0 {
                    position.unsigned_abs()
                } else if position as usize >= WHISPER_WINDOW_SAMPLES {
                    2 * WHISPER_WINDOW_SAMPLES - 2 - position as usize
                } else {
                    position as usize
                };
                *bin = Complex32::new(
                    pcm.get(sample).copied().unwrap_or_default() * self.window[index],
                    0.0,
                );
            }
            self.fft.process_with_scratch(&mut complex, &mut scratch);
            for (value, bin) in power.iter_mut().zip(&complex) {
                *value = bin.norm_sqr();
            }
            for (mel_idx, filter) in self.filters.iter().enumerate() {
                let energy = filter
                    .iter()
                    .map(|&(bin, weight)| power[bin] * weight)
                    .sum::<f32>();
                mel[mel_idx * WHISPER_FRAMES + frame] = energy.max(1e-10).log10();
            }
        }

        let floor = mel.iter().copied().fold(f32::NEG_INFINITY, f32::max) - 8.0;
        for value in &mut mel {
            *value = (value.max(floor) + 4.0) / 4.0;
        }
        mel
    }
}

/// Returns the default Whisper decoder max sequence length (`448`).
///
/// Accepts no arguments. Returns `448`. Cannot fail.
#[cfg(feature = "whisper-onnx")]
fn default_max_length() -> usize {
    DEFAULT_MAX_LENGTH
}

/// Subset of Hugging Face Whisper `generation_config.json` needed for greedy decoding.
#[cfg(feature = "whisper-onnx")]
#[derive(Debug, Clone, Deserialize)]
struct GenerationConfig {
    /// Initial decoder start token (`<|startoftranscript|>`).
    decoder_start_token_id: i64,
    /// Token suppressing timestamp emission (`<|notimestamps|>`).
    no_timestamps_token_id: i64,
    /// End-of-sequence token ID (`<|endoftext|>`).
    eos_token_id: usize,
    /// Tokens suppressed only on step 0 of decoding.
    #[serde(default)]
    begin_suppress_tokens: HashSet<usize>,
    /// Tokens suppressed on every decoding step.
    #[serde(default)]
    suppress_tokens: HashSet<usize>,
    /// Maximum total decoder token length including prompt tokens.
    #[serde(default = "default_max_length")]
    max_length: usize,
    /// Whether the model supports multilingual language/task prompt tokens.
    #[serde(default)]
    is_multilingual: bool,
    /// Optional multilingual language tag map (for example `"<|en|>" -> 50259`).
    #[serde(default)]
    lang_to_id: HashMap<String, i64>,
    /// Optional multilingual task tag map (for example `"transcribe" -> 50359`).
    #[serde(default)]
    task_to_id: HashMap<String, i64>,
}

/// Reads a UTF-8 JSON model asset from disk while enforcing a strict byte-size safety cap.
///
/// Accepts the asset file path, maximum permitted size in bytes, and a human-readable
/// asset label for diagnostics. Opens the file and reads at most `max_bytes + 1` bytes
/// using [`Read::take`] so oversized or malformed files cannot exhaust memory before
/// JSON deserialization begins. Returns the UTF-8 contents as a `String`. Returns
/// [`AsrError::Io`] on open or read failure, or [`AsrError::Config`] when the file
/// exceeds `max_bytes` or is not valid UTF-8.
#[cfg(feature = "whisper-onnx")]
fn read_bounded_json_file(path: &Path, max_bytes: u64, label: &str) -> Result<String, AsrError> {
    let file = File::open(path)?;
    let read_limit = max_bytes.saturating_add(1);
    let mut bytes = Vec::new();
    file.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(AsrError::Config(format!(
            "{label} {} exceeded {max_bytes}-byte safety cap",
            path.display()
        )));
    }
    String::from_utf8(bytes).map_err(|err| {
        AsrError::Config(format!(
            "{label} {} is not valid UTF-8: {err}",
            path.display()
        ))
    })
}

/// Parses and validates a Whisper `generation_config.json` document string.
///
/// Accepts the raw JSON text and source path for diagnostic context. Deserializes
/// [`GenerationConfig`] and enforces
/// `(MIN_GENERATION_MAX_LENGTH..=MAX_GENERATION_MAX_LENGTH)` on `max_length` plus
/// a non-zero `eos_token_id`. Returns the validated [`GenerationConfig`]. Returns
/// [`AsrError::Config`] if `max_length` is outside `4..=4096`, or
/// [`AsrError::Decode`] if the JSON is malformed or `eos_token_id` is `0`.
#[cfg(feature = "whisper-onnx")]
fn parse_generation_config(raw: &str, source: &Path) -> Result<GenerationConfig, AsrError> {
    let config: GenerationConfig = serde_json::from_str(raw).map_err(|err| {
        AsrError::Decode(format!(
            "failed to parse Whisper generation config {}: {err}",
            source.display()
        ))
    })?;
    if !(MIN_GENERATION_MAX_LENGTH..=MAX_GENERATION_MAX_LENGTH).contains(&config.max_length) {
        return Err(AsrError::Config(format!(
            "invalid Whisper generation_config.json max_length {} in {} (expected {}..={})",
            config.max_length,
            source.display(),
            MIN_GENERATION_MAX_LENGTH,
            MAX_GENERATION_MAX_LENGTH
        )));
    }
    if config.eos_token_id == 0 {
        return Err(AsrError::Decode(format!(
            "invalid Whisper generation_config.json eos_token_id 0 in {}",
            source.display()
        )));
    }
    Ok(config)
}

#[cfg(feature = "whisper-onnx")]
impl GenerationConfig {
    /// Loads and validates `generation_config.json` from disk within [`MAX_GENERATION_CONFIG_BYTES`].
    ///
    /// Accepts the file path. Reads at most `MAX_GENERATION_CONFIG_BYTES + 1` bytes
    /// via [`read_bounded_json_file`] and validates fields with
    /// [`parse_generation_config`]. Returns a validated [`GenerationConfig`].
    /// Returns [`AsrError::Io`] on open or read failure, [`AsrError::Config`] if
    /// the file exceeds [`MAX_GENERATION_CONFIG_BYTES`], is not valid UTF-8, or
    /// specifies a `max_length` outside `4..=4096`, or [`AsrError::Decode`] if the
    /// JSON is malformed or has an unusable `eos_token_id`.
    fn load_from_file(path: &Path) -> Result<Self, AsrError> {
        let raw = read_bounded_json_file(
            path,
            MAX_GENERATION_CONFIG_BYTES,
            "Whisper generation config",
        )?;
        parse_generation_config(&raw, path)
    }

    /// Builds the initial decoder prompt token sequence for greedy transcription.
    ///
    /// Accepts `&self` and an optional BCP-47/ISO language hint. For English-only
    /// models, emits `[decoder_start_token_id, no_timestamps_token_id]`. For
    /// multilingual models with a matching language entry and `"transcribe"` task
    /// token, emits `[decoder_start_token_id, lang_id, transcribe_id, no_timestamps_token_id]`.
    /// Cannot fail.
    fn initial_tokens(&self, language: Option<&str>) -> Vec<i64> {
        if self.is_multilingual
            && let Some(lang) = language
        {
            let primary = lang
                .trim()
                .split(['-', '_'])
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            let key = format!("<|{primary}|>");
            if let (Some(&lang_id), Some(&task_id)) =
                (self.lang_to_id.get(&key), self.task_to_id.get("transcribe"))
            {
                return vec![
                    self.decoder_start_token_id,
                    lang_id,
                    task_id,
                    self.no_timestamps_token_id,
                ];
            }
        }
        vec![self.decoder_start_token_id, self.no_timestamps_token_id]
    }
}

/// Loads `vocab.json` from disk within [`MAX_VOCAB_JSON_BYTES`] and builds its byte-level BPE token table.
///
/// Accepts the path to `vocab.json`. Reads at most `MAX_VOCAB_JSON_BYTES + 1`
/// bytes via [`read_bounded_json_file`] before deserializing the vocabulary map.
/// Returns `HashMap<usize, Vec<u8>>` mapping token IDs to raw UTF-8 bytes.
/// Returns [`AsrError::Io`] on open or read failure, [`AsrError::Config`] if
/// the file exceeds [`MAX_VOCAB_JSON_BYTES`] or is not valid UTF-8, or
/// [`AsrError::Decode`] if `vocab.json` is malformed or yields an empty table.
#[cfg(feature = "whisper-onnx")]
fn load_vocab_table(path: &Path) -> Result<HashMap<usize, Vec<u8>>, AsrError> {
    let raw = read_bounded_json_file(path, MAX_VOCAB_JSON_BYTES, "Whisper vocabulary")?;
    let vocab: HashMap<String, usize> = serde_json::from_str(&raw).map_err(|err| {
        AsrError::Decode(format!(
            "failed to parse Whisper vocabulary {}: {err}",
            path.display()
        ))
    })?;
    let table = build_token_byte_table(&vocab);
    if table.is_empty() {
        return Err(AsrError::Decode(format!(
            "Whisper vocabulary {} yielded zero decodable BPE tokens",
            path.display()
        )));
    }
    Ok(table)
}

/// Opens an ONNX Runtime [`Session`] for one Whisper model file with strict execution provider rules.
///
/// Accepts the active ONNX Runtime [`Environment`], the `.onnx` file path, the
/// requested [`OnnxDevice`], and the intra-op thread count. Returns the loaded
/// [`Session`]. Fails closed with [`AsrError::Config`] if CUDA is requested
/// without the `whisper-onnx-cuda` feature, or [`AsrError::Runtime`] if session
/// configuration, CUDA provider registration, or model loading fails.
#[cfg(feature = "whisper-onnx")]
fn open_session(
    environment: &Environment,
    model_path: &Path,
    device: OnnxDevice,
    threads: usize,
) -> Result<Session, AsrError> {
    let config_err = |err: ort::Error<ort::session::builder::SessionBuilder>| {
        AsrError::Runtime(format!(
            "failed to configure ONNX Runtime session for {}: {err}",
            model_path.display()
        ))
    };
    let mut builder = Session::builder(environment)
        .map_err(|err| {
            AsrError::Runtime(format!(
                "failed to create ONNX Runtime session builder for {}: {err}",
                model_path.display()
            ))
        })?
        .with_intra_threads(threads.max(1))
        .map_err(config_err)?
        .with_inter_threads(1)
        .map_err(config_err)?
        .with_intra_op_spinning(false)
        .map_err(config_err)?;

    match device {
        OnnxDevice::Cpu => {}
        OnnxDevice::Cuda => {
            #[cfg(feature = "whisper-onnx-cuda")]
            {
                builder = builder
                    .with_execution_providers([ort::ep::CUDA::default()
                        .with_tf32(false)
                        .build()
                        .error_on_failure()])
                    .map_err(config_err)?;
            }
            #[cfg(not(feature = "whisper-onnx-cuda"))]
            {
                return Err(AsrError::Config(
                    "local Whisper ONNX CUDA execution (--asr-device cuda) requires building glassbox-cli with the `whisper-onnx-cuda` feature; refusing silent CPU fallback".to_string(),
                ));
            }
        }
    }

    builder.commit_from_file(model_path).map_err(|err| {
        AsrError::Runtime(format!(
            "failed to load Whisper ONNX model {}: {err}",
            model_path.display()
        ))
    })
}

/// Runs greedy KV-cached autoregressive decoding over one encoded Whisper window.
///
/// Accepts a mutable decoder [`Session`], the encoder's `last_hidden_state`
/// [`DynValue`], parsed [`GenerationConfig`], dynamically inferred
/// [`KvCacheSpec`], and an optional language hint. Returns the generated token
/// ID sequence excluding prompt and EOS tokens. Returns [`AsrError::Runtime`]
/// on ONNX execution failures or [`AsrError::Decode`] on missing outputs,
/// unusable logits, or token-budget exhaustion before EOS.
#[cfg(feature = "whisper-onnx")]
fn run_greedy_decoder(
    decoder: &mut Session,
    hidden: &DynValue,
    config: &GenerationConfig,
    kv_spec: KvCacheSpec,
    language: Option<&str>,
) -> Result<Vec<usize>, AsrError> {
    let mut cache: HashMap<String, DynValue> = HashMap::with_capacity(kv_spec.num_layers * 4);
    for layer in 0..kv_spec.num_layers {
        for attention in ["decoder", "encoder"] {
            for item in ["key", "value"] {
                let suffix = format!("{layer}.{attention}.{item}");
                let empty = Tensor::from_array((
                    [1usize, kv_spec.num_heads, 0usize, kv_spec.head_dim],
                    Vec::<f32>::new(),
                ))
                .map_err(|err| {
                    AsrError::Runtime(format!(
                        "failed to allocate initial KV-cache tensor past_key_values.{suffix}: {err}"
                    ))
                })?
                .into_dyn();
                cache.insert(suffix, empty);
            }
        }
    }

    let mut tokens = config.initial_tokens(language);
    let max_new_tokens = config.max_length.saturating_sub(tokens.len());
    let mut result = Vec::new();

    for step in 0..max_new_tokens {
        let ids = Tensor::from_array(([1usize, tokens.len()], tokens)).map_err(|err| {
            AsrError::Runtime(format!("failed to create decoder input_ids tensor: {err}"))
        })?;
        let use_cache = Tensor::from_array(([1usize], vec![step > 0])).map_err(|err| {
            AsrError::Runtime(format!(
                "failed to create decoder use_cache_branch tensor: {err}"
            ))
        })?;

        let mut inputs: Vec<(String, SessionInputValue<'_>)> = Vec::with_capacity(3 + cache.len());
        inputs.push(("input_ids".to_string(), ids.into()));
        inputs.push(("encoder_hidden_states".to_string(), hidden.into()));
        inputs.push(("use_cache_branch".to_string(), use_cache.into()));
        inputs.extend(
            cache
                .iter()
                .map(|(suffix, value)| (format!("past_key_values.{suffix}"), value.into())),
        );

        let mut outputs = decoder.run(inputs).map_err(|err| {
            AsrError::Runtime(format!(
                "Whisper ONNX decoder inference failed at step {step}: {err}"
            ))
        })?;
        let logits_value = outputs.remove("logits").ok_or_else(|| {
            AsrError::Decode(format!(
                "Whisper ONNX decoder omitted `logits` output at step {step}"
            ))
        })?;
        let (shape, values) = logits_value.try_extract_tensor::<f32>().map_err(|err| {
            AsrError::Decode(format!(
                "failed to extract Whisper decoder logits tensor at step {step}: {err}"
            ))
        })?;
        if shape.len() != 3 || shape[2] <= 0 {
            return Err(AsrError::Decode(format!(
                "unexpected Whisper decoder logits shape {shape:?} at step {step}"
            )));
        }
        let vocab_size = shape[2] as usize;
        if values.len() < vocab_size {
            return Err(AsrError::Decode(format!(
                "Whisper decoder logits buffer too small ({} < {vocab_size}) at step {step}",
                values.len()
            )));
        }
        let logits = &values[values.len() - vocab_size..];
        let limit = config.eos_token_id.saturating_add(1).min(logits.len());
        let (next, _) = logits
            .iter()
            .copied()
            .enumerate()
            .take(limit)
            .filter(|(id, logit)| {
                logit.is_finite()
                    && !config.suppress_tokens.contains(id)
                    && (step > 0 || !config.begin_suppress_tokens.contains(id))
            })
            .max_by(|left, right| left.1.total_cmp(&right.1))
            .ok_or_else(|| {
                AsrError::Decode(format!(
                    "Whisper decoder produced no usable finite logits at step {step}"
                ))
            })?;
        if next == config.eos_token_id {
            return Ok(result);
        }
        result.push(next);
        tokens = vec![next as i64];
        for (suffix, value) in &mut cache {
            // The cached branch emits fresh decoder self-attention state; encoder cross-attention stays constant after step 0.
            if step == 0 || suffix.contains(".decoder.") {
                *value = outputs.remove(format!("present.{suffix}")).ok_or_else(|| {
                    AsrError::Decode(format!(
                        "Whisper decoder omitted KV-cache output present.{suffix} at step {step}"
                    ))
                })?;
            }
        }
    }

    Err(AsrError::Decode(format!(
        "Whisper decoder exhausted its token budget ({max_new_tokens} tokens) before producing EOS"
    )))
}

/// Process-wide ONNX Runtime environment shared across all Whisper sessions and worker threads.
///
/// `ort` retains only a weak reference to the active environment internally, so
/// storing [`Environment`] in a process-wide static initializes ONNX Runtime at
/// most once and keeps it alive across files and `--batch --jobs N` workers.
#[cfg(feature = "whisper-onnx")]
static WHISPER_ORT_ENV: OnceLock<Environment> = OnceLock::new();

/// Mutex serializing first-time process-wide ONNX Runtime initialization across worker threads.
#[cfg(feature = "whisper-onnx")]
static WHISPER_ORT_INIT_LOCK: Mutex<()> = Mutex::new(());

/// Initializes or returns the process-wide ONNX Runtime [`Environment`] for Whisper sessions.
///
/// Accepts the canonicalized ONNX Runtime shared library path. On first call,
/// ensures `ORT_DISABLE_TELEMETRY=1` is set if unset (defensively covering
/// direct library and unit-test callers), loads `runtime_path`, and builds one
/// process-wide `"glassbox-whisper-onnx"` [`Environment`] with telemetry
/// disabled. Subsequent calls across files or `--batch --jobs N` worker threads
/// return the cached `&'static Environment` lock-free. Returns
/// [`AsrError::Runtime`] if loading the library or building the environment fails.
#[cfg(feature = "whisper-onnx")]
fn get_or_init_ort_environment(runtime_path: &Path) -> Result<&'static Environment, AsrError> {
    if let Some(environment) = WHISPER_ORT_ENV.get() {
        return Ok(environment);
    }
    let _guard = WHISPER_ORT_INIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(environment) = WHISPER_ORT_ENV.get() {
        return Ok(environment);
    }
    ensure_ort_telemetry_disabled();
    let environment = ort::init_from(runtime_path)
        .map_err(|err| {
            AsrError::Runtime(format!(
                "failed to load ONNX Runtime library {}: {err}",
                runtime_path.display()
            ))
        })?
        .with_name("glassbox-whisper-onnx")
        .with_telemetry(false)
        .build()
        .map_err(|err| {
            AsrError::Runtime(format!(
                "failed to initialize ONNX Runtime environment: {err}"
            ))
        })?;
    let _ = WHISPER_ORT_ENV.set(environment);
    Ok(WHISPER_ORT_ENV
        .get()
        .expect("WHISPER_ORT_ENV initialized above"))
}

/// Warm ONNX Runtime sessions, tokenizer, and feature extractor kept across windows.
#[cfg(feature = "whisper-onnx")]
struct LoadedWhisperRuntime {
    /// Warm Whisper encoder ONNX session.
    encoder: Session,
    /// Warm merged Whisper decoder ONNX session.
    decoder: Session,
    /// Dynamically discovered decoder KV-cache tensor geometry.
    kv_spec: KvCacheSpec,
    /// Parsed generation hyperparameters and suppression lists.
    generation: GenerationConfig,
    /// Byte-level BPE token ID to UTF-8 bytes map.
    token_bytes: HashMap<usize, Vec<u8>>,
    /// Precomputed 80-bin Slaney log-Mel extractor.
    features: Features,
}

/// Local offline Whisper ONNX implementation of [`AsrBackend`].
#[cfg(feature = "whisper-onnx")]
pub struct WhisperOnnxAsrBackend {
    /// Backend configuration (model directory, execution device, thread count).
    config: WhisperOnnxConfig,
    /// Stable model label derived from `config.model_dir`.
    model_id: String,
    /// Lazily initialized warm ONNX Runtime sessions reused across all windows of a file.
    loaded: Option<LoadedWhisperRuntime>,
}

#[cfg(feature = "whisper-onnx")]
impl WhisperOnnxAsrBackend {
    /// Creates a new [`WhisperOnnxAsrBackend`] without eagerly loading ONNX sessions.
    ///
    /// Accepts a [`WhisperOnnxConfig`]. Returns a backend ready for [`AsrBackend::validate`]
    /// and [`AsrBackend::transcribe_window`]. Performs no I/O or network calls and cannot fail.
    pub fn new(config: WhisperOnnxConfig) -> Self {
        let model_id = derive_model_id(&config.model_dir);
        Self {
            config,
            model_id,
            loaded: None,
        }
    }

    /// Lazily loads and caches warm Whisper encoder/decoder sessions, KV spec, and tokenizer.
    ///
    /// Accepts `&mut self`. Acquires the process-wide ONNX Runtime
    /// [`Environment`] via [`get_or_init_ort_environment`] (initialized at most
    /// once per process across files and batch worker threads) and reuses the
    /// already-warm [`LoadedWhisperRuntime`] on subsequent windows without
    /// reloading model weights. Returns [`AsrError::Config`],
    /// [`AsrError::MissingAsset`], [`AsrError::Decode`], [`AsrError::Runtime`],
    /// or [`AsrError::Io`] on fail-closed precondition or initialization failures.
    fn ensure_loaded(&mut self) -> Result<&mut LoadedWhisperRuntime, AsrError> {
        if self.loaded.is_none() {
            self.check_device_feature_support()?;
            let assets = resolve_model_assets(&self.config.model_dir)?;
            let ort_dylib = resolve_ort_dylib()?;
            let generation = GenerationConfig::load_from_file(&assets.generation_config_path)?;
            let token_bytes = load_vocab_table(&assets.vocab_path)?;

            let runtime_path = ort_dylib.canonicalize().unwrap_or(ort_dylib);
            let environment = get_or_init_ort_environment(&runtime_path)?;

            let encoder = open_session(
                environment,
                &assets.encoder_path,
                self.config.device,
                self.config.threads,
            )?;
            let decoder = open_session(
                environment,
                &assets.decoder_path,
                self.config.device,
                self.config.threads,
            )?;
            let kv_spec = parse_kv_cache_spec_from_session(&decoder)?;

            self.loaded = Some(LoadedWhisperRuntime {
                encoder,
                decoder,
                kv_spec,
                generation,
                token_bytes,
                features: Features::new(),
            });
        }
        Ok(self.loaded.as_mut().expect("loaded initialized above"))
    }

    /// Verifies that the requested execution device is supported by the compiled binary features.
    ///
    /// Accepts `&self`. Returns `Ok(())` for CPU or CUDA when `whisper-onnx-cuda`
    /// is compiled in. Returns [`AsrError::Config`] when CUDA is requested
    /// without `whisper-onnx-cuda`, preventing silent CPU fallback.
    fn check_device_feature_support(&self) -> Result<(), AsrError> {
        #[cfg(not(feature = "whisper-onnx-cuda"))]
        if self.config.device == OnnxDevice::Cuda {
            return Err(AsrError::Config(
                "local Whisper ONNX CUDA execution (--asr-device cuda) requires building glassbox-cli with the `whisper-onnx-cuda` feature; refusing silent CPU fallback".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(feature = "whisper-onnx")]
impl AsrBackend for WhisperOnnxAsrBackend {
    /// Returns the stable backend identifier `"whisper-onnx"`.
    ///
    /// Accepts `&self`. Returns `"whisper-onnx"`. Cannot fail.
    fn backend_name(&self) -> &'static str {
        "whisper-onnx"
    }

    /// Returns the model label derived from the configured model directory basename.
    ///
    /// Accepts `&self`. Returns `&str`. Cannot fail.
    fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Returns the active execution provider label (`"cpu"` or `"cuda"`).
    ///
    /// Accepts `&self`. Returns `"cpu"` or `"cuda"`. Cannot fail.
    fn provider(&self) -> &str {
        self.config.device.as_str()
    }

    /// Validates all offline preconditions before FFmpeg starts decoding audio.
    ///
    /// Accepts `&self`. Verifies compiled CUDA feature support when CUDA is
    /// requested, checks `model_dir` and required model files via
    /// [`resolve_model_assets`], resolves the ONNX Runtime shared library via
    /// [`resolve_ort_dylib`], and parses `generation_config.json` and
    /// `vocab.json`. Performs zero network I/O. Returns `Ok(())` when ready, or
    /// an actionable [`AsrError`] on any missing asset or misconfiguration.
    fn validate(&self) -> Result<(), AsrError> {
        self.check_device_feature_support()?;
        let assets = resolve_model_assets(&self.config.model_dir)?;
        let _ort_dylib = resolve_ort_dylib()?;
        let _generation = GenerationConfig::load_from_file(&assets.generation_config_path)?;
        let _vocab = load_vocab_table(&assets.vocab_path)?;
        Ok(())
    }

    /// Transcribes one 16 kHz mono PCM window completely offline using warm Whisper ONNX sessions.
    ///
    /// Accepts `&mut self`, an [`AudioWindow`], and an optional BCP-47/ISO
    /// language hint. Validates finite PCM samples, initializes or reuses warm
    /// encoder/decoder sessions, extracts 80-bin Slaney log-Mel features, runs
    /// encoder + greedy KV-cached decoder, and decodes BPE tokens into trimmed
    /// UTF-8 text. Performs zero network I/O. Returns a [`TranscriptChunk`]
    /// preserving `window.start_ms` and `window.end_ms`, or [`AsrError`] on
    /// invalid PCM, missing assets, or ONNX Runtime/decoding failure.
    fn transcribe_window(
        &mut self,
        window: &AudioWindow,
        language: Option<&str>,
    ) -> Result<TranscriptChunk, AsrError> {
        if !window.pcm_16k_f32.iter().all(|sample| sample.is_finite()) {
            return Err(AsrError::Decode(format!(
                "audio window {} contains non-finite PCM samples",
                window.index
            )));
        }

        let runtime = self.ensure_loaded()?;
        if window.pcm_16k_f32.is_empty() {
            return Ok(TranscriptChunk {
                start_ms: window.start_ms,
                end_ms: window.end_ms,
                text: String::new(),
            });
        }

        let mel = runtime.features.log_mel(&window.pcm_16k_f32);
        let mel_tensor = Tensor::from_array(([1usize, WHISPER_MELS, WHISPER_FRAMES], mel))
            .map_err(|err| {
                AsrError::Runtime(format!(
                    "failed to build Whisper encoder input_features tensor: {err}"
                ))
            })?;

        let mut enc_outputs = runtime
            .encoder
            .run(ort::inputs!["input_features" => mel_tensor])
            .map_err(|err| {
                AsrError::Runtime(format!(
                    "Whisper ONNX encoder inference failed on window {}: {err}",
                    window.index
                ))
            })?;
        let hidden = enc_outputs.remove("last_hidden_state").ok_or_else(|| {
            AsrError::Decode(format!(
                "Whisper ONNX encoder omitted `last_hidden_state` output on window {}",
                window.index
            ))
        })?;

        let token_ids = run_greedy_decoder(
            &mut runtime.decoder,
            &hidden,
            &runtime.generation,
            runtime.kv_spec,
            language,
        )?;
        let decoded = decode_token_ids(&token_ids, &runtime.token_bytes)?;

        Ok(TranscriptChunk {
            start_ms: window.start_ms,
            end_ms: window.end_ms,
            text: decoded.trim().to_string(),
        })
    }
}

/// Fail-closed stub used when `glassbox-cli` is compiled without the `whisper-onnx` feature.
#[cfg(not(feature = "whisper-onnx"))]
pub struct WhisperOnnxAsrBackend {
    /// Stored configuration retained for diagnostic reporting.
    config: WhisperOnnxConfig,
    /// Stable model label derived from `config.model_dir`.
    model_id: String,
}

#[cfg(not(feature = "whisper-onnx"))]
impl WhisperOnnxAsrBackend {
    /// Creates a stub [`WhisperOnnxAsrBackend`] that fails closed because `whisper-onnx` is disabled.
    ///
    /// Accepts a [`WhisperOnnxConfig`]. Returns the stub backend. Cannot fail.
    pub fn new(config: WhisperOnnxConfig) -> Self {
        let model_id = derive_model_id(&config.model_dir);
        Self { config, model_id }
    }
}

#[cfg(not(feature = "whisper-onnx"))]
impl AsrBackend for WhisperOnnxAsrBackend {
    /// Returns the stable backend identifier `"whisper-onnx"`.
    ///
    /// Accepts `&self`. Returns `"whisper-onnx"`. Cannot fail.
    fn backend_name(&self) -> &'static str {
        "whisper-onnx"
    }

    /// Returns the model label derived from the configured model directory basename.
    ///
    /// Accepts `&self`. Returns `&str`. Cannot fail.
    fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Returns the configured execution device label (`"cpu"` or `"cuda"`).
    ///
    /// Accepts `&self`. Returns `"cpu"` or `"cuda"`. Cannot fail.
    fn provider(&self) -> &str {
        self.config.device.as_str()
    }

    /// Fails closed explaining that `glassbox-cli` was built without the `whisper-onnx` feature.
    ///
    /// Accepts `&self`. Always returns [`AsrError::Config`].
    fn validate(&self) -> Result<(), AsrError> {
        Err(AsrError::Config(
            "built without whisper-onnx feature; rebuild glassbox-cli with `--features whisper-onnx` to use local Whisper ONNX ASR".to_string(),
        ))
    }

    /// Fails closed explaining that `glassbox-cli` was built without the `whisper-onnx` feature.
    ///
    /// Accepts `&mut self`, an [`AudioWindow`], and an optional language hint.
    /// Always returns [`AsrError::Config`].
    fn transcribe_window(
        &mut self,
        _window: &AudioWindow,
        _language: Option<&str>,
    ) -> Result<TranscriptChunk, AsrError> {
        self.validate()?;
        unreachable!("validate always returns Err when whisper-onnx feature is disabled")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Creates a unique isolated temporary directory under `extensionalities/target/tests/whisper/`.
    ///
    /// Accepts a descriptive test label. Returns the created directory path.
    /// Panics if directory creation fails.
    fn make_temp_dir(label: &str) -> PathBuf {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tests/whisper")
            .join(format!(
                "{label}-{}-{}",
                std::process::id(),
                TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        if base.exists() {
            let _ = fs::remove_dir_all(&base);
        }
        fs::create_dir_all(&base).expect("create test temp dir");
        base
    }

    /// Writes a placeholder file inside `dir`, creating parent subdirectories as needed.
    ///
    /// Accepts a base directory, relative path, and content bytes. Returns the
    /// created file path. Panics on I/O error.
    fn write_file(dir: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&path, bytes).expect("write fixture file");
        path
    }

    /// Synthesizes `(name, shape)` decoder input entries for a Whisper model geometry.
    ///
    /// Accepts layer count, head count, and per-head dimension. Returns owned
    /// `(String, Vec<i64>)` pairs matching Hugging Face Optimum's merged decoder
    /// export schema. Cannot fail.
    fn synthetic_kv_inputs(layers: usize, heads: i64, head_dim: i64) -> Vec<(String, Vec<i64>)> {
        let mut entries = vec![
            ("input_ids".to_string(), vec![-1, -1]),
            (
                "encoder_hidden_states".to_string(),
                vec![-1, 1500, heads * head_dim],
            ),
            ("use_cache_branch".to_string(), vec![1]),
        ];
        for layer in 0..layers {
            for attention in ["decoder", "encoder"] {
                for item in ["key", "value"] {
                    entries.push((
                        format!("past_key_values.{layer}.{attention}.{item}"),
                        vec![-1, heads, -1, head_dim],
                    ));
                }
            }
        }
        entries
    }

    #[test]
    fn resolve_model_assets_accepts_int8_unquantized_and_onnx_subdir() {
        let dir_int8 = make_temp_dir("assets-int8");
        write_file(&dir_int8, "encoder_model_int8.onnx", b"enc-i8");
        write_file(&dir_int8, "encoder_model.onnx", b"enc-fp32");
        write_file(&dir_int8, "decoder_model_merged_int8.onnx", b"dec-i8");
        write_file(&dir_int8, "decoder_model_merged.onnx", b"dec-fp32");
        write_file(&dir_int8, "generation_config.json", b"{}");
        write_file(&dir_int8, "vocab.json", b"{}");

        let resolved = resolve_model_assets(&dir_int8).expect("int8 preference should succeed");
        assert!(resolved.encoder_path.ends_with("encoder_model_int8.onnx"));
        assert!(
            resolved
                .decoder_path
                .ends_with("decoder_model_merged_int8.onnx")
        );
        assert!(
            resolved
                .generation_config_path
                .ends_with("generation_config.json")
        );
        assert!(resolved.vocab_path.ends_with("vocab.json"));

        let dir_fp32 = make_temp_dir("assets-fp32");
        write_file(&dir_fp32, "encoder_model.onnx", b"enc");
        write_file(&dir_fp32, "decoder_model_merged.onnx", b"dec");
        write_file(&dir_fp32, "generation_config.json", b"{}");
        write_file(&dir_fp32, "vocab.json", b"{}");

        let resolved_fp32 = resolve_model_assets(&dir_fp32).expect("unquantized should succeed");
        assert!(resolved_fp32.encoder_path.ends_with("encoder_model.onnx"));
        assert!(
            resolved_fp32
                .decoder_path
                .ends_with("decoder_model_merged.onnx")
        );

        let dir_onnx = make_temp_dir("assets-onnx-subdir");
        write_file(&dir_onnx, "onnx/encoder_model_int8.onnx", b"enc");
        write_file(&dir_onnx, "onnx/decoder_model_merged.onnx", b"dec");
        write_file(&dir_onnx, "generation_config.json", b"{}");
        write_file(&dir_onnx, "vocab.json", b"{}");

        let resolved_onnx = resolve_model_assets(&dir_onnx).expect("onnx subdir should succeed");
        assert!(
            resolved_onnx
                .encoder_path
                .ends_with("onnx/encoder_model_int8.onnx")
        );
        assert!(
            resolved_onnx
                .decoder_path
                .ends_with("onnx/decoder_model_merged.onnx")
        );

        let _ = fs::remove_dir_all(&dir_int8);
        let _ = fs::remove_dir_all(&dir_fp32);
        let _ = fs::remove_dir_all(&dir_onnx);
    }

    #[test]
    fn resolve_model_assets_fails_cleanly_on_missing_dir_or_files() {
        let missing_dir = make_temp_dir("assets-missing-root").join("does-not-exist");
        let err = resolve_model_assets(&missing_dir).expect_err("missing dir must fail");
        assert!(matches!(err, AsrError::MissingAsset(_)));
        assert!(err.to_string().contains("does not exist"));

        let partial = make_temp_dir("assets-partial");
        write_file(&partial, "encoder_model_int8.onnx", b"enc");
        let err = resolve_model_assets(&partial).expect_err("partial dir must fail");
        assert!(matches!(err, AsrError::MissingAsset(_)));
        let msg = err.to_string();
        assert!(msg.contains("decoder_model_merged_int8.onnx"));
        assert!(msg.contains("generation_config.json"));
        assert!(msg.contains("vocab.json"));

        let _ = fs::remove_dir_all(&partial);
        if let Some(parent) = missing_dir.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn resolve_ort_dylib_honors_explicit_env_and_liteparse_cache_and_fails_closed() {
        let root = make_temp_dir("ort-dylib");
        let explicit_lib = write_file(&root, "custom/libonnxruntime.so", b"fake-so");
        let cache_home = root.join("home");
        let cached_lib = write_file(
            &cache_home,
            &format!(
                ".cache/liteparse/models/ocria-nano/runtime/{}",
                ORT_DYLIB_NAMES[0]
            ),
            b"cached-so",
        );

        // 1. Valid explicit ORT_DYLIB_PATH wins over cache.
        let found = resolve_ort_dylib_with(
            |key| match key {
                ORT_DYLIB_ENV_VAR => Some(explicit_lib.to_string_lossy().into_owned()),
                "HOME" => Some(cache_home.to_string_lossy().into_owned()),
                _ => None,
            },
            &[],
        )
        .expect("explicit ORT_DYLIB_PATH must resolve");
        assert_eq!(found, explicit_lib);

        // 2. Explicit ORT_DYLIB_PATH pointing to a missing file MUST fail closed (no silent fallback to cache).
        let bogus = root.join("missing/libonnxruntime.so");
        let err = resolve_ort_dylib_with(
            |key| match key {
                ORT_DYLIB_ENV_VAR => Some(bogus.to_string_lossy().into_owned()),
                "HOME" => Some(cache_home.to_string_lossy().into_owned()),
                _ => None,
            },
            &[],
        )
        .expect_err("invalid explicit ORT_DYLIB_PATH must fail closed");
        assert!(matches!(err, AsrError::MissingAsset(_)));
        assert!(err.to_string().contains(ORT_DYLIB_ENV_VAR));

        // 3. Unset ORT_DYLIB_PATH falls back to LiteParse's ocria-nano/runtime cache.
        let found_cached = resolve_ort_dylib_with(
            |key| match key {
                "HOME" => Some(cache_home.to_string_lossy().into_owned()),
                _ => None,
            },
            &[],
        )
        .expect("LiteParse cache fallback must resolve");
        assert_eq!(found_cached, cached_lib);

        // 4. Missing everywhere returns actionable ORT_DYLIB_PATH error.
        let err = resolve_ort_dylib_with(|_| None, &[])
            .expect_err("missing runtime everywhere must fail");
        assert!(matches!(err, AsrError::MissingAsset(_)));
        assert!(err.to_string().contains(ORT_DYLIB_ENV_VAR));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn infer_kv_cache_spec_handles_tiny_base_small_and_rejects_malformed() {
        for (layers, heads, head_dim) in [(4, 6, 64), (6, 8, 64), (12, 12, 64)] {
            let owned = synthetic_kv_inputs(layers, heads, head_dim);
            let refs: Vec<(&str, &[i64])> = owned
                .iter()
                .map(|(name, shape)| (name.as_str(), shape.as_slice()))
                .collect();
            let spec = infer_kv_cache_spec(&refs).expect("valid Whisper schema must parse");
            assert_eq!(
                spec,
                KvCacheSpec {
                    num_layers: layers,
                    num_heads: heads as usize,
                    head_dim: head_dim as usize,
                }
            );
        }

        // Empty schema must fail.
        let err = infer_kv_cache_spec(&[]).expect_err("empty schema must fail");
        assert!(matches!(err, AsrError::Decode(_)));

        // Non-KV inputs only must fail.
        let err = infer_kv_cache_spec(&[("input_ids", &[-1, -1])])
            .expect_err("missing KV inputs must fail");
        assert!(matches!(err, AsrError::Decode(_)));

        // Incomplete layer (missing encoder.value) must fail.
        let mut incomplete = synthetic_kv_inputs(4, 6, 64);
        incomplete.pop();
        let refs: Vec<(&str, &[i64])> = incomplete
            .iter()
            .map(|(n, s)| (n.as_str(), s.as_slice()))
            .collect();
        assert!(infer_kv_cache_spec(&refs).is_err());

        // Mismatched head count across layers must fail.
        let mut mismatched = synthetic_kv_inputs(4, 6, 64);
        mismatched[5].1[1] = 8;
        let refs: Vec<(&str, &[i64])> = mismatched
            .iter()
            .map(|(n, s)| (n.as_str(), s.as_slice()))
            .collect();
        assert!(infer_kv_cache_spec(&refs).is_err());

        // Dynamic head_dim (-1) must fail.
        let mut dynamic_dim = synthetic_kv_inputs(4, 6, 64);
        dynamic_dim[4].1[3] = -1;
        let refs: Vec<(&str, &[i64])> = dynamic_dim
            .iter()
            .map(|(n, s)| (n.as_str(), s.as_slice()))
            .collect();
        assert!(infer_kv_cache_spec(&refs).is_err());

        // Non-contiguous layer index gap (skip layer 1) must fail.
        let mut gapped = synthetic_kv_inputs(2, 6, 64);
        for (name, _) in &mut gapped {
            if name.starts_with("past_key_values.1.") {
                *name = name.replacen("past_key_values.1.", "past_key_values.2.", 1);
            }
        }
        let refs: Vec<(&str, &[i64])> = gapped
            .iter()
            .map(|(n, s)| (n.as_str(), s.as_slice()))
            .collect();
        assert!(infer_kv_cache_spec(&refs).is_err());
    }

    #[cfg(feature = "whisper-onnx")]
    #[test]
    fn slaney_log_mel_has_exact_shape_finite_values_and_expected_dynamic_range() {
        let features = Features::new();
        // 1-second 440 Hz sine wave at 16 kHz.
        let pcm: Vec<f32> = (0..SAMPLE_RATE_HZ)
            .map(|i| 0.5 * (TAU * 440.0 * i as f32 / SAMPLE_RATE_HZ as f32).sin())
            .collect();

        let mel = features.log_mel(&pcm);
        assert_eq!(mel.len(), WHISPER_MELS * WHISPER_FRAMES);
        assert_eq!(mel.len(), 240_000);
        assert!(mel.iter().all(|v| v.is_finite()));

        let min_val = mel.iter().copied().fold(f32::INFINITY, f32::min);
        let max_val = mel.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        assert!(
            (max_val - min_val) <= 2.0001,
            "normalized log-Mel span must not exceed 2.0 (8 decades / 4), got [{min_val}, {max_val}]"
        );
        assert!(
            max_val > min_val + 0.5,
            "440 Hz tone must produce non-flat Mel energy, got [{min_val}, {max_val}]"
        );

        // Silence input must also remain finite and exact shape.
        let silence = features.log_mel(&vec![0.0; SAMPLE_RATE_HZ]);
        assert_eq!(silence.len(), 240_000);
        assert!(silence.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn byte_level_bpe_decodes_spaces_punctuation_and_multibyte_utf8() {
        // GPT-2 byte-to-Unicode alias helper for test construction:
        // 'Ġ' (U+0120) = 0x20 (' '), 'Ã' (U+00C3) = 0xC3, '©' (U+00A9) = 0xA9 ('é' = C3 A9).
        let mut vocab = HashMap::new();
        vocab.insert("Hello".to_string(), 10);
        vocab.insert(",".to_string(), 11);
        vocab.insert("\u{0120}caf\u{00C3}\u{00A9}".to_string(), 12);
        vocab.insert("!".to_string(), 13);

        let table = build_token_byte_table(&vocab);
        let text = decode_token_ids(&[10, 11, 12, 13], &table).expect("valid BPE decode");
        assert_eq!(text, "Hello, café!");

        let err = decode_token_ids(&[10, 9999], &table).expect_err("unknown token must fail");
        assert!(matches!(err, AsrError::Decode(_)));
    }

    #[cfg(all(feature = "whisper-onnx", not(feature = "whisper-onnx-cuda")))]
    #[test]
    fn cuda_without_feature_fails_closed_in_validate() {
        let dir = make_temp_dir("cuda-fail-closed");
        let backend =
            WhisperOnnxAsrBackend::new(WhisperOnnxConfig::new(dir.clone(), OnnxDevice::Cuda));
        assert_eq!(backend.backend_name(), "whisper-onnx");
        assert_eq!(backend.provider(), "cuda");
        let err = backend
            .validate()
            .expect_err("CUDA without whisper-onnx-cuda feature must fail closed");
        assert!(matches!(err, AsrError::Config(_)));
        assert!(err.to_string().contains("whisper-onnx-cuda"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "whisper-onnx")]
    #[test]
    fn generation_config_rejects_out_of_range_max_length() {
        let source = Path::new("generation_config.json");

        for invalid in [0usize, 3, 4097, usize::MAX] {
            let raw = format!(
                r#"{{"decoder_start_token_id":50258,"no_timestamps_token_id":50363,"eos_token_id":50257,"max_length":{invalid}}}"#
            );
            let err = parse_generation_config(&raw, source)
                .expect_err("out-of-range max_length must be rejected");
            match err {
                AsrError::Config(msg) => {
                    assert!(
                        msg.contains("max_length"),
                        "error for max_length={invalid} must mention max_length: {msg}"
                    );
                    assert!(
                        msg.contains("4..=4096"),
                        "error for max_length={invalid} must mention 4..=4096: {msg}"
                    );
                }
                other => {
                    panic!("expected AsrError::Config for max_length={invalid}, got {other:?}")
                }
            }
        }

        for valid in [4usize, 448, 4096] {
            let raw = format!(
                r#"{{"decoder_start_token_id":50258,"no_timestamps_token_id":50363,"eos_token_id":50257,"max_length":{valid}}}"#
            );
            let cfg = parse_generation_config(&raw, source)
                .expect("in-range max_length must be accepted");
            assert_eq!(cfg.max_length, valid);
        }

        let omitted_max_length = r#"{"decoder_start_token_id":50258,"no_timestamps_token_id":50363,"eos_token_id":50257}"#;
        let default_cfg = parse_generation_config(omitted_max_length, source)
            .expect("omitted max_length must use default 448");
        assert_eq!(default_cfg.max_length, DEFAULT_MAX_LENGTH);
    }

    #[cfg(feature = "whisper-onnx")]
    #[test]
    fn bounded_json_reader_rejects_oversized_file() {
        let dir = make_temp_dir("bounded-json-reader");
        let cap = 128u64;

        let exact_payload = " ".repeat(cap as usize - 2) + "{}";
        assert_eq!(exact_payload.len(), cap as usize);
        let exact_path = write_file(&dir, "exact.json", exact_payload.as_bytes());
        let read_exact =
            read_bounded_json_file(&exact_path, cap, "test JSON").expect("exact cap must succeed");
        assert_eq!(read_exact.len(), cap as usize);

        let over_by_one_path = write_file(&dir, "over_one.json", &vec![b'a'; cap as usize + 1]);
        let err_one = read_bounded_json_file(&over_by_one_path, cap, "test JSON")
            .expect_err("cap + 1 bytes must be rejected");
        assert!(
            matches!(err_one, AsrError::Config(ref msg) if msg.contains("128-byte safety cap")),
            "unexpected error for cap + 1: {err_one:?}"
        );

        let huge_path = write_file(&dir, "huge.json", &vec![b'b'; 8192]);
        let err_huge = read_bounded_json_file(&huge_path, cap, "test JSON")
            .expect_err("large oversized file must be rejected");
        assert!(
            matches!(err_huge, AsrError::Config(ref msg) if msg.contains("128-byte safety cap")),
            "unexpected error for huge file: {err_huge:?}"
        );

        let invalid_utf8_path = write_file(&dir, "invalid_utf8.json", &[0xff, 0xfe]);
        let err_utf8 = read_bounded_json_file(&invalid_utf8_path, cap, "test JSON")
            .expect_err("invalid UTF-8 must be rejected");
        assert!(
            matches!(err_utf8, AsrError::Config(ref msg) if msg.contains("UTF-8")),
            "unexpected error for invalid UTF-8: {err_utf8:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

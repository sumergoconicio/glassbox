//! Module: glassbox-cli persistent model registry and ASR backend configuration resolver.
//! File: models.rs.
//! Responsibility: Persist and validate `models.json` endpoints and capability bindings without storing raw secrets, and resolve ASR backends under strict `CLI > env > models.json` precedence with fail-closed disambiguation.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

/// Schema version stored in `models.json`.
pub const MODELS_SCHEMA_VERSION: u32 = 1;

/// Canonical capability identifier for Automatic Speech Recognition bindings.
pub const CAPABILITY_ASR: &str = "asr";

/// Default per-window HTTP read timeout in seconds for OpenAI-compatible ASR requests.
pub const DEFAULT_ASR_TIMEOUT_SECS: u64 = 120;

/// Environment variable overriding the Glassbox configuration directory.
pub const ENV_CONFIG_DIR: &str = "GLASSBOX_CONFIG_DIR";

/// XDG configuration home directory fallback environment variable.
pub const ENV_XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";

/// Home directory fallback environment variable.
pub const ENV_HOME: &str = "HOME";

/// Environment variable selecting the ASR backend explicitly (`openai` or `whisper-onnx`).
pub const ENV_ASR_BACKEND: &str = "GLASSBOX_ASR_BACKEND";

/// Environment variable supplying the OpenAI-compatible ASR API base URL.
pub const ENV_ASR_URL: &str = "GLASSBOX_ASR_URL";

/// Environment variable supplying the OpenAI-compatible ASR model identifier.
pub const ENV_ASR_MODEL: &str = "GLASSBOX_ASR_MODEL";

/// Environment variable naming the env var that holds the OpenAI-compatible API key.
pub const ENV_ASR_API_KEY_ENV: &str = "GLASSBOX_ASR_API_KEY_ENV";

/// Environment variable selecting the OpenAI-compatible wire protocol (`transcriptions` or `chat`).
pub const ENV_ASR_PROTOCOL: &str = "GLASSBOX_ASR_PROTOCOL";

/// Environment variable supplying the local Whisper ONNX model directory.
pub const ENV_ASR_MODEL_DIR: &str = "GLASSBOX_ASR_MODEL_DIR";

/// Environment variable selecting the local Whisper ONNX execution device (`cpu` or `cuda`).
pub const ENV_ASR_DEVICE: &str = "GLASSBOX_ASR_DEVICE";

/// Environment variable overriding the per-window ASR HTTP read timeout in seconds.
pub const ENV_ASR_TIMEOUT_SECS: &str = "GLASSBOX_ASR_TIMEOUT_SECS";

/// Monotonic counter used to generate unique hidden temp filenames in the same directory.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Errors returned while loading, modifying, persisting, or resolving model configurations.
#[derive(Debug)]
pub enum ModelsError {
    /// Invalid CLI arguments, invalid env-var/URL syntax, or ambiguous/missing backend config (exit `64`).
    Usage(String),
    /// Invalid, corrupt, or unresolvable persistent `models.json` configuration (exit `1`).
    Config(String),
    /// Local filesystem I/O failure while reading or writing `models.json` (exit `1`).
    Io(std::io::Error),
}

impl ModelsError {
    /// Returns the CLI exit code corresponding to this model configuration error.
    ///
    /// Accepts `&self`. Returns `64` for [`ModelsError::Usage`] and `1` for
    /// [`ModelsError::Config`] or [`ModelsError::Io`]. Cannot fail.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 64,
            Self::Config(_) | Self::Io(_) => 1,
        }
    }
}

impl fmt::Display for ModelsError {
    /// Formats a human-readable diagnostic message for the model registry error.
    ///
    /// Accepts a mutable formatter. Returns `fmt::Result` after writing the
    /// error message without exposing any secret values. Fails only if formatting fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(msg) | Self::Config(msg) => f.write_str(msg),
            Self::Io(err) => write!(f, "models configuration I/O error: {err}"),
        }
    }
}

impl std::error::Error for ModelsError {
    /// Returns the underlying OS error when this error wraps an I/O failure.
    ///
    /// Accepts `&self`. Returns `Some` for [`ModelsError::Io`] and `None` otherwise.
    /// Cannot fail.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Usage(_) | Self::Config(_) => None,
        }
    }
}

impl From<std::io::Error> for ModelsError {
    /// Converts a standard I/O error into [`ModelsError::Io`].
    ///
    /// Accepts an `std::io::Error`. Returns [`ModelsError::Io`]. Cannot fail.
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Wire protocol used when talking to an OpenAI-compatible ASR endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiProtocol {
    /// Multipart WAV upload to `/audio/transcriptions` (default).
    #[default]
    Transcriptions,
    /// Base64 WAV `input_audio` JSON request to `/chat/completions`.
    Chat,
}

impl OpenAiProtocol {
    /// Returns the canonical lowercase string name for this wire protocol.
    ///
    /// Accepts `self`. Returns `"transcriptions"` or `"chat"`. Cannot fail.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transcriptions => "transcriptions",
            Self::Chat => "chat",
        }
    }
}

impl fmt::Display for OpenAiProtocol {
    /// Formats the protocol using its canonical lowercase identifier.
    ///
    /// Accepts a mutable formatter. Returns `fmt::Result`. Fails only if formatting fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OpenAiProtocol {
    type Err = ModelsError;

    /// Parses an OpenAI wire protocol from `"transcriptions"` or `"chat"`.
    ///
    /// Accepts a string slice. Returns the parsed [`OpenAiProtocol`]. Returns
    /// [`ModelsError::Usage`] on any unsupported value.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "transcriptions" => Ok(Self::Transcriptions),
            "chat" => Ok(Self::Chat),
            _ => Err(ModelsError::Usage(format!(
                "invalid ASR protocol '{}': expected 'transcriptions' or 'chat'",
                raw.trim()
            ))),
        }
    }
}

/// Execution device for local offline Whisper ONNX inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnnxDevice {
    /// CPU execution provider (default).
    #[default]
    Cpu,
    /// CUDA execution provider (fails closed when unavailable).
    Cuda,
}

impl OnnxDevice {
    /// Returns the canonical lowercase string name for this ONNX execution device.
    ///
    /// Accepts `self`. Returns `"cpu"` or `"cuda"`. Cannot fail.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
        }
    }
}

impl fmt::Display for OnnxDevice {
    /// Formats the ONNX device using its canonical lowercase identifier.
    ///
    /// Accepts a mutable formatter. Returns `fmt::Result`. Fails only if formatting fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OnnxDevice {
    type Err = ModelsError;

    /// Parses a local ONNX device from `"cpu"` or `"cuda"`.
    ///
    /// Accepts a string slice. Returns the parsed [`OnnxDevice`]. Returns
    /// [`ModelsError::Usage`] on any unsupported value.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "cpu" => Ok(Self::Cpu),
            "cuda" => Ok(Self::Cuda),
            _ => Err(ModelsError::Usage(format!(
                "invalid ONNX device '{}': expected 'cpu' or 'cuda'",
                raw.trim()
            ))),
        }
    }
}

/// Supported ASR backend kinds for explicit selection and disambiguation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AsrBackendKind {
    /// Remote or local HTTP OpenAI-compatible ASR endpoint.
    Openai,
    /// Local offline Whisper ONNX runtime backend.
    WhisperOnnx,
}

impl AsrBackendKind {
    /// Returns the canonical CLI string identifier for this ASR backend kind.
    ///
    /// Accepts `self`. Returns `"openai"` or `"whisper-onnx"`. Cannot fail.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::WhisperOnnx => "whisper-onnx",
        }
    }
}

impl fmt::Display for AsrBackendKind {
    /// Formats the ASR backend kind using its canonical identifier.
    ///
    /// Accepts a mutable formatter. Returns `fmt::Result`. Fails only if formatting fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AsrBackendKind {
    type Err = ModelsError;

    /// Parses an ASR backend selector from `"openai"` or `"whisper-onnx"`.
    ///
    /// Accepts a string slice (also accepting `"whisper_onnx"`). Returns the parsed
    /// [`AsrBackendKind`]. Returns [`ModelsError::Usage`] on any unsupported value.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" => Ok(Self::Openai),
            "whisper-onnx" | "whisper_onnx" => Ok(Self::WhisperOnnx),
            _ => Err(ModelsError::Usage(format!(
                "invalid ASR backend '{}': expected 'openai' or 'whisper-onnx'",
                raw.trim()
            ))),
        }
    }
}

/// Precedence tier that determined the resolved ASR configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    /// Resolved from command-line flags.
    Cli,
    /// Resolved from `GLASSBOX_ASR_*` environment variables.
    Env,
    /// Resolved from persistent `models.json`.
    ModelsConfig,
}

impl ConfigSource {
    /// Returns a human-readable source tag for diagnostics and reports.
    ///
    /// Accepts `self`. Returns `"cli"`, `"env"`, or `"models.json"`. Cannot fail.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Env => "env",
            Self::ModelsConfig => "models.json",
        }
    }
}

/// Named OpenAI-compatible endpoint entry stored inside `models.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointConfig {
    /// Normalized `http` or `https` base URL without trailing slashes.
    pub base_url: String,
    /// Optional environment variable name storing the Bearer API key (never the raw key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// Capability binding entry stored inside `models.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapabilityBinding {
    /// External or local HTTP OpenAI-compatible model binding.
    Openai {
        /// Key referencing an entry in [`ModelsConfig::endpoints`].
        endpoint: String,
        /// Remote model identifier sent in API payloads.
        model: String,
        /// OpenAI wire protocol (`transcriptions` or `chat`).
        #[serde(default)]
        protocol: OpenAiProtocol,
    },
    /// Local offline ONNX model directory binding.
    LocalOnnx {
        /// Local filesystem directory containing ONNX encoder/decoder weights and tokenizer files.
        model_dir: PathBuf,
        /// Execution provider (`cpu` or `cuda`).
        #[serde(default)]
        device: OnnxDevice,
    },
}

/// Persistent user model registry persisted in `models.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsConfig {
    /// Config schema version (`1`).
    pub schema_version: u32,
    /// Named OpenAI-compatible HTTP endpoints.
    #[serde(default)]
    pub endpoints: BTreeMap<String, EndpointConfig>,
    /// Capability bindings keyed by capability name (such as `"asr"`).
    #[serde(default)]
    pub bindings: BTreeMap<String, CapabilityBinding>,
}

impl Default for ModelsConfig {
    /// Creates an empty version-1 [`ModelsConfig`] with no endpoints or bindings.
    ///
    /// Accepts no arguments. Returns a default [`ModelsConfig`]. Cannot fail.
    fn default() -> Self {
        Self {
            schema_version: MODELS_SCHEMA_VERSION,
            endpoints: BTreeMap::new(),
            bindings: BTreeMap::new(),
        }
    }
}

/// CLI override values supplied to `glassbox extract --asr`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AsrCliOverrides {
    /// Explicit `--asr-backend` selection (`openai` or `whisper-onnx`).
    pub backend: Option<AsrBackendKind>,
    /// Explicit `--asr-url` override for OpenAI-compatible endpoints.
    pub url: Option<String>,
    /// Explicit `--asr-model` override for OpenAI-compatible endpoints.
    pub model: Option<String>,
    /// Explicit `--asr-api-key-env` override naming the Bearer-token env var.
    pub api_key_env: Option<String>,
    /// Explicit `--asr-protocol` override (`transcriptions` or `chat`).
    pub protocol: Option<OpenAiProtocol>,
    /// Explicit `--asr-model-dir` override for local Whisper ONNX.
    pub model_dir: Option<PathBuf>,
    /// Explicit `--asr-device` override (`cpu` or `cuda`).
    pub device: Option<OnnxDevice>,
    /// Explicit per-window HTTP timeout override in seconds.
    pub timeout_secs: Option<u64>,
}

/// Fully validated ASR backend configuration ready to construct an `AsrBackend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAsrConfig {
    /// OpenAI-compatible HTTP backend settings.
    OpenAi {
        /// Normalized `http`/`https` base URL without trailing slash.
        base_url: String,
        /// Non-empty model identifier.
        model: String,
        /// Optional validated environment variable name holding the Bearer token.
        api_key_env: Option<String>,
        /// Wire protocol (`transcriptions` or `chat`).
        protocol: OpenAiProtocol,
        /// Per-window HTTP read timeout in seconds.
        timeout_secs: u64,
        /// Highest precedence tier that configured this backend.
        source: ConfigSource,
    },
    /// Local offline Whisper ONNX backend settings.
    WhisperOnnx {
        /// Local model directory path.
        model_dir: PathBuf,
        /// Execution device (`cpu` or `cuda`).
        device: OnnxDevice,
        /// Highest precedence tier that configured this backend.
        source: ConfigSource,
    },
}

/// Validates that an environment variable name matches `^[A-Za-z_][A-Za-z0-9_]*$`.
///
/// Accepts the candidate environment variable name. Returns `Ok(())` when valid.
/// Returns [`ModelsError::Usage`] (exit `64`) for empty strings, whitespace,
/// hyphens, leading digits, raw API keys such as `sk-...`, or non-ASCII bytes.
pub fn validate_env_var_name(name: &str) -> Result<(), ModelsError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(ModelsError::Usage(
            "invalid API key environment variable name '': expected an env var name matching ^[A-Za-z_][A-Za-z0-9_]*$ (never pass a raw secret)".to_string(),
        ));
    };
    let valid_first = first.is_ascii_alphabetic() || first == '_';
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_first || !valid_rest {
        return Err(ModelsError::Usage(
            "invalid API key environment variable name: expected an env var identifier matching ^[A-Za-z_][A-Za-z0-9_]*$ (never pass a raw secret)".to_string(),
        ));
    }
    Ok(())
}

/// Validates and normalizes an OpenAI-compatible HTTP(S) base URL.
///
/// Accepts a raw URL string. Returns the normalized URL string with trailing `/`
/// characters stripped. Returns [`ModelsError::Usage`] (exit `64`) if the URL
/// cannot be parsed, uses a scheme other than `http` or `https`, lacks a host,
/// or embeds credentials, query parameters, or fragments.
pub fn validate_base_url(raw: &str) -> Result<String, ModelsError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ModelsError::Usage(
            "invalid endpoint URL: URL must not be empty".to_string(),
        ));
    }
    let parsed = Url::parse(trimmed)
        .map_err(|err| ModelsError::Usage(format!("invalid endpoint URL '{trimmed}': {err}")))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(ModelsError::Usage(format!(
            "invalid endpoint URL '{trimmed}': scheme must be http or https"
        )));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(ModelsError::Usage(format!(
            "invalid endpoint URL '{trimmed}': host must not be empty"
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ModelsError::Usage(
            "invalid endpoint URL: embedded credentials are not allowed; use --api-key-env"
                .to_string(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ModelsError::Usage(
            "invalid endpoint URL: query strings and fragments are not allowed in a base URL"
                .to_string(),
        ));
    }
    let mut normalized = parsed.to_string();
    while normalized.ends_with('/') {
        normalized.pop();
    }
    Ok(normalized)
}

/// Validates an endpoint or capability identifier (`^[A-Za-z0-9][A-Za-z0-9._-]*$`).
///
/// Accepts the identifier kind label (for error context) and candidate name.
/// Returns `Ok(())` when valid, or [`ModelsError::Usage`] when empty or containing
/// whitespace, path separators, or non-identifier characters.
fn validate_identifier(kind: &str, name: &str) -> Result<(), ModelsError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(ModelsError::Usage(format!(
            "invalid {kind} name: must not be empty"
        )));
    };
    let valid_first = first.is_ascii_alphanumeric();
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid_first || !valid_rest {
        return Err(ModelsError::Usage(format!(
            "invalid {kind} name '{name}': use ASCII letters, digits, '.', '_', or '-'"
        )));
    }
    Ok(())
}

/// Resolves the persistent `models.json` path from the live process environment.
///
/// Accepts no arguments. Returns `$GLASSBOX_CONFIG_DIR/models.json`, otherwise
/// `$XDG_CONFIG_HOME/glassbox/models.json`, otherwise `$HOME/.config/glassbox/models.json`.
/// Returns [`ModelsError::Config`] when none of those environment variables are set.
pub fn resolve_models_path() -> Result<PathBuf, ModelsError> {
    resolve_models_path_with(|key| std::env::var(key).ok())
}

/// Resolves the persistent `models.json` path using a caller-supplied environment lookup.
///
/// Accepts an environment lookup closure `env(name) -> Option<String>`. Returns
/// the resolved `models.json` path following `$GLASSBOX_CONFIG_DIR` ->
/// `$XDG_CONFIG_HOME/glassbox` -> `$HOME/.config/glassbox`. Returns
/// [`ModelsError::Config`] when none of those variables are non-empty.
pub fn resolve_models_path_with<F>(env: F) -> Result<PathBuf, ModelsError>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(dir) = non_empty_env(&env, ENV_CONFIG_DIR) {
        return Ok(PathBuf::from(dir).join("models.json"));
    }
    if let Some(xdg) = non_empty_env(&env, ENV_XDG_CONFIG_HOME) {
        return Ok(PathBuf::from(xdg).join("glassbox").join("models.json"));
    }
    if let Some(home) = non_empty_env(&env, ENV_HOME) {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("glassbox")
            .join("models.json"));
    }
    Err(ModelsError::Config(
        "cannot determine models.json path: set GLASSBOX_CONFIG_DIR, XDG_CONFIG_HOME, or HOME"
            .to_string(),
    ))
}

/// Loads `models.json` from the resolved environment config location.
///
/// Accepts no arguments. Returns [`ModelsConfig::default`] when `models.json`
/// does not exist yet. Returns [`ModelsError::Config`] on unresolvable paths,
/// corrupt JSON, or unsupported schema versions, and [`ModelsError::Io`] on I/O errors.
pub fn load_config() -> Result<ModelsConfig, ModelsError> {
    let path = resolve_models_path()?;
    load_config_from(&path)
}

/// Loads `models.json` from an explicit file path.
///
/// Accepts the target `models.json` file path. Returns [`ModelsConfig::default`]
/// when the file does not exist. Returns [`ModelsError::Config`] on invalid JSON
/// or unsupported `schema_version`, and [`ModelsError::Io`] on read failures.
pub fn load_config_from(path: &Path) -> Result<ModelsConfig, ModelsError> {
    let raw = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelsConfig::default());
        }
        Err(err) => return Err(ModelsError::Io(err)),
    };
    let config: ModelsConfig = serde_json::from_str(&raw).map_err(|err| {
        ModelsError::Config(format!(
            "failed to parse models configuration at {}: {err}",
            path.display()
        ))
    })?;
    if config.schema_version != MODELS_SCHEMA_VERSION {
        return Err(ModelsError::Config(format!(
            "unsupported models.json schema_version {} at {} (expected {})",
            config.schema_version,
            path.display(),
            MODELS_SCHEMA_VERSION
        )));
    }
    Ok(config)
}

/// Persists `models.json` atomically at the resolved environment config location.
///
/// Accepts a [`ModelsConfig`] reference. Returns the written `models.json` path
/// after creating the parent directory with mode `0700`, writing a hidden `0600`
/// temporary file, calling `sync_all`, and atomically renaming it into place.
/// Returns [`ModelsError::Usage`] if any stored endpoint violates secret/URL hygiene,
/// [`ModelsError::Config`] on unresolvable paths or serialization errors, or
/// [`ModelsError::Io`] on filesystem failures.
pub fn save_config(config: &ModelsConfig) -> Result<PathBuf, ModelsError> {
    let path = resolve_models_path()?;
    save_config_to(&path, config)?;
    Ok(path)
}

/// Persists `models.json` atomically to an explicit file path with `0700`/`0600` permissions.
///
/// Accepts the target file path and [`ModelsConfig`]. Returns `Ok(())` once the
/// file is atomically renamed into place and synced. Cleans up the staged hidden
/// temp file on any error. Rejects symlink components in the parent directory
/// and re-validates all endpoint URLs and `api_key_env` identifiers so raw
/// secrets are never written to disk.
pub fn save_config_to(path: &Path, config: &ModelsConfig) -> Result<(), ModelsError> {
    if config.schema_version != MODELS_SCHEMA_VERSION {
        return Err(ModelsError::Config(format!(
            "unsupported models.json schema_version {} (expected {})",
            config.schema_version, MODELS_SCHEMA_VERSION
        )));
    }
    for (name, ep) in &config.endpoints {
        validate_identifier("endpoint", name)?;
        validate_base_url(&ep.base_url)?;
        if let Some(env_name) = &ep.api_key_env {
            validate_env_var_name(env_name)?;
        }
    }

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            ModelsError::Config(format!(
                "invalid models.json path without parent directory: {}",
                path.display()
            ))
        })?;
    ensure_private_config_dir(parent)?;

    let mut payload = serde_json::to_vec_pretty(config)
        .map_err(|err| ModelsError::Config(format!("failed to serialize models.json: {err}")))?;
    payload.push(b'\n');

    let (tmp_path, mut tmp_file) = create_private_temp_file(parent)?;
    let write_result = (|| -> std::io::Result<()> {
        tmp_file.write_all(&payload)?;
        tmp_file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp_file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();

    drop(tmp_file);

    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(ModelsError::Io(err));
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ModelsError::Io(err));
    }

    // Best-effort directory metadata durability sync.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Ensures the parent configuration directory exists as a real directory with `0700` permissions.
///
/// Accepts the target parent directory path. Returns `Ok(())` when every missing
/// component has been created with mode `0700` and the leaf directory is a real
/// directory with `0700` permissions on Unix. Returns [`ModelsError::Io`] if any
/// traversed component is a symbolic link or non-directory, or if creation fails.
fn ensure_private_config_dir(parent: &Path) -> Result<(), ModelsError> {
    let mut current = PathBuf::new();
    for component in parent.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(ModelsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "symbolic-link config directory component rejected: {}",
                        current.display()
                    ),
                )));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(ModelsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "config directory component is not a directory: {}",
                        current.display()
                    ),
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                create_dir_0700(&current)?;
            }
            Err(err) => return Err(ModelsError::Io(err)),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if parent.parent().is_some() {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Creates a single missing directory component atomically with mode `0700` on Unix.
///
/// Accepts the missing directory path. Returns `Ok(())` when created or if a
/// concurrent caller created a real directory at the same path. Returns
/// [`ModelsError::Io`] on creation failure or if a symlink/file won the race.
fn create_dir_0700(path: &Path) -> Result<(), ModelsError> {
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    };
    #[cfg(not(unix))]
    let created = fs::create_dir(path);

    match created {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            match fs::symlink_metadata(path) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(()),
                _ => Err(ModelsError::Io(err)),
            }
        }
        Err(err) => Err(ModelsError::Io(err)),
    }
}

/// Creates an exclusive hidden temporary file with mode `0600` inside `parent`.
///
/// Accepts the validated config parent directory. Returns `(temp_path, File)`
/// opened with `O_CREAT | O_EXCL` (and `O_NOFOLLOW` + mode `0600` on Unix).
/// Returns [`ModelsError::Io`] if a unique file cannot be created after bounded retries.
fn create_private_temp_file(parent: &Path) -> Result<(PathBuf, fs::File), ModelsError> {
    let pid = std::process::id();
    for _ in 0..64 {
        let seq = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".models.json.tmp.{pid}.{seq}"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(ModelsError::Io(err)),
        }
    }
    Err(ModelsError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to allocate unique temporary file for models.json",
    )))
}

impl ModelsConfig {
    /// Adds or replaces a named OpenAI-compatible endpoint entry.
    ///
    /// Accepts the endpoint name, base URL, and optional API-key environment
    /// variable name. Returns the stored normalized [`EndpointConfig`]. Returns
    /// [`ModelsError::Usage`] (exit `64`) if the name, URL, or env var name is invalid.
    pub fn add_endpoint(
        &mut self,
        name: &str,
        base_url: &str,
        api_key_env: Option<&str>,
    ) -> Result<EndpointConfig, ModelsError> {
        validate_identifier("endpoint", name)?;
        let normalized_url = validate_base_url(base_url)?;
        let validated_env = match api_key_env {
            Some(var) => {
                validate_env_var_name(var)?;
                Some(var.to_string())
            }
            None => None,
        };
        let entry = EndpointConfig {
            base_url: normalized_url,
            api_key_env: validated_env,
        };
        self.endpoints.insert(name.to_string(), entry.clone());
        Ok(entry)
    }

    /// Removes a named endpoint, refusing to break active bindings unless `force` is true.
    ///
    /// Accepts the endpoint name and `force` flag. Returns the sorted list of
    /// removed dangling capability bindings (empty when none referenced the endpoint).
    /// Returns [`ModelsError::Usage`] (exit `64`) if the endpoint does not exist
    /// or if bindings reference it while `force` is `false`.
    pub fn remove_endpoint(&mut self, name: &str, force: bool) -> Result<Vec<String>, ModelsError> {
        if !self.endpoints.contains_key(name) {
            return Err(ModelsError::Usage(format!(
                "endpoint '{name}' does not exist"
            )));
        }
        let referencing: Vec<String> = self
            .bindings
            .iter()
            .filter_map(|(cap, binding)| match binding {
                CapabilityBinding::Openai { endpoint, .. } if endpoint == name => Some(cap.clone()),
                _ => None,
            })
            .collect();
        if !referencing.is_empty() && !force {
            return Err(ModelsError::Usage(format!(
                "endpoint '{name}' is referenced by capability binding(s) [{}]; unbind first or pass --force",
                referencing.join(", ")
            )));
        }
        self.endpoints.remove(name);
        for cap in &referencing {
            self.bindings.remove(cap);
        }
        Ok(referencing)
    }

    /// Returns all configured endpoints in deterministic lexical name order.
    ///
    /// Accepts `&self`. Returns a vector of `(name, &EndpointConfig)` tuples.
    /// Cannot fail.
    pub fn list_endpoints(&self) -> Vec<(&str, &EndpointConfig)> {
        self.endpoints
            .iter()
            .map(|(name, cfg)| (name.as_str(), cfg))
            .collect()
    }

    /// Binds a capability (such as `"asr"`) to a named OpenAI-compatible endpoint and model.
    ///
    /// Accepts capability name, endpoint name, model identifier, and wire protocol.
    /// Returns the stored [`CapabilityBinding`]. Returns [`ModelsError::Usage`] (exit `64`)
    /// if the capability is invalid, the endpoint is not registered, or the model is blank.
    pub fn bind_openai(
        &mut self,
        capability: &str,
        endpoint: &str,
        model: &str,
        protocol: OpenAiProtocol,
    ) -> Result<CapabilityBinding, ModelsError> {
        validate_identifier("capability", capability)?;
        if !self.endpoints.contains_key(endpoint) {
            return Err(ModelsError::Usage(format!(
                "cannot bind capability '{capability}': endpoint '{endpoint}' is not configured"
            )));
        }
        let trimmed_model = model.trim();
        if trimmed_model.is_empty() {
            return Err(ModelsError::Usage(format!(
                "cannot bind capability '{capability}': model identifier must not be empty"
            )));
        }
        let binding = CapabilityBinding::Openai {
            endpoint: endpoint.to_string(),
            model: trimmed_model.to_string(),
            protocol,
        };
        self.bindings
            .insert(capability.to_string(), binding.clone());
        Ok(binding)
    }

    /// Binds a capability (such as `"asr"`) to a local offline ONNX model directory.
    ///
    /// Accepts capability name, local `model_dir`, and execution `device`. Returns
    /// the stored [`CapabilityBinding`]. Returns [`ModelsError::Usage`] (exit `64`)
    /// if the capability is invalid or `model_dir` is empty.
    pub fn bind_local_onnx(
        &mut self,
        capability: &str,
        model_dir: PathBuf,
        device: OnnxDevice,
    ) -> Result<CapabilityBinding, ModelsError> {
        validate_identifier("capability", capability)?;
        if model_dir.as_os_str().is_empty() {
            return Err(ModelsError::Usage(format!(
                "cannot bind capability '{capability}': --model-dir must not be empty"
            )));
        }
        let binding = CapabilityBinding::LocalOnnx { model_dir, device };
        self.bindings
            .insert(capability.to_string(), binding.clone());
        Ok(binding)
    }

    /// Removes the configured binding for a capability.
    ///
    /// Accepts the capability name. Returns the removed [`CapabilityBinding`].
    /// Returns [`ModelsError::Usage`] (exit `64`) when the capability is invalid
    /// or currently unbound.
    pub fn unbind(&mut self, capability: &str) -> Result<CapabilityBinding, ModelsError> {
        validate_identifier("capability", capability)?;
        self.bindings.remove(capability).ok_or_else(|| {
            ModelsError::Usage(format!(
                "no binding is configured for capability '{capability}'"
            ))
        })
    }

    /// Formats configured endpoints as deterministic plain text without reading secrets.
    ///
    /// Accepts `&self`. Returns a multi-line human-readable listing showing endpoint
    /// names, normalized base URLs, and `api_key_env` names only. Cannot fail.
    pub fn format_endpoints_text(&self) -> String {
        if self.endpoints.is_empty() {
            return "No endpoints configured.".to_string();
        }
        let mut lines = Vec::with_capacity(self.endpoints.len());
        for (name, ep) in &self.endpoints {
            match &ep.api_key_env {
                Some(env_var) => {
                    lines.push(format!("{name}\t{}\tapi_key_env={env_var}", ep.base_url));
                }
                None => {
                    lines.push(format!("{name}\t{}\tapi_key_env=(none)", ep.base_url));
                }
            }
        }
        lines.join("\n")
    }

    /// Formats the full registry or a single capability binding as redacted plain text.
    ///
    /// Accepts an optional capability name filter. Returns human-readable text that
    /// displays only `api_key_env` variable names and never resolves or prints secret
    /// values. Cannot fail.
    pub fn format_show_text(&self, capability: Option<&str>) -> String {
        if let Some(cap) = capability {
            return match self.bindings.get(cap) {
                Some(binding) => self.format_binding_line(cap, binding),
                None => format!("{cap}: (unbound)"),
            };
        }
        let mut sections = Vec::new();
        sections.push("Endpoints:".to_string());
        if self.endpoints.is_empty() {
            sections.push("  (none)".to_string());
        } else {
            for (name, ep) in &self.endpoints {
                let auth = ep
                    .api_key_env
                    .as_deref()
                    .map(|v| format!("api_key_env={v}"))
                    .unwrap_or_else(|| "api_key_env=(none)".to_string());
                sections.push(format!("  {name}: {} ({auth})", ep.base_url));
            }
        }
        sections.push("Bindings:".to_string());
        if self.bindings.is_empty() {
            sections.push("  (none)".to_string());
        } else {
            for (cap, binding) in &self.bindings {
                sections.push(format!("  {}", self.format_binding_line(cap, binding)));
            }
        }
        sections.join("\n")
    }

    /// Formats one capability binding line without resolving any environment secret.
    ///
    /// Accepts the capability name and [`CapabilityBinding`]. Returns one summary
    /// line containing endpoint URL and `api_key_env` variable name only. Cannot fail.
    fn format_binding_line(&self, capability: &str, binding: &CapabilityBinding) -> String {
        match binding {
            CapabilityBinding::Openai {
                endpoint,
                model,
                protocol,
            } => {
                let ep_detail = match self.endpoints.get(endpoint) {
                    Some(ep) => {
                        let auth = ep
                            .api_key_env
                            .as_deref()
                            .map(|v| format!(", api_key_env={v}"))
                            .unwrap_or_default();
                        format!("url={}{auth}", ep.base_url)
                    }
                    None => "url=(missing endpoint)".to_string(),
                };
                format!(
                    "{capability}: openai endpoint={endpoint} model={model} protocol={protocol} ({ep_detail})"
                )
            }
            CapabilityBinding::LocalOnnx { model_dir, device } => {
                format!(
                    "{capability}: local_onnx model_dir={} device={device}",
                    model_dir.display()
                )
            }
        }
    }

    /// Serializes the full registry or one capability binding as redacted JSON.
    ///
    /// Accepts an optional capability name filter. Returns a `serde_json::Value`
    /// containing only stored configuration metadata (never resolved env-var secrets).
    /// Returns [`ModelsError::Usage`] if the supplied capability name is invalid,
    /// or [`ModelsError::Config`] if serialization fails.
    pub fn show_json(&self, capability: Option<&str>) -> Result<Value, ModelsError> {
        if let Some(cap) = capability {
            validate_identifier("capability", cap)?;
            let binding = self.bindings.get(cap);
            let endpoint = match binding {
                Some(CapabilityBinding::Openai { endpoint, .. }) => self.endpoints.get(endpoint),
                _ => None,
            };
            return Ok(json!({
                "schema_version": self.schema_version,
                "capability": cap,
                "binding": binding,
                "endpoint": endpoint,
            }));
        }
        serde_json::to_value(self)
            .map_err(|err| ModelsError::Config(format!("failed to serialize models config: {err}")))
    }
}

/// Resolves ASR backend configuration using live process environment variables.
///
/// Accepts CLI overrides and the loaded [`ModelsConfig`]. Returns a validated
/// [`ResolvedAsrConfig`] following strict `CLI > env > models.json` precedence.
/// Fails closed with [`ModelsError::Usage`] (exit `64`) on ambiguous tiers,
/// invalid identifiers/URLs, or missing required backend fields, and
/// [`ModelsError::Config`] when a `models.json` binding references a missing endpoint.
pub fn resolve_asr_config(
    cli: &AsrCliOverrides,
    config: &ModelsConfig,
) -> Result<ResolvedAsrConfig, ModelsError> {
    resolve_asr_config_with_env(cli, config, |key| std::env::var(key).ok())
}

/// Resolves ASR backend configuration using an injected environment lookup closure.
///
/// Accepts CLI overrides, loaded [`ModelsConfig`], and `env(name) -> Option<String>`.
/// Enforces strict `CLI > env > models.json` precedence, rejects simultaneous
/// OpenAI and Whisper ONNX indicators at the active tier without explicit backend
/// selection, and avoids leaking a `models.json` endpoint's `api_key_env` when
/// `base_url` is overridden from CLI or env. Returns [`ResolvedAsrConfig`] or a
/// fail-closed [`ModelsError`].
pub fn resolve_asr_config_with_env<F>(
    cli: &AsrCliOverrides,
    config: &ModelsConfig,
    env: F,
) -> Result<ResolvedAsrConfig, ModelsError>
where
    F: Fn(&str) -> Option<String>,
{
    let env_backend = parse_env_opt::<AsrBackendKind>(&env, ENV_ASR_BACKEND)?;
    let env_url = non_empty_env(&env, ENV_ASR_URL);
    let env_model = non_empty_env(&env, ENV_ASR_MODEL);
    let env_api_key_env = non_empty_env(&env, ENV_ASR_API_KEY_ENV);
    let env_protocol = parse_env_opt::<OpenAiProtocol>(&env, ENV_ASR_PROTOCOL)?;
    let env_model_dir = non_empty_env(&env, ENV_ASR_MODEL_DIR).map(PathBuf::from);
    let env_device = parse_env_opt::<OnnxDevice>(&env, ENV_ASR_DEVICE)?;
    let env_timeout_secs = parse_positive_u64_env(&env, ENV_ASR_TIMEOUT_SECS)?;

    if let Some(0) = cli.timeout_secs {
        return Err(ModelsError::Usage(
            "ASR timeout must be at least 1 second".to_string(),
        ));
    }
    if let Some(ref var) = cli.api_key_env {
        validate_env_var_name(var)?;
    }
    if let Some(ref var) = env_api_key_env {
        validate_env_var_name(var)?;
    }

    let timeout_secs = cli
        .timeout_secs
        .or(env_timeout_secs)
        .unwrap_or(DEFAULT_ASR_TIMEOUT_SECS);

    let cli_has_openai = cli.url.is_some();
    let cli_has_onnx = cli.model_dir.is_some();
    let env_has_openai = env_url.is_some();
    let env_has_onnx = env_model_dir.is_some();

    let file_binding = config.bindings.get(CAPABILITY_ASR);

    let (selected_backend, backend_source) = if let Some(explicit) = cli.backend {
        (explicit, ConfigSource::Cli)
    } else if cli_has_openai && cli_has_onnx {
        return Err(ModelsError::Usage(
            "ambiguous ASR backend: both OpenAI endpoint and Whisper ONNX model dir are configured; set --asr-backend or GLASSBOX_ASR_BACKEND".to_string(),
        ));
    } else if cli_has_openai {
        (AsrBackendKind::Openai, ConfigSource::Cli)
    } else if cli_has_onnx {
        (AsrBackendKind::WhisperOnnx, ConfigSource::Cli)
    } else if let Some(explicit) = env_backend {
        (explicit, ConfigSource::Env)
    } else if env_has_openai && env_has_onnx {
        return Err(ModelsError::Usage(
            "ambiguous ASR backend: both OpenAI endpoint and Whisper ONNX model dir are configured; set --asr-backend or GLASSBOX_ASR_BACKEND".to_string(),
        ));
    } else if env_has_openai {
        (AsrBackendKind::Openai, ConfigSource::Env)
    } else if env_has_onnx {
        (AsrBackendKind::WhisperOnnx, ConfigSource::Env)
    } else if let Some(binding) = file_binding {
        match binding {
            CapabilityBinding::Openai { .. } => {
                (AsrBackendKind::Openai, ConfigSource::ModelsConfig)
            }
            CapabilityBinding::LocalOnnx { .. } => {
                (AsrBackendKind::WhisperOnnx, ConfigSource::ModelsConfig)
            }
        }
    } else {
        return Err(ModelsError::Usage(
            "no ASR backend configured: pass --asr-url/--asr-model or --asr-model-dir, set GLASSBOX_ASR_* env vars, or bind one with `glassbox models bind asr`".to_string(),
        ));
    };

    match selected_backend {
        AsrBackendKind::Openai => {
            let file_openai = match file_binding {
                Some(CapabilityBinding::Openai {
                    endpoint,
                    model,
                    protocol,
                }) => {
                    let ep = config.endpoints.get(endpoint).ok_or_else(|| {
                        ModelsError::Config(format!(
                            "models.json ASR binding references unknown endpoint '{endpoint}'"
                        ))
                    })?;
                    Some((ep, model.as_str(), *protocol))
                }
                _ => None,
            };

            let url_overridden = cli.url.is_some() || env_url.is_some();
            let raw_url = cli
                .url
                .as_deref()
                .or(env_url.as_deref())
                .or_else(|| file_openai.map(|(ep, _, _)| ep.base_url.as_str()))
                .ok_or_else(|| {
                    ModelsError::Usage(
                        "OpenAI ASR backend requires a base URL (--asr-url, GLASSBOX_ASR_URL, or `glassbox models bind asr`)".to_string(),
                    )
                })?;
            let base_url = validate_base_url(raw_url)?;

            let model = cli
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| env_model.as_deref().map(str::trim).filter(|s| !s.is_empty()))
                .or_else(|| file_openai.map(|(_, m, _)| m))
                .ok_or_else(|| {
                    ModelsError::Usage(
                        "OpenAI ASR backend requires a model ID (--asr-model, GLASSBOX_ASR_MODEL, or `glassbox models bind asr`)".to_string(),
                    )
                })?
                .to_string();

            // Never inherit a models.json endpoint's API key env var when the caller pointed at a different URL.
            let api_key_env = if let Some(ref var) = cli.api_key_env {
                Some(var.clone())
            } else if let Some(ref var) = env_api_key_env {
                Some(var.clone())
            } else if !url_overridden {
                file_openai.and_then(|(ep, _, _)| ep.api_key_env.clone())
            } else {
                None
            };
            if let Some(ref var) = api_key_env {
                validate_env_var_name(var)?;
            }

            let protocol = cli
                .protocol
                .or(env_protocol)
                .or_else(|| file_openai.map(|(_, _, p)| p))
                .unwrap_or_default();

            let source = highest_source(
                backend_source,
                cli.url.is_some()
                    || cli.model.is_some()
                    || cli.api_key_env.is_some()
                    || cli.protocol.is_some()
                    || cli.timeout_secs.is_some(),
                env_url.is_some()
                    || env_model.is_some()
                    || env_api_key_env.is_some()
                    || env_protocol.is_some()
                    || env_timeout_secs.is_some(),
            );

            Ok(ResolvedAsrConfig::OpenAi {
                base_url,
                model,
                api_key_env,
                protocol,
                timeout_secs,
                source,
            })
        }
        AsrBackendKind::WhisperOnnx => {
            let file_onnx = match file_binding {
                Some(CapabilityBinding::LocalOnnx { model_dir, device }) => {
                    Some((model_dir.as_path(), *device))
                }
                _ => None,
            };

            let model_dir = cli
                .model_dir
                .clone()
                .filter(|p| !p.as_os_str().is_empty())
                .or_else(|| env_model_dir.clone().filter(|p| !p.as_os_str().is_empty()))
                .or_else(|| file_onnx.map(|(dir, _)| dir.to_path_buf()))
                .ok_or_else(|| {
                    ModelsError::Usage(
                        "Whisper ONNX ASR backend requires a model directory (--asr-model-dir, GLASSBOX_ASR_MODEL_DIR, or `glassbox models bind asr --local-onnx`)".to_string(),
                    )
                })?;

            let device = cli
                .device
                .or(env_device)
                .or_else(|| file_onnx.map(|(_, d)| d))
                .unwrap_or_default();

            let source = highest_source(
                backend_source,
                cli.model_dir.is_some() || cli.device.is_some(),
                env_model_dir.is_some() || env_device.is_some(),
            );

            Ok(ResolvedAsrConfig::WhisperOnnx {
                model_dir,
                device,
                source,
            })
        }
    }
}

/// Computes the highest precedence tier that contributed to the resolved backend.
///
/// Accepts the base backend source and booleans indicating CLI or env overrides.
/// Returns [`ConfigSource::Cli`] > [`ConfigSource::Env`] > [`ConfigSource::ModelsConfig`].
/// Cannot fail.
fn highest_source(base: ConfigSource, has_cli: bool, has_env: bool) -> ConfigSource {
    if base == ConfigSource::Cli || has_cli {
        ConfigSource::Cli
    } else if base == ConfigSource::Env || has_env {
        ConfigSource::Env
    } else {
        ConfigSource::ModelsConfig
    }
}

/// Reads an environment variable via closure and returns `Some(trimmed)` only when non-empty.
///
/// Accepts the env lookup closure and variable name. Returns `None` if absent or
/// whitespace-only, or `Some(String)` with surrounding whitespace removed. Cannot fail.
fn non_empty_env<F>(env: &F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    env(name).and_then(|val| {
        let trimmed = val.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// Parses an optional environment variable via `FromStr`, returning `ModelsError::Usage` on invalid values.
///
/// Accepts the env lookup closure and variable name. Returns `Ok(None)` when
/// absent/blank, `Ok(Some(T))` when valid, or [`ModelsError::Usage`] when invalid.
fn parse_env_opt<T>(
    env: &impl Fn(&str) -> Option<String>,
    name: &str,
) -> Result<Option<T>, ModelsError>
where
    T: FromStr<Err = ModelsError>,
{
    match non_empty_env(env, name) {
        Some(raw) => T::from_str(&raw).map(Some),
        None => Ok(None),
    }
}

/// Parses an optional positive integer environment variable in seconds.
///
/// Accepts the env lookup closure and variable name. Returns `Ok(None)` when
/// absent/blank, `Ok(Some(n))` when `n >= 1`, or [`ModelsError::Usage`] on zero
/// or non-integer text.
fn parse_positive_u64_env(
    env: &impl Fn(&str) -> Option<String>,
    name: &str,
) -> Result<Option<u64>, ModelsError> {
    let Some(raw) = non_empty_env(env, name) else {
        return Ok(None);
    };
    let parsed = raw.parse::<u64>().map_err(|_| {
        ModelsError::Usage(format!(
            "invalid {name} value '{raw}': expected a positive integer number of seconds"
        ))
    })?;
    if parsed == 0 {
        return Err(ModelsError::Usage(format!(
            "invalid {name} value '0': timeout must be at least 1 second"
        )));
    }
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Counter ensuring each test uses a unique directory under `extensionalities/target/tests/models/`.
    static TEST_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Creates an isolated workspace-local test directory under `extensionalities/target/tests/models/`.
    ///
    /// Accepts a descriptive test label. Returns a fresh directory `PathBuf` inside
    /// the gitignored downstream target directory (never `/tmp`). Panics on I/O failure.
    fn make_test_dir(label: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tests/models")
            .join(format!(
                "{label}-{}-{}",
                std::process::id(),
                TEST_DIR_SEQ.fetch_add(1, AtomicOrdering::Relaxed)
            ));
        if root.exists() {
            let _ = fs::remove_dir_all(&root);
        }
        fs::create_dir_all(&root).expect("create isolated test root");
        root
    }

    /// Builds an env lookup closure backed by a static key-value slice.
    ///
    /// Accepts slice pairs `(&str, &str)`. Returns a closure suitable for
    /// [`resolve_models_path_with`] and [`resolve_asr_config_with_env`]. Cannot fail.
    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn resolves_config_path_with_exact_precedence() {
        let from_override = resolve_models_path_with(env_map(&[
            (ENV_CONFIG_DIR, "/custom/gb"),
            (ENV_XDG_CONFIG_HOME, "/xdg"),
            (ENV_HOME, "/home/frank"),
        ]))
        .expect("override path");
        assert_eq!(from_override, PathBuf::from("/custom/gb/models.json"));

        let from_xdg = resolve_models_path_with(env_map(&[
            (ENV_CONFIG_DIR, "   "),
            (ENV_XDG_CONFIG_HOME, "/xdg"),
            (ENV_HOME, "/home/frank"),
        ]))
        .expect("xdg path");
        assert_eq!(from_xdg, PathBuf::from("/xdg/glassbox/models.json"));

        let from_home =
            resolve_models_path_with(env_map(&[(ENV_HOME, "/home/frank")])).expect("home path");
        assert_eq!(
            from_home,
            PathBuf::from("/home/frank/.config/glassbox/models.json")
        );

        let missing = resolve_models_path_with(env_map(&[]));
        assert!(matches!(missing, Err(ModelsError::Config(_))));
    }

    #[test]
    fn validates_env_var_names_and_rejects_raw_keys() {
        assert!(validate_env_var_name("OPENAI_API_KEY").is_ok());
        assert!(validate_env_var_name("_LOCAL_KEY_2").is_ok());
        assert!(validate_env_var_name("a").is_ok());

        for bad in [
            "",
            "   ",
            "sk-live-1234567890",
            "OPENAI_API_KEY=secret",
            "1LEADING_DIGIT",
            "HAS SPACE",
            "HAS-HYPHEN",
            "KEY\nINJECT",
        ] {
            let err = validate_env_var_name(bad).expect_err("must reject invalid env var");
            assert_eq!(err.exit_code(), 64, "bad input '{bad}' must return exit 64");
        }
    }

    #[test]
    fn validates_and_normalizes_base_urls() {
        assert_eq!(
            validate_base_url("http://127.0.0.1:8080/v1/").expect("valid"),
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            validate_base_url("https://api.example.com/v1///").expect("valid"),
            "https://api.example.com/v1"
        );
        assert_eq!(
            validate_base_url("http://127.0.0.1:8080/").expect("valid"),
            "http://127.0.0.1:8080"
        );

        for bad in [
            "",
            "not-a-url",
            "ftp://example.com/v1",
            "file:///etc/passwd",
            "http://user:secret@example.com/v1",
            "https://example.com/v1?api_key=raw",
            "https://example.com/v1#frag",
        ] {
            let err = validate_base_url(bad).expect_err("must reject invalid url");
            assert_eq!(err.exit_code(), 64, "bad url '{bad}' must return exit 64");
        }

        let query_secret_err = validate_base_url("https://example.com/v1?api_key=sk-secret-999")
            .expect_err("must reject query string secret");
        assert_eq!(query_secret_err.exit_code(), 64);
        let query_msg = query_secret_err.to_string();
        assert!(
            !query_msg.contains("?api_key=sk-secret-999") && !query_msg.contains("sk-secret-999"),
            "query string secrets must never be echoed in error output: {query_msg}"
        );
    }

    #[test]
    fn saves_and_loads_config_atomically_with_private_permissions() {
        let root = make_test_dir("atomic-perms");
        let config_dir = root.join("nested").join("cfg");
        let models_path = config_dir.join("models.json");

        // Missing file returns empty default cleanly.
        let initial = load_config_from(&models_path).expect("default on missing");
        assert_eq!(initial, ModelsConfig::default());

        let mut cfg = ModelsConfig::default();
        cfg.add_endpoint("local", "http://127.0.0.1:8080/v1/", Some("LOCAL_ASR_KEY"))
            .expect("add endpoint");
        cfg.bind_openai(
            CAPABILITY_ASR,
            "local",
            "whisper-large-v3",
            OpenAiProtocol::Transcriptions,
        )
        .expect("bind asr");

        save_config_to(&models_path, &cfg).expect("save config");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(&config_dir)
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            let file_mode = fs::metadata(&models_path)
                .expect("file meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "config dir must be 0700");
            assert_eq!(file_mode, 0o600, "models.json must be 0600");
        }

        // Verify zero leftover hidden temp files.
        let leftover: Vec<_> = fs::read_dir(&config_dir)
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".models.json.tmp.")
            })
            .collect();
        assert!(leftover.is_empty(), "no temp files may remain");

        let reloaded = load_config_from(&models_path).expect("reload");
        assert_eq!(reloaded, cfg);

        // Corrupt JSON fails clearly with Config error.
        fs::write(&models_path, "{not valid json").expect("write corrupt");
        let corrupt_err = load_config_from(&models_path).expect_err("must fail on corrupt json");
        assert!(matches!(corrupt_err, ModelsError::Config(_)));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn endpoint_remove_guards_active_bindings_unless_forced() {
        let mut cfg = ModelsConfig::default();
        cfg.add_endpoint("cloud", "https://api.example.com/v1/", Some("CLOUD_KEY"))
            .expect("add endpoint");
        cfg.bind_openai(CAPABILITY_ASR, "cloud", "whisper-1", OpenAiProtocol::Chat)
            .expect("bind asr");

        let err = cfg
            .remove_endpoint("cloud", false)
            .expect_err("bound endpoint remove without --force must fail");
        assert_eq!(err.exit_code(), 64);
        assert!(cfg.endpoints.contains_key("cloud"));
        assert!(cfg.bindings.contains_key(CAPABILITY_ASR));

        let removed_bindings = cfg
            .remove_endpoint("cloud", true)
            .expect("forced remove must succeed");
        assert_eq!(removed_bindings, vec![CAPABILITY_ASR.to_string()]);
        assert!(!cfg.endpoints.contains_key("cloud"));
        assert!(!cfg.bindings.contains_key(CAPABILITY_ASR));
    }

    #[test]
    fn show_never_prints_secret_values() {
        let mut cfg = ModelsConfig::default();
        cfg.add_endpoint(
            "secure",
            "https://api.example.com/v1",
            Some("SUPER_SECRET_ENV"),
        )
        .expect("add endpoint");
        cfg.bind_openai(
            CAPABILITY_ASR,
            "secure",
            "gpt-4o-transcribe",
            OpenAiProtocol::Transcriptions,
        )
        .expect("bind asr");

        let text = cfg.format_show_text(None);
        assert!(text.contains("SUPER_SECRET_ENV"));
        assert!(text.contains("https://api.example.com/v1"));

        let single = cfg.format_show_text(Some(CAPABILITY_ASR));
        assert!(single.contains("SUPER_SECRET_ENV"));

        let json_val = cfg.show_json(Some(CAPABILITY_ASR)).expect("show json");
        let rendered = serde_json::to_string(&json_val).expect("stringify");
        assert!(rendered.contains("SUPER_SECRET_ENV"));
        assert!(!rendered.contains("Bearer"));
    }

    #[test]
    fn resolves_strict_cli_over_env_over_models_json_precedence() {
        let mut cfg = ModelsConfig::default();
        cfg.add_endpoint("saved", "https://saved.example.com/v1/", Some("SAVED_KEY"))
            .expect("add endpoint");
        cfg.bind_openai(
            CAPABILITY_ASR,
            "saved",
            "saved-model",
            OpenAiProtocol::Transcriptions,
        )
        .expect("bind");

        // Tier 3: models.json only.
        let from_file =
            resolve_asr_config_with_env(&AsrCliOverrides::default(), &cfg, env_map(&[]))
                .expect("from models.json");
        assert_eq!(
            from_file,
            ResolvedAsrConfig::OpenAi {
                base_url: "https://saved.example.com/v1".to_string(),
                model: "saved-model".to_string(),
                api_key_env: Some("SAVED_KEY".to_string()),
                protocol: OpenAiProtocol::Transcriptions,
                timeout_secs: DEFAULT_ASR_TIMEOUT_SECS,
                source: ConfigSource::ModelsConfig,
            }
        );

        // Tier 2: env overrides models.json and does NOT leak SAVED_KEY onto the new URL.
        let from_env = resolve_asr_config_with_env(
            &AsrCliOverrides::default(),
            &cfg,
            env_map(&[
                (ENV_ASR_URL, "http://127.0.0.1:8000/v1/"),
                (ENV_ASR_MODEL, "env-model"),
                (ENV_ASR_PROTOCOL, "chat"),
                (ENV_ASR_TIMEOUT_SECS, "45"),
            ]),
        )
        .expect("from env");
        assert_eq!(
            from_env,
            ResolvedAsrConfig::OpenAi {
                base_url: "http://127.0.0.1:8000/v1".to_string(),
                model: "env-model".to_string(),
                api_key_env: None,
                protocol: OpenAiProtocol::Chat,
                timeout_secs: 45,
                source: ConfigSource::Env,
            }
        );

        // Tier 1: CLI overrides env and models.json.
        let cli = AsrCliOverrides {
            url: Some("http://127.0.0.1:9000/v1/".to_string()),
            model: Some("cli-model".to_string()),
            api_key_env: Some("CLI_KEY".to_string()),
            protocol: Some(OpenAiProtocol::Transcriptions),
            timeout_secs: Some(30),
            ..Default::default()
        };
        let from_cli = resolve_asr_config_with_env(
            &cli,
            &cfg,
            env_map(&[
                (ENV_ASR_URL, "http://127.0.0.1:8000/v1"),
                (ENV_ASR_MODEL, "env-model"),
                (ENV_ASR_API_KEY_ENV, "ENV_KEY"),
                (ENV_ASR_PROTOCOL, "chat"),
            ]),
        )
        .expect("from cli");
        assert_eq!(
            from_cli,
            ResolvedAsrConfig::OpenAi {
                base_url: "http://127.0.0.1:9000/v1".to_string(),
                model: "cli-model".to_string(),
                api_key_env: Some("CLI_KEY".to_string()),
                protocol: OpenAiProtocol::Transcriptions,
                timeout_secs: 30,
                source: ConfigSource::Cli,
            }
        );

        // Env Whisper ONNX overrides models.json OpenAI cleanly.
        let env_onnx = resolve_asr_config_with_env(
            &AsrCliOverrides::default(),
            &cfg,
            env_map(&[
                (ENV_ASR_MODEL_DIR, "/models/whisper-tiny"),
                (ENV_ASR_DEVICE, "cuda"),
            ]),
        )
        .expect("env onnx overrides saved openai");
        assert_eq!(
            env_onnx,
            ResolvedAsrConfig::WhisperOnnx {
                model_dir: PathBuf::from("/models/whisper-tiny"),
                device: OnnxDevice::Cuda,
                source: ConfigSource::Env,
            }
        );
    }

    #[test]
    fn fails_closed_on_ambiguous_env_or_cli_without_explicit_backend() {
        let cfg = ModelsConfig::default();

        // Simultaneous GLASSBOX_ASR_URL + GLASSBOX_ASR_MODEL_DIR without backend -> exit 64.
        let ambig_env = resolve_asr_config_with_env(
            &AsrCliOverrides::default(),
            &cfg,
            env_map(&[
                (ENV_ASR_URL, "https://api.example.com/v1"),
                (ENV_ASR_MODEL, "whisper-1"),
                (ENV_ASR_MODEL_DIR, "/models/whisper"),
            ]),
        )
        .expect_err("ambiguous env must fail closed");
        assert_eq!(ambig_env.exit_code(), 64);
        assert!(ambig_env.to_string().contains("ambiguous ASR backend"));

        // Explicit GLASSBOX_ASR_BACKEND disambiguates cleanly.
        let disambig_env = resolve_asr_config_with_env(
            &AsrCliOverrides::default(),
            &cfg,
            env_map(&[
                (ENV_ASR_BACKEND, "whisper-onnx"),
                (ENV_ASR_URL, "https://api.example.com/v1"),
                (ENV_ASR_MODEL, "whisper-1"),
                (ENV_ASR_MODEL_DIR, "/models/whisper"),
            ]),
        )
        .expect("explicit env backend disambiguates");
        assert!(matches!(
            disambig_env,
            ResolvedAsrConfig::WhisperOnnx {
                device: OnnxDevice::Cpu,
                source: ConfigSource::Env,
                ..
            }
        ));

        // Simultaneous --asr-url + --asr-model-dir without --asr-backend -> exit 64.
        let ambig_cli = resolve_asr_config_with_env(
            &AsrCliOverrides {
                url: Some("https://api.example.com/v1".to_string()),
                model: Some("whisper-1".to_string()),
                model_dir: Some(PathBuf::from("/models/whisper")),
                ..Default::default()
            },
            &cfg,
            env_map(&[]),
        )
        .expect_err("ambiguous cli must fail closed");
        assert_eq!(ambig_cli.exit_code(), 64);

        // Explicit --asr-backend overrides even ambiguous env.
        let disambig_cli = resolve_asr_config_with_env(
            &AsrCliOverrides {
                backend: Some(AsrBackendKind::WhisperOnnx),
                ..Default::default()
            },
            &cfg,
            env_map(&[
                (ENV_ASR_URL, "https://api.example.com/v1"),
                (ENV_ASR_MODEL_DIR, "/models/whisper"),
            ]),
        )
        .expect("explicit cli backend disambiguates");
        assert!(matches!(
            disambig_cli,
            ResolvedAsrConfig::WhisperOnnx {
                source: ConfigSource::Cli,
                ..
            }
        ));
    }

    #[test]
    fn fails_closed_when_unconfigured_or_missing_required_fields() {
        let cfg = ModelsConfig::default();

        // Nothing configured -> exit 64.
        let none_err = resolve_asr_config_with_env(&AsrCliOverrides::default(), &cfg, env_map(&[]))
            .expect_err("unconfigured must fail closed");
        assert_eq!(none_err.exit_code(), 64);

        // OpenAI URL without model -> exit 64.
        let no_model = resolve_asr_config_with_env(
            &AsrCliOverrides::default(),
            &cfg,
            env_map(&[(ENV_ASR_URL, "http://127.0.0.1:8080/v1")]),
        )
        .expect_err("openai without model must fail");
        assert_eq!(no_model.exit_code(), 64);

        // Explicit whisper-onnx without model_dir -> exit 64.
        let no_dir = resolve_asr_config_with_env(
            &AsrCliOverrides {
                backend: Some(AsrBackendKind::WhisperOnnx),
                ..Default::default()
            },
            &cfg,
            env_map(&[]),
        )
        .expect_err("onnx without model_dir must fail");
        assert_eq!(no_dir.exit_code(), 64);
    }
}

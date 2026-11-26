//! Module: glassbox-cli OpenAI-compatible HTTP ASR backend.
//! File: media/asr_openai.rs.
//! Responsibility: Encode 16 kHz mono PCM windows into in-memory WAV buffers and transcribe via OpenAI-compatible `/audio/transcriptions` or `/chat/completions` endpoints with bounded retries and secret redaction.

use super::asr::{AsrBackend, AsrError, AudioWindow, SAMPLE_RATE_HZ, TranscriptChunk};
use crate::models::{
    DEFAULT_ASR_TIMEOUT_SECS, OpenAiProtocol, validate_base_url, validate_env_var_name,
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::multipart::{Form, Part};
use serde_json::{Value, json};
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// TCP/TLS connection timeout in seconds for OpenAI-compatible ASR requests.
pub const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Maximum retry count on transient HTTP 5xx or transport reset failures.
pub const MAX_TRANSIENT_RETRIES: usize = 1;

/// Default backoff delay before the single transient retry attempt.
const RETRY_BACKOFF_MS: u64 = 100;

/// Maximum error response body bytes retained in diagnostics after redaction.
const MAX_ERROR_BODY_BYTES: usize = 512;

/// Maximum HTTP 2xx ASR response body size buffered in memory (8 MiB).
pub const MAX_ASR_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Default verbatim transcription prompt for `/chat/completions` `input_audio` mode.
const DEFAULT_CHAT_TRANSCRIPTION_PROMPT: &str =
    "Transcribe this audio verbatim. Output only the spoken words.";

/// Type alias for an injected environment lookup function used to read API keys safely.
type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Encodes mono 16 kHz `f32` PCM samples into a complete in-memory 16-bit PCM WAV buffer.
///
/// Accepts a slice of normalized `f32` audio samples in `[-1.0, 1.0]` sampled at
/// [`SAMPLE_RATE_HZ`] (16 kHz). Returns a valid 44-byte RIFF/WAVE header followed
/// by little-endian `i16` PCM samples with non-finite samples mapped to `0` and
/// out-of-range values clamped to `[-1.0, 1.0]`. Never performs disk I/O or fails.
pub fn encode_wav_16k_mono_pcm16(pcm: &[f32]) -> Vec<u8> {
    let data_bytes = pcm.len().saturating_mul(2);
    let data_size_u32 = u32::try_from(data_bytes).unwrap_or(u32::MAX);
    let riff_size_u32 = data_size_u32.saturating_add(36);
    let sample_rate_u32 = SAMPLE_RATE_HZ as u32;
    let byte_rate_u32 = sample_rate_u32.saturating_mul(2);

    let mut out = Vec::with_capacity(44usize.saturating_add(data_bytes));
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size_u32.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM format chunk length.
    out.extend_from_slice(&1u16.to_le_bytes()); // Audio format 1 = uncompressed PCM.
    out.extend_from_slice(&1u16.to_le_bytes()); // Mono (1 channel).
    out.extend_from_slice(&sample_rate_u32.to_le_bytes());
    out.extend_from_slice(&byte_rate_u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // Block align = 1 channel * 16 bits / 8.
    out.extend_from_slice(&16u16.to_le_bytes()); // Bits per sample.
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size_u32.to_le_bytes());

    for &sample in pcm {
        let clamped = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let quantized = if clamped <= -1.0 {
            i16::MIN
        } else if clamped >= 1.0 {
            i16::MAX
        } else {
            (clamped * f32::from(i16::MAX)).round() as i16
        };
        out.extend_from_slice(&quantized.to_le_bytes());
    }
    out
}

/// Configuration for [`OpenAiAsrBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiAsrConfig {
    /// OpenAI-compatible HTTP(S) base URL (for example `http://127.0.0.1:8080/v1`).
    pub base_url: String,
    /// Model identifier sent to the remote ASR endpoint.
    pub model: String,
    /// Optional environment variable name storing the Bearer API key (never the raw key).
    pub api_key_env: Option<String>,
    /// OpenAI wire protocol (`/audio/transcriptions` or `/chat/completions`).
    pub protocol: OpenAiProtocol,
    /// Per-window HTTP read timeout in seconds (default `120`).
    pub timeout_secs: u64,
}

impl OpenAiAsrConfig {
    /// Constructs a default [`OpenAiAsrConfig`] using the `transcriptions` protocol and 120 s timeout.
    ///
    /// Accepts `base_url` and `model` strings. Returns an [`OpenAiAsrConfig`] with
    /// `api_key_env: None`, `protocol: OpenAiProtocol::Transcriptions`, and
    /// `timeout_secs: DEFAULT_ASR_TIMEOUT_SECS`. Cannot fail.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key_env: None,
            protocol: OpenAiProtocol::Transcriptions,
            timeout_secs: DEFAULT_ASR_TIMEOUT_SECS,
        }
    }
}

/// HTTP ASR backend speaking OpenAI-compatible `/audio/transcriptions` or `/chat/completions`.
pub struct OpenAiAsrBackend {
    /// Validated backend configuration (holding only `api_key_env` name, never raw secrets).
    config: OpenAiAsrConfig,
    /// Validated `http`/`https` base URL with trailing slashes removed.
    normalized_base_url: String,
    /// Shared async HTTP client configured with connect/read timeouts and no redirects.
    client: reqwest::Client,
    /// Environment variable resolver used to read the Bearer token at request time.
    env_lookup: EnvLookup,
}

impl fmt::Debug for OpenAiAsrBackend {
    /// Formats diagnostic metadata for [`OpenAiAsrBackend`] without ever exposing API keys.
    ///
    /// Accepts a mutable formatter. Returns `fmt::Result` displaying the normalized
    /// URL, model, protocol, timeout, and `api_key_env` name only. Fails only if formatting fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiAsrBackend")
            .field("normalized_base_url", &self.normalized_base_url)
            .field("model", &self.config.model)
            .field("api_key_env", &self.config.api_key_env)
            .field("protocol", &self.config.protocol)
            .field("timeout_secs", &self.config.timeout_secs)
            .finish()
    }
}

impl OpenAiAsrBackend {
    /// Constructs a new [`OpenAiAsrBackend`] reading optional API keys from the live process environment.
    ///
    /// Accepts an [`OpenAiAsrConfig`]. Returns an initialized [`OpenAiAsrBackend`]
    /// with validated URL/model/env-var syntax and configured HTTP client timeouts.
    /// Returns [`AsrError::Config`] on invalid config or [`AsrError::Runtime`] if
    /// the TLS/HTTP client cannot be built.
    pub fn new(config: OpenAiAsrConfig) -> Result<Self, AsrError> {
        Self::with_env_lookup(config, |key| std::env::var(key).ok())
    }

    /// Constructs a new [`OpenAiAsrBackend`] with a custom environment lookup closure.
    ///
    /// Accepts an [`OpenAiAsrConfig`] and `env_lookup(name) -> Option<String>`.
    /// Returns an initialized [`OpenAiAsrBackend`]. Returns [`AsrError::Config`]
    /// when `base_url`, `model`, `api_key_env`, or `timeout_secs` is invalid, or
    /// [`AsrError::Runtime`] if the HTTP client builder fails.
    pub fn with_env_lookup<F>(config: OpenAiAsrConfig, env_lookup: F) -> Result<Self, AsrError>
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        if config.timeout_secs == 0 {
            return Err(AsrError::Config(
                "OpenAI ASR timeout_secs must be at least 1 second".to_string(),
            ));
        }
        let normalized_base_url =
            validate_base_url(&config.base_url).map_err(|err| AsrError::Config(err.to_string()))?;
        let trimmed_model = config.model.trim().to_string();
        if trimmed_model.is_empty() {
            return Err(AsrError::Config(
                "OpenAI ASR model identifier must not be empty".to_string(),
            ));
        }
        if let Some(ref env_var) = config.api_key_env {
            validate_env_var_name(env_var).map_err(|err| AsrError::Config(err.to_string()))?;
        }

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(config.timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| AsrError::Runtime(format!("failed to build ASR HTTP client: {err}")))?;

        let sanitized_config = OpenAiAsrConfig {
            model: trimmed_model,
            ..config
        };

        Ok(Self {
            config: sanitized_config,
            normalized_base_url,
            client,
            env_lookup: Arc::new(env_lookup),
        })
    }

    /// Resolves the optional Bearer secret at request time without logging or caching its value.
    ///
    /// Accepts `&self`. Returns `Ok(None)` when `api_key_env` is `None`, or
    /// `Ok(Some(secret))` when the named environment variable is non-empty.
    /// Returns [`AsrError::Config`] naming only the variable name if `api_key_env`
    /// is invalid, unset, or blank.
    fn resolve_api_key(&self) -> Result<Option<String>, AsrError> {
        let Some(ref env_var) = self.config.api_key_env else {
            return Ok(None);
        };
        validate_env_var_name(env_var).map_err(|err| AsrError::Config(err.to_string()))?;
        let value = (self.env_lookup)(env_var);
        match value.as_deref().map(str::trim) {
            Some(secret) if !secret.is_empty() => Ok(Some(secret.to_string())),
            _ => Err(AsrError::Config(format!(
                "API key environment variable '{env_var}' is unset or empty"
            ))),
        }
    }

    /// Executes one window transcription request over HTTP with at most one transient retry.
    ///
    /// Accepts encoded WAV bytes, optional language hint, and optional Bearer token.
    /// Returns the extracted transcript text string. Streams HTTP 2xx responses up
    /// to [`MAX_ASR_RESPONSE_BYTES`] before UTF-8/JSON decoding (failing closed
    /// without retrying on oversized bodies), retries at most once on HTTP 5xx or
    /// transport errors, never retries 4xx or 3xx, and redacts any secret token
    /// from every returned error message.
    async fn send_with_retry(
        &self,
        wav_bytes: Vec<u8>,
        language: Option<String>,
        api_key: Option<String>,
    ) -> Result<String, AsrError> {
        let endpoint_url = match self.config.protocol {
            OpenAiProtocol::Transcriptions => {
                format!("{}/audio/transcriptions", self.normalized_base_url)
            }
            OpenAiProtocol::Chat => {
                format!("{}/chat/completions", self.normalized_base_url)
            }
        };

        for attempt in 0..=MAX_TRANSIENT_RETRIES {
            let request = self.build_request(
                &endpoint_url,
                &wav_bytes,
                language.as_deref(),
                api_key.as_deref(),
            )?;

            match request.send().await {
                Ok(response) => {
                    let status_code = response.status().as_u16();
                    let content_type = response
                        .headers()
                        .get(CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);

                    if (200..300).contains(&status_code) {
                        let body = match read_bounded_success_body(
                            response,
                            MAX_ASR_RESPONSE_BYTES,
                            api_key.as_deref(),
                        )
                        .await
                        {
                            Ok(body) => body,
                            Err(AsrError::Http {
                                retryable: true, ..
                            }) if attempt < MAX_TRANSIENT_RETRIES => {
                                tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MS)).await;
                                continue;
                            }
                            Err(err) => return Err(err),
                        };
                        return match self.config.protocol {
                            OpenAiProtocol::Transcriptions => parse_transcription_response(
                                content_type.as_deref(),
                                &body,
                                api_key.as_deref(),
                            ),
                            OpenAiProtocol::Chat => parse_chat_response(&body, api_key.as_deref()),
                        };
                    }

                    let body_snippet = read_error_snippet(response, api_key.as_deref()).await;
                    let is_server_error = status_code >= 500;
                    if is_server_error && attempt < MAX_TRANSIENT_RETRIES {
                        tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MS)).await;
                        continue;
                    }

                    let message = if body_snippet.is_empty() {
                        format!(
                            "OpenAI ASR endpoint returned HTTP {status_code} ({})",
                            self.config.protocol
                        )
                    } else {
                        format!(
                            "OpenAI ASR endpoint returned HTTP {status_code} ({}): {body_snippet}",
                            self.config.protocol
                        )
                    };
                    return Err(AsrError::Http {
                        status: Some(status_code),
                        message,
                        retryable: is_server_error,
                    });
                }
                Err(err) => {
                    if attempt < MAX_TRANSIENT_RETRIES {
                        tokio::time::sleep(Duration::from_millis(RETRY_BACKOFF_MS)).await;
                        continue;
                    }
                    let safe_msg = redact_secret(
                        &format!(
                            "OpenAI ASR transport failure ({}): {err}",
                            self.config.protocol
                        ),
                        api_key.as_deref(),
                    );
                    return Err(AsrError::Http {
                        status: None,
                        message: safe_msg,
                        retryable: true,
                    });
                }
            }
        }

        Err(AsrError::Runtime(
            "OpenAI ASR retry loop terminated unexpectedly".to_string(),
        ))
    }

    /// Builds a single HTTP POST request for either `transcriptions` or `chat` protocol.
    ///
    /// Accepts the target URL, encoded WAV bytes, optional language tag, and
    /// optional Bearer token. Returns a configured [`reqwest::RequestBuilder`].
    /// Returns [`AsrError::Runtime`] if MIME part construction fails.
    fn build_request(
        &self,
        url: &str,
        wav_bytes: &[u8],
        language: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<reqwest::RequestBuilder, AsrError> {
        let mut builder = self.client.post(url).header(ACCEPT, "application/json");
        if let Some(secret) = api_key {
            builder = builder.header(AUTHORIZATION, format!("Bearer {secret}"));
        }

        match self.config.protocol {
            OpenAiProtocol::Transcriptions => {
                let file_part = Part::bytes(wav_bytes.to_vec())
                    .file_name("window.wav")
                    .mime_str("audio/wav")
                    .map_err(|err| {
                        AsrError::Runtime(format!("failed to construct WAV multipart part: {err}"))
                    })?;
                let mut form = Form::new()
                    .part("file", file_part)
                    .text("model", self.config.model.clone())
                    .text("response_format", "json");
                if let Some(lang) = language.map(str::trim).filter(|s| !s.is_empty()) {
                    form = form.text("language", lang.to_string());
                }
                Ok(builder.multipart(form))
            }
            OpenAiProtocol::Chat => {
                let b64_wav = BASE64_STANDARD.encode(wav_bytes);
                let prompt = match language.map(str::trim).filter(|s| !s.is_empty()) {
                    Some(lang) => format!(
                        "Transcribe this audio verbatim in {lang}. Output only the spoken words."
                    ),
                    None => DEFAULT_CHAT_TRANSCRIPTION_PROMPT.to_string(),
                };
                let payload = json!({
                    "model": self.config.model,
                    "messages": [
                        {
                            "role": "user",
                            "content": [
                                { "type": "text", "text": prompt },
                                {
                                    "type": "input_audio",
                                    "input_audio": {
                                        "data": b64_wav,
                                        "format": "wav"
                                    }
                                }
                            ]
                        }
                    ],
                    "stream": false
                });
                Ok(builder.json(&payload))
            }
        }
    }
}

impl AsrBackend for OpenAiAsrBackend {
    /// Returns the canonical backend name `"openai"`.
    ///
    /// Accepts `&self`. Returns `"openai"`. Cannot fail.
    fn backend_name(&self) -> &'static str {
        "openai"
    }

    /// Returns the configured remote model identifier.
    ///
    /// Accepts `&self`. Returns `&str`. Cannot fail.
    fn model_id(&self) -> &str {
        &self.config.model
    }

    /// Returns the normalized endpoint base URL as the provider label (never containing credentials).
    ///
    /// Accepts `&self`. Returns the validated, normalized base URL string slice. Cannot fail.
    fn provider(&self) -> &str {
        &self.normalized_base_url
    }

    /// Returns the active wire protocol (`"transcriptions"` or `"chat"`).
    ///
    /// Accepts `&self`. Returns `Some("transcriptions")` or `Some("chat")`. Cannot fail.
    fn protocol(&self) -> Option<&str> {
        Some(self.config.protocol.as_str())
    }

    /// Validates endpoint URL, model ID, timeout, and optional API-key env var presence before decoding audio.
    ///
    /// Accepts `&self`. Returns `Ok(())` when the backend is ready to send requests.
    /// Returns [`AsrError::Config`] if URL/model/timeout is invalid or if `api_key_env`
    /// is configured but unset or empty (naming only the env var, never any secret).
    fn validate(&self) -> Result<(), AsrError> {
        if self.config.timeout_secs == 0 {
            return Err(AsrError::Config(
                "OpenAI ASR timeout_secs must be at least 1 second".to_string(),
            ));
        }
        validate_base_url(&self.normalized_base_url)
            .map_err(|err| AsrError::Config(err.to_string()))?;
        if self.config.model.trim().is_empty() {
            return Err(AsrError::Config(
                "OpenAI ASR model identifier must not be empty".to_string(),
            ));
        }
        let _ = self.resolve_api_key()?;
        Ok(())
    }

    /// Encodes one audio window as in-memory WAV and transcribes it over the configured OpenAI protocol.
    ///
    /// Accepts `&mut self`, an [`AudioWindow`], and an optional ISO language hint.
    /// Returns a [`TranscriptChunk`] preserving `window.start_ms` and `window.end_ms`.
    /// Returns [`AsrError::Config`] on missing API key env vars, [`AsrError::Http`]
    /// on HTTP/transport failures (with max 1 retry on 5xx/transport reset and 0
    /// retries on 4xx), or [`AsrError::Decode`] on malformed server responses.
    fn transcribe_window(
        &mut self,
        window: &AudioWindow,
        language: Option<&str>,
    ) -> Result<TranscriptChunk, AsrError> {
        let api_key = self.resolve_api_key()?;
        let wav_bytes = encode_wav_16k_mono_pcm16(&window.pcm_16k_f32);
        let language_owned = language.map(str::to_string);
        let text = run_async(self.send_with_retry(wav_bytes, language_owned, api_key))?;
        Ok(TranscriptChunk {
            start_ms: window.start_ms,
            end_ms: window.end_ms,
            text,
        })
    }
}

/// Parses an OpenAI `/audio/transcriptions` response body as JSON `{"text": ...}` or `text/plain`.
///
/// Accepts optional response `Content-Type`, response body text, and optional
/// active secret for redaction. Returns the trimmed transcript text. Returns
/// [`AsrError::Decode`] if JSON parsing fails on a non-plain-text response or if
/// the JSON object lacks a string `"text"` field.
pub fn parse_transcription_response(
    content_type: Option<&str>,
    body: &str,
    secret: Option<&str>,
) -> Result<String, AsrError> {
    let is_plain_text = content_type
        .map(|ct| ct.trim().to_ascii_lowercase().starts_with("text/plain"))
        .unwrap_or(false);
    if is_plain_text {
        return Ok(body.trim().to_string());
    }

    let parsed: Value = serde_json::from_str(body).map_err(|err| {
        let msg = redact_secret(
            &format!("invalid JSON from /audio/transcriptions endpoint: {err}"),
            secret,
        );
        AsrError::Decode(msg)
    })?;
    let text = parsed.get("text").and_then(Value::as_str).ok_or_else(|| {
        AsrError::Decode("transcription JSON response missing string 'text' field".to_string())
    })?;
    Ok(text.trim().to_string())
}

/// Parses an OpenAI `/chat/completions` JSON response extracting `choices[0].message.content`.
///
/// Accepts the response body string and optional active secret for redaction.
/// Returns the trimmed transcript text from either a string `content` or an array
/// of `{ "type": "text", "text": "..." }` content parts. Returns [`AsrError::Decode`]
/// when the response is not valid JSON or lacks usable text content.
pub fn parse_chat_response(body: &str, secret: Option<&str>) -> Result<String, AsrError> {
    let parsed: Value = serde_json::from_str(body).map_err(|err| {
        let msg = redact_secret(
            &format!("invalid JSON from /chat/completions endpoint: {err}"),
            secret,
        );
        AsrError::Decode(msg)
    })?;
    let content = parsed
        .pointer("/choices/0/message/content")
        .ok_or_else(|| {
            AsrError::Decode(
                "chat completions response missing choices[0].message.content".to_string(),
            )
        })?;
    match content {
        Value::String(text) => Ok(text.trim().to_string()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|part| {
                    let is_text = part.get("type").and_then(Value::as_str) == Some("text");
                    if is_text {
                        part.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect();
            if texts.is_empty() {
                Err(AsrError::Decode(
                    "chat completions response content array contained no text parts".to_string(),
                ))
            } else {
                Ok(texts.join("\n").trim().to_string())
            }
        }
        _ => Err(AsrError::Decode(
            "chat completions response choices[0].message.content had unsupported type".to_string(),
        )),
    }
}

/// Scrubs any configured secret token and every echoed `Bearer` token from diagnostic strings.
///
/// Accepts a diagnostic text string and optional secret value. Replaces all
/// literal occurrences of the secret (when non-empty) and scans forward to
/// scrub every case-insensitive `bearer <token>` occurrence with `[REDACTED]`.
/// Returns the sanitized `String`. Cannot fail.
pub fn redact_secret(text: &str, secret: Option<&str>) -> String {
    const BEARER_PREFIX: &str = "bearer ";
    const REDACTED: &str = "[REDACTED]";

    let mut sanitized = match secret {
        Some(s) if !s.is_empty() => text.replace(s, REDACTED),
        _ => text.to_string(),
    };

    let mut cursor = 0usize;
    while let Some(rel_pos) = sanitized[cursor..].to_ascii_lowercase().find(BEARER_PREFIX) {
        let after_bearer = cursor + rel_pos + BEARER_PREFIX.len();
        let ws_len = sanitized[after_bearer..]
            .bytes()
            .take_while(|&b| b == b' ' || b == b'\t')
            .count();
        let token_start = after_bearer + ws_len;
        let rest = &sanitized[token_start..];
        let token_end_rel = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '}'))
            .unwrap_or(rest.len());
        let token_end = token_start + token_end_rel;
        sanitized.replace_range(token_start..token_end, REDACTED);
        cursor = token_start + REDACTED.len();
    }
    sanitized
}

/// Truncates a UTF-8 string to at most `max_bytes` bytes without splitting a character.
///
/// Accepts a string slice and maximum byte length. Returns an owned `String`
/// whose byte length is `<= max_bytes` and ends on a valid UTF-8 character
/// boundary. Cannot fail.
fn truncate_utf8_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Streams an HTTP 2xx ASR response body into UTF-8 text while enforcing a hard byte cap.
///
/// Accepts a consumed [`reqwest::Response`], the maximum allowed byte length
/// `max_bytes`, and an optional active secret for error redaction. Rejects
/// responses declaring a `Content-Length` above `max_bytes` immediately, then
/// streams chunks via [`reqwest::Response::chunk`] into a `Vec<u8>`, dropping
/// the response immediately if accumulated bytes would exceed `max_bytes`.
/// Returns the decoded UTF-8 body string. Returns [`AsrError::Decode`] when
/// `max_bytes` is exceeded or the body is not valid UTF-8, or [`AsrError::Http`]
/// (with redacted diagnostics) on mid-stream transport failures.
async fn read_bounded_success_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    secret: Option<&str>,
) -> Result<String, AsrError> {
    let status_code = response.status().as_u16();
    if let Some(content_len) = response.content_length()
        && content_len > max_bytes as u64
    {
        drop(response);
        return Err(AsrError::Decode(format!(
            "ASR HTTP 2xx response exceeded {max_bytes}-byte safety cap"
        )));
    }

    let initial_cap = response
        .content_length()
        .and_then(|len| usize::try_from(len).ok())
        .map_or(0, |len| len.min(max_bytes).min(64 * 1024));
    let mut buf = Vec::with_capacity(initial_cap);

    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len().saturating_add(chunk.len()) > max_bytes {
                    drop(response);
                    return Err(AsrError::Decode(format!(
                        "ASR HTTP 2xx response exceeded {max_bytes}-byte safety cap"
                    )));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(err) => {
                let safe_msg =
                    redact_secret(&format!("failed reading ASR response body: {err}"), secret);
                return Err(AsrError::Http {
                    status: Some(status_code),
                    message: safe_msg,
                    retryable: true,
                });
            }
        }
    }
    drop(response);

    String::from_utf8(buf).map_err(|err| {
        AsrError::Decode(format!(
            "ASR HTTP 2xx response body is not valid UTF-8: {err}"
        ))
    })
}

/// Streams, redacts, and bounds an HTTP error response snippet for failure diagnostics.
///
/// Accepts a consumed [`reqwest::Response`] and optional secret value. Streams
/// response chunks only up to a hard cap of
/// `(MAX_ERROR_BODY_BYTES + secret_len.min(4096) + 256).min(8192)` bytes before
/// dropping the response immediately, collapses whitespace, runs
/// [`redact_secret`] before truncating so secrets straddling the 512-byte
/// boundary cannot leak a raw prefix, and returns a single-line UTF-8 snippet
/// of at most [`MAX_ERROR_BODY_BYTES`] bytes. Returns an empty string if no
/// bytes could be read.
async fn read_error_snippet(mut response: reqwest::Response, secret: Option<&str>) -> String {
    let read_cap = (MAX_ERROR_BODY_BYTES + secret.map_or(0, str::len).min(4096) + 256).min(8192);
    let mut bytes = Vec::with_capacity(read_cap.min(1024));
    while let Ok(Some(chunk)) = response.chunk().await {
        let remaining = read_cap.saturating_sub(bytes.len());
        if remaining == 0 {
            break;
        }
        let take = chunk.len().min(remaining);
        bytes.extend_from_slice(&chunk[..take]);
        if bytes.len() >= read_cap {
            break;
        }
    }
    drop(response);

    let lossy = String::from_utf8_lossy(&bytes);
    let collapsed = lossy.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = redact_secret(&collapsed, secret);
    truncate_utf8_bytes(&redacted, MAX_ERROR_BODY_BYTES)
}

/// Executes an async ASR future safely from sync callers, multi-thread Tokio, or current-thread Tokio.
///
/// Accepts a `Send` future yielding `Result<T, AsrError>`. Uses
/// `tokio::task::block_in_place` when already inside a multi-thread Tokio runtime,
/// runs a scoped OS thread with a dedicated current-thread Tokio runtime when
/// called from a current-thread Tokio runtime, or builds a local current-thread
/// runtime directly for synchronous callers. Returns the future's result or
/// [`AsrError::Runtime`] if runtime construction fails.
fn run_async<F, T>(future: F) -> Result<T, AsrError>
where
    F: Future<Output = Result<T, AsrError>> + Send,
    T: Send,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        Ok(_) => std::thread::scope(|scope| {
            let worker = scope.spawn(move || -> Result<T, AsrError> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| {
                        AsrError::Runtime(format!("failed to create ASR Tokio runtime: {err}"))
                    })?;
                rt.block_on(future)
            });
            match worker.join() {
                Ok(result) => result,
                Err(_) => Err(AsrError::Runtime(
                    "OpenAI ASR worker thread panicked".to_string(),
                )),
            }
        }),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| {
                    AsrError::Runtime(format!("failed to create ASR Tokio runtime: {err}"))
                })?;
            rt.block_on(future)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Recorded HTTP request captured by the in-process mock loopback server.
    #[derive(Debug, Clone)]
    struct CapturedRequest {
        /// Request line (for example `POST /v1/audio/transcriptions HTTP/1.1`).
        request_line: String,
        /// Lowercased HTTP header map.
        headers: HashMap<String, String>,
        /// Raw request body bytes.
        body: Vec<u8>,
    }

    /// Canned HTTP response returned by the mock loopback server for one request.
    #[derive(Debug, Clone)]
    struct MockResponse {
        /// HTTP status code and reason phrase (for example `"200 OK"`).
        status_line: &'static str,
        /// Value of the `Content-Type` response header.
        content_type: &'static str,
        /// Response payload text.
        body: String,
    }

    /// Spawns a local loopback HTTP/1.1 mock server serving a queue of canned responses.
    ///
    /// Accepts a vector of [`MockResponse`] entries served in order. Returns the
    /// loopback base URL (`http://127.0.0.1:<port>/v1`), an atomic request counter,
    /// and a shared vector of [`CapturedRequest`] records. Panics on bind error.
    async fn spawn_mock_server(
        responses: Vec<MockResponse>,
    ) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<CapturedRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));

        let hits_bg = Arc::clone(&hits);
        let captured_bg = Arc::clone(&captured);
        let queue_bg = Arc::clone(&queue);

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let Some(resp) = queue_bg.lock().expect("lock queue").pop_front() else {
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

                if queue_bg.lock().expect("lock queue").is_empty() {
                    break;
                }
            }
        });

        (format!("http://{addr}/v1"), hits, captured)
    }

    /// Reads one complete HTTP/1.1 request (headers plus `Content-Length` body) from a TCP stream.
    ///
    /// Accepts a mutable `tokio::net::TcpStream`. Returns a parsed [`CapturedRequest`].
    /// Panics if the socket closes before the header delimiter arrives.
    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> CapturedRequest {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let n = stream.read(&mut chunk).await.expect("read request");
            assert!(n > 0, "unexpected EOF before HTTP headers completed");
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
        let content_length = headers
            .get("content-length")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        let mut body = buf[header_end..].to_vec();
        while body.len() < content_length {
            let n = stream.read(&mut chunk).await.expect("read body");
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(content_length);

        CapturedRequest {
            request_line,
            headers,
            body,
        }
    }

    /// Constructs a short synthetic 16 kHz audio window for unit tests.
    ///
    /// Accepts start and end timestamps in milliseconds. Returns an [`AudioWindow`]
    /// populated with 160 deterministic samples. Cannot fail.
    fn sample_window(start_ms: u64, end_ms: u64) -> AudioWindow {
        AudioWindow {
            index: 0,
            start_ms,
            end_ms,
            pcm_16k_f32: [0.0, 0.25, -0.25, 0.5, -0.5].repeat(32),
        }
    }

    #[test]
    fn encodes_valid_44_byte_wav_header_and_clamps_samples() {
        let samples = [0.0, 1.0, -1.0, 2.5, -3.0, f32::NAN, f32::INFINITY, 0.5];
        let wav = encode_wav_16k_mono_pcm16(&samples);

        assert_eq!(wav.len(), 44 + samples.len() * 2);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()),
            36 + (samples.len() as u32) * 2
        );
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1); // mono
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32_000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()),
            (samples.len() as u32) * 2
        );

        let decoded: Vec<i16> = wav[44..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(decoded[0], 0);
        assert_eq!(decoded[1], i16::MAX);
        assert_eq!(decoded[2], i16::MIN);
        assert_eq!(decoded[3], i16::MAX);
        assert_eq!(decoded[4], i16::MIN);
        assert_eq!(decoded[5], 0);
        assert_eq!(decoded[6], 0);
        assert_eq!(decoded[7], 16_384);
    }

    #[test]
    fn normalizes_base_url_and_validates_api_key_env() {
        let mut cfg = OpenAiAsrConfig::new("http://127.0.0.1:8080/v1///", "whisper-1");
        cfg.api_key_env = Some("MY_ASR_TOKEN".to_string());

        let missing_env =
            OpenAiAsrBackend::with_env_lookup(cfg.clone(), |_| None).expect("construct backend");
        assert_eq!(missing_env.provider(), "http://127.0.0.1:8080/v1");
        let err = missing_env
            .validate()
            .expect_err("unset env var must fail validate");
        match err {
            AsrError::Config(msg) => {
                assert!(msg.contains("MY_ASR_TOKEN"));
            }
            other => panic!("expected Config error, got {other:?}"),
        }

        let present_env = OpenAiAsrBackend::with_env_lookup(cfg, |key| {
            (key == "MY_ASR_TOKEN").then(|| "sk-test-secret".to_string())
        })
        .expect("construct backend");
        assert!(present_env.validate().is_ok());
    }

    #[test]
    fn parses_transcriptions_and_chat_responses_and_redacts_secrets() {
        assert_eq!(
            parse_transcription_response(
                Some("application/json"),
                r#"{"text": " hello world "}"#,
                None
            )
            .expect("json text"),
            "hello world"
        );
        assert_eq!(
            parse_transcription_response(Some("text/plain; charset=utf-8"), " plain line \n", None)
                .expect("plain text"),
            "plain line"
        );
        assert!(matches!(
            parse_transcription_response(Some("application/json"), r#"{"no_text": 1}"#, None),
            Err(AsrError::Decode(_))
        ));

        assert_eq!(
            parse_chat_response(
                r#"{"choices":[{"message":{"content":" spoken words "}}]}"#,
                None
            )
            .expect("chat string"),
            "spoken words"
        );
        assert_eq!(
            parse_chat_response(
                r#"{"choices":[{"message":{"content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"}]}}]}"#,
                None
            )
            .expect("chat array"),
            "line one\nline two"
        );
        assert!(matches!(
            parse_chat_response(r#"{"choices":[]}"#, None),
            Err(AsrError::Decode(_))
        ));

        let redacted = redact_secret(
            "upstream failed with Authorization: Bearer top-secret-token-99 and body top-secret-token-99",
            Some("top-secret-token-99"),
        );
        assert!(!redacted.contains("top-secret-token-99"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_transcriptions_multipart_without_auth_succeeds() {
        let (base_url, hits, captured) = spawn_mock_server(vec![MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"text":"Hello from transcriptions"}"#.to_string(),
        }])
        .await;

        // Add trailing slash to confirm normalization on live request path.
        let cfg = OpenAiAsrConfig::new(format!("{base_url}/"), "whisper-large-v3");
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |_| None).expect("backend");
        backend.validate().expect("validate");

        let chunk = backend
            .transcribe_window(&sample_window(1_000, 4_000), Some("en"))
            .expect("transcribe");
        assert_eq!(chunk.start_ms, 1_000);
        assert_eq!(chunk.end_ms, 4_000);
        assert_eq!(chunk.text, "Hello from transcriptions");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let reqs = captured.lock().expect("lock captured");
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0]
                .request_line
                .starts_with("POST /v1/audio/transcriptions ")
        );
        assert!(
            !reqs[0].headers.contains_key("authorization"),
            "Authorization header must not be sent when api_key_env is None"
        );
        let body_lossy = String::from_utf8_lossy(&reqs[0].body);
        assert!(body_lossy.contains("filename=\"window.wav\""));
        assert!(body_lossy.contains("whisper-large-v3"));
        assert!(body_lossy.contains("name=\"language\""));
        assert!(body_lossy.contains("RIFF"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_chat_input_audio_sends_bearer_auth_and_base64_wav() {
        let (base_url, hits, captured) = spawn_mock_server(vec![MockResponse {
            status_line: "200 OK",
            content_type: "application/json",
            body: r#"{"choices":[{"message":{"content":"Hello from chat input_audio"}}]}"#
                .to_string(),
        }])
        .await;

        let mut cfg = OpenAiAsrConfig::new(base_url, "gemma-4-audio");
        cfg.protocol = OpenAiProtocol::Chat;
        cfg.api_key_env = Some("TEST_CHAT_KEY".to_string());

        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |key| {
            (key == "TEST_CHAT_KEY").then(|| "secret-bearer-xyz".to_string())
        })
        .expect("backend");

        let chunk = backend
            .transcribe_window(&sample_window(0, 2_500), None)
            .expect("transcribe chat");
        assert_eq!(chunk.text, "Hello from chat input_audio");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let reqs = captured.lock().expect("lock captured");
        assert!(
            reqs[0]
                .request_line
                .starts_with("POST /v1/chat/completions ")
        );
        assert_eq!(
            reqs[0].headers.get("authorization").map(String::as_str),
            Some("Bearer secret-bearer-xyz")
        );

        let payload: Value = serde_json::from_slice(&reqs[0].body).expect("valid json payload");
        assert_eq!(payload["model"], "gemma-4-audio");
        assert_eq!(payload["messages"][0]["content"][1]["type"], "input_audio");
        assert_eq!(
            payload["messages"][0]["content"][1]["input_audio"]["format"],
            "wav"
        );
        let b64_data = payload["messages"][0]["content"][1]["input_audio"]["data"]
            .as_str()
            .expect("base64 wav string");
        let decoded_wav = BASE64_STANDARD.decode(b64_data).expect("valid base64");
        assert_eq!(&decoded_wav[0..4], b"RIFF");
        assert_eq!(&decoded_wav[8..12], b"WAVE");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_5xx_retries_once_and_succeeds_on_second_attempt() {
        let (base_url, hits, _captured) = spawn_mock_server(vec![
            MockResponse {
                status_line: "503 Service Unavailable",
                content_type: "application/json",
                body: r#"{"error":"transient overload"}"#.to_string(),
            },
            MockResponse {
                status_line: "200 OK",
                content_type: "application/json",
                body: r#"{"text":"recovered after retry"}"#.to_string(),
            },
        ])
        .await;

        let cfg = OpenAiAsrConfig::new(base_url, "whisper-1");
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |_| None).expect("backend");

        let chunk = backend
            .transcribe_window(&sample_window(0, 3_000), None)
            .expect("second attempt must succeed");
        assert_eq!(chunk.text, "recovered after retry");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "must attempt exactly twice");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_5xx_stops_after_single_retry_and_reports_retryable() {
        let (base_url, hits, _captured) = spawn_mock_server(vec![
            MockResponse {
                status_line: "502 Bad Gateway",
                content_type: "application/json",
                body: r#"{"error":"upstream down 1"}"#.to_string(),
            },
            MockResponse {
                status_line: "502 Bad Gateway",
                content_type: "application/json",
                body: r#"{"error":"upstream down 2"}"#.to_string(),
            },
            MockResponse {
                status_line: "200 OK",
                content_type: "application/json",
                body: r#"{"text":"must never be reached on third attempt"}"#.to_string(),
            },
        ])
        .await;

        let cfg = OpenAiAsrConfig::new(base_url, "whisper-1");
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |_| None).expect("backend");

        let err = backend
            .transcribe_window(&sample_window(0, 3_000), None)
            .expect_err("persistent 5xx must fail after 1 retry");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "5xx must perform initial attempt plus at most 1 retry"
        );
        match err {
            AsrError::Http {
                status, retryable, ..
            } => {
                assert_eq!(status, Some(502));
                assert!(retryable);
            }
            other => panic!("expected retryable Http error, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_401_makes_zero_retries_and_redacts_secret_from_error() {
        let secret = "sk-ultra-sensitive-998877";
        let (base_url, hits, _captured) = spawn_mock_server(vec![
            MockResponse {
                status_line: "401 Unauthorized",
                content_type: "application/json",
                body: format!(r#"{{"error":"rejected Bearer {secret}"}}"#),
            },
            MockResponse {
                status_line: "200 OK",
                content_type: "application/json",
                body: r#"{"text":"should never be reached"}"#.to_string(),
            },
        ])
        .await;

        let mut cfg = OpenAiAsrConfig::new(base_url, "whisper-1");
        cfg.api_key_env = Some("TEST_401_KEY".to_string());
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, move |key| {
            (key == "TEST_401_KEY").then(|| secret.to_string())
        })
        .expect("backend");

        let err = backend
            .transcribe_window(&sample_window(0, 1_000), None)
            .expect_err("401 must fail immediately");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "4xx responses must perform zero retries"
        );
        match err {
            AsrError::Http {
                status,
                message,
                retryable,
            } => {
                assert_eq!(status, Some(401));
                assert!(!retryable);
                assert!(
                    !message.contains(secret),
                    "secret must never appear in error message: {message}"
                );
            }
            other => panic!("expected Http error, got {other:?}"),
        }
    }

    #[test]
    fn redact_secret_scrubs_multiple_bearer_tokens() {
        let input = "first Bearer sk-alpha-111, second bearer sk-beta-222 — third BEARER \"sk-gamma-333\" and trailing Bearer sk-delta-444";
        let redacted = redact_secret(input, Some("sk-gamma-333"));

        for token in [
            "sk-alpha-111",
            "sk-beta-222",
            "sk-gamma-333",
            "sk-delta-444",
        ] {
            assert!(
                !redacted.contains(token),
                "token '{token}' must be scrubbed from: {redacted}"
            );
        }
        assert!(
            redacted.matches("[REDACTED]").count() >= 4,
            "expected all four occurrences to be redacted: {redacted}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_401_redacts_secret_straddling_512_byte_boundary() {
        let secret = "sk-straddle-9876543210-secret-token-abcd";
        assert_eq!(secret.len(), 40);
        let preamble = "a".repeat(496);
        let (base_url, hits, _captured) = spawn_mock_server(vec![MockResponse {
            status_line: "401 Unauthorized",
            content_type: "text/plain",
            body: format!("{preamble}{secret}"),
        }])
        .await;

        let mut cfg = OpenAiAsrConfig::new(base_url, "whisper-1");
        cfg.api_key_env = Some("TEST_STRADDLE_KEY".to_string());
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, move |key| {
            (key == "TEST_STRADDLE_KEY").then(|| secret.to_string())
        })
        .expect("backend");

        let err = backend
            .transcribe_window(&sample_window(0, 1_000), None)
            .expect_err("401 must fail immediately");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        match err {
            AsrError::Http {
                status,
                message,
                retryable,
            } => {
                assert_eq!(status, Some(401));
                assert!(!retryable);
                assert!(
                    !message.contains(secret),
                    "full secret must not appear: {message}"
                );
                assert!(
                    !message.contains(&secret[..6]),
                    "secret prefix must not leak across 512-byte boundary: {message}"
                );
                assert!(
                    !message.contains("straddle"),
                    "secret fragment must not survive in error message: {message}"
                );
                assert!(
                    message.contains("[REDACTED]"),
                    "expected [REDACTED] marker in error message: {message}"
                );
            }
            other => panic!("expected Http error, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_oversized_error_body_is_bounded() {
        let huge_body = "x".repeat(96 * 1024);
        let (base_url, hits, _captured) = spawn_mock_server(vec![MockResponse {
            status_line: "400 Bad Request",
            content_type: "text/plain",
            body: huge_body,
        }])
        .await;

        let cfg = OpenAiAsrConfig::new(base_url, "whisper-1");
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |_| None).expect("backend");

        let err = backend
            .transcribe_window(&sample_window(0, 1_000), None)
            .expect_err("400 must fail");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        match err {
            AsrError::Http {
                status,
                message,
                retryable,
            } => {
                assert_eq!(status, Some(400));
                assert!(!retryable);
                let (_, snippet) = message
                    .split_once(": ")
                    .expect("error message must include body snippet");
                assert!(
                    snippet.len() <= MAX_ERROR_BODY_BYTES,
                    "snippet len {} exceeded MAX_ERROR_BODY_BYTES ({MAX_ERROR_BODY_BYTES})",
                    snippet.len()
                );
                assert_eq!(snippet.len(), MAX_ERROR_BODY_BYTES);
            }
            other => panic!("expected Http error, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_timeout_expires_and_returns_retryable_http_error() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind slow listener");
        let addr = listener.local_addr().expect("slow listener addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = Arc::clone(&hits);

        tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                hits_bg.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _held_stream = stream;
                    tokio::time::sleep(Duration::from_millis(2_500)).await;
                });
            }
        });

        let mut cfg = OpenAiAsrConfig::new(format!("http://{addr}/v1"), "whisper-1");
        cfg.timeout_secs = 1;
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |_| None).expect("backend");

        let started = std::time::Instant::now();
        let err = backend
            .transcribe_window(&sample_window(0, 1_000), None)
            .expect_err("slow server must trigger request timeout");
        let elapsed = started.elapsed();

        assert_eq!(
            hits.load(Ordering::SeqCst),
            MAX_TRANSIENT_RETRIES + 1,
            "timeout must perform initial attempt plus single transient retry"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "timeout did not abort promptly: elapsed={elapsed:?}"
        );
        match err {
            AsrError::Http {
                status, retryable, ..
            } => {
                assert_eq!(status, None);
                assert!(retryable);
            }
            other => panic!("expected retryable Http transport error, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_302_redirect_is_not_followed_and_makes_zero_retries() {
        let trap_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind trap listener");
        let trap_port = trap_listener.local_addr().expect("trap addr").port();
        let trap_hits = Arc::new(AtomicUsize::new(0));
        let trap_hits_bg = Arc::clone(&trap_hits);

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = trap_listener.accept().await {
                trap_hits_bg.fetch_add(1, Ordering::SeqCst);
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });

        let redirect_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind redirect listener");
        let redirect_addr = redirect_listener.local_addr().expect("redirect addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = Arc::clone(&hits);

        tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut stream, _)) = redirect_listener.accept().await else {
                    break;
                };
                hits_bg.fetch_add(1, Ordering::SeqCst);
                let _req = read_http_request(&mut stream).await;
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{trap_port}/steal\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nRedirect!"
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let mut cfg = OpenAiAsrConfig::new(format!("http://{redirect_addr}/v1"), "whisper-1");
        cfg.api_key_env = Some("TEST_REDIRECT_KEY".to_string());
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |key| {
            (key == "TEST_REDIRECT_KEY").then(|| "sk-never-redirect-me".to_string())
        })
        .expect("backend");

        let err = backend
            .transcribe_window(&sample_window(0, 1_000), None)
            .expect_err("302 redirect must not be followed");
        tokio::task::yield_now().await;

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "302 redirect must perform zero retries"
        );
        assert_eq!(
            trap_hits.load(Ordering::SeqCst),
            0,
            "redirect target must never be contacted"
        );
        match err {
            AsrError::Http {
                status, retryable, ..
            } => {
                assert_eq!(status, Some(302));
                assert!(!retryable);
            }
            other => panic!("expected non-retryable 302 Http error, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_200_oversized_body_fails_closed_without_retry() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind oversized 200 listener");
        let addr = listener.local_addr().expect("oversized 200 addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = Arc::clone(&hits);

        tokio::spawn(async move {
            // Accept up to 2 connections so any unexpected retry is detected by `hits`.
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                hits_bg.fetch_add(1, Ordering::SeqCst);
                let _req = read_http_request(&mut stream).await;

                // Stream without Content-Length to exercise the chunk-by-chunk cap path.
                let header = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"text\":\"";
                if stream.write_all(header).await.is_err() {
                    continue;
                }
                let block = vec![b'a'; 64 * 1024];
                let mut sent = 9usize; // length of `{"text":"`
                let target = MAX_ASR_RESPONSE_BYTES + 128 * 1024;
                while sent < target {
                    if stream.write_all(&block).await.is_err() {
                        break;
                    }
                    sent += block.len();
                }
                let _ = stream.shutdown().await;
            }
        });

        let cfg = OpenAiAsrConfig::new(format!("http://{addr}/v1"), "whisper-1");
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |_| None).expect("backend");

        let err = backend
            .transcribe_window(&sample_window(0, 1_000), None)
            .expect_err("oversized 200 OK body must fail closed");
        tokio::task::yield_now().await;

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "oversized 200 OK response must perform zero retries"
        );
        match err {
            AsrError::Decode(msg) => {
                assert!(
                    msg.contains(&MAX_ASR_RESPONSE_BYTES.to_string()),
                    "error message must name byte cap ({MAX_ASR_RESPONSE_BYTES}): {msg}"
                );
                assert!(
                    msg.contains("safety cap"),
                    "error message must explain safety cap: {msg}"
                );
            }
            other => panic!("expected Decode error for oversized 2xx body, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_bounded_success_body_enforces_exact_cap_and_utf8() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind bounded reader listener");
        let addr = listener.local_addr().expect("bounded reader addr");

        let cap = 64usize;
        let exact_body = "x".repeat(cap);
        let over_body = "y".repeat(cap + 1);
        let exact_body_clone = exact_body.clone();
        let over_body_clone = over_body.clone();

        tokio::spawn(async move {
            let responses: Vec<Vec<u8>> = vec![
                // 1. Exact cap bytes with Content-Length -> succeeds.
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    exact_body_clone.len(),
                    exact_body_clone
                )
                .into_bytes(),
                // 2. Declared Content-Length > cap -> fast-rejected before streaming.
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    over_body_clone.len(),
                    over_body_clone
                )
                .into_bytes(),
                // 3. Streamed without Content-Length > cap -> rejected during chunk loop.
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{}",
                    over_body_clone
                )
                .into_bytes(),
                // 4. Invalid UTF-8 within cap -> rejected as Decode.
                {
                    let mut raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n".to_vec();
                    raw.extend_from_slice(&[0xff, 0xfe]);
                    raw
                },
            ];

            for payload in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let _req = read_http_request(&mut stream).await;
                let _ = stream.write_all(&payload).await;
                let _ = stream.shutdown().await;
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/test");

        // 1. Exact cap succeeds.
        let resp = client.get(&url).send().await.expect("send exact");
        let text = read_bounded_success_body(resp, cap, None)
            .await
            .expect("exact cap must succeed");
        assert_eq!(text, exact_body);

        // 2. Declared oversized Content-Length rejected.
        let resp = client.get(&url).send().await.expect("send over-cl");
        let err = read_bounded_success_body(resp, cap, None)
            .await
            .expect_err("declared Content-Length above cap must fail");
        assert!(matches!(err, AsrError::Decode(ref msg) if msg.contains("64-byte safety cap")));

        // 3. Streamed oversized body rejected.
        let resp = client.get(&url).send().await.expect("send over-stream");
        let err = read_bounded_success_body(resp, cap, None)
            .await
            .expect_err("streamed body above cap must fail");
        assert!(matches!(err, AsrError::Decode(ref msg) if msg.contains("64-byte safety cap")));

        // 4. Invalid UTF-8 rejected.
        let resp = client.get(&url).send().await.expect("send invalid utf8");
        let err = read_bounded_success_body(resp, cap, None)
            .await
            .expect_err("invalid UTF-8 must fail");
        assert!(matches!(err, AsrError::Decode(ref msg) if msg.contains("UTF-8")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_200_mid_stream_transport_error_retries_once_and_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind truncated-200 listener");
        let addr = listener.local_addr().expect("truncated-200 addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = Arc::clone(&hits);

        tokio::spawn(async move {
            for attempt in 0..2 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                hits_bg.fetch_add(1, Ordering::SeqCst);
                let _req = read_http_request(&mut stream).await;
                if attempt == 0 {
                    // Truncated chunked 200 OK: closes abruptly mid-stream before final 0\r\n\r\n.
                    let truncated = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10\r\n{\"text\":\"part";
                    let _ = stream.write_all(truncated).await;
                    let _ = stream.shutdown().await;
                } else {
                    let body = r#"{"text":"recovered after mid-stream drop"}"#;
                    let ok_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(ok_resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            }
        });

        let cfg = OpenAiAsrConfig::new(format!("http://{addr}/v1"), "whisper-1");
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |_| None).expect("backend");

        let transcript = backend
            .transcribe_window(&sample_window(0, 1_000), None)
            .expect("mid-stream transport error on 200 OK must retry once and succeed");

        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "mid-stream 200 OK failure must perform exactly one transient retry"
        );
        assert_eq!(transcript.text, "recovered after mid-stream drop");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_200_repeated_mid_stream_transport_error_fails_after_single_retry() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind repeated truncated-200 listener");
        let addr = listener.local_addr().expect("repeated truncated-200 addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = Arc::clone(&hits);

        tokio::spawn(async move {
            // Accept up to 3 connections to confirm retry count stays bounded at 1 + MAX_TRANSIENT_RETRIES.
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                hits_bg.fetch_add(1, Ordering::SeqCst);
                let _req = read_http_request(&mut stream).await;
                let truncated = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10\r\n{\"text\":\"part";
                let _ = stream.write_all(truncated).await;
                let _ = stream.shutdown().await;
            }
        });

        let cfg = OpenAiAsrConfig::new(format!("http://{addr}/v1"), "whisper-1");
        let mut backend = OpenAiAsrBackend::with_env_lookup(cfg, |_| None).expect("backend");

        let err = backend
            .transcribe_window(&sample_window(0, 1_000), None)
            .expect_err("repeated mid-stream transport failure must error after single retry");

        assert_eq!(
            hits.load(Ordering::SeqCst),
            1 + MAX_TRANSIENT_RETRIES,
            "repeated mid-stream 200 OK failure must stop after MAX_TRANSIENT_RETRIES"
        );
        match err {
            AsrError::Http {
                status, retryable, ..
            } => {
                assert_eq!(status, Some(200));
                assert!(retryable);
            }
            other => {
                panic!("expected retryable Http error on repeated mid-stream drop, got {other:?}")
            }
        }
    }
}

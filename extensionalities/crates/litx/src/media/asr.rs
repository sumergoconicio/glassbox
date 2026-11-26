//! Module: glassbox-cli ASR shared types and backend trait.
//! File: media/asr.rs.
//! Responsibility: Define shared ASR sampling constants, mode/format enums, window and chunk records, error types, and the pluggable `AsrBackend` trait.

use std::fmt;

/// Target mono PCM sample rate in Hz expected by every Glassbox ASR pipeline.
pub const SAMPLE_RATE_HZ: usize = 16_000;

/// Maximum audio window duration in seconds for chunked ASR transcription.
pub const WINDOW_SECONDS: usize = 30;

/// Overlap duration in seconds carried across adjacent sequential ASR windows.
pub const OVERLAP_SECONDS: usize = 4;

/// Audio sampling strategy selected by `--asr [short|adaptive|full]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrMode {
    /// Detect speech onset with local VAD and transcribe up to 60 seconds.
    Short,
    /// Sample independent 30-second windows every 15 minutes starting at 0:00.
    Adaptive,
    /// Transcribe the entire audio track using overlapped 30-second windows.
    Full,
}

impl AsrMode {
    /// Returns the canonical lowercase CLI and receipt string for this ASR mode.
    ///
    /// Accepts `self` by value. Returns `"short"`, `"adaptive"`, or `"full"`.
    /// This method performs no I/O and cannot fail.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Adaptive => "adaptive",
            Self::Full => "full",
        }
    }
}

/// Output container/codec format selected by `--extract-audio [wav|flac|m4a|opus]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioExtractFormat {
    /// 16 kHz mono signed 16-bit PCM wrapped in a RIFF/WAV header.
    Wav,
    /// Lossless FLAC audio stream.
    Flac,
    /// AAC audio inside a fragmented MP4 (`m4a`) container.
    M4a,
    /// Opus audio inside an Ogg container.
    Opus,
}

impl AudioExtractFormat {
    /// Returns the canonical lowercase filename extension for this extract format.
    ///
    /// Accepts `self` by value. Returns `"wav"`, `"flac"`, `"m4a"`, or `"opus"`.
    /// This method performs no I/O and cannot fail.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Opus => "opus",
        }
    }
}

/// One decoded mono 16 kHz floating-point PCM audio window scheduled for ASR.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioWindow {
    /// Zero-based sequential window index within the current transcription run.
    pub index: usize,
    /// Inclusive window start offset in milliseconds from the source audio start.
    pub start_ms: u64,
    /// Exclusive window end offset in milliseconds from the source audio start.
    pub end_ms: u64,
    /// Decoded mono 16 kHz IEEE-754 `f32` PCM samples normalized to `[-1.0, 1.0]`.
    pub pcm_16k_f32: Vec<f32>,
}

/// One raw or stitched transcript interval paired with its source time bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptChunk {
    /// Inclusive chunk start offset in milliseconds from the source audio start.
    pub start_ms: u64,
    /// Exclusive chunk end offset in milliseconds from the source audio start.
    pub end_ms: u64,
    /// Transcribed text associated with `[start_ms, end_ms)`.
    pub text: String,
}

/// Structured errors returned during ASR configuration validation or window transcription.
#[derive(Debug)]
#[allow(dead_code)] // Variants such as MissingAsset and Runtime are consumed by Wave 5 and Milestone 3 backends.
pub enum AsrError {
    /// Invalid or incomplete ASR configuration supplied by CLI, env, or `models.json`.
    Config(String),
    /// Required local model file or runtime shared library was missing or unreadable.
    MissingAsset(String),
    /// Remote HTTP ASR request failed with an optional status code and retry hint.
    Http {
        /// HTTP response status code when a response was received, or `None` on transport errors.
        status: Option<u16>,
        /// Redacted diagnostic message describing the HTTP failure without secrets.
        message: String,
        /// Whether retrying the request once is permitted (true for 5xx and transport resets).
        retryable: bool,
    },
    /// Audio encoding, response parsing, or token decoding failed.
    Decode(String),
    /// Local ASR inference runtime or execution provider failed.
    Runtime(String),
    /// Local OS I/O error occurred while reading configuration or model assets.
    Io(std::io::Error),
}

impl fmt::Display for AsrError {
    /// Formats a concise, secret-free diagnostic message for this ASR error.
    ///
    /// Accepts a mutable formatter. Returns `fmt::Result` after writing the
    /// variant description. Fails only if the underlying formatter fails.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg)
            | Self::MissingAsset(msg)
            | Self::Decode(msg)
            | Self::Runtime(msg) => f.write_str(msg),
            Self::Http {
                status: Some(code),
                message,
                ..
            } => write!(f, "ASR HTTP error (status {code}): {message}"),
            Self::Http {
                status: None,
                message,
                ..
            } => write!(f, "ASR HTTP error: {message}"),
            Self::Io(err) => write!(f, "ASR I/O error: {err}"),
        }
    }
}

impl std::error::Error for AsrError {
    /// Returns the wrapped OS error when this error is `AsrError::Io`.
    ///
    /// Accepts `&self`. Returns `Some` for `AsrError::Io` and `None` for all
    /// other variants. This method cannot fail.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AsrError {
    /// Converts a standard I/O error into `AsrError::Io`.
    ///
    /// Accepts an `std::io::Error`. Returns `AsrError::Io`. This conversion
    /// cannot fail.
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Pluggable automatic speech recognition backend operating on 16 kHz mono windows.
pub trait AsrBackend {
    /// Returns the short stable backend identifier (for example, `"openai"` or `"whisper-onnx"`).
    ///
    /// Accepts `&self`. Returns a static backend name string. Cannot fail.
    fn backend_name(&self) -> &'static str;

    /// Returns the configured model identifier reported in receipts and frontmatter.
    ///
    /// Accepts `&self`. Returns the active model name string slice. Cannot fail.
    fn model_id(&self) -> &str;

    /// Returns the provider or execution target label (for example, endpoint URL or `"cpu"`).
    ///
    /// Accepts `&self`. Returns the provider identifier string slice. Cannot fail.
    fn provider(&self) -> &str;

    /// Returns the optional wire protocol name when the backend speaks a selectable protocol.
    ///
    /// Accepts `&self`. Returns `None` by default, or `Some("transcriptions")` /
    /// `Some("chat")` for OpenAI-compatible HTTP backends. Cannot fail.
    fn protocol(&self) -> Option<&str> {
        None
    }

    /// Validates backend readiness before FFmpeg begins decoding audio.
    ///
    /// Accepts `&self`. Returns `Ok(())` when configuration, credentials, or
    /// local runtime assets are verified ready. Returns an actionable
    /// `AsrError` when fail-closed preconditions are not met.
    fn validate(&self) -> Result<(), AsrError>;

    /// Transcribes one decoded mono 16 kHz PCM audio window into a timestamped chunk.
    ///
    /// Accepts `&mut self`, an `AudioWindow` slice, and an optional BCP-47/ISO
    /// language hint. Returns a `TranscriptChunk` carrying the window's time
    /// interval and transcribed text. Returns `AsrError` on transport, decode,
    /// or inference failures.
    fn transcribe_window(
        &mut self,
        window: &AudioWindow,
        language: Option<&str>,
    ) -> Result<TranscriptChunk, AsrError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    struct DummyBackend;

    impl AsrBackend for DummyBackend {
        fn backend_name(&self) -> &'static str {
            "dummy"
        }

        fn model_id(&self) -> &str {
            "dummy-v1"
        }

        fn provider(&self) -> &str {
            "local-test"
        }

        fn validate(&self) -> Result<(), AsrError> {
            Ok(())
        }

        fn transcribe_window(
            &mut self,
            window: &AudioWindow,
            _language: Option<&str>,
        ) -> Result<TranscriptChunk, AsrError> {
            Ok(TranscriptChunk {
                start_ms: window.start_ms,
                end_ms: window.end_ms,
                text: "hello world".to_string(),
            })
        }
    }

    #[test]
    fn mode_and_extract_format_strings_match_contract() {
        assert_eq!(SAMPLE_RATE_HZ, 16_000);
        assert_eq!(WINDOW_SECONDS, 30);
        assert_eq!(OVERLAP_SECONDS, 4);

        assert_eq!(AsrMode::Short.as_str(), "short");
        assert_eq!(AsrMode::Adaptive.as_str(), "adaptive");
        assert_eq!(AsrMode::Full.as_str(), "full");

        assert_eq!(AudioExtractFormat::Wav.extension(), "wav");
        assert_eq!(AudioExtractFormat::Flac.extension(), "flac");
        assert_eq!(AudioExtractFormat::M4a.extension(), "m4a");
        assert_eq!(AudioExtractFormat::Opus.extension(), "opus");
    }

    #[test]
    fn asr_error_display_source_and_io_conversion() {
        let cfg = AsrError::Config("missing URL".to_string());
        assert_eq!(cfg.to_string(), "missing URL");
        assert!(cfg.source().is_none());

        let http_status = AsrError::Http {
            status: Some(503),
            message: "service unavailable".to_string(),
            retryable: true,
        };
        assert_eq!(
            http_status.to_string(),
            "ASR HTTP error (status 503): service unavailable"
        );

        let http_net = AsrError::Http {
            status: None,
            message: "connection reset".to_string(),
            retryable: true,
        };
        assert_eq!(http_net.to_string(), "ASR HTTP error: connection reset");

        let io_err = AsrError::from(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file missing",
        ));
        assert!(io_err.source().is_some());
        assert!(io_err.to_string().contains("file missing"));
    }

    #[test]
    fn dummy_backend_default_protocol_is_none() {
        let mut backend = DummyBackend;
        assert_eq!(backend.backend_name(), "dummy");
        assert_eq!(backend.model_id(), "dummy-v1");
        assert_eq!(backend.provider(), "local-test");
        assert_eq!(backend.protocol(), None);
        backend.validate().expect("dummy validate");

        let chunk = backend
            .transcribe_window(
                &AudioWindow {
                    index: 0,
                    start_ms: 0,
                    end_ms: 1_000,
                    pcm_16k_f32: vec![0.0; 16_000],
                },
                Some("en"),
            )
            .expect("dummy transcribe");
        assert_eq!(chunk.start_ms, 0);
        assert_eq!(chunk.end_ms, 1_000);
        assert_eq!(chunk.text, "hello world");
    }
}

// Module: glassbox-cli unified executable.
// File: main.rs.
// Responsibility: Route upstream parsing, schema v3 metadata-only inspection, canonical native-unit extraction, persistent model registry management, and local media contact-sheet/ASR/audio extraction.
use clap::{Args, Parser, Subcommand, ValueEnum};
use glassbox_inspect::hashing::HashPolicy;
use glassbox_inspect::reports::{
    Artifact, ArtifactKind, Completeness, ContactSheetReceipt, Diagnostic, DiagnosticSeverity,
    DiagnosticStage, EmbeddedMetadataField, ExtractOptions, ExtractReport, ExtractedContent,
    FactSource, InspectOptions, LEGACY_REPORT_SCHEMA_VERSION, MediaReceipt, MetadataReport,
    NativeSelection, NativeUnit, OmissionStatus, OperationStatus, QualifiedCount, ReportKind,
    SelectionReport, SessionBudget, SourcedMetadata, ThumbnailOptions,
};
use glassbox_inspect::{Inspector, extract_with_policy, inspect_with_policy};
use liteparse::LiteParseConfig;
use serde_json::Value;
use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};

mod batch;
mod inventory;
mod media;
mod models;
mod output;

/// Maximum accepted `--jobs` value, shared by inspect and extract.
///
/// Each job admits one worker thread, one slot in the bounded work and credit
/// channels, and 4 MiB of out-of-order completion buffering, so 32 keeps the
/// worst-case reorder reservation at 128 MiB and the thread count moderate.
/// Clap rejects larger values at parse time with the documented 1..=32 range.
const MAX_JOBS: usize = 32;

mod upstream {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/upstream.rs"));
}

#[derive(Parser, Debug)]
#[command(
    name = "glassbox",
    version = env!("GLASSBOX_UPSTREAM_VERSION"),
    about = "What's in the box?! Local file inspection and extraction."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Parse with every upstream liteparse command, option, and default.
    Parse,
    /// Inspect file identity, properties, and native structure without body extraction or OCR.
    Inspect(MetadataArgs),
    /// Extract all or selected native units into schema v3 reports or bodymatter:v1 Markdown.
    Extract(Box<ExtractArgs>),
    /// Manage persistent model endpoints and capability bindings (`models.json`).
    Models(ModelsArgs),
    /// Print the upstream library's default configuration as JSON.
    Defaults,
}

/// Output format selector for `glassbox models` read commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModelsFormat {
    /// Plain human-readable text output without resolving secrets.
    Text,
    /// Structured JSON output without resolving secrets.
    Json,
}

/// Top-level arguments for `glassbox models`.
#[derive(Debug, Args)]
struct ModelsArgs {
    /// Model registry management action.
    #[command(subcommand)]
    command: ModelsCommand,
}

/// Subcommands under `glassbox models`.
#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// Manage named OpenAI-compatible HTTP endpoints.
    Endpoint(ModelsEndpointArgs),
    /// Bind a capability (such as `asr`) to an OpenAI endpoint or local ONNX directory.
    Bind(ModelsBindArgs),
    /// Remove a capability binding.
    Unbind(ModelsUnbindArgs),
    /// Show configured endpoints and capability bindings (never prints secret values).
    Show(ModelsShowArgs),
}

/// Arguments for `glassbox models endpoint`.
#[derive(Debug, Args)]
struct ModelsEndpointArgs {
    /// Endpoint registry action.
    #[command(subcommand)]
    command: ModelsEndpointCommand,
}

/// Subcommands under `glassbox models endpoint`.
#[derive(Debug, Subcommand)]
enum ModelsEndpointCommand {
    /// Add or replace a named OpenAI-compatible HTTP endpoint.
    Add {
        /// Short endpoint identifier (`[A-Za-z0-9._-]`).
        name: String,
        /// HTTP(S) API base URL (for example `http://127.0.0.1:8080/v1`).
        #[arg(long)]
        base_url: String,
        /// Optional environment variable name storing the Bearer API key (never a raw secret).
        #[arg(long)]
        api_key_env: Option<String>,
    },
    /// List configured endpoints without reading environment secrets.
    List {
        /// Output format (`text` or `json`).
        #[arg(long, value_enum, default_value_t = ModelsFormat::Text)]
        format: ModelsFormat,
    },
    /// Remove a named endpoint (refuses if bound unless `--force` is supplied).
    Remove {
        /// Endpoint name to remove.
        name: String,
        /// Also remove any capability bindings referencing this endpoint.
        #[arg(long)]
        force: bool,
    },
}

/// Arguments for `glassbox models bind`.
#[derive(Debug, Args)]
struct ModelsBindArgs {
    /// Capability name to bind (for example `asr`).
    capability: String,
    /// Named OpenAI-compatible endpoint previously added via `models endpoint add`.
    #[arg(long, conflicts_with_all = ["local_onnx", "model_dir", "device"])]
    endpoint: Option<String>,
    /// Model identifier sent to the OpenAI-compatible endpoint.
    #[arg(long, conflicts_with_all = ["local_onnx", "model_dir", "device"])]
    model: Option<String>,
    /// Wire protocol (`transcriptions` or `chat`; defaults to `transcriptions`).
    #[arg(long, value_enum, conflicts_with_all = ["local_onnx", "model_dir", "device"])]
    protocol: Option<OpenAiProtocolArg>,
    /// Bind to a local offline ONNX model directory instead of an HTTP endpoint.
    #[arg(long, conflicts_with_all = ["endpoint", "model", "protocol"])]
    local_onnx: bool,
    /// Local filesystem directory containing ONNX encoder/decoder weights and tokenizer files.
    #[arg(long, conflicts_with_all = ["endpoint", "model", "protocol"])]
    model_dir: Option<PathBuf>,
    /// Execution device for local ONNX inference (`cpu` or `cuda`; defaults to `cpu`).
    #[arg(long, value_enum, conflicts_with_all = ["endpoint", "model", "protocol"])]
    device: Option<OnnxDeviceArg>,
}

/// Arguments for `glassbox models unbind`.
#[derive(Debug, Args)]
struct ModelsUnbindArgs {
    /// Capability name to unbind (for example `asr`).
    capability: String,
}

/// Arguments for `glassbox models show`.
#[derive(Debug, Args)]
struct ModelsShowArgs {
    /// Optional single capability name to inspect.
    capability: Option<String>,
    /// Output format (`text` or `json`).
    #[arg(long, value_enum, default_value_t = ModelsFormat::Text)]
    format: ModelsFormat,
}

/// CLI sampling mode selector for `--asr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AsrModeArg {
    /// Detect speech onset with local VAD and transcribe up to 60 seconds (default).
    Short,
    /// Sample independent 30-second windows every 15 minutes starting at 0:00.
    Adaptive,
    /// Transcribe the entire audio track using overlapped 30-second windows.
    Full,
}

impl From<AsrModeArg> for media::asr::AsrMode {
    /// Converts the CLI `--asr` mode argument into [`media::asr::AsrMode`].
    ///
    /// Accepts an [`AsrModeArg`]. Returns the corresponding [`media::asr::AsrMode`].
    /// Performs no I/O and cannot fail.
    fn from(value: AsrModeArg) -> Self {
        match value {
            AsrModeArg::Short => Self::Short,
            AsrModeArg::Adaptive => Self::Adaptive,
            AsrModeArg::Full => Self::Full,
        }
    }
}

/// CLI audio container/codec selector for `--extract-audio`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AudioExtractFormatArg {
    /// 16 kHz mono signed 16-bit PCM WAV (default).
    Wav,
    /// Lossless FLAC audio.
    Flac,
    /// AAC audio inside fragmented MP4 (`m4a`).
    M4a,
    /// Opus audio inside Ogg container.
    Opus,
}

impl From<AudioExtractFormatArg> for media::asr::AudioExtractFormat {
    /// Converts the CLI `--extract-audio` argument into [`media::asr::AudioExtractFormat`].
    ///
    /// Accepts an [`AudioExtractFormatArg`]. Returns the matching
    /// [`media::asr::AudioExtractFormat`]. Performs no I/O and cannot fail.
    fn from(value: AudioExtractFormatArg) -> Self {
        match value {
            AudioExtractFormatArg::Wav => Self::Wav,
            AudioExtractFormatArg::Flac => Self::Flac,
            AudioExtractFormatArg::M4a => Self::M4a,
            AudioExtractFormatArg::Opus => Self::Opus,
        }
    }
}

/// CLI ASR backend selector for `--asr-backend`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AsrBackendArg {
    /// External or local OpenAI-compatible HTTP ASR endpoint.
    Openai,
    /// Local offline Whisper ONNX runtime backend.
    #[value(name = "whisper-onnx")]
    WhisperOnnx,
}

impl From<AsrBackendArg> for models::AsrBackendKind {
    /// Converts the CLI `--asr-backend` argument into [`models::AsrBackendKind`].
    ///
    /// Accepts an [`AsrBackendArg`]. Returns the matching [`models::AsrBackendKind`].
    /// Performs no I/O and cannot fail.
    fn from(value: AsrBackendArg) -> Self {
        match value {
            AsrBackendArg::Openai => Self::Openai,
            AsrBackendArg::WhisperOnnx => Self::WhisperOnnx,
        }
    }
}

/// CLI OpenAI wire protocol selector for `--asr-protocol` and `models bind --protocol`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OpenAiProtocolArg {
    /// Multipart WAV upload to `/audio/transcriptions` (default).
    Transcriptions,
    /// Base64 WAV `input_audio` JSON request to `/chat/completions`.
    Chat,
}

impl From<OpenAiProtocolArg> for models::OpenAiProtocol {
    /// Converts the CLI OpenAI wire protocol argument into [`models::OpenAiProtocol`].
    ///
    /// Accepts an [`OpenAiProtocolArg`]. Returns the matching [`models::OpenAiProtocol`].
    /// Performs no I/O and cannot fail.
    fn from(value: OpenAiProtocolArg) -> Self {
        match value {
            OpenAiProtocolArg::Transcriptions => Self::Transcriptions,
            OpenAiProtocolArg::Chat => Self::Chat,
        }
    }
}

/// CLI local ONNX execution device selector for `--asr-device` and `models bind --device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OnnxDeviceArg {
    /// CPU execution provider (default).
    Cpu,
    /// CUDA execution provider (fails closed when unavailable).
    Cuda,
}

impl From<OnnxDeviceArg> for models::OnnxDevice {
    /// Converts the CLI ONNX device argument into [`models::OnnxDevice`].
    ///
    /// Accepts an [`OnnxDeviceArg`]. Returns the matching [`models::OnnxDevice`].
    /// Performs no I/O and cannot fail.
    fn from(value: OnnxDeviceArg) -> Self {
        match value {
            OnnxDeviceArg::Cpu => Self::Cpu,
            OnnxDeviceArg::Cuda => Self::Cuda,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Json,
    Text,
    Markdown,
}

/// CLI scheduling mode selector for `--imagegrid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImageGridArg {
    /// Fixed 5x5 grid sampling 25 evenly spaced midpoints.
    Default,
    /// Duration-scaled 5-column grid (1 row below 15 min, up to 10 rows above 150 min).
    Adaptive,
    /// Fixed-interval grid controlled by `--every-seconds` (capped at 250 tiles).
    Custom,
    /// Internal sentinel populated only when `--imagegrid` is supplied without a value.
    #[value(hide = true, name = "__bare__")]
    Bare,
}

/// CLI encoding selector for `--imagegrid-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImageGridFormatArg {
    /// JPEG encoded at quality 85.
    Jpg,
    /// Lossless PNG.
    Png,
}

impl From<ImageGridFormatArg> for media::contact_sheet::ImageGridFormat {
    /// Converts the CLI `--imagegrid-format` enum into the media encoder format.
    ///
    /// Accepts an [`ImageGridFormatArg`]. Returns the matching
    /// [`media::contact_sheet::ImageGridFormat`]. Performs no I/O and cannot fail.
    fn from(value: ImageGridFormatArg) -> Self {
        match value {
            ImageGridFormatArg::Jpg => Self::Jpg,
            ImageGridFormatArg::Png => Self::Png,
        }
    }
}

/// Validated contact-sheet generation and publication settings for `extract`.
#[derive(Debug, Clone, PartialEq)]
struct ContactSheetConfig {
    /// Scheduling mode (`default`, `adaptive`, or `custom` with validated interval).
    mode: media::contact_sheet::ImageGridMode,
    /// Resolution preset (`quality` by default, or `performance`).
    preset: media::contact_sheet::ImageGridPreset,
    /// Encoded image format (`jpg` or `png`).
    format: media::contact_sheet::ImageGridFormat,
    /// Explicit `-o PATH` destination when supplied instead of `--output-dir` naming.
    explicit_output: Option<PathBuf>,
}

/// Result of executing contact-sheet generation for one input file.
enum ContactSheetRun {
    /// Populated schema v3 extraction report (complete or failed with diagnostics).
    Report(Box<ExtractReport>),
    /// Invalid media parameters discovered after probing (such as `> 250` tiles), requiring exit `64`.
    InvalidInput(String),
}

#[derive(Debug, Args)]
struct MetadataArgs {
    #[arg(required_unless_present = "batch", conflicts_with = "batch")]
    path: Option<PathBuf>,
    /// Read one {"id":...,"path":"..."} JSON request per stdin line.
    #[arg(long, conflicts_with_all = ["pretty", "thumbnail", "format"])]
    batch: bool,
    /// Save a content-free v3 summary and v3 JSON report per document, without overwriting.
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Recurse into same-mount real subdirectories; links and other mounts are skipped.
    #[arg(long, conflicts_with_all = ["batch", "thumbnail", "pretty"])]
    recursive: bool,
    /// Maximum simultaneous files from 1 to 32, including recognition inside each file lease.
    #[arg(
        long,
        default_value_t = 1,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=MAX_JOBS as u64)
    )]
    jobs: usize,
    /// Explicitly render or decode one bounded thumbnail; ordinary inspection does neither.
    #[arg(long)]
    thumbnail: Option<PathBuf>,
    #[arg(long, default_value_t = 320, value_parser = clap::value_parser!(u32).range(1..=4096))]
    thumbnail_max_edge: u32,
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    #[arg(long)]
    pretty: bool,
    /// Stream a full SHA-256 digest even for files above the default sampled-fingerprint threshold.
    #[arg(long)]
    full_hash: bool,
    /// Include reader provenance details in v3 JSON and summary outputs.
    #[arg(long)]
    verbose: bool,
}

#[derive(Debug, Args)]
struct ExtractArgs {
    #[arg(required_unless_present = "batch", conflicts_with = "batch")]
    path: Option<PathBuf>,
    /// Read one {"id":...,"path":"..."} JSON request per stdin line.
    #[arg(long, conflicts_with_all = ["pretty", "thumbnail", "format"])]
    batch: bool,
    /// Save Markdown content and a v3 JSON report per document, without overwriting.
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Maximum simultaneous files from 1 to 32, including recognition inside each file lease.
    #[arg(
        long,
        default_value_t = 1,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=MAX_JOBS as u64)
    )]
    jobs: usize,
    /// Compatibility flag; local OCR remains limited to pages that need it.
    #[arg(long)]
    identify: bool,
    /// Whitespace-delimited word tokens, not model-specific subword tokens.
    #[arg(long)]
    max_tokens: Option<usize>,
    /// Select first five units when bare, first N for an integer, or inclusive ranges.
    #[arg(long, num_args = 0..=1, default_missing_value = "5", conflicts_with = "max_pages")]
    pages: Option<String>,
    /// Deprecated alias for `--pages N`.
    #[arg(long)]
    max_pages: Option<usize>,
    #[arg(long, conflicts_with_all = ["probe_ocr", "identify"])]
    no_ocr: bool,
    /// Local OCR language, such as `en`, `eng`, or `deu+fra`; default `eng`.
    #[arg(long, default_value = "eng")]
    ocr_language: String,
    #[arg(long)]
    max_chars: Option<usize>,
    #[arg(long, default_value_t = 5)]
    max_units: usize,
    #[arg(long)]
    thumbnail: Option<PathBuf>,
    #[arg(long, default_value_t = 320, value_parser = clap::value_parser!(u32).range(1..=4096))]
    thumbnail_max_edge: u32,
    #[arg(long)]
    packet: bool,
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    #[arg(long)]
    probe_ocr: bool,
    #[arg(long)]
    pretty: bool,
    /// Stream a full SHA-256 digest even for files above the default sampled-fingerprint threshold.
    #[arg(long)]
    full_hash: bool,
    /// Include reader and extraction provenance details in v3 JSON reports.
    #[arg(long)]
    verbose: bool,
    /// Omit `bodymatter:v1` structure comment markers from Markdown stdout and saved `.md` artifacts.
    #[arg(long)]
    no_markers: bool,
    /// Generate a video contact sheet (`default` 5x5, `adaptive` 1-10 rows, or `custom` with `--every-seconds`).
    #[arg(long, num_args = 0..=1, default_missing_value = "__bare__", value_enum)]
    imagegrid: Option<ImageGridArg>,
    /// Fixed screenshot interval in seconds for `--imagegrid custom` (or bare `--imagegrid`), capped at 250 tiles.
    #[arg(long, allow_negative_numbers = true)]
    every_seconds: Option<f64>,
    /// Use high-resolution 1,920 px contact-sheet cells (360x202); default when `--imagegrid` is enabled.
    #[arg(long, conflicts_with = "performance")]
    quality: bool,
    /// Use compact 1,280 px contact-sheet cells (240x134) for faster `--imagegrid` generation.
    #[arg(long, conflicts_with = "quality")]
    performance: bool,
    /// Contact-sheet image encoding (`jpg` or `png`; defaults to `jpg` under `--output-dir`).
    #[arg(long, value_enum)]
    imagegrid_format: Option<ImageGridFormatArg>,
    /// Explicit destination file path (`.jpg`, `.jpeg`, or `.png`) for single-file `--imagegrid`.
    #[arg(short = 'o', long = "output")]
    output: Option<PathBuf>,
    /// Transcribe audio via automatic speech recognition (`short` VAD <=60s default, `adaptive` 15-min samples, or `full`).
    #[arg(long, num_args = 0..=1, default_missing_value = "short", value_enum)]
    asr: Option<AsrModeArg>,
    /// Extract the full audio track to `<stem>-extract.<ext>` inside `--output-dir` (`wav`, `flac`, `m4a`, or `opus`).
    #[arg(long, num_args = 0..=1, default_missing_value = "wav", value_enum)]
    extract_audio: Option<AudioExtractFormatArg>,
    /// Explicit ASR backend selector (`openai` or `whisper-onnx`).
    #[arg(long, value_enum)]
    asr_backend: Option<AsrBackendArg>,
    /// OpenAI-compatible ASR API base URL override.
    #[arg(long)]
    asr_url: Option<String>,
    /// OpenAI-compatible ASR model identifier override.
    #[arg(long)]
    asr_model: Option<String>,
    /// Environment variable name storing the Bearer API key for OpenAI-compatible ASR (never a raw key).
    #[arg(long)]
    asr_api_key_env: Option<String>,
    /// OpenAI-compatible wire protocol override (`transcriptions` or `chat`).
    #[arg(long, value_enum)]
    asr_protocol: Option<OpenAiProtocolArg>,
    /// Local Whisper ONNX model directory override.
    #[arg(long)]
    asr_model_dir: Option<PathBuf>,
    /// Local Whisper ONNX execution device override (`cpu` or `cuda`).
    #[arg(long, value_enum)]
    asr_device: Option<OnnxDeviceArg>,
    /// Optional BCP-47 / ISO language hint forwarded to the ASR backend.
    #[arg(long)]
    asr_language: Option<String>,
    /// Omit `schema: "glassbox:v1"` YAML frontmatter from transcript Markdown stdout and saved `.md` artifacts.
    #[arg(long)]
    no_frontmatter: bool,
}

/// Finds the first extraction-only content or media flag passed to `inspect` before `--`.
///
/// Accepts the process argument vector. Returns `Some(flag)` when `args[1]` is
/// `"inspect"` and an extraction-only document or media flag (including
/// `--flag=value` spellings) appears before any `"--"` option terminator;
/// returns `None` for non-inspect routes, valid inspect flags, or literal
/// filenames placed after `"--"`. Performs no I/O and cannot panic.
fn find_disallowed_inspect_flag(args: &[std::ffi::OsString]) -> Option<String> {
    if args.get(1).is_none_or(|argument| argument != "inspect") {
        return None;
    }
    const FLAGS: &[&str] = &[
        "--identify",
        "--max-pages",
        "--max-tokens",
        "--max-chars",
        "--probe-ocr",
        "--no-ocr",
        "--ocr-language",
        "--packet",
        "--no-markers",
        "--imagegrid",
        "--every-seconds",
        "--quality",
        "--performance",
        "--imagegrid-format",
        "--asr",
        "--asr-backend",
        "--asr-url",
        "--asr-model",
        "--asr-api-key-env",
        "--asr-protocol",
        "--asr-model-dir",
        "--asr-device",
        "--asr-language",
        "--extract-audio",
        "--no-frontmatter",
    ];
    for argument in args.iter().skip(2) {
        if argument == "--" {
            break;
        }
        let argument = argument.to_string_lossy();
        if FLAGS
            .iter()
            .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
        {
            return Some(argument.into_owned());
        }
    }
    None
}

/// Rejects extraction-only content controls on inspect before Clap's generic error path.
///
/// Accepts the process argument vector. Returns unit for valid or non-inspect
/// routes and stops scanning at the `--` option terminator so literal file names
/// after `--` pass through to Clap as positional paths. Extraction-only flags
/// before `--` print explicit extract migration advice and terminate with exit
/// 64; no file is opened and no inspection session starts.
fn reject_inspect_content_flags(args: &[std::ffi::OsString]) {
    if let Some(argument) = find_disallowed_inspect_flag(args) {
        eprintln!(
            "error: `{argument}` belongs to content extraction, not metadata inspection.\n\
             Use `glassbox extract PATH {argument}` (or `litx extract ...`) for bounded body/OCR extraction."
        );
        std::process::exit(64);
    }
}

/// Dispatches upstream parse, content-free inspection, and transitional extraction.
///
/// Accepts the process argument vector implicitly. Runs synchronously on the
/// main thread and constructs a multi-threaded Tokio runtime only for the
/// upstream `parse` route, keeping `inspect`, `extract`, `models`, and
/// `defaults` strictly single-threaded through process initialization (including
/// `ORT_DISABLE_TELEMETRY` setup before batch threads or ONNX Runtime load).
/// Returns unit after requested output is flushed. Operational I/O/JSON errors
/// propagate; inspect report statuses map to 0 complete, 2 partial/unsupported,
/// and 1 failed, while legacy content flags on inspect and `--pretty` on a
/// directory terminate with 64 and migration advice. A supplied batch or
/// inventory output directory is initialized and validated with exit 64 before
/// any worker starts, so ordinary concurrent jobs never race on directory
/// creation and per-item publication keeps no-overwrite semantics.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if !args.is_empty() {
        args[0] = std::ffi::OsString::from("glassbox");
    }
    if args.get(1).is_some_and(|argument| argument == "parse") {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(upstream::run());
    }
    reject_inspect_content_flags(&args);
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let success = matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            let _ = error.print();
            std::process::exit(if success { 0 } else { 64 });
        }
    };
    match cli.command {
        Commands::Parse => unreachable!("parse is routed before downstream Clap"),
        Commands::Inspect(args) => {
            validate_jobs(args.jobs);
            let options = InspectOptions {
                thumbnail: args.thumbnail.clone().map(|path| ThumbnailOptions {
                    path,
                    max_edge: args.thumbnail_max_edge,
                }),
                budget: session_budget(args.jobs),
            };
            if args.batch {
                if let Some(directory) = args.output_dir.as_deref() {
                    initialize_output_directory(directory);
                }
                let outcome = run_metadata_batch(
                    &options,
                    args.output_dir.as_deref(),
                    args.jobs,
                    args.full_hash,
                    args.verbose,
                )?;
                finish_batch(outcome)?;
            } else {
                let path = args.path.as_ref().unwrap();
                let is_directory = std::fs::symlink_metadata(path)
                    .map(|metadata| metadata.is_dir())
                    .unwrap_or(false);
                if is_directory {
                    if args.pretty {
                        eprintln!(
                            "error: --pretty is not supported for directory inspection; directory mode emits ordered JSONL only"
                        );
                        std::process::exit(64);
                    }
                    if args.format != Format::Json {
                        eprintln!(
                            "error: directory inspection emits ordered JSONL only; use a single-file path for other formats"
                        );
                        std::process::exit(64);
                    }
                    if args.thumbnail.is_some() {
                        eprintln!(
                            "error: directory inspection emits JSONL only and cannot use one thumbnail path"
                        );
                        std::process::exit(64);
                    }
                    if let Some(output) = args.output_dir.as_deref()
                        && inventory::output_inside_root(path, output)
                    {
                        eprintln!(
                            "error: directory inventory artifacts must be outside the source directory"
                        );
                        std::process::exit(64);
                    }
                    if let Some(directory) = args.output_dir.as_deref() {
                        initialize_output_directory(directory);
                    }
                    let output = std::io::stdout();
                    let mut output = output.lock();
                    let status = inventory::run(
                        path,
                        args.recursive,
                        args.jobs,
                        args.output_dir.as_deref(),
                        args.full_hash,
                        args.verbose,
                        &mut output,
                    )?;
                    finish_inspect(status)?;
                } else {
                    if args.recursive {
                        eprintln!("error: --recursive requires a directory inspection path");
                        std::process::exit(64);
                    }
                    let mut report =
                        inspect_with_policy(path, &options, hash_policy(args.full_hash));
                    if let Some(directory) = args.output_dir {
                        output::persist_metadata(&mut report, &directory, 1, args.verbose);
                    }
                    print_metadata(&report, args.format, args.pretty, args.verbose)?;
                    finish_inspect(report.metadata.status)?;
                }
            }
        }
        Commands::Extract(args) => {
            validate_jobs(args.jobs);
            let contact_sheet = match contact_sheet_config(&args) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(64);
                }
            };
            let audio_pipeline = match audio_pipeline_config(&args) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(i32::from(error.exit_code()));
                }
            };
            if matches!(
                audio_pipeline
                    .as_ref()
                    .and_then(|cfg| cfg.asr_config.as_ref()),
                Some(models::ResolvedAsrConfig::WhisperOnnx { .. })
            ) {
                // SAFETY: `fn main()` is synchronous and has not spawned a Tokio
                // runtime or any worker threads on the `extract` route, so this
                // call executes while the process is strictly single-threaded
                // before `run_media_batch` spawns threads or `ort::init_from`
                // loads `libonnxruntime`.
                media::asr_whisper::ensure_ort_telemetry_disabled();
            }
            let options = match extraction_options(&args) {
                Ok(options) => options,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(64);
                }
            };
            let include_markers = !args.no_markers;
            let include_frontmatter = !args.no_frontmatter;
            if contact_sheet.is_some() || audio_pipeline.is_some() {
                if args.batch {
                    let directory = args.output_dir.as_deref().map(initialize_output_directory);
                    let outcome = run_media_batch(
                        contact_sheet.as_ref(),
                        audio_pipeline.as_ref(),
                        directory.as_deref(),
                        args.jobs,
                        args.full_hash,
                        args.verbose,
                        include_markers,
                        include_frontmatter,
                    )?;
                    finish_batch(outcome)?;
                } else {
                    let validated_output_dir =
                        args.output_dir.as_deref().map(initialize_output_directory);
                    let path = args.path.as_ref().unwrap();
                    let concurrency = contact_sheet_concurrency(false, args.jobs);
                    let metadata_report = inspect_with_policy(
                        path,
                        &InspectOptions::default(),
                        hash_policy(args.full_hash),
                    );
                    let mut report = match execute_media_extract(
                        path,
                        metadata_report,
                        contact_sheet.as_ref(),
                        audio_pipeline.as_ref(),
                        validated_output_dir.as_deref(),
                        concurrency,
                    ) {
                        ContactSheetRun::Report(report) => *report,
                        ContactSheetRun::InvalidInput(message) => {
                            eprintln!("error: {message}");
                            std::process::exit(64);
                        }
                    };
                    let rendered_markdown = validated_output_dir.as_deref().map(|directory| {
                        output::persist_extract(
                            &mut report,
                            directory,
                            1,
                            args.verbose,
                            include_markers,
                            include_frontmatter,
                        )
                    });
                    print_extract(
                        &report,
                        args.format,
                        args.pretty,
                        args.verbose,
                        include_markers,
                        include_frontmatter,
                        rendered_markdown.as_deref(),
                    )?;
                    if args.format != Format::Json
                        && matches!(
                            report.extraction_status(),
                            OperationStatus::Failed | OperationStatus::Partial
                        )
                    {
                        for diagnostic in report
                            .diagnostics
                            .iter()
                            .filter(|item| item.severity == DiagnosticSeverity::Error)
                        {
                            eprintln!("error: {}", diagnostic.message);
                        }
                    }
                    finish_inspect(report.extraction_status())?;
                }
            } else if args.batch {
                if let Some(directory) = args.output_dir.as_deref() {
                    initialize_output_directory(directory);
                }
                let outcome = run_extract_batch(
                    &options,
                    args.output_dir.as_deref(),
                    args.jobs,
                    args.full_hash,
                    args.verbose,
                    include_markers,
                    include_frontmatter,
                )?;
                finish_batch(outcome)?;
            } else {
                let mut report = extract_with_policy(
                    args.path.as_ref().unwrap(),
                    &options,
                    hash_policy(args.full_hash),
                );
                let rendered_markdown = args.output_dir.as_deref().map(|directory| {
                    output::persist_extract(
                        &mut report,
                        directory,
                        1,
                        args.verbose,
                        include_markers,
                        include_frontmatter,
                    )
                });
                print_extract(
                    &report,
                    args.format,
                    args.pretty,
                    args.verbose,
                    include_markers,
                    include_frontmatter,
                    rendered_markdown.as_deref(),
                )?;
                finish_inspect(report.extraction_status())?;
            }
        }
        Commands::Models(args) => {
            if let Err(error) = run_models_command(args) {
                eprintln!("error: {error}");
                std::process::exit(i32::from(error.exit_code()));
            }
        }
        Commands::Defaults => {
            writeln!(
                std::io::stdout().lock(),
                "{}",
                serde_json::to_string_pretty(&LiteParseConfig::default())?
            )?;
        }
    }
    Ok(())
}

/// Rejects zero or oversize job counts before any input path or worker opens.
///
/// Accepts the parsed unsigned job count. Returns unit for values inside
/// 1..=MAX_JOBS. Out-of-range values print a usage diagnostic and exit 64;
/// Clap already rejects the same range at parse time, so this guard only
/// protects future callers that bypass argument parsing.
fn validate_jobs(jobs: usize) {
    if jobs == 0 || jobs > MAX_JOBS {
        eprintln!("error: --jobs must be between 1 and {MAX_JOBS}");
        std::process::exit(64);
    }
}

/// Initializes the private output directory before any worker starts.
///
/// Accepts the user-supplied output directory. Returns the validated absolute
/// path after creating missing components privately through atomic
/// no-replace creation that tolerates concurrent creators. Symbolic links,
/// non-directory components, root escapes, and group/other-accessible final
/// directories terminate with exit 64 before any input is read, any worker
/// starts, or any file is published; per-item no-overwrite semantics remain
/// unchanged at the publication stage.
fn initialize_output_directory(directory: &std::path::Path) -> std::path::PathBuf {
    match output::initialize_output_directory(directory) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: cannot initialize private output directory: {error}");
            std::process::exit(64);
        }
    }
}

/// Derives bounded queue and result capacities from the one shared job count.
///
/// Accepts a validated positive job count. Returns metadata/extraction session
/// controls with equal file/OCR slots and N-item queue/reorder limits. No pool,
/// file, OCR engine, or allocation beyond the small value is created.
fn session_budget(jobs: usize) -> SessionBudget {
    SessionBudget {
        shared_job_slots: jobs,
        queue_capacity: jobs,
        result_buffer_capacity: jobs,
    }
}

/// Builds canonical extraction options and validates selector compatibility.
///
/// Accepts parsed CLI arguments. Returns full selection by default, first five
/// for bare `--pages`, checked first-N/range selections otherwise, and local
/// OCR controls carrying the requested `--ocr-language` spelling (`eng` by
/// default). Zero, malformed, overflowing, or conflicting values return an
/// error suitable for exit 64 before any input file is opened.
fn extraction_options(args: &ExtractArgs) -> Result<ExtractOptions, String> {
    let selection = match (&args.pages, args.max_pages) {
        (Some(_), Some(_)) => {
            return Err("--pages conflicts with deprecated --max-pages".into());
        }
        (Some(value), None) => glassbox_inspect::selection::parse_pages(value)?,
        (None, Some(count)) => {
            if count == 0 {
                return Err("--max-pages must be a positive integer".into());
            }
            eprintln!("warning: --max-pages is deprecated; use --pages {count}");
            NativeSelection::FirstN {
                count: count as u64,
            }
        }
        (None, None) => NativeSelection::All,
    };
    if args.packet {
        eprintln!(
            "warning: --packet is deprecated; v3 content.units carries the extracted payload"
        );
    }
    Ok(ExtractOptions {
        selection,
        max_tokens: args.max_tokens,
        max_chars: args.max_chars.unwrap_or(64 << 20),
        max_units: args.max_units,
        thumbnail: args.thumbnail.as_ref().map(|path| ThumbnailOptions {
            path: path.clone(),
            max_edge: args.thumbnail_max_edge,
        }),
        ocr: glassbox_inspect::reports::OcrOptions {
            enabled: !args.no_ocr,
            language: args.ocr_language.clone(),
            ..Default::default()
        },
        budget: session_budget(args.jobs),
    })
}

/// Maps the CLI `--full-hash` flag to the library [`HashPolicy`].
///
/// Accepts `full_hash` (`true` selects [`HashPolicy::Full`]; `false` selects
/// [`HashPolicy::SampledAboveThreshold`]). Returns the corresponding policy.
/// Performs no I/O and cannot fail.
fn hash_policy(full_hash: bool) -> HashPolicy {
    if full_hash {
        HashPolicy::Full
    } else {
        HashPolicy::SampledAboveThreshold
    }
}

/// Prints selected content or its canonical schema v3 JSON report.
///
/// Accepts an extraction report, output format, JSON pretty flag, `verbose`
/// provenance flag, `include_markers` (`true` for `bodymatter:v1` comments,
/// `false` for `--no-markers`), `include_frontmatter` (`true` for
/// `schema: "glassbox:v1"` transcript YAML frontmatter, `false` for
/// `--no-frontmatter`), and an optional `pre_rendered_markdown` string returned
/// by [`output::persist_extract`] so combined `--output-dir` + `--format markdown`
/// runs emit the exact same rendered Markdown (including `id` ULID and
/// `updated_at` timestamp) to stdout and disk. Returns unit after stdout output.
/// Serialization and stdout failures are returned; text and Markdown include
/// only selected content, never metadata fabrication.
fn print_extract(
    report: &ExtractReport,
    format: Format,
    pretty: bool,
    verbose: bool,
    include_markers: bool,
    include_frontmatter: bool,
    pre_rendered_markdown: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rendered: std::borrow::Cow<'_, str> = match format {
        Format::Markdown => match pre_rendered_markdown {
            Some(markdown) => std::borrow::Cow::Borrowed(markdown),
            None => std::borrow::Cow::Owned(glassbox_inspect::selection::render_markdown_full(
                report,
                include_markers,
                include_frontmatter,
            )),
        },
        Format::Text => std::borrow::Cow::Owned(glassbox_inspect::selection::render_text(report)),
        Format::Json if pretty => {
            std::borrow::Cow::Owned(serde_json::to_string_pretty(&report.to_v3(verbose))?)
        }
        Format::Json => std::borrow::Cow::Owned(serde_json::to_string(&report.to_v3(verbose))?),
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(rendered.as_bytes())?;
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

/// Prints one metadata report in the selected content-free schema v3 representation.
///
/// Accepts a report, format, JSON pretty-print flag, and `verbose` provenance
/// flag. Returns unit after stdout output. JSON serialization and stdout write
/// failures are returned; text uses `summary_text` and Markdown uses
/// `summary_markdown`, both content-free.
fn print_metadata(
    report: &MetadataReport,
    format: Format,
    pretty: bool,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let rendered = match format {
        Format::Markdown => glassbox_inspect::metadata::summary_markdown(report, verbose),
        Format::Text => glassbox_inspect::metadata::summary_text(report, verbose),
        Format::Json if pretty => serde_json::to_string_pretty(&report.to_v3(verbose))?,
        Format::Json => serde_json::to_string(&report.to_v3(verbose))?,
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(rendered.as_bytes())?;
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

/// Maps the final aggregate inspect status to the public process exit code.
///
/// Accepts complete, partial, unsupported, failed, or unknown status. Returns
/// unit for complete after flushing stdout. Partial/unsupported exits 2;
/// failed/unknown exits 1. Stdout flush failures are returned before exit.
fn finish_inspect(status: OperationStatus) -> Result<(), Box<dyn std::error::Error>> {
    std::io::stdout().flush()?;
    match status {
        OperationStatus::Complete => Ok(()),
        OperationStatus::Partial | OperationStatus::Unsupported => std::process::exit(2),
        OperationStatus::Failed | OperationStatus::Unknown => std::process::exit(1),
    }
}

/// Flushes JSONL output and applies invalid/failure/partial/complete exit severity.
///
/// Accepts the aggregate batch outcome. Returns unit only for a completely
/// valid successful batch. Invalid requests exit 64, operational failures 1,
/// and partial/unsupported reports 2; stdout flush failures are returned.
fn finish_batch(outcome: batch::BatchOutcome) -> Result<(), Box<dyn std::error::Error>> {
    std::io::stdout().flush()?;
    if outcome.invalid {
        std::process::exit(64);
    }
    finish_inspect(outcome.status)
}

/// Stdin reader that wakes on cancellation even when the writer keeps the pipe open.
struct PollStdin {
    input: std::io::Stdin,
    cancellation: batch::CancellationToken,
}

impl Read for PollStdin {
    /// Reads available stdin bytes after bounded cancellation-aware polling.
    ///
    /// Accepts the caller's read buffer. Returns bytes read, clean EOF, or
    /// Interrupted after cancellation. Poll/read OS errors propagate; a 50ms
    /// timeout rechecks cancellation without treating temporary no-data as EOF.
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            #[repr(C)]
            struct PollFd {
                fd: i32,
                events: i16,
                revents: i16,
            }
            unsafe extern "C" {
                fn poll(descriptors: *mut PollFd, count: usize, timeout_ms: i32) -> i32;
            }
            loop {
                if self.cancellation.is_cancelled() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "batch input cancelled",
                    ));
                }
                let mut descriptor = PollFd {
                    fd: 0,
                    events: 0x001,
                    revents: 0,
                };
                // SAFETY: the descriptor is a valid pollfd and stdin stays process-owned.
                let ready = unsafe { poll(&mut descriptor, 1, 50) };
                if ready > 0 {
                    return self.input.read(buffer);
                }
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            self.input.read(buffer)
        }
    }
}
/// Reads one JSONL line with a 65536-byte cap and bounded oversized-line drain.
///
/// Accepts a buffered input and reusable byte vector. Returns `Ok(true)` for
/// one line and `Ok(false)` for clean EOF, signal-interrupted input, or a
/// closed pipe (including while draining an oversized line). Other I/O errors
/// are returned. At most 65537 bytes plus the existing buffer are retained.
fn read_capped_line(input: &mut dyn BufRead, line: &mut Vec<u8>) -> std::io::Result<bool> {
    const MAX_LINE: usize = 65_536;
    line.clear();
    let read = match (&mut *input)
        .take((MAX_LINE + 1) as u64)
        .read_until(b'\n', line)
    {
        Ok(0) => return Ok(false),
        Ok(_) => true,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::BrokenPipe
            ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if line.len() > MAX_LINE && line.last() != Some(&b'\n') {
        match input.skip_until(b'\n') {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(read)
}

/// Produces bounded JSONL requests and immediate per-line validation responses.
///
/// Accepts the batch producer with its admission credits. Returns unit at EOF or
/// cancellation. Input I/O and disconnected queue failures are returned; malformed,
/// blank, missing-path, and oversized lines become invalid responses and later
/// valid requests continue with 64-over-1-over-2-over-0 aggregate severity.
fn produce_jsonl(producer: &mut batch::BatchProducer) -> Result<(), String> {
    let input = PollStdin {
        input: std::io::stdin(),
        cancellation: producer.cancellation_token(),
    };
    let mut input = std::io::BufReader::new(input);
    let mut line = Vec::new();
    loop {
        if !read_capped_line(&mut input, &mut line).map_err(|error| error.to_string())? {
            return Ok(());
        }
        let completion = if line.len() > 65_536 {
            Some(batch::BatchCompletion::invalid(
                Value::Null,
                "request exceeds 65536-byte line limit",
            ))
        } else {
            match serde_json::from_slice::<Value>(&line) {
                Ok(request) => match request.get("path").and_then(|path| path.as_str()) {
                    Some(path) => {
                        let id = request.get("id").cloned().unwrap_or(Value::Null);
                        if producer.request(id, PathBuf::from(path), None)? {
                            line = Vec::new();
                            continue;
                        }
                        return Ok(());
                    }
                    None => Some(batch::BatchCompletion::invalid(
                        request.get("id").cloned().unwrap_or(Value::Null),
                        "request requires a string path",
                    )),
                },
                Err(error) => Some(batch::BatchCompletion::invalid(
                    Value::Null,
                    error.to_string(),
                )),
            }
        };
        if let Some(completion) = completion
            && !producer.complete(completion)?
        {
            return Ok(());
        }
        line = Vec::new();
    }
}

/// Runs content-free schema v3 JSONL inspection through the actual shared worker budget.
///
/// Accepts metadata options, optional private artifact directory, positive
/// jobs, `full_hash` override, and `verbose` provenance flag. Returns aggregate
/// status after ordered incremental responses and worker join.
/// Input/stdout/serialization failures return errors; per-file diagnostics and
/// invalid lines remain ordered JSONL responses with preserved external IDs.
fn run_metadata_batch(
    options: &InspectOptions,
    output_dir: Option<&std::path::Path>,
    jobs: usize,
    full_hash: bool,
    verbose: bool,
) -> Result<batch::BatchOutcome, Box<dyn std::error::Error>> {
    let output = std::io::stdout();
    let mut output = output.lock();
    batch::run(
        jobs,
        produce_jsonl,
        |budget, _cancellation| {
            let mut inspector = Inspector::with_shared_budget(budget).with_full_hash(full_hash);
            move |request| {
                let sequence = request.sequence;
                let mut report = inspector.inspect(&request.path, options);
                if let Some(directory) = output_dir {
                    crate::output::persist_metadata(
                        &mut report,
                        directory,
                        sequence as usize,
                        verbose,
                    );
                }
                let status = report.metadata.status;
                batch::BatchCompletion::report(
                    request,
                    status,
                    serde_json::to_value(report.to_v3(verbose)),
                )
            }
        },
        &mut output,
    )
    .map_err(std::io::Error::other)
    .map_err(Into::into)
}

/// Runs canonical schema v3 native-unit extraction through the actual shared worker budget.
///
/// Accepts extraction options, optional private artifact directory, positive
/// jobs, `full_hash` override, `verbose` provenance flag, `include_markers`
/// (`!no_markers`), and `include_frontmatter` (`!no_frontmatter`) for persisted
/// Markdown artifacts. Returns aggregate status after ordered flushed responses
/// and worker join. Native content remains available on OCR failure; malformed
/// transport and output failures retain their respective per-item invalid or
/// process operational status.
fn run_extract_batch(
    options: &ExtractOptions,
    output_dir: Option<&std::path::Path>,
    jobs: usize,
    full_hash: bool,
    verbose: bool,
    include_markers: bool,
    include_frontmatter: bool,
) -> Result<batch::BatchOutcome, Box<dyn std::error::Error>> {
    let output = std::io::stdout();
    let mut output = output.lock();
    batch::run(
        jobs,
        produce_jsonl,
        |budget, _cancellation| {
            let mut inspector = Inspector::with_shared_budget(budget).with_full_hash(full_hash);
            move |request| {
                let sequence = request.sequence;
                let mut report = inspector.extract(&request.path, options);
                if let Some(directory) = output_dir {
                    crate::output::persist_extract(
                        &mut report,
                        directory,
                        sequence as usize,
                        verbose,
                        include_markers,
                        include_frontmatter,
                    );
                }
                let status = report.extraction_status();
                batch::BatchCompletion::report(
                    request,
                    status,
                    serde_json::to_value(report.to_v3(verbose)),
                )
            }
        },
        &mut output,
    )
    .map_err(std::io::Error::other)
    .map_err(Into::into)
}

/// Validates an explicit `-o/--output` file path and derives its contact-sheet format.
///
/// Accepts a user-supplied output path. Returns `ImageGridFormat::Jpg` for
/// `.jpg`/`.jpeg` or `ImageGridFormat::Png` for `.png` (ASCII case-insensitive)
/// when the path ends with a normal filename component. Empty paths, trailing
/// directory separators, `.`/`..` endings, and unsupported or missing
/// extensions return an exit-64 error string before any filesystem I/O occurs.
fn validate_output_file_format(
    path: &Path,
) -> Result<media::contact_sheet::ImageGridFormat, String> {
    let raw = path.as_os_str().to_string_lossy();
    if raw.is_empty()
        || raw == "."
        || raw == ".."
        || raw.ends_with('/')
        || raw.ends_with('\\')
        || raw.ends_with("/.")
        || raw.ends_with("/..")
        || raw.ends_with("\\.")
        || raw.ends_with("\\..")
        || !matches!(path.components().next_back(), Some(Component::Normal(_)))
    {
        return Err("-o/--output path must end with a normal .jpg, .jpeg, or .png filename".into());
    }
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("jpg" | "jpeg") => Ok(media::contact_sheet::ImageGridFormat::Jpg),
        Some("png") => Ok(media::contact_sheet::ImageGridFormat::Png),
        _ => Err("-o/--output path must end with .jpg, .jpeg, or .png".into()),
    }
}

/// Validates all contact-sheet CLI flag combinations before opening any input file.
///
/// Accepts parsed [`ExtractArgs`]. Returns `Ok(None)` when `--imagegrid` is not
/// requested and no contact-sheet-only flags were supplied, or
/// `Ok(Some(ContactSheetConfig))` when all mode, interval, preset, format,
/// batch, and destination constraints are satisfied. Returns `Err(String)`
/// suitable for exit `64` on missing destinations, `-o` without `--imagegrid`,
/// `-o` with `--batch`, invalid `-o` suffixes, conflicting `--imagegrid-format`
/// and `-o` suffixes, invalid `--every-seconds` pairings/bounds, or orphan
/// `--quality`/`--performance`/`--imagegrid-format` flags.
fn contact_sheet_config(args: &ExtractArgs) -> Result<Option<ContactSheetConfig>, String> {
    use media::contact_sheet::{ImageGridFormat, ImageGridMode, ImageGridPreset};

    let Some(grid_arg) = args.imagegrid else {
        if args.output.is_some() {
            return Err("-o/--output requires --imagegrid".into());
        }
        if args.every_seconds.is_some() {
            return Err("--every-seconds requires --imagegrid".into());
        }
        if args.quality {
            return Err("--quality requires --imagegrid".into());
        }
        if args.performance {
            return Err("--performance requires --imagegrid".into());
        }
        if args.imagegrid_format.is_some() {
            return Err("--imagegrid-format requires --imagegrid".into());
        }
        return Ok(None);
    };

    if args.batch && args.output.is_some() {
        return Err("-o/--output cannot be used with --batch; use --output-dir instead".into());
    }
    if args.output.is_none() && args.output_dir.is_none() {
        return Err(
            "--imagegrid requires an explicit destination: pass -o PATH or --output-dir DIR".into(),
        );
    }
    if args.quality && args.performance {
        return Err("--quality cannot be used with --performance".into());
    }

    let mode = match (grid_arg, args.every_seconds) {
        (ImageGridArg::Bare | ImageGridArg::Default, None) => ImageGridMode::Default,
        (ImageGridArg::Default, Some(_)) => {
            return Err(
                "--every-seconds cannot be used with --imagegrid default; use --imagegrid custom or omit the mode"
                    .into(),
            );
        }
        (ImageGridArg::Adaptive, None) => ImageGridMode::Adaptive,
        (ImageGridArg::Adaptive, Some(_)) => {
            return Err(
                "--every-seconds cannot be used with --imagegrid adaptive; use --imagegrid custom or omit the mode"
                    .into(),
            );
        }
        (ImageGridArg::Custom, None) => {
            return Err("--imagegrid custom requires --every-seconds".into());
        }
        (ImageGridArg::Custom | ImageGridArg::Bare, Some(secs)) => {
            if !secs.is_finite() || secs < 0.000_001 {
                return Err(
                    "--every-seconds must be a finite number of at least 0.000001 seconds".into(),
                );
            }
            ImageGridMode::Custom {
                every_seconds: secs,
            }
        }
    };

    let preset = if args.performance {
        ImageGridPreset::Performance
    } else {
        ImageGridPreset::Quality
    };

    let format = if let Some(out_path) = args.output.as_deref() {
        let ext_format = validate_output_file_format(out_path)?;
        if let Some(explicit_format) = args.imagegrid_format {
            let requested: ImageGridFormat = explicit_format.into();
            if requested != ext_format {
                return Err(format!(
                    "--imagegrid-format {} conflicts with -o/--output extension (.{})",
                    contact_sheet_extension(requested),
                    contact_sheet_extension(ext_format),
                ));
            }
        }
        ext_format
    } else {
        args.imagegrid_format
            .map(Into::into)
            .unwrap_or(ImageGridFormat::Jpg)
    };

    Ok(Some(ContactSheetConfig {
        mode,
        preset,
        format,
        explicit_output: args.output.clone(),
    }))
}

/// Computes the bounded per-video FFmpeg worker concurrency for contact sheets.
///
/// Accepts whether execution is in `--batch` mode and the CLI `--jobs` count.
/// Returns `1` per file inside multi-job batch runs (`batch && jobs > 1`), or
/// `min(4, available_parallelism)` for single-file and single-job batch runs.
/// Performs no I/O and cannot fail.
fn contact_sheet_concurrency(batch: bool, jobs: usize) -> usize {
    if batch && jobs > 1 {
        1
    } else {
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1)
            .min(4)
    }
}

/// Returns the canonical lowercase file extension for an [`ImageGridFormat`].
///
/// Accepts an [`media::contact_sheet::ImageGridFormat`]. Returns `"jpg"` or
/// `"png"`. Performs no I/O and cannot fail.
fn contact_sheet_extension(format: media::contact_sheet::ImageGridFormat) -> &'static str {
    match format {
        media::contact_sheet::ImageGridFormat::Jpg => "jpg",
        media::contact_sheet::ImageGridFormat::Png => "png",
    }
}

/// Executes a `glassbox models` subcommand against the persistent `models.json` registry.
///
/// Accepts parsed [`ModelsArgs`]. Loads and atomically saves `$GLASSBOX_CONFIG_DIR/models.json`
/// (or XDG/HOME fallback) with `0700`/`0600` permissions, validates endpoint URLs and
/// `--api-key-env` identifiers without storing or printing secret values, and writes
/// requested text or JSON output to stdout. Returns [`models::ModelsError::Usage`]
/// (exit `64`) on invalid arguments or dangling endpoint removal without `--force`,
/// or [`models::ModelsError::Config`] / [`models::ModelsError::Io`] (exit `1`) on
/// configuration or filesystem failures.
fn run_models_command(args: ModelsArgs) -> Result<(), models::ModelsError> {
    let mut stdout = std::io::stdout().lock();
    match args.command {
        ModelsCommand::Endpoint(ep_args) => match ep_args.command {
            ModelsEndpointCommand::Add {
                name,
                base_url,
                api_key_env,
            } => {
                let mut config = models::load_config()?;
                let entry = config.add_endpoint(&name, &base_url, api_key_env.as_deref())?;
                let path = models::save_config(&config)?;
                writeln!(
                    stdout,
                    "Saved endpoint '{name}' ({}) to {}",
                    entry.base_url,
                    path.display()
                )?;
            }
            ModelsEndpointCommand::List { format } => {
                let config = models::load_config()?;
                match format {
                    ModelsFormat::Text => {
                        writeln!(stdout, "{}", config.format_endpoints_text())?;
                    }
                    ModelsFormat::Json => {
                        let endpoints: std::collections::BTreeMap<&str, &models::EndpointConfig> =
                            config.list_endpoints().into_iter().collect();
                        let payload = serde_json::json!({
                            "schema_version": config.schema_version,
                            "endpoints": endpoints,
                        });
                        let rendered = serde_json::to_string_pretty(&payload).map_err(|err| {
                            models::ModelsError::Config(format!(
                                "failed to serialize endpoints JSON: {err}"
                            ))
                        })?;
                        writeln!(stdout, "{rendered}")?;
                    }
                }
            }
            ModelsEndpointCommand::Remove { name, force } => {
                let mut config = models::load_config()?;
                let removed_bindings = config.remove_endpoint(&name, force)?;
                let path = models::save_config(&config)?;
                if removed_bindings.is_empty() {
                    writeln!(stdout, "Removed endpoint '{name}' from {}", path.display())?;
                } else {
                    writeln!(
                        stdout,
                        "Removed endpoint '{name}' and unbound [{}] from {}",
                        removed_bindings.join(", "),
                        path.display()
                    )?;
                }
            }
        },
        ModelsCommand::Bind(bind_args) => {
            let mut config = models::load_config()?;
            if bind_args.local_onnx {
                let model_dir = bind_args.model_dir.ok_or_else(|| {
                    models::ModelsError::Usage("--local-onnx requires --model-dir PATH".to_string())
                })?;
                let device = bind_args
                    .device
                    .map(Into::into)
                    .unwrap_or(models::OnnxDevice::Cpu);
                config.bind_local_onnx(&bind_args.capability, model_dir, device)?;
            } else if bind_args.endpoint.is_some()
                || bind_args.model.is_some()
                || bind_args.protocol.is_some()
            {
                let endpoint = bind_args.endpoint.ok_or_else(|| {
                    models::ModelsError::Usage(
                        "OpenAI binding requires both --endpoint NAME and --model MODEL_ID"
                            .to_string(),
                    )
                })?;
                let model = bind_args.model.ok_or_else(|| {
                    models::ModelsError::Usage(
                        "OpenAI binding requires both --endpoint NAME and --model MODEL_ID"
                            .to_string(),
                    )
                })?;
                let protocol = bind_args
                    .protocol
                    .map(Into::into)
                    .unwrap_or(models::OpenAiProtocol::Transcriptions);
                config.bind_openai(&bind_args.capability, &endpoint, &model, protocol)?;
            } else {
                return Err(models::ModelsError::Usage(
                    "bind requires either --endpoint NAME --model MODEL_ID or --local-onnx --model-dir PATH"
                        .to_string(),
                ));
            }
            let path = models::save_config(&config)?;
            writeln!(
                stdout,
                "Bound capability '{}' in {}",
                bind_args.capability,
                path.display()
            )?;
        }
        ModelsCommand::Unbind(unbind_args) => {
            let mut config = models::load_config()?;
            config.unbind(&unbind_args.capability)?;
            let path = models::save_config(&config)?;
            writeln!(
                stdout,
                "Unbound capability '{}' in {}",
                unbind_args.capability,
                path.display()
            )?;
        }
        ModelsCommand::Show(show_args) => {
            let config = models::load_config()?;
            match show_args.format {
                ModelsFormat::Text => {
                    writeln!(
                        stdout,
                        "{}",
                        config.format_show_text(show_args.capability.as_deref())
                    )?;
                }
                ModelsFormat::Json => {
                    let payload = config.show_json(show_args.capability.as_deref())?;
                    let rendered = serde_json::to_string_pretty(&payload).map_err(|err| {
                        models::ModelsError::Config(format!(
                            "failed to serialize models show JSON: {err}"
                        ))
                    })?;
                    writeln!(stdout, "{rendered}")?;
                }
            }
        }
    }
    Ok(())
}

/// Validates all `--asr` and `--extract-audio` CLI flags and resolves ASR config before opening any input file.
///
/// Accepts parsed [`ExtractArgs`]. Returns `Ok(None)` when neither `--asr` nor
/// `--extract-audio` (nor any orphan `--asr-*` override flag) was supplied, or
/// `Ok(Some(AudioPipelineConfig))` when all CLI rules and backend resolution
/// succeed. Returns [`models::ModelsError::Usage`] (exit `64`) on orphan
/// `--asr-*` overrides, `--extract-audio` without `--output-dir`, `--extract-audio`
/// combined with `--asr short` or `--asr adaptive`, invalid `--asr-api-key-env`,
/// blank `--asr-language`, or missing/ambiguous ASR backend configuration; returns
/// [`models::ModelsError::Config`] / [`models::ModelsError::Io`] (exit `1`) on
/// corrupt `models.json`.
fn audio_pipeline_config(
    args: &ExtractArgs,
) -> Result<Option<media::AudioPipelineConfig>, models::ModelsError> {
    if args.asr.is_none() {
        let orphan = if args.asr_backend.is_some() {
            Some("--asr-backend")
        } else if args.asr_url.is_some() {
            Some("--asr-url")
        } else if args.asr_model.is_some() {
            Some("--asr-model")
        } else if args.asr_api_key_env.is_some() {
            Some("--asr-api-key-env")
        } else if args.asr_protocol.is_some() {
            Some("--asr-protocol")
        } else if args.asr_model_dir.is_some() {
            Some("--asr-model-dir")
        } else if args.asr_device.is_some() {
            Some("--asr-device")
        } else if args.asr_language.is_some() {
            Some("--asr-language")
        } else {
            None
        };
        if let Some(flag) = orphan {
            return Err(models::ModelsError::Usage(format!("{flag} requires --asr")));
        }
    }

    if args.extract_audio.is_some() {
        if args.output_dir.is_none() {
            return Err(models::ModelsError::Usage(
                "--extract-audio requires --output-dir DIR".to_string(),
            ));
        }
        if matches!(args.asr, Some(AsrModeArg::Short | AsrModeArg::Adaptive)) {
            return Err(models::ModelsError::Usage(
                "--extract-audio can run standalone or with --asr full; it cannot be combined with --asr short or --asr adaptive"
                    .to_string(),
            ));
        }
    }

    if args.asr.is_none() && args.extract_audio.is_none() {
        return Ok(None);
    }

    let asr_language = match args.asr_language.as_deref() {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(models::ModelsError::Usage(
                    "--asr-language must not be empty".to_string(),
                ));
            }
            Some(trimmed.to_owned())
        }
        None => None,
    };

    let asr_config = if args.asr.is_some() {
        if let Some(env_name) = args.asr_api_key_env.as_deref() {
            models::validate_env_var_name(env_name)?;
        }
        let models_cfg = match models::resolve_models_path() {
            Ok(path) => models::load_config_from(&path)?,
            Err(_) => models::ModelsConfig::default(),
        };
        let overrides = models::AsrCliOverrides {
            backend: args.asr_backend.map(Into::into),
            url: args.asr_url.clone(),
            model: args.asr_model.clone(),
            api_key_env: args.asr_api_key_env.clone(),
            protocol: args.asr_protocol.map(Into::into),
            model_dir: args.asr_model_dir.clone(),
            device: args.asr_device.map(Into::into),
            timeout_secs: None,
        };
        Some(models::resolve_asr_config(&overrides, &models_cfg)?)
    } else {
        None
    };

    Ok(Some(media::AudioPipelineConfig {
        asr_mode: args.asr.map(Into::into),
        asr_config,
        asr_language,
        extract_audio: args.extract_audio.map(Into::into),
    }))
}

/// Runs the `--imagegrid` contact-sheet stage into a mutable [`ExtractReport`].
///
/// Accepts the input media path, mutable [`ExtractReport`], validated
/// [`ContactSheetConfig`], optional private `--output-dir`, and per-video
/// worker concurrency cap. Returns `Err(message)` when probing reveals invalid
/// user input (such as exceeding 250 tiles, requiring exit `64`), or
/// `Ok(OperationStatus::Complete | OperationStatus::Failed)` after attaching
/// either the published `ArtifactKind::ContactSheet` + [`ContactSheetReceipt`]
/// or a `DiagnosticStage::Media` error.
fn apply_contact_sheet_stage(
    path: &Path,
    report: &mut ExtractReport,
    config: &ContactSheetConfig,
    output_dir: Option<&Path>,
    concurrency: usize,
) -> Result<OperationStatus, String> {
    use media::contact_sheet::{ImageGridMode, ImageGridPreset};

    if let Err(error) = media::resolve_ffmpeg().and_then(|_| media::resolve_ffprobe()) {
        report.diagnostics.push(Diagnostic {
            stage: DiagnosticStage::Media,
            severity: DiagnosticSeverity::Error,
            code: media::media_error_code(&error).to_owned(),
            message: error.to_string(),
            unit_index: None,
        });
        return Ok(OperationStatus::Failed);
    }

    let sheet = match media::contact_sheet::generate_contact_sheet(
        path,
        config.mode,
        config.preset,
        config.format,
        concurrency,
    ) {
        Ok(sheet) => sheet,
        Err(media::MediaError::InvalidInput(message)) => {
            return Err(message);
        }
        Err(error) => {
            report.diagnostics.push(Diagnostic {
                stage: DiagnosticStage::Media,
                severity: DiagnosticSeverity::Error,
                code: media::media_error_code(&error).to_owned(),
                message: error.to_string(),
                unit_index: None,
            });
            return Ok(OperationStatus::Failed);
        }
    };

    if let Some(title) = sheet.title
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

    let format_name = contact_sheet_extension(config.format);
    let destination = match config.explicit_output.as_ref() {
        Some(explicit) => explicit.clone(),
        None => {
            let directory = output_dir.expect("validated --output-dir when -o is absent");
            directory.join(output::contact_sheet_filename(path, format_name))
        }
    };

    let published_path = match output::publish_single_file(&destination, &sheet.bytes) {
        Ok(published) => published,
        Err(error) => {
            let code = if error.kind() == std::io::ErrorKind::AlreadyExists {
                "contact_sheet_exists"
            } else {
                "contact_sheet_output_failed"
            };
            report.diagnostics.push(Diagnostic {
                stage: DiagnosticStage::Media,
                severity: DiagnosticSeverity::Error,
                code: code.to_owned(),
                message: format!(
                    "publish contact sheet to '{}': {error}",
                    destination.display()
                ),
                unit_index: None,
            });
            return Ok(OperationStatus::Failed);
        }
    };

    report.metadata.artifacts.push(Artifact {
        kind: ArtifactKind::ContactSheet,
        path: published_path.clone(),
        source: FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
        width: Some(sheet.width_px),
        height: Some(sheet.height_px),
    });

    let (mode_name, every_seconds) = match config.mode {
        ImageGridMode::Default => ("default", None),
        ImageGridMode::Adaptive => ("adaptive", None),
        ImageGridMode::Custom { every_seconds } => ("custom", Some(every_seconds)),
    };
    let preset_name = match config.preset {
        ImageGridPreset::Quality => "quality",
        ImageGridPreset::Performance => "performance",
    };

    report
        .media
        .get_or_insert_with(MediaReceipt::default)
        .contact_sheet = Some(ContactSheetReceipt {
        output: published_path,
        duration_seconds: sheet.duration_seconds,
        columns: sheet.columns,
        rows: sheet.rows,
        tiles: sheet.tiles,
        mode: mode_name.to_owned(),
        preset: preset_name.to_owned(),
        format: format_name.to_owned(),
        width_px: sheet.width_px,
        height_px: sheet.height_px,
        cell_width_px: sheet.cell_width_px,
        cell_height_px: sheet.cell_height_px,
        seek_seconds: sheet.seek_seconds,
        reused_tiles: sheet.reused_tiles.len() as u32,
        linear_seek_tiles: sheet.linear_seek_tiles.len() as u32,
        every_seconds,
    });

    Ok(OperationStatus::Complete)
}

/// Executes requested media stages (`--imagegrid`, `--asr`, `--extract-audio`) for one input file.
///
/// Accepts the input media path, its base [`MetadataReport`], optional
/// [`ContactSheetConfig`], optional [`media::AudioPipelineConfig`], optional
/// validated private `--output-dir`, and per-video contact-sheet worker
/// concurrency. Enforces local regular non-symlink input admission before
/// spawning any `ffprobe` or `ffmpeg` subprocess by inspecting
/// `metadata_report.metadata.diagnostics` for `DiagnosticStage::Input` errors
/// and verifying [`media::ffmpeg_local_input_arg`]; rejected inputs write zero
/// media artifacts and return `OperationStatus::Failed` with
/// `Completeness::Unavailable`, `OmissionStatus::PartialFailure`, `media: None`,
/// and a `DiagnosticStage::Input` error diagnostic. Returns
/// [`ContactSheetRun::InvalidInput`] on post-probe invalid parameters (exit
/// `64`), or [`ContactSheetRun::Report`] with reconciled v3 status (`Complete`
/// exit `0`, `Partial` exit `2`, or `Failed` exit `1`), honest v3 `selection`
/// omission/completeness projection, all completed artifacts preserved, and
/// `report.media` normalized to `None` whenever no media sub-receipts succeeded.
fn execute_media_extract(
    path: &Path,
    metadata_report: MetadataReport,
    contact_sheet: Option<&ContactSheetConfig>,
    audio_pipeline: Option<&media::AudioPipelineConfig>,
    output_dir: Option<&Path>,
    concurrency: usize,
) -> ContactSheetRun {
    let mut report = ExtractReport {
        schema_version: LEGACY_REPORT_SCHEMA_VERSION,
        kind: ReportKind::Extract,
        status: OperationStatus::Complete,
        diagnostics: Vec::new(),
        metadata: metadata_report.metadata,
        content: ExtractedContent::default(),
        provenance: Vec::new(),
        completeness: Completeness::Exact,
        selection: SelectionReport {
            requested: NativeSelection::All,
            unit: NativeUnit::Unknown,
            available: QualifiedCount::default(),
            returned_indices: Vec::new(),
            omitted: OmissionStatus::None,
            completeness: Completeness::Exact,
        },
        media: None,
    };

    let metadata_input_error = report
        .metadata
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.stage == DiagnosticStage::Input
                && diagnostic.severity == DiagnosticSeverity::Error
        })
        .cloned();
    let local_input_check = media::ffmpeg_local_input_arg(path);
    if metadata_input_error.is_some() || local_input_check.is_err() {
        if let Some(input_error) = metadata_input_error {
            report.diagnostics.push(input_error);
        } else if let Err(error) = local_input_check {
            report.diagnostics.push(Diagnostic {
                stage: DiagnosticStage::Input,
                severity: DiagnosticSeverity::Error,
                code: "input_media_rejected".to_owned(),
                message: error.to_string(),
                unit_index: None,
            });
        }
        report.status = OperationStatus::Failed;
        report.completeness = Completeness::Unavailable;
        report.selection.omitted = OmissionStatus::PartialFailure;
        report.selection.completeness = Completeness::Unavailable;
        report.media = None;
        return ContactSheetRun::Report(Box::new(report));
    }

    let cs_status = if let Some(cs_cfg) = contact_sheet {
        match apply_contact_sheet_stage(path, &mut report, cs_cfg, output_dir, concurrency) {
            Ok(status) => Some(status),
            Err(message) => return ContactSheetRun::InvalidInput(message),
        }
    } else {
        None
    };

    let audio_status = audio_pipeline.map(|audio_cfg| {
        media::run_asr_and_audio_extract(path, &mut report, audio_cfg, output_dir)
    });

    match (cs_status, audio_status) {
        (Some(OperationStatus::Complete), None) => {
            report.status = OperationStatus::Complete;
            report.completeness = Completeness::Exact;
        }
        (Some(OperationStatus::Failed), None) | (None, Some(OperationStatus::Failed)) => {
            report.status = OperationStatus::Failed;
            report.completeness = Completeness::Unavailable;
            report.selection.omitted = OmissionStatus::PartialFailure;
            report.selection.completeness = Completeness::Unavailable;
        }
        (None, Some(_)) => {
            // `run_asr_and_audio_extract` already set status, completeness, and selection.
        }
        (Some(cs), Some(audio)) => match (cs, audio) {
            (OperationStatus::Complete, OperationStatus::Complete) => {
                // Keep audio stage's selection/completeness (Exact for full, Partial for short/adaptive).
                report.status = OperationStatus::Complete;
            }
            (OperationStatus::Failed, OperationStatus::Failed) => {
                report.status = OperationStatus::Failed;
                report.completeness = Completeness::Unavailable;
                report.selection.omitted = OmissionStatus::PartialFailure;
                report.selection.completeness = Completeness::Unavailable;
            }
            _ => {
                report.status = OperationStatus::Partial;
                report.completeness = Completeness::Partial;
                report.selection.omitted = OmissionStatus::PartialFailure;
                report.selection.completeness = Completeness::Partial;
            }
        },
        _ => {}
    }

    media::prune_empty_media_receipt(&mut report);

    ContactSheetRun::Report(Box::new(report))
}

/// Runs media extraction (`--imagegrid`, `--asr`, `--extract-audio`) over stdin JSONL requests.
///
/// Accepts optional validated [`ContactSheetConfig`], optional validated
/// [`media::AudioPipelineConfig`], optional already-initialized private
/// `--output-dir`, positive job count, `full_hash` override, `verbose`
/// provenance flag, `include_markers` (`!no_markers`), and `include_frontmatter`
/// (`!no_frontmatter`). Returns the aggregate [`batch::BatchOutcome`] after
/// ordered JSONL output and worker join. Per-item `MediaError::InvalidInput`
/// becomes an invalid batch completion (exit `64` precedence); runtime media
/// and publication errors remain structured per-item `Partial` or `Failed`
/// reports.
#[allow(clippy::too_many_arguments)]
fn run_media_batch(
    contact_sheet: Option<&ContactSheetConfig>,
    audio_pipeline: Option<&media::AudioPipelineConfig>,
    output_dir: Option<&Path>,
    jobs: usize,
    full_hash: bool,
    verbose: bool,
    include_markers: bool,
    include_frontmatter: bool,
) -> Result<batch::BatchOutcome, Box<dyn std::error::Error>> {
    let per_file_concurrency = contact_sheet_concurrency(true, jobs);
    let output = std::io::stdout();
    let mut output = output.lock();
    batch::run(
        jobs,
        produce_jsonl,
        |budget, _cancellation| {
            let mut inspector = Inspector::with_shared_budget(budget).with_full_hash(full_hash);
            let cs_config = contact_sheet.cloned();
            let audio_config = audio_pipeline.cloned();
            let directory = output_dir.map(Path::to_path_buf);
            move |request| {
                let sequence = request.sequence;
                let metadata_report = inspector.inspect(&request.path, &InspectOptions::default());
                match execute_media_extract(
                    &request.path,
                    metadata_report,
                    cs_config.as_ref(),
                    audio_config.as_ref(),
                    directory.as_deref(),
                    per_file_concurrency,
                ) {
                    ContactSheetRun::InvalidInput(message) => {
                        batch::BatchCompletion::invalid(request.id, message)
                    }
                    ContactSheetRun::Report(mut report) => {
                        if let Some(dir) = directory.as_deref() {
                            crate::output::persist_extract(
                                &mut report,
                                dir,
                                sequence as usize,
                                verbose,
                                include_markers,
                                include_frontmatter,
                            );
                        }
                        let status = report.extraction_status();
                        batch::BatchCompletion::report(
                            request,
                            status,
                            serde_json::to_value(report.to_v3(verbose)),
                        )
                    }
                }
            }
        },
        &mut output,
    )
    .map_err(std::io::Error::other)
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that the defaults route remains available under both binary aliases.
    #[test]
    fn defaults_command_is_available() {
        for name in ["glassbox", "litx"] {
            let cli = Cli::try_parse_from([name, "defaults"]).unwrap();
            assert!(matches!(cli.command, Commands::Defaults));
        }
    }

    /// Checks that upstream defaults retain their public JSON representation.
    #[test]
    fn upstream_defaults_serialize_as_json() {
        let config = serde_json::to_value(LiteParseConfig::default()).unwrap();
        assert!(config.is_object());
        assert_eq!(config["ocr_language"], "eng");
    }

    /// Checks that inspect exposes metadata controls and not extraction controls.
    #[test]
    fn inspect_rejects_legacy_content_flags_at_clap_boundary() {
        let error =
            Cli::try_parse_from(["glassbox", "inspect", "a.pdf", "--max-pages", "2"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(Cli::try_parse_from(["glassbox", "inspect", "a.pdf", "--pretty"]).is_ok());
    }

    /// Checks bare, count, range, and deprecated selector semantics.
    #[test]
    fn extraction_selectors_default_to_full_and_validate_first_n() {
        let bare = Cli::try_parse_from(["glassbox", "extract", "a.txt", "--pages"]).unwrap();
        let Commands::Extract(args) = bare.command else {
            panic!("extract route expected")
        };
        assert_eq!(
            extraction_options(&args).unwrap().selection,
            NativeSelection::FirstN { count: 5 }
        );
        let full = Cli::try_parse_from(["glassbox", "extract", "a.txt"]).unwrap();
        let Commands::Extract(args) = full.command else {
            panic!("extract route expected")
        };
        assert_eq!(
            extraction_options(&args).unwrap().selection,
            NativeSelection::All
        );
        let ranged =
            Cli::try_parse_from(["glassbox", "extract", "a.txt", "--pages", "3-7,10,3-4"]).unwrap();
        let Commands::Extract(args) = ranged.command else {
            panic!("extract route expected")
        };
        assert_eq!(
            extraction_options(&args).unwrap().selection,
            NativeSelection::Ranges {
                ranges: vec![
                    glassbox_inspect::reports::NativeRange { start: 3, end: 7 },
                    glassbox_inspect::reports::NativeRange { start: 10, end: 10 }
                ]
            }
        );
    }

    /// Checks `--jobs` values outside 1..=32 are rejected at the Clap boundary.
    ///
    /// Accepts no parameters. Returns unit after parsing inspect and extract
    /// routes with zero, oversize, and overflowing values. An accepted
    /// out-of-range value or a rejected maximum value panics.
    #[test]
    fn jobs_bounds_are_enforced_at_clap_boundary() {
        for command in ["inspect", "extract"] {
            for invalid in ["0", "33", "999999999999999999999999999999999999"] {
                let error = Cli::try_parse_from(["glassbox", command, "a.txt", "--jobs", invalid])
                    .unwrap_err();
                assert_eq!(
                    error.kind(),
                    clap::error::ErrorKind::ValueValidation,
                    "{command} {invalid}"
                );
            }
            let bounded =
                Cli::try_parse_from(["glassbox", command, "a.txt", "--jobs", "32"]).unwrap();
            match bounded.command {
                Commands::Inspect(args) => assert_eq!(args.jobs, MAX_JOBS),
                Commands::Extract(args) => assert_eq!(args.jobs, MAX_JOBS),
                _ => panic!("{command} route expected"),
            }
        }
    }

    /// Checks the OCR language default, spellings, and option flow.
    ///
    /// Accepts no parameters. Returns unit after parsing extract with the
    /// default, alias, canonical, and combined spellings, then building
    /// extraction options. A lost default, a dropped language, OCR disabled
    /// while enabled, or an inspect acceptance panics.
    #[test]
    fn ocr_language_defaults_to_eng_and_flows_into_extraction_options() {
        let default = Cli::try_parse_from(["glassbox", "extract", "a.txt"]).unwrap();
        let Commands::Extract(args) = default.command else {
            panic!("extract route expected")
        };
        let options = extraction_options(&args).unwrap();
        assert!(options.ocr.enabled);
        assert_eq!(options.ocr.language, "eng");
        for language in ["en", "eng", "deu+fra"] {
            let requested =
                Cli::try_parse_from(["glassbox", "extract", "a.txt", "--ocr-language", language])
                    .unwrap();
            let Commands::Extract(args) = requested.command else {
                panic!("extract route expected")
            };
            assert_eq!(
                extraction_options(&args).unwrap().ocr.language,
                language,
                "{language}"
            );
        }
        let disabled = Cli::try_parse_from([
            "glassbox",
            "extract",
            "a.txt",
            "--no-ocr",
            "--ocr-language",
            "en",
        ])
        .unwrap();
        let Commands::Extract(args) = disabled.command else {
            panic!("extract route expected")
        };
        let options = extraction_options(&args).unwrap();
        assert!(!options.ocr.enabled);
        assert_eq!(options.ocr.language, "en");
        let error = Cli::try_parse_from(["glassbox", "inspect", "a.pdf", "--ocr-language", "eng"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    /// Checks `--full-hash`, `--verbose`, and `--no-markers` parsing boundaries.
    ///
    /// Accepts no parameters. Returns unit after checking defaults and explicit
    /// flags on inspect and extract, plus Clap rejection of `--no-markers` on
    /// inspect. Any drifted default or missing flag panics.
    #[test]
    fn v3_cli_flags_parse_with_expected_defaults_and_boundaries() {
        let inspect_default = Cli::try_parse_from(["glassbox", "inspect", "a.pdf"]).unwrap();
        let Commands::Inspect(args) = inspect_default.command else {
            panic!("inspect route expected")
        };
        assert!(!args.full_hash);
        assert!(!args.verbose);

        let inspect_flags =
            Cli::try_parse_from(["glassbox", "inspect", "a.pdf", "--full-hash", "--verbose"])
                .unwrap();
        let Commands::Inspect(args) = inspect_flags.command else {
            panic!("inspect route expected")
        };
        assert!(args.full_hash);
        assert!(args.verbose);

        let inspect_no_markers =
            Cli::try_parse_from(["glassbox", "inspect", "a.pdf", "--no-markers"]).unwrap_err();
        assert_eq!(
            inspect_no_markers.kind(),
            clap::error::ErrorKind::UnknownArgument
        );

        let extract_default = Cli::try_parse_from(["glassbox", "extract", "a.pdf"]).unwrap();
        let Commands::Extract(args) = extract_default.command else {
            panic!("extract route expected")
        };
        assert!(!args.full_hash);
        assert!(!args.verbose);
        assert!(!args.no_markers);

        let extract_flags = Cli::try_parse_from([
            "glassbox",
            "extract",
            "a.pdf",
            "--full-hash",
            "--verbose",
            "--no-markers",
            "--format",
            "json",
        ])
        .unwrap();
        let Commands::Extract(args) = extract_flags.command else {
            panic!("extract route expected")
        };
        assert!(args.full_hash);
        assert!(args.verbose);
        assert!(args.no_markers);
        assert_eq!(args.format, Format::Json);
    }

    /// Checks that inspect's flag precheck stops at `--` while still rejecting prior content flags.
    ///
    /// Accepts no parameters. Returns unit after checking positional flag-like
    /// filenames after `--`, pre-terminator rejections, and Clap parsing of
    /// `glassbox inspect -- --no-markers`. Any missed rejection or blocked
    /// post-terminator filename panics.
    #[test]
    fn inspect_precheck_stops_at_option_terminator_and_still_rejects_prior_flags() {
        let to_os = |items: &[&str]| -> Vec<std::ffi::OsString> {
            items.iter().map(std::ffi::OsString::from).collect()
        };

        assert_eq!(
            find_disallowed_inspect_flag(&to_os(&["glassbox", "inspect", "--", "--no-markers"])),
            None
        );
        assert_eq!(
            find_disallowed_inspect_flag(&to_os(&[
                "glassbox", "inspect", "--pretty", "--", "--no-ocr"
            ])),
            None
        );
        assert_eq!(
            find_disallowed_inspect_flag(&to_os(&[
                "glassbox",
                "inspect",
                "--no-markers",
                "file.txt"
            ])),
            Some("--no-markers".to_owned())
        );
        assert_eq!(
            find_disallowed_inspect_flag(&to_os(&[
                "glassbox",
                "inspect",
                "--max-pages=5",
                "--",
                "--no-markers"
            ])),
            Some("--max-pages=5".to_owned())
        );

        let parsed = Cli::try_parse_from(["glassbox", "inspect", "--", "--no-markers"]).unwrap();
        let Commands::Inspect(args) = parsed.command else {
            panic!("inspect route expected")
        };
        assert_eq!(args.path, Some(PathBuf::from("--no-markers")));
    }

    /// Buffered test reader yielding a prefix before failing with one I/O error.
    struct FailingDrainReader {
        prefix: Vec<u8>,
        offset: usize,
        error_kind: std::io::ErrorKind,
    }

    impl Read for FailingDrainReader {
        /// Reads remaining prefix bytes, then returns the configured I/O error.
        ///
        /// Accepts a destination buffer. Returns bytes copied from the prefix,
        /// or the configured error once the prefix is exhausted.
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.offset < self.prefix.len() {
                let remaining = &self.prefix[self.offset..];
                let count = remaining.len().min(buffer.len());
                buffer[..count].copy_from_slice(&remaining[..count]);
                self.offset += count;
                return Ok(count);
            }
            Err(std::io::Error::new(self.error_kind, "injected drain error"))
        }
    }

    /// Buffered reader double returning a configured error directly from `skip_until`.
    struct SkipUntilErrorReader {
        prefix: Vec<u8>,
        offset: usize,
        skip_error: std::io::ErrorKind,
    }

    impl Read for SkipUntilErrorReader {
        /// Reads prefix bytes into the caller buffer.
        ///
        /// Accepts a destination buffer. Returns bytes copied or zero at prefix
        /// end; no OS I/O occurs.
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let remaining = &self.prefix[self.offset..];
            let count = remaining.len().min(buffer.len());
            buffer[..count].copy_from_slice(&remaining[..count]);
            self.offset += count;
            Ok(count)
        }
    }

    impl BufRead for SkipUntilErrorReader {
        /// Exposes unread prefix bytes without performing OS I/O.
        ///
        /// Accepts no parameters. Returns the remaining slice of the prefix.
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Ok(&self.prefix[self.offset..])
        }

        /// Advances the unread prefix offset by `amount` bytes.
        ///
        /// Accepts a consumed byte count. Returns unit after advancing safely.
        fn consume(&mut self, amount: usize) {
            self.offset = (self.offset + amount).min(self.prefix.len());
        }

        /// Simulates an error raised while draining an oversized line to `\n`.
        ///
        /// Accepts a delimiter byte. Always returns the configured I/O error.
        fn skip_until(&mut self, _byte: u8) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                self.skip_error,
                "injected skip_until error",
            ))
        }
    }

    /// Verifies oversized-line draining treats `Interrupted` and `BrokenPipe` as clean stop and propagates other errors.
    ///
    /// Accepts no parameters. Returns unit after exercising `read_capped_line`
    /// with a `>65536`-byte line ending in `BrokenPipe`, `Interrupted`, and
    /// `PermissionDenied` during `skip_until`. Any unexpected error handling or
    /// dropped error panics.
    #[test]
    fn read_capped_line_handles_interrupted_and_broken_pipe_during_oversized_drain() {
        let mut line = Vec::new();

        let mut broken_pipe = std::io::BufReader::new(FailingDrainReader {
            prefix: vec![b'a'; 65_537],
            offset: 0,
            error_kind: std::io::ErrorKind::BrokenPipe,
        });
        assert!(!read_capped_line(&mut broken_pipe, &mut line).unwrap());

        let mut interrupted = SkipUntilErrorReader {
            prefix: vec![b'b'; 65_537],
            offset: 0,
            skip_error: std::io::ErrorKind::Interrupted,
        };
        assert!(!read_capped_line(&mut interrupted, &mut line).unwrap());

        let mut permission_denied = std::io::BufReader::new(FailingDrainReader {
            prefix: vec![b'c'; 65_537],
            offset: 0,
            error_kind: std::io::ErrorKind::PermissionDenied,
        });
        let error = read_capped_line(&mut permission_denied, &mut line).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    /// Parses an `extract` CLI invocation and runs `contact_sheet_config`.
    ///
    /// Accepts the full argv slice starting with `"glassbox"`. Returns the
    /// `contact_sheet_config` result after Clap succeeds; panics if Clap itself
    /// rejects the argv.
    fn parse_contact_sheet(argv: &[&str]) -> Result<Option<ContactSheetConfig>, String> {
        let cli = Cli::try_parse_from(argv).expect("expected Clap to parse argv");
        let Commands::Extract(args) = cli.command else {
            panic!("extract route expected");
        };
        contact_sheet_config(&args)
    }

    /// Verifies valid contact-sheet CLI flag combinations resolve expected modes, presets, and formats.
    ///
    /// Accepts no parameters. Returns unit after checking non-media passthrough,
    /// bare/default/adaptive/custom modes, `--quality`/`--performance`, `-o`
    /// case-insensitive extensions, and `--output-dir` defaults. Panics on mismatch.
    #[test]
    fn contact_sheet_config_accepts_valid_combinations() {
        use media::contact_sheet::{ImageGridFormat, ImageGridMode, ImageGridPreset};

        assert_eq!(
            parse_contact_sheet(&["glassbox", "extract", "doc.pdf"]).unwrap(),
            None
        );

        let bare = parse_contact_sheet(&[
            "glassbox",
            "extract",
            "clip.mp4",
            "--imagegrid",
            "-o",
            "sheet.JPG",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(bare.mode, ImageGridMode::Default);
        assert_eq!(bare.preset, ImageGridPreset::Quality);
        assert_eq!(bare.format, ImageGridFormat::Jpg);
        assert_eq!(bare.explicit_output, Some(PathBuf::from("sheet.JPG")));

        let bare_custom = parse_contact_sheet(&[
            "glassbox",
            "extract",
            "clip.mp4",
            "--imagegrid",
            "--every-seconds",
            "2.5",
            "-o",
            "sheet.JPEG",
            "--imagegrid-format",
            "jpg",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            bare_custom.mode,
            ImageGridMode::Custom { every_seconds: 2.5 }
        );
        assert_eq!(bare_custom.format, ImageGridFormat::Jpg);

        let adaptive_dir = parse_contact_sheet(&[
            "glassbox",
            "extract",
            "clip.mp4",
            "--imagegrid",
            "adaptive",
            "--performance",
            "--imagegrid-format",
            "png",
            "--output-dir",
            "out",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(adaptive_dir.mode, ImageGridMode::Adaptive);
        assert_eq!(adaptive_dir.preset, ImageGridPreset::Performance);
        assert_eq!(adaptive_dir.format, ImageGridFormat::Png);
        assert_eq!(adaptive_dir.explicit_output, None);

        let combined_dest = parse_contact_sheet(&[
            "glassbox",
            "extract",
            "clip.mp4",
            "--imagegrid",
            "custom",
            "--every-seconds",
            "10",
            "-o",
            "preview.png",
            "--output-dir",
            "out",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            combined_dest.mode,
            ImageGridMode::Custom {
                every_seconds: 10.0
            }
        );
        assert_eq!(combined_dest.format, ImageGridFormat::Png);
        assert_eq!(
            combined_dest.explicit_output,
            Some(PathBuf::from("preview.png"))
        );
    }

    /// Verifies all invalid contact-sheet CLI combinations fail validation before file I/O.
    ///
    /// Accepts no parameters. Returns unit after checking every exit-64 rule
    /// (missing destination, orphan flags, `-o` with `--batch`, invalid `-o`
    /// extensions, format mismatch, invalid `--every-seconds` pairings/values,
    /// and `--quality` vs `--performance`). Panics if any invalid invocation passes.
    #[test]
    fn contact_sheet_config_rejects_invalid_combinations() {
        let invalid_cases: &[&[&str]] = &[
            &["glassbox", "extract", "clip.mp4", "--imagegrid"],
            &["glassbox", "extract", "clip.mp4", "-o", "out.jpg"],
            &[
                "glassbox",
                "extract",
                "--batch",
                "--imagegrid",
                "-o",
                "out.jpg",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "-o",
                "out.webp",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "-o",
                "out",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "-o",
                "dir/out.jpg/",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "--imagegrid-format",
                "png",
                "-o",
                "out.jpg",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--every-seconds",
                "5",
                "--output-dir",
                "out",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "default",
                "--every-seconds",
                "5",
                "-o",
                "out.jpg",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "adaptive",
                "--every-seconds",
                "5",
                "-o",
                "out.jpg",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "custom",
                "-o",
                "out.jpg",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "custom",
                "--every-seconds",
                "0",
                "-o",
                "out.jpg",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "custom",
                "--every-seconds",
                "-2.5",
                "-o",
                "out.jpg",
            ],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid",
                "custom",
                "--every-seconds",
                "NaN",
                "-o",
                "out.jpg",
            ],
            &["glassbox", "extract", "clip.mp4", "--quality"],
            &["glassbox", "extract", "clip.mp4", "--performance"],
            &[
                "glassbox",
                "extract",
                "clip.mp4",
                "--imagegrid-format",
                "png",
            ],
        ];
        for argv in invalid_cases {
            let err = parse_contact_sheet(argv)
                .expect_err(&format!("expected validation failure for {argv:?}"));
            assert!(!err.is_empty(), "{argv:?}");
        }

        let preset_conflict = Cli::try_parse_from([
            "glassbox",
            "extract",
            "clip.mp4",
            "--imagegrid",
            "--quality",
            "--performance",
            "-o",
            "out.jpg",
        ])
        .unwrap_err();
        assert_eq!(
            preset_conflict.kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
    }

    /// Verifies contact-sheet worker concurrency caps at 4 single-file and 1 in multi-job batch.
    ///
    /// Accepts no parameters. Returns unit after checking single-file,
    /// single-job batch, and multi-job batch concurrency values. Panics if
    /// bounds are violated.
    #[test]
    fn contact_sheet_concurrency_respects_single_and_batch_caps() {
        let single = contact_sheet_concurrency(false, 1);
        assert!((1..=4).contains(&single));
        assert_eq!(contact_sheet_concurrency(true, 1), single);
        assert_eq!(contact_sheet_concurrency(true, 2), 1);
        assert_eq!(contact_sheet_concurrency(true, 8), 1);
    }

    /// Verifies `find_disallowed_inspect_flag` rejects all 16 media flags before `--` and allows literal filenames after `--`.
    ///
    /// Accepts no parameters. Returns unit after testing every planned media
    /// flag in bare and `=value` forms as well as post-`--` pass-through.
    /// Panics if any flag is missed or any literal filename is blocked.
    #[test]
    fn inspect_precheck_rejects_all_media_flags_before_terminator() {
        const MEDIA_FLAGS: &[&str] = &[
            "--imagegrid",
            "--every-seconds",
            "--quality",
            "--performance",
            "--imagegrid-format",
            "--asr",
            "--asr-backend",
            "--asr-url",
            "--asr-model",
            "--asr-api-key-env",
            "--asr-protocol",
            "--asr-model-dir",
            "--asr-device",
            "--asr-language",
            "--extract-audio",
            "--no-frontmatter",
        ];
        for flag in MEDIA_FLAGS {
            let bare: Vec<std::ffi::OsString> = ["glassbox", "inspect", flag, "clip.mp4"]
                .iter()
                .map(std::ffi::OsString::from)
                .collect();
            assert_eq!(
                find_disallowed_inspect_flag(&bare),
                Some((*flag).to_owned()),
                "bare {flag}"
            );

            let eq = format!("{flag}=val");
            let with_eq: Vec<std::ffi::OsString> = ["glassbox", "inspect", eq.as_str(), "clip.mp4"]
                .iter()
                .map(std::ffi::OsString::from)
                .collect();
            assert_eq!(
                find_disallowed_inspect_flag(&with_eq),
                Some(eq),
                "equals {flag}"
            );

            let after_terminator: Vec<std::ffi::OsString> = ["glassbox", "inspect", "--", flag]
                .iter()
                .map(std::ffi::OsString::from)
                .collect();
            assert_eq!(
                find_disallowed_inspect_flag(&after_terminator),
                None,
                "post-terminator {flag}"
            );
        }
    }

    /// Verifies Whisper ONNX CLI flags and combined `--imagegrid` + `--asr` + `--extract-audio` guards.
    ///
    /// Accepts no parameters. Returns unit after checking that combined single-file
    /// and `--batch` invocations resolve Whisper ONNX settings cleanly while invalid
    /// combinations (`-o` with `--batch`, `--extract-audio` with `--asr short|adaptive`)
    /// fail with exit-64 usage errors. Panics on mismatch.
    #[test]
    fn combined_media_and_whisper_onnx_cli_validation_enforces_contracts() {
        let parsed = Cli::try_parse_from([
            "glassbox",
            "extract",
            "clip.mp4",
            "--imagegrid",
            "adaptive",
            "-o",
            "sheet.png",
            "--asr",
            "full",
            "--asr-backend",
            "whisper-onnx",
            "--asr-model-dir",
            "/opt/models/whisper-tiny",
            "--asr-device",
            "cpu",
            "--extract-audio",
            "wav",
            "--output-dir",
            "out",
        ])
        .expect("combined CLI args should parse");
        let Commands::Extract(args) = parsed.command else {
            panic!("extract route expected");
        };
        let cs = contact_sheet_config(&args)
            .expect("contact sheet config valid")
            .expect("contact sheet enabled");
        assert_eq!(cs.explicit_output, Some(PathBuf::from("sheet.png")));
        let audio = audio_pipeline_config(&args)
            .expect("audio pipeline config valid")
            .expect("audio pipeline enabled");
        assert_eq!(audio.asr_mode, Some(media::asr::AsrMode::Full));
        assert_eq!(
            audio.extract_audio,
            Some(media::asr::AudioExtractFormat::Wav)
        );
        assert_eq!(
            audio.asr_config,
            Some(models::ResolvedAsrConfig::WhisperOnnx {
                model_dir: PathBuf::from("/opt/models/whisper-tiny"),
                device: models::OnnxDevice::Cpu,
                source: models::ConfigSource::Cli,
            })
        );

        // Batch combined mode with --output-dir is accepted; -o with --batch is rejected.
        let batch_ok = Cli::try_parse_from([
            "glassbox",
            "extract",
            "--batch",
            "--imagegrid",
            "adaptive",
            "--asr",
            "full",
            "--asr-model-dir",
            "/opt/models/whisper-tiny",
            "--extract-audio",
            "wav",
            "--output-dir",
            "out",
        ])
        .expect("batch combined args should parse");
        let Commands::Extract(batch_args) = batch_ok.command else {
            panic!("extract route expected");
        };
        assert!(contact_sheet_config(&batch_args).unwrap().is_some());
        assert!(audio_pipeline_config(&batch_args).unwrap().is_some());

        // --extract-audio combined with --asr short or adaptive must fail with Usage (exit 64).
        for mode in ["short", "adaptive"] {
            let bad = Cli::try_parse_from([
                "glassbox",
                "extract",
                "clip.mp4",
                "--asr",
                mode,
                "--asr-model-dir",
                "/opt/models/whisper-tiny",
                "--extract-audio",
                "wav",
                "--output-dir",
                "out",
            ])
            .expect("clap parses before semantic guard");
            let Commands::Extract(bad_args) = bad.command else {
                panic!("extract route expected");
            };
            let err = audio_pipeline_config(&bad_args)
                .expect_err("extract-audio with short/adaptive must fail");
            assert_eq!(err.exit_code(), 64, "mode={mode}: {err}");
        }
    }

    /// Verifies `prune_empty_media_receipt` clears an empty `MediaReceipt` so v3 JSON omits `"media"`.
    ///
    /// Accepts no parameters. Returns unit after checking that an empty `Some(MediaReceipt::default())`
    /// is normalized to `None` and omitted from serialized schema v3 JSON. Panics on mismatch.
    #[test]
    fn empty_media_receipt_is_pruned_from_v3_json() {
        let mut report = ExtractReport {
            status: OperationStatus::Failed,
            media: Some(MediaReceipt::default()),
            ..ExtractReport::default()
        };
        media::prune_empty_media_receipt(&mut report);
        assert!(report.media.is_none());
        let v3_json = serde_json::to_value(report.to_v3(false)).expect("serialize v3");
        assert!(
            v3_json.get("media").is_none(),
            "v3 JSON must never emit empty media object: {v3_json}"
        );
    }

    /// Verifies `execute_media_extract` rejects metadata input errors and invalid local paths before running media stages.
    ///
    /// Accepts no parameters. Returns unit after verifying both a metadata
    /// `DiagnosticStage::Input` error and a protocol-like path rejected by
    /// [`media::ffmpeg_local_input_arg`] return `OperationStatus::Failed`,
    /// `Completeness::Unavailable`, `OmissionStatus::PartialFailure`,
    /// `selection.completeness == Completeness::Unavailable`, `media == None`,
    /// and a `DiagnosticStage::Input` error diagnostic without invoking FFmpeg.
    #[test]
    fn execute_media_extract_rejects_unadmitted_inputs_before_media_stages() {
        let cs_cfg = ContactSheetConfig {
            mode: media::contact_sheet::ImageGridMode::Adaptive,
            preset: media::contact_sheet::ImageGridPreset::Performance,
            format: media::contact_sheet::ImageGridFormat::Jpg,
            explicit_output: Some(PathBuf::from("/nonexistent/must-not-write.jpg")),
        };

        // 1. Metadata stage already flagged a `DiagnosticStage::Input` error.
        let mut meta_rejected = MetadataReport::default();
        meta_rejected.metadata.status = OperationStatus::Failed;
        meta_rejected.metadata.diagnostics.push(Diagnostic {
            stage: DiagnosticStage::Input,
            severity: DiagnosticSeverity::Error,
            code: "input_symlink_rejected".to_owned(),
            message: "symbolic link rejected".to_owned(),
            unit_index: None,
        });
        let ContactSheetRun::Report(report1) = execute_media_extract(
            Path::new("link.mp4"),
            meta_rejected,
            Some(&cs_cfg),
            None,
            None,
            1,
        ) else {
            panic!("expected structured Failed report for metadata input rejection");
        };
        assert_eq!(report1.status, OperationStatus::Failed);
        assert_eq!(report1.completeness, Completeness::Unavailable);
        assert_eq!(report1.selection.omitted, OmissionStatus::PartialFailure);
        assert_eq!(report1.selection.completeness, Completeness::Unavailable);
        assert!(report1.media.is_none());
        assert!(report1.diagnostics.iter().any(|diag| {
            diag.stage == DiagnosticStage::Input
                && diag.severity == DiagnosticSeverity::Error
                && diag.code == "input_symlink_rejected"
        }));

        // 2. Clean metadata but protocol-like path caught by defense-in-depth `ffmpeg_local_input_arg`.
        let clean_meta = MetadataReport::default();
        let ContactSheetRun::Report(report2) = execute_media_extract(
            Path::new("http://127.0.0.1:9/test.mp4"),
            clean_meta,
            Some(&cs_cfg),
            None,
            None,
            1,
        ) else {
            panic!("expected structured Failed report for protocol rejection");
        };
        assert_eq!(report2.status, OperationStatus::Failed);
        assert_eq!(report2.completeness, Completeness::Unavailable);
        assert_eq!(report2.selection.omitted, OmissionStatus::PartialFailure);
        assert_eq!(report2.selection.completeness, Completeness::Unavailable);
        assert!(report2.media.is_none());
        assert!(report2.diagnostics.iter().any(|diag| {
            diag.stage == DiagnosticStage::Input
                && diag.severity == DiagnosticSeverity::Error
                && diag.code == "input_media_rejected"
        }));
    }
}

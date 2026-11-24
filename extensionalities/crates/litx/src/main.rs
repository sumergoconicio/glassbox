// Module: glassbox-cli unified executable.
// File: main.rs.
// Responsibility: Route upstream parsing, schema v3 metadata-only inspection, and canonical native-unit extraction.
use clap::{Args, Parser, Subcommand, ValueEnum};
use glassbox_inspect::hashing::HashPolicy;
use glassbox_inspect::reports::{
    ExtractOptions, ExtractReport, InspectOptions, MetadataReport, NativeSelection,
    OperationStatus, SessionBudget, ThumbnailOptions,
};
use glassbox_inspect::{Inspector, extract_with_policy, inspect_with_policy};
use liteparse::LiteParseConfig;
use serde_json::Value;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

mod batch;
mod inventory;
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
    Extract(ExtractArgs),
    /// Print the upstream library's default configuration as JSON.
    Defaults,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Json,
    Text,
    Markdown,
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
}

/// Finds the first extraction-only content flag passed to `inspect` before `--`.
///
/// Accepts the process argument vector. Returns `Some(flag)` when `args[1]` is
/// `"inspect"` and an extraction-only flag appears before any `"--"` option
/// terminator; returns `None` for non-inspect routes, valid inspect flags, or
/// literal paths placed after `"--"`. Performs no I/O and cannot panic.
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
/// Accepts the process argument vector implicitly. Returns unit after requested
/// output is flushed. Operational I/O/JSON errors propagate; inspect report
/// statuses map to 0 complete, 2 partial/unsupported, and 1 failed, while legacy
/// content flags on inspect and `--pretty` on a directory terminate with 64 and
/// migration advice. A supplied batch or inventory output directory is
/// initialized and validated with exit 64 before any worker starts, so ordinary
/// concurrent jobs never race on directory creation and per-item publication
/// keeps no-overwrite semantics.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if !args.is_empty() {
        args[0] = std::ffi::OsString::from("glassbox");
    }
    if args.get(1).is_some_and(|argument| argument == "parse") {
        return upstream::run().await;
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
            let options = match extraction_options(&args) {
                Ok(options) => options,
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(64);
                }
            };
            let include_markers = !args.no_markers;
            if args.batch {
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
                )?;
                finish_batch(outcome)?;
            } else {
                let mut report = extract_with_policy(
                    args.path.as_ref().unwrap(),
                    &options,
                    hash_policy(args.full_hash),
                );
                if let Some(directory) = args.output_dir {
                    output::persist_extract(
                        &mut report,
                        &directory,
                        1,
                        args.verbose,
                        include_markers,
                    );
                }
                print_extract(
                    &report,
                    args.format,
                    args.pretty,
                    args.verbose,
                    include_markers,
                )?;
                finish_inspect(report.extraction_status())?;
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
/// provenance flag, and `include_markers` (`true` for `bodymatter:v1` comments,
/// `false` for `--no-markers`). Returns unit after stdout output. Serialization
/// and stdout failures are returned; text and Markdown include only selected
/// content, never metadata fabrication.
fn print_extract(
    report: &ExtractReport,
    format: Format,
    pretty: bool,
    verbose: bool,
    include_markers: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let rendered = match format {
        Format::Markdown => {
            glassbox_inspect::selection::render_markdown_with_options(report, include_markers)
        }
        Format::Text => glassbox_inspect::selection::render_text(report),
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
/// jobs, `full_hash` override, `verbose` provenance flag, and `include_markers`
/// (`!no_markers`) for persisted Markdown artifacts. Returns aggregate status
/// after ordered flushed responses and worker join. Native content remains
/// available on OCR failure; malformed transport and output failures retain
/// their respective per-item invalid or process operational status.
fn run_extract_batch(
    options: &ExtractOptions,
    output_dir: Option<&std::path::Path>,
    jobs: usize,
    full_hash: bool,
    verbose: bool,
    include_markers: bool,
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
}

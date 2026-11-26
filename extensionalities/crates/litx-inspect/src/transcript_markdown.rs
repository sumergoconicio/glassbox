//! Module: glassbox-inspect `glassbox:v1` transcript Markdown formatting.
//! File: transcript_markdown.rs.
//! Responsibility: Generate pure-Rust Crockford Base32 ULIDs, format UTC RFC 3339
//! timestamps without external date crates, serialize flat JSON-in-YAML
//! `schema: "glassbox:v1"` transcript frontmatter, and render `bodymatter:v1`
//! `<time>` interval transcript bodies.

use crate::bodymatter::{BodymatterMarker, format_marker, sanitize_body};
use crate::reports::{
    ArtifactKind, AsrReceipt, Completeness, EmbeddedMetadataField, ExtractReport, NativeUnit,
};
use serde::Serialize;
use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared-metadata-aligned schema identifier for Glassbox transcript frontmatter.
pub const GLASSBOX_TRANSCRIPT_SCHEMA: &str = "glassbox:v1";

/// Uppercase Crockford Base32 alphabet used for ULID encoding (excludes I, L, O, U).
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Monotonic counter mixed into fallback entropy if `/dev/urandom` is unavailable.
static ENTROPY_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Rendering options for `glassbox:v1` transcript Markdown output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRenderOptions {
    /// Emit `<!-- bodymatter:v1 <time start="..." end="..."/> -->` markers before intervals.
    pub include_markers: bool,
    /// Emit flat UTF-8 YAML `schema: "glassbox:v1"` frontmatter inside `---` fences.
    pub include_frontmatter: bool,
    /// Optional deterministic ULID override for reproducible unit tests.
    pub id_override: Option<String>,
    /// Optional deterministic UTC RFC 3339 `updated_at` override; an empty string omits `updated_at`.
    pub updated_at_override: Option<String>,
}

impl Default for TranscriptRenderOptions {
    /// Creates default transcript rendering options with frontmatter and `<time>` markers enabled.
    ///
    /// Accepts no parameters. Returns options that generate a fresh ULID and
    /// current UTC RFC 3339 timestamp and emit both `glassbox:v1` frontmatter
    /// and `bodymatter:v1` `<time>` markers. Performs no I/O and cannot fail.
    fn default() -> Self {
        Self {
            include_markers: true,
            include_frontmatter: true,
            id_override: None,
            updated_at_override: None,
        }
    }
}

/// Typed `extraction` JSON object serialized into `glassbox:v1` frontmatter.
#[derive(Debug, Serialize)]
struct ExtractionFrontmatter<'a> {
    mode: &'a str,
    backend: &'a str,
    model: &'a str,
    completeness: Completeness,
    duration_ms: u64,
    sampled_ranges_ms: &'a [[u64; 2]],
    #[serde(skip_serializing_if = "Option::is_none")]
    contact_sheet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_extract: Option<String>,
}

/// Encodes a deterministic 26-character uppercase Crockford Base32 ULID.
///
/// Accepts a UTC Unix timestamp in milliseconds (masked to the canonical 48-bit
/// ULID range) and 10 bytes (80 bits) of randomness. Returns a 26-character
/// uppercase ASCII string matching `^[0-7][0-9A-HJKMNP-TV-Z]{25}$`. Performs
/// no I/O and cannot fail.
pub fn generate_ulid_at(ms: u64, random: [u8; 10]) -> String {
    let ts = ms & 0x0000_FFFF_FFFF_FFFF;
    let mut out = String::with_capacity(26);

    for shift in (0..10).rev() {
        let index = ((ts >> (shift * 5)) & 0x1F) as usize;
        out.push(CROCKFORD_BASE32[index] as char);
    }

    let mut rand_bits: u128 = 0;
    for byte in random {
        rand_bits = (rand_bits << 8) | u128::from(byte);
    }
    for shift in (0..16).rev() {
        let index = ((rand_bits >> (shift * 5)) & 0x1F) as usize;
        out.push(CROCKFORD_BASE32[index] as char);
    }

    out
}

/// Generates a fresh 26-character uppercase Crockford Base32 ULID using system time and OS entropy.
///
/// Accepts no parameters. Returns a 26-character uppercase ULID matching
/// `^[0-7][0-9A-HJKMNP-TV-Z]{25}$`. Reads 80 bits of randomness from
/// `/dev/urandom` on Unix and falls back seamlessly to OS-seeded `RandomState`
/// hashing mixed with monotonic and clock entropy if `/dev/urandom` cannot be
/// read; never panics or returns an error.
pub fn generate_ulid() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let mut random = [0u8; 10];
    fill_random_bytes(&mut random);
    generate_ulid_at(ms, random)
}

/// Reports whether `candidate` is a canonical 26-character uppercase Crockford Base32 ULID.
///
/// Accepts a string slice. Returns `true` iff `candidate` matches
/// `^[0-7][0-9A-HJKMNP-TV-Z]{25}$`. Performs no allocation or I/O and cannot fail.
pub fn is_valid_ulid(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    if bytes.len() != 26 {
        return false;
    }
    if !(b'0'..=b'7').contains(&bytes[0]) {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|byte| CROCKFORD_BASE32.contains(byte))
}

/// Formats a Unix epoch timestamp in seconds as UTC RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).
///
/// Accepts seconds since `1970-01-01T00:00:00Z`. Returns the proleptic Gregorian
/// UTC timestamp computed with Howard Hinnant's civil-from-days algorithm
/// without external date crates. Performs no I/O and cannot fail.
pub fn format_utc_rfc3339(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Howard Hinnant's civil_from_days algorithm (days since 1970-01-01).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Renders an extraction report as `glassbox:v1` transcript Markdown.
///
/// Accepts an [`ExtractReport`] and [`TranscriptRenderOptions`]. Returns UTF-8
/// Markdown containing:
/// 1. When `options.include_frontmatter` is `true`, a flat JSON-in-YAML frontmatter
///    block inside `---` fences with `schema: "glassbox:v1"`, `id`, `title`,
///    `type: "transcript"`, `tags: []`, and sparse optional `source_url`,
///    `language`, `updated_at`, and `extraction` fields (explicitly omitting
///    `content_type`, `sha256`, `metadata_profile`, `domains`, `systems`, and
///    `classification`).
/// 2. Blank-line-delimited [`NativeUnit::TimeInterval`] blocks preceded by
///    `<!-- bodymatter:v1 <time start="HH:MM:SS.mmm" end="HH:MM:SS.mmm"/> -->`
///    markers (when `options.include_markers` is `true` and `start_ms < end_ms`)
///    with every transcript text body neutralized through [`sanitize_body`].
///
/// Returns an empty string when both frontmatter and body blocks are empty.
/// Performs no filesystem or network I/O beyond optional OS entropy reading
/// when `id_override` is `None`; cannot fail.
pub fn render_transcript_markdown(
    report: &ExtractReport,
    options: &TranscriptRenderOptions,
) -> String {
    let mut sections: Vec<String> = Vec::new();

    if options.include_frontmatter {
        sections.push(render_frontmatter(report, options));
    }

    for unit in &report.content.units {
        if unit.unit != NativeUnit::TimeInterval {
            continue;
        }
        if options.include_markers
            && let Some((start_ms, end_ms)) = unit.start_ms.zip(unit.end_ms)
            && let Some(marker) = BodymatterMarker::time(start_ms, end_ms)
        {
            sections.push(format_marker(&marker));
        }
        if let Some(text) = &unit.text {
            let sanitized = sanitize_body(text);
            let trimmed = sanitized.trim_end_matches(['\r', '\n']);
            if !trimmed.trim().is_empty() {
                sections.push(trimmed.to_owned());
            }
        }
    }

    if sections.is_empty() {
        String::new()
    } else {
        let mut out = sections.join("\n\n");
        out.push('\n');
        out
    }
}

/// Formats the flat JSON-in-YAML `schema: "glassbox:v1"` frontmatter fence block.
///
/// Accepts an [`ExtractReport`] and [`TranscriptRenderOptions`]. Returns the
/// fenced frontmatter block (`---\n...\n---`) with no trailing newline. Every
/// scalar, array, and object value is JSON-encoded via [`yaml_json_value`] so
/// arbitrary media titles or paths cannot inject YAML syntax or break fences.
/// Performs no filesystem I/O beyond optional OS entropy reading when
/// `id_override` is absent; cannot fail.
fn render_frontmatter(report: &ExtractReport, options: &TranscriptRenderOptions) -> String {
    let id = options
        .id_override
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(generate_ulid);
    let title = resolve_title(report);
    let source_url = resolve_source_url(report);
    let language = resolve_language(report);
    let updated_at = resolve_updated_at(options);
    let extraction = report
        .media
        .as_ref()
        .and_then(|media| media.asr.as_ref())
        .map(|asr| build_extraction_frontmatter(report, asr));

    let empty_tags: [&str; 0] = [];
    let mut out = String::with_capacity(256);
    out.push_str("---\n");
    let _ = writeln!(
        out,
        "schema: {}",
        yaml_json_value(&GLASSBOX_TRANSCRIPT_SCHEMA)
    );
    let _ = writeln!(out, "id: {}", yaml_json_value(&id));
    let _ = writeln!(out, "title: {}", yaml_json_value(&title));
    let _ = writeln!(out, "type: {}", yaml_json_value(&"transcript"));
    let _ = writeln!(out, "tags: {}", yaml_json_value(&empty_tags));

    if let Some(source_url) = source_url {
        let _ = writeln!(out, "source_url: {}", yaml_json_value(&source_url));
    }
    if let Some(language) = language {
        let _ = writeln!(out, "language: {}", yaml_json_value(&language));
    }
    if let Some(updated_at) = updated_at {
        let _ = writeln!(out, "updated_at: {}", yaml_json_value(&updated_at));
    }
    if let Some(extraction) = extraction {
        let _ = writeln!(out, "extraction: {}", yaml_json_value(&extraction));
    }

    out.push_str("---");
    out
}

/// Resolves the required `title` field from embedded metadata or the source path stem.
///
/// Accepts an [`ExtractReport`]. Returns the first non-empty trimmed embedded
/// `Title` property if present, otherwise the filename stem from
/// `report.metadata.identity.path`, otherwise the filename, and finally
/// `"Untitled transcript"` when the path is empty. Performs no I/O and cannot fail.
fn resolve_title(report: &ExtractReport) -> String {
    if let Some(embedded_title) = report
        .metadata
        .embedded_metadata
        .iter()
        .find(|item| item.field == EmbeddedMetadataField::Title)
        .map(|item| item.value.trim())
        .filter(|value| !value.is_empty())
    {
        return embedded_title.to_owned();
    }
    let path = &report.metadata.identity.path;
    if let Some(stem) = path
        .file_stem()
        .map(|value| value.to_string_lossy().trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return stem;
    }
    if let Some(name) = path
        .file_name()
        .map(|value| value.to_string_lossy().trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return name;
    }
    "Untitled transcript".to_owned()
}

/// Resolves the sparse optional `source_url` field from the report source path.
///
/// Accepts an [`ExtractReport`]. Returns `Some(path)` when
/// `report.metadata.identity.path` is non-empty after trimming, else `None`.
/// Performs no I/O and cannot fail.
fn resolve_source_url(report: &ExtractReport) -> Option<String> {
    let raw = report.metadata.identity.path.to_string_lossy();
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Resolves the sparse optional `language` field from the ASR receipt or embedded metadata.
///
/// Accepts an [`ExtractReport`]. Checks `report.media.asr.language` first and
/// falls back to the first non-empty trimmed [`EmbeddedMetadataField::Language`]
/// entry. Returns `None` when neither is present. Performs no I/O and cannot fail.
fn resolve_language(report: &ExtractReport) -> Option<String> {
    if let Some(asr_lang) = report
        .media
        .as_ref()
        .and_then(|media| media.asr.as_ref())
        .and_then(|asr| asr.language.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(asr_lang.to_owned());
    }
    report
        .metadata
        .embedded_metadata
        .iter()
        .find(|item| item.field == EmbeddedMetadataField::Language)
        .map(|item| item.value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Resolves the sparse optional UTC RFC 3339 `updated_at` timestamp.
///
/// Accepts [`TranscriptRenderOptions`]. When `updated_at_override` is `Some(value)`,
/// returns `Some(trimmed)` if non-empty or `None` if explicitly empty. When
/// `updated_at_override` is `None`, formats the current system UTC time with
/// [`format_utc_rfc3339`]. Performs no I/O and cannot fail.
fn resolve_updated_at(options: &TranscriptRenderOptions) -> Option<String> {
    match options.updated_at_override.as_deref() {
        Some(value) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        None => Some(current_utc_rfc3339()),
    }
}

/// Formats the current system clock as UTC RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).
///
/// Accepts no parameters. Returns the formatted UTC timestamp derived from
/// [`SystemTime::now`], defaulting to the Unix epoch if the system clock
/// precedes `1970-01-01T00:00:00Z`. Performs no I/O and cannot fail.
fn current_utc_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_utc_rfc3339(secs)
}

/// Builds the typed `extraction` frontmatter summary from report and ASR receipts.
///
/// Accepts an [`ExtractReport`] and its [`AsrReceipt`]. Returns an
/// [`ExtractionFrontmatter`] carrying `mode`, `backend`, `model`,
/// `completeness`, `duration_ms`, `sampled_ranges_ms`, and optional
/// `contact_sheet` and `audio_extract` paths (resolved from `report.media`
/// first and falling back to matching `report.metadata.artifacts` entries).
/// Performs no I/O and cannot fail.
fn build_extraction_frontmatter<'a>(
    report: &'a ExtractReport,
    asr: &'a AsrReceipt,
) -> ExtractionFrontmatter<'a> {
    let contact_sheet = report
        .media
        .as_ref()
        .and_then(|media| media.contact_sheet.as_ref())
        .map(|receipt| receipt.output.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            report
                .metadata
                .artifacts
                .iter()
                .find(|artifact| artifact.kind == ArtifactKind::ContactSheet)
                .map(|artifact| artifact.path.to_string_lossy().into_owned())
                .filter(|value| !value.trim().is_empty())
        });
    let audio_extract = report
        .media
        .as_ref()
        .and_then(|media| media.audio_extract.as_ref())
        .map(|receipt| receipt.path.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            report
                .metadata
                .artifacts
                .iter()
                .find(|artifact| artifact.kind == ArtifactKind::AudioExtract)
                .map(|artifact| artifact.path.to_string_lossy().into_owned())
                .filter(|value| !value.trim().is_empty())
        });

    ExtractionFrontmatter {
        mode: &asr.mode,
        backend: &asr.backend,
        model: &asr.model,
        completeness: report.completeness,
        duration_ms: asr.duration_ms,
        sampled_ranges_ms: &asr.sampled_ranges_ms,
        contact_sheet,
        audio_extract,
    }
}

/// Serializes any value as a single-line JSON-in-YAML payload.
///
/// Accepts any [`Serialize`] value. Returns its `serde_json::to_string` output
/// with Unicode line separators (`U+2028` and `U+2029`) escaped to `\u2028` and
/// `\u2029` so emitted YAML lines never split across line terminators. All
/// callers pass infallible UTF-8 JSON types; on any hypothetical serialization
/// error this falls back to `"null"` without panicking.
fn yaml_json_value<T: Serialize>(value: &T) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
    if encoded.contains('\u{2028}') || encoded.contains('\u{2029}') {
        encoded
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029")
    } else {
        encoded
    }
}

/// Populates 10 random bytes from OS entropy with a safe std-only fallback.
///
/// Accepts a mutable 10-byte buffer. On Unix targets, reads `/dev/urandom`
/// directly. If `/dev/urandom` is unavailable (or on non-Unix targets), fills
/// the buffer by hashing OS-seeded `RandomState` instances combined with system
/// time, process ID, and a monotonic atomic counter. Never panics or fails.
fn fill_random_bytes(out: &mut [u8; 10]) {
    #[cfg(unix)]
    {
        use std::io::Read as _;
        if let Ok(mut file) = std::fs::File::open("/dev/urandom")
            && file.read_exact(out).is_ok()
        {
            return;
        }
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let pid = u64::from(std::process::id());
    let seq = ENTROPY_COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut h1 = RandomState::new().build_hasher();
    h1.write_u64(nanos);
    h1.write_u64(pid);
    h1.write_u64(seq);
    let first = h1.finish().to_be_bytes();

    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(first[0] as u64 ^ seq.rotate_left(17));
    h2.write_u64(nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let second = h2.finish().to_be_bytes();

    out[..8].copy_from_slice(&first);
    out[8..10].copy_from_slice(&second[..2]);
}

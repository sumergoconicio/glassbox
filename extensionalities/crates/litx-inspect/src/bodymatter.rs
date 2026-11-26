//! Module: glassbox-inspect `bodymatter:v1` structural Markdown formatting.
//! File: bodymatter.rs.
//! Responsibility: Format self-describing `bodymatter:v1` HTML comment markers,
//! escape XML attribute payloads safely, neutralize HTML comment injection in
//! extracted text, apply minimal line-leading Markdown block escaping, and
//! render GitHub Flavored Markdown (GFM) tables without performing any I/O.

use crate::reports::ContentRow;
use std::fmt::Write as _;

/// Versioned comment prefix emitted at the start of every structural marker.
pub const BODYMATTER_PREFIX: &str = "bodymatter:v1";

/// Typed forward-scoped structural marker for extracted Markdown regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodymatterMarker<'a> {
    /// Physical PDF page boundary with optional printed page label, extraction
    /// origin (`native`, `ocr`, or `mixed`), and optional partial flag.
    Page {
        /// One-based physical page index.
        number: u64,
        /// Optional printed label from the PDF `/PageLabels` tree.
        label: Option<&'a str>,
        /// Content extraction origin (`native`, `ocr`, or `mixed`).
        origin: &'a str,
        /// Whether this page unit was truncated or partially extracted.
        partial: bool,
    },
    /// Spreadsheet worksheet or tabular dataset boundary.
    Sheet {
        /// One-based worksheet or table index.
        number: u64,
        /// Optional worksheet name; omitted for unnamed tabular inputs.
        name: Option<&'a str>,
    },
    /// Presentation slide boundary.
    Slide {
        /// One-based physical slide index.
        number: u64,
    },
    /// Document table boundary (for example in DOCX or ODT).
    Table {
        /// One-based table index in source order.
        number: u64,
    },
    /// Reflowable e-book spine item boundary (for example in EPUB).
    Chapter {
        /// One-based spine item index.
        number: u64,
        /// Optional container-relative href for the spine item.
        href: Option<&'a str>,
    },
    /// Explicit authored page break inside a reflowable document (for example
    /// DOCX `<w:br w:type="page"/>`).
    PageBreak {
        /// One-based count of explicit page breaks in source order.
        number: u64,
    },
    /// Timed media transcript interval (`start` inclusive, `end` exclusive, in
    /// milliseconds from media start).
    Time {
        /// Inclusive interval start timestamp in milliseconds.
        start_ms: u64,
        /// Exclusive interval end timestamp in milliseconds (must exceed `start_ms`).
        end_ms: u64,
    },
}

impl BodymatterMarker<'_> {
    /// Constructs a validated [`BodymatterMarker::Time`] when `end_ms > start_ms`.
    ///
    /// Accepts inclusive `start_ms` and exclusive `end_ms` offsets in milliseconds
    /// from media start. Returns `Some(BodymatterMarker::Time { start_ms, end_ms })`
    /// when `end_ms > start_ms`, or `None` for empty or inverted intervals.
    /// Performs no I/O and cannot fail.
    pub fn time(start_ms: u64, end_ms: u64) -> Option<Self> {
        (end_ms > start_ms).then_some(Self::Time { start_ms, end_ms })
    }
}

/// Formats a millisecond offset as `HH:MM:SS.mmm` with at least two hour digits.
///
/// Accepts an offset in milliseconds from media start. Returns a zero-padded
/// timestamp (`HH:MM:SS.mmm`) where hours expand naturally beyond two digits
/// when `hours >= 100` (for example `100:00:00.000`). Performs no I/O and
/// cannot fail.
pub fn format_time_ms(ms: u64) -> String {
    let millis = ms % 1_000;
    let total_seconds = ms / 1_000;
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let minutes = total_minutes % 60;
    let hours = total_minutes / 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// Formats one structural marker as a single-line `bodymatter:v1` HTML comment.
///
/// Accepts a [`BodymatterMarker`] carrying source coordinates and optional
/// string metadata. Returns a single line in the form
/// `<!-- bodymatter:v1 <ELEMENT ATTR="VALUE" .../> -->` with deterministic
/// attribute order, no trailing newline, and XML-safe attributes guaranteed not
/// to contain `--` or line breaks. Empty optional string attributes (`label`,
/// `name`, `href`) are omitted. For [`BodymatterMarker::Time`], `end_ms > start_ms`
/// is always enforced by clamping non-increasing intervals (`end_ms <= start_ms`)
/// to `(start_ms, start_ms + 1)` (or `(u64::MAX - 1, u64::MAX)` at saturation)
/// so formatting never panics or emits an empty/inverted interval. This function
/// performs no I/O and cannot fail.
pub fn format_marker(marker: &BodymatterMarker<'_>) -> String {
    let mut out = String::with_capacity(64);
    out.push_str("<!-- ");
    out.push_str(BODYMATTER_PREFIX);
    out.push_str(" <");
    match marker {
        BodymatterMarker::Page {
            number,
            label,
            origin,
            partial,
        } => {
            let _ = write!(out, "page number=\"{number}\"");
            if let Some(label) = label.filter(|value| !value.is_empty()) {
                out.push_str(" label=\"");
                out.push_str(&escape_attr(label));
                out.push('"');
            }
            out.push_str(" origin=\"");
            out.push_str(&escape_attr(origin));
            out.push('"');
            if *partial {
                out.push_str(" completeness=\"partial\"");
            }
        }
        BodymatterMarker::Sheet { number, name } => {
            let _ = write!(out, "sheet number=\"{number}\"");
            if let Some(name) = name.filter(|value| !value.is_empty()) {
                out.push_str(" name=\"");
                out.push_str(&escape_attr(name));
                out.push('"');
            }
        }
        BodymatterMarker::Slide { number } => {
            let _ = write!(out, "slide number=\"{number}\"");
        }
        BodymatterMarker::Table { number } => {
            let _ = write!(out, "table number=\"{number}\"");
        }
        BodymatterMarker::Chapter { number, href } => {
            let _ = write!(out, "chapter number=\"{number}\"");
            if let Some(href) = href.filter(|value| !value.is_empty()) {
                out.push_str(" href=\"");
                out.push_str(&escape_attr(href));
                out.push('"');
            }
        }
        BodymatterMarker::PageBreak { number } => {
            let _ = write!(out, "page-break number=\"{number}\"");
        }
        BodymatterMarker::Time { start_ms, end_ms } => {
            let (clamped_start, clamped_end) = if *end_ms > *start_ms {
                (*start_ms, *end_ms)
            } else if *start_ms < u64::MAX {
                (*start_ms, *start_ms + 1)
            } else {
                (u64::MAX - 1, u64::MAX)
            };
            let _ = write!(
                out,
                "time start=\"{}\" end=\"{}\"",
                format_time_ms(clamped_start),
                format_time_ms(clamped_end)
            );
        }
    }
    out.push_str("/> -->");
    out
}

/// Escapes an arbitrary string for a double-quoted XML attribute inside an HTML comment.
///
/// Accepts any UTF-8 attribute value. Returns an XML 1.0 §2.2-legal string with
/// `&`, `<`, `>`, `"`, `\r`, `\n`, `\t`, and Unicode line terminators (`U+0085`,
/// `U+2028`, `U+2029`) XML-escaped (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#13;`,
/// `&#10;`, `&#9;`, `&#133;`, `&#8232;`, `&#8233;`), XML 1.0 §2.2 forbidden
/// scalars (`U+0000..=U+0008`, `U+000B`, `U+000C`, `U+000E..=U+001F`, `U+FFFE`,
/// `U+FFFF`) replaced with `U+FFFD` so strict XML 1.0 parsers never reject the
/// marker, and every `--` pair replaced with `&#45;&#45;` so the emitted
/// attribute can neither break out of an HTML comment nor violate XML comment
/// syntax. All XML 1.0-legal inputs decode losslessly under standard XML 1.0
/// parsers while guaranteeing single-line framing under Unicode `splitlines()`.
/// Performs no I/O and cannot fail.
pub fn escape_attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '\n' => out.push_str("&#10;"),
            '\t' => out.push_str("&#9;"),
            '\u{0085}' => out.push_str("&#133;"),
            '\u{2028}' => out.push_str("&#8232;"),
            '\u{2029}' => out.push_str("&#8233;"),
            '\u{0000}'..='\u{0008}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000E}'..='\u{001F}'
            | '\u{FFFE}'
            | '\u{FFFF}' => out.push('\u{FFFD}'),
            _ => out.push(ch),
        }
    }
    if out.contains("--") {
        out.replace("--", "&#45;&#45;")
    } else {
        out
    }
}

/// Reports whether `ch` is a forbidden C0 control or noncharacter scalar.
///
/// Accepts any Unicode scalar value. Returns `true` for forbidden C0 controls
/// (`U+0000..=U+0008`, `U+000E..=U+001F`, including ESC `U+001B`) and the
/// noncharacters `U+FFFE` and `U+FFFF`; returns `false` for `\t`, line
/// terminators (`\n`, `\r`, `\u{000B}`, `\u{000C}`), and all other scalars.
/// Performs no I/O and cannot fail.
fn is_forbidden_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{0000}'..='\u{0008}' | '\u{000E}'..='\u{001F}' | '\u{FFFE}' | '\u{FFFF}'
    )
}

/// Neutralizes raw source HTML, control characters, and line-leading Markdown block triggers.
///
/// Accepts raw extracted prose. Streams in a single pass over `text` without
/// full-document duplicate buffers, normalizing `\r\n`, `\r`, `\n`, `\u{000B}`,
/// `\u{000C}`, `\u{0085}`, `\u{2028}`, and `\u{2029}` line boundaries to `\n`,
/// replacing forbidden C0 controls (`U+0000..=U+0008`, `U+000E..=U+001F`,
/// including ESC `U+001B`) and `U+FFFE`/`U+FFFF` with `U+FFFD` while preserving
/// `\t` as ordinary whitespace, escaping every `<` to `&lt;` (which neutralizes
/// both `<!--` HTML comments and raw HTML blocks/inline tags), and escaping
/// only line-leading (after 0–3 leading spaces) Markdown block syntax: ATX
/// headings (`#`), blockquotes (`>`), unordered lists (`-`, `+`, `*`), thematic
/// breaks and Setext underlines (`---`, `===`, `***`, `___`), ordered lists
/// (`1.`, `1)`), and fenced code blocks (```` ``` ````, `~~~`). Returns the
/// sanitized prose string. Ordinary inline prose is left without spurious
/// backslashes. Performs no I/O and cannot fail.
pub fn sanitize_body(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut line_buf = String::new();
    let mut line_start = 0usize;
    let mut chars = text.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        match ch {
            '\r' => {
                sanitize_line(&text[line_start..idx], &mut line_buf, &mut out);
                out.push('\n');
                if let Some(&(next_idx, '\n')) = chars.peek() {
                    let _ = chars.next();
                    line_start = next_idx + '\n'.len_utf8();
                } else {
                    line_start = idx + '\r'.len_utf8();
                }
            }
            '\n' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}' => {
                sanitize_line(&text[line_start..idx], &mut line_buf, &mut out);
                out.push('\n');
                line_start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    sanitize_line(&text[line_start..], &mut line_buf, &mut out);
    out
}

/// Neutralizes `<` and forbidden control scalars on one line and applies minimal block escaping.
///
/// Accepts one newline-free line, a reusable scratch buffer used only when the
/// line contains `<` or a forbidden control scalar, and the destination output
/// buffer. Returns unit after replacing every `<` with `&lt;` and every
/// [`is_forbidden_control`] scalar with `U+FFFD`, then writing the minimally
/// block-escaped line to `out`. Performs no I/O and cannot fail.
fn sanitize_line(line: &str, line_buf: &mut String, out: &mut String) {
    if line.chars().any(|ch| ch == '<' || is_forbidden_control(ch)) {
        line_buf.clear();
        for ch in line.chars() {
            match ch {
                '<' => line_buf.push_str("&lt;"),
                ch if is_forbidden_control(ch) => line_buf.push('\u{FFFD}'),
                _ => line_buf.push(ch),
            }
        }
        escape_leading_block_syntax(line_buf, out);
    } else {
        escape_leading_block_syntax(line, out);
    }
}

/// Escapes one cell value for safe inclusion inside a GFM pipe table row.
///
/// Accepts raw cell text. Returns a single-line cell payload with every source
/// `<` escaped to `&lt;` (neutralizing both `<!--` comments and raw HTML tags
/// before generated breaks are inserted), `\r\n`, `\r`, `\n`, `\t`, `\u{000B}`,
/// `\u{000C}`, `\u{0085}`, `\u{2028}`, and `\u{2029}` mapped to `<br>`,
/// forbidden C0 controls (`U+0000..=U+0008`, `U+000E..=U+001F`) and
/// `U+FFFE`/`U+FFFF` replaced with `U+FFFD`, and every pipe `|` (along with any
/// immediately preceding backslashes) escaped so cell boundaries cannot split.
/// Every emitted cell remains strictly single-line even under Unicode
/// `splitlines()` and contains zero raw C0 controls. Performs no I/O and cannot
/// fail.
pub fn escape_table_cell(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    let mut pending_backslashes = 0usize;
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            pending_backslashes += 1;
            continue;
        }
        if ch == '|' {
            // Double every backslash immediately preceding `|` and add one more
            // so GFM sees an odd backslash run and keeps the pipe inside the cell.
            for _ in 0..(pending_backslashes * 2 + 1) {
                out.push('\\');
            }
            pending_backslashes = 0;
            out.push('|');
            continue;
        }
        if pending_backslashes > 0 {
            for _ in 0..pending_backslashes {
                out.push('\\');
            }
            pending_backslashes = 0;
        }
        match ch {
            '<' => out.push_str("&lt;"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                }
                out.push_str("<br>");
            }
            '\n' | '\t' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}' => {
                out.push_str("<br>");
            }
            ch if is_forbidden_control(ch) => out.push('\u{FFFD}'),
            _ => out.push(ch),
        }
    }
    for _ in 0..pending_backslashes {
        out.push('\\');
    }
    out
}

/// Renders structured dataset rows as a GitHub Flavored Markdown (GFM) pipe table.
///
/// Accepts a slice of [`ContentRow`] values. Returns an empty string when `rows`
/// is empty; otherwise returns a header row (`rows[0]`), a GFM separator row
/// (`| --- | ... |`), and one data row per remaining entry, joined by `'\n'`
/// with no trailing newline. Short rows are padded with empty cells to match
/// the widest row (at least one column), and every cell is sanitized via
/// [`escape_table_cell`]. Performs no I/O and cannot fail.
pub fn render_gfm_table(rows: &[ContentRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let cols = rows
        .iter()
        .map(|row| row.values.len())
        .max()
        .unwrap_or(0)
        .max(1);
    let mut out = String::new();
    write_gfm_row(&rows[0].values, cols, &mut out);
    out.push('\n');
    out.push('|');
    for _ in 0..cols {
        out.push_str(" --- |");
    }
    for row in &rows[1..] {
        out.push('\n');
        write_gfm_row(&row.values, cols, &mut out);
    }
    out
}

/// Appends one line of prose to `out`, escaping only a leading Markdown block trigger.
///
/// Accepts a single newline-free line and destination buffer. Returns unit after
/// writing the line unchanged or with a single backslash neutralizing a CommonMark
/// block trigger after 0–3 leading spaces. Cannot fail.
fn escape_leading_block_syntax(line: &str, out: &mut String) {
    let leading_spaces = line.bytes().take_while(|&byte| byte == b' ').count();
    if leading_spaces > 3 {
        out.push_str(line);
        return;
    }
    let (indent, rest) = line.split_at(leading_spaces);
    if let Some(delim_idx) = ordered_list_delimiter_index(rest) {
        out.push_str(indent);
        out.push_str(&rest[..delim_idx]);
        out.push('\\');
        out.push_str(&rest[delim_idx..]);
        return;
    }
    if needs_leading_escape(rest) {
        out.push_str(indent);
        out.push('\\');
        out.push_str(rest);
    } else {
        out.push_str(line);
    }
}

/// Returns the byte index of `.` or `)` when `rest` begins with a CommonMark ordered list marker.
///
/// Accepts a line slice after 0–3 leading spaces. Returns `Some(index)` when
/// `rest` starts with 1–9 ASCII digits followed by `.` or `)` and then space,
/// tab, or end-of-line; otherwise returns `None`. Cannot fail.
fn ordered_list_delimiter_index(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if !(1..=9).contains(&digits) {
        return None;
    }
    let delim = *bytes.get(digits)?;
    if delim != b'.' && delim != b')' {
        return None;
    }
    if matches!(bytes.get(digits + 1), None | Some(b' ') | Some(b'\t')) {
        Some(digits)
    } else {
        None
    }
}

/// Reports whether `rest` starts with a single-prefix Markdown block-syntax trigger.
///
/// Accepts a line slice after 0–3 leading spaces. Returns `true` for ATX
/// headings, blockquotes, unordered list markers, thematic breaks / Setext
/// `=` and `-` underlines, and backtick/tilde code fences; returns `false` for
/// ordinary prose lines. Cannot fail.
fn needs_leading_escape(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    match first {
        b'#' => is_atx_heading(bytes),
        b'>' => true,
        b'=' => bytes
            .iter()
            .skip_while(|&&byte| byte == b'=')
            .all(|&byte| byte == b' ' || byte == b'\t'),
        b'-' => {
            matches!(bytes.get(1), None | Some(b' ') | Some(b'\t'))
                || bytes
                    .iter()
                    .all(|&byte| byte == b'-' || byte == b' ' || byte == b'\t')
        }
        b'+' => matches!(bytes.get(1), None | Some(b' ') | Some(b'\t')),
        b'*' => {
            matches!(bytes.get(1), None | Some(b' ') | Some(b'\t'))
                || is_thematic_break(bytes, b'*')
        }
        b'_' => is_thematic_break(bytes, b'_'),
        b'`' => rest.starts_with("```"),
        b'~' => rest.starts_with("~~~"),
        _ => false,
    }
}

/// Reports whether `bytes` starts with a CommonMark ATX heading (`1..=6` `#` + whitespace/EOL).
///
/// Accepts raw UTF-8 bytes after 0–3 leading spaces. Returns `true` when an ATX
/// heading prefix is present, else `false`. Cannot fail.
fn is_atx_heading(bytes: &[u8]) -> bool {
    let hashes = bytes.iter().take_while(|&&byte| byte == b'#').count();
    (1..=6).contains(&hashes) && matches!(bytes.get(hashes), None | Some(b' ') | Some(b'\t'))
}

/// Reports whether `bytes` forms a CommonMark thematic break for `marker`.
///
/// Accepts raw UTF-8 bytes after 0–3 leading spaces and the marker byte
/// (`b'*'` or `b'_'`). Returns `true` when `bytes` contains at least three
/// `marker` bytes and only spaces or tabs otherwise. Cannot fail.
fn is_thematic_break(bytes: &[u8], marker: u8) -> bool {
    let mut count = 0usize;
    for &byte in bytes {
        if byte == marker {
            count += 1;
        } else if byte != b' ' && byte != b'\t' {
            return false;
        }
    }
    count >= 3
}

/// Appends one padded GFM table row to `out`.
///
/// Accepts the row's cell values, target column count, and destination buffer.
/// Returns unit after writing `| c1 | c2 | ... |` with every cell sanitized via
/// [`escape_table_cell`] and missing trailing columns padded as empty cells.
/// Cannot fail.
fn write_gfm_row(values: &[String], cols: usize, out: &mut String) {
    out.push('|');
    for index in 0..cols {
        let cell = values.get(index).map(String::as_str).unwrap_or("");
        out.push(' ');
        out.push_str(&escape_table_cell(cell));
        out.push_str(" |");
    }
}

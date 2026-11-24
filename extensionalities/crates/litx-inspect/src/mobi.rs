//! Module: glassbox-inspect PalmDB/MOBI ebook reader.
//! File: mobi.rs.
//! Responsibility: Parse PalmDB/MOBI headers, decode continuous text streams across record boundaries, and emit selected text-record units.
use crate::{
    Claim, InspectOptions, InspectReport,
    binary::{Source, be16, be32, slice},
    office, xml,
};
use quick_xml::{Reader, events::Event};
use std::path::Path;

/// Byte span of one decompressed PalmDB text record inside the continuous `text_length`-bounded stream.
struct TextRecordSpan {
    record_num: u64,
    byte_start: usize,
    byte_end: usize,
}

/// Inspects a MOBI/AZW/PRC ebook path and records metadata plus extracted text or diagnostics.
///
/// Accepts a source file path, destination report, and extraction options.
/// Returns unit after metadata and text-record extraction; header or I/O
/// failures are recorded under stage `"mobi"`.
pub(crate) fn inspect(path: &Path, report: &mut InspectReport, options: &InspectOptions) {
    if let Err(error) = inner(path, report, options) {
        report.error("mobi", error);
    }
}

fn claim(report: &mut InspectReport, key: &str, value: impl ToString, source: &'static str) {
    report.claims.push(Claim {
        key: key.into(),
        value: value.to_string(),
        source,
    });
}

/// Decodes a single MOBI byte slice according to the declared MOBI text encoding.
///
/// Accepts raw bytes and the MOBI header encoding code (`65001` UTF-8 or `1252`
/// Windows-1252). Returns the decoded UTF-8 string or an error for invalid UTF-8
/// or unsupported encodings.
fn decode(bytes: &[u8], encoding: u32) -> xml::Result<String> {
    match encoding {
        65001 => Ok(std::str::from_utf8(bytes)?.into()),
        1252 => Ok(encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned()),
        _ => Err("unsupported MOBI text encoding".into()),
    }
}

/// Decoded UTF-8 text stream paired with per-record `(record_num, decoded_start, decoded_end)` spans.
type DecodedRecordStream = (String, Vec<(u64, usize, usize)>);

/// Decodes the continuous MOBI byte stream once while mapping each PalmDB record
/// to its byte span inside the decoded UTF-8 string.
///
/// Accepts the `text_length`-truncated continuous byte stream, raw record byte
/// spans, and MOBI text encoding code (`65001` or `1252`). Returns the decoded
/// UTF-8 string and per-record `(record_num, decoded_start, decoded_end)` spans.
/// Multi-byte UTF-8 sequences split across adjacent records decode cleanly as a
/// single continuous stream; invalid UTF-8 or unsupported encodings return an error.
fn decode_stream_with_spans(
    stream: &[u8],
    raw_spans: &[TextRecordSpan],
    encoding: u32,
) -> xml::Result<DecodedRecordStream> {
    match encoding {
        65001 => {
            let decoded = std::str::from_utf8(stream)?.to_owned();
            let spans = raw_spans
                .iter()
                .map(|span| (span.record_num, span.byte_start, span.byte_end))
                .collect();
            Ok((decoded, spans))
        }
        1252 => {
            let mut decoded = String::with_capacity(stream.len());
            let mut spans = Vec::with_capacity(raw_spans.len());
            for span in raw_spans {
                let start = decoded.len();
                let chunk = encoding_rs::WINDOWS_1252
                    .decode(&stream[span.byte_start..span.byte_end])
                    .0;
                decoded.push_str(&chunk);
                spans.push((span.record_num, start, decoded.len()));
            }
            Ok((decoded, spans))
        }
        _ => Err("unsupported MOBI text encoding".into()),
    }
}

/// Parses PalmDB structure and extracts selected decoded text records.
///
/// Accepts a source path, destination report, and extraction controls. Returns
/// unit after container metadata and content units. Invalid offsets, encrypted
/// text, unsupported compression, expansion overflow, and selection errors fail.
fn inner(path: &Path, report: &mut InspectReport, options: &InspectOptions) -> xml::Result<()> {
    let mut source = Source::open(path)?;
    let pdb = source.range(0, 78)?;
    if slice(&pdb, 60, 8)? != b"BOOKMOBI" {
        return Err("invalid MOBI PalmDB signature".into());
    }
    let count = be16(&pdb, 76)? as usize;
    if count == 0 || count > 4096 {
        return Err("MOBI record-count cap exceeded".into());
    }
    let table = source.range(78, count * 8)?;
    let mut offsets = Vec::with_capacity(count + 1);
    for i in 0..count {
        offsets.push(u64::from(be32(&table, i * 8)?));
    }
    offsets.push(source.len);
    if offsets[0] < (78 + count * 8) as u64 || offsets.windows(2).any(|p| p[0] >= p[1]) {
        return Err("invalid MOBI record offsets".into());
    }
    let head = source.range(offsets[0], (offsets[1] - offsets[0]) as usize)?;
    if slice(&head, 16, 4)? != b"MOBI" {
        return Err("missing MOBI record header".into());
    }
    let header_size = be32(&head, 20)? as usize;
    if header_size < 116 {
        return Err("short MOBI header".into());
    }
    slice(&head, 16, header_size)?;
    let encoding = be32(&head, 28)?;
    let version = be32(&head, 36)?;
    office::identify(
        report,
        "mobi",
        "application/x-mobipocket-ebook",
        "MOBI ebook",
    );
    // AZW aliases cannot be distinguished by extension; KF8 has a byte version.
    report.format.disagreement = !report.format.extension.is_empty()
        && !matches!(
            report.format.extension.as_str(),
            "mobi" | "azw" | "azw3" | "prc"
        );
    claim(report, "mobi_version", version, "bytes");
    claim(report, "text_encoding", encoding, "bytes");
    let title_offset = be32(&head, 84)? as usize;
    let title_length = be32(&head, 88)? as usize;
    if title_length > 0 {
        claim(
            report,
            "title",
            decode(slice(&head, title_offset, title_length)?, encoding)?,
            "container_metadata",
        );
    }
    let mut cover = None;
    if be32(&head, 128)? & 0x40 != 0 {
        let exth = slice(&head, 16 + header_size, 12)?;
        if &exth[..4] != b"EXTH" {
            return Err("missing EXTH signature".into());
        }
        let exth = slice(&head, 16 + header_size, be32(exth, 4)? as usize)?;
        let records = be32(exth, 8)? as usize;
        if records > 4096 {
            return Err("EXTH record-count cap exceeded".into());
        }
        let mut cursor = 12;
        for _ in 0..records {
            let kind = be32(exth, cursor)?;
            let len = be32(exth, cursor + 4)? as usize;
            if len < 8 {
                return Err("invalid EXTH record length".into());
            }
            let data = slice(exth, cursor + 8, len - 8)?;
            let key = match kind {
                100 => Some("author"),
                101 => Some("publisher"),
                113 | 504 => Some("asin"),
                503 => Some("title"),
                524 => Some("language"),
                _ => None,
            };
            if let Some(key) = key {
                claim(report, key, decode(data, encoding)?, "container_metadata");
            }
            if kind == 201 {
                cover = Some(be32(data, 0)?);
            }
            cursor += len;
        }
    }
    if !report.claims.iter().any(|c| c.key == "language") {
        let locale = be32(&head, 92)?;
        let language = match locale & 0x3ff {
            9 => Some("en"),
            7 => Some("de"),
            10 => Some("es"),
            12 => Some("fr"),
            16 => Some("it"),
            17 => Some("ja"),
            18 => Some("ko"),
            4 => Some("zh"),
            25 => Some("ru"),
            22 => Some("pt"),
            1 => Some("ar"),
            _ => None,
        };
        if let Some(language) = language {
            claim(report, "language", language, "container_metadata");
        }
        claim(report, "mobi_locale", locale, "container_metadata");
    }
    if options.thumbnail.is_some()
        && let Some(relative) = cover
    {
        let result = (|| -> xml::Result<()> {
            let index = be32(&head, 108)?
                .checked_add(relative)
                .ok_or("MOBI cover offset overflow")? as usize;
            if index >= count {
                return Err("MOBI cover record out of bounds".into());
            }
            let bytes = source.range(
                offsets[index],
                (offsets[index + 1] - offsets[index]) as usize,
            )?;
            crate::thumbnail::write(std::io::Cursor::new(bytes), "embedded", options, report);
            Ok(())
        })();
        if let Err(error) = result {
            report.error("thumbnail", error);
        }
    }
    let text_result = (|| -> xml::Result<()> {
        if be16(&head, 12)? != 0 {
            return Err("encrypted MOBI text is unavailable; metadata retained".into());
        }
        let compression = be16(&head, 0)?;
        if !matches!(compression, 1 | 2) {
            return Err("unsupported MOBI compression (HUFF/CDIC); metadata retained".into());
        }
        let text_records = be16(&head, 8)? as usize;
        if text_records >= count {
            return Err("MOBI text record count exceeds database".into());
        }
        let text_length = be32(&head, 4)? as usize;
        if text_length > 32 << 20 {
            return Err("MOBI expanded text cap exceeded".into());
        }
        let extras = if header_size >= 228 {
            be32(&head, 240)?
        } else {
            0
        };
        let plan = match &options.native_selection {
            Some(selection) => Some(crate::selection::begin(
                report,
                selection,
                crate::reports::NativeUnit::MobiTextRecord,
                text_records as u64,
            )?),
            None => None,
        };
        let mut stream = Vec::new();
        let mut raw_spans = Vec::with_capacity(text_records);
        for i in 1..=text_records {
            let bytes = source.range(offsets[i], (offsets[i + 1] - offsets[i]) as usize)?;
            let body = strip_trailing(&bytes, extras)?;
            let decoded = if compression == 1 {
                body.to_vec()
            } else {
                palmdoc(body)?
            };
            if decoded.len() > 65_536 || stream.len().saturating_add(decoded.len()) > 32 << 20 {
                return Err("MOBI record expansion cap exceeded".into());
            }
            let raw_start = stream.len();
            stream.extend_from_slice(&decoded);
            raw_spans.push(TextRecordSpan {
                record_num: i as u64,
                byte_start: raw_start.min(text_length),
                byte_end: stream.len().min(text_length),
            });
        }
        if stream.len() < text_length {
            return Err("truncated MOBI text".into());
        }
        stream.truncate(text_length);
        let (decoded, decoded_spans) = decode_stream_with_spans(&stream, &raw_spans, encoding)?;
        if let Some(plan) = plan {
            let record_texts = markup_text_by_record(&decoded, &decoded_spans)?;
            for ((record_num, _, _), text) in decoded_spans.into_iter().zip(record_texts) {
                if !plan.contains(record_num) {
                    continue;
                }
                report.native_units.push(crate::selection::text_unit(
                    record_num,
                    crate::reports::NativeUnit::MobiTextRecord,
                    Some(format!("PalmDB record {record_num}")),
                    text,
                    crate::reports::ContentOrigin::Native,
                ));
            }
        } else {
            let text = markup_text(&decoded)?;
            report.append_text(&text, options.max_chars);
        }
        claim(
            report,
            "text_status",
            if version >= 8 {
                "best_effort_kf8_flow_text"
            } else {
                "native_mobi_text"
            },
            "bytes",
        );
        Ok(())
    })();
    if let Err(error) = text_result {
        report.error("mobi_text", error);
    }
    Ok(())
}

fn strip_trailing(mut data: &[u8], flags: u32) -> xml::Result<&[u8]> {
    for bit in 1..32 {
        if flags & (1 << bit) == 0 {
            continue;
        }
        let mut length = 0usize;
        let mut done = false;
        for n in 0..4 {
            let b = *data
                .get(
                    data.len()
                        .checked_sub(n + 1)
                        .ok_or("truncated MOBI trailing data")?,
                )
                .ok_or("truncated MOBI trailing data")?;
            length |= ((b & 0x7f) as usize) << (7 * n);
            if b & 0x80 != 0 {
                done = true;
                break;
            }
        }
        if !done || length == 0 || length > data.len() {
            return Err("invalid MOBI trailing entry size".into());
        }
        data = &data[..data.len() - length];
    }
    if flags & 1 != 0 {
        let length = (data.last().ok_or("missing MOBI overlap byte")? & 3) as usize + 1;
        data = data
            .get(
                ..data
                    .len()
                    .checked_sub(length)
                    .ok_or("truncated MOBI overlap")?,
            )
            .ok_or("truncated MOBI overlap")?;
    }
    Ok(data)
}

fn palmdoc(data: &[u8]) -> xml::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        i += 1;
        match b {
            1..=8 => {
                out.extend_from_slice(slice(data, i, b as usize)?);
                i += b as usize;
            }
            0 | 9..=127 => out.push(b),
            128..=191 => {
                let pair = u16::from(b) * 256
                    + u16::from(*data.get(i).ok_or("truncated PalmDOC backreference")?);
                i += 1;
                let distance = ((pair & 0x3fff) >> 3) as usize;
                if distance == 0 || distance > out.len() {
                    return Err("invalid PalmDOC backreference distance".into());
                }
                for _ in 0..(pair & 7) + 3 {
                    out.push(out[out.len() - distance]);
                }
            }
            _ => {
                out.push(b' ');
                out.push(b ^ 0x80);
            }
        }
        if out.len() > 65_536 {
            return Err("PalmDOC expansion cap exceeded".into());
        }
    }
    Ok(out)
}

/// Finds the zero-based record slot in `spans` owning UTF-8 byte `offset`.
///
/// Accepts non-empty sorted `(record_num, start, end)` spans partitioning the
/// decoded stream and a byte offset within the stream. Returns the index of
/// the first record whose `end > offset`, clamping to the last slot at EOF.
/// Performs no I/O and cannot fail.
fn record_for_offset(spans: &[(u64, usize, usize)], offset: usize) -> usize {
    let idx = spans.partition_point(|&(_, _, end)| end <= offset);
    idx.min(spans.len().saturating_sub(1))
}

/// Attributes decoded text from an XML text event across the record spans that
/// own each character's leading byte.
///
/// Accepts the event's raw UTF-8 slice, its byte range `[start_pos..end_pos)`
/// inside the full decoded stream, per-record spans, mutable per-record output
/// buffers, and XML-normalized text for the single-record fast path. Returns
/// unit after pushing characters to their owning records while normalizing
/// `\r\n` and `\r` to `\n`. Performs no I/O and cannot fail.
fn push_text_range(
    raw: &str,
    start_pos: usize,
    end_pos: usize,
    spans: &[(u64, usize, usize)],
    outputs: &mut [String],
    xml_text: &str,
) {
    if outputs.is_empty() || raw.is_empty() || start_pos >= end_pos {
        return;
    }
    let first = record_for_offset(spans, start_pos);
    if end_pos <= spans[first].2 {
        outputs[first].push_str(xml_text);
        return;
    }
    let mut chars = raw.char_indices().peekable();
    while let Some((rel, ch)) = chars.next() {
        let out_ch = if ch == '\r' {
            if matches!(chars.peek(), Some(&(_, '\n'))) {
                chars.next();
            }
            '\n'
        } else {
            ch
        };
        let idx = record_for_offset(spans, start_pos + rel);
        outputs[idx].push(out_ch);
    }
}

/// Parses continuous MOBI HTML markup once while attributing visible text to
/// each PalmDB text record by source byte offset.
///
/// Accepts the continuously decoded MOBI markup string and per-record
/// `(record_num, decoded_start, decoded_end)` spans. Returns one visible text
/// string per span in the same order, carrying open-tag, blocked-section
/// (`head`, `title`, `style`, `script`), entity, and newline state across
/// record boundaries. Leading UTF-8 BOMs are stripped explicitly before lexer
/// construction so `buffer_position()` stays aligned with `spans`. Malformed
/// character references or unrecoverable XML lexer failures return an error.
fn markup_text_by_record(decoded: &str, spans: &[(u64, usize, usize)]) -> xml::Result<Vec<String>> {
    let mut outputs = vec![String::new(); spans.len()];
    if spans.is_empty() {
        return Ok(outputs);
    }
    let bom_len = decoded.len() - decoded.trim_start_matches('\u{feff}').len();
    let reader_slice = &decoded[bom_len..];
    let mut reader = Reader::from_str(reader_slice);
    reader.config_mut().check_end_names = false;
    reader.config_mut().allow_unmatched_ends = true;
    let mut blocked = None;
    let version = quick_xml::XmlVersion::default();
    loop {
        let start_pos = bom_len + reader.buffer_position() as usize;
        let event = reader.read_event()?;
        let end_pos = bom_len + reader.buffer_position() as usize;
        match event {
            Event::Start(e) => {
                let name = e.name().as_ref().to_ascii_lowercase();
                if blocked.is_none()
                    && matches!(name.as_str(), "head" | "title" | "style" | "script")
                {
                    blocked = Some(name.clone());
                }
                if blocked.is_none() && name == "br" {
                    let idx = record_for_offset(spans, start_pos);
                    outputs[idx].push('\n');
                }
            }
            Event::Empty(e) if blocked.is_none() => {
                let name = e.name().as_ref().to_ascii_lowercase();
                if name == "br" {
                    let idx = record_for_offset(spans, start_pos);
                    outputs[idx].push('\n');
                }
            }
            Event::End(e) => {
                let name = e.name().as_ref().to_ascii_lowercase();
                if blocked.as_ref() == Some(&name) {
                    blocked = None;
                }
                if blocked.is_none()
                    && matches!(
                        name.as_str(),
                        "p" | "div"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "li"
                            | "tr"
                            | "blockquote"
                    )
                {
                    let idx = record_for_offset(spans, start_pos);
                    if !outputs[idx].is_empty() && !outputs[idx].ends_with('\n') {
                        outputs[idx].push('\n');
                    }
                }
            }
            Event::Text(e) if blocked.is_none() => {
                let xml_text = e.xml_content(version);
                push_text_range(
                    e.as_ref(),
                    start_pos,
                    end_pos,
                    spans,
                    &mut outputs,
                    &xml_text,
                );
            }
            Event::GeneralRef(e) if blocked.is_none() => {
                let idx = record_for_offset(spans, start_pos);
                if let Some(c) = e.resolve_char_ref()? {
                    outputs[idx].push(c);
                } else {
                    outputs[idx].push_str(match e.xml_content(version).as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        "nbsp" => " ",
                        _ => "�",
                    });
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(outputs)
}

/// Extracts visible text from a single continuous MOBI markup stream.
///
/// Accepts decoded MOBI HTML markup. Returns visible plain text with
/// `<head>`/`<title>`/`<style>`/`<script>` omitted, HTML entities resolved, and
/// block breaks preserved. Returns an error on invalid character references or
/// unrecoverable XML lexer failures.
fn markup_text(value: &str) -> xml::Result<String> {
    let spans = [(1_u64, 0_usize, value.len())];
    Ok(markup_text_by_record(value, &spans)?
        .into_iter()
        .next()
        .unwrap_or_default())
}

//! Module: glassbox-inspect EPUB package reader.
//! File: epub.rs.
//! Responsibility: Parse EPUB container/OPF metadata, resolve spine XHTML parts, and extract readable text with block/table whitespace boundaries.
use crate::{
    Claim, InspectOptions, InspectReport,
    archive::Archive,
    office,
    xml::{self, Content, Node},
};
use std::collections::BTreeMap;

const CONTAINER: &str = "urn:oasis:names:tc:opendocument:xmlns:container";
const OPF: &str = "http://www.idpf.org/2007/opf";
const DC: &str = "http://purl.org/dc/elements/1.1/";

/// Inspects an EPUB archive if `META-INF/container.xml` is present.
///
/// Accepts an open ZIP archive, destination report, and extraction options.
/// Returns unit after populating metadata and selected spine text; container
/// or OPF failures are recorded under stage `"epub"`.
pub(crate) fn inspect(archive: &mut Archive, report: &mut InspectReport, options: &InspectOptions) {
    if !archive.names.iter().any(|n| n == "META-INF/container.xml") {
        return;
    }
    if let Err(error) = inner(archive, report, options) {
        report.error("epub", error);
    }
}

/// Parses EPUB package structure and extracts selected native spine items.
///
/// Accepts an open archive, destination report, and extraction controls. Returns
/// unit after metadata and selected XHTML text. Invalid packages, missing parts,
/// unsupported spine media, and unsatisfiable selections return reader errors;
/// individual selected-item failures remain partial-output diagnostics.
fn inner(
    archive: &mut Archive,
    report: &mut InspectReport,
    options: &InspectOptions,
) -> xml::Result<()> {
    let container = office::read_xml(archive, "META-INF/container.xml")?;
    if !container.is(CONTAINER, "container") {
        return Err("invalid EPUB container namespace/root".into());
    }
    let package = container
        .descendants(CONTAINER, "rootfile")
        .into_iter()
        .find(|n| n.attr("media-type") == Some("application/oebps-package+xml"))
        .ok_or("EPUB has no OPF rootfile")?;
    let path = resolve(
        "",
        package
            .attr("full-path")
            .ok_or("missing EPUB rootfile path")?,
    )?;
    let root = office::read_xml(archive, &path)?;
    if !root.is(OPF, "package") {
        return Err("invalid OPF namespace/root".into());
    }
    office::identify(report, "epub", "application/epub+zip", "EPUB ebook");
    let mut cover_id = None;
    if let Some(metadata) = root.children().find(|n| n.is(OPF, "metadata")) {
        for node in metadata.children() {
            if node.ns == DC {
                let value = node.text();
                if !value.trim().is_empty() {
                    report.claims.push(Claim {
                        key: if node.name == "creator" {
                            "author".into()
                        } else {
                            node.name.clone()
                        },
                        value,
                        source: "container_metadata",
                    });
                }
            }
            if node.is(OPF, "meta") && node.attr("name") == Some("cover") {
                cover_id = node.attr("content");
            }
        }
    }
    let manifest = root
        .children()
        .find(|n| n.is(OPF, "manifest"))
        .ok_or("missing EPUB manifest")?;
    let mut items = BTreeMap::new();
    for item in manifest.children().filter(|n| n.is(OPF, "item")) {
        if items.len() >= 4096 {
            return Err("EPUB manifest item cap exceeded".into());
        }
        let id = item.attr("id").ok_or("manifest item has no id")?;
        if items.insert(id, item).is_some() {
            return Err("duplicate EPUB manifest id".into());
        }
    }
    let spine = root
        .children()
        .find(|n| n.is(OPF, "spine"))
        .ok_or("missing EPUB spine")?;
    let references: Vec<_> = spine.children().filter(|n| n.is(OPF, "itemref")).collect();
    if references.len() > 4096 {
        return Err("EPUB spine cap exceeded".into());
    }
    report.counts.spine_items = Some(references.len());
    if let Some(selection) = &options.native_selection {
        let plan = crate::selection::begin(
            report,
            selection,
            crate::reports::NativeUnit::SpineItem,
            references.len() as u64,
        )?;
        for (index, reference) in references.into_iter().enumerate() {
            let number = index as u64 + 1;
            if !plan.contains(number) {
                continue;
            }
            let result = (|| -> xml::Result<()> {
                let item = items
                    .get(reference.attr("idref").ok_or("spine item has no idref")?)
                    .ok_or("spine references missing manifest item")?;
                if item.attr("media-type") != Some("application/xhtml+xml") {
                    return Err(
                        "unsupported EPUB spine media type; only XHTML text is extracted".into(),
                    );
                }
                let part = resolve(&path, item.attr("href").ok_or("missing spine href")?)?;
                let content = xml::parse_xhtml(
                    &archive
                        .read(&part)?
                        .ok_or_else(|| format!("missing EPUB spine part {part}"))?,
                )?;
                if !content.is("http://www.w3.org/1999/xhtml", "html") {
                    return Err("invalid EPUB XHTML root".into());
                }
                let mut text = String::new();
                visible(&content, &mut text);
                report.native_units.push(crate::selection::text_unit(
                    number,
                    crate::reports::NativeUnit::SpineItem,
                    Some(part),
                    text,
                    crate::reports::ContentOrigin::Native,
                ));
                Ok(())
            })();
            if let Err(error) = result {
                report.error("epub_text", format!("spine item {number}: {error}"));
            }
        }
    } else {
        let partial = references.len() > options.text_unit_limit();
        for reference in references.into_iter().take(options.text_unit_limit()) {
            let result = (|| -> xml::Result<()> {
                let item = items
                    .get(reference.attr("idref").ok_or("spine item has no idref")?)
                    .ok_or("spine references missing manifest item")?;
                if item.attr("media-type") != Some("application/xhtml+xml") {
                    return Err(
                        "unsupported EPUB spine media type; only XHTML text is extracted".into(),
                    );
                }
                let part = resolve(&path, item.attr("href").ok_or("missing spine href")?)?;
                let content = xml::parse_xhtml(
                    &archive
                        .read(&part)?
                        .ok_or_else(|| format!("missing EPUB spine part {part}"))?,
                )?;
                if !content.is("http://www.w3.org/1999/xhtml", "html") {
                    return Err("invalid EPUB XHTML root".into());
                }
                let mut text = String::new();
                visible(&content, &mut text);
                crate::markdown::xhtml(&content, report, options);
                report.append_text(&text, options.max_chars);
                Ok(())
            })();
            if let Err(error) = result {
                report.error("epub_text", error);
            }
        }
        report.text.truncated |= partial;
    }
    if options.thumbnail.is_some() {
        let cover = cover_id.and_then(|id| items.get(id).copied()).or_else(|| {
            items.values().copied().find(|n| {
                n.attr("properties")
                    .unwrap_or("")
                    .split_whitespace()
                    .any(|p| p == "cover-image")
            })
        });
        if let Some(cover) = cover {
            let result = (|| -> xml::Result<()> {
                let part = resolve(&path, cover.attr("href").ok_or("missing cover href")?)?;
                let bytes = archive.read(&part)?.ok_or("missing EPUB cover part")?;
                crate::thumbnail::write(std::io::Cursor::new(bytes), "embedded", options, report);
                Ok(())
            })();
            if let Err(error) = result {
                report.error("thumbnail", error);
            }
        }
    }
    Ok(())
}

/// Resolves an EPUB relative href against `base` after stripping fragments and percent-decoding.
///
/// Accepts the base package part path and raw href string. Returns the safe
/// archive-internal path or an error when decoding fails or the path escapes
/// the package root.
fn resolve(base: &str, target: &str) -> xml::Result<String> {
    let target = target.split('#').next().unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(target).decode_utf8()?;
    office::resolve_part(base, &decoded)
}

/// Appends visible XHTML text from `node` to `out`, preserving readable
/// boundary whitespace across block elements and table cells.
///
/// Accepts a parsed XHTML element node and mutable destination string. Returns
/// unit after appending visible descendant text while skipping `head`,
/// `script`, and `style` subtrees. Emits a trailing `\n` after block elements
/// (when `out` is non-empty and does not already end with `\n`) and `\t` after
/// `td`/`th` cells (when `out` is non-empty and does not already end with
/// `\n`, `\t`, or `' '`). Performs no I/O and cannot fail.
fn visible(node: &Node, out: &mut String) {
    if matches!(node.name.as_str(), "head" | "script" | "style") {
        return;
    }
    for child in &node.content {
        match child {
            Content::Text(t) => out.push_str(t),
            Content::Element(n) => visible(n, out),
        }
    }
    if matches!(
        node.name.as_str(),
        "p" | "div"
            | "br"
            | "li"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "nav"
            | "aside"
            | "blockquote"
            | "pre"
            | "tr"
            | "dt"
            | "dd"
            | "figure"
            | "figcaption"
            | "hr"
    ) && !out.is_empty()
        && !out.ends_with('\n')
    {
        if node.name == "tr" && out.ends_with('\t') {
            out.pop();
        }
        out.push('\n');
    } else if matches!(node.name.as_str(), "td" | "th")
        && !out.is_empty()
        && !out.ends_with(['\n', '\t', ' '])
    {
        out.push('\t');
    }
}

//! Module: glassbox-inspect OOXML (WordprocessingML/PresentationML) reader.
//! File: office.rs.
//! Responsibility: Identify OOXML packages, read container metadata, and
//! extract native units with boundary-aware, budget-bounded text ownership.
use crate::{
    Claim, InspectOptions, InspectReport, Preflight,
    archive::Archive,
    detect,
    xml::{self, Content, Node},
};

pub(crate) const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
pub(crate) const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
pub(crate) const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
pub(crate) const S: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
pub(crate) const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
pub(crate) const PKG: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const DC: &str = "http://purl.org/dc/elements/1.1/";

/// Maximum characters of one native unit's assembled text before clipping.
const UNIT_TEXT_CHAR_CAP: usize = 65_536;
/// Maximum nested Word table depth inside one unit's text assembly; deeper
/// nested tables are skipped and the clip is reported.
const WORD_TABLE_NESTING_CAP: usize = 16;
/// Maximum number of native units expanded from one DOCX body.
const DOCX_UNIT_CAP: usize = 4096;
/// Maximum wrapper nesting depth traversed inside a DOCX `<w:body>` or table.
const DOCX_WRAPPER_DEPTH_CAP: usize = 32;
/// Explicit partial diagnostic appended when unit text assembly clips.
const UNIT_TEXT_TRUNCATION_MARKER: &str = "\n[unit text truncated]";

pub(crate) fn read_xml(archive: &mut Archive, name: &str) -> xml::Result<Node> {
    xml::parse(
        &archive
            .read(name)?
            .ok_or_else(|| format!("missing package part {name}"))?,
    )
}

pub(crate) fn preflight(
    report: &mut InspectReport,
    kind: &'static str,
    index: usize,
    chars: usize,
    images: bool,
    options: &InspectOptions,
) {
    if index >= options.max_units {
        return;
    }
    let mut reasons = vec![];
    if chars < 20 {
        reasons.push("text_length_below_20".into());
    }
    if images {
        reasons.push("has_images".into());
    }
    report.preflight.push(Preflight {
        page_number: index + 1,
        unit_kind: kind,
        needs_ocr: !reasons.is_empty(),
        reasons,
        confidence: "deterministic",
        coverage_available: false,
    });
}

pub(crate) fn identify(report: &mut InspectReport, ext: &str, mime: &str, name: &str) {
    report.format.detected_mime = Some(mime.into());
    report.format.detected_name = Some(name.into());
    report.format.bucket = detect::bucket(ext).into();
    report.format.basis = "bytes".into();
    report.format.disagreement =
        !report.format.extension.is_empty() && report.format.extension != ext;
    report.format.detector = "file-format+package-xml".into();
}

pub(crate) fn metadata(archive: &mut Archive, report: &mut InspectReport) {
    for name in ["docProps/core.xml", "docProps/app.xml", "meta.xml"] {
        if !archive.names.iter().any(|n| n == name) {
            continue;
        }
        match read_xml(archive, name) {
            Ok(root) => {
                let expected = match name {
                    "docProps/core.xml" => (
                        "http://schemas.openxmlformats.org/package/2006/metadata/core-properties",
                        "coreProperties",
                    ),
                    "docProps/app.xml" => (
                        "http://schemas.openxmlformats.org/officeDocument/2006/extended-properties",
                        "Properties",
                    ),
                    _ => (crate::odf::O, "document-meta"),
                };
                if !root.is(expected.0, expected.1) {
                    report.error("metadata", format!("{name}: namespace/root mismatch"));
                    continue;
                }
                root.visit(&mut |node| {
                    if ![
                        expected.0,
                        DC,
                        "http://purl.org/dc/terms/",
                        "urn:oasis:names:tc:opendocument:xmlns:meta:1.0",
                    ]
                    .contains(&node.ns.as_str())
                    {
                        return;
                    }
                    if node.name == "language" && node.ns != DC {
                        return;
                    }
                    if node.children().next().is_some() {
                        return;
                    }
                    let value = node.text();
                    if value.trim().is_empty() {
                        return;
                    }
                    let key = if node.ns == DC && node.name == "creator" {
                        "author"
                    } else {
                        node.name.as_str()
                    };
                    report.claims.push(Claim {
                        key: key.into(),
                        value,
                        source: "container_metadata",
                    });
                });
            }
            Err(e) => report.error("metadata", format!("{name}: {e}")),
        }
    }
}

/// Inspects an OpenDocument or OOXML package inside an open ZIP archive.
///
/// Accepts an open archive, mutable destination report, and extraction options.
/// Returns `true` when ODF or OOXML recognized the archive (including when a
/// recognized package fails and records an `"office"` error so downstream ZIP
/// readers do not run), and `false` when neither ODF nor OOXML matched.
pub(crate) fn inspect(
    archive: &mut Archive,
    report: &mut InspectReport,
    options: &InspectOptions,
) -> bool {
    match inspect_inner(archive, report, options) {
        Ok(handled) => handled,
        Err(error) => {
            report.error("office", error);
            true
        }
    }
}

/// Dispatches ODF (`mimetype`) and OOXML (`[Content_Types].xml`) package parsing.
///
/// Accepts an open archive, mutable destination report, and extraction options.
/// Returns `Ok(true)` when ODF or OOXML recognized and inspected the archive,
/// `Ok(false)` when neither ODF nor OOXML matched, or an error when reading or
/// validating a recognized Office package fails.
fn inspect_inner(
    archive: &mut Archive,
    report: &mut InspectReport,
    options: &InspectOptions,
) -> xml::Result<bool> {
    if archive.names.iter().any(|n| n == "mimetype")
        && crate::odf::inspect(archive, report, options)?
    {
        return Ok(true);
    }
    if !archive.names.iter().any(|n| n == "[Content_Types].xml") {
        return Ok(false);
    }
    let types = read_xml(archive, "[Content_Types].xml")?;
    if !types.is(CT, "Types") {
        return Err("invalid OOXML content types root".into());
    }
    let mut main = None;
    for node in types.children().filter(|n| n.is(CT, "Override")) {
        let content_type = node.attr("ContentType").unwrap_or("");
        let kind = match content_type {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" => {
                Some(("docx", W, "document"))
            }
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml" => {
                Some(("pptx", P, "presentation"))
            }
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml" => {
                Some(("xlsx", S, "workbook"))
            }
            "application/vnd.ms-excel.sheet.macroEnabled.main+xml" => Some(("xlsm", S, "workbook")),
            _ => None,
        };
        if let Some(kind) = kind {
            if main.is_some() {
                return Err("ambiguous OOXML main parts".into());
            }
            main = Some((
                kind,
                node.attr("PartName")
                    .ok_or("main part has no name")?
                    .trim_start_matches('/')
                    .to_string(),
            ));
        }
    }
    let Some(((ext, ns, root_name), part)) = main else {
        return Ok(false);
    };
    if part.split('/').any(|p| matches!(p, ".." | "." | "")) || part.contains('\\') {
        return Err("unsafe OOXML main part name".into());
    }
    let mut root = read_xml(archive, &part)?;
    if !root.is(ns, root_name) {
        return Err("OOXML main part namespace/root mismatch".into());
    }
    match ext {
        "docx" => {
            identify(
                report,
                ext,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "Word document",
            );
            metadata(archive, report);
            let paragraphs = root.descendants(W, "p").len();
            if let Some(selection) = &options.native_selection {
                docx_units(&root, &part, report, selection)?;
            } else {
                let partial = limit_word_sections(&mut root, options.text_unit_limit());
                docx(&root, report, options);
                crate::tables::Context::new(report, &part, "section", 1, options)
                    .word_tables(&root);
                crate::markdown::word(&root, report, options);
                report.text.truncated |= partial;
            }
            report.counts.paragraphs = Some(paragraphs);
        }
        "pptx" => {
            identify(
                report,
                ext,
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "PowerPoint presentation",
            );
            metadata(archive, report);
            pptx(archive, &root, &part, report, options)?;
        }
        "xlsx" | "xlsm" => {
            identify(
                report,
                ext,
                if ext == "xlsx" {
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                } else {
                    "application/vnd.ms-excel.sheet.macroEnabled.12"
                },
                "Excel workbook",
            );
            metadata(archive, report);
            crate::spreadsheet::xlsx(archive, &root, &part, report, options)?;
        }
        _ => {}
    }
    Ok(true)
}

pub(crate) fn resolve_part(base: &str, target: &str) -> xml::Result<String> {
    if target.contains(['\\', ':', '?', '#', '\0']) {
        return Err("external or invalid package relationship target".into());
    }
    let mut components: Vec<&str> = if target.starts_with('/') {
        vec![]
    } else {
        base.rsplit_once('/')
            .map(|(dir, _)| dir.split('/').collect())
            .unwrap_or_default()
    };
    for component in target.trim_start_matches('/').split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components
                    .pop()
                    .ok_or("relationship escapes package root")?;
            }
            _ => components.push(component),
        }
    }
    if components.is_empty() {
        return Err("empty relationship target".into());
    }
    Ok(components.join("/"))
}

pub(crate) fn relationships(archive: &mut Archive, part: &str) -> xml::Result<Node> {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let name = if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}/_rels/{file}.rels")
    };
    let root = read_xml(archive, &name)?;
    if !root.is(PKG, "Relationships") {
        return Err("invalid package relationships root".into());
    }
    Ok(root)
}

pub(crate) fn related(rels: &Node, part: &str, id: &str, kind: &str) -> xml::Result<String> {
    let found: Vec<_> = rels
        .children()
        .filter(|n| n.is(PKG, "Relationship") && n.attr("Id") == Some(id))
        .collect();
    if found.len() != 1 {
        return Err("missing or duplicate relationship id".into());
    }
    let rel = found[0];
    if rel.attr("TargetMode").is_some_and(|m| m != "Internal")
        || rel.attr("Type") != Some(&format!("{R}/{kind}"))
    {
        return Err("external or mismatched package relationship".into());
    }
    resolve_part(
        part,
        rel.attr("Target").ok_or("relationship missing target")?,
    )
}

/// Assembles one slide's owned text under the unit budget.
///
/// Accepts a parsed `p:sld` root. Returns the bounded unit text (one line per
/// paragraph, trailing newline included), the slide's total owned character
/// count before clipping, and whether the unit budget clipped. Every `a:p`
/// under the slide — including table-cell, grouped-shape, and nested
/// paragraphs — is enumerated exactly once and streams its owned fragments
/// through the shared boundary-aware walker exactly once, so nesting never
/// multiplies slide text and no full-size per-paragraph buffers exist.
fn slide_text(slide: &Node) -> (String, usize, bool) {
    let mut text = String::new();
    let mut chars = 0;
    let mut budget = UnitTextBudget::new();
    for paragraph in slide.descendants(A, "p") {
        walk_owned_text(paragraph, TextFamily::Drawing, &mut |piece| {
            chars += piece.chars().count();
            budget.push(&mut text, piece);
        });
        budget.push(&mut text, "\n");
    }
    (text, chars, budget.truncated)
}

/// Extracts selected PPTX slides in presentation relationship order.
///
/// Accepts an archive, presentation root and part, report, and controls. Returns
/// unit after selected slide content. Invalid relationships and unsatisfiable
/// selections fail; an individual slide failure remains a partial diagnostic.
/// Every owned fragment of a slide is emitted exactly once in document order
/// through the shared boundary-aware walker. Each slide unit's text assembly
/// is bounded; a clip appends the explicit truncation marker and downgrades
/// that unit's completeness to partial.
fn pptx(
    archive: &mut Archive,
    root: &Node,
    part: &str,
    report: &mut InspectReport,
    options: &InspectOptions,
) -> xml::Result<()> {
    let slides = root.descendants(P, "sldId");
    report.counts.slides = Some(slides.len());
    let rels = relationships(archive, part)?;
    let plan = match &options.native_selection {
        Some(selection) => Some(crate::selection::begin(
            report,
            selection,
            crate::reports::NativeUnit::Slide,
            slides.len() as u64,
        )?),
        None => None,
    };
    for (i, slide) in slides.iter().enumerate() {
        if let Some(plan) = &plan {
            if !plan.contains(i as u64 + 1) {
                continue;
            }
        } else if i >= options.text_unit_limit() {
            break;
        }
        let result = (|| -> xml::Result<()> {
            let target = related(
                &rels,
                part,
                slide
                    .attr_ns(R, "id")
                    .ok_or("slide has no relationship id")?,
                "slide",
            )?;
            let slide = read_xml(archive, &target)?;
            if !slide.is(P, "sld") {
                return Err("invalid slide namespace/root".into());
            }
            if plan.is_none() {
                let table_start = report.tables.len();
                crate::tables::Context::new(report, &target, "slide", i + 1, options)
                    .drawing_tables(&slide);
                crate::markdown::append(
                    report,
                    &format!("## Slide {}\n\n", i + 1),
                    options,
                    "native_structure",
                );
                crate::markdown::drawing(&slide, report, options, table_start);
            }
            let (mut text, chars, truncated) = slide_text(&slide);
            if truncated {
                text.push_str(UNIT_TEXT_TRUNCATION_MARKER);
            }
            if plan.is_some() {
                let mut unit = crate::selection::text_unit(
                    i as u64 + 1,
                    crate::reports::NativeUnit::Slide,
                    Some(target),
                    text,
                    crate::reports::ContentOrigin::Native,
                );
                if truncated {
                    unit.completeness = crate::reports::Completeness::Partial;
                }
                report.native_units.push(unit);
            } else {
                report.append_text(&text, options.max_chars);
                report.text.truncated |= truncated;
                preflight(
                    report,
                    "slide",
                    i,
                    chars,
                    !slide.descendants(A, "blip").is_empty()
                        || !slide.descendants(P, "pic").is_empty(),
                    options,
                );
            }
            Ok(())
        })();
        if let Err(e) = result {
            report.error("slide", format!("slide {}: {e}", i + 1));
        }
    }
    if plan.is_none() {
        report.text.truncated |= slides.len() > options.text_unit_limit();
    }
    Ok(())
}

/// One source-order DOCX body unit assembled before native selection filtering.
struct DocxPlannedUnit {
    unit: crate::reports::NativeUnit,
    text: Option<String>,
    partial: bool,
}

/// Appends one planned DOCX unit while enforcing [`DOCX_UNIT_CAP`].
///
/// Accepts the destination vector and planned unit. Returns `Ok(())` when the
/// unit fits within the cap, or an error when adding the unit would exceed
/// `DOCX_UNIT_CAP`. Performs no I/O.
fn push_docx_unit(planned: &mut Vec<DocxPlannedUnit>, unit: DocxPlannedUnit) -> xml::Result<()> {
    if planned.len() >= DOCX_UNIT_CAP {
        return Err("DOCX native block cap exceeded".into());
    }
    planned.push(unit);
    Ok(())
}

/// Reports whether `node` is an explicit WordprocessingML page break (`<w:br w:type="page"/>`).
///
/// Accepts a parsed XML element node. Returns `true` only for `w:br` elements
/// whose `w:type` (or unprefixed `type`) attribute equals `"page"`. Soft
/// `<w:lastRenderedPageBreak/>`, plain `<w:br/>`, and non-page break types
/// return `false`. Performs no I/O and cannot fail.
fn is_word_page_break(node: &Node) -> bool {
    node.is(W, "br") && node.attr_ns(W, "type").or_else(|| node.attr("type")) == Some("page")
}

/// Reports whether `node` is WordprocessingML control metadata, section
/// properties, or deleted/moved-from markup excluded from body text.
///
/// Accepts a parsed XML element node. Returns `true` for `w:sdtPr`,
/// `w:sdtEndPr`, `w:sectPr`, `w:del`, and `w:moveFrom`. Performs no I/O and
/// cannot fail.
fn is_word_skipped_metadata(node: &Node) -> bool {
    node.ns == W
        && matches!(
            node.name.as_str(),
            "sdtPr" | "sdtEndPr" | "sectPr" | "del" | "moveFrom"
        )
}

/// Reports whether `node` is a supported WordprocessingML content-bearing
/// wrapper whose children belong to the surrounding body or table flow.
///
/// Accepts a parsed XML element node. Returns `true` for `w:sdt`,
/// `w:sdtContent`, `w:ins`, `w:moveTo`, `w:smartTag`, `w:customXml`, and
/// `w:hyperlink`. Performs no I/O and cannot fail.
fn is_word_content_wrapper(node: &Node) -> bool {
    node.ns == W
        && matches!(
            node.name.as_str(),
            "sdt" | "sdtContent" | "ins" | "moveTo" | "smartTag" | "customXml" | "hyperlink"
        )
}

/// Reports whether `node` contains any unextracted WordprocessingML block or
/// non-empty `w:t` text node beneath it.
///
/// Accepts a parsed XML element node. Returns `true` when `node` or any
/// descendant is `w:p`, `w:tbl`, or a `w:t` containing non-whitespace text.
/// Performs no I/O and cannot fail.
fn contains_word_content(node: &Node) -> bool {
    let mut found = false;
    node.visit(&mut |descendant| {
        if found {
            return;
        }
        if descendant.is(W, "p")
            || descendant.is(W, "tbl")
            || (descendant.is(W, "t") && !descendant.text().trim().is_empty())
        {
            found = true;
        }
    });
    found
}

/// Collects DOCX body `<w:p>` and `<w:tbl>` blocks in exact document order
/// across supported content-bearing wrappers.
///
/// Accepts a container node (`w:body` or a supported wrapper), destination
/// vector, current wrapper depth, and a mutable flag set to `true` whenever an
/// unsupported wrapper or depth overflow hides nested `<w:p>`, `<w:tbl>`, or
/// non-empty `<w:t>` content. Returns `Ok(())` when within [`DOCX_UNIT_CAP`],
/// or an error if adding a block would exceed [`DOCX_UNIT_CAP`]. Control and
/// section metadata (`w:sdtPr`, `w:sdtEndPr`, `w:sectPr`) and deleted or
/// moved-from markup (`w:del`, `w:moveFrom`) are skipped intentionally.
fn collect_docx_blocks<'a>(
    node: &'a Node,
    blocks: &mut Vec<&'a Node>,
    depth: usize,
    skipped_content: &mut bool,
) -> xml::Result<()> {
    for child in node.children() {
        if child.is(W, "p") || child.is(W, "tbl") {
            if blocks.len() >= DOCX_UNIT_CAP {
                return Err("DOCX native block cap exceeded".into());
            }
            blocks.push(child);
        } else if is_word_skipped_metadata(child) {
            // Control properties, section properties, and deleted/moved-from
            // ranges are excluded from active body text.
        } else if is_word_content_wrapper(child) {
            if depth >= DOCX_WRAPPER_DEPTH_CAP {
                if contains_word_content(child) {
                    *skipped_content = true;
                }
            } else {
                collect_docx_blocks(child, blocks, depth + 1, skipped_content)?;
            }
        } else if contains_word_content(child) {
            *skipped_content = true;
        }
    }
    Ok(())
}

/// Accumulates owned text fragments for one DOCX paragraph and splits it at
/// explicit `<w:br w:type="page"/>` elements.
struct WordParagraphSplitter<'a> {
    planned: &'a mut Vec<DocxPlannedUnit>,
    text: String,
    budget: UnitTextBudget,
    saw_page_break: bool,
}

impl<'a> WordParagraphSplitter<'a> {
    /// Creates a splitter targeting `planned` with a fresh per-unit character budget.
    ///
    /// Accepts the mutable destination vector. Returns an empty splitter with
    /// no page breaks observed. Performs no I/O and cannot fail.
    fn new(planned: &'a mut Vec<DocxPlannedUnit>) -> Self {
        Self {
            planned,
            text: String::new(),
            budget: UnitTextBudget::new(),
            saw_page_break: false,
        }
    }

    /// Walks `node` in document order, splitting at explicit page breaks while
    /// ignoring `<w:lastRenderedPageBreak/>`, skipping control/deleted metadata,
    /// traversing inline content wrappers (`w:sdt`/`w:sdtContent`/`w:ins`), and
    /// skipping nested block boundaries.
    ///
    /// Accepts a parsed XML node and whether direct character data is owned by
    /// an enclosing `w:t` element. Returns `Ok(())` on completion, or an error
    /// if emitting a split unit exceeds [`DOCX_UNIT_CAP`]. Performs no I/O.
    fn walk(&mut self, node: &Node, owns_text: bool) -> xml::Result<()> {
        for content in &node.content {
            match content {
                Content::Text(text) if owns_text => self.budget.push(&mut self.text, text),
                Content::Element(child) => {
                    if child.is(W, "t") {
                        self.walk(child, true)?;
                    } else if child.is(W, "tab") {
                        self.budget.push(&mut self.text, "\t");
                    } else if is_word_page_break(child) {
                        self.emit_page_break()?;
                    } else if child.is(W, "br") || child.is(W, "cr") {
                        self.budget.push(&mut self.text, "\n");
                    } else if child.is(W, "lastRenderedPageBreak")
                        || is_word_skipped_metadata(child)
                    {
                        // Soft rendered page breaks, control metadata, and
                        // deleted/moved-from markup do not emit active text.
                    } else if child.is(W, "p") || child.is(W, "tbl") {
                        self.budget.truncated = true;
                    } else {
                        self.walk(child, false)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Flushes any non-empty text accumulated before an explicit page break and
    /// appends a [`crate::reports::NativeUnit::PageBreak`] unit.
    ///
    /// Accepts `&mut self`. Returns `Ok(())` on success, or an error when
    /// appending exceeds [`DOCX_UNIT_CAP`]. Performs no I/O.
    fn emit_page_break(&mut self) -> xml::Result<()> {
        self.saw_page_break = true;
        if !self.text.trim().is_empty() || self.budget.truncated {
            let mut flushed = std::mem::take(&mut self.text);
            if self.budget.truncated {
                flushed.push_str(UNIT_TEXT_TRUNCATION_MARKER);
            }
            push_docx_unit(
                self.planned,
                DocxPlannedUnit {
                    unit: crate::reports::NativeUnit::DocumentBlock,
                    text: Some(flushed),
                    partial: self.budget.truncated,
                },
            )?;
        } else {
            self.text.clear();
        }
        self.budget = UnitTextBudget::new();
        push_docx_unit(
            self.planned,
            DocxPlannedUnit {
                unit: crate::reports::NativeUnit::PageBreak,
                text: None,
                partial: false,
            },
        )
    }

    /// Flushes remaining paragraph text after walking the entire `<w:p>` node.
    ///
    /// Accepts `self`. Returns `Ok(())` on success, or an error when appending
    /// exceeds [`DOCX_UNIT_CAP`]. Paragraphs without explicit page breaks always
    /// emit one block unit; paragraphs split by an explicit page break emit a
    /// trailing block unit only when non-empty or clipped. Performs no I/O.
    fn finish(mut self) -> xml::Result<()> {
        if !self.saw_page_break || !self.text.trim().is_empty() || self.budget.truncated {
            if self.budget.truncated {
                self.text.push_str(UNIT_TEXT_TRUNCATION_MARKER);
            }
            push_docx_unit(
                self.planned,
                DocxPlannedUnit {
                    unit: crate::reports::NativeUnit::DocumentBlock,
                    text: Some(self.text),
                    partial: self.budget.truncated,
                },
            )?;
        }
        Ok(())
    }
}

/// Extracts selected DOCX body paragraphs, atomic tables, and explicit page breaks.
///
/// Accepts the document root, main part name, report, and native selection.
/// Returns unit after ordered body blocks (including blocks inside supported
/// content-bearing wrappers such as `w:sdt`/`w:sdtContent`, `w:ins`, `w:moveTo`,
/// `w:smartTag`, `w:customXml`, and `w:hyperlink`) are emitted: `<w:tbl>` nodes
/// emit [`crate::reports::NativeUnit::Table`], explicit `<w:br w:type="page"/>`
/// breaks split surrounding paragraph text and emit
/// [`crate::reports::NativeUnit::PageBreak`] (ignoring
/// `<w:lastRenderedPageBreak/>`), and ordinary paragraphs emit
/// [`crate::reports::NativeUnit::DocumentBlock`]. A missing body, excess unit
/// count, or unsatisfiable range returns an error; unsupported wrappers or
/// depth-capped wrappers that hide content set `report.text.truncated = true`
/// and emit an extraction diagnostic so omitted content never reports complete
/// exact extraction. Each unit's text assembly is bounded; a clip appends the
/// explicit truncation marker and downgrades that unit's completeness to partial.
fn docx_units(
    root: &Node,
    part: &str,
    report: &mut InspectReport,
    selection: &crate::reports::NativeSelection,
) -> xml::Result<()> {
    let body = root
        .children()
        .find(|node| node.is(W, "body"))
        .ok_or("DOCX document has no body")?;
    let mut blocks = Vec::new();
    let mut skipped_content = false;
    collect_docx_blocks(body, &mut blocks, 0, &mut skipped_content)?;
    if skipped_content {
        report.text.truncated = true;
        report.error(
            "office",
            "DOCX body contains unsupported or deeply nested wrapper elements with unextracted content",
        );
    }
    let mut planned = Vec::with_capacity(blocks.len());
    for block in blocks {
        if block.is(W, "p") {
            let mut splitter = WordParagraphSplitter::new(&mut planned);
            splitter.walk(block, false)?;
            splitter.finish()?;
        } else {
            let mut text = String::new();
            let mut budget = UnitTextBudget::new();
            budget.word_table(block, &mut text, 1);
            if budget.truncated {
                text.push_str(UNIT_TEXT_TRUNCATION_MARKER);
            }
            push_docx_unit(
                &mut planned,
                DocxPlannedUnit {
                    unit: crate::reports::NativeUnit::Table,
                    text: Some(text),
                    partial: budget.truncated,
                },
            )?;
        }
    }
    let plan = crate::selection::begin(
        report,
        selection,
        crate::reports::NativeUnit::DocumentBlock,
        planned.len() as u64,
    )?;
    let mut table_ordinal: u64 = 0;
    let mut page_break_ordinal: u64 = 0;
    for (index, item) in planned.into_iter().enumerate() {
        let number = index as u64 + 1;
        let marker_index = match item.unit {
            crate::reports::NativeUnit::Table => {
                table_ordinal += 1;
                Some(table_ordinal)
            }
            crate::reports::NativeUnit::PageBreak => {
                page_break_ordinal += 1;
                Some(page_break_ordinal)
            }
            _ => None,
        };
        if !plan.contains(number) {
            continue;
        }
        let mut unit = match item.text {
            Some(text) => crate::selection::text_unit(
                number,
                item.unit,
                Some(format!("{part}#body/{number}")),
                text,
                crate::reports::ContentOrigin::Native,
            ),
            None => crate::reports::ContentUnit {
                unit_index: number,
                unit: item.unit,
                source_part: Some(format!("{part}#body/{number}")),
                label: None,
                marker_index: None,
                text: None,
                rows: Vec::new(),
                provenance: vec![crate::reports::ExtractionProvenance {
                    reader: "native-reader".into(),
                    stage: "native-unit-extract".into(),
                    source: crate::reports::FactSource::NativeStructure,
                    origin: crate::reports::ContentOrigin::Native,
                    ocr: None,
                }],
                completeness: crate::reports::Completeness::Exact,
            },
        };
        unit.marker_index = marker_index;
        if item.partial {
            unit.completeness = crate::reports::Completeness::Partial;
        }
        report.native_units.push(unit);
    }
    Ok(())
}

/// The OOXML markup family whose run content one text walk owns.
#[derive(Clone, Copy)]
enum TextFamily {
    /// WordprocessingML runs: `w:t` character data, `w:tab`, `w:br`, `w:cr`.
    Word,
    /// DrawingML runs: `a:t` character data, `a:br`.
    Drawing,
}

/// Streams every text fragment one paragraph owns to `emit`, exactly once and
/// in document order.
///
/// Accepts the paragraph node (`w:p` or `a:p`), the family whose run elements
/// this walk owns, and a sink receiving each owned fragment. Returns `true`
/// when a nested `w:p`/`w:tbl`/`a:p`/`a:tbl` boundary was skipped, because
/// that content belongs to another enumerated unit. Each `w:t`/`a:t`
/// contributes only its direct character data, with nested text elements
/// emitting their own data in place, so nesting never re-attributes or
/// multiplies text. Tab and break elements keep their run positions, and
/// character data outside text elements is not owned by the paragraph.
fn walk_owned_text(node: &Node, family: TextFamily, emit: &mut dyn FnMut(&str)) -> bool {
    walk_owned_content(node, family, false, emit)
}

/// Walks one node's content for [`walk_owned_text`]; `owns_text` is set only
/// for text elements, whose direct character data is interleaved with nested
/// elements in the same single pass.
fn walk_owned_content(
    node: &Node,
    family: TextFamily,
    owns_text: bool,
    emit: &mut dyn FnMut(&str),
) -> bool {
    let mut skipped_boundary = false;
    for content in &node.content {
        match content {
            Content::Text(text) if owns_text => emit(text),
            Content::Element(child) => {
                let text_element = match family {
                    TextFamily::Word => child.is(W, "t"),
                    TextFamily::Drawing => child.is(A, "t"),
                };
                if text_element {
                    skipped_boundary |= walk_owned_content(child, family, true, emit);
                    continue;
                }
                match family {
                    TextFamily::Word => {
                        if child.is(W, "tab") {
                            emit("\t");
                        } else if child.is(W, "br") || child.is(W, "cr") {
                            emit("\n");
                        } else if child.is(W, "p") || child.is(W, "tbl") {
                            skipped_boundary = true;
                        } else if is_word_skipped_metadata(child) {
                            // Control properties and deleted/moved-from runs are excluded.
                        } else {
                            skipped_boundary |= walk_owned_content(child, family, false, emit);
                        }
                    }
                    TextFamily::Drawing => {
                        if child.is(A, "br") {
                            emit("\n");
                        } else if child.is(A, "p") || child.is(A, "tbl") {
                            skipped_boundary = true;
                        } else {
                            skipped_boundary |= walk_owned_content(child, family, false, emit);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    skipped_boundary
}

/// Shared character budget for one native unit's text assembly. Text is
/// clipped at the cap and every clip is reported, so a unit never retains
/// more than the cap plus the explicit truncation marker.
struct UnitTextBudget {
    remaining: usize,
    truncated: bool,
}

impl UnitTextBudget {
    /// Production construction pins the per-unit cap at its design constant.
    fn new() -> Self {
        Self {
            remaining: UNIT_TEXT_CHAR_CAP,
            truncated: false,
        }
    }

    /// Reduced-limit seam for internal unit tests only; production limits are
    /// unreachable at test scale, never weakened at runtime.
    #[cfg(test)]
    fn with_remaining(remaining: usize) -> Self {
        Self {
            remaining,
            truncated: false,
        }
    }

    /// Appends `piece` while budget remains, recording any clip.
    fn push(&mut self, out: &mut String, piece: &str) {
        let chars = piece.chars().count();
        if chars > self.remaining {
            self.truncated = true;
        }
        let take = chars.min(self.remaining);
        out.extend(piece.chars().take(take));
        self.remaining -= take;
    }

    /// Appends one Word paragraph's owned fragments — direct `w:t` character
    /// data plus `w:tab` and `w:br`/`w:cr` — in run order within the budget.
    /// The walk stops at nested `w:p`/`w:tbl` boundaries: skipping one inside
    /// this paragraph records a clip, because that content belongs to another
    /// unit and must never be multiplied into this unit's text.
    fn paragraph(&mut self, paragraph: &Node, out: &mut String) {
        let skipped = walk_owned_text(paragraph, TextFamily::Word, &mut |piece| {
            self.push(out, piece)
        });
        if skipped {
            self.truncated = true;
        }
    }

    /// Appends one Word table as rows and tab-separated cells. Rows and cells
    /// are collected across direct children and supported content-bearing
    /// wrappers without crossing nested table boundaries, so each nested table
    /// is rendered exactly once inside its parent cell.
    fn word_table(&mut self, table: &Node, out: &mut String, depth: usize) {
        let mut rows = Vec::new();
        collect_word_children(table, "tr", &mut rows, 0, &mut self.truncated);
        for (row_index, row) in rows.into_iter().enumerate() {
            if row_index > 0 {
                self.push(out, "\n");
            }
            let mut cells = Vec::new();
            collect_word_children(row, "tc", &mut cells, 0, &mut self.truncated);
            for (cell_index, cell) in cells.into_iter().enumerate() {
                if cell_index > 0 {
                    self.push(out, "\t");
                }
                self.word_cell(cell, out, depth);
            }
        }
    }

    /// Appends one Word cell's block content in document order: direct
    /// paragraphs and nested tables bounded by the nesting cap.
    fn word_cell(&mut self, cell: &Node, out: &mut String, depth: usize) {
        let mut blocks = Vec::new();
        collect_word_blocks(cell, &mut blocks);
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                self.push(out, "\n");
            }
            if block.is(W, "p") {
                self.paragraph(block, out);
            } else if depth >= WORD_TABLE_NESTING_CAP {
                self.truncated = true;
            } else {
                self.word_table(block, out, depth + 1);
            }
        }
    }
}

/// Collects direct or wrapper-enclosed `w:<target>` children (such as `tr`
/// inside `tbl` or `tc` inside `tr`) without crossing nested table or cell
/// boundaries.
///
/// Accepts a parent node, target local name, destination vector, current
/// wrapper depth, and truncation flag set when wrapper nesting exceeds
/// [`DOCX_WRAPPER_DEPTH_CAP`]. Performs no I/O and cannot fail.
fn collect_word_children<'a>(
    node: &'a Node,
    target: &str,
    out: &mut Vec<&'a Node>,
    depth: usize,
    truncated: &mut bool,
) {
    for child in node.children() {
        if child.is(W, target) {
            out.push(child);
        } else if is_word_skipped_metadata(child) {
            // Control metadata and deleted ranges are excluded.
        } else if is_word_content_wrapper(child) {
            if depth >= DOCX_WRAPPER_DEPTH_CAP {
                *truncated = true;
            } else {
                collect_word_children(child, target, out, depth + 1, truncated);
            }
        }
    }
}

/// Collects a Word cell's block-level `w:p`/`w:tbl` content in document order
/// without crossing nested-table boundaries: recursion stops at paragraphs,
/// tables, and excluded control/deleted metadata, so nested table content
/// belongs to its own table alone.
fn collect_word_blocks<'a>(node: &'a Node, blocks: &mut Vec<&'a Node>) {
    for child in node.children() {
        if child.is(W, "p") || child.is(W, "tbl") {
            blocks.push(child);
        } else if is_word_skipped_metadata(child) {
            // Control metadata and deleted ranges are excluded.
        } else {
            collect_word_blocks(child, blocks);
        }
    }
}

/// Drops body content beyond the first `limit` Word sections in place.
///
/// Accepts the mutable document root and a section limit. Returns whether
/// meaningful content was clipped, so callers can report a partial. A
/// final `w:sectPr` marker is never counted as clipped content.
fn limit_word_sections(root: &mut Node, limit: usize) -> bool {
    if limit == usize::MAX {
        return false;
    }
    let Some(body) = root.content.iter_mut().find_map(|item| match item {
        Content::Element(node) if node.is(W, "body") => Some(node),
        _ => None,
    }) else {
        return false;
    };
    let mut sections = 0;
    let mut partial = false;
    body.content.retain(|item| {
        if sections >= limit {
            partial |= match item {
                Content::Element(node) => !node.is(W, "sectPr"),
                Content::Text(text) => !text.trim().is_empty(),
            };
            return false;
        }
        if let Content::Element(node) = item {
            sections += node.descendants(W, "sectPr").len();
        }
        true
    });
    partial
}

/// Extracts the legacy Word text body. Every paragraph in the document —
/// including table-cell and nested paragraphs — is enumerated once, and each
/// streams its owned fragments directly into the report's character budget
/// through the shared boundary-aware walker, so nesting never multiplies text
/// and no full-size per-paragraph buffers exist.
///
/// Accepts the document root, report, and controls. Returns unit after
/// section preflight rows are emitted; paragraph lines keep run order, tabs,
/// and breaks. Paragraphs with images flag their section for OCR preflight.
fn docx(root: &Node, report: &mut InspectReport, options: &InspectOptions) {
    let paragraphs = root.descendants(W, "p");
    report.counts.paragraphs = Some(paragraphs.len());
    let (mut chars, mut images, mut unit) = (0, false, 0);
    for paragraph in paragraphs {
        walk_owned_text(paragraph, TextFamily::Word, &mut |piece| {
            chars += piece.chars().count();
            report.append_text(piece, options.max_chars);
        });
        report.append_text("\n", options.max_chars);
        images |= !paragraph.descendants(A, "blip").is_empty()
            || !paragraph.descendants(W, "pict").is_empty()
            || !paragraph.descendants(W, "drawing").is_empty();
        if !paragraph.descendants(W, "sectPr").is_empty() {
            preflight(report, "section", unit, chars, images, options);
            unit += 1;
            chars = 0;
            images = false;
        }
    }
    if chars > 0 || images || unit == 0 {
        preflight(report, "section", unit, chars, images, options);
    }
}

pub(crate) fn language(report: &InspectReport) -> &'static str {
    let mut language = None;
    for claim in report
        .claims
        .iter()
        .filter(|c| c.key == "language" && c.source == "container_metadata")
    {
        let code = claim.value.to_ascii_lowercase();
        let code = code.split(['-', '_']).next().unwrap_or("");
        let bucket = match code {
            "en" | "eng" => "English",
            "zh" | "zho" | "chi" | "yue" => "Chinese",
            "fr" | "fra" | "fre" => "French",
            "de" | "deu" | "ger" => "German",
            "es" | "spa" => "Spanish",
            "nl" | "it" | "ru" | "pl" | "el" | "sv" | "da" | "no" | "fi" | "hu" | "pt" | "uk"
            | "cs" | "ro" | "bg" | "hr" | "sk" | "sl" | "et" | "lv" | "lt" | "is" | "ga" | "cy" => {
                "Other European"
            }
            "ar" | "he" | "fa" | "tr" | "ara" | "heb" | "fas" | "tur" => "Middle Eastern",
            "hi" | "ur" | "bn" | "ta" | "te" => "South Asian",
            "ja" | "ko" | "vi" | "id" | "tl" | "fil" | "jpn" | "kor" => "East & SE Asian",
            "sw" | "yo" | "qu" | "eu" | "ht" => "Other / Indigenous",
            _ => "N/A",
        };
        if language.is_some_and(|l| l != bucket) {
            return "N/A";
        }
        language = Some(bucket);
    }
    language.unwrap_or("N/A")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a WordprocessingML fragment under the real `w:` namespace.
    fn word_root(inner: &str) -> Node {
        crate::xml::parse(format!("<w:document xmlns:w=\"{W}\">{inner}</w:document>").as_bytes())
            .expect("test fixture must parse")
    }

    /// One paragraph fixture.
    fn p(text: &str) -> String {
        format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
    }

    /// Renders a table node through the bounded unit-text assembly.
    fn table_text(root: &Node) -> (String, bool) {
        let table = root
            .children()
            .find(|node| node.is(W, "tbl"))
            .expect("fixture must contain a table");
        let mut text = String::new();
        let mut budget = UnitTextBudget::new();
        budget.word_table(table, &mut text, 1);
        (text, budget.truncated)
    }

    /// Ordinary single-level tables keep the exact row/cell/paragraph text
    /// format of the previous unbounded renderer.
    #[test]
    fn simple_word_table_keeps_row_cell_text_format() {
        let root = word_root(&format!(
            "<w:tbl><w:tr><w:tc>{}{}</w:tc><w:tc>{}</w:tc></w:tr>\
             <w:tr><w:tc>{}</w:tc></w:tr></w:tbl>",
            p("a"),
            p("b"),
            p("c"),
            p("d")
        ));
        let (text, truncated) = table_text(&root);
        assert_eq!(text, "a\nb\tc\nd");
        assert!(!truncated);
    }

    /// Nested table text is rendered exactly once inside its parent cell; it
    /// neither reappears as phantom parent rows nor repeats per nesting level.
    #[test]
    fn nested_word_table_text_appears_once_per_level() {
        let nested = format!(
            "<w:tbl><w:tr><w:tc>{}<w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl></w:tc></w:tr></w:tbl>",
            p("outer"),
            p("inner")
        );
        let root = word_root(&nested);
        let (text, truncated) = table_text(&root);
        assert_eq!(text, "outer\ninner");
        assert_eq!(text.matches("inner").count(), 1);
        assert_eq!(text.matches("outer").count(), 1);
        assert!(!truncated);
    }

    /// Deeply nested chains stay linear: each level's text appears once no
    /// matter how many ancestor levels enclose it.
    #[test]
    fn deeply_nested_word_tables_render_each_level_once() {
        let tbl2 = format!(
            "<w:tbl><w:tr><w:tc>{}{}</w:tc></w:tr></w:tbl>",
            p("l2"),
            p("deep")
        );
        let tbl1 = format!(
            "<w:tbl><w:tr><w:tc>{}{}</w:tc></w:tr></w:tbl>",
            p("l1"),
            tbl2
        );
        let outer = format!(
            "<w:tbl><w:tr><w:tc>{}{}</w:tc></w:tr></w:tbl>",
            p("l0"),
            tbl1
        );
        let root = word_root(&outer);
        let (text, truncated) = table_text(&root);
        assert_eq!(text, "l0\nl1\nl2\ndeep");
        for label in ["l0", "l1", "l2", "deep"] {
            assert_eq!(text.matches(label).count(), 1, "{label}");
        }
        assert!(!truncated);
    }

    /// The nesting cap skips tables beyond the boundary and reports the clip
    /// through the shared budget instead of failing the whole unit.
    #[test]
    fn word_table_nesting_cap_reports_partial_clip() {
        let mut inner = p("deep");
        for _ in 0..=WORD_TABLE_NESTING_CAP {
            inner = format!("<w:tbl><w:tr><w:tc>{inner}</w:tc></w:tr></w:tbl>");
        }
        let root = word_root(&inner);
        let (text, truncated) = table_text(&root);
        assert!(truncated);
        assert!(!text.contains("deep"));
    }

    /// The per-unit budget clips at the character boundary, records the clip,
    /// and never retains more than the budget allows.
    #[test]
    fn unit_text_budget_clips_and_reports_truncation() {
        let mut out = String::new();
        let mut budget = UnitTextBudget::with_remaining(3);
        budget.push(&mut out, "abcdef");
        assert_eq!(out, "abc");
        assert!(budget.truncated);
        budget.push(&mut out, "more");
        assert_eq!(out, "abc");
        let mut empty = String::new();
        let mut budget = UnitTextBudget::with_remaining(4);
        budget.push(&mut empty, "abcd");
        assert_eq!(empty, "abcd");
        assert!(!budget.truncated);
    }

    /// Parses a PresentationML slide fragment under the real `p:`/`a:`
    /// namespaces.
    fn slide_root(inner: &str) -> Node {
        crate::xml::parse(
            format!(
                "<p:sld xmlns:p=\"{P}\" xmlns:a=\"{A}\">\
                 <p:cSld><p:spTree>{inner}</p:spTree></p:cSld></p:sld>"
            )
            .as_bytes(),
        )
        .expect("test fixture must parse")
    }

    /// One slide's text keeps paragraph order, line breaks, and DrawingML
    /// table-cell paragraphs exactly once.
    #[test]
    fn slide_text_preserves_order_breaks_and_table_cells() {
        let slide = slide_root(concat!(
            "<p:sp><a:p><a:r><a:t>one</a:t></a:r><a:br/><a:r><a:t>two</a:t></a:r></a:p></p:sp>",
            "<p:graphicFrame><a:tbl><a:tr><a:tc><a:txBody>",
            "<a:p><a:r><a:t>cell</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl>",
            "</p:graphicFrame>",
        ));
        let (text, chars, truncated) = slide_text(&slide);
        assert_eq!(text, "one\ntwo\ncell\n");
        assert_eq!(chars, "one\ntwocell".chars().count());
        assert!(!truncated);
    }

    /// Nested `a:p` wrappers are enumerated as their own paragraphs; each
    /// emits only its own fragments, so the payload appears exactly once.
    #[test]
    fn slide_text_nested_paragraphs_emit_payload_once() {
        let mut inner = String::from("<a:r><a:t>payload</a:t></a:r>");
        for _ in 0..30 {
            inner = format!("<a:p>{inner}</a:p>");
        }
        let slide = slide_root(&inner);
        let (text, chars, truncated) = slide_text(&slide);
        assert_eq!(text, format!("{}payload\n", "\n".repeat(29)));
        assert_eq!(text.matches("payload").count(), 1);
        assert_eq!(chars, "payload".chars().count());
        assert!(!truncated);
    }

    /// Nested `a:t` wrappers each contribute only their direct character
    /// data, so the payload appears exactly once instead of once per level.
    #[test]
    fn slide_text_nested_text_elements_emit_payload_once() {
        let mut inner = String::from("payload");
        for _ in 0..30 {
            inner = format!("<a:t>{inner}</a:t>");
        }
        let slide = slide_root(&format!("<a:p><a:r>{inner}</a:r></a:p>"));
        let (text, chars, truncated) = slide_text(&slide);
        assert_eq!(text, "payload\n");
        assert_eq!(chars, "payload".chars().count());
        assert!(!truncated);
    }

    /// The unit budget clips at the exact Unicode character cap — even for
    /// multi-byte text — and reports the clip; one char below the cap keeps
    /// the full text and the trailing newline unclipped.
    #[test]
    fn slide_text_clips_at_exact_unicode_cap() {
        let over = "é".repeat(UNIT_TEXT_CHAR_CAP + 5);
        let slide = slide_root(&format!("<a:p><a:r><a:t>{over}</a:t></a:r></a:p>"));
        let (text, chars, truncated) = slide_text(&slide);
        assert!(truncated);
        assert_eq!(chars, UNIT_TEXT_CHAR_CAP + 5);
        assert_eq!(text.chars().count(), UNIT_TEXT_CHAR_CAP);
        assert_eq!(text.len(), 2 * UNIT_TEXT_CHAR_CAP);
        let exact = "é".repeat(UNIT_TEXT_CHAR_CAP - 1);
        let slide = slide_root(&format!("<a:p><a:r><a:t>{exact}</a:t></a:r></a:p>"));
        let (text, chars, truncated) = slide_text(&slide);
        assert!(!truncated);
        assert_eq!(chars, UNIT_TEXT_CHAR_CAP - 1);
        assert_eq!(text, format!("{exact}\n"));
    }

    /// Native block units keep body order and numbering across mixed
    /// paragraph and table blocks.
    #[test]
    fn docx_native_blocks_keep_body_order_and_numbering() {
        let root = word_root(&format!(
            "<w:body>{}<w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl>{}</w:body>",
            p("first"),
            p("cell"),
            p("third")
        ));
        let mut report = InspectReport::default();
        docx_units(
            &root,
            "word/document.xml",
            &mut report,
            &crate::reports::NativeSelection::All,
        )
        .expect("selection must be satisfiable");
        let texts: Vec<_> = report
            .native_units
            .iter()
            .map(|unit| unit.text.as_deref())
            .collect();
        assert_eq!(texts, [Some("first"), Some("cell"), Some("third")]);
        assert_eq!(
            report
                .native_units
                .iter()
                .map(|unit| unit.unit_index)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    /// A unit that exceeds the exact Unicode cap keeps exactly the capped
    /// characters, gains the explicit truncation marker, and is downgraded to
    /// partial; later units stay exact.
    #[test]
    fn docx_native_unit_clipping_appends_marker_and_downgrades_completeness() {
        let payload = "é".repeat(UNIT_TEXT_CHAR_CAP + 1);
        let root = word_root(&format!("<w:body>{}{}</w:body>", p(&payload), p("tail")));
        let mut report = InspectReport::default();
        docx_units(
            &root,
            "word/document.xml",
            &mut report,
            &crate::reports::NativeSelection::All,
        )
        .expect("selection must be satisfiable");
        assert_eq!(report.native_units.len(), 2);
        let clipped = &report.native_units[0];
        let text = clipped.text.as_deref().expect("clipped unit keeps text");
        assert_eq!(
            text.chars().count(),
            UNIT_TEXT_CHAR_CAP + UNIT_TEXT_TRUNCATION_MARKER.chars().count()
        );
        assert!(text.ends_with(UNIT_TEXT_TRUNCATION_MARKER));
        assert!(text.chars().take(UNIT_TEXT_CHAR_CAP).all(|c| c == 'é'));
        assert_eq!(
            text.len(),
            2 * UNIT_TEXT_CHAR_CAP + UNIT_TEXT_TRUNCATION_MARKER.len()
        );
        assert_eq!(clipped.completeness, crate::reports::Completeness::Partial);
        assert_eq!(report.native_units[1].text.as_deref(), Some("tail"));
        assert_eq!(
            report.native_units[1].completeness,
            crate::reports::Completeness::Exact
        );
    }

    /// A paragraph unit that contains a nested (invalid) block skips that
    /// block's content once, never multiplies it, and reports the omission as
    /// an explicit partial with the truncation marker.
    #[test]
    fn docx_native_paragraph_skips_nested_block_and_reports_partial() {
        let root = word_root(&format!(
            "<w:body><w:p><w:r><w:t>own</w:t>\
             <w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl></w:r></w:p>{}</w:body>",
            p("nested"),
            p("after")
        ));
        let mut report = InspectReport::default();
        docx_units(
            &root,
            "word/document.xml",
            &mut report,
            &crate::reports::NativeSelection::All,
        )
        .expect("selection must be satisfiable");
        assert_eq!(report.native_units.len(), 2);
        let first = &report.native_units[0];
        let text = first.text.as_deref().expect("unit keeps text");
        assert_eq!(text, format!("own{UNIT_TEXT_TRUNCATION_MARKER}"));
        assert!(!text.contains("nested"));
        assert_eq!(first.completeness, crate::reports::Completeness::Partial);
        assert_eq!(report.native_units[1].text.as_deref(), Some("after"));
    }

    /// The legacy Word text body emits nested paragraphs, nested text
    /// elements, and table-cell paragraphs exactly once, preserving run
    /// order, tabs, and breaks.
    #[test]
    fn legacy_docx_text_emits_nested_ownership_once() {
        let root = word_root(&format!(
            "<w:body>\
             <w:p><w:r><w:t>outer</w:t></w:r><w:p><w:r><w:t>inner</w:t></w:r></w:p></w:p>\
             <w:tbl><w:tr><w:tc>{}</w:tc></w:tr></w:tbl>\
             <w:p><w:r><w:t>pre<w:t>core</w:t>post</w:t></w:r></w:p>\
             <w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t></w:r><w:br/><w:r><w:t>c</w:t></w:r></w:p>\
             </w:body>",
            p("cell")
        ));
        let mut report = InspectReport::default();
        let options = InspectOptions::default();
        docx(&root, &mut report, &options);
        let expected = "outer\ninner\ncell\nprecorepost\na\tb\nc\n";
        assert_eq!(report.text.content, expected);
        assert_eq!(report.counts.chars, expected.chars().count());
        assert!(!report.text.truncated);
        assert_eq!(report.counts.paragraphs, Some(5));
        assert_eq!(report.preflight.len(), 1);
        assert!(!report.preflight[0].needs_ocr);
    }

    /// Explicit `<w:br w:type="page"/>` splits surrounding paragraph text into
    /// separate block units separated by `NativeUnit::PageBreak`, while
    /// `<w:lastRenderedPageBreak/>`, plain `<w:br/>`, and `<w:br w:type="column"/>`
    /// never emit page-break units, and `<w:tbl>` emits `NativeUnit::Table`.
    #[test]
    fn docx_units_split_explicit_page_breaks_and_tag_tables() {
        let root = word_root(concat!(
            "<w:body>",
            "<w:p><w:r><w:t>before</w:t><w:br w:type=\"page\"/><w:t>after</w:t></w:r></w:p>",
            "<w:p><w:r><w:t>soft </w:t><w:lastRenderedPageBreak/><w:t>join</w:t><w:br/><w:t>line</w:t></w:r></w:p>",
            "<w:p><w:r><w:br w:type=\"page\"></w:br></w:r></w:p>",
            "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl>",
            "<w:p><w:r><w:br type=\"page\"/><w:t>leading</w:t></w:r></w:p>",
            "<w:p><w:r><w:t>trailing</w:t><w:br w:type=\"page\"/></w:r></w:p>",
            "</w:body>",
        ));
        let mut report = InspectReport::default();
        docx_units(
            &root,
            "word/document.xml",
            &mut report,
            &crate::reports::NativeSelection::All,
        )
        .expect("selection must be satisfiable");

        let summary: Vec<_> = report
            .native_units
            .iter()
            .map(|u| (u.unit_index, u.unit.clone(), u.text.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    1,
                    crate::reports::NativeUnit::DocumentBlock,
                    Some("before".into()),
                ),
                (2, crate::reports::NativeUnit::PageBreak, None),
                (
                    3,
                    crate::reports::NativeUnit::DocumentBlock,
                    Some("after".into()),
                ),
                (
                    4,
                    crate::reports::NativeUnit::DocumentBlock,
                    Some("soft join\nline".into()),
                ),
                (5, crate::reports::NativeUnit::PageBreak, None),
                (6, crate::reports::NativeUnit::Table, Some("cell".into())),
                (7, crate::reports::NativeUnit::PageBreak, None),
                (
                    8,
                    crate::reports::NativeUnit::DocumentBlock,
                    Some("leading".into()),
                ),
                (
                    9,
                    crate::reports::NativeUnit::DocumentBlock,
                    Some("trailing".into()),
                ),
                (10, crate::reports::NativeUnit::PageBreak, None),
            ]
        );
        for unit in report
            .native_units
            .iter()
            .filter(|u| u.unit == crate::reports::NativeUnit::PageBreak)
        {
            assert!(unit.text.is_none());
            assert!(unit.rows.is_empty());
            assert!(unit.label.is_none());
            assert_eq!(unit.completeness, crate::reports::Completeness::Exact);
        }
    }
}

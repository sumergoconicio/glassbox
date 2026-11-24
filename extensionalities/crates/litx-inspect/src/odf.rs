//! OpenDocument inspection for the `litx-inspect` crate.
//! File: `odf.rs`.
//! Responsibility: extract bounded ODF text, slides, and spreadsheet grids
//! with cumulative expansion and workbook work budgets.
use crate::{
    InspectOptions, InspectReport,
    archive::Archive,
    office,
    profile::WorkBudget,
    xml::{self, Content, Node},
};

pub(crate) const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
pub(crate) const T: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
pub(crate) const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
const DRAW: &str = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";

pub(crate) fn text(node: &Node) -> xml::Result<String> {
    text_capped(node, 16 << 20, "ODF expanded text cap exceeded")
}

fn text_capped(node: &Node, cap: usize, label: &'static str) -> xml::Result<String> {
    fn append(node: &Node, out: &mut String, cap: usize, label: &'static str) -> xml::Result<()> {
        if node.is(T, "s") {
            let count: usize = node.attr_ns(T, "c").unwrap_or("1").parse()?;
            if count > 16_384 {
                return Err("ODF explicit-space cap exceeded".into());
            }
            out.extend(std::iter::repeat_n(' ', count));
        } else if node.is(T, "tab") {
            out.push('\t');
        } else if node.is(T, "line-break") {
            out.push('\n');
        } else {
            for item in &node.content {
                match item {
                    Content::Text(text) => out.push_str(text),
                    Content::Element(child) => append(child, out, cap, label)?,
                }
            }
        }
        if out.len() > cap {
            return Err(label.into());
        }
        Ok(())
    }
    let mut out = String::new();
    append(node, &mut out, cap, label)?;
    Ok(out)
}

/// Cumulative expanded-value byte cap across an ODF document.
pub(crate) const ODF_EXPANDED_CAP: usize = 256 << 20;

/// Shared document-wide budget for expanded ODF text (`<text:s>` and paragraph
/// joins) across all document-table cells, tables, and prose paragraphs.
/// Charged with full expanded bytes before any retention decision; the per-node
/// 16 MiB cap still applies inside the remaining budget.
pub(crate) struct ExpansionBudget {
    remaining: usize,
}

impl ExpansionBudget {
    pub(crate) fn new(cap: usize) -> Self {
        Self { remaining: cap }
    }

    /// Expand one text node within the remaining document budget and charge it.
    pub(crate) fn text(&mut self, node: &Node) -> xml::Result<String> {
        let (cap, label) = if self.remaining < 16 << 20 {
            (self.remaining, "ODF document expansion budget exceeded")
        } else {
            (16 << 20, "ODF expanded text cap exceeded")
        };
        let out = text_capped(node, cap, label)?;
        self.remaining -= out.len();
        Ok(out)
    }
}

/// Joins ODF paragraphs within both expanded-value and workbook work budgets.
/// Accepts paragraph nodes, previously expanded bytes, logical repetition
/// weight, the 256 MiB expanded cap, and the shared workbook budget; returns
/// the joined source-cell value after charging its full logical bytes; rejects
/// malformed space counts, arithmetic overflow, either expanded-value cap, or
/// cumulative value-byte exhaustion without expanding into the next budget.
fn cell_text(
    paragraphs: &[&Node],
    expanded: usize,
    weight: usize,
    cap: usize,
    work: &mut WorkBudget,
) -> xml::Result<String> {
    let odf_value_cap = cap.saturating_sub(expanded) / weight;
    let work_value_cap = work.value_bytes_remaining() / weight;
    let work_is_limit = work_value_cap <= odf_value_cap;
    let value_cap = work_value_cap.min(odf_value_cap);
    let mut value = String::new();
    for (i, p) in paragraphs.iter().enumerate() {
        if i > 0 {
            if value.len() >= value_cap {
                if work_is_limit {
                    work.exhaust_value_bytes();
                    return Err(crate::profile::WORK_VALUE_BUDGET_EXCEEDED.into());
                }
                return Err("ODF expanded value byte cap exceeded".into());
            }
            value.push('\n');
        }
        let remaining = value_cap.saturating_sub(value.len());
        let (paragraph_cap, label) = if remaining < 16 << 20 {
            (
                remaining,
                if work_is_limit {
                    crate::profile::WORK_VALUE_BUDGET_EXCEEDED
                } else {
                    "ODF expanded value byte cap exceeded"
                },
            )
        } else {
            (16 << 20, "ODF expanded text cap exceeded")
        };
        let expanded_text = if paragraph_cap == 16 << 20 {
            text(p)
        } else {
            text_capped(p, paragraph_cap, label)
        };
        match expanded_text {
            Ok(text) => value.push_str(&text),
            Err(error) => {
                if work_is_limit && error.to_string() == crate::profile::WORK_VALUE_BUDGET_EXCEEDED
                {
                    work.exhaust_value_bytes();
                }
                return Err(error);
            }
        }
        if expanded
            .checked_add(
                value
                    .len()
                    .checked_mul(weight)
                    .ok_or("ODF text size overflow")?,
            )
            .is_none_or(|total| total > cap)
        {
            return Err("ODF expanded value byte cap exceeded".into());
        }
    }
    work.charge_value_bytes(
        value
            .len()
            .checked_mul(weight)
            .ok_or("ODF text size overflow")?,
    )?;
    Ok(value)
}

pub(crate) fn inspect(
    archive: &mut Archive,
    report: &mut InspectReport,
    options: &InspectOptions,
) -> xml::Result<()> {
    let Some(bytes) = archive.read("mimetype")? else {
        return Ok(());
    };
    let mime = std::str::from_utf8(&bytes)?.trim();
    let (ext, body_kind, name) = match mime {
        "application/vnd.oasis.opendocument.text" => ("odt", "text", "OpenDocument text"),
        "application/vnd.oasis.opendocument.presentation" => {
            ("odp", "presentation", "OpenDocument presentation")
        }
        "application/vnd.oasis.opendocument.spreadsheet" => {
            ("ods", "spreadsheet", "OpenDocument spreadsheet")
        }
        _ => return Ok(()),
    };
    let root = office::read_xml(archive, "content.xml")?;
    if !root.is(O, "document-content") {
        return Err("invalid ODF content namespace/root".into());
    }
    let body = root
        .children()
        .find(|n| n.is(O, "body"))
        .and_then(|n| n.children().find(|n| n.is(O, body_kind)))
        .ok_or("ODF body does not match mimetype")?;
    office::identify(report, ext, mime, name);
    office::metadata(archive, report);
    // One document-wide budget for all ODF text expansion in tables and prose.
    let mut expansion = ExpansionBudget::new(ODF_EXPANDED_CAP);
    if ext == "ods" {
        spreadsheet(body, report, options)?;
    } else if ext == "odp" {
        let slides: Vec<_> = body.children().filter(|n| n.is(DRAW, "page")).collect();
        report.counts.slides = Some(slides.len());
        if let Some(selection) = &options.native_selection {
            let plan = crate::selection::begin(
                report,
                selection,
                crate::reports::NativeUnit::Slide,
                slides.len() as u64,
            )?;
            for (index, slide) in slides.into_iter().enumerate() {
                let number = index as u64 + 1;
                if plan.contains(number) {
                    let mut text_nodes = Vec::new();
                    slide.visit(&mut |node| {
                        if node.is(T, "p") || node.is(T, "h") {
                            text_nodes.push(node);
                        }
                    });
                    // COND-11: one buffer with early retention clipping. The
                    // unit keeps at most one character beyond the extraction
                    // cap, the expansion budget still charges every
                    // paragraph, and the paragraph separators stay exact.
                    let bound = options.max_chars.saturating_add(1);
                    let mut text = String::new();
                    let mut retained = 0usize;
                    for (position, node) in text_nodes.into_iter().enumerate() {
                        if position > 0 {
                            append_bounded(&mut text, &mut retained, bound, "\n");
                        }
                        let value = expansion.text(node)?;
                        append_bounded(&mut text, &mut retained, bound, &value);
                    }
                    report.native_units.push(crate::selection::text_unit(
                        number,
                        crate::reports::NativeUnit::Slide,
                        Some(format!(
                            "content.xml#slide/{}",
                            slide.attr_ns(DRAW, "name").unwrap_or("unnamed")
                        )),
                        text,
                        crate::reports::ContentOrigin::Native,
                    ));
                }
            }
        } else {
            for (index, slide) in slides.iter().take(options.text_unit_limit()).enumerate() {
                unit(slide, "slide", index, report, options, &mut expansion)?;
            }
            report.text.truncated |= slides.len() > options.text_unit_limit();
        }
    } else if ext == "odt" {
        report.counts.paragraphs =
            Some(body.descendants(T, "p").len() + body.descendants(T, "h").len());
        if let Some(selection) = &options.native_selection {
            let mut blocks = Vec::new();
            for child in body.children() {
                if child.is(T, "section") {
                    blocks.extend(child.children().filter(|node| {
                        node.is(T, "p") || node.is(T, "h") || node.is(TABLE, "table")
                    }));
                } else if child.is(T, "p") || child.is(T, "h") || child.is(TABLE, "table") {
                    blocks.push(child);
                }
            }
            if blocks.len() > 4096 {
                return Err("ODT native block cap exceeded".into());
            }
            let plan = crate::selection::begin(
                report,
                selection,
                crate::reports::NativeUnit::DocumentBlock,
                blocks.len() as u64,
            )?;
            let mut table_ordinal: u64 = 0;
            for (index, block) in blocks.into_iter().enumerate() {
                let number = index as u64 + 1;
                let is_table = block.is(TABLE, "table");
                let marker_index = if is_table {
                    table_ordinal += 1;
                    Some(table_ordinal)
                } else {
                    None
                };
                if plan.contains(number) {
                    let text = if is_table {
                        odf_table_text(block, &mut expansion, options.max_chars)?
                    } else {
                        expansion.text(block)?
                    };
                    let unit_kind = if is_table {
                        crate::reports::NativeUnit::Table
                    } else {
                        crate::reports::NativeUnit::DocumentBlock
                    };
                    let mut unit = crate::selection::text_unit(
                        number,
                        unit_kind,
                        Some(format!("content.xml#body/{number}")),
                        text,
                        crate::reports::ContentOrigin::Native,
                    );
                    unit.marker_index = marker_index;
                    report.native_units.push(unit);
                }
            }
        } else {
            let sections: Vec<_> = body.children().filter(|n| n.is(T, "section")).collect();
            if sections.is_empty() {
                unit(body, "section", 0, report, options, &mut expansion)?;
            } else {
                for (index, child) in body.children().take(options.text_unit_limit()).enumerate() {
                    unit(child, "section", index, report, options, &mut expansion)?;
                }
                report.text.truncated |= body.children().count() > options.text_unit_limit();
            }
        }
    }
    Ok(())
}

/// Appends text to a bounded unit buffer, clipping retention on a character
/// boundary.
///
/// Accepts the destination text, the retained character counter, the retention
/// bound, and the text to append. Returns unit. The buffer retains at most
/// `bound` characters; the cut always lands between Unicode characters, so a
/// clipped prefix stays valid UTF-8.
fn append_bounded(out: &mut String, retained: &mut usize, bound: usize, text: &str) {
    let remaining = bound.saturating_sub(*retained);
    let end = text
        .char_indices()
        .nth(remaining)
        .map_or(text.len(), |(offset, _)| offset);
    out.push_str(&text[..end]);
    *retained += text[..end].chars().count();
}

/// Expands an atomic ODF table as rows and tab-separated cells in one buffer.
///
/// Accepts a table node, the shared expansion budget, and the extraction
/// character cap. Returns deterministic table text: rows join with newlines,
/// cells with tabs, and a cell's paragraphs with newlines, matching the joined
/// representation this replaced. COND-11: the text accumulates directly in one
/// buffer that retains at most one character beyond the cap, so the peak stays
/// bounded by the cap instead of a second whole-unit copy while the downstream
/// content limiter still detects and marks the clipped unit partial; the
/// expansion budget keeps charging every paragraph, so cap errors are
/// unchanged. Malformed space counts and expansion-budget exhaustion return
/// errors. The traversal stops at nested `<table:table>` ownership boundaries:
/// each nested table is processed once, in place, with its own row and cell
/// traversal, so every inner paragraph appears and charges the budget exactly
/// once, in natural document order.
fn odf_table_text(
    table: &Node,
    expansion: &mut ExpansionBudget,
    max_chars: usize,
) -> xml::Result<String> {
    let bound = max_chars.saturating_add(1);
    let mut out = String::new();
    let mut retained = 0usize;
    let mut first_line = true;
    append_table_text(
        table,
        &mut out,
        &mut retained,
        bound,
        expansion,
        &mut first_line,
    )?;
    Ok(out)
}

/// Appends one table's owned rows and cells into the shared unit buffer.
///
/// Accepts the owning table node, the shared buffer, the retained character
/// counter, the retention bound, the shared expansion budget, and the shared
/// line-separator state. Returns unit. Rows owned by this table join with
/// newlines and their cells with tabs. The walk descends through row-group
/// wrappers such as `table:table-header-rows` but stops at nested
/// `table:table` boundaries, consistent with `tables.rs::odf_rows`, so no row
/// or paragraph is visited twice. The separator state arrives from and returns
/// to the enclosing cell, so a nested table lands in natural document order.
fn append_table_text(
    table: &Node,
    out: &mut String,
    retained: &mut usize,
    bound: usize,
    expansion: &mut ExpansionBudget,
    first_line: &mut bool,
) -> xml::Result<()> {
    for row in owned_table_rows(table) {
        if !*first_line {
            append_bounded(out, retained, bound, "\n");
        }
        *first_line = false;
        let mut first_cell = true;
        for cell in row
            .children()
            .filter(|node| node.is(TABLE, "table-cell") || node.is(TABLE, "covered-table-cell"))
        {
            if !first_cell {
                append_bounded(out, retained, bound, "\t");
            }
            first_cell = false;
            let mut first_content = true;
            append_cell_text(cell, out, retained, bound, expansion, &mut first_content)?;
        }
    }
    Ok(())
}

/// Appends one cell's owned paragraphs and nested tables in document order.
///
/// Accepts the cell node, the shared buffer, the retained character counter,
/// the retention bound, the shared expansion budget, and the cell's
/// line-separator state. Returns unit. Paragraphs owned by this cell join
/// with newlines, descending through wrappers such as `text:list` but never
/// across nested-table boundaries. Each nested `<table:table>` is processed
/// once, in place, with its own row and cell traversal that shares this
/// cell's separator state, so inner paragraphs appear and charge the budget
/// exactly once.
fn append_cell_text(
    cell: &Node,
    out: &mut String,
    retained: &mut usize,
    bound: usize,
    expansion: &mut ExpansionBudget,
    first_line: &mut bool,
) -> xml::Result<()> {
    fn visit(
        node: &Node,
        out: &mut String,
        retained: &mut usize,
        bound: usize,
        expansion: &mut ExpansionBudget,
        first_line: &mut bool,
    ) -> xml::Result<()> {
        for child in node.children() {
            if child.is(TABLE, "table") {
                // A nested table is owned by this cell: process it once, in
                // place, with its own row and cell traversal.
                append_table_text(child, out, retained, bound, expansion, first_line)?;
            } else if child.is(T, "p") {
                if !*first_line {
                    append_bounded(out, retained, bound, "\n");
                }
                *first_line = false;
                let value = expansion.text(child)?;
                append_bounded(out, retained, bound, &value);
            } else {
                visit(child, out, retained, bound, expansion, first_line)?;
            }
        }
        Ok(())
    }
    visit(cell, out, retained, bound, expansion, first_line)
}

/// Collects the rows owned by one ODF table.
///
/// Accepts the owning table node. Returns its owned `table:table-row` nodes
/// in document order. The walk descends through row-group wrappers but stops
/// at nested `table:table` boundaries, matching `tables.rs::odf_rows`, so no
/// row belongs to two tables.
fn owned_table_rows(table: &Node) -> Vec<&Node> {
    fn visit<'a>(node: &'a Node, rows: &mut Vec<&'a Node>) {
        if node.is(TABLE, "table") {
            return;
        }
        if node.is(TABLE, "table-row") {
            rows.push(node);
            return;
        }
        for child in node.children() {
            visit(child, rows);
        }
    }
    let mut rows = Vec::new();
    for child in table.children() {
        visit(child, &mut rows);
    }
    rows
}

fn positive(node: &Node, key: &str, cap: usize) -> xml::Result<usize> {
    let n = node.attr_ns(TABLE, key).unwrap_or("1").parse::<usize>()?;
    if n == 0 || n > cap {
        return Err(format!("ODF {key} cap exceeded").into());
    }
    Ok(n)
}

/// Profiles ODS tables under shared sheet, cell, and value-byte limits.
/// Accepts the spreadsheet body, report under construction, and inspection
/// options; returns unit after retaining honest partial output; per-table
/// structure and budget failures become sheet diagnostics, while cumulative
/// work exhaustion stops every later table.
fn spreadsheet(
    body: &Node,
    report: &mut InspectReport,
    options: &InspectOptions,
) -> xml::Result<()> {
    use crate::profile::{Budget, Grid, accumulate, column_name};
    let tables: Vec<_> = body.children().filter(|n| n.is(TABLE, "table")).collect();
    report.counts.sheets = Some(tables.len());
    let selection_plan = match &options.native_selection {
        Some(selection) => {
            if tables.len() > crate::spreadsheet::WORKBOOK_SHEET_CAP {
                return Err("workbook sheet cap exceeded".into());
            }
            Some(crate::selection::begin(
                report,
                selection,
                crate::reports::NativeUnit::Worksheet,
                tables.len() as u64,
            )?)
        }
        None => None,
    };
    let sheet_limit = if selection_plan.is_some() {
        crate::spreadsheet::WORKBOOK_SHEET_CAP
    } else {
        options
            .text_unit_limit()
            .min(crate::spreadsheet::WORKBOOK_SHEET_CAP)
    };
    let sheet_capped = selection_plan.is_none() && tables.len() > sheet_limit;
    let mut budget = Budget::default();
    let mut work = WorkBudget::default();
    let mut remaining_preview_cells = crate::markdown::MAX_WORKBOOK_PREVIEW_CELLS;
    let mut expanded_bytes = 0usize;
    let mut work_capped = false;
    let mut attempted_sheets = 0usize;
    for (index, table) in tables.iter().take(sheet_limit).enumerate() {
        if selection_plan
            .as_ref()
            .is_some_and(|plan| !plan.contains(index as u64 + 1))
        {
            continue;
        }
        attempted_sheets = index + 1;
        let result = (|| -> xml::Result<()> {
            let mut grid = Grid::new(table.attr_ns(TABLE, "name").unwrap_or("").into(), index + 1);
            let mut markdown = crate::markdown::SheetRows::default();
            let mut row_number = 1usize;
            let mut sheet_chars = 0usize;
            for row in table.descendants(TABLE, "table-row") {
                let repeat_rows = positive(row, "number-rows-repeated", 1_048_576)?;
                if row_number
                    .checked_add(repeat_rows - 1)
                    .is_none_or(|n| n > 1_048_576)
                {
                    return Err("ODF row cap exceeded".into());
                }
                let mut col = 1usize;
                let (mut row_prefix, mut row_chars) = (String::new(), 0usize);
                let mut preview_chars = 0usize;
                let mut last_text_col = 0usize;
                let previewing = markdown.can_buffer_row();
                let mut row_cells: Vec<(usize, String)> = Vec::new();
                let mut append = |value: &str, repeats: usize| {
                    let chars = value.chars().count();
                    accumulate(&mut row_chars, chars, repeats);
                    for _ in 0..repeats.min(
                        (options.max_chars.saturating_sub(preview_chars) / chars.max(1))
                            .saturating_add(1),
                    ) {
                        let take = chars.min(options.max_chars.saturating_sub(preview_chars));
                        row_prefix.extend(value.chars().take(take));
                        preview_chars += take;
                    }
                };
                for cell in row
                    .children()
                    .filter(|n| n.is(TABLE, "table-cell") || n.is(TABLE, "covered-table-cell"))
                {
                    let repeat_cols = positive(cell, "number-columns-repeated", 16_384)?;
                    if col.checked_add(repeat_cols - 1).is_none_or(|n| n > 16_384) {
                        return Err("ODF column cap exceeded".into());
                    }
                    let paragraphs = cell.descendants(T, "p");
                    let kind_attr = cell.attr_ns(O, "value-type").unwrap_or("string");
                    // A typed value attribute overrides paragraph text; never expand it.
                    let typed = match kind_attr {
                        "float" | "percentage" | "currency" => cell.attr_ns(O, "value"),
                        "boolean" => cell.attr_ns(O, "boolean-value"),
                        "date" => cell.attr_ns(O, "date-value"),
                        "time" => cell.attr_ns(O, "time-value"),
                        "string" => cell.attr_ns(O, "string-value"),
                        _ => None,
                    };
                    let weight = repeat_rows
                        .checked_mul(repeat_cols)
                        .ok_or("ODF repeat overflow")?;
                    let formula = cell.attr_ns(TABLE, "formula").is_some();
                    // Fully blank tail cells are already O(1): charge their
                    // source visit without rejecting the established fast path.
                    // Values and formulas charge every logical repeat before
                    // expansion, profiling, or output; empty sheets never reset
                    // charges retained from earlier sheets.
                    let has_repeated_work = formula
                        || !paragraphs.is_empty()
                        || typed.is_some_and(|value| !value.is_empty());
                    work.charge_cells(if has_repeated_work { weight } else { 1 })?;
                    let value = match typed {
                        Some(raw) => {
                            let value = raw.to_string();
                            work.charge_value_bytes(
                                value
                                    .len()
                                    .checked_mul(weight)
                                    .ok_or("ODF text size overflow")?,
                            )?;
                            value
                        }
                        None => cell_text(
                            &paragraphs,
                            expanded_bytes,
                            weight,
                            ODF_EXPANDED_CAP,
                            &mut work,
                        )?,
                    };
                    let kind = match kind_attr {
                        "float" | "percentage" | "currency" => {
                            if !value.is_empty() && !value.parse::<f64>().is_ok_and(f64::is_finite)
                            {
                                return Err("invalid ODF number".into());
                            }
                            "number"
                        }
                        "boolean" => "bool",
                        "date" => "date",
                        "time" => "time",
                        "string" => "string",
                        _ => return Err("unsupported ODF cell type".into()),
                    };
                    if !value.is_empty() || formula {
                        expanded_bytes = expanded_bytes
                            .checked_add(
                                value
                                    .len()
                                    .checked_mul(weight)
                                    .ok_or("ODF text size overflow")?,
                            )
                            .ok_or("ODF text size overflow")?;
                        if expanded_bytes > ODF_EXPANDED_CAP {
                            return Err("ODF expanded value byte cap exceeded".into());
                        }
                        for column in col..col + repeat_cols {
                            grid.cell(
                                (row_number, column),
                                &value,
                                kind,
                                formula,
                                repeat_rows,
                                &mut budget,
                            )?;
                            if previewing && column <= crate::markdown::MAX_PREVIEW_COLS {
                                row_cells.push((column, value.clone()));
                            }
                            if column > 1 {
                                append("\t", column - last_text_col.max(1));
                            }
                            append(&value, 1);
                            last_text_col = column;
                        }
                        let span_cols = positive(cell, "number-columns-spanned", 16_384)?;
                        let span_rows = positive(cell, "number-rows-spanned", 1_048_576)?;
                        if span_cols > 1 || span_rows > 1 {
                            if repeat_rows > 1 || repeat_cols > 1 {
                                return Err("repeated merged ODF cells are unsupported".into());
                            }
                            if col + span_cols - 1 > 16_384
                                || row_number + span_rows - 1 > 1_048_576
                            {
                                return Err("ODF merged range out of bounds".into());
                            }
                            grid.merge(format!(
                                "{}{}:{}{}",
                                column_name(col),
                                row_number,
                                column_name(col + span_cols - 1),
                                row_number + span_rows - 1
                            ))?;
                        }
                    }
                    col += repeat_cols;
                }
                if last_text_col > 0 {
                    append("\n", 1);
                    markdown.push_bounded_row(
                        row_number,
                        &row_cells,
                        repeat_rows,
                        options.max_chars,
                    );
                    accumulate(&mut sheet_chars, row_chars, repeat_rows);
                    let preview_repeats = repeat_rows.min(7usize.saturating_sub(row_number));
                    report.append_counted(
                        &row_prefix,
                        row_chars,
                        preview_repeats,
                        options.max_chars,
                    );
                    report.counts.chars = report
                        .counts
                        .chars
                        .saturating_add(row_chars.saturating_mul(repeat_rows - preview_repeats));
                }
                row_number += repeat_rows;
            }
            office::preflight(
                report,
                "sheet",
                index,
                sheet_chars,
                !table.descendants(DRAW, "image").is_empty(),
                options,
            );
            let mut sheet = grid.finish();
            let row_numbers =
                markdown.write(&mut sheet, report, options, &mut remaining_preview_cells);
            if selection_plan.is_some() {
                report.native_units.push(crate::selection::worksheet_unit(
                    index,
                    format!("content.xml#sheet/{}", sheet.name),
                    &mut sheet,
                    row_numbers,
                ));
            }
            report.sheets.push(sheet);
            Ok(())
        })();
        if let Err(error) = result {
            report.error("sheet", format!("sheet {}: {error}", index + 1));
            report.text.truncated = true;
            if work.is_exhausted() {
                work_capped = true;
                break;
            }
        }
    }
    if work_capped {
        report.error(
            "sheet",
            format!(
                "workbook work budget exhausted after {attempted_sheets} sheets; {} declared sheets skipped",
                tables.len().saturating_sub(attempted_sheets)
            ),
        );
        report.text.truncated = true;
    }
    if sheet_capped {
        report.error(
            "sheet",
            format!(
                "workbook sheet cap exceeded: {} sheets declared, processing limited to the first {sheet_limit} sheets",
                tables.len()
            ),
        );
        report.text.truncated = true;
    }
    crate::spreadsheet::finish(report, &budget);
    Ok(())
}

fn unit(
    node: &Node,
    kind: &'static str,
    index: usize,
    report: &mut InspectReport,
    options: &InspectOptions,
    expansion: &mut ExpansionBudget,
) -> xml::Result<()> {
    let table_start = report.tables.len();
    crate::tables::Context::new(report, "content.xml", kind, index + 1, options)
        .with_expansion(expansion)
        .odf_tables(node);
    if kind == "slide" {
        crate::markdown::append(
            report,
            &format!("## Slide {}\n\n", index + 1),
            options,
            "native_structure",
        );
    }
    crate::markdown::odf(node, report, options, table_start, expansion);
    let mut chars = 0;
    let mut result = Ok(());
    node.visit(&mut |n| {
        if n.is(T, "p") || n.is(T, "h") {
            match expansion.text(n) {
                Ok(value) => {
                    chars += value.chars().count();
                    report.append_text(&value, options.max_chars);
                    report.append_text("\n", options.max_chars);
                }
                Err(e) => result = Err(e),
            }
        }
    });
    result?;
    office::preflight(
        report,
        kind,
        index,
        chars,
        !node.descendants(DRAW, "image").is_empty(),
        options,
    );
    Ok(())
}

#[cfg(test)]
mod bounds_tests {
    fn paragraphs(texts: &[&str]) -> Vec<crate::xml::Node> {
        texts
            .iter()
            .map(|text| {
                let bytes = format!("<text:p xmlns:text=\"{}\">{}</text:p>", crate::odf::T, text);
                crate::xml::parse(bytes.as_bytes()).unwrap()
            })
            .collect()
    }

    #[test]
    fn expansion_budget_charges_full_expansion_across_nodes() {
        let owned = paragraphs(&["aaaa", "bbbb", "cccc", "dddd"]);
        // Full expanded bytes are charged per node, not a retained prefix:
        // 12 bytes fit exactly, and the fourth 4-byte paragraph exceeds the
        // exhausted budget before any retention decision.
        let mut budget = super::ExpansionBudget::new(12);
        assert_eq!(budget.text(&owned[0]).unwrap(), "aaaa");
        assert_eq!(budget.text(&owned[1]).unwrap(), "bbbb");
        assert_eq!(budget.text(&owned[2]).unwrap(), "cccc");
        let error = budget.text(&owned[3]).unwrap_err();
        assert!(
            error.to_string().contains("document expansion budget"),
            "{error}"
        );
    }

    #[test]
    fn expansion_budget_charges_expanded_spaces_not_source_bytes() {
        // <text:s text:c="8"/> expands to 8 bytes from a tiny source node.
        let owned = paragraphs(&["<text:s text:c=\"8\"/>x"]);
        let mut budget = super::ExpansionBudget::new(10);
        assert_eq!(budget.text(&owned[0]).unwrap(), "        x");
        let error = budget.text(&owned[0]).unwrap_err();
        assert!(
            error.to_string().contains("document expansion budget"),
            "{error}"
        );
    }
    #[test]
    /// Checks incremental ODF and workbook value-byte limits with small inputs.
    /// Accepts no inputs; returns unit; panics if exact fits, expanded-cap
    /// failures, or cumulative work exhaustion are charged incorrectly.
    fn ods_cell_text_charges_remaining_budget_incrementally() {
        let owned = paragraphs(&["aaaa", "bbbb", "cccc"]);
        let paragraphs: Vec<&crate::xml::Node> = owned.iter().collect();
        // Weight 2: joined "aaaa\nbbbb\ncccc" is 14 bytes; 8 + 14*2 = 36 fits 40.
        let text =
            super::cell_text(&paragraphs, 8, 2, 40, &mut super::WorkBudget::default()).unwrap();
        assert_eq!(text, "aaaa\nbbbb\ncccc");
        // Cap 35 is exceeded once the third paragraph joins; error before append.
        let error =
            super::cell_text(&paragraphs, 8, 2, 35, &mut super::WorkBudget::default()).unwrap_err();
        assert!(error.to_string().contains("cap"), "{error}");
        // A single oversized paragraph is rejected at the first incremental check.
        let error = super::cell_text(
            &paragraphs[..1],
            38,
            1,
            40,
            &mut super::WorkBudget::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cap"), "{error}");
        // The tighter 27-byte work budget stops the 28-byte logical value and
        // latches exhaustion so the caller cannot retry later sheets.
        let mut work = super::WorkBudget::new(10, 27);
        let error = super::cell_text(&paragraphs, 8, 2, 40, &mut work).unwrap_err();
        assert!(error.to_string().contains("value-byte budget"), "{error}");
        assert!(work.is_exhausted());
    }

    /// Builds one ODF table node from row/cell/paragraph markup for tests.
    ///
    /// Accepts the inner table markup. Returns the parsed `table:table` node.
    /// Parse failures panic; no filesystem or archive access occurs.
    fn table(markup: &str) -> crate::xml::Node {
        let bytes = format!(
            "<table:table xmlns:table=\"{}\" xmlns:text=\"{}\">{markup}</table:table>",
            super::TABLE,
            super::T
        );
        crate::xml::parse(bytes.as_bytes()).unwrap()
    }

    /// Checks exact row/cell/paragraph separators without any clipping.
    #[test]
    fn odf_table_text_joins_rows_cells_and_paragraphs_exactly() {
        let node = table(
            "<table:table-row>\
             <table:table-cell><text:p>a</text:p></table:table-cell>\
             <table:table-cell><text:p>b</text:p><text:p>c</text:p></table:table-cell>\
             </table:table-row>\
             <table:table-row>\
             <table:table-cell><text:p>d</text:p></table:table-cell>\
             </table:table-row>",
        );
        let mut expansion = super::ExpansionBudget::new(super::ODF_EXPANDED_CAP);
        let text = super::odf_table_text(&node, &mut expansion, usize::MAX).unwrap();
        // Rows join with newlines, cells with tabs, and one cell's paragraphs
        // with newlines, exactly like the joined representation this replaced.
        assert_eq!(text, "a\tb\nc\nd");
    }

    /// Checks early retention clipping against the extraction character cap.
    #[test]
    fn odf_table_text_clips_retention_early_on_character_boundaries() {
        let node = table(
            "<table:table-row>\
             <table:table-cell><text:p>a</text:p></table:table-cell>\
             <table:table-cell><text:p>b</text:p><text:p>c</text:p></table:table-cell>\
             </table:table-row>\
             <table:table-row>\
             <table:table-cell><text:p>d</text:p></table:table-cell>\
             </table:table-row>",
        );
        // The cap-plus-one retention bound keeps the buffer bounded by the
        // extraction cap instead of the whole expanded unit; the downstream
        // content limiter still sees one character beyond the cap and marks
        // the unit partial.
        let mut expansion = super::ExpansionBudget::new(super::ODF_EXPANDED_CAP);
        let text = super::odf_table_text(&node, &mut expansion, 3).unwrap();
        assert_eq!(text, "a\tb\n");
        assert_eq!(text.chars().count(), 4);
        // Unicode boundaries survive the clip: the retained prefix ends
        // between characters, never inside a multi-byte sequence.
        let unicode = table(
            "<table:table-row>\
             <table:table-cell><text:p>éàü</text:p></table:table-cell>\
             <table:table-cell><text:p>ö</text:p></table:table-cell>\
             </table:table-row>",
        );
        let mut expansion = super::ExpansionBudget::new(super::ODF_EXPANDED_CAP);
        // The cap-plus-one bound retains two of the three accented
        // characters, cutting inside the first paragraph between characters.
        let text = super::odf_table_text(&unicode, &mut expansion, 1).unwrap();
        assert_eq!(text, "éà");
        assert_eq!(text.chars().count(), 2);
    }

    /// Checks expansion-budget charging and cap errors survive the rewrite.
    #[test]
    fn odf_table_text_keeps_charging_the_budget_and_returning_cap_errors() {
        let node = table(
            "<table:table-row>\
             <table:table-cell><text:p>aaaa</text:p></table:table-cell>\
             <table:table-cell><text:p>bbbb</text:p></table:table-cell>\
             <table:table-cell><text:p>cccc</text:p></table:table-cell>\
             </table:table-row>",
        );
        // A 8-byte budget fits exactly two 4-byte paragraphs and rejects the
        // third with the established document-budget error, even when the
        // retention bound would already have clipped it away.
        let mut expansion = super::ExpansionBudget::new(8);
        let error = super::odf_table_text(&node, &mut expansion, 1).unwrap_err();
        assert!(
            error.to_string().contains("document expansion budget"),
            "{error}"
        );
    }

    /// Checks nested tables land once, in place, in document order.
    #[test]
    fn odf_table_text_emits_nested_tables_once_in_document_order() {
        // The outer row sits in a row-group wrapper; the cell holds one
        // paragraph, a doubly nested table, and a closing paragraph.
        let node = table(
            "<table:table-header-rows><table:table-row>\
             <table:table-cell><text:p>OUTER_FIRST</text:p>\
             <table:table><table:table-row>\
             <table:table-cell><text:p>MIDDLE</text:p>\
             <table:table><table:table-row>\
             <table:table-cell><text:p>INNERMOST</text:p></table:table-cell>\
             </table:table-row></table:table>\
             </table:table-cell>\
             </table:table-row></table:table>\
             <text:p>OUTER_LAST</text:p>\
             </table:table-cell>\
             </table:table-row></table:table-header-rows>",
        );
        let mut expansion = super::ExpansionBudget::new(super::ODF_EXPANDED_CAP);
        let text = super::odf_table_text(&node, &mut expansion, usize::MAX).unwrap();
        // The wrapper still yields its row, the nested tables keep their own
        // row and cell delimiters, and each paragraph appears exactly once in
        // natural document order instead of once per owning level.
        assert_eq!(text, "OUTER_FIRST\nMIDDLE\nINNERMOST\nOUTER_LAST");
        assert_eq!(text.matches("INNERMOST").count(), 1);
    }

    /// Checks one pass charges nested paragraphs exactly once.
    #[test]
    fn odf_table_text_charges_nested_paragraphs_exactly_once() {
        let node = table(
            "<table:table-row>\
             <table:table-cell><text:p>OUTER</text:p>\
             <table:table><table:table-row>\
             <table:table-cell><text:p>INNER</text:p></table:table-cell>\
             </table:table-row></table:table>\
             </table:table-cell>\
             </table:table-row>",
        );
        // The two paragraphs expand to 10 bytes together. An exact budget
        // proves a single pass: the boundary-less traversal charged INNER
        // twice, so this budget failed with a document-budget error.
        let mut expansion = super::ExpansionBudget::new(10);
        let text = super::odf_table_text(&node, &mut expansion, usize::MAX).unwrap();
        assert_eq!(text, "OUTER\nINNER");
        assert_eq!(expansion.remaining, 0);
        // One byte short of the single pass still fails with the established
        // document-budget error, proving the charge is neither duplicated
        // nor skipped.
        let mut expansion = super::ExpansionBudget::new(9);
        let error = super::odf_table_text(&node, &mut expansion, usize::MAX).unwrap_err();
        assert!(
            error.to_string().contains("document expansion budget"),
            "{error}"
        );
    }
}

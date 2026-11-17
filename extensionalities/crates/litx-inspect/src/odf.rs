use crate::{
    InspectOptions, InspectReport,
    archive::Archive,
    office,
    xml::{self, Content, Node},
};

pub(crate) const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
pub(crate) const T: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
pub(crate) const TABLE: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
const DRAW: &str = "urn:oasis:names:tc:opendocument:xmlns:drawing:1.0";

pub(crate) fn text(node: &Node) -> xml::Result<String> {
    fn append(node: &Node, out: &mut String) -> xml::Result<()> {
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
                    Content::Element(child) => append(child, out)?,
                }
            }
        }
        if out.len() > 16 << 20 {
            return Err("ODF expanded text cap exceeded".into());
        }
        Ok(())
    }
    let mut out = String::new();
    append(node, &mut out)?;
    Ok(out)
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
    if ext == "ods" {
        spreadsheet(body, report, options)?;
    } else if ext == "odp" {
        let slides: Vec<_> = body.children().filter(|n| n.is(DRAW, "page")).collect();
        report.counts.slides = Some(slides.len());
        for (index, slide) in slides.iter().enumerate() {
            unit(slide, "slide", index, report, options)?;
        }
    } else if ext == "odt" {
        report.counts.paragraphs =
            Some(body.descendants(T, "p").len() + body.descendants(T, "h").len());
        let sections: Vec<_> = body.children().filter(|n| n.is(T, "section")).collect();
        if sections.is_empty() {
            unit(body, "section", 0, report, options)?;
        } else {
            for (index, child) in body.children().enumerate() {
                unit(child, "section", index, report, options)?;
            }
        }
    }
    Ok(())
}

fn positive(node: &Node, key: &str, cap: usize) -> xml::Result<usize> {
    let n = node.attr_ns(TABLE, key).unwrap_or("1").parse::<usize>()?;
    if n == 0 || n > cap {
        return Err(format!("ODF {key} cap exceeded").into());
    }
    Ok(n)
}

fn spreadsheet(
    body: &Node,
    report: &mut InspectReport,
    options: &InspectOptions,
) -> xml::Result<()> {
    use crate::profile::{Budget, Grid, column_name};
    let tables: Vec<_> = body.children().filter(|n| n.is(TABLE, "table")).collect();
    report.counts.sheets = Some(tables.len());
    let mut budget = Budget::default();
    let mut expanded_bytes = 0usize;
    for (index, table) in tables.iter().enumerate() {
        let result = (|| -> xml::Result<()> {
            let mut grid = Grid::new(table.attr_ns(TABLE, "name").unwrap_or("").into(), index + 1);
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
                let mut append = |value: &str, repeats: usize| {
                    let chars = value.chars().count();
                    row_chars += chars * repeats;
                    for _ in 0..repeats
                        .min(options.max_chars.saturating_sub(preview_chars) / chars.max(1) + 1)
                    {
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
                    let mut value = String::new();
                    for (i, p) in paragraphs.iter().enumerate() {
                        if i > 0 {
                            value.push('\n');
                        }
                        value.push_str(&text(p)?);
                    }
                    let kind = match cell.attr_ns(O, "value-type").unwrap_or("string") {
                        "float" | "percentage" | "currency" => {
                            if let Some(raw) = cell.attr_ns(O, "value") {
                                value = raw.into();
                            }
                            if !value.is_empty() && !value.parse::<f64>().is_ok_and(f64::is_finite)
                            {
                                return Err("invalid ODF number".into());
                            }
                            "number"
                        }
                        "boolean" => {
                            value = cell.attr_ns(O, "boolean-value").unwrap_or(&value).into();
                            "bool"
                        }
                        "date" => {
                            value = cell.attr_ns(O, "date-value").unwrap_or(&value).into();
                            "date"
                        }
                        "time" => {
                            value = cell.attr_ns(O, "time-value").unwrap_or(&value).into();
                            "time"
                        }
                        "string" => {
                            if let Some(raw) = cell.attr_ns(O, "string-value") {
                                value = raw.into();
                            }
                            "string"
                        }
                        _ => return Err("unsupported ODF cell type".into()),
                    };
                    let formula = cell.attr_ns(TABLE, "formula").is_some();
                    if !value.is_empty() || formula {
                        let weight = repeat_rows
                            .checked_mul(repeat_cols)
                            .ok_or("ODF repeat overflow")?;
                        expanded_bytes = expanded_bytes
                            .checked_add(
                                value
                                    .len()
                                    .checked_mul(weight)
                                    .ok_or("ODF text size overflow")?,
                            )
                            .ok_or("ODF text size overflow")?;
                        if expanded_bytes > 256 << 20 {
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
                    sheet_chars += row_chars * repeat_rows;
                    report.append_counted(&row_prefix, row_chars, repeat_rows, options.max_chars);
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
            report.sheets.push(grid.finish());
            Ok(())
        })();
        if let Err(error) = result {
            report.error("sheet", format!("sheet {}: {error}", index + 1));
        }
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
) -> xml::Result<()> {
    crate::tables::Context::new(report, "content.xml", kind, index + 1, options).odf_tables(node);
    let mut chars = 0;
    let mut result = Ok(());
    node.visit(&mut |n| {
        if n.is(T, "p") || n.is(T, "h") {
            match text(n) {
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

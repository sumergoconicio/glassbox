use crate::{
    InspectOptions, InspectReport,
    archive::Archive,
    office::{self, PKG, R, S},
    profile::{Budget, Grid},
    xml::{self, Node},
};

fn rich_text(node: &Node) -> String {
    let mut text = String::new();
    for child in node.children() {
        if child.is(S, "t") {
            text.push_str(&child.text());
        } else if child.is(S, "r") {
            text.push_str(&rich_text(child));
        }
    }
    text
}

pub(crate) fn xlsx(
    archive: &mut Archive,
    root: &Node,
    part: &str,
    report: &mut InspectReport,
    options: &InspectOptions,
) -> xml::Result<()> {
    let sheets = root.descendants(S, "sheet");
    report.counts.sheets = Some(sheets.len());
    let rels = office::relationships(archive, part)?;
    let mut strings = vec![];
    for rel in rels.children().filter(|n| {
        n.is(PKG, "Relationship") && n.attr("Type") == Some(&format!("{R}/sharedStrings"))
    }) {
        let target = office::related(
            &rels,
            part,
            rel.attr("Id").ok_or("shared strings missing id")?,
            "sharedStrings",
        )?;
        let root = office::read_xml(archive, &target)?;
        if !root.is(S, "sst") {
            return Err("invalid shared strings root".into());
        }
        strings = root
            .children()
            .filter(|n| n.is(S, "si"))
            .map(rich_text)
            .collect();
    }
    let mut budget = Budget::default();
    for (index, sheet) in sheets.iter().enumerate() {
        let result = (|| -> xml::Result<()> {
            let target = office::related(
                &rels,
                part,
                sheet.attr_ns(R, "id").ok_or("sheet missing id")?,
                "worksheet",
            )?;
            let root = office::read_xml(archive, &target)?;
            if !root.is(S, "worksheet") {
                return Err("invalid worksheet root".into());
            }
            let mut grid = Grid::new(sheet.attr("name").unwrap_or("").into(), index + 1);
            let mut chars = 0;
            let mut previous_row = 0;
            for row in root.descendants(S, "row") {
                let row_number: usize = row
                    .attr("r")
                    .map(str::parse)
                    .transpose()?
                    .unwrap_or(previous_row + 1);
                if row_number <= previous_row {
                    return Err("unordered or duplicate worksheet row".into());
                }
                previous_row = row_number;
                let mut col = 0;
                for cell in row.children().filter(|n| n.is(S, "c")) {
                    let (cell_row, column) = cell
                        .attr("r")
                        .map(crate::profile::position)
                        .transpose()?
                        .unwrap_or((row_number, col + 1));
                    if cell_row != row_number || column <= col {
                        return Err("unordered or duplicate cell coordinate".into());
                    }
                    col = column;
                    let raw = cell
                        .children()
                        .find(|n| n.is(S, "v"))
                        .map(Node::text)
                        .unwrap_or_default();
                    let (value, kind) = match cell.attr("t").unwrap_or("n") {
                        "s" => (
                            strings
                                .get(raw.parse::<usize>()?)
                                .ok_or("shared string index out of bounds")?
                                .clone(),
                            "string",
                        ),
                        "inlineStr" => (
                            cell.children()
                                .find(|n| n.is(S, "is"))
                                .map(rich_text)
                                .unwrap_or_default(),
                            "string",
                        ),
                        "str" => (raw, "string"),
                        "b" => (
                            match raw.as_str() {
                                "1" => "true",
                                "0" => "false",
                                _ => return Err("invalid boolean cell".into()),
                            }
                            .into(),
                            "bool",
                        ),
                        "d" => (raw, "date"),
                        "e" => (raw, "error"),
                        "n" => {
                            if !raw.is_empty() && !raw.parse::<f64>().is_ok_and(f64::is_finite) {
                                return Err("invalid numeric cell".into());
                            }
                            (raw, "number")
                        }
                        _ => return Err("unsupported cell type".into()),
                    };
                    let formula = cell.children().any(|n| n.is(S, "f"));
                    grid.cell((row_number, col), &value, kind, formula, 1, &mut budget)?;
                    chars += value.chars().count();
                    if col > 1 {
                        report.append_text("\t", options.max_chars);
                    }
                    report.append_text(&value, options.max_chars);
                }
                report.append_text("\n", options.max_chars);
            }
            for merge in root.descendants(S, "mergeCell") {
                let range = merge.attr("ref").ok_or("merge missing reference")?;
                let (a, b) = range.split_once(':').unwrap_or((range, range));
                crate::profile::position(a)?;
                crate::profile::position(b)?;
                grid.merge(range.into())?;
            }
            office::preflight(
                report,
                "sheet",
                index,
                chars,
                !root.descendants(S, "drawing").is_empty()
                    || !root.descendants(S, "legacyDrawing").is_empty(),
                options,
            );
            report.sheets.push(grid.finish());
            Ok(())
        })();
        if let Err(error) = result {
            report.error("sheet", format!("sheet {}: {error}", index + 1));
        }
    }
    finish(report, &budget);
    Ok(())
}

pub(crate) fn finish(report: &mut InspectReport, budget: &Budget) {
    if let Some(index) = report
        .sheets
        .iter()
        .enumerate()
        .max_by_key(|(i, s)| (s.rows, s.non_empty_cells, std::cmp::Reverse(*i)))
        .map(|(i, _)| i)
    {
        report.sheets[index].is_longest = true;
    }
    if budget.capped {
        report.error("profile", "cardinality capped: 16384 columns or 524288 distinct hashes; distinct counts are lower bounds");
    }
}

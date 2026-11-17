//! Telemetry caps and prose scoring ported from jsoninspect, not a language detector.
use crate::{Column, Sheet, xml};
use std::collections::{BTreeMap, HashSet};
use unicode_general_category::{GeneralCategory, get_general_category};

pub(crate) const MAX_COLUMNS: usize = 16_384;
pub(crate) const MAX_HASHES: usize = 524_288;
const STOPWORDS: &str = "a about above after again against all am an and any are as at be because been before being below between both but by can could did do does doing down during each few for from further had has have having he her here hers herself him himself his how i if in into is it its itself just me more most my myself no nor not now of off on once only or other our ours ourselves out over own same should so some such than that the their theirs them themselves then there these they this those through to too under until up very was we were what when where which while who whom why will with would you your yours yourself yourselves";

pub(crate) fn prose_density(value: &str) -> f64 {
    let text = value.trim();
    if text.len() < 15 {
        return 0.0;
    }
    let (mut words, mut stops, mut symbols, mut sentences, mut sentence_words) = (0, 0, 0, 0, 0);
    let mut valid = true;
    for word in text.split_whitespace() {
        words += 1;
        symbols += word
            .bytes()
            .filter(|b| b"{}[]();=<>\\|~^`".contains(b))
            .count();
        let cleaned = word.trim_matches(|c| ".,!?:;\"'()[]{}-_".contains(c));
        if STOPWORDS
            .split(' ')
            .any(|s| s.eq_ignore_ascii_case(cleaned))
        {
            stops += 1;
        } else if stops == 0 && valid {
            let letters = word.trim_end_matches(['.', '!', '?']);
            valid = !letters.is_empty()
                && letters.chars().all(|c| {
                    matches!(
                        get_general_category(c),
                        GeneralCategory::UppercaseLetter
                            | GeneralCategory::LowercaseLetter
                            | GeneralCategory::TitlecaseLetter
                            | GeneralCategory::ModifierLetter
                            | GeneralCategory::OtherLetter
                    )
                })
                && (sentence_words != 0
                    || letters.chars().next().is_some_and(|c| {
                        get_general_category(c) == GeneralCategory::UppercaseLetter
                    }));
            if valid {
                sentence_words += 1;
                if letters.len() != word.len() {
                    if sentence_words < 3 {
                        valid = false;
                    } else {
                        sentences += 1;
                        sentence_words = 0;
                    }
                }
            }
        }
    }
    if words < 3 {
        return 0.0;
    }
    let ratio = if stops > 0 {
        stops as f64 / words as f64
    } else if valid && sentence_words == 0 && sentences > 0 {
        sentences as f64 / words as f64
    } else {
        0.0
    };
    ((ratio * 2.2)
        * (1.0 - (symbols as f64 / text.len() as f64 * 4.0).min(1.0))
        * (text.len() as f64 / 120.0).min(1.0))
    .clamp(0.0, 1.0)
}

#[cfg(test)]
mod reference_tests {
    #[test]
    fn prose_scores_match_jsoninspect_reference() {
        let reference: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/prose-reference.json")).unwrap();
        for sample in reference["samples"].as_array().unwrap() {
            let text = sample["text"].as_str().unwrap();
            let expected = sample["score"].as_f64().unwrap();
            let actual = super::prose_density(text);
            assert!(
                (actual - expected).abs() < 1e-12,
                "{text:?}: {actual} != {expected}"
            );
        }
    }
}

pub(crate) fn preview(value: &str) -> String {
    if value.len() <= 60 {
        return value.into();
    }
    let mut end = 60;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

#[derive(Default)]
pub(crate) struct Budget {
    pub columns: usize,
    pub hashes: usize,
    pub capped: bool,
}

struct ColumnBuilder {
    column: Column,
    hashes: HashSet<u64>,
    score_sum: f64,
}

pub(crate) struct Grid {
    sheet: Sheet,
    columns: BTreeMap<usize, ColumnBuilder>,
    min_row: usize,
    min_col: usize,
}

impl Grid {
    pub fn new(name: String, index: usize) -> Self {
        Self {
            sheet: Sheet {
                name,
                index,
                used_range: None,
                rows: 0,
                cols: 0,
                cell_count: 0,
                non_empty_cells: 0,
                formula_count: 0,
                merged_ranges: vec![],
                is_longest: false,
                columns: vec![],
            },
            columns: BTreeMap::new(),
            min_row: usize::MAX,
            min_col: usize::MAX,
        }
    }

    pub fn cell(
        &mut self,
        position: (usize, usize),
        value: &str,
        kind: &str,
        formula: bool,
        repeats: usize,
        budget: &mut Budget,
    ) -> xml::Result<()> {
        let (row, col) = position;
        let end_row = row
            .checked_add(repeats.saturating_sub(1))
            .ok_or("row overflow")?;
        if row == 0 || col == 0 || end_row > 1_048_576 || col > MAX_COLUMNS || repeats == 0 {
            return Err("spreadsheet row/column/repetition cap exceeded".into());
        }
        self.sheet.rows = self.sheet.rows.max(end_row);
        self.sheet.cols = self.sheet.cols.max(col);
        self.min_row = self.min_row.min(row);
        self.min_col = self.min_col.min(col);
        if formula {
            self.sheet.formula_count += repeats;
        }
        if value.is_empty() {
            return Ok(());
        }
        self.sheet.non_empty_cells += repeats;
        if let std::collections::btree_map::Entry::Vacant(entry) = self.columns.entry(col) {
            if budget.columns >= MAX_COLUMNS {
                budget.capped = true;
                return Ok(());
            }
            budget.columns += 1;
            entry.insert(ColumnBuilder {
                column: Column {
                    index: col,
                    header: None,
                    inferred_type: "empty".into(),
                    non_empty_count: 0,
                    distinct_count: 0,
                    total_bytes: 0,
                    max_bytes: 0,
                    mean_bytes: 0.0,
                    avg_prose_score: 0.0,
                    is_prose_candidate: false,
                    sample_values: vec![],
                },
                hashes: HashSet::new(),
                score_sum: 0.0,
            });
        }
        let builder = self.columns.get_mut(&col).expect("column inserted");
        let c = &mut builder.column;
        let mut count = repeats;
        // A string in physical row 1 is an explicit header candidate, not data.
        if row == 1 && kind == "string" && !formula {
            c.header = Some(preview(value));
            count -= 1;
        }
        if count == 0 {
            return Ok(());
        }
        if c.inferred_type == "empty" {
            c.inferred_type = kind.into();
        } else if c.inferred_type != kind {
            c.inferred_type = "mixed".into();
        }
        c.non_empty_count += count;
        c.total_bytes += value.len() * count;
        c.max_bytes = c.max_bytes.max(value.len());
        let hash = value.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        if !builder.hashes.contains(&hash) {
            if budget.hashes < MAX_HASHES {
                builder.hashes.insert(hash);
                budget.hashes += 1;
                c.distinct_count += 1;
            } else {
                budget.capped = true;
            }
        }
        if kind == "string" {
            let score = prose_density(value);
            builder.score_sum += score * count as f64;
            c.is_prose_candidate |= value.len() >= 35 && score >= 0.20;
            for _ in 0..count.min(3usize.saturating_sub(c.sample_values.len())) {
                c.sample_values.push(preview(value));
            }
        }
        Ok(())
    }

    pub fn merge(&mut self, range: String) -> xml::Result<()> {
        if self.sheet.merged_ranges.len() >= 4096 {
            return Err("merged range cap exceeded".into());
        }
        self.sheet.merged_ranges.push(range);
        Ok(())
    }

    pub fn finish(mut self) -> Sheet {
        self.sheet.cell_count = self.sheet.rows * self.sheet.cols;
        if self.sheet.rows > 0 {
            self.sheet.used_range = Some(format!(
                "{}{}:{}{}",
                column_name(self.min_col),
                self.min_row,
                column_name(self.sheet.cols),
                self.sheet.rows
            ));
        }
        self.sheet.columns = self
            .columns
            .into_values()
            .map(|mut b| {
                let n = b.column.non_empty_count;
                if n > 0 {
                    b.column.mean_bytes = b.column.total_bytes as f64 / n as f64;
                    b.column.avg_prose_score = b.score_sum / n as f64;
                }
                b.column
            })
            .collect();
        self.sheet
    }
}

pub(crate) fn column_name(mut column: usize) -> String {
    let mut bytes = vec![];
    while column > 0 {
        column -= 1;
        bytes.push(b'A' + (column % 26) as u8);
        column /= 26;
    }
    bytes.reverse();
    String::from_utf8(bytes).expect("ASCII column")
}

pub(crate) fn position(value: &str) -> xml::Result<(usize, usize)> {
    let split = value
        .find(|c: char| c.is_ascii_digit())
        .ok_or("invalid cell coordinate")?;
    let (letters, row) = value.split_at(split);
    if letters.is_empty() || !letters.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err("invalid cell column".into());
    }
    let mut col = 0usize;
    for b in letters.bytes() {
        col = col
            .checked_mul(26)
            .and_then(|n| n.checked_add((b - b'A' + 1) as usize))
            .ok_or("column overflow")?;
    }
    let row: usize = row.parse()?;
    if row == 0 || row > 1_048_576 || col > MAX_COLUMNS {
        return Err("cell coordinate out of range".into());
    }
    Ok((row, col))
}

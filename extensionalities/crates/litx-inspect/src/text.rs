//! Module: glassbox-inspect plain text and source reader.
//! File: text.rs.
//! Responsibility: Extract UTF-8 logical lines with native-unit selection or legacy whole-text compatibility.

use crate::reports::{Completeness, ContentOrigin, NativeUnit};
use crate::{InspectOptions, InspectReport, binary::Source, xml};
use std::path::Path;

/// Bounded whole-text read window; matches the shared per-range byte cap so
/// text processing stays linear in source size and never fails only because
/// a large text file is large. Test builds shrink the window so the partial
/// coverage contract is verifiable with small fixtures.
#[cfg(not(test))]
const TEXT_WINDOW_BYTES: usize = 16 << 20;
#[cfg(test)]
const TEXT_WINDOW_BYTES: usize = 4 << 10;

/// Hard cap on allocated logical-line units per extraction. Every line in the
/// bounded window is counted so coverage stays truthful, while only a
/// bounded number of selected units is ever allocated; dense tiny-line
/// sources cannot drive unbounded unit allocation. Selected units withheld
/// by the cap are disclosed as truncated coverage, and explicit ranges that
/// fit inside the cap still complete. Test builds shrink the cap so the
/// contract is verifiable with small fixtures.
#[cfg(not(test))]
const TEXT_UNIT_CAP: u64 = 65_536;
#[cfg(test)]
const TEXT_UNIT_CAP: u64 = 2_048;

/// Reads UTF-8 text and emits either selected logical lines or legacy content.
///
/// Accepts a source path, destination report, and extraction controls. Returns
/// unit after recording one-based logical-line units for v2 callers. UTF-8 and
/// selection failures become `text` or `selection` diagnostics. Files beyond
/// the read window are truncated to whole lines inside the window and that
/// cut is disclosed as partial availability instead of a fabricated complete
/// count; line counting covers the whole window, while unit allocation stops
/// at the explicit unit cap and any withheld selected unit is disclosed as
/// truncated coverage. A trailing newline does not invent another line; a
/// final unterminated line counts only when the window holds it completely.
pub(crate) fn inspect(path: &Path, report: &mut InspectReport, options: &InspectOptions) {
    let result = (|| -> xml::Result<()> {
        let mut source = Source::open(path)?;
        let total = source.len as usize;
        let window = total.min(TEXT_WINDOW_BYTES);
        let bytes = source.range(0, window)?;
        let capped = total > window;
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            // The bounded window can split a multi-byte character at its end;
            // back off to the last complete boundary and disclose the cut.
            Err(error) if capped && bytes.len().saturating_sub(error.valid_up_to()) <= 4 => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])?
            }
            Err(error) => return Err(error.into()),
        };
        // Keep only whole logical lines inside the window so a cut line is
        // disclosed as partial coverage rather than emitted as a whole line.
        let text = if capped {
            match text.rfind('\n') {
                Some(position) => &text[..position + 1],
                None => "",
            }
        } else {
            text
        };
        let text = text.trim_start_matches('\u{feff}');
        if let Some(selection) = &options.native_selection {
            // Count every logical line without materializing them, so
            // availability stays exact for the window and allocation stays
            // bounded by the explicit unit cap below.
            let lines = text.lines().count() as u64;
            let plan = crate::selection::begin(report, selection, NativeUnit::LogicalLine, lines)?;
            if capped {
                report.native_available.completeness = Completeness::Partial;
            }
            let mut emitted: u64 = 0;
            for (index, line) in text.lines().enumerate() {
                if emitted >= TEXT_UNIT_CAP {
                    // The unit cap bounds allocation; withheld selected units
                    // are disclosed as truncated coverage, never dropped
                    // silently.
                    report.text.truncated = true;
                    break;
                }
                let number = index as u64 + 1;
                if plan.contains(number) {
                    report.native_units.push(crate::selection::text_unit(
                        number,
                        NativeUnit::LogicalLine,
                        None,
                        line.to_string(),
                        ContentOrigin::Native,
                    ));
                    emitted += 1;
                }
            }
        } else {
            report.append_text(text, options.max_chars);
            if report.format.extension == "md" {
                crate::markdown::append(report, text, options, "native_structure");
            }
        }
        if capped {
            report.text.truncated = true;
        }
        Ok(())
    })();
    if let Err(error) = result {
        report.error("text", error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reports::{NativeRange, NativeSelection};
    use std::path::PathBuf;

    /// Creates a unique ignored fixture directory for text reader tests.
    ///
    /// Accepts a label. Returns a new target path. Creation failures panic;
    /// no prior evidence or user file is removed or overwritten.
    fn workdir(label: &str) -> PathBuf {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tests")
            .join(format!(
                "text-window-{label}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Creates selection-enabled options over a fresh report.
    ///
    /// Accepts a native selection. Returns options whose report captures
    /// native logical-line units, mirroring the v2 extraction controls.
    fn selected(selection: NativeSelection) -> (InspectOptions, InspectReport) {
        let options = InspectOptions {
            native_selection: Some(selection),
            ..InspectOptions::default()
        };
        (options, InspectReport::default())
    }

    /// Proves a window cut keeps whole lines and discloses partial coverage.
    ///
    /// Accepts no parameters. Returns unit after reading a file one line past
    /// the test window. A fabricated complete count, a cut line emitted as a
    /// whole unit, or a missing truncation flag panics; fixture I/O can panic.
    #[test]
    fn window_cut_keeps_whole_lines_and_discloses_partial_coverage() {
        let directory = workdir("cut");
        let path = directory.join("large.txt");
        let line = "aaaaaaaaaa\n";
        let content = line.repeat(400);
        std::fs::write(&path, content).unwrap();
        let (options, mut report) = selected(NativeSelection::All);
        inspect(&path, &mut report, &options);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.text.truncated);
        assert_eq!(report.native_available.completeness, Completeness::Partial);
        // The 4096-byte window holds 372 whole 11-byte lines and a 4-byte cut;
        // only the whole lines may become units.
        assert_eq!(report.native_available.value, Some(372));
        assert_eq!(report.native_units.len(), 372);
        assert_eq!(report.native_units[371].text.as_deref(), Some("aaaaaaaaaa"));
    }

    /// Proves a window cut through a multi-byte character still reads lines.
    ///
    /// Accepts no parameters. Returns unit after reading a file whose window
    /// splits one multi-byte character. A hard UTF-8 failure instead of the
    /// complete-line prefix, or a fabricated complete count, panics.
    #[test]
    fn window_cut_through_a_multibyte_character_still_reads_whole_lines() {
        let directory = workdir("multibyte");
        let path = directory.join("multibyte.txt");
        // 2047 two-byte lines end at offset 4094; the three-byte character
        // then straddles the window boundary and must not fail the read.
        let mut content = "a\n".repeat(2047);
        content.push('\u{20ac}');
        content.push_str("more\n");
        std::fs::write(&path, content).unwrap();
        let (options, mut report) = selected(NativeSelection::All);
        inspect(&path, &mut report, &options);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.text.truncated);
        assert_eq!(report.native_available.value, Some(2047));
        assert_eq!(report.native_available.completeness, Completeness::Partial);
        assert_eq!(report.native_units.len(), 2047);
    }

    /// Proves an unterminated final line still counts in an uncapped file.
    ///
    /// Accepts no parameters. Returns unit after reading a small unterminated
    /// file. The existing complete-count contract must survive the window
    /// change; fixture I/O can panic.
    #[test]
    fn unterminated_final_line_counts_in_an_uncapped_file() {
        let directory = workdir("unterminated");
        let path = directory.join("small.txt");
        std::fs::write(&path, "zero\none\ntwo").unwrap();
        let (options, mut report) = selected(NativeSelection::All);
        inspect(&path, &mut report, &options);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!report.text.truncated);
        assert_eq!(report.native_available.value, Some(3));
        assert_eq!(report.native_available.completeness, Completeness::Exact);
        assert_eq!(report.native_units.len(), 3);
        assert_eq!(report.native_units[2].text.as_deref(), Some("two"));
    }

    /// Proves the unit cap bounds allocated selected units truthfully.
    ///
    /// Accepts no parameters. Returns unit after reading a file whose line
    /// count exceeds the reduced test cap. More allocated units than the cap,
    /// a fabricated availability count, or a missing truncation disclosure
    /// panics; fixture I/O can panic.
    #[test]
    fn unit_cap_bounds_allocated_selected_units_truthfully() {
        let directory = workdir("unit-cap");
        let path = directory.join("tiny-lines.txt");
        // 4096 one-byte empty lines fit the test window exactly, so this is a
        // pure unit-cap trip: availability stays exact and uncapped.
        std::fs::write(&path, "\n".repeat(4096)).unwrap();
        let (options, mut report) = selected(NativeSelection::All);
        inspect(&path, &mut report, &options);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.native_available.value, Some(4096));
        assert_eq!(report.native_available.completeness, Completeness::Exact);
        assert_eq!(report.native_units.len(), TEXT_UNIT_CAP as usize);
        assert!(report.text.truncated);
        assert_eq!(report.native_units[0].unit_index, 1);
        assert_eq!(
            report.native_units[TEXT_UNIT_CAP as usize - 1].unit_index,
            TEXT_UNIT_CAP
        );
    }

    /// Proves explicit ranges that fit the unit cap still complete.
    ///
    /// Accepts no parameters. Returns unit after selecting a late explicit
    /// range of the same dense source. Missing range units or a truncation
    /// disclosure for a satisfiable range panics; fixture I/O can panic.
    #[test]
    fn explicit_ranges_beyond_the_cap_prefix_still_complete() {
        let directory = workdir("late-range");
        let path = directory.join("tiny-lines.txt");
        std::fs::write(&path, "\n".repeat(4096)).unwrap();
        let (options, mut report) = selected(NativeSelection::Ranges {
            ranges: vec![NativeRange {
                start: 4000,
                end: 4005,
            }],
        });
        inspect(&path, &mut report, &options);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.native_available.value, Some(4096));
        assert_eq!(report.native_available.completeness, Completeness::Exact);
        assert_eq!(report.native_units.len(), 6);
        assert!(!report.text.truncated);
        assert_eq!(
            report
                .native_units
                .iter()
                .map(|unit| unit.unit_index)
                .collect::<Vec<_>>(),
            [4000, 4001, 4002, 4003, 4004, 4005]
        );
    }

    /// Proves genuinely invalid UTF-8 still fails truthfully when uncapped.
    ///
    /// Accepts no parameters. Returns unit after reading a small invalid file.
    /// A silent partial read or a missing text diagnostic panics.
    #[test]
    fn invalid_utf8_still_fails_truthfully_when_uncapped() {
        let directory = workdir("invalid");
        let path = directory.join("invalid.txt");
        std::fs::write(&path, b"valid\n\xFFinvalid\n").unwrap();
        let (options, mut report) = selected(NativeSelection::All);
        inspect(&path, &mut report, &options);
        assert!(!report.errors.is_empty());
        assert!(report.native_units.is_empty());
        assert!(report.errors.iter().any(|error| error.stage == "text"));
    }
}

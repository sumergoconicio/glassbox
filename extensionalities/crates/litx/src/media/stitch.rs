//! Module: glassbox-cli ASR overlap transcript stitcher.
//! File: media/stitch.rs.
//! Responsibility: Deterministically deduplicate >=3-word normalized suffix/prefix overlaps across adjacent overlapped ASR windows while leaving non-overlapped chunks untouched.

use super::asr::TranscriptChunk;

/// Minimum number of consecutive normalized words required to strip an overlap boundary.
pub const MIN_OVERLAP_WORDS: usize = 3;

/// Maximum boundary word window inspected for suffix/prefix overlap matching.
pub const MAX_OVERLAP_WORDS: usize = 24;

/// Normalizes a whitespace-delimited token to lowercase alphanumeric characters.
///
/// Accepts a raw transcript token. Returns a string retaining only
/// alphanumeric characters mapped to lowercase, stripping surrounding
/// punctuation and casing differences. Performs no I/O and cannot fail.
fn normalize_word(word: &str) -> String {
    word.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Finds and strips the longest `>= 3`-word normalized overlap between `previous` and `next`.
///
/// Accepts the accumulated prior transcript text and the next raw window's
/// trimmed text. Searches up to `MAX_OVERLAP_WORDS` words at the suffix of
/// `previous` and prefix of `next` using lowercase alphanumeric word
/// normalization. Returns `(matched_word_count, stitched_next_text)` where
/// `matched_word_count` is `0` and `stitched_next_text` equals `next.trim()`
/// when fewer than `MIN_OVERLAP_WORDS` match. Never panics.
pub fn stitch_boundary(previous: &str, next: &str) -> (usize, String) {
    let trimmed_next = next.trim();
    if trimmed_next.is_empty() {
        return (0, String::new());
    }

    let tail: Vec<String> = previous
        .split_whitespace()
        .rev()
        .take(MAX_OVERLAP_WORDS)
        .map(normalize_word)
        .collect();
    let next_words: Vec<&str> = trimmed_next.split_whitespace().collect();
    let head: Vec<String> = next_words
        .iter()
        .take(MAX_OVERLAP_WORDS)
        .map(|word| normalize_word(word))
        .collect();

    let upper = tail.len().min(head.len());
    let matched = (MIN_OVERLAP_WORDS..=upper)
        .rev()
        .find(|&count| {
            head[..count].iter().all(|word| !word.is_empty())
                && tail[..count].iter().rev().eq(&head[..count])
        })
        .unwrap_or(0);

    if matched == 0 {
        return (0, trimmed_next.to_owned());
    }

    (matched, next_words[matched..].join(" "))
}

/// Stitches a sequence of raw ASR chunks into deduplicated display chunks for report output.
///
/// Accepts the raw per-window [`TranscriptChunk`] slice and `overlapped`
/// (`true` for `full` and `short` modes with 4 s overlap, `false` for
/// non-overlapped `adaptive` mode). Callers store the returned stitched display
/// text in `content.units` while preserving raw per-window timestamps
/// (`start_ms`, `end_ms`, and `media.asr.sampled_ranges_ms`). Returns a new
/// `Vec<TranscriptChunk>` with trimmed text and each surviving chunk's original
/// `start_ms` and `end_ms`; when `overlapped` is `true`, `>= 3`-word normalized
/// prefix overlaps matching the accumulated prior suffix are stripped and
/// empty/fully-duplicated chunks are omitted; when `overlapped` is `false`,
/// non-empty trimmed chunks are returned untouched without stripping. Cannot fail.
pub fn stitch_chunks(chunks: &[TranscriptChunk], overlapped: bool) -> Vec<TranscriptChunk> {
    let mut result = Vec::with_capacity(chunks.len());

    if !overlapped {
        for chunk in chunks {
            let trimmed = chunk.text.trim();
            if !trimmed.is_empty() {
                result.push(TranscriptChunk {
                    start_ms: chunk.start_ms,
                    end_ms: chunk.end_ms,
                    text: trimmed.to_owned(),
                });
            }
        }
        return result;
    }

    let mut accumulated = String::new();
    for chunk in chunks {
        let trimmed = chunk.text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (_matched, addition) = stitch_boundary(&accumulated, trimmed);
        if addition.is_empty() {
            continue;
        }
        if !accumulated.is_empty() {
            accumulated.push(' ');
        }
        accumulated.push_str(&addition);
        result.push(TranscriptChunk {
            start_ms: chunk.start_ms,
            end_ms: chunk.end_ms,
            text: addition,
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(start_ms: u64, end_ms: u64, text: &str) -> TranscriptChunk {
        TranscriptChunk {
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    #[test]
    fn deduplicates_exact_three_or_more_word_overlap() {
        let input = vec![
            chunk(0, 30_000, "We discuss usable energy today."),
            chunk(26_000, 56_000, "Usable energy today, then fuels."),
            chunk(
                52_000,
                82_000,
                "energy today then fuels and storage options.",
            ),
        ];

        let stitched = stitch_chunks(&input, true);
        assert_eq!(stitched.len(), 3);
        assert_eq!(stitched[0].text, "We discuss usable energy today.");
        assert_eq!(stitched[1].start_ms, 26_000);
        assert_eq!(stitched[1].end_ms, 56_000);
        assert_eq!(stitched[1].text, "then fuels.");
        assert_eq!(stitched[2].text, "and storage options.");
    }

    #[test]
    fn preserves_fewer_than_three_word_overlap() {
        let input = vec![
            chunk(0, 30_000, "Usable energy"),
            chunk(26_000, 56_000, "Usable energy matters greatly."),
        ];

        let stitched = stitch_chunks(&input, true);
        assert_eq!(stitched.len(), 2);
        assert_eq!(stitched[0].text, "Usable energy");
        assert_eq!(stitched[1].text, "Usable energy matters greatly.");
    }

    #[test]
    fn handles_punctuation_and_case_differences_across_boundary() {
        let (matched, addition) = stitch_boundary(
            "So, ask not what YOUR country can do!",
            "your COUNTRY, can do: ask what you can do.",
        );
        assert_eq!(matched, 4);
        assert_eq!(addition, "ask what you can do.");
    }

    #[test]
    fn skips_empty_whitespace_and_fully_absorbed_chunks() {
        let input = vec![
            chunk(0, 30_000, "   "),
            chunk(26_000, 56_000, "  Alpha beta gamma delta. "),
            chunk(52_000, 60_000, "\t\n"),
            chunk(56_000, 62_000, "beta, GAMMA delta!"),
            chunk(60_000, 90_000, "beta gamma delta epsilon zeta."),
        ];

        let stitched = stitch_chunks(&input, true);
        assert_eq!(stitched.len(), 2);
        assert_eq!(stitched[0].text, "Alpha beta gamma delta.");
        assert_eq!(stitched[1].text, "epsilon zeta.");
    }

    #[test]
    fn non_overlapped_mode_returns_trimmed_chunks_untouched() {
        let input = vec![
            chunk(0, 30_000, "  We discuss usable energy today.  "),
            chunk(900_000, 930_000, "   "),
            chunk(1_800_000, 1_830_000, "  Usable energy today, then fuels. "),
        ];

        let stitched = stitch_chunks(&input, false);
        assert_eq!(stitched.len(), 2);
        assert_eq!(stitched[0].start_ms, 0);
        assert_eq!(stitched[0].text, "We discuss usable energy today.");
        assert_eq!(stitched[1].start_ms, 1_800_000);
        assert_eq!(stitched[1].text, "Usable energy today, then fuels.");
    }
}

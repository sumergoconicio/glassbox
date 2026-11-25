# CLI reference and parsing

`glassbox` is one executable with four top-level commands.
The runtime `--help` output of your build is the authoritative reference.

## Command surface

| Command | Purpose |
| --- | --- |
| `glassbox parse FILE [ARGS]` | Upstream LiteParse parsing, unchanged. |
| `glassbox inspect PATH` | Schema v3 metadata-only inspection (file or directory). |
| `glassbox extract PATH` | Schema v3 content extraction with native-unit selection and `bodymatter:v1` Markdown. |
| `glassbox defaults` | Print the upstream library default configuration as JSON. |

## Upstream parsing: `parse`

`glassbox parse` routes directly to the upstream CLI without changes.
It preserves every upstream command, option, default, exit status, and output file.
Hidden and future upstream commands work through automatic discovery.

```sh
"$GLASSBOX" parse /path/to/document.pdf
"$GLASSBOX" parse --help
```

Nested commands keep their upstream names, such as `glassbox parse is-complex /path/to/document.pdf`.
Help and error labels show Glassbox branding while output and attribution remain upstream.
Downstream flags do not apply to `parse`.

## Inspection: `inspect`

| Flag | Meaning |
| --- | --- |
| `PATH` | File or directory to inspect. Required without `--batch`. |
| `--batch` | Read one `{"id":...,"path":"..."}` JSON request per stdin line. Conflicts with `--pretty`, `--format`, and `--thumbnail`. |
| `--output-dir DIR` | Save a content-free Markdown summary and a v3 JSON report per file. Never overwrites. |
| `--recursive` | With a directory, recurse into same-mount real subdirectories. Links and other mounts are skipped. |
| `--jobs N` | Concurrent worker limit from 1 to 32. Default 1. Shared file and OCR budget. |
| `--thumbnail PATH` | Render one bounded thumbnail where supported. No OCR. Off by default. |
| `--thumbnail-max-edge N` | Thumbnail edge cap in pixels. Default 320. Range 1–4096. |
| `--format` | `json` (default), `text`, or `markdown`. Text and Markdown print content-free v3 summaries. |
| `--pretty` | Indent JSON output. Single-file JSON only. |
| `--full-hash` | Stream full SHA-256 even for files larger than 128 MiB. Works in single-file, directory, and batch modes. |
| `--verbose` | Include reader `provenance` in v3 JSON and summary outputs. Works in single-file, directory, and batch modes. |

File inspection never returns body text, row values, or OCR output.
Files up to 128 MiB receive `file.sha256`.
Larger files receive a `glassbox-sampled-sha256/v1` `file.fingerprint` unless `--full-hash` is set.

Directory inspection emits ordered JSONL carrying schema v3 inspect reports.
By default it scans immediate regular files only; add `--recursive` for same-mount subdirectories.
Directory inspection rejects `--pretty`, `--thumbnail`, and non-JSON `--format` with exit 64.
Saved `--output-dir` artifacts must sit outside the scanned directory.

## Extraction: `extract`

| Flag | Meaning |
| --- | --- |
| `PATH` | File to extract. Required without `--batch`. |
| `--pages [SELECTION]` | Select native units. Bare flag: first five. Integer: first N. Ranges: `3-7,10`. |
| `--max-pages N` | Deprecated alias for `--pages N`. Conflicts with `--pages`. |
| `--no-ocr` | Disable local OCR. Conflicts with `--probe-ocr` and `--identify`. |
| `--ocr-language LANG` | OCR language, such as `en`, `eng`, or `deu+fra`. Default `eng`. Reports the canonical code. |
| `--identify` | Compatibility flag. Local OCR stays limited to pages that need it. |
| `--probe-ocr` | Compatibility flag. Automatic local OCR behavior stays unchanged. |
| `--max-tokens N` | Bound whitespace-delimited word tokens. Not model subword tokens. |
| `--max-chars N` | Bound output characters. Default 67,108,864. Fixed reader caps still apply. |
| `--max-units N` | Bound processed preview or probe units. Default 5. |
| `--thumbnail PATH` | Save a bounded thumbnail alongside extraction. |
| `--thumbnail-max-edge N` | Thumbnail edge cap in pixels. Default 320. Range 1–4096. |
| `--packet` | Deprecated. Schema v3 `content.units` carries the extracted payload. |
| `--output-dir DIR` | Save Markdown content and a v3 JSON report per file. Never overwrites. |
| `--full-hash` | Stream full SHA-256 for embedded metadata even above 128 MiB. |
| `--verbose` | Include reader and extraction `provenance` in v3 JSON reports. |
| `--no-markers` | Omit `bodymatter:v1` structure comments from stdout Markdown and saved `.md` artifacts. |
| `--format FORMAT` | `json` (default, schema v3 extract report), `text` (coverage line plus `[unit N ...]` sections), or `markdown` (`bodymatter:v1` Markdown unless `--no-markers` is set). |
| `--batch`, `--jobs N`, `--pretty` | Same behavior as `inspect`. |

Selection indices are one-based, inclusive, deduplicated, and returned in source order.
Place a bare `--pages` flag after `PATH` so the path is not parsed as its value.
A bare count above the document total completes with all available units.
An explicit range wholly past the document total fails with exit 1.
Dataset outputs stay bounded previews.
See [Formats and limits](formats.md) for native units per format.

## Exit statuses

| Outcome | Downstream exit |
| --- | ---: |
| Complete requested operation | 0 |
| Partial result or unsupported operation | 2 |
| Operational failure | 1 |
| Invalid invocation | 64 |

`glassbox parse` keeps upstream exit codes.
Batch runs aggregate by severity: invalid requests (64) beat failures (1), which beat partial results (2), which beat complete (0).
Reports print to stdout; diagnostics print to stderr or inside report JSON.
Intentional `--pages` subsets can leave source coverage `partial` while the requested operation exits 0.

## Command and file collisions

Nested `parse` subcommands reserve their bare token.
Pass `./screenshot`, an absolute path, or `glassbox parse FILE` when a filename matches a subcommand.

## Migrations

| Old usage | Current behavior |
| --- | --- |
| `inspect --identify`, `--max-pages`, `--max-tokens`, `--max-chars`, `--probe-ocr`, `--no-ocr`, `--ocr-language`, `--packet`, `--no-markers` | Rejected with exit 64 and migration advice. Use `glassbox extract`. |
| `inspect` body previews | Removed. Inspection is metadata-only. Use `glassbox extract`. |
| `extract --max-pages N` | Deprecated; maps to `--pages N` with a warning. |
| `extract --packet` | Deprecated; read v3 `content.units`. |
| Schema v1 or v2 JSON reports | Replaced by schema v3 across all reports. No `--schema 2` flag exists. See [Reports](reports.md). |
| `litx` executable name | Retained as a compatibility alias binary for `glassbox`. Prefer `glassbox` in new commands and scripts. |

See [Automation](automation.md) for batch and directory rules.
See [Reports](reports.md) for schema v3 JSON contracts.
See [OCR and troubleshooting](ocr-and-troubleshooting.md) for OCR models and common errors.

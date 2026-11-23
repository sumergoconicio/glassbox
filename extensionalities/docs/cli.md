# CLI reference and parsing

`glassbox` is one executable with four top-level commands.
The runtime `--help` output of your build is the authoritative reference.

## Command surface

| Command | Purpose |
| --- | --- |
| `glassbox parse FILE [ARGS]` | Upstream LiteParse parsing, unchanged. |
| `glassbox inspect PATH` | Metadata-only inspection. |
| `glassbox extract PATH` | Content extraction with native-unit selection. |
| `glassbox defaults` | Print the upstream library default configuration as JSON. |

## Upstream parsing: `parse`

`glassbox parse` routes through the upstream CLI without changes.
It accepts every upstream command, option, default, status, and file output.
Hidden and future upstream commands work through automatic discovery.
No copied flag list is maintained, so new upstream commands appear without a Glassbox release note.

```sh
"$GLASSBOX" parse /path/to/document.pdf
"$GLASSBOX" parse --help
```

Nested commands keep their upstream names, for example `glassbox parse is-complex /path/to/document.pdf`.
Help and error identity show Glassbox branding.
Returned content and attribution stay upstream.
Upstream exit statuses are unchanged.
Downstream flags do not apply to `parse`.

## Inspection: `inspect`

| Flag | Meaning |
| --- | --- |
| `PATH` | File or directory to inspect. Required without `--batch`. |
| `--batch` | Read one `{"id":...,"path":"..."}` JSON request per stdin line. Conflicts with `--pretty`, `--format`, `--thumbnail`. |
| `--output-dir DIR` | Save a v2 JSON report and a text summary per file. Never overwrites. |
| `--recursive` | With a directory, recurse into same-mount real subdirectories. Links and other mounts are skipped. |
| `--jobs N` | Job count from 1 to 32. Default 1. One shared file and OCR budget. |
| `--thumbnail PATH` | Request one bounded thumbnail where supported. No OCR. Off by default. |
| `--thumbnail-max-edge N` | Thumbnail edge limit in pixels. Default 320. Range 1–4096. |
| `--format` | `json` (default), `text`, or `markdown`. Text and Markdown are metadata-only summaries. |
| `--pretty` | Indent JSON output. |

A file inspection never returns body text, row values, or OCR output.
It streams a full-file SHA-256 by default.

A directory inspection emits ordered JSONL only: one record per regular file, immediate entries by default.
Each record carries a numeric ID, a sequence number, and a report envelope.
Directory inspection rejects `--pretty` and other non-JSON output with exit 64.
It also rejects a thumbnail path and non-JSON formats with exit 64.
Saved artifacts stay outside the scanned directory.

## Extraction: `extract`

| Flag | Meaning |
| --- | --- |
| `PATH` | File to extract. Required without `--batch`. |
| `--pages [SELECTION]` | Select native units. Bare flag: first five. Integer: first N. Ranges: `3-7,10`. |
| `--max-pages N` | Deprecated. Maps to `--pages N`. Conflicts with `--pages`. |
| `--no-ocr` | Disable local OCR. Conflicts with `--probe-ocr` and `--identify`. |
| `--ocr-language LANG` | OCR language for extraction, such as `en`, `eng`, or `deu+fra`. Default `eng`. Reports the canonical spelling. |
| `--identify` | Compatibility flag. Local OCR stays limited to pages that need it. |
| `--probe-ocr` | Accepted compatibility flag. It does not change automatic local OCR. |
| `--max-tokens N` | Bound whitespace-delimited word tokens. Not model-specific tokens. |
| `--max-chars N` | Bound output characters. Default 67,108,864 characters. Fixed reader byte caps still apply. |
| `--max-units N` | Bound processed preview or probe units. Default 5. |
| `--thumbnail PATH` | Save a bounded thumbnail with the extraction. |
| `--thumbnail-max-edge N` | Thumbnail edge limit in pixels. Default 320. Range 1–4096. |
| `--packet` | Deprecated. The v2 `content.units` array carries the payload. |
| `--output-dir DIR` | Save Markdown content and a v2 JSON report per file. Never overwrites. |
| `--batch`, `--jobs N`, `--format`, `--pretty` | Same behavior as `inspect`. |

Selection indices are one-based, inclusive, deduplicated, and returned in source order.
`--pages 3-7,10` returns units 3, 4, 5, 6, 7, and 10.
Put a bare `--pages` after the file path, so the path is not read as the flag value.
A count above the document length completes with the available units, without fake omissions.
An explicit range wholly outside the document fails clearly; it never produces an empty success.
Parser safety caps still apply to full requests.
Dataset outputs stay bounded previews.
See [Formats and limits](formats.md) for the per-format units.

## Exit statuses

| Outcome | Downstream exit |
| --- | ---: |
| Complete requested operation | 0 |
| Partial result or unsupported operation | 2 |
| Operational failure | 1 |
| Invalid invocation | 64 |

`glassbox parse` keeps the upstream statuses.
Batch runs aggregate by severity: any invalid request gives 64, failures give 1, partial results give 2.
Results print to stdout; diagnostics print to stderr or inside reports.
Intentional selection omissions can leave source coverage partial while the requested operation completes.

## Command and file collisions

Nested `parse` commands reserve their bare token.
Name a file unambiguously with `./screenshot`, an absolute path, or the explicit nested form `glassbox parse FILE`.

## Migrations

| Old usage | Current behavior |
| --- | --- |
| `inspect --identify`, `--max-pages`, `--max-tokens`, `--max-chars`, `--probe-ocr`, `--no-ocr`, `--ocr-language`, `--packet` | Rejected with exit 64 and migration advice. Use `extract`. |
| `inspect` body previews | Removed. Inspection is metadata-only. Use `extract` for content. |
| `extract --max-pages N` | Deprecated; maps to `--pages N` with a warning. |
| `extract --packet` | Deprecated; use v2 `content.units`. |
| v1 JSON report fields | Replaced by the v2 schema. See [Reports](reports.md). |

For batch, directory, and job behavior see [Automation](automation.md).
For report fields see [Reports](reports.md).
For OCR data and failures see [OCR and troubleshooting](ocr-and-troubleshooting.md).

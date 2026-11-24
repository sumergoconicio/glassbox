---
title: "Content extraction"
type: doc
content-type: Runbook
parent: docs/README.md
---

# Content extraction

`glassbox extract` returns document content as native units.
A native unit is a page, slide, sheet, table, chapter, block, line, record, or row defined by the format.
Each unit carries its source index and provenance.
The report discloses what was returned and what was omitted.

## Select units with `--pages`

By default, `extract` requests every native unit within fixed reader caps.
Pass `--pages` to select a subset.

| Command | Selection |
| --- | --- |
| `glassbox extract FILE` | All units |
| `glassbox extract FILE --pages` | First five units |
| `glassbox extract FILE --pages 12` | First twelve units |
| `glassbox extract FILE --pages 3-7,10` | Units 3 to 7 and unit 10 |

Selector rules:

- Indices are one-based and must ascend within each range.
- Repeated and overlapping ranges merge into sorted, disjoint order.
- A bare count past the document total returns all available units, completes, and exits 0.
- An explicit range wholly past the document total fails and exits 1.
- Malformed, zero, descending, or overflowing values exit 64 before opening any file.

`--max-pages N` is deprecated.
It maps to `--pages N` and prints a warning.
Combining `--pages` and `--max-pages` exits 64.

## Coverage is disclosed, never guessed

Selection coverage and operation status stay separate:

- A fully emitted subset completes and exits 0.
- Unrequested units stay disclosed through `selection.omitted: "selection"`.
- Content lost to caps, parser errors, or OCR failures marks the report `partial` and exits 2.
- `selection.returned_indices` lists returned units in source order.
- `selection.available` states the available total and its completeness.
- Withheld units set `selection.omitted` to `safety_limit` or `partial_failure` and mark the report `partial`.

## `bodymatter:v1` Markdown structure markers

`glassbox extract --format markdown` emits clean Markdown with `bodymatter:v1` HTML comment markers by default.
Saved `.md` files under `--output-dir` use the exact same renderer.

### Marker syntax

Each marker occupies one line and sits between blank lines:

```text
<!-- bodymatter:v1 <ELEMENT ATTR="VALUE" .../> -->
```

### Elements

| Element | Source formats | Attributes | Notes |
| --- | --- | --- | --- |
| `page` | PDF | `number` (required), `label` (optional), `origin` (`native`\|`ocr`\|`mixed`, required), `completeness="partial"` (optional) | `label` comes from PDF `/PageLabels` when defined. |
| `sheet` | XLSX, XLSM, ODS, CSV, TSV | `number` (required), `name` (optional) | `name` appears for named worksheets and is omitted for unnamed CSV or TSV tables. Followed by a GFM pipe table (`\| --- \|`). |
| `slide` | PPTX, ODP | `number` (required) | Physical slide index. |
| `table` | DOCX, ODT | `number` (required) | One-based document table index in source order. |
| `chapter` | EPUB | `number` (required), `href` (optional) | `href` carries the container-relative spine path when known. |
| `page-break` | DOCX | `number` (required) | Emitted only for explicit `<w:br w:type="page"/>` breaks; `<w:lastRenderedPageBreak/>` is ignored. |

Other units (paragraphs, lines, JSON scalars, XML elements, archive entries, and images) emit no marker.

### Placement, escaping, and anti-forgery rules

1. **Forward scope**: each marker starts a region. The region runs until the next same-element marker or end of output.
2. **Source numbering**: Every `number` attribute keeps its original one-based source ordinal under partial `--pages` selections, matching full extraction. This includes DOCX `page-break` and DOCX or ODT `table` markers, which read `marker_index`. Glassbox never invents page numbers for reflowable formats.
3. **Attribute escaping**: Glassbox XML-escapes `&`, `<`, `>`, `"`, `\r`, `\n`, `\t`, and Unicode line terminators (`U+0085`, `U+2028`, `U+2029`). It replaces XML 1.0 illegal control and noncharacter scalars with `U+FFFD`, and replaces every `--` with `&#45;&#45;`.
4. **HTML, comment, and control neutralization**: Glassbox escapes every source `<` as `&lt;` in body text and table cells. This neutralizes `<!--` comments and raw HTML tags such as `<script>`. Extracted body text normalizes Unicode line terminators (`U+0085`, `U+2028`, `U+2029`, `\x0b`, `\x0c`) to newlines and replaces forbidden C0 controls (including `0x1B`) with `U+FFFD`. Generated `bodymatter:v1` comments and table `<br>` breaks stay intact.
5. **Minimal Markdown escaping**: Glassbox escapes only line-leading block triggers after 0–3 spaces. These include `#`, `>`, `-`, `+`, `*`, ordered lists, thematic breaks, Setext underlines, and code fences. Ordinary inline prose stays clean.
6. **GFM tables**: Row-bearing units render as GFM pipe tables with a `| --- |` separator row. Table cells escape `<` as `&lt;` and `|` as `\|`, and replace forbidden C0 controls (including `0x1B`) with `U+FFFD`. They map line breaks, Unicode line terminators (`U+0085`, `U+2028`, `U+2029`, `\x0b`, `\x0c`), and tabs to `<br>`. Duplicate TSV text stays omitted.
7. **No synthetic headings**: Markdown output omits `# Glassbox extraction`, coverage headers, and `## Unit N (...)` headings. Coverage stays in the JSON report and plain-text output.

### Example Markdown output

```markdown
<!-- bodymatter:v1 <page number="1" label="iv" origin="native"/> -->

Quarterly results improved across all regions.

<!-- bodymatter:v1 <page number="2" label="1" origin="ocr" completeness="partial"/> -->

Appendix A — scanned tables.
```

### Consumer recipe

Process extracted Markdown in three steps:

1. Match lines against `^<!-- bodymatter:v1 (<.*/>) -->$`.
2. Parse capture group 1 with any standard XML parser.
3. Remove the marker line before text chunking.

### Suppressing markers with `--no-markers`

Pass `--no-markers` to omit every `bodymatter:v1` line from stdout Markdown and saved `--output-dir` `.md` files.
Source `<` escaping, minimal block escaping, and clean GFM tables remain active.

## Safety caps

Extraction applies fixed safety caps to every request:

- Characters: 67,108,864 by default (`--max-chars` lowers or changes this budget; fixed reader caps still apply).
- Tokens: unlimited by default (`--max-tokens` bounds whitespace-delimited words, not model subword tokens).
- Tabular headers and spreadsheet rows: CSV and TSV headers obey `--max-chars` and `--max-tokens` budgets alongside data rows. Spreadsheet first rows carry actual cell values (never clipped metadata profiles).
- Dataset previews: five units by default (`--max-units` sets this limit).
- PDF: 4096 pages, 64 MiB decoded text, and a 128 MiB shared OCR raster budget.
- Container readers: 256 MiB physical input and 16 MiB per read range.
- Plain text: 16 MiB read window and 65,536 emitted selected lines.
- DOCX and PPTX: 65,536 characters per unit. Clipped units keep text, append `[unit text truncated]`, and turn `partial`.

See [Formats and limits](formats.md) for full per-format caps.
Glassbox discloses every cap hit through partial status, diagnostics, or truncation markers.

## OCR

Extraction runs local OCR only on pages that need it.
OCR is enabled by default; pass `--no-ocr` to disable it.
The default OCR language is English (`eng`).
Pass `--ocr-language LANG` to choose another language (for example `en`, `eng`, or `deu+fra`).
OCR failure never discards native text; the report retains native text and records a diagnostic.
See [OCR and troubleshooting](ocr-and-troubleshooting.md) for model lookup and errors.

## Output forms and hashing

- **JSON (default)**: schema v3 extract report (`schema_version: 3`, `kind: "extract"`) with embedded v3 `metadata`, `selection`, and `content.units`. Add `--pretty` for indented output and `--verbose` for reader and extraction provenance.
- **Text (`--format text`)**: prints one coverage line followed by `[unit N ...]` sections.
- **Markdown (`--format markdown`)**: prints clean `bodymatter:v1` Markdown (or marker-free Markdown with `--no-markers`).
- **Saved artifacts (`--output-dir DIR`)**: writes `.md` and v3 `.json` files without overwriting. See [Automation](automation.md#save-artifacts).

File hashing follows the same 128 MiB threshold as `inspect`.
Pass `--full-hash` to stream full SHA-256 above 128 MiB.

## Provenance per unit

Each entry in `content.units` records:

- `unit_index`: one-based source position.
- `unit`: native unit kind (for example `physical_page`, `worksheet`, `slide`, `table`, `spine_item`, `page_break`, or `logical_line`).
- `source_part`: internal part or page identifier, when present.
- `label`: printed PDF page label from `/PageLabels`, when present.
- `marker_index`: optional one-based source ordinal for DOCX `page-break` and DOCX or ODT `table` markers, omitted when absent.
- `text` or `rows`: extracted payload.
- `provenance`: reader, stage, fact source, and `origin` (`native`, `ocr`, `mixed`, or `derived_preview`).
- `completeness`: `exact` or `partial`.

Dataset previews keep `derived_preview` origin and `partial` completeness.
They provide bounded evidence, never full exports.

## Example

```sh
glassbox extract /path/to/document.pdf --pages 2-4 --format text
```

For a twelve-page PDF, the first line reports coverage:

```text
Selection: unit=PhysicalPage; available=12 (exact); returned=[2,3,4]; omitted=Selection; completeness=Partial
```

Selected unit text follows below the coverage line.

## Where to go next

- [CLI reference and parsing](cli.md): complete flag tables and migrations.
- [Metadata inspection](inspect.md): schema v3 inspection without content.
- [Formats and limits](formats.md): native units and caps per format.
- [Reports](reports.md): full v3 extraction JSON schema and Rust API.

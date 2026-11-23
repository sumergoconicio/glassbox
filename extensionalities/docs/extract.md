---
title: "Content extraction"
type: doc
content-type: Runbook
parent: docs/README.md
---

# Content extraction

`glassbox extract` returns document content as native units. A native unit is a page, slide, block, line, record, or row that the format itself defines. Each unit carries its source index and provenance. The report states exactly what was returned and what was left out.

## Select units with --pages

By default, extract requests every native unit. `--pages` selects a subset.

| Command | Selection |
| --- | --- |
| `glassbox extract FILE` | All units |
| `glassbox extract FILE --pages` | First five units |
| `glassbox extract FILE --pages 12` | First twelve units |
| `glassbox extract FILE --pages 3-7,10` | Units 3 to 7 and unit 10 |

Selector rules:

- Indices are one-based and must ascend within a range.
- Repeated and overlapping ranges merge into one.
- A bare count past the total returns every available unit. The operation stays `complete` and exits 0.
- A range whose every index is past the total fails. The report records the reason and exits 1.
- Malformed, zero, descending, or overflowing values exit 64 before any file opens.

`--max-pages N` is deprecated. It maps to a first-N selection and prints a warning. Combining it with `--pages` exits 64.

## Coverage is disclosed, never guessed

Selection coverage and operation status stay separate:

- A requested subset that is fully emitted completes. The exit code is 0.
- The report still discloses unrequested source units through `selection.omitted: "selection"`.
- Lost content from caps, parser failures, or OCR failures makes the report `partial`. The exit code becomes 2.
- `selection.returned_indices` lists the exact units in source order. `selection.available` states the total with its completeness.
- A unit cap can withhold selected units while the available count stays exact. `selection.omitted` then reports `safety_limit` for a read-window cut or `partial_failure` for withheld units, and the report turns partial.

## Safety caps

Extraction requests the whole document, and existing caps stay in force:

- Characters: 67,108,864 by default. `--max-chars` sets this budget. Fixed reader byte caps still apply.
- Tokens: none by default. `--max-tokens` caps whitespace-delimited words. This is not a model-specific token count.
- Dataset previews: five units by default. `--max-units` changes the count.
- PDF: 4096 pages, 64 MiB of decoded text, and a 128 MiB OCR raster budget.
- Container readers: 256 MiB physical input and 16 MiB per read range.
- Plain text: a 16 MiB read window and 65,536 emitted selected lines. The available count stays exact for the window; withheld units are disclosed as truncated coverage.
- DOCX and PPTX: 65,536 characters per unit. A clipped unit keeps its text, gains a `[unit text truncated]` marker, and its completeness turns partial.

See [formats and limits](formats.md) for per-format caps. Exceeding a cap discloses the loss through partial status, a diagnostic, a truncation marker, or a qualified count; it is never a silent truncation.

## OCR

Extraction runs local OCR where a page needs it. OCR is enabled by default. Pass `--no-ocr` to disable it.
The default OCR language is English (`eng`). Pass `--ocr-language LANG` to choose another language.
The flag accepts natural spellings such as `en` and reports the canonical spelling in OCR provenance.
OCR failure never loses native text. The report keeps the native text and records the failure. See [OCR and troubleshooting](ocr-and-troubleshooting.md).

## Output forms

JSON is the default. It contains metadata, content units, provenance, and selection coverage. `--format text` prints a coverage line and then each unit. `--format markdown` prints the same coverage plus escaped unit headings, text, and bounded rows. Use `--output-dir` to save Markdown and JSON artifacts; see [automation](automation.md).

## Provenance per unit

Every content unit records:

- `unit_index`: one-based source position.
- `unit`: the native unit kind, such as `physical_page` or `logical_line`.
- `source_part`: the internal part or page it came from, when one applies.
- `text` or `rows`: the extracted payload.
- `provenance`: reader, stage, fact source, and origin. Origin is `native`, `ocr`, `mixed`, or `derived_preview`.
- `completeness`: `exact` or `partial` for that unit.

Dataset previews keep `derived_preview` origin and `partial` completeness. They are evidence, not full exports.

## Example

```sh
glassbox extract /path/to/document.pdf --pages 2-4 --format text
```

For a twelve-page PDF, the first line states the coverage:

```text
Selection: unit=PhysicalPage; available=12 (exact); returned=[2,3,4]; omitted=Selection; completeness=Partial
```

Then each selected unit follows with its native text.

## Where to go next

- [CLI reference and parsing](cli.md): the complete flag table and migrations.
- [Metadata inspection](inspect.md): identity and structure without content.
- [Formats and limits](formats.md): native units and caps per format.
- [Reports](reports.md): the full v2 extraction schema.

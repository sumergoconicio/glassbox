---
title: "Formats and limits"
type: doc
content-type: Guide
parent: docs/README.md
---

# Formats and limits

Glassbox identifies files from a bounded byte prefix and their extension. Identification never implies extraction support. This page lists what each format family returns and where the fixed boundaries are.

## Support matrix

| Input | Inspection | Extraction | Native unit | Boundaries |
| --- | --- | --- | --- | --- |
| PDF | Pages, security, per-page signals, object counts | Yes | Physical page | 4096 pages; 64 MiB text; 128 MiB OCR raster budget |
| Raster images | Image count, header validation, decoded pixel dimensions | Yes | One image unit | 16384 x 16384 decode bound; 128 MiB decode allocation |
| Plain text, Markdown, source code | Exact line count | Yes | Logical line | UTF-8 input only; 16 MiB read window; 65536 emitted units |
| DOCX | Package parts, properties, counts | Yes | Top-level paragraph or table, as one block | 4096 blocks; blocks are never split; 65536 characters per unit |
| PPTX | Slides, properties, counts | Yes | Slide | Slides follow relationship order; 65536 characters per slide |
| XLSX, XLSM | Sheets, columns, headers | Yes | Worksheet | 262144 cells; 64 MiB cell values; 4096 sheets |
| ODT | Properties, counts | Yes | Top-level paragraph or table | Same atomic block rule as DOCX |
| ODP | Slides, properties | Yes | Slide | Document order |
| ODS | Sheets, columns, headers | Yes | Worksheet | Spreadsheet caps above |
| EPUB | Spine, manifest, OPF properties | Yes | Spine item | 4096 manifest items and spine entries; XHTML parts only |
| MOBI, AZW, AZW3, PRC | Headers, EXTH properties | Yes | Decoded text record | Uncompressed or PalmDOC records only; 64 KiB per record; 32 MiB expanded |
| CSV, TSV | Headers, bounded row profile | Preview only | Dataset row preview | 1 MiB reader cap |
| JSON | Key names, scalar counts | Preview only | Dataset preview | 1 MiB schema-path budget; cap trip keeps the observed prefix with partial status |
| XML | Element paths and counts | Preview only | Dataset preview | 16 MiB input; 200000 elements; depth 128; 1 MiB schema-path budget |
| ZIP, CBZ | Entry names and counts; CBZ page count from image names | No content units | Archive entry | 4096 entries; 16 MiB per entry; 256 MiB total |
| RAR, CBR | Header listing only | Not supported | None | No decompression; 16384 headers |
| Apple iWork (Pages, Numbers, Key) | Embedded previews | Preview only | Derived preview | No complete native decoder |
| DjVu | Identification | Not supported | None | No text, image, or OCR extraction; no page count |
| Parquet | Footer identification | Not supported | None | Extraction is deferred |
| Other formats | Identification | Not supported | None | Report states `unsupported` |

Plain text units follow exact newline rules from the source. Lines split at each `\n`; a `\r` directly before it is dropped. A trailing newline does not create an extra empty unit. A final line without a newline still counts as one unit. Empty input has zero lines. A leading byte-order mark is ignored.

Text reads are bounded by a 16 MiB window. A file past the window is cut to whole lines inside the window; the cut line is never emitted as a whole unit, and the available count is disclosed as partial instead of a fabricated complete count. A cut through a multi-byte character backs off to the last complete character. Every line in the window is counted, so availability stays exact for the window. One extraction emits at most 65,536 selected units; a denser source withholds the remaining selected units, and the report discloses the truncation instead of shrinking the count. Explicit ranges that fit inside the cap still complete.

## Dataset outputs stay previews

CSV, TSV, JSON, XML, spreadsheets, and iWork previews are bounded. They prove structure and provide samples. No flag turns them into a full export. Extraction reports mark preview units as `derived_preview` with `partial` completeness. See [content extraction](extract.md).

## Schema-path budgets

JSON and XML inspection publishes schema paths, never values. Both run under a 1 MiB cumulative path budget. When the budget stops the stream, the observed path prefix is published, the count is qualified as partial, and a `metadata_cap_exceeded` warning diagnostic marks the operation partial. XML element counts stay exact; JSON scalar counts become a lower bound at the cap.

## Identification evidence

`format.basis` records how a file was identified. `bytes` means the byte signature decided. `extension` means the signature was ambiguous and the extension chose. `format.disagreement` is true when the extension and the byte signature disagree. A renamed file keeps its true detected family, and the report says so.

## Encryption

PDF security state is recorded, including handler revision and permission bits. Accessible encrypted PDFs open with PDFium and report `encrypted_accessible`. Encrypted MOBI text is unavailable; metadata is retained. Password-protected or DRM'd parts produce diagnostics and partial or failed states. Nothing is bypassed.

## Unsupported and unknown inputs

An unknown format still inspects: you get identity, digest, and format detection. Its report marks `extraction_support: unsupported`, and inspection exits 2. A file whose bytes cannot be read reports `failed` with exit code 1. Identification-only formats (generic ZIP, CBZ, DjVu, Parquet) complete with exit code 0 because identification is their supported operation.

## Safety cap inventory

All caps are fixed in the readers. No flag raises them:

- PDF: 4096 pages, 64 MiB decoded text, 128 MiB OCR staging.
- Container source reader: 256 MiB physical input, 16 MiB per range, 64 MiB cumulative.
- Archive: 4096 entries, 16 MiB per entry, 256 MiB total.
- Spreadsheets: 1048576 rows, 262144 cells, 64 MiB values, 4096 sheets.
- Images: 16384 x 16384 pixels, 128 MiB allocation.
- Plain text: 16 MiB read window, 65536 emitted selected units.
- DOCX and PPTX: 65536 characters per unit; Word tables nest at most 16 levels inside one unit.
- JSON and XML schema paths: 1 MiB cumulative retained bytes.

Cap exhaustion always produces a disclosure: a diagnostic, a truncation marker, or a qualified partial count. It never produces a silent cut.

## Where to go next

- [CLI reference and parsing](cli.md) for the command surface.
- [Metadata inspection](inspect.md) and [content extraction](extract.md) for the two operations.
- [OCR and troubleshooting](ocr-and-troubleshooting.md) for OCR behavior per format.

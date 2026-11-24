---
title: "Formats and limits"
type: doc
content-type: Guide
parent: docs/README.md
---

# Formats and limits

Glassbox identifies files from a 64 KiB byte prefix and their filename suffix.
Format identification never implies extraction support.
This page lists the 23 canonical format types, per-format behavior, and fixed safety caps.

## Canonical `format.type` families

Schema v3 classifies every file into one of 23 lowercase `format.type` identifiers.
Conclusive byte detection overrides the filename suffix.
When byte detection is inconclusive, the longest known suffix decides (`tar.gz`, `tar.bz2`, `tar.xz`, and `tar.zst` match before single suffixes).
Unknown or missing suffixes map to `other`.

- `pdf` (`pdf`)
- `office_docs` (`doc`, `docx`, `odt`, `rtf`, `pages`)
- `presentations` (`ppt`, `pptx`, `odp`, `key`)
- `spreadsheets` (`xls`, `xlsx`, `xlsm`, `ods`, `numbers`)
- `text` (`txt`, `md`, `rst`, `tex`)
- `tabular_data` (`csv`, `tsv`, `parquet`, `arrow`)
- `structured_data` (`json`, `jsonl`, `xml`, `yaml`, `yml`, `toml`)
- `images` (`jpg`, `jpeg`, `png`, `gif`, `webp`, `heic`, `avif`, `tif`, `tiff`, `bmp`)
- `vector_graphics` (`svg`, `eps`)
- `ebooks` (`epub`, `mobi`, `azw`, `azw3`, `prc`, `djvu`, `djv`)
- `comics` (`cbz`, `cbr`)
- `audio` (`mp3`, `m4a`, `aac`, `flac`, `wav`, `ogg`, `opus`, `aiff`)
- `video` (`mp4`, `mkv`, `mov`, `avi`, `webm`, `m4v`, `mpeg`)
- `archives` (`tar.gz`, `tar.bz2`, `tar.xz`, `tar.zst`, `zip`, `rar`, `7z`, `tar`, `gz`, `bz2`, `xz`, `zst`)
- `source_code` (`py`, `go`, `js`, `ts`, `jsx`, `tsx`, `java`, `c`, `cpp`, `rs`, `sh`, `sql`)
- `web` (`html`, `htm`, `css`, `mhtml`)
- `databases` (`sqlite`, `sqlite3`, `db`, `mdb`, `accdb`)
- `personal_info` (`eml`, `msg`, `mbox`, `ics`, `vcf`)
- `subtitles` (`srt`, `vtt`, `ass`)
- `fonts` (`ttf`, `otf`, `woff`, `woff2`)
- `3d_cad` (`stl`, `obj`, `gltf`, `glb`, `step`, `stp`, `dwg`, `dxf`)
- `applications` (`exe`, `msi`, `apk`, `deb`, `rpm`, `appimage`)
- `other` (unmapped or missing suffix)

`format.extension` keeps the literal normalized lowercase suffix without a leading dot, and is omitted when absent.
`format.extension_mismatch: true` appears only when conclusive byte detection disagrees with a known suffix family.
Valid aliases within the same family (such as `.jpg`/`.jpeg` or `.azw3`/MOBI) do not trigger a mismatch.

## Support matrix

| Input | Inspection | Extraction | Native unit | Boundaries |
| --- | --- | --- | --- | --- |
| PDF | Pages, geometry, security, text-layer and OCR readiness | Yes | Physical page (`<page>` with `/PageLabels`) | 4096 pages; 64 MiB text; 128 MiB OCR raster budget |
| Raster images | Image count, header check, pixel dimensions | Yes | One image unit | 16384 x 16384 decode bound; 128 MiB decode allocation |
| Plain text, Markdown, source code | Exact line count | Yes | Logical line | UTF-8 only; 16 MiB read window; 65536 emitted units |
| DOCX | Package parts, properties, counts | Yes | Top-level paragraph, table (`<table>`), or explicit `<w:br w:type="page"/>` (`<page-break>`) | 4096 blocks; 65536 chars/unit; ignores `<w:lastRenderedPageBreak/>` |
| PPTX | Slides, properties, counts | Yes | Slide (`<slide>`) | Relationship order; 65536 characters per slide |
| XLSX, XLSM | Sheets, columns, headers | Yes | Worksheet (`<sheet name="...">` + GFM table) | 262144 cells; 64 MiB cell values; 4096 sheets |
| ODT | Properties, counts | Yes | Top-level paragraph or table (`<table>`) | Same atomic block rules as DOCX; no page-break inference |
| ODP | Slides, properties | Yes | Slide (`<slide>`) | Document order |
| ODS | Sheets, columns, headers | Yes | Worksheet (`<sheet name="...">` + GFM table) | Same spreadsheet caps as XLSX |
| EPUB | Spine, manifest, OPF properties | Yes | Spine item (`<chapter href="...">`) | 4096 manifest and spine entries; XHTML parts only |
| MOBI, AZW, AZW3, PRC | Headers, EXTH properties | Yes | Decoded text record | Uncompressed or PalmDOC only; 64 KiB/record; 32 MiB expanded |
| CSV, TSV | Headers, bounded row profile | Preview only | Dataset row (`<sheet>` + GFM table) | 1 MiB reader cap |
| JSON | Key names, scalar counts | Preview only | Dataset preview | 1 MiB schema-path budget; partial status on cap hit |
| XML | Element paths and counts | Preview only | Dataset preview | 16 MiB input; 200000 elements; depth 128; 1 MiB path budget |
| ZIP, CBZ | Entry names and counts; CBZ image count | Identification only | Archive entry | 4096 entries; 16 MiB per entry; 256 MiB total |
| RAR, CBR | Header listing only | Not supported | None | No decompression; 16384 headers |
| Apple iWork (Pages, Numbers, Key) | Embedded previews | Preview only | Derived preview | No complete native decoder |
| DjVu (`djvu`, `djv`) | Identification | Identification only | None | No text, image, OCR, or page-count extraction |
| Parquet | Footer identification | Identification only | None | Extraction is deferred |
| Other formats | Identification | Not supported | None | Report marks `status: "unsupported"` |

Plain text splits lines at `\n` and strips an immediately preceding `\r`.
A trailing newline does not add an empty unit; a final line without `\n` still counts.
Empty input has zero lines, and a leading byte-order mark is ignored.

Plain text reads stop at a 16 MiB window and cut cleanly on whole UTF-8 lines.
A file past 16 MiB reports partial availability rather than a fabricated total.
One extraction emits at most 65,536 selected units; withheld units are disclosed as truncated coverage.

## Dataset outputs stay previews

CSV, TSV, JSON, XML, spreadsheets, and iWork outputs are bounded previews.
No flag converts them into full exports.
Extraction reports mark preview units with `origin: "derived_preview"` and `completeness: "partial"`.
See [Content extraction](extract.md) for GFM table rendering.

## Schema-path budgets

JSON and XML inspection publishes schema paths in `structure.names`, never values.
Both run under a 1 MiB cumulative path budget.
When the budget stops collection, Glassbox publishes the observed prefix and emits a `metadata_cap_exceeded` warning with `partial` status.
XML element counts stay exact; JSON scalar counts become a lower bound at the cap.

## Encryption

PDF security state appears in `structure.pdf` (`security_handler_revision` and `permissions`) and `readiness.encrypted`.
Accessible encrypted PDFs open with PDFium and report `readiness.encrypted: "accessible"`.
Locked encrypted files report `readiness.encrypted: "locked"`.
Encrypted MOBI text stays unavailable while embedded metadata is retained.
Glassbox never bypasses passwords or DRM.

## Unsupported and identification-only inputs

Unknown formats still report `file`, `format`, and hashing facts with `readiness.extractable: "unsupported"` and exit 2.
Unreadable files report `status: "failed"` and exit 1.
Identification-only formats (generic ZIP, CBZ, DjVu, Parquet) report `readiness.extractable: "identification_only"` and complete inspection with exit 0.

## Safety cap inventory

All reader caps are fixed; no flag raises them:

- PDF: 4096 pages, 64 MiB decoded text, 128 MiB shared OCR staging.
- Container reader: 256 MiB physical input, 16 MiB per range, 64 MiB cumulative.
- Archives: 4096 entries, 16 MiB per entry, 256 MiB total.
- Spreadsheets: 1,048,576 rows, 262,144 cells, 64 MiB values, 4096 sheets.
- Images: 16,384 x 16,384 pixels, 128 MiB allocation.
- Plain text: 16 MiB read window, 65,536 emitted selected units.
- DOCX and PPTX: 65,536 characters per unit; Word tables nest at most 16 levels.
- JSON and XML schema paths: 1 MiB cumulative retained bytes.

Every cap hit discloses the omission through a diagnostic, truncation marker, or partial status.

## Where to go next

- [CLI reference and parsing](cli.md) for all flags and exits.
- [Metadata inspection](inspect.md) for the full 23-format table and v3 readiness mapping.
- [Content extraction](extract.md) for `bodymatter:v1` markers and native selection.
- [OCR and troubleshooting](ocr-and-troubleshooting.md) for OCR behavior per format.

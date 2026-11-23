---
title: "Metadata inspection"
type: doc
content-type: Runbook
parent: docs/README.md
---

# Metadata inspection

`glassbox inspect` reports what a file is. It covers identity, structure, and embedded properties. It never returns body text, row values, or OCR text. Use [content extraction](extract.md) when you need content.

## What a report contains

Every inspect report carries these fact groups:

- **Identity**: source path, byte size, and a full-file SHA-256 digest.
- **Format**: extension, detected MIME type, detected name, and family bucket. `format.basis` states whether identification came from bytes or the extension alone. `format.disagreement` flags a mismatch between them.
- **Qualified counts**: pages, slides, blocks, entries, columns, or lines. Every count names its unit, source, and completeness.
- **Schema identifiers**: column names, worksheet names, archive entry names, JSON pointers, and XML paths. These are names, never values.
- **Embedded metadata**: an allowlist of properties. It covers title, subject, author, keywords, creator, producer, language, dates, and application.
- **Native page signals**: for PDFs, per-page text layer, OCR advice, object counts, page size, and rotation.
- **Encryption and support**: encryption state, PDF security revision and permissions, and extraction support.

Every fact names its source, such as native structure, embedded property, container index, or byte scan. Uncertainty codes explain partial facts.

## Digest behavior

Hashing streams the entire file through a 64 KiB buffer. Memory stays small for large files.
If consistency checks detect a change, the report withholds the digest and records `source_changed`.
These checks do not create an immutable snapshot against concurrent writes.

## Status and exit codes

| Status | Meaning | Exit code |
| --- | --- | --- |
| `complete` | The operation finished | 0 |
| `partial` | Some facts were capped or unavailable | 2 |
| `unsupported` | Format identified; no metadata reader applies | 2 |
| `failed` | Input or reader failure | 1 |
| Invalid invocation | Bad flags or arguments | 64 |

Identification-only formats still inspect successfully. Generic ZIP, CBZ, DjVu, and Parquet reports mark `extraction_support` as `identification_only` and exit 0.

## Read a report

```sh
glassbox inspect /path/to/document.pdf --format text
```

The text and Markdown formats print a content-free summary. It lists schema version, source, format, byte size, SHA-256, status, counts, schema names, embedded properties, page signals, raster geometry for image-only documents, diagnostics, and artifacts. JSON is the default format. Add `--pretty` for indented JSON.

## Thumbnails are opt-in

Ordinary inspection neither renders nor decodes images. Pass `--thumbnail PATH` to write one bounded thumbnail. `--thumbnail-max-edge` caps its long edge; the default is 320 pixels. Thumbnail rendering never implies OCR or body extraction.

## Privacy

Inspect never returns document bodies. Note what metadata alone can reveal. Author fields, file paths, and column names can identify people or sensitive systems. Share reports with the same care as the files they describe.

## Content flags are rejected

Inspect rejects `--identify`, `--max-pages`, `--max-tokens`, `--max-chars`, `--probe-ocr`, `--no-ocr`, `--ocr-language`, and `--packet`. Each rejection exits 64 and names `glassbox extract` as the replacement. These flags belong to content work, not identity work.

## Where to go next

- [CLI reference and parsing](cli.md): the complete flag table and command surface.
- [Formats and limits](formats.md): the support matrix and safety caps.
- [Automation and directory inventories](automation.md): batch mode and directory scans.
- [Reports](reports.md): the v2 JSON schema and the Rust API.
- [OCR and troubleshooting](ocr-and-troubleshooting.md): OCR behavior and common failures.

---
title: "Reports and programmatic use"
type: "documentation"
content_type: "API Reference"
schema: "chai.metadata/v2"
id: "01M8A8KCH9J3719VX2989Q1VKD"
tags: ["reports", "programmatic", "separates", "downstream", "liteparse", "upstream", "examples", "placeholders", "row", "values"]
container_id: "01M8A8KCH8CGBADTAZR9N6G7K9"
---

# Reports and programmatic use

Downstream inspection and extraction emit schema version 3 JSON reports (`schema_version: 3`).
The `kind` field separates `"inspect"` from `"extract"`.
Inspect, extract JSON (`metadata`), `--batch` JSONL (`report`), and directory inventory JSONL (`report`) all emit schema v3.
There is no `--schema 2` flag.
`glassbox parse` keeps upstream LiteParse output formats unchanged.

Strings containing `...` in the examples below are placeholders, not literal CLI output.

## Inspect report (schema v3)

```json
{
  "schema_version": 3,
  "kind": "inspect",
  "status": "complete",
  "file": {
    "path": "/in/report.pdf",
    "name": "report.pdf",
    "bytes": 482113,
    "sha256": "9f2c...",
    "file_id": "8:1"
  },
  "format": {
    "type": "pdf",
    "mime": "application/pdf",
    "extension": "pdf"
  },
  "description": {
    "title": "Annual Report",
    "authors": ["J. Doe"],
    "created_at": "2026-01-15T09:30:00Z",
    "software": {
      "producer": "PDF Producer"
    }
  },
  "structure": {
    "counts": {
      "pages": 12
    },
    "pdf": {
      "pages": 12,
      "width_points": 612.0,
      "height_points": 792.0,
      "uniform_page_size": true,
      "security_handler_revision": -1,
      "permissions": 4294967295
    }
  },
  "readiness": {
    "extractable": "supported",
    "encrypted": "none",
    "text_layer": "mixed",
    "ocr": "recommended"
  }
}
```

Key v3 inspect rules:

- **Content-free**: an inspect report never contains body text, row values, or OCR output.
- **Digest versus fingerprint**: files up to 128 MiB (or with `--full-hash`) emit `file.sha256`. Larger files omit `file.sha256`, emit `file.fingerprint` (`{"scheme": "glassbox-sampled-sha256/v1", "value": "..."}`), and add `fingerprint_sampled`. See [Metadata inspection](inspect.md#digest-and-sampled-fingerprint) for the byte framing.
- **Format fields**: `format.type` uses one of the 23 lowercase identifiers in [Metadata inspection](inspect.md#format-classification). `format.extension` is omitted when a filename has no suffix. `format.extension_mismatch: true` appears only on family disagreement.
- **Conditional top-level keys**: `diagnostics` and `artifacts` are omitted when empty. `provenance` is omitted unless you pass `--verbose`. No `next_step` or `advice` block exists.
- **Geometry**: PDFs add optional `structure.pdf`; raster images add optional `structure.image` (`{"width_pixels": 640, "height_pixels": 400}`) with no inferred DPI.

See [Metadata inspection](inspect.md) for every field rule and the 23-format table.

## Extract report (schema v3)

An extract report embeds the v3 inspect report under `metadata` and adds `selection` and `content`:

```json
{
  "schema_version": 3,
  "kind": "extract",
  "status": "complete",
  "completeness": "partial",
  "metadata": {
    "schema_version": 3,
    "kind": "inspect",
    "status": "complete",
    "file": {"path": "/in/report.pdf", "name": "report.pdf", "bytes": 482113, "sha256": "9f2c..."},
    "format": {"type": "pdf", "mime": "application/pdf", "extension": "pdf"},
    "description": {},
    "structure": {"counts": {"pages": 12}},
    "readiness": {"extractable": "supported", "encrypted": "none", "text_layer": "present", "ocr": "not_needed"}
  },
  "selection": {
    "requested": {"mode": "first_n", "count": 2},
    "unit": "physical_page",
    "available": {"unit": "physical_page", "value": 12, "source": "native_structure", "completeness": "exact"},
    "returned_indices": [1, 2],
    "omitted": "selection",
    "completeness": "partial"
  },
  "content": {
    "units": [
      {
        "unit_index": 1,
        "unit": "physical_page",
        "source_part": "page 1",
        "label": "iv",
        "text": "...",
        "rows": [],
        "provenance": [],
        "completeness": "exact"
      }
    ]
  }
}
```

Key extract fields:

- `selection.omitted`: `none`, `selection`, `safety_limit`, `partial_failure`, or `unsupported`.
- `selection.returned_indices`: exact one-based source positions of returned units.
- `completeness`: source coverage of the report (`partial` for a subset even when `status` is `complete`).
- `content.units[].label`: optional printed PDF page label from `/PageLabels`, omitted when absent.
- `content.units[].start_ms`, `content.units[].end_ms`: optional inclusive start and exclusive end milliseconds for `time_interval` (`NativeUnit::TimeInterval`) ASR units, omitted for non-media units. `content.units` holds stitched display text paired with each window's raw `start_ms` and `end_ms` timestamps.
- Top-level `diagnostics` and `artifacts` are omitted when empty; top-level `provenance` is omitted unless `--verbose` is set.
- `artifacts[].kind`: `thumbnail`, `metadata_json`, `metadata_summary`, `extraction_json`, `markdown_content`, `contact_sheet`, or `audio_extract`.

Every diagnostic record contains `stage`, `severity`, `code`, `message`, and optional `unit_index`.
Media-pipeline failures and warnings use `stage: "media"` (`DiagnosticStage::Media`).

### Media extraction receipts (`media`)

`ExtractReportV3` adds an optional top-level `media` object only when at least one media stage produces a receipt.
When empty or absent, `media` is omitted so non-media v3 JSON reports stay byte-identical.

- `media.contact_sheet`: `output`, `duration_seconds`, `columns`, `rows`, `tiles`, `mode`, `preset`, `format`, `width_px`, `height_px`, `cell_width_px`, `cell_height_px`, `seek_seconds`, `reused_tiles`, `linear_seek_tiles`, and optional `every_seconds`.
- `media.asr`: `mode`, `backend`, `model`, `provider`, optional `protocol`, optional `language`, `duration_ms`, `sampled_ranges_ms`, `windows_attempted`, and `windows_succeeded`.
- `media.audio_extract`: `path`, `format`, and `duration_ms`.

## Migration from v2 and legacy shapes

Schema v3 replaces v2 across all inspect, extract, batch, and directory inventory JSON outputs:

- **Flat keyed inspect envelope**: v2's nested `metadata.*` wrapper is replaced by top-level `status`, `file`, `format`, `description`, `structure`, and `readiness`.
- **Canonical `format.type`**: display strings (`"Office Docs"`, `"PDF"`) become 23 lowercase identifiers (`"office_docs"`, `"pdf"`). `format.extension` now preserves the literal suffix, and `format.extension_mismatch: true` replaces `format.disagreement`.
- **Separated digest fields**: full digests stay in `file.sha256`. Above 128 MiB, sampled hashes move to `file.fingerprint` unless `--full-hash` is set. `file.consistent` is removed.
- **Cleaner defaults**: empty `diagnostics` and `artifacts` are omitted, `advice` is removed, and `provenance` requires `--verbose`.
- **Clean Markdown**: `glassbox extract --format markdown` replaces synthetic headings and preamble with `bodymatter:v1` comments (or `--no-markers`).
- **Legacy content flags**: `inspect` rejects extraction flags with exit 64. See the rejected list in [Metadata inspection](inspect.md#content-flags-are-rejected). Use `extract`.

## Rust API

The `glassbox-inspect` crate exposes inspection, extraction, v3 projections, hashing policies, and `bodymatter:v1` formatting directly.

`MetadataReport` and `ExtractReport` use the legacy wrapper layout.
Serializing either struct directly emits `LEGACY_REPORT_SCHEMA_VERSION` (`schema_version: 2`).
Project them with `.to_v3(verbose)`, `inspect_to_v3`, or `extract_to_v3` to obtain `InspectReportV3` or `ExtractReportV3`.
Those v3 types and all CLI commands emit `REPORT_SCHEMA_VERSION` (`schema_version: 3`), the canonical wire layout.

This example expects `document.txt` to contain at least ten UTF-8 lines:

```rust
use glassbox_inspect::{extract, inspect, reports};
use std::path::Path;

fn main() {
    let path = Path::new("document.txt");
    let raw = inspect(path, &reports::InspectOptions::default());
    let v3 = raw.to_v3(false);
    assert_eq!(v3.schema_version, 3);

    let options = reports::ExtractOptions {
        selection: glassbox_inspect::selection::parse_pages("3-7,10").unwrap(),
        ..reports::ExtractOptions::default()
    };
    let extracted = extract(path, &options);
    let extracted_v3 = extracted.to_v3(false);
    assert_eq!(extracted_v3.selection.returned_indices, vec![3, 4, 5, 6, 7, 10]);
}
```

Main public modules and symbols:

- `inspect(path, options) -> MetadataReport` and `inspect_with_policy(path, options, HashPolicy) -> MetadataReport`.
- `extract(path, options) -> ExtractReport` and `extract_with_policy(path, options, HashPolicy) -> ExtractReport`.
- `Inspector::with_budget(&SessionBudget)`, `Inspector::with_shared_budget(SharedWorkBudget)`, `Inspector::with_full_hash(bool)`, and `Inspector::with_hash_policy(HashPolicy)`.
- `reports` module: `REPORT_SCHEMA_VERSION` (`3`), `LEGACY_REPORT_SCHEMA_VERSION` (`2`), `InspectOptions`, `ExtractOptions`, `MediaReceipt`, `ContactSheetReceipt`, `AsrReceipt`, and `AudioExtractReceipt`.
- `reports_v3` module: `InspectReportV3`, `ExtractReportV3`, `REPORT_SCHEMA_VERSION_V3` (`3`), `inspect_to_v3`, `extract_to_v3`, `normalize_metadata_date`, and `ALL_FORMAT_TYPES`.
- `hashing` module: `HashPolicy`, `DEFAULT_FULL_HASH_MAX_BYTES`, `SAMPLED_FINGERPRINT_SCHEME`, and `hash_file_with_policy`.
- `bodymatter` module: `BodymatterMarker` (including `Time`), `format_marker`, `escape_attr`, `sanitize_body`, `escape_table_cell`, and `render_gfm_table`.
- `transcript_markdown` module: `GLASSBOX_TRANSCRIPT_SCHEMA` (`"glassbox:v1"`), `TranscriptRenderOptions`, `render_transcript_markdown`, and `generate_ulid`.
- `selection` module: `parse_pages`, `render_text`, `render_markdown`, `render_markdown_with_options(report, include_markers)`, and `render_markdown_full(report, include_markers, include_frontmatter)`.
- `metadata` module: `summary`, `summary_markdown(report, verbose)`, and `summary_text(report, verbose)`.

## Build from source

Glassbox is not published to a package registry.
Depend on it from a local checkout:

```toml
[dependencies]
glassbox-inspect = { path = "extensionalities/crates/litx-inspect" }
```

Adjust the relative path to match your consumer's `Cargo.toml`.
Follow [Installation](installation.md) to build the `glassbox` executable.

## Where to go next

- [CLI reference and parsing](cli.md) for commands, flags, and exit codes.
- [Metadata inspection](inspect.md) and [Content extraction](extract.md) for field and marker rules.
- [Automation and directory inventories](automation.md) for batch and inventory envelopes.

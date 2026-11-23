---
title: "Reports and programmatic use"
type: doc
content-type: API Reference
parent: docs/README.md
---

# Reports and programmatic use

Downstream inspection and extraction return v2 reports. `schema_version` is `2`. The `kind` field separates `inspect` from `extract`.
`parse` keeps upstream output formats. The excerpts below omit fields; use CLI output for complete reports.
Strings containing `...` are placeholders, not literal CLI output.

## Inspect report

```json
{
  "schema_version": 2,
  "kind": "inspect",
  "metadata": {
    "identity": {"path": "...", "byte_size": 1204, "opaque_file_id": "8:1", "consistency": "consistent"},
    "format": {"extension": "pdf", "detected_mime": "application/pdf", "detected_name": "...", "bucket": "PDF", "basis": "bytes", "disagreement": false, "detector": "file-format"},
    "digest": {"algorithm": "sha256", "digest": "...", "bytes_hashed": {"unit": "byte", "value": 1204, "source": "byte_scan", "completeness": "exact"}, "consistency": "consistent", "source_identity": "consistent", "snapshot_guarantee": "not_immutable"},
    "counts": [{"unit": "physical_page", "value": 12, "source": "native_structure", "completeness": "exact"}],
    "schema_identifiers": [{"kind": "column", "name": "...", "source": "native_structure"}],
    "embedded_metadata": [{"field": "author", "value": "...", "source": "embedded_property", "completeness": "exact"}],
    "text_layer": "mixed",
    "ocr_recommended": "recommended",
    "encryption": "not_encrypted",
    "extraction_support": "supported",
    "native_page_signals": [{"page_index": 0, "text_layer": "present", "ocr_recommended": "not_recommended", "object_counts": [], "coverage": "exact", "uncertainties": []}],
    "pdf": {"security_handler_revision": -1, "permissions": 4294967295},
    "status": "complete",
    "completeness": "exact",
    "diagnostics": [],
    "artifacts": [],
    "advice": [],
    "provenance": [{"reader": "pdf-structure", "stage": "...", "source": "native_structure"}]
  }
}
```

An inspect report has no body, row, or OCR field by design. Image-only documents add one optional `raster` object, such as `{"width_pixels": 640, "height_pixels": 400}`, holding decoded pixel dimensions; the key is absent for every other format, and pixels never enter the PDF-style point fields of `native_page_signals`. No DPI is inferred from raster dimensions. See [metadata inspection](inspect.md) for field meanings.

## Extract report

The extract report extends the metadata block with content and coverage:

```json
{
  "schema_version": 2,
  "kind": "extract",
  "metadata": {},
  "content": {"units": [{"unit_index": 1, "unit": "physical_page", "source_part": "page 1", "text": "...", "rows": [], "provenance": [], "completeness": "exact"}, {"unit_index": 2, "unit": "physical_page", "source_part": "page 2", "text": "...", "rows": [], "provenance": [], "completeness": "exact"}]},
  "provenance": [],
  "completeness": "exact",
  "selection": {"requested": {"mode": "first_n", "count": 2}, "unit": "physical_page", "available": {"unit": "physical_page", "value": 12, "source": "native_structure", "completeness": "exact"}, "returned_indices": [1, 2], "omitted": "selection", "completeness": "partial"}
}
```

Key coverage fields:

- `selection.omitted`: `none`, `selection`, `safety_limit`, `partial_failure`, or `unsupported`.
- `selection.returned_indices`: exact one-based source positions of returned units.
- Top-level `completeness`: how much of the source content this report covers. A fully emitted subset is `partial` source coverage with a `complete` operation.

Every diagnostic is structured: `stage`, `severity`, `code`, `message`, and an optional `unit_index`. Advice is an inert argument list with an `unknown` cost; Glassbox never executes it.

## Migration from the legacy shape

Earlier Glassbox reports mixed a bounded preview into inspection output. The v2 shape changes this:

- Inspection is metadata-only. The old `text`, `claims`, and preview fields are gone.
- Content now lives in `content.units` of an extract report.
- Legacy content flags (`--identify`, `--max-pages`, `--max-tokens`, `--max-chars`, `--probe-ocr`) are rejected on `inspect` with exit 64. Use `extract`.
- Old extract defaults changed: bare extraction now requests the whole document within fixed caps, not a five-page preview. Use `--pages` for a subset.
- `--max-pages` still works on `extract` with a deprecation warning.

## Rust API

The `glassbox-inspect` crate exposes the operations and types directly:

This example expects `document.txt` to contain at least ten UTF-8 lines.

```rust
use glassbox_inspect::{inspect, extract, reports};
use std::path::Path;

fn main() {
    let path = Path::new("document.txt");
    let report = inspect(path, &reports::InspectOptions::default());
    assert_eq!(report.schema_version, 2);
    let options = reports::ExtractOptions {
        selection: glassbox_inspect::selection::parse_pages("3-7,10").unwrap(),
        ..reports::ExtractOptions::default()
    };
    let extracted = extract(path, &options);
    assert_eq!(extracted.selection.returned_indices, vec![3, 4, 5, 6, 7, 10]);
}
```

Main public symbols:

- `inspect(path: &Path, options: &InspectOptions) -> MetadataReport`
- `extract(path: &Path, options: &ExtractOptions) -> ExtractReport`
- `Inspector::with_budget(&SessionBudget)`, `Inspector::with_shared_budget(SharedWorkBudget)`, and the same two operations as methods for worker reuse.
- `selection::parse_pages(&str) -> Result<NativeSelection, String>`, `selection::render_text(&ExtractReport) -> String`, `selection::render_markdown(&ExtractReport) -> String`.
- `metadata::summary(&MetadataReport) -> String`.
- `reports` module: `InspectOptions`, `ExtractOptions`, `NativeSelection`, `NativeRange`, `OcrOptions`, `SessionBudget`, `ThumbnailOptions`, `OperationStatus`, `Completeness`, and the report structs.

## Build from source

Glassbox is not published to a package registry. Build it from a source checkout:

```toml
[dependencies]
glassbox-inspect = { path = "extensionalities/crates/litx-inspect" }
```

The dependency path is relative to your consumer's `Cargo.toml`; adjust it to your checkout.
Follow [installation](installation.md) to build the `glassbox` executable.

## Where to go next

- [CLI reference and parsing](cli.md) for command and exit details.
- [Metadata inspection](inspect.md) and [content extraction](extract.md) for field semantics.
- [Automation and directory inventories](automation.md) for batch envelopes.

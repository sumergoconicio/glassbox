# Study references

Implementation plan references. Except for the optional Magika crate noted
below, these are study-only references: no source code is copied and no
dependency is taken from them.

- Shiva (`TOOLS/shiva`): native document metadata and legacy spreadsheet reference.
- Microsoft MarkItDown (`MEDIA/markitdown`): format coverage reference.
- Magika (`TOOLS/magika`): detection reference; the off-by-default `magika`
  feature now depends on the published Magika Rust crate and dynamic ONNX
  bindings. No source is vendored or copied into this workspace.
- subextractor, VideOCR, Video Subtitle Extractor (VSE), Subtitler,
  iSubRip, quick-subtitles: subtitle workflow references.
- Subloader: candidate ranking reference.
- ffsubsync: subtitle alignment reference.
- Roku BIF specification: future read-only frame indexing reference.

These are plan references, not a claim that all repositories were inspected.
Apple TV scraping and all other commercial-service scraping are excluded.

The spreadsheet prose-density formula and telemetry limits are intentionally
ported from AskClaire `jsoninspect/inspector.go` and `scanner.go`, as authorized
in the implementation plan. This is separate from the study-only projects above.
Actual Rust library dependencies and licenses are recorded in Cargo manifests
and Cargo.lock. Tesseract remains enabled through upstream LiteParse.

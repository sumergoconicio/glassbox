# Phase 4: PDF and raster-image Quick Look

Continues under the approved headless scope. Changes remain inside
`extensionalities/`; upstream Tesseract support stays enabled. No pushes.

## PDF

- Use upstream public stages for native text extraction/projection and metadata.
  Process one page at a time without OCR, conversion or embedded-image decoding.
- Delegate sampled OCR/layout decisions to `LiteParse::is_complex`, preserving
  raw complexity and coverage in `pdf.complexity` and normalized `preflight`.
  A short native-text page can still be `sparse-text`; preserve upstream policy.
- Record full source page counts and point-space page geometry. Preview scalar
  limits do not cap native character counting or document page traversal.
- Render page 1 only when `--thumbnail` is supplied, through upstream screenshot
  stages (which delegate to `render.rs`). Disable form-action rendering.
  Bound render size before allocation and refuse output overwrites.
- Pass verified PDF bytes to upstream APIs, so misleading filename extensions
  cannot trigger conversions. Isolate the async bridge on a scoped thread so
  the synchronous inspection API also works inside an existing Tokio runtime.

Bounds: 16 MiB encoded PDF input, 4096 native pages, 64 preflight pages,
64 MiB cumulative projected text. Preserve total page counts; report partial
character scans through errors and `pdf.text_complete = false`.

## Raster images

- Decode with a 16384-pixel dimension limit and 128 MiB decoder allocation
  budget. Encoded inputs cap at 16 MiB.
- Composite transparency over white for sampling. Never upscale the source.
  Sample at most a 1024-pixel edge. Report a 16-bin luminance histogram,
  white fraction, pixel aspect ratio and decoded dimensions.
- Report physical density from PNG pHYs and JPEG JFIF inches/centimeters.
  Pixel-aspect-only density and unsupported/missing metadata remain unknown.
  EXIF/TIFF density is not implemented.
- Classify as blank/near-blank, document-scan-like, or photo/graphic, with
  `basis = heuristic`. This is not a calibrated classifier or proof of no text.
- Opt-in `--probe-ocr`: native Tesseract, local `eng.traineddata` only, maximum
  2048-pixel probe edge without upscaling. Report whitespace-delimited recognized
  word count and engine mean confidence normalized to 0–1, with `basis = probed`.
  Keep heuristic metrics unchanged. Do not place recognized text in the preview.
  `--max-units 0` suppresses the probe.
- Honor `TESSDATA_PREFIX` when set. Otherwise check existing conventional local
  cache/system directories. Never download or install data. Missing/invalid
  trained data becomes a `probe_ocr` error while native image metrics survive.

These budgets do not sandbox native PDFium or Tesseract execution.
Images represent the first decoded raster, not an animation frame inventory.

## Verification

The user requested at least five PDF pages. Added a six-page self-authored
fixture: sparse native text, image-only white raster, dense native text, blank,
caption with an embedded raster, and landscape native text. Checked five
default preflight entries and all six with a larger explicit limit. Verified
that page 6 text and all page/character counts survive preview truncation.

Independent Poppler `pdfinfo` confirms all six dimensions, including the
landscape page. Rendered and visually checked all six using `pdftoppm`.
These are synthetic regression fixtures, not a real-world conformance corpus.

Checks passed:

- Formatting and strict workspace Clippy (`--all-targets --no-deps`).
- Full locked workspace tests with `--include-ignored`: 44 passed, none failed.
  This explicitly includes both environment-dependent real local OCR tests:
  blank image and native PDF-rendered words.
- PDF first-page thumbnail dimensions, no-overwrite, byte-verified renamed PDF,
  existing-runtime invocation and malformed PDF handling.
- Image transparency, PNG/JPEG density units, no sampling upscaling, encoded-byte
  and dimension caps, and missing-model behavior without downloads.
- TDD regressions observed before implementation, including preventing false
  truncation errors when a preflight limit exceeds a small document's size.
- Protected upstream paths remain unchanged against baseline
  `14c9eb64b8c843216c114eed8bfcb0aa404bb622`. AskClaire-MONO remains unchanged.
  Two existing upstream dead-code warnings remain outside this workspace.

Poppler is a development cross-check only, not a runtime dependency.
Generated reports, renders and test outputs stay under ignored `target/`.

## Remaining roadmap

Phase 5: ffprobe media facts, speech/VAD excerpts and opt-in transcription,
contact sheets/BIF slices and NFO sidecars. Phase 6: subtitle parsing, the
evidence ladder, timing diagnostics, optional fetching and synchronized output.

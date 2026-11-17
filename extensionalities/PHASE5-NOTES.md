# Phase 5: document extraction, Markdown, and persistent inspection

## Command contract

- `litx inspect PATH`: bounded Quick Look, five PDF pages and 500
  whitespace-delimited word tokens by default.
- `litx inspect PATH --identify`: also enable local image OCR and per-page PDF
  digital/scanned signals. The native library does not download OCR models.
- `litx extract PATH`: full-document extraction with local OCR. It has no
  default five-page or 500-word limit. Explicit options and safety caps still apply.
- `--no-ocr` disables OCR. `--max-pages`, `--max-tokens`, and `--max-chars`
  provide explicit limits. PDF OCR currently uses local English Tesseract.
- `--format markdown` writes Markdown to stdout. JSON reports always contain
  `markdown.content`, `markdown.basis`, and `markdown.truncated`.

`max_units` bounds preflight work, not full document structure. Non-PDF native
documents retain their existing structural scan limits; five PDF pages do not
mean five DOCX paragraphs or five EPUB spine items.

## Persistent mixed-format worker

Both commands accept `--batch`. Send one JSON object per stdin line, such as
`{"id":"example","path":"/absolute/path/document.pdf"}`. A successful request
produces one `{"id":...,"path":...,"report":...}` response line. Malformed
requests receive an `error` object instead of `report`.

Each response is flushed before reading the next request. Stdin remains open
until the caller closes it. Malformed or oversized requests do not discard the
next line. The limit is 65,536 bytes per request line, including its newline.
An oversized request cannot reliably retain its `id`; its error uses `id:null`.

One `Inspector` holds a lazily initialized Tesseract session across files.
The worker runs files sequentially, rather than loading a batch into memory.
Responses remain bounded reports, not a promise of constant process memory.

## Markdown and provenance

Native Office and ebook structure supplies headings, lists, tables, slide and
sheet boundaries, and available section breaks. Native Markdown inputs retain
their structure. UTF-8 plain text uses escaped text fallback.

PDF Markdown uses upstream layout inference and explicit page boundaries.
OCR text does not imply recovered headings or tables. `basis` distinguishes
`native_structure`, `inferred_layout`, `ocr_text_fallback`, and `text_fallback`.
Outputs combining sources may use `mixed`. Partial results carry a notice and
remain inspectable through JSON errors and truncation fields.

This is not a full-fidelity document converter. Not all formats preserve
numbering, hyperlinks, nested tables, or visual reading order.

## Datasets are previews, not exports

- XLSX/XLSM/ODS retain sheet names and at most five header names in
  `headers_preview`, plus at most five data rows per sheet in `preview_rows`.
  Existing column profiles and structural counts remain available.
- CSV/TSV use the first record as headers and return five data rows in
  `dataset.rows`. Quoted delimiters and newlines are supported. Values remain
  strings, so identifiers such as `001` are not coerced to numbers.
- CSV/TSV look ahead one record to set `has_more`; they do not scan the file for
  a total row count. Preview parsing has a 1 MiB input budget and 16,384-column
  header cap. Individual preview strings are limited to 512 characters.
- Parquet reports footer row counts, row groups, schema paths, and types.
  The default native build also decodes at most five typed rows across row
  groups. Strings are capped at 512 characters and nested array samples at
  five elements; `values_truncated` records clipping.
- Parquet row previews currently support UNCOMPRESSED, SNAPPY, and LZ4_RAW.
  Other codecs retain schema output and add a `parquet_preview` error.
  External column paths are never opened. Encrypted files are unsupported.
- Parquet limits: 1 MiB footer, schema depth 64, 16,384 schema nodes/row groups,
  16 MiB per selected column chunk, 64 MiB selected chunk bytes, 8 MiB per page,
  and 128 MiB declared expanded page bytes. Page headers are checked before
  record decoding. Selected chunks are snapshotted to prevent later file
  changes from bypassing those checks.

These are conservative supported subsets. Large row groups can exceed preview
limits even when the requested output contains only five rows.

## Archives and iWork

Ordinary ZIP reports include entry counts and filenames in JSON and Markdown.
Listing does not read file contents or extract paths onto disk. Existing ZIP
directory, entry-count, expanded-byte, and path safeguards still apply.

For `.pages`, `.numbers`, and `.key` ZIP wrappers, inspect only allowlisted
embedded PDF, image, or UTF-8 text previews. Prefer `QuickLook/Preview.pdf`;
otherwise consider `QuickLook/Preview.txt`, preview PNG/JPEG entries, and a
QuickLook thumbnail.

Embedded previews are capped at one PDF page or one image and 4,000 characters,
including during `extract`. Local image/PDF OCR follows the requested OCR
settings. This also applies to Numbers: a bounded page/image excerpt is allowed,
but it is not a native five-cell-row spreadsheet extraction.

`derived_preview` records the source part, content type, and `partial:true`.
Its PDF/image analysis and counts describe the embedded preview, not verified
structure of the original iWork document. Native IWA decoding and directory-form
iWork packages are not implemented. Missing or invalid previews produce partial
reports. The source archive is never rewritten.

## Optional output files

`--output-dir DIRECTORY` writes one `.md` and one `.json` per document, including
in batch mode. Names combine a run identifier and request sequence. Files use
exclusive creation and Unix mode `0600`, and never overwrite an existing file.
Writes are not an atomic pair: a later JSON failure can leave a Markdown file.
The response reports output errors and successfully created artifacts.

## Safety and support boundaries

Full extraction is still bounded. PDF scanning stops at 4,096 pages and a
64 MiB extracted-text budget. Existing binary-source limits apply: PDF, image,
JSON, and plain-text inputs currently use a 16 MiB per-read cap. Larger inputs
can therefore fail before page/text limits are reached. Office/ebook containers
retain the limits documented in earlier phase notes.

JSON uses a non-DOM visitor with normalized array paths, scalar statistics,
samples, and repeated-array descriptions. The bounded source bytes are held
in memory; this is not constant-memory streaming input. Limits include depth
64, two million nodes, 16,384 tracked scalar/array paths, and 524,288 distinct
value hashes. Distinct counts are hash-based, not collision-free guarantees.

Safety budgets are adapter-level limits, not OS-enforced memory or wall-time
sandboxing. Unsupported legacy Office formats and other identified formats may
offer identity/metadata only. No cloud service or subprocess converter is used.

## Lightweight WASM delivery seam

The existing upstream `liteparse-wasm` remains the browser PDF path. This
native `litx` workspace has not become WASM-compatible.

A future browser delivery should separate byte/container/XML parsing and shared
report/Markdown models from native filesystem, Tesseract, and optional FFmpeg
adapters. OCR should use the browser callback seam rather than native Tesseract.
The native Parquet row decoder is independently feature-gated as
`parquet-preview`; library-only builds can disable default features and retain
footer inspection. The current CLI dependency enables the library defaults.

No browser bundle, WASM size target, or cross-platform verification is claimed.
Tesseract stays enabled in this native workspace throughout.

## Verification, 2026-11-18

All commands used the independent workspace manifest and isolated native target
and home directories.

- Full workspace test suite: 76 passed, zero failures or ignored tests, using
  `--include-ignored --test-threads=4` and the local English Tesseract model.
  This includes PDF/image OCR and reuse of one OCR engine across files.
- Metadata-only Parquet test with `-p litx-inspect --no-default-features`: passed.
- Workspace all-target Clippy with `--no-deps -- -D warnings`: passed after
  correcting two collapsible-condition lint findings.
- Workspace formatting and `git diff --check`: passed.
- Cargo feature inspection: upstream `liteparse/tesseract` remains enabled.
- Entire upstream tree outside `extensionalities/`: no diff against
  `14c9eb64b8c843216c114eed8bfcb0aa404bb622`. Protected root manifest,
  root lockfile, and upstream CLI are unchanged. AskClaire-MONO remains clean.

Two existing upstream dead-code warnings remain; upstream files were not
changed to silence them. These were native debug-profile checks, not a release
benchmark. Optional Magika/FFmpeg configurations, browser/WASM builds, and other
platforms were not exercised by this phase's checks. No speech-to-text dependency
or model was added.

# Phase 3: native ebooks, comics, and DjVu

Continues after the reviewed Phase 2 gate under the user's headless
implementation and local-commit authorization. No pushes.

## Implemented

- EPUB: resolve container → OPF → spine in declared order; Dublin Core claims,
  visible XHTML body text, OPF cover metadata and EPUB 3 cover-image properties.
  Decode package URI paths before safe resolution. Ignore XHTML DTD declarations
  without fetching or expanding them. Support predefined XML entities and
  XHTML nonbreaking spaces; reject other named entities cleanly.
- MOBI/AZW/AZW3: bounded PalmDB record reads, MOBI/EXTH claims, UTF-8 and
  Windows-1252 text, native PalmDOC literal/back-reference decompression,
  record trailer stripping, and cover records. Unsupported HUFF/CDIC and
  encryption retain available metadata and produce explicit text-stage errors.
  KF8 text is best-effort legacy text only, not a full KF8 reconstruction.
- CBZ: image pages in archive-entry order, page geometry, first-page cover.
- CBR: CRC-checked RAR4/RAR5 header walks and image-entry counts.
  No RAR decompression. Page dimensions and cover pixels remain unavailable.
- DjVu: single-page and bundled IFF framing, INFO dimensions/version/DPI,
  DIRM component locations, shared-dictionary exclusion from page counts.
  BZZ text and indirect multi-page documents report explicit limitations.

## Bounds and provenance

All additions stay inside `extensionalities/`. Random reads use a 16 MiB
per-read budget; existing ZIP entry/count/directory/inflation budgets remain.
EPUB manifests/spines and comic/RAR entry scans cap at 4096 entries.
Native binary parsing uses checked lengths, offsets and record boundaries.
Preview text respects Unicode scalar limits while native structure counts
remain independent. No external decompressor, subprocess, or network request
runs in these parsers. Thumbnail writes preserve fitting embedded bytes,
downscale larger images and refuse to overwrite any existing output.

## Verification

TDD: new format integration tests failed before the readers existed, then
passed. An additional regression failed on encoded EPUB spine paths before
safe URI decoding and inert XHTML doctype support were implemented.

Checks:

- Workspace formatting and strict Clippy with `--all-targets --no-deps`.
- Full locked workspace tests: 32 total, including 29 `litx-inspect` tests.
- Byte-identical covers and no-overwrite tests across EPUB, MOBI, AZW3 and CBZ.
- Truncated and mutated EPUB, MOBI, CBZ, RAR4/RAR5 and DjVu fixtures:
  no panics, previews remain bounded.
- External-entity regression: no expansion, metadata/other spine text retained.
- Protected upstream paths unchanged against baseline
  `14c9eb64b8c843216c114eed8bfcb0aa404bb622`; AskClaire-MONO unchanged.

Fixtures are self-authored framing tests, not third-party conformance corpora.
RAR/DjVu decoders and independent ebook renderers were not used to certify
the synthetic fixtures. Two pre-existing upstream dead-code warnings remain;
upstream files are deliberately unchanged.

## Remaining roadmap

Phase 4: PDF normalization/rendering and image heuristics/opt-in OCR probe.
Phase 5: bounded ffprobe metadata, excerpts/VAD/transcription, contact sheets,
BIF slicing and NFO sidecars. Phase 6: subtitle parsers, evidence ladder,
timing diagnosis, opt-in fetching and synchronized output.

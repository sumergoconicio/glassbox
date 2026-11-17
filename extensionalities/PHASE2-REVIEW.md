# litx inspect: Phase 2 review

## Handoff

- **handoff_schema:** chai-handoff/v1
- **Identity:** work_id `litx-inspect`; producer Droid; consumer Sri; artifact `extensionalities/PHASE2-REVIEW.md`; updated 2026-11-17 UTC.
- **State:** native Office Quick Look through Phase 2; stop here for review. Next action requires Sri's sign-off before Phase 3.
- **Source:** `liteparse-extended`, branch `feat/litx-inspect`, predecessor `2ed8bba`; implementation paths are under `extensionalities/`. This report accompanies the Phase 2 commit.
- **Authority:** approved implementation scope and local commits from the conversation; no upstream edits, AskClaire changes, pushes, delivery changes, or Phase 3 work.
- **Evidence:** tests, lint, fidelity script and logs below. Missing coverage includes FFmpeg linking, real ONNX inference, and broad document fidelity.
- **Receipt:** Sri; pending review; unresolved input is Phase 2 sign-off; next action is review only.

## Delivered behavior

- Byte/container verification, provenance-tagged metadata from `docProps/core.xml`, `docProps/app.xml`, and ODF `meta.xml`.
- Namespace-checked Dublin Core language claims and packet language mapping. Claims are document assertions, not verified identities.
- DOCX main-body text and sections; PPTX slides in presentation relationship order; ODF paragraphs, tabs, explicit spaces, and slides.
- Native XLSX/ODS sheet profiles: observed bounds, occupied cells, formulas, merged ranges, typed column counts, byte statistics, capped distinct hashes, samples, and prose scores.
- Deterministic preflight for bounded logical units. Less than 20 characters or image presence flags `needs_ocr`. `coverage_available` is false: no rendered geometry or OCR is performed.
- Bounded flat text preview and independent shared table-text budget. Full parsed structure and total native character counts do not stop at `--max-chars` or `--max-units`.
- Embedded thumbnail copying when already small, otherwise downscaling without upscaling. Output paths never overwrite existing files.

## Logical table contract

`tables` contains explicit document tables in DOCX, PPTX, ODT, and ODP. XLSX/ODS grids use `sheets` instead.

All indices, logical units, rows, and columns are one-based. Each table records its archive part, section/slide number, dimensions, explicit header rows, parent table, and cells. Each cell records its anchor, row/column span, text, original character count, truncation flag, and nested table indices.

- **DOCX:** `w:tbl`, `w:gridSpan`, `w:vMerge`, legacy `w:hMerge`, row grid offsets, and explicit `w:tblHeader`. Logical units are sections, not rendered pages.
- **PPTX:** DrawingML `a:tbl`, grid/row spans, and horizontal/vertical merge continuations. Visual first-row styling does not imply a declared header, so `header_rows` stays empty.
- **ODT/ODP:** ODF `table:table`, declared header-row groups, repeated rows/cells, covered cells, spans, names, and nested tables.
- Covered/continuation cells refer to their anchor instead of duplicating it. Continuation text is retained where present.
- Nested table text stays in its nested table; surrounding parent-cell paragraphs retain their order. Parent cells link to nested tables explicitly.
- Malformed merge structures generate a `table` error and discard that table and its nested subtree, without discarding other useful report data.
- `complete` means the table structure parsed, not that all cell text fits the budget. Inspect each cell's `text_truncated`.

No inferred tables, cell bounding boxes, widths, styling-derived semantics, page geometry, rendered reading order, or formula evaluation are claimed.

## Bounds and safeguards

- ZIP: 256 MiB physical and declared total expanded size; 4096 entries; 2 MiB classic central-directory bytes checked before directory loading; 16 MiB per expanded entry and 256 MiB cumulative entry reads. Entry-name checks and duplicate/security warnings remain active.
- ZIP64 directories and multi-disk archives are explicitly unsupported rather than bypassing the classic directory guard.
- XML: 200000 element nodes and depth 128 per part; no DTD/external entity expansion; namespace-aware names and attributes.
- Sheet telemetry: 16384 tracked columns and 524288 distinct hashes across the report. Capped distinct counts are lower bounds, and hashes are not collision-free identities.
- Document tables: 4096 tables, 200000 cells, 16384 columns, and bounded row/span/repetition handling. Huge ODF repetitions do not expand the blank spreadsheet tail.
- External or package-escaping relationships are rejected; archive paths are not extracted to disk.

These are explicit parsing bounds, not measured process-memory guarantees or a comprehensive security audit.

## Verification, Linux ARM64

Use the target-isolated environment described in `README.md`. All Cargo commands target `extensionalities/Cargo.toml`; built-in Tesseract stays enabled.

| Check | Result |
|---|---|
| Workspace formatting check | PASS |
| `cargo clippy --locked --all-targets --no-deps -j 8 -- -D warnings` | PASS |
| `cargo test --locked -j 8` | PASS, 24 tests |
| Magika feature, missing-runtime degradation test | PASS |
| All-features test build | BLOCKED: missing native `libavutil.pc`/FFmpeg development libraries; build-time GPL notice observed |
| Real Magika/ONNX inference | UNTESTED: no runtime/model validation |
| Upstream protected files and AskClaire worktree | Unchanged |

Two existing upstream dead-code warnings remain in normal dependency builds. Workspace strict lint uses `--no-deps`; no warnings were suppressed or upstream files edited.

Regression coverage includes forged namespaces, extension disagreements, malformed/encrypted XML packages, external relationships, entry count and read caps, ZIP64 rejection, native sheet order, repeated ODF rows, Unicode preview truncation, thumbnail byte identity/no-overwrite, nested tables, and merge semantics.

### Ground truth and text fidelity

`scripts/verify-fidelity.py` uses only self-authored fixtures, unique output directories, isolated LibreOffice profiles, 60-second subprocess timeouts, `unzip`, `xmllint --nonet`, and upstream `lit parse --no-ocr`.

- ZIP names, sizes, compressed sizes, and XML titles matched independent tool reads for seven fixtures.
- Embedded ODF thumbnails matched `unzip -p` byte-for-byte for ODT, ODP, and ODS.
- `notes.docx`: native 86 vs rendered 85 characters; whitespace-token multisets matched.
- `tables.docx`: native 147 vs rendered 192 characters; whitespace-token multisets matched. Layout whitespace differs.
- `grid.xlsx`: native 260 vs rendered 107 characters. Native extraction retained prose that the default rendered sheet clipped to `insp`; native boolean `true` appeared as rendered `TRUE`. Native and rendered representations intentionally differ.
- LibreOffice did not produce PDFs for the minimal `deck.pptx`, `notes.odt`, `deck.odp`, or `grid.ods` fixtures. PPTX loaded but PDF export failed; ODF sources could not be loaded. Their native tests pass, but rendered fidelity remains BLOCKED, not proven.
- Fifteen scorer inputs matched the original Go `jsoninspect.CalculateProseDensity` reference within `1e-12`. A discovered Roman-numeral/letter classification mismatch was fixed using Unicode 15 categories, matching the inspected Go runtime. `fixtures/prose_reference.go` reproduces the JSON reference from the unchanged AskClaire package.

Local evidence, not committed: `target/verification/phase2-tests.log`, `clippy.log`, `phase2-magika.log`, `phase2-features.log`, and `fidelity-20261117T213408947062Z/summary.json`. Fixtures and the verification script are committed; generated profiles, PDFs, logs, and binaries are ignored.

## Remaining boundaries

Legacy binary DOC/PPT/XLS, strict OOXML namespace variants, full layout fidelity, broad real-world corpus validation, PDF geometry, OCR execution, subtitles, and media extraction are not delivered by this phase. Headers/footers, tracked-change policy, notes, comments, drawing text, and styles are not a full Office semantic model.

The default Quick Look path does not invoke LibreOffice or FFmpeg. Native XML parsing avoids those runtime requirements; the upstream parser still needs LibreOffice for its own Office-to-PDF path. No consumer delivery or Wasm/browser compatibility is claimed.

**Review gate:** Phase 3 has not started. No push or release action is authorized.

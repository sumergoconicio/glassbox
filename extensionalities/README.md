# LiteParse Extended: extensionalities

Fork-specific code lives here, outside upstream-owned paths. This independent
Cargo workspace has its own manifest, lockfile, and build output. Its `litx`
binary calls the upstream `liteparse` library through a path dependency.
Built-in Tesseract remains enabled.

## Scaffold commands

From the repository root, first isolate the native build environment in a
dedicated shell. Tesseract's cache is architecture-blind: never use a shared
`~/.tesseract-rs` cache across targets. Keep the real Cargo and Rustup stores
while giving each target its own home and build output:

```sh
HOST_TRIPLE=$(rustc -vV | awk '/^host:/ { print $2 }')
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_TARGET_DIR="$PWD/extensionalities/target/$HOST_TRIPLE"
export HOME="$PWD/extensionalities/target/home-$HOST_TRIPLE"
mkdir -p "$HOME"
```

This setup is for native Unix builds, not a cross-compilation recipe. Do not
reuse a target home for another architecture. Then run:

- Build: `cargo build --release --locked --manifest-path extensionalities/Cargo.toml -j 8`
- Test: `cargo test --release --locked --manifest-path extensionalities/Cargo.toml -j 8`
- Format: `cargo fmt --manifest-path extensionalities/Cargo.toml --all -- --check`
- Lint: `cargo clippy --release --locked --manifest-path extensionalities/Cargo.toml --all-targets -j 8 -- -D warnings`
- Version: `"$CARGO_TARGET_DIR/release/litx" --version`
- Library smoke check: `"$CARGO_TARGET_DIR/release/litx" defaults`

`defaults` serializes `LiteParseConfig::default()` from the actual upstream
library. It does not parse documents or exercise OCR. `litx` does not yet
implement upstream `lit` commands and must not replace a delivered parser.
No asynchronous runtime is needed until parsing is implemented.

## Upstream merge boundary

Do not edit root `Cargo.toml`, root `Cargo.lock`, upstream CLI source, or
upstream CI to register this workspace. Root workspace commands intentionally
do not build or test `litx`; run the commands above separately.

The nested `.gitignore` makes common fixture formats visible despite broad
upstream ignores. Generated `target/` directories and environment secrets stay
ignored. Visibility is not permission to commit private documents or logs.

The current upstream history contains no `extensionalities/` paths. This
minimizes file conflicts; it cannot guarantee that upstream will never adopt
the same name. Clean Git merges also do not guarantee API compatibility.
Upstream library or dependency changes may require updates here.

When updating upstream, keep its workspace untouched. Reconcile this
workspace's lockfile explicitly and rerun its build, tests, and lint checks.
PDFium remains an adjacent runtime dependency, and Office conversion still
requires LibreOffice. Separate output directories may duplicate compilation.

## Scaffold verification

Verified on Linux ARM64 with Rust/Cargo 1.95.0 against fork base
`14c9eb64b8c843216c114eed8bfcb0aa404bb622`:

- Independent workspace metadata, upstream field inheritance, and enabled
  Tesseract feature passed.
- Release build, both unit tests, and scaffold Clippy with warnings denied passed.
- Native version, help, default-configuration JSON, and unsupported-command
  rejection passed.
- Original workspace all-target build (excluding Python/NAPI), Clippy
  all-target checks, and both workspace formatting checks passed.
- Root manifest and lockfile remained byte-identical to the fork base.
  Fixture visibility, output/secret ignores, and upstream path-history checks passed.

Initial builds failed because the existing shared Tesseract cache contained
x64 objects. Retrying with an isolated native home and target directory passed;
the existing cache was not removed or replaced. Successful run logs are local
under `target/verification/`. This run used `target/native-home/` for the
isolated home and `target/native-aarch64/` for Cargo output; the setup above
uses the full host triple for future cache separation.

Upstream warnings remain. Document parsing, actual OCR execution, consumer
integration, and other platforms were not tested by this scaffold.

## Original scaffold scope

The initial scaffold added no EXIF, ID3, or Office extractors, consumer integration,
delivery naming, recipe changes, or release assets. Those are separate work.

## Native Quick Look, through Phase 4

`litx inspect PATH --pretty` now detects file bytes, lists guarded ZIP entries,
and inspects native DOCX/PPTX/XLSX and ODT/ODP/ODS XML. It extracts metadata,
bounded text previews, spreadsheet profiles, logical document tables, embedded
thumbnails, and deterministic OCR-preflight signals. No LibreOffice conversion,
rasterization, network access, or OCR runs during native Office inspection.

Use `--max-chars` (default 16000) for preview text and the separate shared table
text budget. `--max-units` (default 5) limits preflight only, not full structure
or character counts. `--thumbnail PATH` refuses to overwrite; its default edge
limit is 320 pixels. `--packet` adds the lightweight taxonomy projection.

Document tables preserve explicit row/column spans, declared header rows,
source parts, logical units, and nested table links. They do not claim visual
geometry or inferred header semantics. Spreadsheet grids remain in `sheets`,
not document `tables`. Partial failures appear in `errors`.

Optional `magika` and `ffmpeg-link` Cargo features are off by default. Magika
requires an existing `ORT_DYLIB_PATH`; nothing downloads a runtime automatically.
Linked FFmpeg requires native development libraries and may impose GPL
obligations depending on the selected build. Tesseract remains enabled.

See [the Phase 2 review report](PHASE2-REVIEW.md) for exact table semantics,
resource limits, and verification results. Following review and approval,
Phase 3 adds native EPUB spine/metadata/cover extraction, MOBI-family PalmDOC
text and EXTH metadata, CBZ image pages, CBR header-only listing, and DjVu page
geometry. See [Phase 3 notes](PHASE3-NOTES.md) for supported subsets and limits.

Phase 4 delegates PDF native text, metadata and preflight to upstream LiteParse.
PDF thumbnails render the first page only when requested. Raster images report
sampled white fraction, luminance histogram, aspect ratio and available PNG/JFIF
physical density. Image classification is explicitly heuristic, not OCR proof.
`--probe-ocr` adds local English Tesseract word counts and mean confidence;
missing trained data produces a partial report, never an automatic download.
See [Phase 4 notes](PHASE4-NOTES.md) for bounds and six-page verification.

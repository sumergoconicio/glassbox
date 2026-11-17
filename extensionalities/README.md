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

## Scope

This scaffold adds no EXIF, ID3, or Office extractors, consumer integration,
delivery naming, recipe changes, or release assets. Those are separate work.

# Installation

Glassbox ships from source.
Build it with Cargo inside a checkout of the Glassbox repository.

## Prerequisites

- A current stable Rust toolchain with Rust 2024 support.
- A C/C++ compiler, linker, and CMake.
- Tesseract and Leptonica development files.
- Tesseract language data for the languages you OCR.

See the inherited [native build prerequisites](../../CONTRIBUTING.md#development-prerequisites) for platform details.
Do not disable Tesseract to make a build pass.

## Build

Some build archives vary by CPU architecture.
A default `HOME` that already holds foreign-architecture Tesseract archives breaks native builds.
The fix is an isolated, architecture-specific build `HOME` under the downstream target directory.
The real Cargo, Rustup, and shared caches stay pinned at their original paths.
No cache is deleted. Missing dependencies and new native archives may still download.

From the repository root:

```sh
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
XDG_CACHE_HOME="${XDG_CACHE_HOME:-$HOME/.cache}"
BUILD_HOME="$PWD/extensionalities/target/build-home-$(uname -m)"
mkdir -p "$BUILD_HOME"
env HOME="$BUILD_HOME" CARGO_HOME="$CARGO_HOME" RUSTUP_HOME="$RUSTUP_HOME" \
  XDG_CACHE_HOME="$XDG_CACHE_HOME" CARGO_TARGET_DIR="$PWD/extensionalities/target" \
  cargo build --locked --release --manifest-path extensionalities/Cargo.toml --bin glassbox
GLASSBOX="$PWD/extensionalities/target/release/glassbox"
```

The recipe captures the real cache paths before changing `HOME`.
It creates the build `HOME` if needed; it never deletes existing caches.
The build downloads dependencies, including PDFium.
This command builds `glassbox` without building the `litx` compatibility executable.
Reuse the same target directory for local builds on the same platform.
See [Local build cleanup](automation.md#local-build-cleanup) for isolated verification runs.

This recipe follows the setup used for the current local ARM64 builds.
A complete fresh-machine installation, with new network downloads and prerequisite installs, has not been validated yet.

## Offline builds

Fetch all dependencies before going offline.
Cargo fetch alone does not populate native build caches.
Complete a build with the same build `HOME` and target directory before going offline.
Prepare the OCR language data in the same step.
Set `TESSDATA_PREFIX` to your language-data directory when it is not found automatically.

## Optional FFmpeg linking

The optional `ffmpeg-link` feature links against system FFmpeg libraries.
It requires their pkg-config files, including `libavutil.pc`.
Without them, use the default build.
This feature is not fully verified yet.

## Platform coverage

Glassbox is developed and locally tested on Linux ARM64 (aarch64).
Linux x64 is required for release coverage and is not yet verified locally.
The Glassbox CI workflow targets Linux x64 and ARM64.
Other platforms inherit the upstream prerequisites and remain unverified.
Artifact saving and directory inventory are Linux-only.

## Verify the build

```sh
"$GLASSBOX" --help
"$GLASSBOX" defaults
"$GLASSBOX" inspect extensionalities/fixtures/hello.txt --format text
```

`--help` lists `parse`, `inspect`, `extract`, and `defaults`.
`defaults` prints the upstream library default configuration as JSON.
The fixture inspection exits 0 and prints a summary starting with `# Glassbox metadata inspection`.

## Distribution

This project currently provides no published Glassbox packages; build from source.
Published LiteParse packages install upstream tools, not Glassbox.
Upstream code, license, and attribution stay in the repository root.

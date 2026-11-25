# Glassbox

> What's in the box?!

Glassbox parses, inspects, and extracts documents locally.
It reports file identity and structure without cloud services.
It builds on [LiteParse](https://github.com/run-llama/liteparse) and adds a local triage layer.

This directory holds the isolated downstream workspace inside the Glassbox fork.

**Development status:** build from source.
Downstream reports use schema version 3, sampled fingerprints above 128 MiB, and `bodymatter:v1` Markdown markers.
Read the [user guides](docs/README.md) for installation, commands, formats, and troubleshooting.

## Quickstart

Run these commands from the repository root, the parent of this directory.
Capture your real tool caches first, then build with an isolated, architecture-specific `HOME`.
This prevents stale foreign-architecture Tesseract archives and preserves existing caches.
See [Installation](docs/installation.md) for full build details.

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
"$GLASSBOX" inspect extensionalities/fixtures/hello.txt --format text
```

The command exits 0.
The text summary starts with `Schema: 3`, then prints status, source, format, size, SHA-256, readiness, and counts.
It contains no body text.

### Prerequisites

Read [Installation](docs/installation.md) for toolchain, OCR data, platform coverage, and verification steps.
See [Optional dependencies and the installation-agent prompt](DEPENDENCIES.md) for helper setup and saved parse paths.
Read [Local build cleanup](docs/automation.md#local-build-cleanup) for disposable test and build trees.

## Usage

Glassbox handles three tasks:

| Task | Command | Output |
| --- | --- | --- |
| Parse like upstream LiteParse | `glassbox parse FILE [ARGS]` | Full upstream behavior: commands, options, exit codes, and files. |
| Triage a file without reading content | `glassbox inspect PATH` | Schema v3 identity, SHA-256 or sampled fingerprint, format type, metadata, structure, and readiness. No body text, row values, or OCR output. |
| Extract document content | `glassbox extract PATH [--pages ...]` | Full document or selected units as `bodymatter:v1` Markdown, plain text, or v3 JSON. Local OCR runs only where needed. |

Pass a directory to `glassbox inspect` for an ordered JSONL inventory.
Pass `--batch` to `inspect` or `extract` to read JSONL requests from stdin.
See the [CLI reference](docs/cli.md) for every flag and migration note.

These older README sections live in the user guides:

- [<a id="inspect-before-extracting"></a>Inspect before extracting](docs/inspect.md)
- [<a id="extract-document-content"></a>Extract document content](docs/extract.md)
- [<a id="preview-data"></a>Preview data](docs/formats.md)
- [<a id="save-markdown-and-json"></a>Save Markdown and JSON](docs/extract.md)
- [<a id="process-json-lines"></a>Process JSON Lines](docs/reports.md)
- [<a id="read-completeness-and-provenance"></a>Read completeness and provenance](docs/reports.md)

## Format support and limits

Read [Formats and limits](docs/formats.md) for the support matrix and fixed caps.
Format identification never implies extraction support.
Dataset outputs stay bounded previews.
Glassbox is not a malware scanner or a security sandbox.

### Development checks

Run downstream checks from the repository root:

```sh
cargo fmt --manifest-path extensionalities/Cargo.toml -p glassbox-cli -p glassbox-inspect --check
cargo test --locked --manifest-path extensionalities/Cargo.toml --workspace
python3 -B extensionalities/scripts/ci.py check
```

Keep all downstream implementation inside `extensionalities/`.

### Downstream continuous integration

Moved to [Automation](docs/automation.md#continuous-integration).

### Branch and merge policy

Moved to [Automation](docs/automation.md#branch-and-merge-policy).

### Weekly upstream synchronization

Moved to [Automation](docs/automation.md#weekly-upstream-synchronization).

### Activation checklist

Moved to [Automation](docs/automation.md#one-time-activation-checklist).

## Docs and support

- [User guides](docs/README.md): installation, CLI, inspection, extraction, formats, automation, reports, and troubleshooting.
- [CLI implementation](crates/litx/src/main.rs) and [CLI tests](crates/litx/tests/inspect_cli.rs).
- [Inspection library](crates/litx-inspect/src/lib.rs).
- [Issue tracker](https://github.com/sumergoconicio/glassbox/issues).
- [Upstream LiteParse documentation](https://developers.llamaindex.ai/liteparse/) for the inherited parsing library.

Report the command, diagnostics, and a small non-sensitive reproducer.
Never upload private documents, credentials, or raw extraction reports.

## License and attribution

Glassbox uses the [Apache License 2.0](../LICENSE).
It retains LiteParse's upstream code, license, and attribution.
PDFium and Tesseract provide inherited PDF and OCR capabilities.
The [original LiteParse documentation](../README.md) remains unchanged in the repository root.

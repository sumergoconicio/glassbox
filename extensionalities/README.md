# Glassbox

> What's in the box?!

Glassbox parses, inspects, and extracts documents locally.
It reports file identity and structure without cloud services.
It builds on [LiteParse](https://github.com/run-llama/liteparse) and adds a local triage layer.

This directory contains the isolated downstream inspection workspace inside the Glassbox fork.

**Development status:** build from source.
Recent changes still require independent review.
The [user guides](docs/README.md) cover installation through troubleshooting.

## Quickstart

Run from the repository root, the parent of this directory.
Capture the real tool caches first, then build with an isolated, architecture-specific HOME.
This avoids stale foreign-architecture Tesseract archives and never deletes existing caches.
See [Installation](docs/installation.md) for details.

```sh
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
XDG_CACHE_HOME="${XDG_CACHE_HOME:-$HOME/.cache}"
BUILD_HOME="$PWD/extensionalities/target/build-home-$(uname -m)"
mkdir -p "$BUILD_HOME"
env HOME="$BUILD_HOME" CARGO_HOME="$CARGO_HOME" RUSTUP_HOME="$RUSTUP_HOME" \
  XDG_CACHE_HOME="$XDG_CACHE_HOME" CARGO_TARGET_DIR="$PWD/extensionalities/target" \
  cargo build --locked --release --manifest-path extensionalities/Cargo.toml --workspace --bins
GLASSBOX="$PWD/extensionalities/target/release/glassbox"
"$GLASSBOX" inspect extensionalities/fixtures/hello.txt --format text
```

The inspection exits 0.
The text summary starts with `# Glassbox metadata inspection`, then lists format, size, SHA-256, and counts.
It contains no body text.

### Prerequisites

See [Installation](docs/installation.md) for toolchain, OCR data, platform coverage, and verification.

## Usage

Glassbox serves three different jobs:

| Job | Command | Result |
| --- | --- | --- |
| Parse documents exactly like upstream LiteParse | `glassbox parse FILE [ARGS]` | Complete upstream behavior: commands, options, statuses, files. |
| Triage a file without exposing content | `glassbox inspect PATH` | Identity, full streamed SHA-256, qualified counts, native signals, embedded metadata. No body text, row values, or OCR output. |
| Take document content out | `glassbox extract PATH [--pages ...]` | Full request or selected native units as Markdown, text, or JSON. Local OCR only where needed. |

Point `glassbox inspect` at a directory for a JSONL inventory.
Use `--batch` on either operation to read JSONL requests from stdin.
See the [CLI reference](docs/cli.md) for every flag and migration.

These older README sections moved into the guide set:

- [<a id="inspect-before-extracting"></a>Inspect before extracting](docs/inspect.md)
- [<a id="extract-document-content"></a>Extract document content](docs/extract.md)
- [<a id="preview-data"></a>Preview data](docs/formats.md)
- [<a id="save-markdown-and-json"></a>Save Markdown and JSON](docs/extract.md)
- [<a id="process-json-lines"></a>Process JSON Lines](docs/reports.md)
- [<a id="read-completeness-and-provenance"></a>Read completeness and provenance](docs/reports.md)

## Format support and limits

See [Formats and limits](docs/formats.md).
Identification never implies extraction support.
Dataset outputs stay bounded previews.
Glassbox is not a malware scanner or a security sandbox.

### Development checks

Run downstream checks from the repository root:

```sh
cargo fmt --manifest-path extensionalities/Cargo.toml -p glassbox-cli -p glassbox-inspect --check
cargo test --locked --manifest-path extensionalities/Cargo.toml --workspace
python3 -B extensionalities/scripts/ci.py check
```

Keep downstream implementation inside `extensionalities/`.

### Downstream continuous integration

Moved to [Automation](docs/automation.md).

### Branch and merge policy

Moved to [Automation](docs/automation.md).

### Weekly upstream synchronization

Moved to [Automation](docs/automation.md).

### Activation checklist

Moved to [Automation](docs/automation.md).

## Docs and support

- [User guides](docs/README.md): installation, CLI, inspection, extraction, formats, automation, reports, troubleshooting.
- [CLI implementation](crates/litx/src/main.rs) and [CLI tests](crates/litx/tests/inspect_cli.rs).
- [Inspection library](crates/litx-inspect/src/lib.rs).
- [Issue tracker](https://github.com/sumergoconicio/glassbox/issues).
- [Upstream LiteParse documentation](https://developers.llamaindex.ai/liteparse/) for the inherited parsing library.

Report the command, diagnostics, and a small non-sensitive reproducer.
Do not upload private documents, credentials, or raw extraction reports.

## License and attribution

Glassbox uses the [Apache License 2.0](../LICENSE).
It retains LiteParse's upstream code, license, and attribution.
PDFium and Tesseract provide inherited PDF and OCR capabilities.
The [original LiteParse documentation](../README.md) stays unchanged in the repository root.

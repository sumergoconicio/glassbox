# Glassbox

> What's in the box?!

Glassbox helps developers inspect unfamiliar files and extract document content locally.
It reports file identity, native structure, text, previews, and the sources behind its findings.
Outputs include JSON, readable summaries, and Markdown.

Glassbox builds on [LiteParse](https://github.com/run-llama/liteparse).
This directory contains the isolated downstream inspection workspace.

**Development status:** build from source.
Recent inspection changes still require independent review.
The primary command is `glassbox`; `litx` remains available as a compatibility alias.
Rust package names remain `litx` and `litx-inspect`.

## Quickstart

Run these examples from the repository root, the parent of this directory.
Use a checkout containing [`extensionalities/Cargo.toml`](Cargo.toml).
These examples use a POSIX shell.

### Prerequisites

- A current stable Rust toolchain with Rust 2024 support.
- A C/C++ compiler, linker, and CMake.
- Tesseract and Leptonica development dependencies for the native build.
- Tesseract language data for OCR.

See the inherited [native build prerequisites](../CONTRIBUTING.md#development-prerequisites) for platform details.
Source builds can download dependencies, including PDFium.
Prepare dependencies and OCR language data before offline use.
Set `TESSDATA_PREFIX` to your language-data directory when necessary.

Run the bundled text example:

```sh
cargo run --locked --release --manifest-path extensionalities/Cargo.toml -p litx -- inspect extensionalities/fixtures/hello.txt --format text
```

After compilation, expect:

```text
Text (Plain Text)
50 bytes, 50 chars
This is a small self-authored inspection fixture.
```

For the remaining examples, point `GLASSBOX` at that executable:

```sh
GLASSBOX="${CARGO_TARGET_DIR:-$PWD/extensionalities/target}/release/glassbox"
"$GLASSBOX" --help
```

Published LiteParse packages install the upstream tools, not this downstream inspection CLI.

## Usage

Replace `/path/to/...` with a local file.
Inspection runs locally; the examples do not require an OCR service.

### Inspect before extracting

```sh
"$GLASSBOX" inspect /path/to/document.pdf --identify --pretty
```

`inspect` defaults to a bounded preview.
Its defaults include five PDF pages, 500 whitespace-delimited words, and 16,000 text characters.
`--identify` enables local OCR where supported.
Use `--max-pages`, `--max-tokens`, and `--max-chars` to set explicit limits.
Word counts are not model-specific token counts.

JSON is the default output.
Use `--format text` for a summary or `--format markdown` for Markdown.

### Extract document content

```sh
"$GLASSBOX" extract /path/to/document.docx --format markdown
```

`extract` requests the whole document and enables local OCR where supported.
Use `--no-ocr` to disable OCR.
Character budgets and format-specific safety limits still apply.
Dataset outputs remain previews, including under `extract`.

### Preview data

```sh
"$GLASSBOX" inspect /path/to/dataset.csv --format markdown
```

CSV and TSV produce bounded row previews.
JSON and XML expose structural profiles alongside bounded content.
Spreadsheet reports include sheet details and column summaries.
These previews are not complete dataset exports.

### Save Markdown and JSON

The next command creates `extensionalities/target/glassbox-output/` and writes uniquely named Markdown and JSON files.
It does not overwrite existing files.
Source documents are not modified.

```sh
"$GLASSBOX" extract /path/to/document.pdf --output-dir ./extensionalities/target/glassbox-output --format markdown
```

The JSON report records saved paths under `artifacts`.
Check `errors` for failures while saving.

### Process JSON Lines

Send one request per line and receive one response per line.
Responses retain request identifiers.
File results appear under `report`; malformed requests produce an `error` object.

```sh
printf '%s\n' '{"id":"sample","path":"extensionalities/fixtures/hello.txt"}' | "$GLASSBOX" inspect --batch
```

Both `inspect` and `extract` accept `--batch`.
Batch mode does not accept `--pretty`, `--format`, or `--thumbnail`.

## Format support and limits

Support depends on the format and the structures present.
Identification does not imply extraction support.

| Input | Current behavior |
| --- | --- |
| PDF | Native text and page signals, with local OCR where needed. |
| Raster images | Image properties, thumbnails, and optional local OCR. |
| DOCX, XLSX, PPTX | Native content, logical structure, and previews from supported package parts. |
| ODT, ODS, ODP | Native content and structure from supported OpenDocument parts. |
| EPUB and MOBI-family ebooks | Native text and metadata from supported, unencrypted content. |
| Plain text and source files | Bounded text inspection. |
| CSV and TSV | Bounded row previews with column headers. |
| JSON and XML | Structural profiles and bounded content. |
| ZIP and ZIP-based containers | Bounded entry inspection and supported embedded previews. |
| DjVu | Identification only; no text, image, or OCR extraction. |
| Parquet | Identification only; extraction is deferred. |

Office inspection uses native readers rather than LibreOffice conversion.
Other containers, legacy formats, and protected documents can yield identification or partial results without extracted content.
Apple iWork handling uses available embedded previews; it is not a complete native document decoder.

### Read completeness and provenance

Treat results as evidence about a file, not verified statements about its author or contents.
Reports distinguish metadata claims, native content, and derived previews.
Use these fields before accepting a result:

- `format.basis` and `format.disagreement` describe identification evidence and mismatches.
- `claims[].source` identifies where a metadata claim came from.
- `text.origin` and `markdown.basis` describe output provenance.
- `text.truncated` and `markdown.truncated` indicate incomplete output.
- `errors` records unsupported formats, parser failures, and other stage diagnostics.

A successful process exit does not guarantee complete extraction.
Inspect the report before treating empty output as an empty document.
Configured limits and parser safeguards can produce partial results.
Glassbox is not a security sandbox or malware scanner.

## Docs and support

- [CLI implementation](crates/litx/src/main.rs) and [CLI regression tests](crates/litx/tests/).
- [Inspection library](crates/litx-inspect/src/lib.rs) and [format regression tests](crates/litx-inspect/tests/).
- [Downstream workspace](Cargo.toml) for the isolated build boundary.
- [Issue tracker](https://github.com/sumergoconicio/glassbox/issues) for bug reports.
- [Upstream LiteParse documentation](https://developers.llamaindex.ai/liteparse/) for the inherited parsing library and bindings.

Include the command, relevant diagnostics, and a small non-sensitive reproducer when reporting a problem.
Do not upload private documents, credentials, or raw extraction reports.

### Development checks

Run downstream checks from the repository root:

```sh
cargo fmt --manifest-path extensionalities/Cargo.toml --all --check
cargo test --locked --manifest-path extensionalities/Cargo.toml --workspace
```

Keep downstream changes inside `extensionalities/`.
The only upstream-file exception is the approved ignore rule in the root `.gitignore`.
The upstream workspace, CLI, and package names remain separate.
The upstream `lit` command is not a Glassbox compatibility interface.

## License and attribution

Glassbox uses the [Apache License 2.0](../LICENSE).
It retains LiteParse's upstream code, license, and attribution.
PDFium and Tesseract provide inherited PDF and OCR capabilities.
The [original LiteParse documentation](../README.md) remains unchanged in the repository root.
Its package instructions and benchmarks describe upstream LiteParse, not Glassbox.

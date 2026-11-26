---
schema: "chai.metadata/v2"
id: "01M8A8KCH8J8GPJAW1JN0BTZ9S"
type: "documentation"
title: "CLI reference and parsing"
tags: ["cli", "reference", "parsing", "args", "liteparse", "runtime", "authoritative", "library", "configuration", "complex"]
---
# CLI reference and parsing

`glassbox` is one executable with five top-level commands.
The runtime `--help` output of your build is the authoritative reference.

## Command surface

| Command | Purpose |
| --- | --- |
| `glassbox parse FILE [ARGS]` | Upstream LiteParse parsing, unchanged. |
| `glassbox inspect PATH` | Schema v3 metadata-only inspection (file or directory). |
| `glassbox extract PATH` | Schema v3 content and media extraction with `bodymatter:v1` Markdown. |
| `glassbox models SUBCOMMAND` | Manage persistent model endpoints and capability bindings (`models.json`). |
| `glassbox defaults` | Print the upstream library default configuration as JSON. |

## Upstream parsing: `parse`

`glassbox parse` routes directly to the upstream CLI without changes.
It preserves every upstream command, option, default, exit status, and output file.
Hidden and future upstream commands work through automatic discovery.

```sh
"$GLASSBOX" parse /path/to/document.pdf
"$GLASSBOX" parse --help
```

Nested commands keep their upstream names, such as `glassbox parse is-complex /path/to/document.pdf`.
Help and error labels show Glassbox branding while output and attribution remain upstream.
Downstream flags do not apply to `parse`.

## Inspection: `inspect`

| Flag | Meaning |
| --- | --- |
| `PATH` | File or directory to inspect. Required without `--batch`. |
| `--batch` | Read one `{"id":...,"path":"..."}` JSON request per stdin line. Conflicts with `--pretty`, `--format`, and `--thumbnail`. |
| `--output-dir DIR` | Save a content-free Markdown summary and a v3 JSON report per file. Never overwrites. |
| `--recursive` | With a directory, recurse into same-mount real subdirectories. Links and other mounts are skipped. |
| `--jobs N` | Concurrent worker limit from 1 to 32. Default 1. Shared file and OCR budget. |
| `--thumbnail PATH` | Render one bounded thumbnail where supported. No OCR. Off by default. |
| `--thumbnail-max-edge N` | Thumbnail edge cap in pixels. Default 320. Range 1–4096. |
| `--format` | `json` (default), `text`, or `markdown`. Text and Markdown print content-free v3 summaries. |
| `--pretty` | Indent JSON output. Single-file JSON only. |
| `--full-hash` | Stream full SHA-256 even for files larger than 128 MiB. Works in single-file, directory, and batch modes. |
| `--verbose` | Include reader `provenance` in v3 JSON and summary outputs. Works in single-file, directory, and batch modes. |

File inspection never returns body text, row values, or OCR output.
Files up to 128 MiB receive `file.sha256`.
Larger files receive a `glassbox-sampled-sha256/v1` `file.fingerprint` unless `--full-hash` is set.

Directory inspection emits ordered JSONL carrying schema v3 inspect reports.
By default it scans immediate regular files only; add `--recursive` for same-mount subdirectories.
Directory inspection rejects `--pretty`, `--thumbnail`, and non-JSON `--format` with exit 64.
Saved `--output-dir` artifacts must sit outside the scanned directory.

## Extraction: `extract`

| Flag | Meaning |
| --- | --- |
| `PATH` | File to extract. Required without `--batch`. |
| `--pages [SELECTION]` | Select native units. Bare flag: first five. Integer: first N. Ranges: `3-7,10`. |
| `--max-pages N` | Deprecated alias for `--pages N`. Conflicts with `--pages`. |
| `--no-ocr` | Disable local OCR. Conflicts with `--probe-ocr` and `--identify`. |
| `--ocr-language LANG` | OCR language, such as `en`, `eng`, or `deu+fra`. Default `eng`. Reports the canonical code. |
| `--identify` | Compatibility flag. Local OCR stays limited to pages that need it. |
| `--probe-ocr` | Compatibility flag. Automatic local OCR behavior stays unchanged. |
| `--max-tokens N` | Bound whitespace-delimited word tokens. Not model subword tokens. |
| `--max-chars N` | Bound output characters. Default 67,108,864. Fixed reader caps still apply. |
| `--max-units N` | Bound processed preview or probe units. Default 5. |
| `--thumbnail PATH` | Save a bounded thumbnail alongside extraction. |
| `--thumbnail-max-edge N` | Thumbnail edge cap in pixels. Default 320. Range 1–4096. |
| `--packet` | Deprecated. Schema v3 `content.units` carries the extracted payload. |
| `--output-dir DIR` | Save Markdown content and a v3 JSON report per file. Never overwrites. |
| `--full-hash` | Stream full SHA-256 for embedded metadata even above 128 MiB. |
| `--verbose` | Include reader and extraction `provenance` in v3 JSON reports. |
| `--no-markers` | Omit `bodymatter:v1` structure comments from stdout Markdown and saved `.md` artifacts. |
| `--no-frontmatter` | Omit `schema: "glassbox:v1"` YAML frontmatter from `--asr` Markdown stdout and saved `.md` files. |
| `--imagegrid [MODE]` | Generate a video contact sheet: `default` (5x5), `adaptive` (1–10 rows), or `custom`. |
| `--every-seconds N` | Fixed interval in seconds for `--imagegrid custom` (or bare `--imagegrid`). Capped at 250 tiles. |
| `--quality` | Use 360x202 contact-sheet cells (1,920 px wide). Default with `--imagegrid`. Conflicts with `--performance`. |
| `--performance` | Use 240x134 contact-sheet cells (1,280 px wide). Conflicts with `--quality`. |
| `--imagegrid-format FMT` | Contact-sheet encoding: `jpg` (default under `--output-dir`) or `png`. |
| `-o`, `--output PATH` | Explicit `.jpg`, `.jpeg`, or `.png` file path for single-file `--imagegrid`. Rejected with `--batch`. |
| `--asr [MODE]` | Transcribe audio: `short` (default, VAD up to 60 s), `adaptive` (30 s every 15 min), or `full`. |
| `--extract-audio [FMT]` | Save full audio to `<stem>-extract.<ext>` in `--output-dir`: `wav` (default), `flac`, `m4a`, or `opus`. |
| `--asr-backend KIND` | Explicit ASR backend override: `openai` or `whisper-onnx`. |
| `--asr-url URL` | OpenAI-compatible ASR API base URL override. |
| `--asr-model ID` | OpenAI-compatible ASR model identifier override. |
| `--asr-api-key-env VAR` | Environment variable name holding the Bearer API key. Rejects raw secrets with exit 64. |
| `--asr-protocol PROTO` | OpenAI-compatible wire protocol: `transcriptions` (default) or `chat`. |
| `--asr-model-dir PATH` | Local Whisper ONNX model directory override. |
| `--asr-device DEV` | Local Whisper ONNX device override: `cpu` (default) or `cuda`. |
| `--asr-language LANG` | Optional BCP-47 or ISO language hint sent to the ASR backend. |
| `--format FORMAT` | `json` (default, schema v3 extract report), `text` (coverage line plus `[unit N ...]` sections), or `markdown` (`bodymatter:v1` Markdown unless `--no-markers` is set). |
| `--batch`, `--jobs N`, `--pretty` | Same behavior as `inspect`. |

Selection indices are one-based, inclusive, deduplicated, and returned in source order.
Place `PATH` before bare `--pages`, `--imagegrid`, `--asr`, or `--extract-audio` (or use `--flag=value`) so `PATH` is not parsed as a flag value.
A bare count above the document total completes with all available units.
An explicit range wholly past the document total fails with exit 1.
Dataset outputs stay bounded previews.
`--extract-audio` requires `--output-dir`. It runs standalone or with `--asr full` only, and exits 64 with `--asr short` or `--asr adaptive`.
`glassbox inspect` rejects all media flags with exit 64.
See [Formats and limits](formats.md) for native units per format and [Content extraction](extract.md#media-extraction) for media workflows.

## Model configuration: `models`

`glassbox models` manages named OpenAI-compatible endpoints and capability bindings without storing raw API keys.

| Command | Purpose |
| --- | --- |
| `glassbox models endpoint add NAME --base-url URL [--api-key-env VAR]` | Add or replace a named OpenAI-compatible HTTP endpoint. |
| `glassbox models endpoint list [--format text\|json]` | List saved endpoints without reading secret values. |
| `glassbox models endpoint remove NAME [--force]` | Remove an endpoint. Fails with exit 64 if bound unless `--force` is set. |
| `glassbox models bind CAPABILITY --endpoint NAME --model ID [--protocol transcriptions\|chat]` | Bind a capability (such as `asr`) to a saved HTTP endpoint. |
| `glassbox models bind CAPABILITY --local-onnx --model-dir PATH [--device cpu\|cuda]` | Bind a capability to a local offline ONNX model directory. |
| `glassbox models unbind CAPABILITY` | Remove a capability binding. |
| `glassbox models show [CAPABILITY] [--format text\|json]` | Show endpoints and bindings without printing secret values. |

Configuration storage and security rules:

- Config path order: `$GLASSBOX_CONFIG_DIR/models.json`, then `$XDG_CONFIG_HOME/glassbox/models.json`, then `~/.config/glassbox/models.json`.
- Glassbox creates missing config directories with mode `0700` and writes `models.json` atomically with mode `0600`.
- `--api-key-env` accepts only an environment variable identifier matching `^[A-Za-z_][A-Za-z0-9_]*$` and rejects raw keys with exit 64.

### Media and ASR environment variables

ASR resolution follows strict `CLI > env > models.json` precedence.
Setting both `GLASSBOX_ASR_URL` and `GLASSBOX_ASR_MODEL_DIR` without `--asr-backend` or `GLASSBOX_ASR_BACKEND` fails closed with exit 64.

| Variable | Purpose |
| --- | --- |
| `GLASSBOX_CONFIG_DIR` | Override the directory that stores `models.json`. |
| `GLASSBOX_ASR_BACKEND` | Select ASR backend (`openai` or `whisper-onnx`). |
| `GLASSBOX_ASR_URL` | OpenAI-compatible ASR API base URL. |
| `GLASSBOX_ASR_MODEL` | OpenAI-compatible ASR model identifier. |
| `GLASSBOX_ASR_API_KEY_ENV` | Environment variable name holding the Bearer API key. |
| `GLASSBOX_ASR_PROTOCOL` | OpenAI-compatible wire protocol (`transcriptions` or `chat`). |
| `GLASSBOX_ASR_TIMEOUT_SECS` | Per-window HTTP read timeout in seconds. Default `120`. |
| `GLASSBOX_ASR_MODEL_DIR` | Local Whisper ONNX model directory path. |
| `GLASSBOX_ASR_DEVICE` | Local Whisper ONNX execution device (`cpu` or `cuda`). |
| `GLASSBOX_FFMPEG`, `GLASSBOX_FFPROBE` | Override `ffmpeg` and `ffprobe` executable paths. |
| `GLASSBOX_FFMPEG_IO_TIMEOUT_SECS` | Idle-read timeout in seconds for piped FFmpeg audio streams. Default `30`, clamped to `1..=600`. |
| `ORT_DYLIB_PATH` | Override the ONNX Runtime shared library path (`libonnxruntime.so` or `libonnxruntime.dylib`). |

## Exit statuses

| Outcome | Downstream exit |
| --- | ---: |
| Complete requested operation | 0 |
| Partial result or unsupported operation | 2 |
| Operational failure | 1 |
| Invalid invocation | 64 |

`glassbox parse` keeps upstream exit codes.
Batch runs aggregate by severity: invalid requests (64) beat failures (1), which beat partial results (2), which beat complete (0).
Reports print to stdout; diagnostics print to stderr or inside report JSON.
Intentional `--pages`, `--asr short`, and `--asr adaptive` subsets leave source coverage `partial` while the requested operation exits 0.

## Command and file collisions

Nested `parse` subcommands reserve their bare token.
Pass `./screenshot`, an absolute path, or `glassbox parse FILE` when a filename matches a subcommand.

## Migrations

| Old usage | Current behavior |
| --- | --- |
| `inspect --identify`, `--max-pages`, `--max-tokens`, `--max-chars`, `--probe-ocr`, `--no-ocr`, `--ocr-language`, `--packet`, `--no-markers`, and all media flags (`--imagegrid`, `--every-seconds`, `--quality`, `--performance`, `--imagegrid-format`, `--asr`, `--asr-*`, `--extract-audio`, `--no-frontmatter`) | Rejected with exit 64 and migration advice. Use `glassbox extract`. |
| `inspect` body previews | Removed. Inspection is metadata-only. Use `glassbox extract`. |
| `extract --max-pages N` | Deprecated; maps to `--pages N` with a warning. |
| `extract --packet` | Deprecated; read v3 `content.units`. |
| Schema v1 or v2 JSON reports | Replaced by schema v3 across all reports. No `--schema 2` flag exists. See [Reports](reports.md). |
| `litx` executable name | Retained as a compatibility alias binary for `glassbox`. Prefer `glassbox` in new commands and scripts. |

See [Automation](automation.md) for batch and directory rules.
See [Reports](reports.md) for schema v3 JSON contracts.
See [OCR and troubleshooting](ocr-and-troubleshooting.md) for OCR models, media helpers, and common errors.

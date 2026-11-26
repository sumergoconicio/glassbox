---
title: "OCR and troubleshooting"
type: "documentation"
content_type: "Runbook"
schema: "chai.metadata/v2"
id: "01M8A8KCH8M69K2NE0T5R7MFDH"
tags: ["ocr", "troubleshooting", "configuration", "downstream", "http", "servers", "retains", "readable", "rendered", "pixel"]
container_id: "01M8A8KCH8CGBADTAZR9N6G7K9"
---

# OCR and troubleshooting

Downstream extraction runs OCR locally through Tesseract and reads models from disk.
`parse` retains upstream OCR configuration, including HTTP servers when explicitly configured.

## How OCR runs

- OCR runs only during `extract`. `inspect` never runs OCR; it only reports whether OCR looks useful.
- Extraction runs OCR only on pages that need it, such as scanned pages inside a PDF. A present text layer does not prove a readable page, and a missing one does not prove a scanned one.
- PDF pages are rendered with a 3072-pixel long edge for recognition. OCR results merge with native text per page.
- OCR failure never loses native text. The report keeps the native text and records a diagnostic.
- Raster images run OCR on the whole image whenever extraction has OCR enabled. The document-like heuristic only sets inspection advice; it never gates OCR. A recognized image unit carries OCR provenance: engine, language, confidence, and raster long edge.
- Pass `--no-ocr` to skip OCR entirely.

## Models and languages

The default language is English (`eng`). Pass `--ocr-language LANG` on `extract` to choose another language.
The flag accepts natural spellings such as `en`, canonical spellings such as `eng`, and lists joined with `+`.
Library callers set the language through `ExtractOptions.ocr.language`. The `parse` command keeps the upstream OCR language flags.
Glassbox maps common codes (`de` to `deu`, `zh` to `chi_sim`, and similar) and resolves every listed model file locally.
OCR provenance reports the canonical spelling, such as `eng` for `en`.
A missing or invalid language stops that OCR run with a diagnostic; native content stays.

Model lookup order:

1. `TESSDATA_PREFIX`, when set.
2. `~/.tesseract-rs/tessdata` or the platform equivalent.
3. System directories such as `/usr/share/tesseract-ocr/5/tessdata` or `/usr/share/tessdata`.

An explicit `TESSDATA_PREFIX` overrides automatic search; a bad directory does not fall back to system models.
Without it, Glassbox selects the first candidate containing `eng.traineddata`, then checks the requested language models.
A missing or oversized model stops OCR with a clear error. Fix it by installing language data and pointing `TESSDATA_PREFIX` at it.

## Resource limits

OCR respects the shared budget. Each file lease covers all its OCR work, so `--jobs N` bounds both together. Retained PDF rasters share one 128 MiB staging budget across workers. The budget charges the retained allocation, so a large queue cannot grow memory without bound. When staging is exhausted, that page's OCR is skipped with a diagnostic and native text is kept.

## Choosing parse or extract

`glassbox parse` gives you the upstream LiteParse pipeline, including layout reconstruction and upstream output formats and exit codes.
`glassbox extract` gives you native-unit selection, provenance, coverage facts, and bounded caps.
Use `parse` for full-document conversion flows. Use `extract` for unit-level content with an auditable report.
Both use upstream PDFium and OCR stages. Their selection, reports, output formats, and exit codes differ.

## Troubleshooting

| Symptom | Cause and action |
| --- | --- |
| Exit 64 on `inspect --max-pages ...` | Content flags belong to `extract`. Rerun as `glassbox extract FILE --pages ...`. |
| Exit 64 on `--pages 0`, `--pages 4-2`, or text like `not-a-range` | Selector is invalid. Use one-based positive ascending values. |
| Exit 64 when `--pages` and `--max-pages` are both set | The flags conflict. Keep one. |
| Exit 1 with `selection` diagnostic and no units | Every requested range index is past the total. Request an existing index. |
| Exit 2 with `unsupported` on an identified file | The format has no content reader. See [formats and limits](formats.md). |
| Exit 2 on DjVu or Parquet extraction | These formats are identification-only. Inspection completes; extraction does not. |
| Exit 1 on `extract` of a directory | Extraction reads single files. Inventory directories with `inspect DIR`. |
| Inventory reports the operation as unavailable | Linux (`/proc/self/mountinfo`) and macOS (local `fstatfs` and `st_dev` checks) support directory inventory. Run it on Linux or macOS. |
| Inventory exits 2 with `symlink_skipped` or `cross_mount_skipped` | Safety rules skipped those entries. Do not expect their reports. |
| Inventory rejects the root as non-local | Network mounts such as NFS or CIFS are rejected by design. Copy files to a local filesystem. |
| macOS inventory rejects `/tmp` or `/var` as a symbolic link | `/tmp` and `/var` are macOS symbolic links to `/private/tmp` and `/private/var`. Pass the real path under `/private/tmp` or `/private/var`. |
| macOS inventory emits `cloud_placeholder_skipped` (`Partial`) | Glassbox skips iCloud or FileProvider dataless placeholders (`SF_DATALESS`) without triggering a network download. |
| `metadata_cap_exceeded` or `content_limit` diagnostics | A fixed cap applied. Output is partial and disclosed. Caps are not configurable. |
| OCR diagnostic mentions missing `eng.traineddata` | Install Tesseract language data and set `TESSDATA_PREFIX`. |
| OCR diagnostic names another missing model, such as `zz.traineddata` | The requested `--ocr-language` has no local model. Install it or choose an available language. |
| OCR diagnostic says invalid local OCR language | The `--ocr-language` spelling is not accepted. Use codes such as `en`, `eng`, or `deu+fra`. |
| OCR diagnostic says staging budget exhausted | Fewer concurrent jobs (`--jobs`) or a narrower `--pages` selection frees staging capacity. |
| Report says `text_layer: present` but text is wrong | A text layer can embed bad text. Presence is a signal, not a quality guarantee. Check the page in a viewer. |
| Process exits 1 when stdout closes early | This is expected behavior, not a crash. Keep the pipe open or read the output. |
| Encrypted file yields no content | Passwords and DRM are not bypassed. Use the password-removed original. |
| `--format` rejected in batch mode | Batch output is JSONL. Drop `--format`, `--pretty`, and `--thumbnail`. |
| Exit 1 with an input-stage diagnostic | The file could not be opened or read, for example by permissions. Fix read access, then retry. |
| `media_helper_missing` diagnostic for `ffmpeg` or `ffprobe` | Install FFmpeg or point `GLASSBOX_FFMPEG` and `GLASSBOX_FFPROBE` at executable paths. See [Optional dependencies](../DEPENDENCIES.md). |
| `asr_asset_missing` for Whisper ONNX model files | Put `encoder_model[_int8].onnx`, `decoder_model_merged[_int8].onnx`, `generation_config.json`, and `vocab.json` in `model_dir` (or `model_dir/onnx/`). |
| `asr_asset_missing` for ONNX Runtime (`libonnxruntime`) | Set `ORT_DYLIB_PATH` to `libonnxruntime.so` or `libonnxruntime.dylib`, or install the cached runtime via `glassbox parse models pull ocria-nano`. |
| `asr_config_failed` on `--asr-device cuda` | CUDA refuses silent CPU fallback. Rebuild `glassbox-cli` with `--features whisper-onnx-cuda`, or pass `--asr-device cpu`. |
| Exit 64 on ambiguous ASR environment variables | Both `GLASSBOX_ASR_URL` and `GLASSBOX_ASR_MODEL_DIR` are set without a backend selector. Set `--asr-backend` or `GLASSBOX_ASR_BACKEND` (`openai` or `whisper-onnx`). |
| Compile error enabling `--features magika` with default features | Default `whisper-onnx` and optional `magika` pin incompatible `ort` versions. Build `magika` with `--no-default-features --features magika`. |

## Diagnostics first

Every failure lands in `metadata.diagnostics` with a stage and a code. Read them before the content. When you report a problem, include the command, the exit code, and the diagnostics. Never upload private documents or raw reports.

## Where to go next

- [CLI reference and parsing](cli.md): the full `parse` surface and every flag.
- [Metadata inspection](inspect.md) for OCR advice without OCR.
- [Content extraction](extract.md) for selection and coverage.
- [Automation and directory inventories](automation.md) for batch failures.

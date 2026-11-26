---
title: "Metadata inspection"
type: "documentation"
content_type: "Runbook"
schema: "chai.metadata/v2"
id: "01M8A8KCH87VDMX2AY8PF3JAYG"
tags: ["metadata", "inspection", "exposing", "subject", "keywords", "serializes", "properties", "advice", "hop", "routing"]
container_id: "01M8A8KCH8CGBADTAZR9N6G7K9"
---

# Metadata inspection

`glassbox inspect` reports what a file is without exposing content.
It returns file identity, format, embedded metadata, structure, and extraction readiness.
It never returns body text, row values, or OCR text.
Use [content extraction](extract.md) when you need document content.

## Report structure (schema v3)

Every inspect report uses `schema_version: 3` and `kind: "inspect"`.
It contains `status` (`complete`, `partial`, `unsupported`, or `failed`) and five keyed blocks:

- **`file`**: `path`, `name`, `bytes`, optional `sha256`, optional `fingerprint`, and optional `file_id`.
- **`format`**: `type`, optional `mime`, optional `extension`, and optional `extension_mismatch: true`.
- **`description`**: embedded properties (`title`, `authors`, `subject`, `keywords`, `language`, `created_at`, `modified_at`, and `software`). Serializes as `{}` when empty.
- **`structure`**: keyed `counts`, optional grouped `names`, optional `pdf` geometry, and optional `image` geometry.
- **`readiness`**: single-hop routing flags (`extractable`, `encrypted`, `text_layer`, and `ocr`).

Glassbox omits `diagnostics` and `artifacts` when empty.
Glassbox omits `provenance` unless you pass `--verbose`.
No `next_step` or `advice` block appears in v3 reports.

## Format classification

`format.type` classifies every file into one of 23 lowercase identifiers.
Conclusive byte detection overrides the filename suffix.
When byte detection is inconclusive, the longest known suffix decides (`tar.gz` before `gz`).
Unknown or absent suffixes map to `other`.

`format.extension` keeps the literal normalized lowercase suffix without a leading dot.
Glassbox omits `format.extension` when a filename has no suffix.
`format.extension_mismatch: true` appears only when conclusive byte detection disagrees with a known suffix family.
A generic ZIP with a container suffix such as `.docx` needs valid Office structure to match that family.
Without that structure, Glassbox classifies it as `archives` and sets `extension_mismatch: true`.
Unsupported raster formats such as PSD classify as `images` and return `unsupported` status instead of failing inspection.

| `format.type` | Extensions (longest suffix first) |
| --- | --- |
| `pdf` | `pdf` |
| `office_docs` | `doc`, `docx`, `odt`, `rtf`, `pages` |
| `presentations` | `ppt`, `pptx`, `odp`, `key` |
| `spreadsheets` | `xls`, `xlsx`, `xlsm`, `ods`, `numbers` |
| `text` | `txt`, `md`, `rst`, `tex` |
| `tabular_data` | `csv`, `tsv`, `parquet`, `arrow` |
| `structured_data` | `json`, `jsonl`, `xml`, `yaml`, `yml`, `toml` |
| `images` | `jpg`, `jpeg`, `png`, `gif`, `webp`, `heic`, `avif`, `tif`, `tiff`, `bmp` |
| `vector_graphics` | `svg`, `eps` |
| `ebooks` | `epub`, `mobi`, `azw`, `azw3`, `prc`, `djvu`, `djv` |
| `comics` | `cbz`, `cbr` |
| `audio` | `mp3`, `m4a`, `aac`, `flac`, `wav`, `ogg`, `opus`, `aiff` |
| `video` | `mp4`, `mkv`, `mov`, `avi`, `webm`, `m4v`, `mpeg` |
| `archives` | `tar.gz`, `tar.bz2`, `tar.xz`, `tar.zst`, `zip`, `rar`, `7z`, `tar`, `gz`, `bz2`, `xz`, `zst` |
| `source_code` | `py`, `go`, `js`, `ts`, `jsx`, `tsx`, `java`, `c`, `cpp`, `rs`, `sh`, `sql` |
| `web` | `html`, `htm`, `css`, `mhtml` |
| `databases` | `sqlite`, `sqlite3`, `db`, `mdb`, `accdb` |
| `personal_info` | `eml`, `msg`, `mbox`, `ics`, `vcf` |
| `subtitles` | `srt`, `vtt`, `ass` |
| `fonts` | `ttf`, `otf`, `woff`, `woff2` |
| `3d_cad` | `stl`, `obj`, `gltf`, `glb`, `step`, `stp`, `dwg`, `dxf` |
| `applications` | `exe`, `msi`, `apk`, `deb`, `rpm`, `appimage` |
| `other` | unmapped or missing suffix |

## Description and date normalization

`description` separates authoring tools from human authors.
Human authors stay in `description.authors`.
Tool names move to `description.software` (`application`, `creator_tool`, and `producer`).

Glassbox normalizes embedded `created_at` and `modified_at` timestamps without external dependencies:

- Full PDF (`D:...`) and ISO-8601 or W3CDTF timestamps normalize to RFC 3339.
- Valid partial dates (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`) stay verbatim with zero diagnostics.
- Malformed or out-of-range dates stay verbatim and emit an info diagnostic `date_unnormalized`.

## Structure and readiness

`structure.counts` maps plural unit names to integer totals.
Keys include `pages`, `sheets`, `slides`, `tables`, `chapters`, `paragraphs`, `blocks`, `lines`, `entries`, `rows`, `columns`, `previews`, `images`, `scalars`, `elements`, `records`, and `page_breaks`.
`structure.names` appears only when non-empty.
It groups names under `sheets`, `columns`, `entries`, `xml_paths`, `xml_namespaces`, `json_pointers`, `documents`, and `embedded`.
These entries contain schema names and namespace URIs only, never cell or body values.

- **`json_pointers`**: slash-delimited schema pointers using RFC 6901 token escaping (`~` as `~0`, `/` as `~1`). Each array step uses `*` as an element wildcard (for example, `/items/*/id` and `/a.b` versus `/a/b`).
- **`xml_paths` and `xml_namespaces`**: slash-delimited element paths using generated `ns0`, `ns1`, ... prefixes for namespaced tags. `structure.names.xml_namespaces` maps each prefix to its exact namespace URI when an XML document declares namespaces.

For PDFs, `structure.pdf` reports page count, page size in points, security revision, permissions, and `uniform_page_size`.
Glassbox sets `uniform_page_size` to `true` only after scanning every page and finding matching sizes.
It sets `false` when observed sizes differ, and omits the field when scanning stops early.
For raster images, `structure.image` reports `width_pixels` and `height_pixels` without inferring DPI.

`readiness` summarizes extraction signals in four fields:

| Field | Values |
| --- | --- |
| `extractable` | `supported`, `identification_only`, `unsupported` |
| `encrypted` | `none`, `accessible`, `locked`, `unknown` |
| `text_layer` | `present`, `mixed`, `absent`, `not_applicable`, `unknown` |
| `ocr` | `not_needed`, `recommended`, `required`, `unknown` |

Non-textual containers (`audio`, `video`, `archives`, `databases`, `fonts`, `3d_cad`, `applications`) report `text_layer: "not_applicable"` and `ocr: "not_needed"`.

## Digest and sampled fingerprint

The default full-hash threshold is 128 MiB (`134,217,728` bytes).

- **At or below 128 MiB, or with `--full-hash`**: Glassbox streams the whole file through a 64 KiB buffer. It emits `file.sha256` matching `sha256sum` and omits `file.fingerprint`.
- **Above 128 MiB without `--full-hash`**: Glassbox computes a sampled fingerprint and emits `file.fingerprint`. It omits `file.sha256` and adds an info diagnostic `fingerprint_sampled` with message `"change detection used size and modification time only."`.

The `glassbox-sampled-sha256/v1` scheme computes SHA-256 over this exact byte sequence:

1. ASCII `glassbox-sampled-sha256/v1` followed by one `0x00` byte.
2. File byte length as an unsigned 64-bit big-endian integer.
3. Three 1 MiB samples in order: head (`0`), middle (`floor((bytes - 1 MiB) / 2)`), and tail (`bytes - 1 MiB`).
4. Before each sample's bytes: its offset (`u64` big-endian) and sample length (`u64` big-endian).

Glassbox rechecks file size, modification time, and identity before and after reading.
If the source changes during hashing, Glassbox withholds both `file.sha256` and `file.fingerprint` and emits `source_changed_during_hash`.
These checks do not create an immutable snapshot against concurrent writes.

## Status and exit codes

| Status | Meaning | Exit code |
| --- | --- | ---: |
| `complete` | Inspection finished cleanly | 0 |
| `partial` | Some facts hit caps or were unavailable | 2 |
| `unsupported` | Format identified; no metadata reader applies | 2 |
| `failed` | Input or reader failure | 1 |
| Invalid invocation | Bad flags or arguments | 64 |

Identification-only formats still complete inspection cleanly.
Generic ZIP, CBZ, DjVu, and Parquet report `readiness.extractable: "identification_only"` and exit 0.

## Read a report

```sh
glassbox inspect /path/to/document.pdf --format text
```

JSON is the default output format.
Add `--pretty` for indented JSON, or `--verbose` to include reader `provenance`.
`--format text` prints a content-free summary starting with `Schema: 3`.
`--format markdown` prints a content-free summary starting with `# Glassbox inspection`.
Both summaries display `SHA-256` or `Fingerprint (glassbox-sampled-sha256/v1)` and omit empty sections.

## Thumbnails are opt-in

Ordinary inspection neither renders pages nor decodes image bodies.
Pass `--thumbnail PATH` to write one bounded thumbnail.
Use `--thumbnail-max-edge N` to set the long-edge cap in pixels (default 320, range 1–4096).
Thumbnail rendering never runs OCR or body extraction.

## Privacy

Inspect never outputs document bodies or row values.
Metadata alone can still reveal sensitive facts such as author names, file paths, or column names.
Handle inspection reports with the same care as the source files.

## Content flags are rejected

`glassbox inspect` rejects extraction flags: `--identify`, `--max-pages`, `--max-tokens`, `--max-chars`, `--probe-ocr`, `--no-ocr`, `--ocr-language`, `--packet`, and `--no-markers`.
It also rejects media extraction flags (`--imagegrid`, `--every-seconds`, `--quality`, `--performance`, `--imagegrid-format`, `--asr`, `--asr-*`, `--extract-audio`, `--no-frontmatter`); see [CLI migrations](cli.md#migrations).
Each rejection exits 64 and prints the equivalent `glassbox extract` command.
Place `--` before a path to end option parsing.
Use `glassbox inspect -- --no-markers` to inspect a file whose name starts with a flag.

## Where to go next

- [CLI reference and parsing](cli.md): complete flag tables and migrations.
- [Formats and limits](formats.md): format matrix and fixed caps.
- [Automation and directory inventories](automation.md): batch JSONL, directory scans, and saved artifacts.
- [Reports](reports.md): full v3 JSON examples and Rust API.
- [OCR and troubleshooting](ocr-and-troubleshooting.md): OCR behavior and common failures.

---
schema: "chai.metadata/v2"
id: "01M8A8KCH8CGBADTAZR9N6G7K9"
type: "documentation"
title: "Glassbox user documentation"
tags: ["glassbox", "documentation", "guides", "unified", "output", "authoritative", "prerequisites", "platforms", "parse", "commands"]
---
# Glassbox user documentation

Glassbox provides local parsing, metadata inspection, and content extraction.

Use these guides for the unified `glassbox` CLI.
The `--help` output of your build is the authoritative reference for that build.

- [Installation](installation.md) — build from source, prerequisites, platforms, and build verification.
- [CLI reference and parsing](cli.md) — downstream commands, flags, upstream `parse`, and migration notes.
- [Metadata inspection](inspect.md) — schema v3 inspection reports, 23 format types, sampled fingerprints, and thumbnails.
- [Content extraction](extract.md) — native-unit selection, `bodymatter:v1` Markdown markers, OCR, and caps.
- [Formats and limits](formats.md) — format families, support matrix, and fixed safety caps.
- [Automation and directory inventories](automation.md) — batch JSONL, directory scans, saved artifacts, CI, sync, and build cleanup.
- [Reports and programmatic use](reports.md) — schema v3 JSON contracts, Rust API, and migrations.
- [OCR and troubleshooting](ocr-and-troubleshooting.md) — language models, OCR behavior, and common failures.

Start with [Installation](installation.md), then read the [CLI reference](cli.md).
The downstream [README](../README.md) stays the quick entry point.

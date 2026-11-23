# Glassbox

> What's in the box?!

**Glassbox** is a local-first document triage, metadata-only inspection, and native-unit extraction fork built on [LiteParse](https://github.com/run-llama/liteparse).
It runs locally without cloud services.

## Start in `extensionalities/`

All Glassbox source code, documentation, and tests live under [`extensionalities/`](../extensionalities/README.md).
That isolation keeps weekly upstream LiteParse syncs clean and leaves the root upstream files untouched.

Read the [Glassbox workspace overview and quickstart](../extensionalities/README.md) to build from source and run your first inspection.

## Three jobs

| Job | Command | Result |
| --- | --- | --- |
| Parse documents like upstream LiteParse | `glassbox parse FILE [ARGS]` | Complete upstream behavior: commands, options, statuses, and files. |
| Triage a file without exposing content | `glassbox inspect PATH` | Identity, full streamed SHA-256, qualified counts, native signals, and embedded metadata. No body text, row values, or OCR output. |
| Take document content out | `glassbox extract PATH [--pages ...]` | Full document or selected native units as Markdown, text, or JSON. Local OCR runs only where needed. |

## Navigation

- [Glassbox workspace overview and quickstart](../extensionalities/README.md) — build instructions, quickstart command, and development checks.
- [User guides](../extensionalities/docs/README.md) — installation, CLI reference, metadata inspection, content extraction, formats, reports, automation, and troubleshooting.
- [Upstream LiteParse documentation](../README.md) — untouched upstream repository README.
- [License](../LICENSE) — Apache License 2.0.

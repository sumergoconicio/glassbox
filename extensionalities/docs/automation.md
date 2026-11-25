---
title: "Automation and directory inventories"
type: doc
content-type: Runbook
parent: docs/README.md
---

# Automation and directory inventories

Glassbox supports two scripted flows: JSONL batch mode and directory inventory.
Both stream ordered JSONL records to stdout and write diagnostics to stderr.
Both accept `--jobs N`, `--output-dir DIR`, `--full-hash`, and `--verbose`.

## JSONL batch mode

Pass `--batch` to `glassbox inspect` or `glassbox extract`.
Send one JSON request per stdin line:

```json
{"id": "lease-1", "path": "/data/contracts/lease.pdf"}
```

`id` is optional and may be null or duplicated; `path` must be a string.
One response line follows each request in request order, carrying a one-based `sequence`:

```json
{"id": "lease-1", "path": "/data/contracts/lease.pdf", "status": "complete", "report": {"schema_version": 3, "kind": "inspect"}, "sequence": 1}
```

The `report` object above is abbreviated; a real response carries the full schema v3 report.
In `extract --batch`, `--no-markers` also applies to saved `.md` artifacts under `--output-dir`.
Cancelled requests carry an `error` object instead of `report`:

```json
{"id": "lease-2", "path": "/data/contracts/lease.pdf", "status": "cancelled", "cancelled": true, "error": {"stage": "cancellation", "code": "request_not_started", "message": "batch cancelled before this request started"}, "sequence": 2}
```

File-reader failures return a failed v3 report with diagnostics.
Malformed lines return an invalid response:

```json
{"id": null, "status": "invalid", "error": {"stage": "batch", "message": "request requires a string path"}, "sequence": 3}
```

An invalid line keeps your `id` when valid JSON parsed; unparseable lines return `id: null`.
Invalid lines never stop later valid requests.
Lines exceeding 65,536 bytes are rejected as invalid and drained.
Batch mode rejects `--pretty`, `--format`, and `--thumbnail` with exit 64.

## Directory inventory

Pass a directory path to `glassbox inspect` to inventory its regular files:

```sh
"$GLASSBOX" inspect /data/archive
```

By default, this inspects immediate regular files only.
Add `--recursive` to descend into real subdirectories on the same mount.
Add `--full-hash` to stream full SHA-256 above 128 MiB, or `--verbose` to include reader provenance.
Output is ordered JSONL using the same envelope as batch mode, with the admission number as `id`:

```json
{"id": 1, "path": "/data/archive/notes.txt", "status": "complete", "report": {"schema_version": 3, "kind": "inspect"}, "sequence": 1}
```

The `report` object above is abbreviated.
Directory inspection rejects `--pretty`, `--thumbnail`, and non-JSON `--format` with exit 64.

Safety rules enforced by traversal:

- Symbolic links are never followed; each link emits a partial `symlink_skipped` record.
- Every entry stays on the root mount; different mounts (including bind mounts) emit partial `cross_mount_skipped` records.
- Unknown mount identity fails closed as a failed record.
- Network, FUSE, and unknown filesystem roots are rejected before traversal begins.
- Non-regular, non-link entries are skipped without records; subdirectories require `--recursive`.
- Non-UTF-8 paths emit a failed diagnostic record instead of a report.
- Linux and macOS support directory inventory and private artifact publication. Other platforms report these operations as unsupported.
- `--output-dir` must sit outside the scanned directory.

A skipped link or mount makes the aggregate exit 2 (`partial`).
A failed record makes the aggregate exit 1 (`failed`).
Traversal checks anchored file identity against inspection; a mismatch withholds any digest or fingerprint and downgrades the report.

## Save artifacts

Both modes accept `--output-dir DIR`.
Inspection saves a content-free Markdown summary (`.md`) and a schema v3 JSON report (`.json`) per file.
Extraction saves Markdown content (`.md`, honoring `--no-markers`) and a schema v3 JSON report (`.json`) per file.

Artifact stems combine a sanitized source basename, a six-digit sequence, and a hash token:

- `<12-char sha256 prefix>` when a consistent full `file.sha256` was computed.
- `fp-<12-char fingerprint prefix>` when a consistent sampled `file.fingerprint` was computed.
- `unhashed` when hashing failed, was unavailable, or detected a source change.

Files publish atomically with mode `0600` into a `0700` directory.
Existing files are never overwritten; collisions preserve old bytes and fail that record.

## Concurrency and backpressure

`--jobs N` defaults to 1 and accepts 1 to 32.
`N` is one shared budget across file work and local OCR, not a multiplier.
Admission waits when `N` requests are unfinished.
Responses print in request order using a bounded reorder buffer of 4 MiB per job.
The current head result always writes immediately, regardless of size.

`SIGINT` stops new admissions.
In-flight work finishes cleanly, unstarted requests emit `cancelled` records, and the process exits 1.
A closed stdout exits 1 without panicking.

## Exit codes

| Condition | Exit code |
| --- | ---: |
| Any invalid request or invocation | 64 |
| Any failed file operation or cancellation | 1 |
| Any partial or unsupported report, nothing failed | 2 |
| Everything complete | 0 |

Severity precedence is `64 > 1 > 2 > 0`.
Records print to stdout; diagnostics print to stderr.

## Continuous integration

The [Glassbox CI workflow](../../.github/workflows/glassbox-ci.yml) checks only the downstream Cargo workspace.
It builds upstream dependencies without running upstream tests or lints.
Checks cover:

- Formatting for `glassbox-cli` and `glassbox-inspect` without following upstream path dependencies.
- Workflow syntax and expressions using pinned `actionlint`.
- Clippy with warnings denied, full downstream workspace tests, and Python automation tests.
- Release builds and smoke checks: schema v3 inspection, extraction, batch requests, and offline OCR failure behavior.
- Linux x64, Linux ARM64, and macOS Apple Silicon (`macos-latest`) on standard GitHub-hosted runners.

Upstream dependency changes also trigger downstream checks.
Change detection skips heavy jobs for unrelated changes while keeping `Glassbox downstream gate` reporting cleanly.
Existing ignored Rust tests remain ignored; CI never replaces independent local audit clearance.

Run the same checks locally:

```sh
python3 -B extensionalities/scripts/ci.py format
python3 -B extensionalities/scripts/ci.py check
python3 -B extensionalities/scripts/ci.py workflows
```

Workflow validation additionally requires Go 1.24+ and access to public Go modules.
Workflow checks do not run upstream workflow files.

## Local build cleanup

Use the [local build recipe](installation.md#build) when you only need `glassbox`.
Full verification builds both `glassbox` and the compatibility alias binary `litx`.
Upstream parity checks compare `glassbox parse` against LiteParse's `lit`.

Routine builds share the repository's `target/` for Cargo intermediates. Spikes inside the checkout inherit this configuration.
Parity runs set both `CARGO_TARGET_DIR` and `CARGO_BUILD_BUILD_DIR` to their isolated run path. Use both variables when measuring a clean build. A custom target directory alone selects the final artifact location.
Run cleanup against the intended profile or isolated build path. Cargo can clean the shared build directory from any workspace that uses it.
Keep logs and results separate from disposable build trees.
Record exact disposable paths before starting and confirm approval before deleting non-build paths.
Check for stale build trees before a campaign, after a run ends, and when disk space is low.
Preserve current release binaries, active caches, fixtures, and audit evidence.
See the [agent guide](../AGENTS.md#local-build-cleanup) for full cleanup safety checks.

## Branch and merge policy

Develop and audit changes locally on a feature branch.
After publication approval, push that branch and open a PR targeting Glassbox's `main`.
GitHub Actions is the final external check, never a replacement for local audit.
Wait for every required check to pass, then request explicit permission to merge.
Passing checks alone never authorize merging a feature PR.

After approval, merge through GitHub without bypassing protection.
Fetch the remote commit and fast-forward local `main` to match.
Stop if local `main` diverged or has uncommitted edits.
Never push a local merge commit to protected `main`, and never delete branches without separate permission.
Weekly upstream-sync PRs are the approved automatic exception.

## Weekly upstream synchronization

The [sync workflow](../../.github/workflows/glassbox-upstream-sync.yml) runs Mondays at 03:17 UTC and supports manual dispatch.
It opens a sync PR and waits up to 40 minutes for every reported check.
The workflow merges only when every check succeeds.
Failed, skipped, neutral, or cancelled checks stop the merge, as do base or head changes.
Resolve any open sync PR before starting another sync.

Ownership boundaries preserved by sync:

- `extensionalities/` keeps its exact Git tree and lockfile.
- `.github/workflows/` keeps its exact Git tree.
- Root `.gitignore` mirrors upstream while preserving `/extensionalities/dev/`.
- All other tracked paths mirror `run-llama/liteparse`'s `main`, including upstream deletions.

Keep fork-specific work inside protected paths.
The automation never force-pushes, alters credentials, or edits downstream code.
A broken build or dependency change leaves the sync PR unmerged.

## One-time activation checklist

This checklist records the historical one-time repository setup for the committed configuration.
It is a record, not an instruction to repeat setup.
Do not infer live GitHub settings from this page; verify repository state directly.

1. Review local validation and approve cutover.
2. Disable inherited upstream workflows before publishing the setup PR.
3. Protect `main` and require `Glassbox downstream gate` from GitHub Actions.
4. Require PRs and up-to-date branches; enforce protection for administrators.
5. Block force pushes and branch deletion; require resolved conversations.
6. Preserve any other existing branch-protection rules.
7. Add the separately approved `UPSTREAM_SYNC_TOKEN` repository secret.
8. Publish the setup branch and PR, then verify Glassbox jobs on GitHub.
9. Wait for explicit permission before merging the setup PR.
10. After merging, verify the weekly schedule and fast-forward local `main`.

The sync token needs repository writes, PR writes, check reads, and branch-protection reads.
A classic token requires `repo` and `workflow` scopes.
Ordinary CI jobs never receive this token, and the sync job never executes newly fetched upstream code.
Use standard runners only; the workflow does not configure billing, paid runners, or package publishing.
The sync never enables GitHub auto-merge or deletes branches automatically.

## Drive Glassbox from Python

Run from the repository root using the standard library only (no shell invocation):

```python
import json
import os
import pathlib
import subprocess

repo = pathlib.Path.cwd()
glassbox = pathlib.Path(os.environ.get("GLASSBOX", repo / "extensionalities/target/release/glassbox"))
files = [repo / "extensionalities/fixtures/hello.txt"]

requests = "".join(
    json.dumps({"id": path.name, "path": str(path)}) + "\n"
    for path in files
)
result = subprocess.run(
    [str(glassbox), "extract", "--batch", "--no-ocr"],
    input=requests,
    text=True,
    capture_output=True,
)
for line in result.stdout.splitlines():
    response = json.loads(line)
    if response["status"] == "invalid":
        print("invalid:", response["error"]["message"])
        continue
    report = response.get("report")
    if report is None:
        print(response.get("id"), response["status"], response.get("error", {}).get("message", ""))
        continue
    print(response["id"], report["status"], report["metadata"]["format"]["type"])
print("aggregate exit:", result.returncode)
raise SystemExit(result.returncode)
```

Batch input encodes each path inside JSON, not positional arguments.
`capture_output=True` buffers every response in memory and suits bounded batches only.
For large streams, use `subprocess.Popen` and write one request at a time.
Read each response before sending more, and drain stderr on a background thread.
Inspect the process exit code for the aggregate outcome.

## Where to go next

- [CLI reference and parsing](cli.md) for all flags and migrations.
- [Reports](reports.md) for schema v3 report fields.
- [Metadata inspection](inspect.md) and [Content extraction](extract.md) for single-file workflows.
- [OCR and troubleshooting](ocr-and-troubleshooting.md) for failure handling.

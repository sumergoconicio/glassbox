---
title: "Automation and directory inventories"
type: doc
content-type: Runbook
parent: docs/README.md
---

# Automation and directory inventories

Glassbox supports two scripted flows: JSONL batch mode and directory inventory.
Both stream JSONL records to stdout and keep diagnostics on stderr.
Both accept `--jobs N` and `--output-dir DIR`.

## JSONL batch mode

Pass `--batch` to `inspect` or `extract`. Send one JSON request per stdin line:

```json
{"id": "lease-1", "path": "/data/contracts/lease.pdf"}
```

`id` is optional and may be null or duplicated. `path` must be a string.
One response line follows each request, in request order.
The emitter adds a one-based `sequence`, independent of your optional or duplicated `id`:

```json
{"id": "lease-1", "path": "/data/contracts/lease.pdf", "status": "complete", "report": {"schema_version": 2, "kind": "inspect"}, "sequence": 1}
```

The `report` object is abbreviated above; a real response carries the full v2 report.
Cancelled requests carry an `error` object instead of a `report`.

```json
{"id": "lease-2", "path": "/data/contracts/lease.pdf", "status": "cancelled", "cancelled": true, "error": {"stage": "cancellation", "code": "request_not_started", "message": "batch cancelled before this request started"}, "sequence": 2}
```

File-reader failures can return a failed v2 report with diagnostics.
Malformed requests return an invalid response:

```json
{"id": null, "status": "invalid", "error": {"stage": "batch", "message": "request requires a string path"}, "sequence": 3}
```

An invalid line keeps your `id` when the line parsed as JSON; unparseable lines return `id: null`.
Invalid lines never stop later valid work.
A line above 65536 bytes is rejected as invalid and drained.
Batch mode rejects `--pretty`, `--format`, and `--thumbnail` with exit 64.

## Directory inventory

Point `inspect` at a directory to enumerate its regular files:

```sh
"$GLASSBOX" inspect /data/archive
```

This lists immediate regular files only. Add `--recursive` to descend into subdirectories on the same mount.
Output is ordered JSONL with one record per file. Each record uses the same envelope as batch mode.
`--pretty` is rejected with exit 64; directory output is JSONL only.
The internal admission number serves as the record `id`. A record looks like:

```json
{"id": 1, "path": "/data/archive/notes.txt", "status": "complete", "report": {"schema_version": 2, "kind": "inspect"}, "sequence": 1}
```

The `report` object is abbreviated above.

Safety rules, enforced by the traversal:

- Symbolic links are never followed. Each link emits an explicit partial skip record.
- Every entry must stay on the root's mount. A different mount, including a same-device bind mount, emits a `cross_mount_skipped` partial record.
- Unknown mount identity fails closed as a failed record.
- Roots on network, FUSE, or unknown filesystem types are rejected before traversal.
- Non-regular, non-link entries are skipped without records. Subdirectories are visited only with `--recursive`.
- Non-Unicode paths produce a failed diagnostic record instead of a report.
- The inventory and artifact publication are Linux-only. Other platforms report the operation as unsupported.
- `--output-dir` must sit outside the scanned directory.

A skipped link or mount makes the aggregate partial (exit 2).
A failed record makes the aggregate failed (exit 1).
The traversal compares each file's anchored identity with the inspected file; a mismatch withholds the digest and downgrades the report.

## Save artifacts

Both modes accept `--output-dir DIR`.
Inspection saves a content-free summary and a JSON report per file.
Extraction saves Markdown content and a JSON report per file.
Names combine a sanitized source stem, a six-digit sequence, and a twelve-character digest prefix (`nohash` when the digest is untrusted).
Files are written atomically with mode 0600 into a 0700 directory.
Existing files are never overwritten; a collision preserves the old bytes and downgrades that record.

## Concurrency and backpressure

`--jobs N` defaults to 1 and accepts values from 1 to 32.
N is one shared budget for file work and OCR, not a multiplier.
Admission waits when N requests are unfinished.
Results print in request order.
Out-of-order results wait behind the current head within a bounded reorder buffer of 4 MiB per job.
The current head result always writes immediately, whatever its size.

A SIGINT stops new admissions.
Work in progress finishes, unfinished requests return `cancelled` records, and the process exits 1.
A closed stdout returns exit 1 without a panic.

## Exit codes

| Condition | Exit code |
| --- | ---: |
| Any invalid request or invocation | 64 |
| Any failed file operation or cancellation | 1 |
| Any partial or unsupported report, nothing failed | 2 |
| Everything complete | 0 |

Severity is ordered: 64 beats 1, which beats 2, which beats 0.
Records go to stdout; diagnostics go to stderr.

## Continuous integration

The [Glassbox CI workflow](../../.github/workflows/glassbox-ci.yml) checks only the downstream Cargo workspace.
It compiles upstream dependencies without running their tests or lint checks.
Checks cover:

- Formatting for `glassbox-cli` and `glassbox-inspect`, without following upstream path dependencies.
- Workflow syntax and expressions, using pinned actionlint.
- Clippy with warnings denied, plus the full downstream workspace tests and the Python automation tests.
- Release builds and release smoke checks: v2 inspection, extraction, batch requests, and offline OCR failure behavior.
- Linux x64 and ARM64 on standard GitHub-hosted runners.

Upstream dependency changes also trigger downstream checks.
Change detection skips expensive jobs for unrelated changes.
The stable `Glassbox downstream gate` job still reports a result, which avoids permanently pending required checks.
Existing ignored Rust tests remain ignored; CI does not provide independent audit clearance.

Run the same checks locally:

```sh
python3 -B extensionalities/scripts/ci.py format
python3 -B extensionalities/scripts/ci.py check
python3 -B extensionalities/scripts/ci.py workflows
```

Workflow validation additionally requires Go 1.24+ and access to public Go modules.
The workflow checks do not run upstream workflow files.

## Local build cleanup

Use the [local build recipe](installation.md#build) when you only need `glassbox`.
The full verification workflow still builds `litx` to check compatibility.
Upstream parity compares Glassbox commands with LiteParse's `lit`.

Parity runs create isolated Cargo build trees. Keep their logs and results separate from the build trees.
Before starting a run, record the exact disposable paths and get approval to remove them after the run.
Check for stale paths before a campaign, after a run ends or is cancelled, and when disk space is low.
Clean approved paths after their jobs exit. For other paths, get approval before removal.
Keep the current release, shared caches in use, fixtures, and audit evidence.
See the [agent guide](../AGENTS.md#local-build-cleanup) for the safety checks.

## Branch and merge policy

Develop and audit changes locally on a feature branch.
After publication approval, push that branch and open a PR targeting Glassbox's `main`.
GitHub Actions provides the final external check; it does not replace the local audit.
Wait for every applicable check to succeed.
Then request explicit permission to merge that PR.
Passing checks alone never authorize a feature PR merge.

After permission, merge the checked head through GitHub without bypassing protection.
Fetch the resulting remote commit and fast-forward local `main` to match.
Stop if local `main` has diverged or contains changes.
Do not create a separate local merge commit and push it to protected `main`.
Do not delete the feature branch without separate permission.
Weekly upstream-sync PRs are the approved automatic exception.

## Weekly upstream synchronization

The [sync workflow](../../.github/workflows/glassbox-upstream-sync.yml) runs Mondays at 03:17 UTC.
It also supports manual dispatch.
The workflow creates a sync PR, waits for every reported check, and merges only when all succeed.
Pending or missing checks wait up to 40 minutes.
Failed, skipped, neutral, or cancelled checks stop the merge.
Base or head changes also stop the merge.
Existing open sync PRs require resolution before another sync starts.

The sync uses a precise ownership boundary:

- `extensionalities/` retains its exact Git tree, including its lockfile.
- `.github/workflows/` retains its exact Git tree.
- The root ignore file follows upstream, with `/extensionalities/dev/` preserved.
- All other tracked paths mirror `run-llama/liteparse`'s `main`, including upstream deletions.

Keep fork-specific changes within the protected paths.
The sync does not preserve other root-level customizations.
Its merge commit records both the current Glassbox and upstream commits as parents.
Planning never changes working files, the normal Git index, or branch references.
The automation never force-pushes, changes credentials, or rewrites downstream code to repair compatibility.
A failed build or dependency change leaves the PR unmerged.

## One-time activation checklist

This checklist records the one-time repository setup that the committed source configuration was built for.
This is a historical record, not an instruction to repeat the setup.
Each step needed separate approval. This page does not activate or change a live repository.
Do not infer the current live GitHub settings from this page; verify repository state directly before relying on it.

1. Review local validation and approve the cutover.
2. Disable the inherited upstream workflows in Glassbox before publishing the setup PR.
3. Protect `main` and require `Glassbox downstream gate` from GitHub Actions.
4. Require PRs and up-to-date branches; enforce protection for administrators.
5. Block force pushes and deletion; require resolved conversations.
6. Preserve any other existing protection requirements.
7. Add the separately approved `UPSTREAM_SYNC_TOKEN` repository secret.
8. Publish the setup branch and PR, then verify the Glassbox jobs on GitHub.
9. Wait for explicit permission before merging the setup PR.
10. After merging, verify the weekly schedule and synchronize local `main`.

The sync token needs repository and PR writes, check reads, and branch-protection read access.
Use `repo` and `workflow` scopes for a classic sync token, subject to your account permissions.
The ordinary CI jobs never receive this token.
The sync job never executes newly fetched upstream code.
Use a personal or app token so new PRs can trigger CI.
Copying a global token into a repository secret needs explicit approval.

The workflow does not configure billing, paid runners, package publishing, or spending limits.
Standard runners do not replace account-level spending controls.
Use only standard runners for this public repository.
The sync does not approve PRs, bypass protections, or enable GitHub's native auto-merge setting.
It requests a normal, head-pinned merge only after every check succeeds.
After a timeout or interruption, inspect the existing PR before closing or merging it.
Branches and PRs are never automatically deleted.

## Drive Glassbox from Python

Run from the repository root. Export `GLASSBOX` from the installation recipe first.
Use the standard library only. Do not invoke a shell:

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
    print(response["id"], report["metadata"]["status"])
print("aggregate exit:", result.returncode)
raise SystemExit(result.returncode)
```

The batch input is JSON, not positional arguments; each path is encoded inside its request line.
`capture_output=True` collects every response in memory before the program continues.
It suits bounded batches, not unbounded streams.
For large batches, use `Popen` and write one request line at a time.
Read each response before writing more. Drain stderr in a separate thread so no pipe fills.
Check the process exit code for the aggregate outcome.

## Where to go next

- [CLI reference and parsing](cli.md) for flags and migrations.
- [Reports](reports.md) for the response schema.
- [Metadata inspection](inspect.md) and [content extraction](extract.md) for single-file use.
- [OCR and troubleshooting](ocr-and-troubleshooting.md) for failure handling.

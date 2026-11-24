# Extensionalities agent guide

## Isolation contract (downstream mini-repo seam)

Treat `extensionalities/` as a standalone downstream mini-repo inside `liteparse-extended` so upstream `liteparse` syncs and merges stay clean:

- Keep all fork-specific source, manifests, lockfile, fixtures, development records (`dev/` and `dev/PRDs/`), and test/audit scratch (`sandbox/` and `target/`) inside `extensionalities/`. The shared Cargo intermediate directory is the repository's ignored `../target/`.
- Keep `dev/`, `PRDs/`, `sandbox/`, and downstream final artifacts inside `extensionalities/`.
- This is an independent Cargo workspace. Do not register its crates in the upstream root workspace.
- Use the upstream `liteparse` public library through the path dependency. Do not patch its CLI or other upstream-owned files.
- Build and check with the explicit `extensionalities/Cargo.toml` manifest; commit this workspace's `Cargo.lock`.
- Keep built-in Tesseract enabled. Do not disable it to make a build pass.
- `glassbox` provides upstream parsing, metadata-only inspection, and extraction. `litx` remains a compatibility alias binary for the same implementation.
- The Rust packages are `glassbox-cli` and `glassbox-inspect`. The crate directories stay `crates/litx` and `crates/litx-inspect`.
- Neither command replaces upstream `lit`. Keep `litx` out of public command examples; migration text may name it.
- Further features and consumer delivery require separate approval.
- Preserve the root workspace and lockfile during dependency updates. Public API changes can break this workspace even when Git merges cleanly.
- Keep build products, sandbox files, credentials, and private documents out of commits. Review fixtures before adding them.
- Do not push, publish, activate delivery bundles, or modify consumer recipes without explicit authorization.

## Documentation, sandbox, and entrypoints

- Keep non-code development documentation under [`dev/`](dev/README.md), not beside the Cargo workspace files. This `AGENTS.md` remains at the downstream root as the agent entrypoint.
- Public user documentation lives under [`docs/`](docs/README.md). Development records stay under `dev/`.
- [`README.md`](README.md) is the public Glassbox guide. Keep `../README.md` unchanged as upstream documentation.
- Current Glassbox behavior follows the v3 contracts in [`docs/reports.md`](docs/reports.md) and [`20261123_inspect-extract-outputs`](dev/PRDs/20261123_inspect-extract-outputs/spec.md). Inspection is metadata-only; extraction selects native units and emits `bodymatter:v1` Markdown markers. Historical v2 contracts remain in [`dev/docs/settling-in-interfaces.md`](dev/docs/settling-in-interfaces.md).
- Group each feature's phases in its own `dev/PRDs/YYYYMMDD_<feature-slug>/` folder. Prior work covers [`20261117_litx-inspect-extract`](dev/PRDs/20261117_litx-inspect-extract/README.md), [`20261118_xml-structure-profile`](dev/PRDs/20261118_xml-structure-profile/README.md), and v2 unification [`20261121_settling_in`](dev/PRDs/20261121_settling_in/spec.md); the current v3 inspect/extract outputs work is [`20261123_inspect-extract-outputs`](dev/PRDs/20261123_inspect-extract-outputs/spec.md).
- Keep temporary audit harnesses, dogfood probes, and scratch test fixtures inside [`sandbox/`](sandbox/) (gitignored by `extensionalities/.gitignore`), not in the upstream root or `/tmp`. Clean temporary scratch inputs after runs.
- Keep code, Cargo manifests/lockfile, scripts, and executable test fixtures in their existing locations. Text fixtures such as `fixtures/hello.txt` are test data, not development documentation.
- Preserve historical reports and their approvals, limitations, verification claims, and receipts. Moving a report does not make its historical checks current or grant audit clearance.
- Before editing this guide, preserve a dated recoverable original under `dev/instruction-history/` and record its SHA-256. Prior versions: [`20261118-AGENTS.before-prd-routing.md`](dev/instruction-history/20261118-AGENTS.before-prd-routing.md) (`7aec3d6677b48b1ce0e00eea2deb7b3ccd19d5e46fe448f05b0f42f61a89dc56`) and [`20261118-AGENTS.before-downstream-seam.md`](dev/instruction-history/20261118-AGENTS.before-downstream-seam.md) (`8adb95c42350b17cf232029b377d8b1cf7f901c15f04dd603f92f83c0d310e27`).

## Local build cleanup

- Use the shared Cargo intermediate directory in `../target/` for routine upstream, downstream, and spike builds. Cargo keeps final artifacts in each workspace's target directory. For a local Glassbox-only build, use `--bin glassbox`.
- Set both `CARGO_TARGET_DIR` and `CARGO_BUILD_BUILD_DIR` to the isolated path for clean-build measurements and verification that requires isolation.
- For isolated runs, record the exact disposable paths under `target/` or `sandbox/` before starting. Keep logs, results, and audit evidence separate from disposable build trees.
- Check for stale build trees before a new campaign, after a run ends or stops, and under disk pressure. Sri grants standing approval to clean verified stale Cargo build trees under `target/` without asking again. For other paths, list exact targets and obtain approval before removal. No scheduler is required.
- Before cleanup, confirm each path is local, inside this workspace, ignored by Git, and free of active builds, symlinked path components, and nested mounts. Preserve current release binaries, shared caches in use, fixtures, and audit evidence. Do not treat arbitrary old or ignored files as stale build trees.
- Remove only verified stale build trees or separately approved paths. Verify the retained outputs and report the net change in available disk space.

The guide before build cleanup is preserved in [`20261123-AGENTS.before-build-cleanup.md`](dev/instruction-history/20261123-AGENTS.before-build-cleanup.md).
Its SHA-256 is `8fe3daf15467ac995a4ea451e9e8cfd5f394da820e81dd49ac899ef653191f0e`.

The guide before standing stale-build cleanup is preserved in [`20261123-AGENTS.before-auto-build-cleanup.md`](dev/instruction-history/20261123-AGENTS.before-auto-build-cleanup.md).
Its SHA-256 is `4e01c9650a7f030c0055e219b3b7de572abbb278581d679c587d1730f3c51d75`.

## Shared PRD management

The pre-branding guide is preserved in [`20261119-AGENTS.before-glassbox-branding.md`](dev/instruction-history/20261119-AGENTS.before-glassbox-branding.md).
Its SHA-256 is `b0bafe0f8712a4e648c7cb6ba5bd9805344754c74403e012d6cfd6449c2a9867`.

The guide before README relocation is preserved in [`20261120-AGENTS.before-readme-isolation.md`](dev/instruction-history/20261120-AGENTS.before-readme-isolation.md).
Its SHA-256 is `86f35f33e121e364f53651a50a988c1ac7c53fd86f40553cd29a0d7ca8d60fa9`.

Use the same shared lifecycle as `~/chai/agents/coding/`; do not copy the manager or agent definitions into this repository.

Read these shared contracts when producing or receiving feature records:

- `~/chai/agents/meta/contracts/handoff-contract.md`
- `~/chai/agents/meta/contracts/prd-notes.md`
- `~/chai/agents/meta/contracts/prd-vocabulary.md`
- For audits, also `~/chai/agents/meta/contracts/audit-return-contract.md`.

Resolve `~/chai` on the current host. Run `python3 -B "$HOME/chai/agents/scripts/manage_PRDs.py" --folder <absolute-feature-folder> --route` at intake and before handoff. Follow the returned paths instead of guessing the active stage from phase numbers or timestamps.

| Owner | Lifecycle record and gate |
| --- | --- |
| `master-explore` | Agree the feature folder name, then `scaffold intent`. Keep unpromoted ideas as folderless entries in `dev/PRDs/IDEAS.md` when needed. |
| `master-refine` | `scaffold design`; own numbered ADRs under `decisions/`. Cite the source intent and decisions. |
| `master-plan` | `scaffold spec` and `scaffold milestones`; own the verification contract, task DAG, and routing clarity. |
| `master-drive` / implementation workers | Follow the approved spec and `milestones.json`; require both recorded approval and `execution_ready: true` before new planned implementation. |
| `master-audit` | Independently audit in another thread. Use milestone-scoped `scaffold audit`, preserve review waves/remediation and snapshot evidence, then `record-audit` with reconciliation/verdict outputs. |
| `worker-record` | Reconcile documentation and evidence; archive the entire feature under `dev/documentary/` only after the required audit and release gates. |

Use the manager's `scaffold`, `link`, and `record-audit` commands to maintain managed frontmatter and relationships. New stage records remain `draft` until approval is recorded in their Handoff; do not infer approval from implementation, a passing test run, or a phase report.

After changing managed records, run `--write-index --route` for that feature. The manager owns only its marked section in the feature's generated `AGENTS.md`; preserve surrounding instructions. Atlas Indexer is required by the shared manager. Report missing tools rather than inventing an index or bypassing checks.

The migrated inspection/extraction reports are historical evidence linked from a retrospective **draft** intent. No historical design/spec approval, task completion, or independent audit verdict has been fabricated. The pending read-only audit may proceed under its explicit user brief; the migration does not authorize new implementation.

The guide before the shared Cargo build directory is preserved in [`20261124-AGENTS.before-shared-cargo-build-dir.md`](dev/instruction-history/20261124-AGENTS.before-shared-cargo-build-dir.md).
Its SHA-256 is `86b0938998358314e21a3f21a0f06dc1086661c0cbab4695e3eff9f42340ff45`.

## Upstream-sync ignore guard

User-authorized upstream-file exceptions cover the ignore rule, the shared Cargo config, and the GitHub entry files:

- `../.gitignore`: preserve `/extensionalities/dev/`.
- `../.cargo/config.toml`: share Cargo intermediates for routine builds.
- `../.github/README.md`: Glassbox GitHub landing page.
- `../.github/workflows/glassbox-ci.yml`: downstream checks.
- `../.github/workflows/glassbox-upstream-sync.yml`: checked upstream synchronization.

Keep workflow implementation and tests under `scripts/`; keep the root README unchanged.
The sync preserves exact downstream and workflow trees and the protected root files. It mirrors upstream-owned paths elsewhere.
Automatic merging requires every reported check to succeed and enforced, strict required checks on `main`.
Publication, live activation, credential installation, and billing changes require separate authorization.

The pre-automation guide is preserved in [`20261121-AGENTS.before-ci-automation.md`](dev/instruction-history/20261121-AGENTS.before-ci-automation.md).
Its SHA-256 is `7e7b23fea299cb8df936dfd96f66a87efc3755c0af8e96b74f2a9a646f0d005a`.

The guide before the GitHub landing page is preserved in [`20261123-AGENTS.before-github-readme.md`](dev/instruction-history/20261123-AGENTS.before-github-readme.md).
Its SHA-256 is `d7da2b7cd13da0d0e235b0c35509294431858658786c6d2bfcfb57372928f266`.

- Check that exact rule after upstream pulls, merges, rebases, or other syncs, and before staging development records.
- If a sync removes it, restore only that rule while preserving the other upstream ignore entries.
- From the repository root, verify with `git check-ignore --no-index -v extensionalities/dev/README.md`. The matching rule must be `/extensionalities/dev/` in the root `.gitignore`.
- Ignoring a directory does not untrack existing files. Do not use `git rm --cached`, force-add ignored records, commit, or alter the index without explicit authorization.
- Keep private source documents, raw extraction reports, binaries, caches, and generated test evidence under ignored `target/`, not in PRDs.

## Branch, PR, and merge approval

Work on feature branches and complete local audits before opening a PR to Glassbox's `main`.
GitHub checks are a final external check, not independent audit clearance.
Run downstream formatting, Clippy, tests, release smoke checks, and Glassbox workflow validation.
Do not run the inherited upstream test matrices.

Feature PRs, including the CI setup PR, require Sri's explicit merge instruction after checks pass.
Green checks never supply that permission.
Weekly upstream-sync PRs are the approved all-green automatic exception.
Never bypass protection, enable auto-merge for feature PRs, or push directly to `main`.
After an approved GitHub merge, fetch and fast-forward local `main` to the exact remote commit.
Preserve the current branch and dirty work; stop on divergence instead of resetting or creating another merge.
Do not delete branches without separate permission.

The guide before PR activation is preserved in [`20261121-AGENTS.before-pr-activation.md`](dev/instruction-history/20261121-AGENTS.before-pr-activation.md).
Its SHA-256 is `603911a38e2cb8cebe49b4274f4b73eb126777cd9f87b0f2b3364f0442f97ebf`.

The guide before settling-in documentation is preserved in [`20261122-AGENTS.before-settling-in.md`](dev/instruction-history/20261122-AGENTS.before-settling-in.md).
Its SHA-256 is `92ae9c00167b064b99278d17b7e2bbe2d644719303090b628a292bcd7e1d54be`.

The guide before v3 documentation parity is preserved in [`20261124-AGENTS.before-v3-doc-parity.md`](dev/instruction-history/20261124-AGENTS.before-v3-doc-parity.md).
Its SHA-256 is `18efdee80e0d4b0bd368bd93735da2c880c0b530cc78e6755c1dc8e37bef30cc`.

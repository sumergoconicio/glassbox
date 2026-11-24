"""Module: Glassbox downstream automation tests.
File: test_automation.py.
Responsibility: Verify isolated sync, workflow, package-selection, smoke, and automation check gates.

Isolated automation tests. Retain synthetic repositories under ignored target."""

import importlib.util
import itertools
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
TARGET = SCRIPTS.parent / "target"
TARGET.mkdir(exist_ok=True)


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ci = load("ci")
sync = load("upstream_sync")


def protection():
    return {
        "required_status_checks": {"strict": True, "contexts": [sync.GATE]},
        "enforce_admins": {"enabled": True},
    }


def green():
    return [
        {"name": sync.GATE, "workflow": sync.WORKFLOW, "state": "SUCCESS"},
        {"name": "checks (ubuntu-24.04)", "state": "SUCCESS"},
    ]


class TreeTests(unittest.TestCase):
    def setUp(self):
        """Create an isolated synthetic Git repository with base protected trees and files.

        Accepts only self (TreeTests).
        Returns None after populating self.root, self.files, and self.base.
        Raises subprocess.CalledProcessError or sync.SyncError if Git fixture creation fails.
        """
        self.root = Path(tempfile.mkdtemp(prefix="test-sync-", dir=TARGET))
        subprocess.run(["git", "init", "--quiet", str(self.root)], check=True)
        self.counter = 0
        self.files = {
            ".gitignore": b"target/\n/extensionalities/dev/\n",
            ".github/README.md": b"# Glassbox\n",
            ".cargo/config.toml": b'[build]\nbuild-dir = "target"\n',
            ".github/workflows/glassbox-ci.yml": b"ours\n",
            "extensionalities/Cargo.toml": b"[workspace]\n",
            "extensionalities/README.md": b"downstream\n",
            "README.md": b"upstream old\n",
            "old-only.txt": b"remove with upstream\n",
        }
        self.base = self.commit(self.files)

    def commit(self, files, parent=None):
        records = []
        for path, value in sorted(files.items()):
            mode, content = value if isinstance(value, tuple) else ("100644", value)
            blob = sync.git(self.root, "hash-object", "-w", "--stdin", data=content).strip()
            records.append(mode.encode() + b" blob " + blob + b"\t" + path.encode() + b"\0")
        self.counter += 1
        env = {**os.environ, "GIT_INDEX_FILE": str(self.root / f"fixture-index-{self.counter}")}
        sync.git(self.root, "update-index", "-z", "--index-info", data=b"".join(records), env=env)
        tree = sync.git(self.root, "write-tree", env=env).strip().decode()
        # Synthetic Git fixture data, not a configured author identity.
        body = f"tree {tree}\n"
        if parent:
            body += f"parent {parent}\n"
        body += (
            "author Fixture <fixture@example.invalid> 1 +0000\n"
            "committer Fixture <fixture@example.invalid> 1 +0000\n\nfixture\n"
        )
        return sync.git(self.root, "hash-object", "-t", "commit", "-w", "--stdin",
                        data=body.encode()).strip().decode()

    def upstream(self, **changes):
        files = dict(self.files)
        files.pop("old-only.txt")
        files.update({"README.md": b"new upstream\n", ".gitignore": b"new-ignore/\n"})
        files.update(changes)
        return self.commit(files, self.base)

    def test_exact_overlay_including_hostile_protected_changes_and_deletions(self):
        """Verify sync planning preserves all protected trees and files despite hostile upstream edits.

        Accepts only self (TreeTests).
        Returns None after asserting exact overlay OIDs, upstream mirroring, and index isolation.
        Raises AssertionError or sync.SyncError if any protected path is altered or working files change.
        """
        upstream = self.upstream(**{
            ".github/README.md": b"must not overwrite landing page\n",
            ".cargo/config.toml": b'[build]\nbuild-dir = "upstream-target"\n',
            "extensionalities/README.md": b"must not win\n",
            "extensionalities/new.rs": b"must not be imported\n",
            ".github/workflows/new-upstream.yml": b"must not run\n",
            "new.rs": b"upstream code\n",
        })
        result = sync.plan(self.root, self.base, upstream)
        for path in (*sync.PROTECTED, *sync.PROTECTED_FILES):
            self.assertEqual(sync.oid(self.root, f"{result['tree']}:{path}"),
                             sync.oid(self.root, f"{self.base}:{path}"))
        entries = sync.tree_entries(self.root, result["tree"])
        self.assertNotIn(b"old-only.txt", entries)
        self.assertIn(b"new.rs", entries)
        self.assertEqual(sync.git(self.root, "show", result["tree"] + ":README.md"), b"new upstream\n")
        self.assertEqual(sync.git(self.root, "show", result["tree"] + ":.gitignore"),
                         b"new-ignore/\n/extensionalities/dev/\n")
        self.assertFalse((self.root / ".git/index").exists())
        self.assertFalse((self.root / "README.md").exists())

    def test_idempotent_and_no_downstream_lockfile_rewrite(self):
        result = sync.plan(self.root, self.base, self.base)
        self.assertFalse(result["changed"])
        self.assertEqual(result["tree"], sync.oid(self.root, self.base + "^{tree}"))

    def test_plan_cleans_up_temporary_index_directory_on_success_and_failure(self):
        """Verify sync.plan removes its temporary sync-plan-* index directory on both success and failure.

        Accepts only self (TreeTests).
        Returns None after asserting zero sync-plan-* directories remain under extensionalities/target
        following both a successful plan and a mid-index failure.
        Raises AssertionError if any temporary sync-plan-* directory leaks.
        """
        scratch = self.root / "extensionalities/target"
        upstream = self.upstream()

        sync.plan(self.root, self.base, upstream)
        self.assertEqual(list(scratch.glob("sync-plan-*")), [])

        real_git = sync.git

        def fail_on_write_tree(root, *args, **kwargs):
            if args and args[0] == "write-tree":
                raise sync.SyncError("simulated write-tree failure")
            return real_git(root, *args, **kwargs)

        with patch.object(sync, "git", side_effect=fail_on_write_tree):
            with self.assertRaisesRegex(sync.SyncError, "simulated write-tree failure"):
                sync.plan(self.root, self.base, upstream)
        self.assertEqual(list(scratch.glob("sync-plan-*")), [])

    def test_change_detection_handles_renames_out_of_the_downstream_tree(self):
        """Verify change detection triggers on moved code manifests and skips documentation-only edits.

        Accepts only self (TreeTests).
        Returns None after checking rename detection and non-code documentation commits.
        Raises AssertionError or sync.SyncError if Git fixtures fail or relevance classification regresses.
        """
        files = dict(self.files)
        files["docs/moved-Cargo.toml"] = files.pop("extensionalities/Cargo.toml")
        head = self.commit(files, self.base)
        self.assertTrue(ci.needs_checks(self.root, self.base, head))
        for doc_path in (
            "README.md",
            ".github/README.md",
            "extensionalities/AGENTS.md",
            "extensionalities/docs/automation.md",
        ):
            with self.subTest(doc_path=doc_path):
                doc_files = {**self.files, doc_path: b"documentation update\n"}
                doc_head = self.commit(doc_files, self.base)
                self.assertFalse(ci.needs_checks(self.root, self.base, doc_head))

    def test_upstream_protected_root_symlink_cannot_replace_downstream(self):
        files = {p: c for p, c in self.files.items() if not p.startswith("extensionalities/")}
        files["extensionalities"] = ("120000", b"/outside")
        upstream = self.commit(files, self.base)
        result = sync.plan(self.root, self.base, upstream)
        self.assertEqual(sync.oid(self.root, result["tree"] + ":extensionalities"),
                         sync.oid(self.root, self.base + ":extensionalities"))

    def test_upstream_protected_file_symlink_or_directory_cannot_replace_downstream(self):
        """Verify hostile upstream symlinks or directories cannot replace protected files and base symlinks fail.

        Accepts only self (TreeTests).
        Returns None after testing upstream symlink replacement, nested directory leakage, and base symlink rejection.
        Raises AssertionError or sync.SyncError if protected file integrity is violated.
        """
        with self.subTest(case="upstream-symlink"):
            files = dict(self.files)
            files[".github/README.md"] = ("120000", b"/outside")
            upstream = self.commit(files, self.base)
            result = sync.plan(self.root, self.base, upstream)
            self.assertEqual(sync.oid(self.root, result["tree"] + ":.github/README.md"),
                             sync.oid(self.root, self.base + ":.github/README.md"))
            entries = sync.tree_entries(self.root, result["tree"])
            self.assertTrue(entries[b".github/README.md"].startswith(b"100644 blob "))
        with self.subTest(case="upstream-directory"):
            files = {p: c for p, c in self.files.items() if p != ".github/README.md"}
            files[".github/README.md/nested.md"] = b"hostile nested file\n"
            upstream = self.commit(files, self.base)
            result = sync.plan(self.root, self.base, upstream)
            self.assertEqual(sync.oid(self.root, result["tree"] + ":.github/README.md"),
                             sync.oid(self.root, self.base + ":.github/README.md"))
            entries = sync.tree_entries(self.root, result["tree"])
            self.assertNotIn(b".github/README.md/nested.md", entries)
        with self.subTest(case="base-symlink-rejected"):
            bad_base_files = {**self.files, ".github/README.md": ("120000", b"/outside")}
            bad_base = self.commit(bad_base_files)
            with self.assertRaises(sync.SyncError):
                sync.plan(self.root, bad_base, self.upstream())

    def test_unsafe_ignore_or_github_shape_is_rejected(self):
        for path in [".gitignore", ".github"]:
            with self.subTest(path=path):
                files = {p: c for p, c in self.files.items()
                         if p != path and not p.startswith(path + "/")}
                files[path] = ("120000", b"/outside")
                upstream = self.commit(files, self.base)
                with self.assertRaises(sync.SyncError):
                    sync.plan(self.root, self.base, upstream)

    def test_upstream_deletions_of_protected_trees_are_ignored(self):
        """Verify upstream deletion of protected trees and files does not remove downstream overlays.

        Accepts only self (TreeTests).
        Returns None after asserting extensionalities, .github/README.md, and ignore guard survive.
        Raises AssertionError or sync.SyncError if any protected path is dropped.
        """
        upstream = self.commit({"README.md": b"upstream only\n"}, self.base)
        result = sync.plan(self.root, self.base, upstream)
        self.assertEqual(sync.oid(self.root, result["tree"] + ":extensionalities"),
                         sync.oid(self.root, self.base + ":extensionalities"))
        self.assertEqual(sync.oid(self.root, result["tree"] + ":.github/README.md"),
                         sync.oid(self.root, self.base + ":.github/README.md"))
        self.assertEqual(result["overlays"][-1]["content"], "/extensionalities/dev/\n")

    def test_ignore_guard_wins_over_later_upstream_negation(self):
        upstream = self.upstream(**{
            ".gitignore": b"/extensionalities/dev/\n!/extensionalities/dev/\n",
        })
        result = sync.plan(self.root, self.base, upstream)
        (self.root / ".gitignore").write_text(result["overlays"][-1]["content"])
        output = sync.git(self.root, "check-ignore", "--no-index", "-v",
                          "extensionalities/dev/README.md")
        self.assertTrue(output.split(b"\t")[0].endswith(b":/extensionalities/dev/"))

    def test_publisher_uses_verified_tree_and_does_not_override_identity(self):
        """Verify fresh sync publication probes the ref (404), publishes verified objects, and opens a PR.

        Accepts only self (TreeTests).
        Returns None after asserting 4 POST calls and default GitHub commit identity.
        Raises AssertionError or sync.SyncError if tree overlays, parents, or ref probing regress.
        """
        upstream = self.upstream()
        candidate = sync.plan(self.root, self.base, upstream)
        calls = []

        def api(repository, path="", payload=None, method="GET", paginate=False):
            calls.append((path, payload, method))
            if path == "branches/main/protection":
                return protection()
            if path == "":
                return {"default_branch": "main", "allow_merge_commit": True}
            if path.startswith("pulls?"):
                return [[]]
            if path == "branches/main":
                return {"commit": {"sha": self.base}}
            if path.startswith("git/ref/"):
                raise sync.NotFoundError(f"GitHub resource not found (GET {path})")
            if path == "git/trees":
                self.assertEqual(payload["tree"], candidate["overlays"])
                return {"sha": candidate["tree"]}
            if path == "git/commits":
                self.assertEqual(payload["parents"], [self.base, upstream])
                self.assertNotIn("author", payload)
                self.assertNotIn("committer", payload)
                return {"sha": "c" * 40}
            if path == "pulls":
                return {"number": 7, "html_url": "https://github.com/example/repository/pull/7"}
            return {}

        with patch.dict(os.environ, {
            "GITHUB_ACTIONS": "true", "GITHUB_REPOSITORY": sync.REPOSITORY,
            "GH_TOKEN": "synthetic-test-credential",
        }), patch.object(sync, "api", side_effect=api), patch.object(sync, "merge_when_green") as merge:
            sync.automate(self.root, self.base, upstream)
            merge.assert_called_once()
        self.assertEqual(sum(method == "POST" for _, _, method in calls), 4)

    def test_retry_after_partial_publication_reconciles_verified_ref_and_blocks_mismatch(self):
        """Verify retry after partial publication reuses verified ref and fails closed on tampering.

        Accepts only self (TreeTests).
        Returns None after testing:
          1. Matching existing ref -> skips POST git/trees|commits|refs, creates PR, and merges.
          2. Tampered tree SHA on existing ref -> fails closed with SyncError and zero POSTs.
          3. Mismatched parents on existing ref -> fails closed with SyncError and zero POSTs.
        Raises AssertionError if retry fails to recover or accepts a tampered existing branch.
        """
        upstream = self.upstream()
        candidate = sync.plan(self.root, self.base, upstream)
        branch = f"automation/upstream-sync/{candidate['upstream'][:12]}-{candidate['base'][:12]}"
        existing_sha = "d" * 40
        env = {
            "GITHUB_ACTIONS": "true",
            "GITHUB_REPOSITORY": sync.REPOSITORY,
            "GH_TOKEN": "synthetic-test-credential",
        }

        with self.subTest(case="matching-existing-ref-recovers-and-creates-pr"):
            calls = []

            def api_match(repository, path="", payload=None, method="GET", paginate=False):
                calls.append((path, payload, method))
                if path == "branches/main/protection":
                    return protection()
                if path == "":
                    return {"default_branch": "main", "allow_merge_commit": True}
                if path.startswith("pulls?"):
                    return [[]]
                if path == "branches/main":
                    return {"commit": {"sha": self.base}}
                if path == f"git/ref/heads/{branch}":
                    return {
                        "ref": f"refs/heads/{branch}",
                        "object": {"type": "commit", "sha": existing_sha},
                    }
                if path == f"git/commits/{existing_sha}":
                    return {
                        "sha": existing_sha,
                        "tree": {"sha": candidate["tree"]},
                        "parents": [{"sha": self.base}, {"sha": upstream}],
                    }
                if path == "pulls":
                    self.assertEqual(payload["head"], branch)
                    self.assertEqual(payload["base"], "main")
                    return {"number": 8, "html_url": "https://github.com/example/repository/pull/8"}
                return {}

            with patch.dict(os.environ, env), \
                 patch.object(sync, "api", side_effect=api_match), \
                 patch.object(sync, "merge_when_green") as merge:
                sync.automate(self.root, self.base, upstream)
                merge.assert_called_once_with(
                    sync.REPOSITORY, 8, branch, existing_sha, candidate["base"]
                )
            post_paths = [p for p, _, m in calls if m == "POST"]
            self.assertEqual(post_paths, ["pulls"])

        with self.subTest(case="tampered-tree-sha-fails-closed"):
            calls = []

            def api_bad_tree(repository, path="", payload=None, method="GET", paginate=False):
                calls.append((path, payload, method))
                if path == "branches/main/protection":
                    return protection()
                if path == "":
                    return {"default_branch": "main", "allow_merge_commit": True}
                if path.startswith("pulls?"):
                    return [[]]
                if path == "branches/main":
                    return {"commit": {"sha": self.base}}
                if path == f"git/ref/heads/{branch}":
                    return {
                        "ref": f"refs/heads/{branch}",
                        "object": {"type": "commit", "sha": existing_sha},
                    }
                if path == f"git/commits/{existing_sha}":
                    return {
                        "sha": existing_sha,
                        "tree": {"sha": "f" * 40},
                        "parents": [{"sha": self.base}, {"sha": upstream}],
                    }
                return {}

            with patch.dict(os.environ, env), \
                 patch.object(sync, "api", side_effect=api_bad_tree), \
                 patch.object(sync, "merge_when_green") as merge:
                with self.assertRaisesRegex(sync.SyncError, "does not match verified tree/parents"):
                    sync.automate(self.root, self.base, upstream)
                merge.assert_not_called()
            self.assertEqual([p for p, _, m in calls if m == "POST"], [])

        with self.subTest(case="mismatched-parents-fails-closed"):
            calls = []

            def api_bad_parents(repository, path="", payload=None, method="GET", paginate=False):
                calls.append((path, payload, method))
                if path == "branches/main/protection":
                    return protection()
                if path == "":
                    return {"default_branch": "main", "allow_merge_commit": True}
                if path.startswith("pulls?"):
                    return [[]]
                if path == "branches/main":
                    return {"commit": {"sha": self.base}}
                if path == f"git/ref/heads/{branch}":
                    return {
                        "ref": f"refs/heads/{branch}",
                        "object": {"type": "commit", "sha": existing_sha},
                    }
                if path == f"git/commits/{existing_sha}":
                    return {
                        "sha": existing_sha,
                        "tree": {"sha": candidate["tree"]},
                        "parents": [{"sha": self.base}],
                    }
                return {}

            with patch.dict(os.environ, env), \
                 patch.object(sync, "api", side_effect=api_bad_parents), \
                 patch.object(sync, "merge_when_green") as merge:
                with self.assertRaisesRegex(sync.SyncError, "does not match verified tree/parents"):
                    sync.automate(self.root, self.base, upstream)
                merge.assert_not_called()
            self.assertEqual([p for p, _, m in calls if m == "POST"], [])

    def test_api_classifies_http_404_as_not_found_and_non_404_as_sync_error(self):
        """Verify sync.api distinguishes HTTP 404 NotFoundError from general SyncError failures.

        Accepts only self (TreeTests).
        Returns None after verifying 404 raises NotFoundError and 500 raises plain SyncError.
        Raises AssertionError if HTTP status classification fails.
        """
        res_404 = subprocess.CompletedProcess(
            ["gh", "api"], 1,
            '{"message":"Not Found","status":"404"}',
            "gh: Not Found (HTTP 404)\n",
        )
        with patch.object(sync.subprocess, "run", return_value=res_404):
            with self.assertRaises(sync.NotFoundError):
                sync.api(sync.REPOSITORY, "git/ref/heads/missing")

        res_500 = subprocess.CompletedProcess(
            ["gh", "api"], 1,
            '{"message":"Internal Server Error","status":"500"}',
            "gh: Internal Server Error (HTTP 500)\n",
        )
        with patch.object(sync.subprocess, "run", return_value=res_500):
            with self.assertRaises(sync.SyncError) as ctx:
                sync.api(sync.REPOSITORY, "git/ref/heads/missing")
            self.assertNotIsInstance(ctx.exception, sync.NotFoundError)


class GateTests(unittest.TestCase):
    def test_real_workflow_gate_accepts_only_the_two_valid_job_combinations(self):
        source = (ci.ROOT / ".github/workflows/glassbox-ci.yml").read_text()
        code = source.split("          python3 - <<'PY'\n", 1)[1].rsplit("          PY", 1)[0]
        program = compile(textwrap.dedent(code), "<workflow-gate>", "exec")
        valid = {
            ("success", "true", "success", "success", "success"),
            ("success", "false", "skipped", "skipped", "skipped"),
        }
        states = ("success", "failure", "cancelled", "skipped")
        for values in itertools.product(states, ("true", "false", "", "unknown"),
                                        states, states, states):
            with patch.dict(os.environ, dict(zip(
                ("CHANGES", "REQUIRED", "FORMAT", "WORKFLOWS", "CHECKS"), values,
            ))):
                try:
                    exec(program, {})
                    accepted = True
                except AssertionError:
                    accepted = False
            self.assertEqual(accepted, values in valid, values)

    def test_requires_actual_named_gate_and_every_check_successful(self):
        self.assertFalse(sync.all_green([]))
        self.assertFalse(sync.all_green([{"name": "other", "state": "SUCCESS"}]))
        self.assertTrue(sync.all_green(green()))
        for state in ["PENDING", "QUEUED", "IN_PROGRESS", "WAITING"]:
            self.assertFalse(sync.all_green(green() + [{"state": state}]))
        for state in ["FAILURE", "CANCELLED", "SKIPPED", "NEUTRAL", "TIMED_OUT", None]:
            with self.subTest(state=state), self.assertRaises(sync.SyncError):
                sync.all_green(green() + [{"state": state}])

    def test_protection_requires_gate_strictness_and_admin_enforcement(self):
        sync.require_protection(protection())
        for settings in [
            {}, {"required_status_checks": {"strict": True}},
            {**protection(), "enforce_admins": {"enabled": False}},
            {**protection(), "required_status_checks": {"contexts": [sync.GATE]}},
        ]:
            with self.subTest(settings=settings), self.assertRaises(sync.SyncError):
                sync.require_protection(settings)

    def test_merge_rechecks_green_and_pins_head(self):
        calls = []

        def api(repository, path, payload=None, method="GET"):
            calls.append((path, payload, method))
            return protection() if path.endswith("protection") else {"merged": True}

        with patch.object(sync, "pr_checks", return_value=green()), \
             patch.object(sync, "verify_refs") as refs, \
             patch.object(sync, "api", side_effect=api):
            sync.merge_when_green("owner/repo", 7, "branch", "head", "base")
            refs.assert_called_once_with("owner/repo", 7, "branch", "head", "base")
        self.assertEqual(calls[-1][1]["sha"], "head")
        self.assertEqual(calls[-1][2], "PUT")

    def test_failed_or_missing_checks_never_merge(self):
        with patch.object(sync, "pr_checks", return_value=[{"state": "FAILURE"}]), \
             patch.object(sync, "api") as api, self.assertRaises(sync.SyncError):
            sync.merge_when_green("owner/repo", 7, "branch", "head", "base")
        api.assert_not_called()
        with patch.object(sync, "api") as api, self.assertRaises(sync.SyncError):
            sync.merge_when_green("owner/repo", 7, "branch", "head", "base", wait_seconds=0)
        api.assert_not_called()

    def test_base_or_head_movement_blocks_merge(self):
        with patch.object(sync, "pr_checks", return_value=green()), \
             patch.object(sync, "verify_refs", side_effect=sync.SyncError("moved")), \
             patch.object(sync, "api") as api, self.assertRaises(sync.SyncError):
            sync.merge_when_green("owner/repo", 7, "branch", "head", "base")
        api.assert_not_called()

    def test_ref_verification_rejects_conflicts_unknown_mergeability_and_changed_refs(self):
        repository = "owner/repo"
        valid = {
            "state": "open", "draft": False, "mergeable": True,
            "base": {"ref": "main", "repo": {"full_name": repository}},
            "head": {"ref": "branch", "sha": "head", "repo": {"full_name": repository}},
        }
        with patch.object(sync, "api", side_effect=[valid, {"commit": {"sha": "base"}}]):
            sync.verify_refs(repository, 7, "branch", "head", "base")
        bad_cases = [
            {**valid, "mergeable": False}, {**valid, "mergeable": None},
            {**valid, "draft": True}, {**valid, "state": "closed"},
            {**valid, "head": {**valid["head"], "sha": "moved"}},
            {**valid, "head": {**valid["head"], "repo": {"full_name": "outsider/repo"}}},
        ]
        for pr in bad_cases:
            with self.subTest(pr=pr), \
                 patch.object(sync, "api", side_effect=[pr, {"commit": {"sha": "base"}}]), \
                 self.assertRaises(sync.SyncError):
                sync.verify_refs(repository, 7, "branch", "head", "base")
        with patch.object(sync, "api", side_effect=[valid, {"commit": {"sha": "moved"}}]), \
             self.assertRaises(sync.SyncError):
            sync.verify_refs(repository, 7, "branch", "head", "base")

    def test_pending_checks_can_complete_and_are_rechecked_before_merging(self):
        pending = green() + [{"name": "late-check", "state": "PENDING"}]
        with patch.object(sync, "pr_checks", side_effect=[pending, green(), green()]), \
             patch.object(sync, "verify_refs"), patch.object(sync.time, "sleep") as sleep, \
             patch.object(sync, "api", side_effect=[protection(), {"merged": True}]) as api:
            sync.merge_when_green("owner/repo", 7, "branch", "head", "base")
        sleep.assert_called_once_with(15)
        self.assertEqual(api.call_args.kwargs["method"], "PUT")

    def test_checks_regression_between_observations_never_merges(self):
        with patch.object(sync, "pr_checks",
                          side_effect=[green(), [{"state": "FAILURE"}]]), \
             patch.object(sync, "verify_refs"), \
             patch.object(sync, "api", return_value=protection()) as api, \
             self.assertRaises(sync.SyncError):
            sync.merge_when_green("owner/repo", 7, "branch", "head", "base")
        self.assertEqual(api.call_count, 1)  # Protection read only.

    def test_local_automation_never_writes_github(self):
        with patch.dict(os.environ, {"GITHUB_ACTIONS": "false"}), \
             patch.object(sync, "api") as api, self.assertRaises(sync.SyncError):
            sync.automate(Path("."), "HEAD", "HEAD")
        api.assert_not_called()


class ChangesTests(unittest.TestCase):
    def test_format_targets_renamed_packages(self):
        """Verify formatting selects only the renamed downstream packages.

        Accepts no parameters and mocks the subprocess boundary.
        Returns unit after checking exact Cargo package arguments.
        Raises AssertionError when legacy package names or an unscoped formatter remains.
        """
        with patch.object(ci.subprocess, "run") as run, patch.object(
                sys, "argv", ["ci.py", "format"]):
            ci.main()
        arguments = run.call_args.args[0]
        self.assertEqual(list(arguments[-5:-1]), [
            "-p", "glassbox-cli", "-p", "glassbox-inspect"])
        self.assertNotIn("litx-inspect", arguments)

    def test_full_check_runs_automation_tests_rust_checks_and_smoke(self):
        """Verify the check command runs scoped automation and driver tests, Cargo checks, and smoke.

        Accepts only self (ChangesTests) and mocks ci.run and ci.smoke.
        Returns None after asserting exact command order and no harness references.
        Raises AssertionError when any required check is missing, misordered, or invokes verify-settling-in.py.
        """
        with patch.object(ci, "run") as run, patch.object(ci, "smoke") as smoke, patch.object(
                sys, "argv", ["ci.py", "check"]):
            ci.main()
        commands = [call.args for call in run.call_args_list]
        self.assertEqual(len(commands), 4)
        self.assertEqual(
            commands[0],
            (
                "python3", "-B", "-m", "unittest",
                "extensionalities/scripts/tests/test_automation.py",
                "extensionalities/scripts/tests/test_inspect_extract_outputs_driver.py",
                "-v",
            ),
        )
        self.assertEqual(commands[1][:2], ("cargo", "clippy"))
        self.assertEqual(commands[2][:2], ("cargo", "test"))
        self.assertEqual(commands[3][:4], ("cargo", "build", "--locked", "--release"))
        for command in commands:
            self.assertFalse(
                any("verify-settling-in.py" in arg for arg in command),
                command,
            )
        smoke.assert_called_once_with()

    def test_smoke_distinguishes_metadata_inspection_from_canonical_extraction(self):
        """Verify smoke requires schema v3 inspect reports, bodymatter Markdown, and extract reports.

        Accepts no parameters and mocks only the release-binary subprocess boundary.
        Returns unit after checking single-file, Markdown marker, batch, and offline-OCR invocations.
        Raises AssertionError if smoke accepts a body field or skips the alias checks.
        """
        metadata_report = {
            "schema_version": 3, "kind": "inspect", "status": "complete",
            "file": {"path": "hello.txt", "name": "hello.txt", "bytes": 42, "sha256": "a" * 64},
            "format": {"type": "text", "extension": "txt"},
            "description": {},
            "structure": {"counts": {"lines": 1}},
            "readiness": {
                "extractable": "supported",
                "encrypted": "none",
                "text_layer": "present",
                "ocr": "not_needed",
            },
        }
        extract_report = {
            "schema_version": 3, "kind": "extract", "status": "complete",
            "completeness": "exact",
            "metadata": metadata_report,
            "content": {"units": [{
                "text": (ci.ROOT / "extensionalities/fixtures/hello.txt").read_text()
            }]},
        }
        calls = []

        def output(arguments, **kwargs):
            calls.append(arguments)
            if "--batch" in arguments:
                return json.dumps({"id": "smoke", "report": metadata_report}).encode()
            if "--format" in arguments and "markdown" in arguments:
                return b'<!-- bodymatter:v1 <page number="1" origin="native"/> -->\n\nNative PDF evidence.\n'
            report = metadata_report if arguments[1] == "inspect" else extract_report
            return json.dumps(report).encode()

        def missing_model(arguments, **kwargs):
            calls.append(arguments)
            self.assertEqual(arguments[1], "extract")
            report = {
                "schema_version": 3, "kind": "extract",
                "status": "partial", "completeness": "partial",
                "metadata": {**metadata_report, "status": "partial",
                             "diagnostics": [{"code": "pdf_ocr"}]},
                "diagnostics": [{"code": "pdf_ocr"}],
                "content": {"units": [{"text": "Native PDF evidence"}]},
            }
            return subprocess.CompletedProcess(arguments, 2, json.dumps(report).encode(), b"")

        with patch.object(ci.subprocess, "check_output", side_effect=output), patch.object(
                ci.subprocess, "run", side_effect=missing_model):
            ci.smoke()
        self.assertEqual(sum("--batch" in call for call in calls), 2)
        self.assertEqual(sum(call[1] == "extract" for call in calls), 4)

    def test_workflow_linter_is_pinned_scoped_and_cache_isolated(self):
        with patch.object(ci.subprocess, "run") as run:
            ci.workflows()
        args = run.call_args.args[0]
        self.assertIn("github.com/rhysd/actionlint/cmd/actionlint@v1.7.12", args)
        self.assertEqual(args[-2:], [
            ".github/workflows/glassbox-ci.yml",
            ".github/workflows/glassbox-upstream-sync.yml",
        ])
        self.assertTrue(run.call_args.kwargs["check"])
        for key in ("GOPATH", "GOMODCACHE", "GOCACHE"):
            self.assertTrue(Path(run.call_args.kwargs["env"][key]).is_relative_to(TARGET))

    def test_downstream_and_dependency_paths(self):
        """Verify ci.relevant selects downstream code, manifests, and workflows while skipping docs.

        Accepts only self (ChangesTests).
        Returns None after checking representative code, dependency, workflow, and docs paths.
        Raises AssertionError when any path is misclassified by ci.relevant.
        """
        for path in [
            "extensionalities/crates/litx/src/main.rs",
            "extensionalities/fixtures/hello.txt",
            "extensionalities/scripts/ci.py",
            "extensionalities/Cargo.toml",
            "extensionalities/Cargo.lock",
            "crates/liteparse/src/lib.rs",
            "crates/pdfium-sys/build.rs",
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/config.toml",
            ".github/workflows/glassbox-ci.yml",
            "rust-toolchain.toml",
        ]:
            self.assertTrue(ci.relevant(path), path)
        for path in [
            "README.md",
            ".github/README.md",
            "extensionalities/AGENTS.md",
            "extensionalities/README.md",
            "extensionalities/atlas.json",
            "extensionalities/docs/automation.md",
            "extensionalities/.gitignore",
            "packages/node/README.md",
            "docs/index.md",
        ]:
            self.assertFalse(ci.relevant(path), path)

    def test_missing_diff_is_fail_safe_and_sync_is_always_checked(self):
        self.assertTrue(ci.needs_checks(Path("."), None, "head"))
        self.assertTrue(ci.needs_checks(Path("."), "0" * 40, "head"))
        self.assertTrue(ci.needs_checks(Path("."), "base", "head", force=True))
        result = subprocess.CompletedProcess([], 1, b"", b"missing")
        with patch.object(ci.subprocess, "run", return_value=result):
            self.assertTrue(ci.needs_checks(Path("."), "base", "head"))


if __name__ == "__main__":
    unittest.main()

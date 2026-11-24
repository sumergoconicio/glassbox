"""Module: inspect-extract-outputs verification driver unit tests.
File: test_inspect_extract_outputs_driver.py.
Responsibility: Unit-test XML marker parsing, strip equivalence, sampled fingerprint framing, --docs-only checks, and result schema builder without slow subprocesses.
"""

from __future__ import annotations

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import stat
import struct
import subprocess
import unittest
import zipfile

SCRIPTS_DIR = Path(__file__).resolve().parents[1]
EXT_DIR = SCRIPTS_DIR.parent
SANDBOX_DIR = EXT_DIR / "sandbox"
VERIFICATION_DIR = EXT_DIR / "target" / "verification"
DRIVER_PATH = SCRIPTS_DIR / "verify-inspect-extract-outputs.py"


def load_driver():
    spec = importlib.util.spec_from_file_location(
        "verify_inspect_extract_outputs", DRIVER_PATH
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


driver = load_driver()


class SandboxTestCase(unittest.TestCase):
    """Base test case providing an isolated sandbox dir cleaned on teardown."""

    def setUp(self) -> None:
        SANDBOX_DIR.mkdir(parents=True, exist_ok=True)
        self.tmp = SANDBOX_DIR / f"io-driver-unittest-{os.getpid()}-{secrets.token_hex(4)}"
        self.tmp.mkdir(parents=True, exist_ok=False)

    def tearDown(self) -> None:
        if self.tmp.exists():
            shutil.rmtree(self.tmp)


class EvidenceDirValidationTests(SandboxTestCase):
    def test_accepts_relative_and_absolute_paths_inside_verification_root(self) -> None:
        rel = "extensionalities/target/verification/inspect-extract-outputs/unit-check"
        resolved_rel = driver.validate_evidence_dir(rel)
        self.assertEqual(
            resolved_rel,
            VERIFICATION_DIR / "inspect-extract-outputs" / "unit-check",
        )
        abs_target = VERIFICATION_DIR / "inspect-extract-outputs" / "abs-check"
        self.assertEqual(driver.validate_evidence_dir(abs_target), abs_target)

    def test_rejects_outside_root_parent_escape_and_root_itself(self) -> None:
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir("/tmp/evil-evidence")
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(
                str(VERIFICATION_DIR / "inspect-extract-outputs" / ".." / ".." / "escape")
            )
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(str(VERIFICATION_DIR))

    def test_rejects_symlink_escape(self) -> None:
        outside = self.tmp / "outside"
        outside.mkdir()
        root = self.tmp / "ver-root"
        root.mkdir()
        link = root / "escape-link"
        os.symlink(outside, link)
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(link / "run-1", permitted_root=root)

    def test_rejects_dangling_and_self_symlink_bypasses(self) -> None:
        root = self.tmp / "ver-root"
        root.mkdir()
        dangling = root / "dangling-escape"
        os.symlink(self.tmp / "does-not-exist-outside", dangling)
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(dangling, permitted_root=root)
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(dangling / "nested", permitted_root=root)

        self_link = root / "self-link"
        os.symlink(root, self_link)
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(self_link, permitted_root=root)

        loop_link = root / "loop-link"
        os.symlink(loop_link, loop_link)
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(loop_link, permitted_root=root)

        inside_target = root / "real-run"
        inside_target.mkdir()
        inside_link = root / "inside-link"
        os.symlink(inside_target, inside_link)
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(inside_link, permitted_root=root)


class ExclusiveFileWriteTests(SandboxTestCase):
    """Verify O_CREAT|O_EXCL|O_NOFOLLOW file creation and log-dir hardening."""

    def test_write_exclusive_bytes_creates_new_file_and_blocks_overwrite_and_symlinks(
        self,
    ) -> None:
        """Verify write_exclusive_bytes creates new files and rejects existing files or symlinks.

        Parameters:
          self: TestCase instance with isolated sandbox directory.

        Returns:
          None after asserting creation, overwrite rejection, and symlink non-following.

        Failure modes:
          Raises AssertionError if existing files or symlink targets are overwritten.
        """
        target = self.tmp / "result.json"
        driver.write_exclusive_bytes(target, b'{"ok": true}\n')
        self.assertEqual(target.read_bytes(), b'{"ok": true}\n')
        self.assertEqual(stat.S_IMODE(target.stat().st_mode) & 0o077, 0)

        # Must refuse to overwrite an existing regular file
        with self.assertRaises(driver.DriverError):
            driver.write_exclusive_bytes(target, b"overwrite")
        self.assertEqual(target.read_bytes(), b'{"ok": true}\n')

        # Must refuse to follow a symlink pointing to an existing victim file
        victim = self.tmp / "victim.txt"
        victim.write_bytes(b"untouched")
        live_link = self.tmp / "live-link.json"
        os.symlink(victim, live_link)
        with self.assertRaises(driver.DriverError):
            driver.write_exclusive_bytes(live_link, b"clobbered")
        self.assertEqual(victim.read_bytes(), b"untouched")

        # Must refuse to follow a dangling symlink
        dangling_target = self.tmp / "should-not-be-created.txt"
        dangling_link = self.tmp / "dangling-link.json"
        os.symlink(dangling_target, dangling_link)
        with self.assertRaises(driver.DriverError):
            driver.write_exclusive_bytes(dangling_link, b"created-via-link")
        self.assertFalse(dangling_target.exists())

    def test_run_cmd_rejects_symlink_log_dir_and_duplicate_labels(self) -> None:
        """Verify run_cmd refuses symlinked log directories and overwriting prior logs.

        Parameters:
          self: TestCase instance with isolated sandbox directory.

        Returns:
          None after checking symlink log_dir rejection and duplicate label rejection.

        Failure modes:
          Raises AssertionError if run_cmd follows symlinks or overwrites an existing log file.
        """
        real_logs = self.tmp / "real-logs"
        real_logs.mkdir()
        link_logs = self.tmp / "link-logs"
        os.symlink(real_logs, link_logs)
        with self.assertRaises(driver.DriverError):
            driver.run_cmd(["python3", "-c", "print('hi')"], link_logs, "probe")

        driver.run_cmd(["python3", "-c", "print('first')"], real_logs, "unique_lbl")
        with self.assertRaises(driver.DriverError):
            driver.run_cmd(["python3", "-c", "print('second')"], real_logs, "unique_lbl")
        self.assertEqual((real_logs / "unique_lbl.stdout").read_text(), "first\n")


class BodymatterMarkerParserTests(unittest.TestCase):
    def test_parses_all_six_elements_and_decodes_hostile_attributes(self) -> None:
        markdown = "\n".join(
            [
                '<!-- bodymatter:v1 <page number="1" label="iv" origin="native"/> -->',
                "",
                "Ordinary prose with &lt;!-- bodymatter:v1 <page number=\"99\" origin=\"native\"/> --> neutralized.",
                "",
                '<!-- bodymatter:v1 <sheet number="2" name="Q1&#45;&#45;Draft &lt;x attr=&quot;1&quot;&gt;&amp;"/> -->',
                "",
                '<!-- bodymatter:v1 <slide number="1"/> -->',
                "",
                '<!-- bodymatter:v1 <table number="1"/> -->',
                "",
                '<!-- bodymatter:v1 <chapter number="3" href="ch03.xhtml"/> -->',
                "",
                '<!-- bodymatter:v1 <page-break number="1"/> -->',
            ]
        )
        parsed = driver.parse_bodymatter_markers(markdown)
        self.assertEqual(
            [(m["tag"], m["attrib"]) for m in parsed],
            [
                ("page", {"number": "1", "label": "iv", "origin": "native"}),
                ("sheet", {"number": "2", "name": 'Q1--Draft <x attr="1">&'}),
                ("slide", {"number": "1"}),
                ("table", {"number": "1"}),
                ("chapter", {"number": "3", "href": "ch03.xhtml"}),
                ("page-break", {"number": "1"}),
            ],
        )

    def test_rejects_raw_double_hyphen_inside_comment_body(self) -> None:
        bad = '<!-- bodymatter:v1 <sheet number="1" name="Q1--Draft"/> -->\n'
        with self.assertRaises(ValueError):
            driver.parse_bodymatter_markers(bad)

    def test_rejects_malformed_xml_fragment(self) -> None:
        bad = '<!-- bodymatter:v1 <sheet number="1" name="unclosed/> -->\n'
        with self.assertRaises(ValueError):
            driver.parse_bodymatter_markers(bad)


class MarkerStripEquivalenceTests(unittest.TestCase):
    def test_equivalent_blocks_pass(self) -> None:
        with_markers = (
            '<!-- bodymatter:v1 <page number="1" label="iv" origin="native"/> -->\n\n'
            "Paragraph one.\n\n"
            '<!-- bodymatter:v1 <page number="2" label="1" origin="native"/> -->\n\n'
            "Paragraph two.\n"
        )
        without_markers = "Paragraph one.\n\nParagraph two.\n"
        ok, details = driver.check_marker_strip_equivalence(with_markers, without_markers)
        self.assertTrue(ok, details)
        self.assertEqual(details["with_marker_matches_count"], 2)
        self.assertEqual(details["default_block_count"], 2)
        self.assertEqual(details["no_markers_block_count"], 2)

    def test_detects_content_mismatch_legacy_headings_and_leaked_markers(self) -> None:
        with_markers = (
            '<!-- bodymatter:v1 <page number="1" origin="native"/> -->\n\nAlpha\n'
        )
        ok, _ = driver.check_marker_strip_equivalence(with_markers, "Beta\n")
        self.assertFalse(ok)

        ok_legacy, details_legacy = driver.check_marker_strip_equivalence(
            "# Glassbox extraction\n\nAlpha\n", "Alpha\n"
        )
        self.assertFalse(ok_legacy)
        self.assertIn("# Glassbox extraction", details_legacy["legacy_headings_found"])

        ok_leaked, details_leaked = driver.check_marker_strip_equivalence(
            with_markers, with_markers
        )
        self.assertFalse(ok_leaked)
        self.assertEqual(details_leaked["no_marker_matches_count"], 1)

    def test_rejects_empty_markerless_and_malformed_with_markers_inputs(self) -> None:
        ok_empty, details_empty = driver.check_marker_strip_equivalence("", "")
        self.assertFalse(ok_empty)
        self.assertEqual(details_empty["with_marker_matches_count"], 0)
        self.assertEqual(details_empty["default_block_count"], 0)

        ok_no_markers, details_no_markers = driver.check_marker_strip_equivalence(
            "Alpha\n", "Alpha\n"
        )
        self.assertFalse(ok_no_markers)
        self.assertEqual(details_no_markers["with_marker_matches_count"], 0)

        ok_bad, details_bad = driver.check_marker_strip_equivalence(
            '<!-- bodymatter:v1 <sheet number="1" name="bad--name"/> -->\n\nAlpha\n',
            "Alpha\n",
        )
        self.assertFalse(ok_bad)
        self.assertIn("with_markers_parse_error", details_bad)


class SampledFingerprintOracleTests(SandboxTestCase):
    def test_framing_matches_spec_and_distinguishes_lengths(self) -> None:
        size_a = 4 * 1024 * 1024
        size_b = size_a + 1
        file_a = self.tmp / "a.bin"
        file_b = self.tmp / "b.bin"
        driver.build_sparse_media_file(file_a, size_a, b"SAME-SEED")
        driver.build_sparse_media_file(file_b, size_b, b"SAME-SEED")

        fp_a = driver.compute_sampled_fingerprint_oracle(file_a)
        fp_b = driver.compute_sampled_fingerprint_oracle(file_b)
        self.assertEqual(len(fp_a), 64)
        self.assertNotEqual(fp_a, fp_b)

        # Verify exact domain-separated framing independently
        h = hashlib.sha256()
        h.update(b"glassbox-sampled-sha256/v1\x00")
        h.update(struct.pack(">Q", size_a))
        raw = file_a.read_bytes()
        mib = 1024 * 1024
        for offset in (0, (size_a - mib) // 2, size_a - mib):
            h.update(struct.pack(">Q", offset))
            h.update(struct.pack(">Q", mib))
            h.update(raw[offset : offset + mib])
        self.assertEqual(fp_a, h.hexdigest())


class DocsOnlyValidatorTests(SandboxTestCase):
    def _write_valid_doc_tree(self, root: Path) -> None:
        (root / "docs").mkdir(parents=True, exist_ok=True)
        (root / "crates").mkdir(parents=True, exist_ok=True)
        (root / "fixtures").mkdir(parents=True, exist_ok=True)
        types_table = "\n".join(f"| `{fmt}` | sample |" for fmt in driver.ALL_23_FORMAT_TYPES)
        (root / "README.md").write_text(
            "# Glassbox\n\nSee [Inspect guide](docs/inspect.md) and [Extract guide](docs/extract.md#markers).\n"
            "```markdown\n[ignored broken link](docs/does-not-exist.md)\n```\n",
            encoding="utf-8",
        )
        (root / "docs" / "inspect.md").write_text(
            "# Inspect\n\n`schema_version`: 3\n\nFlags: `--full-hash`, `--verbose`.\n\n"
            "Sampled fingerprint scheme: `glassbox-sampled-sha256/v1`.\n\n"
            "| format.type | ext |\n| --- | --- |\n"
            f"{types_table}\n\nBack to [README](../README.md).\n",
            encoding="utf-8",
        )
        (root / "docs" / "extract.md").write_text(
            "# Extract\n\n## Markers\n\nUse `--no-markers` to suppress comments.\n\n"
            "Consumer recipe: match `^<!-- bodymatter:v1 (<.*/>) -->$` and parse XML.\n",
            encoding="utf-8",
        )
        (root / "crates" / "lib.rs").write_text("// standalone crate\n", encoding="utf-8")
        (root / "fixtures" / "sample.txt").write_text("standalone fixture\n", encoding="utf-8")

    def test_valid_tree_passes(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["status"], "PASS", result)
        self.assertGreater(result["links_checked"], 0)
        self.assertEqual(result["broken_links"], [])
        self.assertEqual(result["missing_format_types"], [])
        self.assertEqual(result["missing_tokens"], [])
        self.assertEqual(result["standalone_matches"], [])

    def test_broken_link_fails(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        (self.tmp / "docs" / "extract.md").write_text(
            "## Markers\n\nBroken: [missing](missing-page.md)\n"
            "`--no-markers` `^<!-- bodymatter:v1 (<.*/>) -->$` XML\n",
            encoding="utf-8",
        )
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["status"], "FAIL")
        self.assertEqual(len(result["broken_links"]), 1)
        self.assertEqual(result["broken_links"][0]["target"], "missing-page.md")

    def test_broken_same_file_and_cross_file_anchors_fail(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        readme = self.tmp / "README.md"
        readme.write_text(
            readme.read_text(encoding="utf-8")
            + "\nSee [same missing](#missing) and [cross missing](docs/extract.md#missing).\n",
            encoding="utf-8",
        )
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["status"], "FAIL")
        targets = [entry["target"] for entry in result["broken_links"]]
        self.assertIn("#missing", targets)
        self.assertIn("docs/extract.md#missing", targets)

    def test_duplicate_heading_slugs_and_fenced_headings_handled(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        extract_md = self.tmp / "docs" / "extract.md"
        extract_md.write_text(
            extract_md.read_text(encoding="utf-8")
            + "\n## Notes\n\n## Notes\n\n```markdown\n## Fenced Only\n```\n\n"
            "Valid: [first](#notes), [second](#notes-1).\n"
            "Invalid fenced heading: [fenced](#fenced-only).\n",
            encoding="utf-8",
        )
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["status"], "FAIL")
        self.assertEqual(
            [entry["target"] for entry in result["broken_links"]],
            ["#fenced-only"],
        )

    def test_empty_angle_bracket_link_does_not_crash(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        readme = self.tmp / "README.md"
        readme.write_text(
            readme.read_text(encoding="utf-8") + "\nEmpty angle link: [x](<>).\n",
            encoding="utf-8",
        )
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["status"], "PASS", result)

    def test_nested_four_backtick_fence_masks_inner_three_backtick_block(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        readme = self.tmp / "README.md"
        readme.write_text(
            readme.read_text(encoding="utf-8")
            + "\n````markdown\n"
            + "```text\n"
            + "[ignored inside inner](docs/nope-1.md)\n"
            + "```\n"
            + "[still inside 4-backtick outer](docs/nope-2.md)\n"
            + "````\n",
            encoding="utf-8",
        )
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["status"], "PASS", result)

        # Adding a broken link AFTER the closing 4-backtick fence must still be caught.
        readme.write_text(
            readme.read_text(encoding="utf-8")
            + "\n[outside broken](docs/nope-3.md)\n",
            encoding="utf-8",
        )
        result_after = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result_after["status"], "FAIL")
        self.assertEqual(
            [entry["target"] for entry in result_after["broken_links"]],
            ["docs/nope-3.md"],
        )

    def test_zero_links_checked_fails(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        (self.tmp / "README.md").write_text("# Glassbox\n\nNo links here.\n", encoding="utf-8")
        inspect_md = self.tmp / "docs" / "inspect.md"
        inspect_md.write_text(
            inspect_md.read_text(encoding="utf-8").replace("Back to [README](../README.md).", ""),
            encoding="utf-8",
        )
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["links_checked"], 0)
        self.assertEqual(result["status"], "FAIL")

    def test_missing_format_type_or_flag_fails(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        inspect_md = self.tmp / "docs" / "inspect.md"
        text = inspect_md.read_text(encoding="utf-8").replace("`3d_cad`", "`removed`")
        text = text.replace("--full-hash", "--other-flag")
        inspect_md.write_text(text, encoding="utf-8")
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["status"], "FAIL")
        self.assertIn("3d_cad", result["missing_format_types"])
        self.assertIn("flag_full_hash", result["missing_tokens"])

    def test_sibling_reference_detected_case_insensitively(self) -> None:
        self._write_valid_doc_tree(self.tmp)
        leaked_token = driver.forbidden_sibling_tokens()[0].upper()
        (self.tmp / "fixtures" / "leak.txt").write_text(
            f"reference to {leaked_token} repo\n", encoding="utf-8"
        )
        result = driver.validate_docs_tree(self.tmp)
        self.assertEqual(result["status"], "FAIL")
        self.assertEqual(len(result["standalone_matches"]), 1)
        self.assertEqual(result["standalone_matches"][0]["file"], "fixtures/leak.txt")


class ResultPayloadSchemaTests(unittest.TestCase):
    def test_overall_status_reflects_verification_matrix(self) -> None:
        ev_dir = VERIFICATION_DIR / "inspect-extract-outputs" / "schema-test"
        pass_payload = driver.build_result_payload(
            mode="e2e",
            verification_matrix={
                "V-IO-01": {"status": "PASS", "checks": [], "commands": []},
                "V-IO-02": {"status": "PASS", "checks": [], "commands": []},
            },
            git_info={"head": "abc", "branch": "feat/test", "dirty_files": []},
            toolchain_info={"rustc": "1.95.0"},
            binaries_info={"glassbox": {"sha256": "0" * 64}},
            timings_ms={"total_e2e_ms": 12.3},
            scratch_cleanup={"removed": True, "verified_absent": True},
            evidence_dir=ev_dir,
        )
        self.assertEqual(pass_payload["overall_status"], "PASS")
        for key in (
            "schema",
            "feature",
            "mode",
            "recorded_at",
            "overall_status",
            "evidence_dir",
            "git",
            "toolchain",
            "binaries",
            "timings_ms",
            "scratch_cleanup",
            "verification_matrix",
        ):
            self.assertIn(key, pass_payload)

        fail_payload = driver.build_result_payload(
            mode="e2e",
            verification_matrix={
                "V-IO-01": {"status": "PASS", "checks": [], "commands": []},
                "V-IO-02": {"status": "FAIL", "checks": [], "commands": []},
            },
            git_info={},
            toolchain_info={},
            binaries_info={},
            timings_ms={},
            scratch_cleanup={},
            evidence_dir=ev_dir,
            source_before={"manifest_sha256": "a" * 64, "file_count": 3},
            source_after={"manifest_sha256": "b" * 64, "file_count": 3},
        )
        self.assertEqual(fail_payload["overall_status"], "FAIL")
        self.assertEqual(fail_payload["source_before"]["manifest_sha256"], "a" * 64)
        self.assertEqual(fail_payload["source_after"]["manifest_sha256"], "b" * 64)
        self.assertFalse(fail_payload["source_snapshot_unchanged"])


class SafeHelpersAndSubprocessSafetyTests(SandboxTestCase):
    def test_safe_json_jsonl_and_marker_helpers(self) -> None:
        obj, err = driver.safe_json_loads('{"schema_version": 3}')
        self.assertIsNone(err)
        self.assertEqual(obj, {"schema_version": 3})

        bad_obj, bad_err = driver.safe_json_loads("{not json")
        self.assertIsNone(bad_obj)
        self.assertIsNotNone(bad_err)

        arr_obj, arr_err = driver.safe_json_loads("[1, 2]")
        self.assertIsNone(arr_obj)
        self.assertIn("expected JSON object", arr_err or "")

        recs, errs = driver.safe_jsonl_loads('{"a": 1}\nnot-json\n{"b": 2}\n')
        self.assertEqual(recs, [{"a": 1}, {"b": 2}])
        self.assertEqual(len(errs), 1)

        markers, m_err = driver.safe_parse_markers(
            '<!-- bodymatter:v1 <sheet number="1" name="a--b"/> -->\n'
        )
        self.assertEqual(markers, [])
        self.assertIsNotNone(m_err)

    def test_run_cmd_raises_driver_error_on_missing_binary_and_timeout(self) -> None:
        log_dir = self.tmp / "logs"
        with self.assertRaises(driver.DriverError):
            driver.run_cmd(
                [str(self.tmp / "nonexistent-binary-xyz")],
                log_dir,
                "missing_bin",
            )
        with self.assertRaises(driver.DriverError):
            driver.run_cmd(
                ["python3", "-c", "import sys, time; sys.stdout.write('partial'); sys.stdout.flush(); time.sleep(5)"],
                log_dir,
                "timeout_cmd",
                timeout=1,
            )
        self.assertTrue((log_dir / "timeout_cmd.stdout").is_file())
        self.assertTrue((log_dir / "timeout_cmd.stderr").is_file())


class DefaultEvidenceDirTests(SandboxTestCase):
    """Verify exclusive per-run default allocation and fail-closed explicit evidence directories."""

    def test_exclusive_default_evidence_dirs_are_distinct_across_same_mode_calls(
        self,
    ) -> None:
        """Verify back-to-back default allocations never collide across E2E or docs-only modes.

        Parameters:
          self: TestCase instance with isolated sandbox directory.

        Returns:
          None after asserting distinct timestamp+PID+nonce paths and exclusive mkdir allocation.

        Failure modes:
          Raises AssertionError if two default allocations collide or ignore mode prefixes.
        """
        args_e2e = driver.parse_args([])
        args_docs = driver.parse_args(["--docs-only"])
        self.assertIsNone(args_e2e.evidence_dir)
        self.assertIsNone(args_docs.evidence_dir)

        root = self.tmp / "ver-root"
        root.mkdir()
        e2e_1 = driver.resolve_evidence_dir(None, docs_only=False, permitted_root=root)
        e2e_2 = driver.resolve_evidence_dir(None, docs_only=False, permitted_root=root)
        docs_1 = driver.resolve_evidence_dir(None, docs_only=True, permitted_root=root)
        docs_2 = driver.resolve_evidence_dir(None, docs_only=True, permitted_root=root)

        self.assertTrue(e2e_1.name.startswith("run-e2e-"), e2e_1.name)
        self.assertTrue(e2e_2.name.startswith("run-e2e-"), e2e_2.name)
        self.assertTrue(docs_1.name.startswith("run-docs-"), docs_1.name)
        self.assertTrue(docs_2.name.startswith("run-docs-"), docs_2.name)
        self.assertEqual(len({e2e_1, e2e_2, docs_1, docs_2}), 4)

        # Exclusive allocation creates the directory and logs/ once, then refuses reuse
        logs_dir = driver.prepare_evidence_dir(
            e2e_1, require_new_dir=True, create_logs=True
        )
        self.assertEqual(logs_dir, e2e_1 / "logs")
        self.assertTrue(logs_dir.is_dir())
        with self.assertRaises(driver.DriverError):
            driver.prepare_evidence_dir(e2e_1, require_new_dir=True, create_logs=True)

    def test_explicit_evidence_dir_with_existing_result_or_logs_fails_closed(
        self,
    ) -> None:
        """Verify explicit --evidence-dir refuses directories containing prior result.json or logs/.

        Parameters:
          self: TestCase instance with isolated sandbox directory.

        Returns:
          None after asserting fresh explicit paths succeed and populated/symlink paths fail closed.

        Failure modes:
          Raises AssertionError if a reused or symlinked evidence directory is accepted.
        """
        root = self.tmp / "ver-root"
        root.mkdir()
        custom = root / "inspect-extract-outputs" / "custom-dir"
        self.assertEqual(
            driver.resolve_evidence_dir(custom, docs_only=True, permitted_root=root),
            custom,
        )

        # Existing result.json must fail closed
        custom.mkdir(parents=True)
        (custom / "result.json").write_text('{"prior": true}\n', encoding="utf-8")
        with self.assertRaises(driver.DriverError):
            driver.resolve_evidence_dir(custom, docs_only=True, permitted_root=root)
        with self.assertRaises(driver.DriverError):
            driver.prepare_evidence_dir(custom, require_new_dir=False, create_logs=False)

        # Existing logs/ directory must also fail closed
        with_logs = root / "inspect-extract-outputs" / "with-logs"
        (with_logs / "logs").mkdir(parents=True)
        with self.assertRaises(driver.DriverError):
            driver.resolve_evidence_dir(with_logs, docs_only=False, permitted_root=root)
        with self.assertRaises(driver.DriverError):
            driver.prepare_evidence_dir(with_logs, require_new_dir=False, create_logs=True)

        # Symlinked explicit evidence dir must fail closed
        real_empty = root / "inspect-extract-outputs" / "real-empty"
        real_empty.mkdir(parents=True)
        sym_dir = root / "inspect-extract-outputs" / "sym-dir"
        os.symlink(real_empty, sym_dir)
        with self.assertRaises(driver.DriverError):
            driver.resolve_evidence_dir(sym_dir, docs_only=False, permitted_root=root)


def _git_in_tmp(repo: Path, *args: str) -> str:
    env = {
        **os.environ,
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CEILING_DIRECTORIES": str(repo.parent),
    }
    completed = subprocess.run(
        [
            "git",
            "-C",
            str(repo),
            "-c",
            "user.name=Glassbox Test",
            "-c",
            "user.email=test@example.com",
            *args,
        ],
        capture_output=True,
        text=True,
        env=env,
        check=True,
    )
    return completed.stdout.strip()


def _init_temp_git_repo(repo: Path) -> str:
    repo.mkdir(parents=True, exist_ok=True)
    _git_in_tmp(repo, "init", "-b", "main")
    (repo / "crates").mkdir()
    (repo / "crates" / "lib.rs").write_text("// upstream\n", encoding="utf-8")
    (repo / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
    (repo / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
    (repo / "README.md").write_text("# Upstream\n", encoding="utf-8")
    (repo / "extensionalities").mkdir()
    (repo / "extensionalities" / "README.md").write_text("# Glassbox\n", encoding="utf-8")
    _git_in_tmp(repo, "add", ".")
    _git_in_tmp(repo, "commit", "-m", "initial")
    return _git_in_tmp(repo, "rev-parse", "HEAD")


class BenchmarkReceiptValidationTests(SandboxTestCase):
    def _valid_receipt(self, commit: str = "8c89a7591d8f7c26b5ad2c90ab1c064e01a1b609") -> dict:
        digest = "a" * 64
        return {
            "verification_id": "V-IO-01",
            "tested_commit": commit,
            "benchmark": {
                "fixture_bytes": 268_435_456,
                "passes_ms": [481.249, 481.453, 481.087, 481.135, 481.229],
                "median_ms": 481.229,
                "sha256_hashing_rs": digest,
                "sha256_system_sha256sum": digest,
                "sha256sum_parity": True,
            },
            "threshold_decision": {
                "chosen_constant_name": "DEFAULT_FULL_HASH_MAX_BYTES",
                "chosen_bytes": 134_217_728,
            },
            "scratch_cleanup": {
                "removed_after_recording": True,
            },
        }

    def _by_name(self, checks: list[dict]) -> dict[str, dict]:
        return {c["name"]: c for c in checks}

    def test_valid_receipt_passes_with_git_ancestry(self) -> None:
        repo = self.tmp / "repo"
        base_commit = _init_temp_git_repo(repo)
        _git_in_tmp(repo, "checkout", "-b", "feat/test")
        (repo / "extensionalities" / "README.md").write_text(
            "# Glassbox updated\n", encoding="utf-8"
        )
        _git_in_tmp(repo, "add", "extensionalities/README.md")
        _git_in_tmp(repo, "commit", "-m", "downstream edit")

        receipt_path = self.tmp / "bench.json"
        receipt_path.write_text(
            json.dumps(self._valid_receipt(base_commit)), encoding="utf-8"
        )
        checks = driver.validate_bench_receipt_file(
            receipt_path, repo_root=repo, check_git_ancestry=True
        )
        self.assertTrue(all(c["ok"] for c in checks), checks)

    def test_rejects_non_ancestor_and_malformed_commit(self) -> None:
        repo = self.tmp / "repo"
        _init_temp_git_repo(repo)
        _git_in_tmp(repo, "checkout", "-b", "side")
        (repo / "extensionalities" / "side.txt").write_text("side\n", encoding="utf-8")
        _git_in_tmp(repo, "add", ".")
        _git_in_tmp(repo, "commit", "-m", "side commit")
        side_commit = _git_in_tmp(repo, "rev-parse", "HEAD")
        _git_in_tmp(repo, "checkout", "main")

        checks_side = self._by_name(
            driver.validate_bench_receipt_payload(
                self._valid_receipt(side_commit), repo_root=repo, check_git_ancestry=True
            )
        )
        self.assertFalse(checks_side["tested_commit_valid_and_in_lineage"]["ok"])

        checks_bad = self._by_name(
            driver.validate_bench_receipt_payload(
                self._valid_receipt("not-a-hex-sha!"), check_git_ancestry=False
            )
        )
        self.assertFalse(checks_bad["tested_commit_valid_and_in_lineage"]["ok"])

    def test_rejects_malformed_types_wrong_median_and_inconsistent_chosen_bytes(self) -> None:
        # 1. bool fixture_bytes
        r_bool_bytes = copy.deepcopy(self._valid_receipt())
        r_bool_bytes["benchmark"]["fixture_bytes"] = True
        c = self._by_name(
            driver.validate_bench_receipt_payload(r_bool_bytes, check_git_ancestry=False)
        )
        self.assertFalse(c["benchmark_fixture_bytes_256_mib"]["ok"])

        # 2. passes_ms with < 5 entries or bool entry
        r_short_passes = copy.deepcopy(self._valid_receipt())
        r_short_passes["benchmark"]["passes_ms"] = [480.0, 481.0, 482.0, 483.0]
        r_short_passes["benchmark"]["median_ms"] = 481.5
        c = self._by_name(
            driver.validate_bench_receipt_payload(r_short_passes, check_git_ancestry=False)
        )
        self.assertFalse(c["benchmark_passes_and_median_consistent"]["ok"])

        r_bool_pass = copy.deepcopy(self._valid_receipt())
        r_bool_pass["benchmark"]["passes_ms"] = [480.0, True, 482.0, 483.0, 484.0]
        c = self._by_name(
            driver.validate_bench_receipt_payload(r_bool_pass, check_git_ancestry=False)
        )
        self.assertFalse(c["benchmark_passes_and_median_consistent"]["ok"])

        # 3. wrong median_ms (off by > 1e-3 ms)
        r_wrong_median = copy.deepcopy(self._valid_receipt())
        r_wrong_median["benchmark"]["median_ms"] = 481.500
        c = self._by_name(
            driver.validate_bench_receipt_payload(r_wrong_median, check_git_ancestry=False)
        )
        self.assertFalse(c["benchmark_passes_and_median_consistent"]["ok"])

        # 4. inconsistent chosen_bytes: fast median (<= 200ms) paired with 128 MiB
        r_inconsistent_fast = copy.deepcopy(self._valid_receipt())
        r_inconsistent_fast["benchmark"]["passes_ms"] = [150.0, 151.0, 152.0, 153.0, 154.0]
        r_inconsistent_fast["benchmark"]["median_ms"] = 152.0
        r_inconsistent_fast["threshold_decision"]["chosen_bytes"] = 134_217_728
        c = self._by_name(
            driver.validate_bench_receipt_payload(
                r_inconsistent_fast, check_git_ancestry=False
            )
        )
        self.assertFalse(c["threshold_decision_consistent_with_median_and_constant"]["ok"])

        # 5. inconsistent chosen_bytes: slow median (> 200ms) paired with 256 MiB
        r_inconsistent_slow = copy.deepcopy(self._valid_receipt())
        r_inconsistent_slow["threshold_decision"]["chosen_bytes"] = 268_435_456
        c = self._by_name(
            driver.validate_bench_receipt_payload(
                r_inconsistent_slow, check_git_ancestry=False
            )
        )
        self.assertFalse(c["threshold_decision_consistent_with_median_and_constant"]["ok"])

        # 6. bad sha256 digests (uppercase or mismatch)
        r_upper_sha = copy.deepcopy(self._valid_receipt())
        r_upper_sha["benchmark"]["sha256_hashing_rs"] = "A" * 64
        r_upper_sha["benchmark"]["sha256_system_sha256sum"] = "A" * 64
        c = self._by_name(
            driver.validate_bench_receipt_payload(r_upper_sha, check_git_ancestry=False)
        )
        self.assertFalse(c["sha256sum_parity_and_digests_valid"]["ok"])

        # 7. CLI --bench-receipt flag default & override
        default_args = driver.parse_args([])
        self.assertEqual(Path(default_args.bench_receipt), driver.HASH_BENCH_RECEIPT)
        custom_args = driver.parse_args(["--bench-receipt", "custom/bench.json"])
        self.assertEqual(custom_args.bench_receipt, "custom/bench.json")


class SourceFreshnessTests(SandboxTestCase):
    """Verify source manifest freshness and cryptographic `.glassbox-build-receipt.json` provenance."""

    def _make_fake_workspace(self) -> tuple[Path, Path, Path, Path]:
        ext = self.tmp / "ext"
        (ext / "crates" / "litx" / "src").mkdir(parents=True)
        src_file = ext / "crates" / "litx" / "src" / "main.rs"
        src_file.write_text("fn main() {}\n", encoding="utf-8")
        (ext / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
        (ext / "Cargo.lock").write_text("# lock\n", encoding="utf-8")

        target = self.tmp / "target"
        rel_dir = target / "release"
        rel_dir.mkdir(parents=True)
        gb = rel_dir / "glassbox"
        litx = rel_dir / "litx"
        for b in (gb, litx):
            b.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            b.chmod(b.stat().st_mode | stat.S_IXUSR)
        return ext, target, gb, litx

    def test_prebuilt_binaries_without_receipt_or_with_mismatched_receipt_fail_closed(
        self,
    ) -> None:
        """Verify newer binary mtimes alone never establish provenance without a matching receipt.

        Parameters:
          self: TestCase instance with isolated sandbox directory.

        Returns:
          None after verifying missing receipt, mismatched manifest SHA, mismatched binary SHA,
          and symlinked receipt all fail closed, while a matching build receipt passes.

        Failure modes:
          Raises AssertionError if unreceipted or tampered prebuilt binaries pass freshness checks.
        """
        ext, target, gb, litx = self._make_fake_workspace()
        src_file = ext / "crates" / "litx" / "src" / "main.rs"

        t_src = 1_700_000_100
        for p in (src_file, ext / "Cargo.toml", ext / "Cargo.lock"):
            os.utime(p, (t_src, t_src))
        os.utime(gb, (t_src + 10, t_src + 10))
        os.utime(litx, (t_src + 10, t_src + 10))

        # 1. Newer mtimes WITHOUT .glassbox-build-receipt.json must fail closed!
        no_receipt = driver.check_binary_freshness(ext, {"glassbox": gb, "litx": litx})
        self.assertTrue(no_receipt["mtime_fresh"])
        self.assertFalse(no_receipt["receipt_valid"])
        self.assertFalse(no_receipt["all_fresh"])
        self.assertTrue(
            any("missing build receipt" in err for err in no_receipt["receipt_errors"]),
            no_receipt["receipt_errors"],
        )

        ev_no_receipt = self.tmp / "ev-no-receipt"
        with self.assertRaises(driver.DriverError) as ctx:
            driver.run_e2e(ev_no_receipt, target, ext_dir=ext, allow_stale_binary=False)
        self.assertIn("--build", str(ctx.exception))

        # 2. Write a valid matching build receipt -> passes check_binary_freshness
        manifest = driver.compute_source_manifest(ext)
        receipt_path = driver.build_receipt_path(target)
        driver.write_build_receipt(
            receipt_path,
            source_manifest_sha256=manifest["manifest_sha256"],
            glassbox_sha256=driver.sha256_file(gb),
            litx_sha256=driver.sha256_file(litx),
            rustc_version="rustc 1.95.0 (test)",
            cargo_version="cargo 1.95.0 (test)",
            built_at_utc="2026-11-23T22:00:00Z",
        )
        fresh = driver.check_binary_freshness(ext, {"glassbox": gb, "litx": litx})
        self.assertTrue(fresh["mtime_fresh"], fresh)
        self.assertTrue(fresh["receipt_valid"], fresh)
        self.assertTrue(fresh["all_fresh"], fresh)
        self.assertEqual(fresh["stale_binaries"], [])
        self.assertEqual(fresh["receipt_errors"], [])
        self.assertEqual(len(fresh["source_manifest"]["manifest_sha256"]), 64)
        self.assertEqual(fresh["source_manifest"]["file_count"], 3)

        # 3. Source content change with older mtime (backdated!) still fails receipt SHA check
        src_file.write_text('fn main() { println!("changed"); }\n', encoding="utf-8")
        os.utime(src_file, (t_src, t_src))
        backdated_src = driver.check_binary_freshness(ext, {"glassbox": gb, "litx": litx})
        self.assertTrue(backdated_src["mtime_fresh"])
        self.assertFalse(backdated_src["receipt_valid"])
        self.assertFalse(backdated_src["all_fresh"])
        self.assertTrue(
            any("source_manifest_sha256" in e for e in backdated_src["receipt_errors"]),
            backdated_src["receipt_errors"],
        )

        # 4. Restore source, tamper binary bytes (preserving newer mtime) -> fails binary SHA check
        src_file.write_text("fn main() {}\n", encoding="utf-8")
        os.utime(src_file, (t_src, t_src))
        gb.write_text("#!/bin/sh\n# tampered\nexit 0\n", encoding="utf-8")
        gb.chmod(gb.stat().st_mode | stat.S_IXUSR)
        os.utime(gb, (t_src + 15, t_src + 15))
        tampered_bin = driver.check_binary_freshness(ext, {"glassbox": gb, "litx": litx})
        self.assertTrue(tampered_bin["mtime_fresh"])
        self.assertFalse(tampered_bin["receipt_valid"])
        self.assertFalse(tampered_bin["all_fresh"])
        self.assertTrue(
            any("glassbox_sha256" in e for e in tampered_bin["receipt_errors"]),
            tampered_bin["receipt_errors"],
        )

        # 5. Newer source mtime fails mtime freshness AND run_e2e aborts with stale error
        os.utime(src_file, (t_src + 30, t_src + 30))
        stale = driver.check_binary_freshness(ext, {"glassbox": gb, "litx": litx})
        self.assertFalse(stale["mtime_fresh"])
        self.assertFalse(stale["all_fresh"])
        self.assertEqual(sorted(stale["stale_binaries"]), ["glassbox", "litx"])
        self.assertGreater(stale["binaries"]["glassbox"]["stale_lag_seconds"], 0.0)

        ev_stale = self.tmp / "ev-stale"
        with self.assertRaises(driver.DriverError) as ctx2:
            driver.run_e2e(ev_stale, target, ext_dir=ext, allow_stale_binary=False)
        self.assertIn("stale release binary", str(ctx2.exception))
        self.assertIn("--build", str(ctx2.exception))

        # 6. Symlinked build receipt is rejected even if target contains valid JSON
        outside_receipt = self.tmp / "outside-receipt.json"
        driver.write_build_receipt(
            outside_receipt,
            source_manifest_sha256=driver.compute_source_manifest(ext)["manifest_sha256"],
            glassbox_sha256=driver.sha256_file(gb),
            litx_sha256=driver.sha256_file(litx),
            rustc_version="rustc 1.95.0 (test)",
            cargo_version="cargo 1.95.0 (test)",
            built_at_utc="2026-11-23T22:00:00Z",
        )
        receipt_path.unlink()
        os.symlink(outside_receipt, receipt_path)
        os.utime(gb, (t_src + 40, t_src + 40))
        os.utime(litx, (t_src + 40, t_src + 40))
        sym_check = driver.check_binary_freshness(ext, {"glassbox": gb, "litx": litx})
        self.assertFalse(sym_check["receipt_valid"])
        self.assertFalse(sym_check["all_fresh"])
        self.assertTrue(
            any("symlinked" in e for e in sym_check["receipt_errors"]),
            sym_check["receipt_errors"],
        )


class UpstreamIsolationGitTests(SandboxTestCase):
    def test_detects_unstaged_staged_committed_and_untracked_upstream_changes(self) -> None:
        repo = self.tmp / "repo"
        _init_temp_git_repo(repo)
        _git_in_tmp(repo, "checkout", "-b", "feat/downstream")

        # Clean state passes
        clean = driver.check_upstream_isolation(repo_root=repo, base_ref="main")
        self.assertTrue(clean["all_empty"], clean)
        self.assertEqual(clean["base_ref"], "main")

        # Downstream-only changes still pass
        (repo / "extensionalities" / "new.txt").write_text("ok\n", encoding="utf-8")
        downstream_only = driver.check_upstream_isolation(repo_root=repo, base_ref="main")
        self.assertTrue(downstream_only["all_empty"], downstream_only)

        # 1. Unstaged upstream modification
        (repo / "README.md").write_text("# Modified upstream\n", encoding="utf-8")
        unstaged = driver.check_upstream_isolation(repo_root=repo, base_ref="main")
        self.assertFalse(unstaged["all_empty"])
        self.assertEqual(unstaged["unstaged_files"], ["README.md"])

        # 2. Staged upstream modification
        _git_in_tmp(repo, "add", "README.md")
        staged = driver.check_upstream_isolation(repo_root=repo, base_ref="main")
        self.assertFalse(staged["all_empty"])
        self.assertEqual(staged["staged_files"], ["README.md"])

        # 3. Committed upstream modification vs base_ref (clean working tree + index!)
        _git_in_tmp(repo, "commit", "-m", "touch upstream readme")
        committed = driver.check_upstream_isolation(repo_root=repo, base_ref="main")
        self.assertFalse(committed["all_empty"])
        self.assertEqual(committed["unstaged_files"], [])
        self.assertEqual(committed["staged_files"], [])
        self.assertEqual(committed["committed_files"], ["README.md"])

        # 4. Untracked file inside upstream crates/ on a clean branch
        _git_in_tmp(repo, "checkout", "-b", "feat/untracked", "main")
        (repo / "crates" / "stray.rs").write_text("// stray\n", encoding="utf-8")
        untracked = driver.check_upstream_isolation(repo_root=repo, base_ref="main")
        self.assertFalse(untracked["all_empty"])
        self.assertEqual(untracked["untracked_files"], ["crates/stray.rs"])

        # 5. Missing explicit base_ref fails closed
        bad_base = driver.check_upstream_isolation(
            repo_root=repo, base_ref="nonexistent-ref-xyz"
        )
        self.assertFalse(bad_base["all_empty"])
        self.assertIn("committed", bad_base["errors"])


class PinnedBaselineAndRegressionFixturesTests(SandboxTestCase):
    def test_pinned_native_pdf_baseline_digest_and_regression_fixtures(self) -> None:
        self.assertEqual(
            hashlib.sha256(
                driver.NATIVE_PDF_PARSE_BASELINE_TEXT.encode("utf-8")
            ).hexdigest(),
            driver.NATIVE_PDF_PARSE_BASELINE_SHA256,
        )
        self.assertTrue(driver.MINIMAL_PSD_BYTES.startswith(b"8BPS"))

        xlsx_path = self.tmp / "probe.xlsx"
        driver.build_synthetic_xlsx(xlsx_path)
        with zipfile.ZipFile(xlsx_path) as zf:
            sheet2 = zf.read("xl/worksheets/sheet2.xml").decode("utf-8")
            sheet3 = zf.read("xl/worksheets/sheet3.xml").decode("utf-8")
        self.assertIn("&lt;script&gt;alert(1)&lt;/script&gt;", sheet2)
        self.assertIn(driver.LONG_XLSX_HEADER, sheet3)
        self.assertIn("<v>2026</v>", sheet3)

        docx_path = self.tmp / "probe.docx"
        driver.build_synthetic_docx(docx_path)
        with zipfile.ZipFile(docx_path) as zf:
            doc_xml = zf.read("word/document.xml").decode("utf-8")
        self.assertEqual(doc_xml.count("<w:tbl>"), 2)
        self.assertEqual(doc_xml.count('<w:br w:type="page"/>'), 2)
        self.assertIn("&lt;script&gt;alert(1)&lt;/script&gt;", doc_xml)


if __name__ == "__main__":
    unittest.main()

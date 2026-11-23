"""Module: settling-in verification driver self-tests.
File: test_settling_in_driver.py.
Responsibility: Check V-SETUP through V-PERF registration, isolation, failure, oracle, and evidence contracts.

Isolated self-tests for the settling-in verification driver
(V-SETUP/V-NAME/V-META/V-PERF among others).

Exercises driver isolation validation, missing executable handling, nonzero
child exits, timeout enforcement, implemented-case registration, evidence
output integrity, no-overwrite semantics, the v2 baseline assertion semantics
and fresh-prerequisite propagation, parity prerequisite resolution without
fabricated receipts, and the V-PERF performance-case helpers (candidate
bounds, frozen scenario matrix, summarization, verdict rules, blocking-clock
metrology with resolution floor and worst-case tradeoffs, cumulative
attempt-history and budget accounting, historical reuse validation, the
C-TEST delivery-validation gates and accepted-evidence chain, and fail-closed
blocking). Stubs replace the native binary; no Rust build runs here. All
writes stay inside the permitted target/sandbox trees and nothing is deleted.

Portability contract: every test in this file runs from a clean checkout
with no retained gitignored evidence (extensionalities/target/ and
extensionalities/sandbox/ contents). Host-local forensic evidence is
checked by the driver itself during explicit verification runs; here the
delivery pin checker, the common build process-group lifecycle, and the
tracked P-OCR reuse oracle manifest are exercised through synthetic
fixtures that verify fail-closed behavior instead of skipping.
"""

import importlib.util
import inspect
import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
TARGET = SCRIPTS.parent / "target"
VERIFICATION = TARGET / "verification"
SANDBOX = SCRIPTS.parent / "sandbox"
DRIVER = SCRIPTS / "verify-settling-in.py"


def load_driver():
    spec = importlib.util.spec_from_file_location("verify_settling_in", DRIVER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


driver = load_driver()


def new_evidence_dir():
    VERIFICATION.mkdir(parents=True, exist_ok=True)
    return VERIFICATION / f"settling-in-test-{driver.run_id()}"


def new_sandbox_dir():
    SANDBOX.mkdir(parents=True, exist_ok=True)
    return SANDBOX / f"settling-in-test-selftest-{driver.run_id()}"


def run_driver(case, evidence_dir, *extra, timeout=300):
    return subprocess.run(
        [sys.executable, "-B", str(DRIVER), case,
         "--evidence-dir", str(evidence_dir), *extra],
        capture_output=True, text=True, errors="replace", timeout=timeout)


def write_stub(directory, name, body):
    path = directory / name
    path.write_text("#!/usr/bin/env python3\n" + body)
    path.chmod(0o755)
    return path


class ValidateEvidenceDirTests(unittest.TestCase):
    def test_rejects_relative_path(self):
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir("settling-in-relative/x")

    def test_rejects_outside_tree(self):
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir("/tmp/settling-in-evil")
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(
                str(SCRIPTS.parent / "settling-in-inside-repo-but-wrong"))

    def test_rejects_wrong_prefix(self):
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(str(VERIFICATION / "other-thing"))

    def test_rejects_parent_escape(self):
        with self.assertRaises(driver.DriverError):
            driver.validate_evidence_dir(
                str(VERIFICATION / "settling-in-ok" / ".." / "other"))

    def test_accepts_permitted_path(self):
        value = driver.validate_evidence_dir(
            str(VERIFICATION / "settling-in-acceptable"))
        self.assertEqual(value, (VERIFICATION / "settling-in-acceptable"))


class CaseRegistryTests(unittest.TestCase):
    def test_every_registered_case_is_implemented(self):
        """Verify every registered case owns an implemented entry.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking registry order and verification IDs.
        Raises AssertionError when an implementation is missing or an ID drifts.
        """
        implemented = [name for name, spec in driver.CASES.items()
                       if spec.implemented]
        self.assertEqual(implemented, ["baseline", "parity", "metadata",
                                       "extract", "inventory", "batch",
                                       "status", "performance"])
        self.assertEqual(driver.CASES["baseline"].verification_id, "V-SETUP")
        self.assertEqual(driver.CASES["parity"].verification_id, "V-NAME")
        self.assertEqual(driver.CASES["metadata"].verification_id, "V-META")
        self.assertEqual(driver.CASES["extract"].verification_id, "V-SELECT")
        self.assertEqual(driver.CASES["inventory"].verification_id, "V-INVENTORY")
        self.assertEqual(driver.CASES["batch"].verification_id, "V-BATCH")
        self.assertEqual(driver.CASES["status"].verification_id, "V-STATUS")
        self.assertEqual(driver.CASES["performance"].verification_id, "V-PERF")
        self.assertEqual(driver.ALL_SEQUENCE[-1], "performance")

    def test_performance_dispatch_reaches_its_runner(self):
        """Verify the performance case dispatches to the real runner.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking run_case forwards to run_performance_case.
        Raises AssertionError when the dispatch is missing or rerouted.
        """
        with patch.object(driver, "run_performance_case",
                          return_value=0) as runner:
            code = driver.run_case("performance", None, None)
        self.assertEqual(code, 0)
        runner.assert_called_once()

    def test_cli_rejects_unknown_case(self):
        completed = run_driver("bogus", new_evidence_dir())
        self.assertEqual(completed.returncode, 2)

    def test_cli_rejects_low_replicate_counts_and_unknown_candidates(self):
        """Verify the command-line boundary keeps the perf matrix bounded.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking replicate and candidate validation errors.
        Raises AssertionError when an under-replicated or oversized matrix is
        accepted quietly.
        """
        low = run_driver("performance", new_evidence_dir(),
                         "--perf-replicates", "3")
        self.assertEqual(low.returncode, 2, low.stderr)
        self.assertIn("at least 5", low.stderr)
        bogus = run_driver("performance", new_evidence_dir(),
                           "--perf-candidates", "fat-lto,magic,more")
        self.assertEqual(bogus.returncode, 2)
        with self.assertRaises(driver.DriverError):
            driver.perf_parse_candidates("unknown-name")
        with self.assertRaises(driver.DriverError):
            driver.perf_parse_candidates("thin-lto,thin-lto")
        with self.assertRaises(driver.DriverError):
            driver.perf_parse_candidates("thin-lto,thin-lto-cgu1,third")
        with self.assertRaises(driver.DriverError):
            driver.perf_parse_candidates("")

    def test_cli_requires_explicit_flags_for_historical_reuse(self):
        """Verify the historical reuse option never silently rebuilds.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking --reuse-historical is rejected without
        the explicit --historical-reconstruction gate.
        Raises AssertionError when reuse could bypass the explicit gate.
        """
        completed = run_driver("performance", new_evidence_dir(),
                               "--reuse-historical", "/absent/target")
        self.assertEqual(completed.returncode, 2, completed.stderr)
        self.assertIn("--historical-reconstruction", completed.stderr)

    def test_all_case_aggregates_every_registered_case(self):
        """Verify aggregation retains baseline failure and every later case.

        Accepts no parameters; a stub fails baseline and mocks isolate cost.
        Returns unit after checking baseline FAIL and all later PASS states.
        Raises AssertionError for incorrect aggregate status or missing evidence.
        """
        evidence = new_evidence_dir()
        stubs = new_sandbox_dir()
        stubs.mkdir(parents=True)
        stub = write_stub(stubs, "exit-seven", "import sys\nsys.exit(7)\n")
        evidence.mkdir(parents=True)
        args = SimpleNamespace(skip_build=True, binary=str(stub), run_timeout=120,
                               build_timeout=1200)
        with patch.object(driver, "run_parity_case", return_value=0), patch.object(
                driver, "run_metadata_case", return_value=0), patch.object(
                driver, "run_extract_case", return_value=0), patch.object(
                driver, "run_inventory_case", return_value=0), patch.object(
                driver, "run_batch_case", return_value=0), patch.object(
                driver, "run_status_case", return_value=0), patch.object(
                driver, "run_performance_case", return_value=0):
            exit_code = driver.run_all_case(args, evidence)
        self.assertEqual(exit_code, 1)
        aggregate = sorted(evidence.glob("all-*/result.json"))
        self.assertEqual(len(aggregate), 1)
        payload = json.loads(aggregate[0].read_text())
        statuses = {entry["case"]: entry["status"] for entry in payload["per_case"]}
        self.assertEqual(statuses["baseline"], "FAIL")
        for case in ("parity", "metadata", "extract", "inventory", "batch",
                     "status", "performance"):
            self.assertEqual(statuses[case], "PASS", case)


class ExtractHelperTests(unittest.TestCase):
    """Focused V-SELECT report-oracle checks without a Rust build."""

    def test_extract_report_checks_exact_content_and_detects_regression(self):
        """Verify native-unit checks use independently supplied text and indices.

        Accepts no parameters and constructs one canonical report in memory.
        Returns unit after accepting the exact oracle and rejecting changed text.
        Raises AssertionError if schema, status, selection, or content failures hide.
        """
        report = {
            "schema_version": 2, "kind": "extract",
            "metadata": {"status": "complete"},
            "selection": {"unit": "logical_line", "available": {"value": 3},
                          "returned_indices": [2, 3], "omitted": "selection"},
            "content": {"units": [
                {"unit_index": 2, "text": "second"},
                {"unit_index": 3, "text": "third"},
            ]},
            "completeness": "partial",
        }
        expected = {"exit": 0, "actual_exit": 0, "status": "complete",
                    "unit": "logical_line", "available": 3, "indices": [2, 3],
                    "texts": ["second", "third"], "completeness": "partial",
                    "omitted": "selection"}
        checks = driver.extract_report_checks(report, expected)
        self.assertTrue(all(check["ok"] for check in checks), checks)
        report["content"]["units"][0]["text"] = "wrong"
        checks = driver.extract_report_checks(report, expected)
        self.assertEqual({check["id"] for check in checks if not check["ok"]},
                         {"independent_exact_text"})


class OptInGuardTests(unittest.TestCase):
    """Reproducibility guards block only through their explicit opt-in flags."""

    def test_resource_pin_blocks_only_when_required(self):
        """Verify the pinned-resource guard stays evidence-only by default.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking an invalid snapshot records the pin as
        not required without raising, and raises DriverError only when
        --require-pinned-resources is supplied.
        """
        invalid = {"resource_limits": {"valid": False}}
        result = dict(invalid)
        self.assertFalse(driver.enforce_resource_pin(
            result, SimpleNamespace(require_pinned_resources=False)))
        self.assertFalse(result["resource_pin_required"])
        with self.assertRaises(driver.DriverError):
            driver.enforce_resource_pin(
                dict(invalid), SimpleNamespace(require_pinned_resources=True))
        pinned = {"resource_limits": {"valid": True}}
        self.assertTrue(driver.enforce_resource_pin(
            pinned, SimpleNamespace(require_pinned_resources=True)))

    def test_base_revision_blocks_only_when_required(self):
        """Verify the base-revision guard records the actual revision by default.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking a differing revision proceeds by default
        and raises DriverError only when --require-base-revision is supplied.
        """
        mismatch = {"upstream_before": {"revision": "actual"},
                    "handoff": {"source": {"revision": "authorized"}}}
        driver.enforce_base_revision(
            dict(mismatch), SimpleNamespace(require_base_revision=False), "B-EXTRACT")
        with self.assertRaises(driver.DriverError):
            driver.enforce_base_revision(
                dict(mismatch), SimpleNamespace(require_base_revision=True), "B-EXTRACT")
        match = {"upstream_before": {"revision": "authorized"},
                 "handoff": {"source": {"revision": "authorized"}}}
        self.assertTrue(driver.enforce_base_revision(
            match, SimpleNamespace(require_base_revision=True), "B-NAME"))

    def test_cli_accepts_the_opt_in_guard_flags(self):
        """Verify the command line accepts both reproducibility flags.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking argparse accepts the flags and still
        rejects an unknown case name. Raises AssertionError when either flag
        is reported as an unrecognized argument.
        """
        completed = run_driver("bogus", new_evidence_dir(),
                               "--require-pinned-resources", "--require-base-revision")
        self.assertEqual(completed.returncode, 2, completed.stderr)
        self.assertIn("invalid choice", completed.stderr)
        self.assertNotIn("unrecognized arguments", completed.stderr)


class ParityNetworkModeTests(unittest.TestCase):
    """Parity Cargo subprocesses stay offline unless CI-network is explicit."""

    def test_permission_defaults_offline_and_honors_inherited_offline(self):
        """Verify the network permission resolves offline unless explicitly opted in.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking the default is offline, --ci-network
        permits network, and an inherited CARGO_NET_OFFLINE=true forces
        offline even with the flag.
        """
        with patch.dict(os.environ, {"CARGO_NET_OFFLINE": ""}):
            self.assertEqual(driver.parity_network_permission(
                SimpleNamespace(ci_network=False)), (False, "default_offline"))
            self.assertEqual(driver.parity_network_permission(
                SimpleNamespace(ci_network=True)), (True, "ci_network_opt_in"))
        with patch.dict(os.environ, {"CARGO_NET_OFFLINE": "true"}):
            self.assertEqual(driver.parity_network_permission(
                SimpleNamespace(ci_network=True)),
                (False, "inherited_explicit_offline"))
        with patch.dict(os.environ, {"CARGO_NET_OFFLINE": "false"}):
            self.assertEqual(driver.parity_network_permission(
                SimpleNamespace(ci_network=True)), (True, "ci_network_opt_in"))

    def test_parity_build_offline_by_default_and_networked_with_permission(self):
        """Verify parity_cargo_build keeps --offline unless CI-network is permitted.

        Accepts no parameters beyond the unittest case and mocks every
        subprocess and filesystem boundary.
        Returns unit after checking the build argv and environment for both
        modes. Raises AssertionError when a default build gains network or a
        permitted build stays offline.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)

        def fake_execute_factory(calls):
            def fake_execute(argv, log_path, timeout, started, cwd=None,
                             environment=None, input_bytes=None):
                calls.append({"argv": list(argv), "env": dict(environment or {})})
                return {"status": 0, "log": str(log_path)}
            return fake_execute

        with patch.object(driver, "build_environment", return_value=({}, {})), \
                patch.object(driver, "ensure_dir"), patch.object(
                driver, "parity_execute", side_effect=fake_execute_factory([])) as execute:
            driver.parity_cargo_build(Path("/manifest/Cargo.toml"), scratch,
                                       "original-upstream-lit", 0, 60)
        offline = {"argv": execute.call_args.args[0],
                   "env": execute.call_args.kwargs["environment"]}
        self.assertIn("--offline", offline["argv"])
        self.assertEqual(offline["env"]["CARGO_NET_OFFLINE"], "true")

        with patch.object(driver, "build_environment", return_value=({}, {})), \
                patch.object(driver, "ensure_dir"), patch.object(
                driver, "parity_execute", side_effect=fake_execute_factory([])) as execute:
            record = driver.parity_cargo_build(
                Path("/manifest/Cargo.toml"), scratch, "original-glassbox",
                0, 60, networked=True)
        networked = {"argv": execute.call_args.args[0],
                     "env": execute.call_args.kwargs["environment"]}
        self.assertNotIn("--offline", networked["argv"])
        self.assertEqual(networked["env"]["CARGO_NET_OFFLINE"], "false")
        self.assertEqual(record["environment"]["CARGO_NET_OFFLINE"], "false")

    def test_inventory_probe_lock_generation_follows_the_network_permission(self):
        """Verify the inventory-probe lock generation matches the permission.

        Accepts no parameters beyond the unittest case and mocks the probe
        crate creation, subprocess, and filesystem boundaries.
        Returns unit after checking both the lock argv and the build argv in
        offline and networked modes.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        calls = []

        def fake_execute(argv, log_path, timeout, started, cwd=None,
                         environment=None, input_bytes=None):
            calls.append({"argv": list(argv), "env": dict(environment or {})})
            return {"status": 0, "log": str(log_path)}

        for networked, lock_offline in ((False, True), (True, False)):
            calls.clear()
            with patch.object(driver, "create_inventory_probe",
                              return_value=Path("/probe/Cargo.toml")), patch.object(
                    driver, "build_environment", return_value=({}, {})), patch.object(
                    driver, "ensure_dir"), patch.object(
                    driver, "parity_execute", side_effect=fake_execute):
                driver.prepare_inventory_probe(
                    Path("/upstream"), scratch / "probe", scratch,
                    "original-upstream-inventory", 0, 60, networked=networked)
            lock, build = calls[0], calls[1]
            self.assertEqual(lock["argv"][0:1], ["cargo"], lock)
            self.assertEqual("--offline" in lock["argv"], lock_offline)
            self.assertEqual("--offline" in build["argv"], lock_offline)
            self.assertEqual(lock["env"]["CARGO_NET_OFFLINE"],
                             "false" if networked else "true")
            self.assertEqual(build["env"]["CARGO_NET_OFFLINE"],
                             "false" if networked else "true")

    def test_cli_scopes_the_ci_network_permission_to_parity_and_metadata(self):
        """Verify --ci-network is rejected outside its permitted cases.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking the extract case rejects the permission
        and the parity case accepts it while failing later on an unrelated
        bounded argument error. Raises AssertionError for scope leaks.
        """
        rejected = run_driver("extract", new_evidence_dir(), "--ci-network")
        self.assertEqual(rejected.returncode, 2, rejected.stderr)
        self.assertIn("--ci-network applies only", rejected.stderr)
        for case in ("parity", "metadata"):
            accepted = run_driver(case, new_evidence_dir(),
                                  "--ci-network", "--perf-replicates", "3")
            self.assertEqual(accepted.returncode, 2, accepted.stderr)
            self.assertIn("at least 5", accepted.stderr)
            self.assertNotIn("--ci-network applies only", accepted.stderr)


class ParityHelperTests(unittest.TestCase):
    """Focused V-NAME helpers that require no Rust compilation."""

    def test_campaigns_isolate_targets_and_bound_dependency_reuse(self):
        """Verify campaign isolation and same-family dependency-cache reuse."""
        first = driver.parity_build_target(Path("/evidence/run-one"), "original-upstream-lit")
        second = driver.parity_build_target(Path("/evidence/run-one"), "mutated-upstream-lit")
        third = driver.parity_build_target(Path("/evidence/run-two"), "original-upstream-lit")
        glassbox = driver.parity_build_target(Path("/evidence/run-one"), "original-glassbox")
        self.assertEqual(first, second)
        self.assertNotEqual(first, third)
        self.assertNotEqual(first, glassbox)
        for target in (first, second, third, glassbox):
            self.assertIn(driver.PRODUCTION_BUILD_TARGET, target.parents)

    def test_parity_gate_rejects_meaningful_output_changes(self):
        """Verify the parity comparator fails processing-output rewrites.

        Accepts no parameters and supplies exact, branded, and changed byte streams.
        Returns unit after checking all three classifications.
        Raises AssertionError if a meaningful output change could pass as branding.
        """
        self.assertEqual(driver.classify_parity_stream(b"same", b"same"), "exact")
        self.assertEqual(driver.classify_parity_stream(
            b"Usage: lit parse", b"Usage: glassbox parse", True),
            "command_identity_branding_only")
        self.assertEqual(driver.classify_parity_stream(
            b"parsed lit content", b"parsed glassbox content", True),
            "meaningful_difference")

    def test_branding_preserves_processing_attribution(self):
        """Verify branding leaves liteparse processing attribution untouched.

        Accepts no parameters and transforms one representative processing stream.
        Returns unit after checking only the command identity changed.
        Raises AssertionError when attribution or content receives a branding rewrite.
        """
        stream = b"Usage: lit parse\n[liteparse] parse complete\n"
        self.assertEqual(driver.brand_expected(stream),
                         b"Usage: glassbox parse\n[liteparse] parse complete\n")

    def test_future_mutation_is_isolated_and_automatic(self):
        """Verify future command, alias, and flag anchors change only a source copy.

        Accepts no parameters and creates one synthetic source tree under the test sandbox.
        Returns unit after checking copied mutations and the real upstream source hash.
        Raises AssertionError for missing future definitions or real upstream modification.
        """
        sandbox = new_sandbox_dir()
        copied_root = sandbox / "upstream"
        source_dir = copied_root / "crates/liteparse/src"
        source_dir.mkdir(parents=True)
        real_source = driver.REPO / "crates/liteparse/src/main.rs"
        before = driver.sha256_file(real_source)
        shutil.copy2(real_source, source_dir / "main.rs")
        record = driver.mutate_upstream(copied_root)
        mutated = (source_dir / "main.rs").read_text()
        self.assertIn("FutureProbe(FutureProbeCommand)", mutated)
        self.assertIn('alias = "future-alias"', mutated)
        self.assertIn("future_parse_option", mutated)
        self.assertEqual(record["sha256"], driver.sha256_file(source_dir / "main.rs"))
        self.assertEqual(driver.sha256_file(real_source), before)

    def test_package_and_lock_identity_is_reconciled(self):
        """Verify renamed packages and reviewed dependency versions are locked.

        Accepts no parameters and reads the current downstream manifests and lockfile.
        Returns unit after checking every package-identity assertion.
        Raises AssertionError with failed check IDs for naming or dependency drift.
        """
        record = driver.verify_package_identity()
        failed = [check["id"] for check in record["checks"] if not check["ok"]]
        self.assertFalse(failed, failed)
        self.assertEqual(record["sha2_version"], "0.10.9")

class MetadataHelperTests(unittest.TestCase):
    """Focused V-META oracle and isolated-instrumentation checks without Cargo."""

    def test_metadata_report_uses_independent_digest_and_rejects_body_fields(self):
        """Verify common report checks use file bytes rather than implementation claims.

        Accepts no parameters beyond the unittest case and creates one sandbox input.
        Returns unit after checking a valid report and two deliberate failures.
        Raises AssertionError if digest/count mismatches or legacy fields pass.
        """
        sandbox = new_sandbox_dir()
        sandbox.mkdir(parents=True)
        path = sandbox / "input.txt"
        payload = b"body sentinel must not be reported\n"
        path.write_bytes(payload)
        report = {
            "schema_version": 2,
            "kind": "inspect",
            "metadata": {
                "status": "complete",
                "digest": {
                    "digest": driver.sha256_bytes(payload),
                    "bytes_hashed": {"unit": "byte", "value": len(payload)},
                    "snapshot_guarantee": "not_immutable",
                },
            },
        }
        checks = driver.verify_metadata_report(report, path, "complete")
        self.assertTrue(all(check["ok"] for check in checks), checks)
        report["metadata"]["text"] = {"content": "forbidden"}
        report["metadata"]["digest"]["digest"] = "0" * 64
        checks = driver.verify_metadata_report(report, path, "complete")
        failed = {check["id"] for check in checks if not check["ok"]}
        self.assertEqual(failed, {"independent_sha256", "no_body_or_legacy_fields"})

    def test_metadata_fixture_matrix_covers_every_required_family(self):
        """Verify the driver prepares all required families and boundary inputs.

        Accepts no parameters beyond the unittest case and builds a fresh isolated oracle.
        Returns unit after checking family membership and independent XLSM packaging.
        Raises AssertionError for missing families or an unchanged OOXML main type.
        """
        import zipfile
        sandbox = new_sandbox_dir()
        oracle = driver.build_fixtures(driver.REPO, sandbox / "oracle")
        fixtures = driver.metadata_case_fixtures(oracle, sandbox)
        families = {fixture["family"] for fixture in fixtures}
        required = {
            "pdf", "docx", "pptx", "xlsx", "xlsm", "odt", "odp", "ods",
            "epub", "mobi", "iwork-pages", "iwork-numbers", "iwork-key",
            "zip", "cbz", "cbr", "csv", "tsv", "json", "xml", "plain",
            "source", "raster", "djvu", "parquet", "unknown", "corrupt",
        }
        self.assertTrue(required <= families, required - families)
        xlsm = Path(next(fixture["path"] for fixture in fixtures
                         if fixture["family"] == "xlsm"))
        with zipfile.ZipFile(xlsm) as package:
            types = package.read("[Content_Types].xml")
        self.assertIn(b"application/vnd.ms-excel.sheet.macroEnabled.main+xml", types)

    def test_metadata_early_blocker_does_not_claim_upstream_mutation(self):
        """Verify a missing before snapshot yields BLOCKED, not a mutation failure.

        Accepts no parameters and forces the opt-in pinned-resource guard to
        fail before any fixture, binary, or historical prerequisite is
        accessed. Returns unit after checking the retained result. A false
        upstream-change failure, PASS, or missing not-assessed marker raises
        AssertionError.
        """
        evidence = new_evidence_dir()
        evidence.mkdir(parents=True)
        args = SimpleNamespace(skip_build=True, binary="unused", run_timeout=1,
                               build_timeout=1, require_pinned_resources=True)
        with patch.object(driver, "resource_limit_snapshot",
                          return_value={"valid": False}), patch.object(
                driver, "upstream_source_snapshot", return_value={"revision": "after"}):
            status = driver.run_metadata_case(args, evidence)
        self.assertEqual(status, 2)
        result_path = next(evidence.glob("metadata-*/result.json"))
        result = json.loads(result_path.read_text(encoding="utf-8"))
        self.assertEqual(result["status"], "BLOCKED")
        self.assertEqual(result["failures"], [])
        self.assertIsNone(result["upstream_unchanged"])
        self.assertEqual(result["upstream_assessment"],
                         "not_assessed_missing_before_snapshot")
        self.assertFalse(any("checkout changed" in blocker["detail"]
                             for blocker in result["blockers"]))
        self.assertTrue(any("guard is not effective" in blocker["detail"]
                            for blocker in result["blockers"]))

    def test_metadata_default_run_proceeds_without_the_pinned_host(self):
        """Verify default verification proceeds on an unpinned standard runner.

        Accepts no parameters and forces the resource snapshot invalid while
        stopping fixture construction with a sentinel error. Returns unit
        after checking the case passed both opt-in guards and that the
        recorded evidence marks the pin as not required. A pin blocker or a
        missing sentinel blocker raises AssertionError.
        """
        evidence = new_evidence_dir()
        evidence.mkdir(parents=True)
        args = SimpleNamespace(skip_build=True, binary="unused", run_timeout=1,
                               build_timeout=1)
        with patch.object(driver, "resource_limit_snapshot",
                          return_value={"valid": False}), patch.object(
                driver, "upstream_source_snapshot",
                return_value={"revision": "other"}), patch.object(
                driver, "build_fixtures",
                side_effect=driver.DriverError("sentinel-after-guards")):
            status = driver.run_metadata_case(args, evidence)
        self.assertEqual(status, 2)
        result_path = next(evidence.glob("metadata-*/result.json"))
        result = json.loads(result_path.read_text(encoding="utf-8"))
        self.assertEqual(result["status"], "BLOCKED")
        self.assertFalse(result["resource_pin_required"])
        self.assertFalse(result["base_revision_required"])
        self.assertEqual(result["upstream_before"]["revision"], "other")
        self.assertTrue(any(blocker["detail"] == "sentinel-after-guards"
                            for blocker in result["blockers"]))
        self.assertFalse(any("guard is not effective" in blocker["detail"]
                             for blocker in result["blockers"]))
        self.assertFalse(any("differs from the authorized" in blocker["detail"]
                              for blocker in result["blockers"]))

    def test_instrumentation_changes_only_the_isolated_source_copy(self):
        """Verify every forbidden-stage trap installs without touching real upstream.

        Accepts no parameters beyond the unittest case and creates minimal copied sources.
        Returns unit after checking nine traps, markers, and real-source hash stability.
        Raises AssertionError for missing traps or any real upstream mutation.
        """
        root = new_sandbox_dir() / "instrumented"
        files = {
            "crates/liteparse/src/stages.rs": (
                "pub fn extract(\n) {}\n"
                "pub fn page_complexity(\n) {}\n"
                "pub fn render_for_ocr(\n) {}\n"
                "pub fn merge_ocr(\n) {}\n"
                "pub fn project(pages: Vec<u8>) {}\n"
            ),
            "crates/liteparse/src/extract.rs": (
                "pub fn extract_pages_from_input(\n) {}\n"
                "pub(crate) fn extract_pages_from_document(\n) {}\n"
            ),
            "crates/liteparse/src/ocr/tesseract.rs": (
                "impl X {\n    fn recognize<'a, 'b: 'a, 'c: 'a>(\n    ) {}\n}\n"
            ),
            "crates/pdfium/src/page.rs": "impl Page {\n    pub fn text(&self) -> TextPage {\n    }\n}\n",
            "extensionalities/crates/litx-inspect/src/lib.rs": "fn inspect_with_session(\n) {}\n",
        }
        for relative, body in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body)
        real = driver.REPO / "crates/liteparse/src/stages.rs"
        before = driver.sha256_file(real)
        record = driver.instrument_metadata_forbidden_stages(root)
        self.assertEqual(record["trap_count"], 9)
        for relative in files:
            self.assertIn("V-META-FORBIDDEN", (root / relative).read_text())
        stages = (root / "crates/liteparse/src/stages.rs").read_text()
        self.assertIn('pub fn extract(\n) {\n    panic!("V-META-FORBIDDEN-STAGES-EXTRACT");', stages)
        tesseract = (root / "crates/liteparse/src/ocr/tesseract.rs").read_text()
        self.assertLess(tesseract.index("fn recognize"), tesseract.index("V-META-FORBIDDEN"))
        self.assertIn(") {\n        panic!", tesseract)
        self.assertEqual(driver.sha256_file(real), before)


class StubBinaryTests(unittest.TestCase):
    """Baseline case driven by stub binaries: no Rust build required."""

    def setUp(self):
        self.evidence = new_evidence_dir()
        self.stubs = new_sandbox_dir() / "stubs"
        self.stubs.mkdir(parents=True)

    def read_result(self, evidence):
        results = sorted(evidence.glob("baseline-*/result.json"))
        self.assertEqual(len(results), 1, f"expected one result.json in {evidence}")
        return results[0], json.loads(results[0].read_text())

    def test_blocks_on_missing_executable(self):
        missing = self.stubs / "does-not-exist"
        completed = run_driver("baseline", self.evidence,
                               "--skip-build", "--binary", str(missing))
        self.assertEqual(completed.returncode, 2, completed.stdout + completed.stderr)
        self.assertIn("missing", completed.stdout)
        _, payload = self.read_result(self.evidence)
        self.assertEqual(payload["status"], "BLOCKED")
        self.assertTrue(any("executable" in blocker["detail"]
                           for blocker in payload["blockers"]))

    def test_records_nonzero_child_exit_as_failure(self):
        stub = write_stub(self.stubs, "exit-seven", "import sys\nsys.exit(7)\n")
        completed = run_driver("baseline", self.evidence,
                               "--skip-build", "--binary", str(stub))
        self.assertEqual(completed.returncode, 1, completed.stdout)
        _, payload = self.read_result(self.evidence)
        self.assertEqual(payload["status"], "FAIL")
        defaults = next(run for run in payload["runs"]
                        if run["name"] == "defaults_first_call")
        self.assertEqual(defaults["status"], 7)
        self.assertTrue(any(check["id"] == "exit_zero" and not check["ok"]
                            for check in defaults["checks"]))

    def test_timeout_is_enforced_and_recorded(self):
        stub = write_stub(self.stubs, "sleep-long", "import time\ntime.sleep(60)\n")
        started = time.monotonic()
        completed = run_driver("baseline", self.evidence,
                              "--skip-build", "--binary", str(stub),
                              "--run-timeout", "2")
        elapsed = time.monotonic() - started
        self.assertLess(elapsed, 90)
        self.assertEqual(completed.returncode, 1, completed.stdout)
        _, payload = self.read_result(self.evidence)
        defaults = next(run for run in payload["runs"]
                        if run["name"] == "defaults_first_call")
        self.assertTrue(defaults["timed_out"])
        self.assertTrue(any(check["id"] == "no_timeout" and not check["ok"]
                            for check in defaults["checks"]))

    def test_result_json_integrity_and_no_overwrite(self):
        stub = write_stub(self.stubs, "junk-json",
                          "print('this is not json')\n")
        first = run_driver("baseline", self.evidence,
                          "--skip-build", "--binary", str(stub))
        self.assertEqual(first.returncode, 1, first.stdout)
        first_path, first_payload = self.read_result(self.evidence)
        first_bytes = first_path.read_bytes()
        self.assertEqual(first_payload["case"], "baseline")
        self.assertEqual(first_payload["verification_id"], "V-SETUP")
        for key in ("environment", "git", "source", "safety_caps",
                    "fixtures", "runs", "failures", "blockers", "status"):
            self.assertIn(key, first_payload)
        self.assertIn("tree_sha256", first_payload["source"])
        self.assertTrue(first_payload["runs"])
        # Fixture oracle must exist and carry provenance and expectations.
        oracle_path = Path(first_payload["fixtures"]["oracle"])
        self.assertTrue(oracle_path.is_file())
        oracle = json.loads(oracle_path.read_text())
        names = {record["name"] for record in oracle["fixtures"]}
        self.assertTrue({"digital-4p.pdf", "scanned-1p.pdf", "mixed-2p.pdf",
                         "blank-1p.pdf", "zero-page.pdf", "plain-lines.txt",
                         "data.json", "doc.xml", "big.txt"} <= names)
        scanned = next(record for record in oracle["fixtures"]
                       if record["name"] == "scanned-1p.pdf")
        self.assertEqual(scanned["expects"]["ocr_words"],
                         driver.OCR_WORDS)
        # Safety caps read from live source, not assumed values.
        caps = first_payload["safety_caps"]["constants"]
        max_pages = [cap for cap in caps
                     if cap.get("name") == "MAX_PAGES" and "pdf.rs" in cap.get("file", "")]
        self.assertEqual(len(max_pages), 2)
        by_cfg = {cap["cfg"]: cap["value"] for cap in max_pages}
        self.assertEqual(by_cfg.get("not(test)"), "4096")
        self.assertEqual(by_cfg.get("test"), "8")
        # A second run creates a new run dir and never touches the first.
        second = run_driver("baseline", self.evidence,
                            "--skip-build", "--binary", str(stub))
        self.assertEqual(second.returncode, 1, second.stdout)
        self.assertEqual(first_path.read_bytes(), first_bytes)
        results = sorted(self.evidence.glob("baseline-*/result.json"))
        self.assertEqual(len(results), 2)

    def test_evidence_runs_stay_inside_requested_dir(self):
        stub = write_stub(self.stubs, "exit-seven", "import sys\nsys.exit(7)\n")
        completed = run_driver("baseline", self.evidence,
                               "--skip-build", "--binary", str(stub))
        self.assertEqual(completed.returncode, 1, completed.stdout)
        run_dirs = [path for path in self.evidence.iterdir() if path.is_dir()]
        self.assertTrue(all(path.name.startswith("baseline-")
                             for path in run_dirs), run_dirs)


class FixtureOracleTests(unittest.TestCase):
    def test_synthetic_fixture_oracle_is_self_consistent(self):
        sandbox = new_sandbox_dir()
        oracle = driver.build_fixtures(Path(driver.REPO), sandbox)
        fixtures = {record["name"]: record for record in oracle["fixtures"]}
        digital = fixtures["digital-4p.pdf"]
        self.assertEqual(digital["expects"]["pages"], 4)
        pdf_bytes = Path(digital["path"]).read_bytes()
        self.assertTrue(pdf_bytes.startswith(b"%PDF-1.7"))
        self.assertEqual(driver.sha256_bytes(pdf_bytes), digital["sha256"])
        self.assertIn(b"DIGITAL PAGE 2 SENTINEL 2026", pdf_bytes)
        # Deterministic writer: identical inputs give identical bytes.
        rebuilt = driver.build_pdf(
            [{"text": f"DIGITAL PAGE {i} SENTINEL 2026"} for i in range(1, 5)])
        self.assertEqual(rebuilt, pdf_bytes)
        # Scanned fixture embeds the rasterized known-text image.
        scanned = fixtures["scanned-1p.pdf"]
        self.assertEqual(scanned["expects"]["ocr_words"], driver.OCR_WORDS)
        scanned_bytes = Path(scanned["path"]).read_bytes()
        self.assertNotIn(b"SENTINEL 2026 (GLASSBOX", scanned_bytes)
        # Generator copy parity against the tracked fixture directory.
        parity = oracle["provenance"]["generator_parity"]
        compared = [entry for entry in parity
                    if entry["identical_to_tracked"] is not None]
        self.assertTrue(compared)
        self.assertTrue(all(entry["identical_to_tracked"] for entry in compared),
                         [e["name"] for e in compared
                          if not e["identical_to_tracked"]])

    def test_exclusive_write_refuses_overwrite(self):
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True, exist_ok=True)
        target = scratch / "exclusive.txt"
        driver.write_exclusive(target, b"first")
        with self.assertRaises(driver.DriverError):
            driver.write_exclusive(target, b"second")
        self.assertEqual(target.read_bytes(), b"first")

    def test_png_decode_handles_filters(self):
        import struct
        import zlib
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True, exist_ok=True)
        width, height = 4, 3
        # Build a tiny RGB PNG whose rows use filters 0, 1, and 2.
        rows = [bytes([0]) + bytes([255, 0, 0] * width),
                bytes([1]) + bytes([1, 2, 3] * width),
                bytes([2]) + bytes([10, 20, 30] * width)]
        def chunk(kind, data):
            return (struct.pack(">I", len(data)) + kind + data
                    + struct.pack(">I", zlib.crc32(kind + data)))
        png = (b"\x89PNG\r\n\x1a\n"
               + chunk(b"IHDR", struct.pack(">IIBBBBB",
                                            width, height, 8, 2, 0, 0, 0))
               + chunk(b"IDAT", zlib.compress(b"".join(rows)))
               + chunk(b"IEND", b""))
        path = scratch / "tiny.png"
        path.write_bytes(png)
        decoded_width, decoded_height, rgb = driver.decode_png_rgb(path)
        self.assertEqual((decoded_width, decoded_height), (width, height))
        self.assertEqual(len(rgb), width * height * 3)
        self.assertEqual(rgb[0:3], bytes([255, 0, 0]))
        # Filter 1 (Sub) reconstructs by adding the left neighbor.
        self.assertEqual(rgb[width * 3:width * 3 + 3], bytes([1, 2, 3]))
        # Filter 2 (Up) reconstructs by adding the row above.
        self.assertEqual(rgb[width * 6:width * 6 + 3],
                         bytes([(10 + 1) & 0xFF, (20 + 2) & 0xFF,
                                (30 + 3) & 0xFF]))


class PerformanceCaseTests(unittest.TestCase):
    """Focused V-PERF helper checks that require no Rust compilation."""

    @staticmethod
    def synthetic_fixtures(root):
        """Build one synthetic frozen fixture map for scenario definitions.

        Accepts a sandbox root path. Returns the fixture map shape consumed
        by perf_scenario_definitions without touching real fixtures.
        """
        pages = ([("exact", f"PERF MIXED NATIVE {index} 2026")
                  for index in range(1, 7)] +
                 [("lines", [f"PERF DENSE PAGE 7 LINE {line} TOKEN 2026"
                             for line in range(1, 26)])
                  for _ in range(5)] +
                 [("empty", None) for _ in range(5)])
        return {
            "tiny6": {"path": str(root / "inputs/tiny6.pdf"),
                      "bytes": 100, "sha256": "a" * 64},
            "mixed16": {"path": str(root / "inputs/mixed16.pdf"),
                        "bytes": 200, "sha256": "b" * 64, "pages": pages},
            "pages4096": {"path": str(root / "inputs/pages4096.pdf"),
                          "bytes": 300, "sha256": "c" * 64},
            "big14mib": {"path": str(root / "inputs/big14mib.txt"),
                         "bytes": 14 * 1024 * 1024, "sha256": "d" * 64,
                         "content": "PERFPLAIN7Q"},
            "sixmib": {"path": str(root / "inputs/sixmib.txt"),
                       "bytes": 6 * 1024 * 1024, "sha256": "e" * 64},
            "huge255mib": {"path": str(root / "inputs/huge255mib.bin"),
                           "bytes": 255 * 1024 * 1024, "sha256": "f" * 64},
            "small-plain.txt": {"path": str(root / "dir/plain.txt"),
                                "bytes": 1, "sha256": "0" * 64},
            "pocr-reuse": {
                "dense-large-small-text.pdf": {
                    "path": str(root / "dense.pdf"), "bytes": 1,
                    "sha256": "1" * 64,
                    "expected_phrases": ["FIELD TEAM LINE"]},
                "scanned-4p.pdf": {
                    "path": str(root / "scanned.pdf"), "bytes": 1,
                    "sha256": "2" * 64,
                    "expected_phrases": ["SCANNED PAGE ALPHA"]},
            },
        }

    def test_candidate_factors_are_bounded_and_factor_only(self):
        """Verify at most two fixed candidates without panic changes.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking the frozen candidate registry.
        Raises AssertionError when a candidate count, factor, or panic
        setting escapes the approved bounded matrix.
        """
        self.assertLessEqual(len(driver.PERF_DEFAULT_CANDIDATES), 2)
        self.assertEqual(driver.PERF_DEFAULT_CANDIDATES,
                         ("thin-lto", "thin-lto-cgu1"))
        for values in driver.PERF_CANDIDATE_FACTORS.values():
            for value in values:
                self.assertIn("profile.release.", value)
                self.assertNotIn("panic", value)
                self.assertNotIn("abort", value)

    def test_frozen_reuse_hashes_match_accepted_records(self):
        """Verify reused P-OCR digests and provenance constants stay frozen.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking spike-recorded hashes and the historical
        revision constant.
        Raises AssertionError when any frozen provenance constant drifts.
        """
        self.assertEqual(driver.PERF_POCR_FROZEN_SHA[
            "scanned-4p.pdf"],
            "af937311d99d05b472be51caf995b1fd75b0f797de728564749783a76008b22e")
        self.assertEqual(driver.PERF_POCR_FROZEN_SHA[
            "dense-large-small-text.pdf"],
            "ed7a63d4f0a96f848f789e736e329727ac812070e6cbb39e383141d40c2ceef0")
        self.assertEqual(driver.PERF_POCR_FROZEN_SHA["mixed-same-page.pdf"],
                         "eda8c7d0a14fd9eeb191d2becd051a683c00fc7f7060017b93134ff555d5e777")
        self.assertEqual(driver.PERF_ACCEPTED_PRODUCTION_SHA,
                         "c72c9928550cf28ae14f7a349ad1975788be86842cf9b44c3f0852bdb6af97c8")
        self.assertEqual(driver.PERF_HISTORICAL_REVISION,
                         "57e3afb7f1d59ca93811eda5748aa917613f64fc")
        self.assertGreaterEqual(driver.PERF_MIN_REPLICATES, 5)

    def test_scenario_definitions_are_frozen_and_bounded(self):
        """Verify the frozen scenario matrix covers the required coverage set.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking scenario names, replicates, arguments,
        jobs values, heavy inputs, and hash coverage.
        Raises AssertionError when required coverage is missing or
        replicate bounds are weakened.
        """
        fixtures = self.synthetic_fixtures(new_sandbox_dir())
        scenarios = driver.perf_scenario_definitions(fixtures, 5)
        by_name = {scenario["name"]: scenario for scenario in scenarios}
        for name in ("inspect-tiny6", "extract-tiny6-default",
                     "extract-tiny6-full", "inspect-mixed16",
                     "extract-mixed16-full", "extract-paged4096",
                     "extract-big14mib", "inspect-big14mib",
                     "inspect-huge255mib", "extract-ocr-dense",
                     "extract-ocr-scanned4p", "parse-tiny6"):
            self.assertIn(name, by_name)
        for jobs in (1, 2, 4):
            self.assertIn(f"extract-ocr-scanned4p-jobs{jobs}", by_name)
            self.assertIn(f"extract-batch-smallset-jobs{jobs}", by_name)
            self.assertIn(f"inspect-inventory-jobs{jobs}", by_name)
        self.assertIn("extract-batch-sixmib-jobs1", by_name)
        for scenario in scenarios:
            if "-jobs" in scenario["name"]:
                self.assertEqual(scenario["replicates"], 1,
                                 scenario["name"])
            else:
                self.assertGreaterEqual(scenario["replicates"], 5,
                                        scenario["name"])
        tiny_full = by_name["extract-tiny6-full"]
        self.assertEqual(tiny_full["argv"][tiny_full["argv"].index("--pages") + 1],
                         "1-6")
        self.assertEqual(tiny_full["expect"]["unit"], "physical_page")
        self.assertEqual(tiny_full["expect"]["available"], 6)
        self.assertEqual(tiny_full["expect"]["indices"], [1, 2, 3, 4, 5, 6])
        tiny_default = by_name["extract-tiny6-default"]
        self.assertEqual(tiny_default["expect"]["unit"], "physical_page")
        self.assertEqual(tiny_default["expect"]["indices"],
                         [1, 2, 3, 4, 5, 6])
        self.assertEqual(tiny_default["expect"]["available"], 6)
        parse = by_name["parse-tiny6"]
        self.assertEqual(parse["expect"]["total_pages"], 6)
        self.assertEqual(len(parse["expect"]["phrases"]), 6)
        mixed = by_name["extract-mixed16-full"]
        self.assertEqual(mixed["expect"]["empty_pages"],
                         [11, 12, 13, 14, 15])
        self.assertIn(7, mixed["expect"]["contain_lines"])
        paged = by_name["extract-paged4096"]
        self.assertEqual(paged["expect"]["available"], 4096)
        self.assertIn("contain_lines", paged["expect"])
        self.assertNotIn("exact_texts", paged["expect"])
        huge = by_name["inspect-huge255mib"]
        self.assertEqual(huge["expect"]["exit"], 2)
        self.assertEqual(huge["expect"]["status"], "unsupported")
        big14 = by_name["extract-big14mib"]
        self.assertEqual(big14["expect"]["exact_text"], "PERFPLAIN7Q")
        dense = by_name["extract-ocr-dense"]
        self.assertEqual(dense["expect"]["phrases"], ["FIELD TEAM LINE"])
        batch = by_name["extract-batch-sixmib-jobs1"]
        self.assertEqual(batch["expect"]["retention"], {7: "sixmib"})
        self.assertTrue(batch["input_bytes"])

    def test_summarize_reports_median_and_worst_case(self):
        """Verify summaries keep the worst replicate alongside the median.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking median, spread, and resource maxima on
        synthetic measured records.
        Raises AssertionError when the worst case can be hidden.
        """
        records = [{"wall_s": 1.0, "cpu_s": 0.5, "peak_rss_kb": 100,
                    "io_read_bytes": 10},
                   {"wall_s": 3.0, "cpu_s": 2.0, "peak_rss_kb": 300,
                    "io_read_bytes": 30},
                   {"wall_s": 2.0, "cpu_s": 1.0, "peak_rss_kb": 200,
                    "io_read_bytes": 20}]
        summary = driver.perf_summarize(records)
        self.assertEqual(summary["replicates"], 3)
        self.assertEqual(summary["median_wall_s"], 2.0)
        self.assertEqual(summary["min_wall_s"], 1.0)
        self.assertEqual(summary["max_wall_s"], 3.0)
        self.assertEqual(summary["max_peak_rss_kb"], 300)
        self.assertEqual(summary["max_io_read_bytes"], 30)
        self.assertEqual(driver.perf_scenario_noise(summary), 2.0)

    def test_candidate_verdict_applies_noise_rules(self):
        """Verify verdicts distinguish beyond-noise gains from regressions.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking improved, within-margin, and regressed
        candidate classifications and the bounded verdicts.
        Raises AssertionError when noise can hide a regression or produce
        an adoption decision.
        """
        baseline = {"heavy": {"median_wall_s": 10.0, "min_wall_s": 9.5,
                              "max_wall_s": 10.5}}
        improved = driver.perf_candidate_verdict(
            baseline, {"heavy": {"median_wall_s": 8.0,
                                 "min_wall_s": 7.9, "max_wall_s": 8.1}},
            ["heavy"])
        self.assertEqual(improved["verdict"], "consider-adoption")
        self.assertEqual(len(improved["improved_beyond_margin"]), 1)
        self.assertEqual(len(improved["regressed_beyond_margin"]), 0)
        within = driver.perf_candidate_verdict(
            baseline, {"heavy": {"median_wall_s": 10.0,
                                 "min_wall_s": 9.9, "max_wall_s": 10.2}},
            ["heavy"])
        self.assertEqual(within["verdict"], "retain-baseline")
        self.assertEqual(len(within["within_margin"]), 1)
        regressed = driver.perf_candidate_verdict(
            baseline, {"heavy": {"median_wall_s": 11.5,
                                 "min_wall_s": 11.4, "max_wall_s": 11.6}},
            ["heavy"])
        self.assertEqual(regressed["verdict"], "retain-baseline")
        self.assertEqual(len(regressed["regressed_beyond_margin"]), 1)
        mixed = driver.perf_candidate_verdict(
            baseline, {"heavy": {"median_wall_s": 8.0, "min_wall_s": 7.9,
                                 "max_wall_s": 8.1}},
            ["heavy", "missing-scenario"])
        self.assertEqual(mixed["verdict"], "consider-adoption")

    def test_candidate_verdict_resolution_floor_and_worst_case_tradeoffs(self):
        """Verify the resolution floor and worst-case disclosure rules.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking that a sub-floor gain is never beyond
        noise, that the 5ms resolution floor gates classification, and
        that a worst-case regression beyond the floor is disclosed as a
        tradeoff that downgrades the verdict instead of being hidden
        behind "zero regressions from medians".
        Raises AssertionError when one poll-tick gains or hidden worst-case
        regressions could drive an adoption verdict.
        """
        baseline = {"heavy": {"median_wall_s": 10.0, "min_wall_s": 10.0,
                              "max_wall_s": 10.0},
                    "other": {"median_wall_s": 0.894, "min_wall_s": 0.894,
                              "max_wall_s": 0.895}}
        # A one-tick 0.020s median gain with tiny noise stays improved only
        # above the floor; a 0.003s gain is within the 5ms floor.
        subfloor = driver.perf_candidate_verdict(
            baseline, {"heavy": {"median_wall_s": 9.997,
                                 "min_wall_s": 9.997, "max_wall_s": 9.998}},
            ["heavy"])
        self.assertEqual(subfloor["verdict"], "retain-baseline")
        self.assertEqual(len(subfloor["within_margin"]), 1)
        # Median gain 0.02s beyond max(noise=0.001, floor=0.005), but the
        # worst case regresses 0.03s beyond the floor: disclosed tradeoff.
        tradeoffs = driver.perf_candidate_verdict(
            baseline, {"heavy": {"median_wall_s": 9.98,
                                 "min_wall_s": 9.98, "max_wall_s": 10.03}},
            ["heavy"])
        self.assertEqual(tradeoffs["verdict"],
                         "consider-adoption-with-worst-case-tradeoffs")
        self.assertEqual(len(tradeoffs["improved_beyond_margin"]), 1)
        self.assertEqual(len(tradeoffs["worst_case_tradeoffs"]), 1)
        self.assertTrue(
            tradeoffs["worst_case_tradeoffs"][0]["worst_regression_beyond_floor"])
        clean = driver.perf_candidate_verdict(
            baseline, {"heavy": {"median_wall_s": 9.98,
                                 "min_wall_s": 9.98, "max_wall_s": 10.002}},
            ["heavy"])
        self.assertEqual(clean["verdict"], "consider-adoption")
        self.assertTrue(all(
            not item["worst_regression_beyond_floor"]
            for item in clean["worst_case_tradeoffs"]))
        # A scenario absent from the summaries never fabricates a verdict.
        absent = driver.perf_candidate_verdict(
            baseline, {"heavy": baseline["heavy"]}, ["heavy", "absent"])
        self.assertEqual(len(absent["worst_case_tradeoffs"]), 1)

    def test_run_measured_blocking_clock_and_timeout(self):
        """Verify the blocking wait4 clock is accurate and timeouts kill.

        Accepts no parameters beyond the unittest case.
        Returns unit after timing a 0.2s real child well below the v1 20ms
        polling quantization error window and a 5s child under a 1s
        timeout. The wall clock must complete at true child exit without
        poll-loop overshoot, and the independent sampler must SIGKILL on
        deadline with timed_out recorded.
        Raises AssertionError when short runs overshoot or timeouts are
        invented statuses.
        """
        sandbox = new_sandbox_dir()
        sandbox.mkdir(parents=True)
        run = driver.perf_run_measured(
            ["sleep", "0.2"], sandbox / "clock.stdout",
            sandbox / "clock.stderr", 30)
        self.assertEqual(run["status"], 0)
        self.assertFalse(run["timed_out"])
        self.assertGreaterEqual(run["wall_s"], 0.15)
        self.assertLess(run["wall_s"], 0.5,
                        f"wall {run['wall_s']}s overshot the 0.2s child")
        killed = driver.perf_run_measured(
            ["sleep", "5"], sandbox / "killed.stdout",
            sandbox / "killed.stderr", 1)
        self.assertTrue(killed["timed_out"])
        self.assertNotEqual(killed["status"], 0)
        self.assertLess(killed["wall_s"], 3.0)
        # A short child may exit before the sampler observes VmHWM; the
        # trace stays unobserved and is never a zero-memory claim.
        self.assertIn("rss_trace_observed", run)

    def test_attempt_history_accumulates_and_never_resets_costs(self):
        """Verify prior attempt costs accumulate and never reset silently.

        Accepts no parameters beyond the unittest case.
        Returns unit after feeding two synthetic retained attempt records
        (one FAIL, one BLOCKED) and checking their known walls sum exactly,
        their statuses survive, and missing evidence or missing cost fields
        raise DriverError instead of being dropped.
        Raises AssertionError when failed attempt costs can vanish from the
        cumulative ledger.
        """
        sandbox = new_sandbox_dir()
        sandbox.mkdir(parents=True)
        first = sandbox / "attempt-fail.json"
        second = sandbox / "attempt-blocked.json"
        first.write_text(json.dumps({
            "case": "performance", "status": "FAIL",
            "costs": {"elapsed_active_s": 79.85,
                      "historical_charged_separately_s": 320.01,
                      "total_wall_s": 400.0}}))
        second.write_text(json.dumps({
            "case": "performance", "status": "BLOCKED",
            "costs": {"elapsed_active_s": 791.25,
                      "historical_charged_separately_s": 0.0,
                      "total_wall_s": 791.2512}}))
        history = driver.perf_attempt_history([first, second])
        self.assertEqual(history["count"], 2)
        self.assertEqual(history["known_total_wall_s"], 1191.2512)
        self.assertEqual([record["status"] for record in
                          history["attempts"]], ["FAIL", "BLOCKED"])
        self.assertIn("unknown", history["note"])
        with self.assertRaises(driver.DriverError):
            driver.perf_attempt_history([sandbox / "missing.json"])
        partial = sandbox / "partial.json"
        partial.write_text(json.dumps({"case": "performance",
                                       "costs": {}}))
        with self.assertRaises(driver.DriverError):
            driver.perf_attempt_history([partial])
        with self.assertRaises(driver.DriverError):
            driver.perf_attempt_history(["attempt-fail.json"])

    def test_budget_accounting_is_cumulative_and_honest(self):
        """Verify budget accounting never retroactively blesses the campaign.

        Accepts no parameters beyond the unittest case.
        Returns unit after accumulating 1990.2853s of known prior walls
        plus a new corrective run and checking the original 30-minute
        budget overage is recorded exactly, the corrective-pass within-
        budget claim is scoped only to the approved allowance, and a
        missing allowance records no corrective claim.
        Raises AssertionError when per-run resets can make the original
        campaign compliant.
        """
        history = {"attempts": [], "count": 4,
                   "known_total_wall_s": 1990.2853}
        accounting = driver.perf_budget_accounting(history, None, 300.0)
        self.assertEqual(accounting["original_campaign_budget_s"], 1800.0)
        self.assertEqual(accounting["known_total_wall_all_attempts_s"],
                         2290.2853)
        self.assertEqual(accounting["original_campaign_budget_exceeded_by_s"],
                         490.2853)
        self.assertFalse(accounting["original_campaign_compliant"])
        self.assertIn("never", accounting["original_campaign_note"])
        self.assertEqual(accounting["unmeasured_preparation"], "unknown")
        self.assertIsNone(accounting["corrective_pass"])
        corrective = driver.perf_budget_accounting(history, 900.0, 300.0)
        self.assertTrue(corrective["corrective_pass"]["within_budget"])
        self.assertIn("only to this explicitly approved corrective pass",
                      corrective["corrective_pass"]["note"])
        self.assertIn("never to the original campaign",
                      corrective["corrective_pass"]["note"])
        exceeded = driver.perf_budget_accounting(history, 900.0, 950.0)
        self.assertFalse(exceeded["corrective_pass"]["within_budget"])

    def test_reuse_historical_parse_is_strict(self):
        """Verify historical reuse accepts only a runnable absolute target.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking a valid reuse directory with a
        runnable release binary and rejecting relative paths and missing
        binaries.
        Raises AssertionError when an unrecorded historical binary could
        be reused silently.
        """
        sandbox = new_sandbox_dir()
        sandbox.mkdir(parents=True)
        target = sandbox / "prior-historical-57e3afb"
        (target / "release").mkdir(parents=True)
        binary = target / "release/glassbox"
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)
        self.assertEqual(driver.perf_parse_reuse_historical(str(target)),
                         target)
        with self.assertRaises(driver.DriverError):
            driver.perf_parse_reuse_historical("relative/historical")
        with self.assertRaises(driver.DriverError):
            driver.perf_parse_reuse_historical(str(sandbox / "absent"))

    def test_performance_case_blocks_without_attempt_history(self):
        """Verify the performance case fails closed without attempt costs.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking a BLOCKED exit with an explicit
        accounting blocker when no prior attempt result is supplied and no
        first attempt is declared.
        Raises AssertionError when the campaign proceeds with unknown
        attempt costs.
        """
        evidence = new_evidence_dir()
        evidence.mkdir(parents=True)
        stubs = new_sandbox_dir()
        stubs.mkdir(parents=True)
        stub = write_stub(stubs, "exit-zero", "import sys\nsys.exit(0)\n")
        args = SimpleNamespace(skip_build=True, binary=str(stub),
                               run_timeout=30, build_timeout=60,
                               perf_replicates=5,
                               perf_candidates="thin-lto",
                               historical_reconstruction=False)
        valid = {"affinity": [0, 1, 2, 3], "nice": 10, "limits": {},
                 "valid": True}
        with patch.object(driver, "resource_limit_snapshot",
                          return_value=valid):
            exit_code = driver.run_performance_case(args, evidence)
        self.assertEqual(exit_code, 2)
        results = sorted(evidence.glob("performance-*/result.json"))
        payload = json.loads(results[0].read_text())
        self.assertEqual(payload["status"], "BLOCKED")
        self.assertTrue(any("prior attempt" in blocker["detail"]
                            for blocker in payload["blockers"]))

    def test_clean_build_reference_branches(self):
        """Verify the clean-build reference reader never invents a duration.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking the retained-record branch and the
        explicit unavailable branch.
        Raises AssertionError when a missing reference is reported as
        available or vice versa.
        """
        sandbox = new_sandbox_dir()
        sandbox.mkdir(parents=True)
        payload = {"build": {"duration_s": 334.6, "status": 0},
                   "binary": {"sha256": "aead97ff" * 8, "bytes": 27952544}}
        reference = sandbox / "reference-result.json"
        reference.write_text(json.dumps(payload))
        with patch.object(driver, "PERF_CLEAN_BUILD_REFERENCE", reference):
            record = driver.perf_clean_build_reference_record()
        self.assertTrue(record["available"])
        self.assertEqual(record["clean_build_duration_s"], 334.6)
        self.assertIsNone(record["build_peak_rss_kb"])
        missing = sandbox / "absent-result.json"
        with patch.object(driver, "PERF_CLEAN_BUILD_REFERENCE", missing):
            record = driver.perf_clean_build_reference_record()
        self.assertFalse(record["available"])

    def test_inspect_checks_use_independent_manifest_digest(self):
        """Verify inspect checks compare against the frozen fixture digest.

        Accepts no parameters beyond the unittest case.
        Returns unit after accepting an exact digest match and rejecting a
        tampered digest.
        Raises AssertionError when an implementation-produced digest passes.
        """
        record = {"sha256": "f" * 64, "bytes": 42}
        report = {"schema_version": 2, "kind": "inspect",
                  "metadata": {"status": "complete",
                               "digest": {"digest": "f" * 64,
                                          "bytes_hashed": {"unit": "byte",
                                                           "value": 42},
                                          "snapshot_guarantee":
                                              "not_immutable"}}}
        checks = driver.perf_inspect_checks(report, record, "complete")
        self.assertTrue(all(check["ok"] for check in checks), checks)
        report["metadata"]["digest"]["digest"] = "0" * 64
        checks = driver.perf_inspect_checks(report, record, "complete")
        self.assertIn("independent_sha256",
                      {check["id"] for check in checks if not check["ok"]})

    def test_batch_scenario_checks_enforce_oracle(self):
        """Verify batch scenario checks accept only the expected stream.

        Accepts no parameters beyond the unittest case.
        Returns unit after accepting an ordered complete/unsupported stream
        with exact retention and rejecting a tampered status.
        Raises AssertionError when a changed stream passes as valid.
        """
        fixtures = self.synthetic_fixtures(new_sandbox_dir())
        fixtures["sixmib"]["content"] = "SIXCONTENT"
        responses = [
            {"id": 7, "report": {"metadata": {"status": "complete"},
                                 "content": {"units": [
                                     {"unit_index": 1,
                                      "text": "SIXCONTENT"}]}}},
        ]
        raw = (json.dumps(responses[0]) + "\n").encode()
        scenario = {"family": "batch",
                    "expect": {"exit": 0, "ids": [7],
                                "statuses": ["complete"],
                                "retention": {7: "sixmib"}}}
        execution = {"status": 0, "timed_out": False}
        checks = driver.perf_scenario_checks(scenario, execution, raw,
                                             fixtures)
        self.assertTrue(all(check["ok"] for check in checks), checks)
        responses[0]["report"]["metadata"]["status"] = "unsupported"
        raw = (json.dumps(responses[0]) + "\n").encode()
        checks = driver.perf_scenario_checks(scenario, execution, raw,
                                             fixtures)
        self.assertTrue(any(not check["ok"] for check in checks))

    def test_reuse_candidate_validation_is_strict(self):
        """Verify candidate reuse accepts only explicit prior target builds.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking a valid reuse entry and rejecting
        malformed names, relative paths, and missing binaries.
        Raises AssertionError when a wrong binary could be reused silently.
        """
        stubs = new_sandbox_dir()
        stubs.mkdir(parents=True)
        target = stubs / "prior-thin-lto"
        (target / "release").mkdir(parents=True)
        binary = target / "release/glassbox"
        binary.write_text("#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)
        reuse = driver.perf_parse_reuse_candidates(
            [f"thin-lto={target}"])
        self.assertEqual(reuse, {"thin-lto": target})
        for bad in ("thin-lto", "unknown=target",
                    "thin-lto=relative/target"):
            with self.assertRaises(driver.DriverError):
                driver.perf_parse_reuse_candidates([bad])
        with self.assertRaises(driver.DriverError):
            driver.perf_parse_reuse_candidates(
                [f"thin-lto-cgu1={stubs / 'absent'}"])
        self.assertEqual(driver.perf_parse_reuse_candidates(None), {})

    def test_performance_case_blocks_without_resource_guards(self):
        """Verify the performance case fails closed before any build.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking a BLOCKED exit with a written result
        when the mandatory CPU guards are not effective.
        Raises AssertionError when the campaign proceeds without guards.
        """
        evidence = new_evidence_dir()
        evidence.mkdir(parents=True)
        stubs = new_sandbox_dir()
        stubs.mkdir(parents=True)
        stub = write_stub(stubs, "exit-zero", "import sys\nsys.exit(0)\n")
        args = SimpleNamespace(skip_build=True, binary=str(stub),
                               run_timeout=30, build_timeout=60,
                               perf_replicates=5,
                               perf_candidates="thin-lto",
                               historical_reconstruction=False)
        invalid = {"affinity": [0], "nice": 0, "limits": {}, "valid": False}
        with patch.object(driver, "resource_limit_snapshot",
                          return_value=invalid):
            exit_code = driver.run_performance_case(args, evidence)
        self.assertEqual(exit_code, 2)
        results = sorted(evidence.glob("performance-*/result.json"))
        self.assertEqual(len(results), 1)
        payload = json.loads(results[0].read_text())
        self.assertEqual(payload["status"], "BLOCKED")
        self.assertTrue(payload["blockers"])


class BaselineV2SemanticsTests(unittest.TestCase):
    """V-SETUP v2 assertion semantics and prerequisite propagation."""

    @staticmethod
    def minimal_oracle(scratch):
        """Build a minimal independent oracle for define_baseline_runs.

        Accepts a scratch directory. Returns an oracle-shaped dict whose
        fixture records carry paths and construction-fixed expectations;
        define_baseline_runs reads expectations only, so no rasterization
        runs here. Raises nothing.
        """
        def record(name, path, expects):
            return {"name": name, "path": str(path), "sha256": "0" * 64,
                    "expects": expects}

        big_path = scratch / "big.txt"
        big_path.write_text("BIG TEXT LINE 000001 X\nBIG TEXT LINE 000002 X\n")
        return {
            "provenance": {"sandbox_dir": str(scratch)},
            "fixtures": [
                record("digital-1p.pdf", scratch / "digital-1p.pdf",
                       {"pages": 1, "sentinels": ["DIGITAL PAGE 1 SENTINEL 2026"]}),
                record("digital-4p.pdf", scratch / "digital-4p.pdf",
                       {"pages": 4, "sentinels": [f"DIGITAL PAGE {i} SENTINEL 2026"
                                                   for i in range(1, 5)]}),
                record("many-12p.pdf", scratch / "many-12p.pdf",
                       {"pages": 12, "sentinels": [f"MANY PAGE {i} LINE 1 SENTINEL 2026"
                                                    for i in range(1, 13)]}),
                record("plain-lines.txt", scratch / "plain-lines.txt",
                       {"lines": list(driver.PLAIN_LINES)}),
                record("big.txt", big_path,
                       {"first_line": "BIG TEXT LINE 000001 X",
                        "last_line": "BIG TEXT LINE 000002 X"}),
                record("blank-1p.pdf", scratch / "blank-1p.pdf", {"pages": 1}),
                record("zero-page.pdf", scratch / "zero-page.pdf", {"pages": 0}),
                record("scanned-1p.pdf", scratch / "scanned-1p.pdf",
                       {"pages": 1, "ocr_words": driver.OCR_WORDS}),
                record("mixed-2p.pdf", scratch / "mixed-2p.pdf",
                       {"pages": 2, "sentinels": [driver.MIXED_SENTINEL]}),
                record("data.json", scratch / "data.json", {}),
                record("doc.xml", scratch / "doc.xml", {}),
            ],
        }

    def test_baseline_runs_assert_v2_metadata_and_content_semantics(self):
        """Verify inspect runs stay metadata-only and extract runs carry content.

        Accepts no parameters beyond the unittest case.
        Returns unit after scanning every generated run's check kinds and
        JSON paths. Raises AssertionError when any legacy v1 body field path
        (counts.pages, text.content, text.truncated, errors_empty) survives
        or a v2 assertion family is missing.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True, exist_ok=True)
        runs = driver.define_baseline_runs(
            "/stub/glassbox", self.minimal_oracle(scratch))
        by_name = {run["name"]: run for run in runs}
        inspect_runs = [run for name, run in by_name.items()
                        if run["argv"][1] == "inspect"
                        and "--help" not in run["argv"]
                        and not run.get("qualified")]
        extract_runs = [run for run in runs if run["argv"][1] == "extract"]
        self.assertGreaterEqual(len(inspect_runs), 4)
        self.assertGreaterEqual(len(extract_runs), 6)
        legacy_paths = {"counts.pages", "text.content", "text.truncated"}
        for run in inspect_runs + extract_runs:
            kinds = {check["id"] for check in run["checks"]}
            paths = {check.get("path") for check in run["checks"]}
            self.assertNotIn("errors_empty", kinds, run["name"])
            self.assertFalse(paths & legacy_paths, run["name"])
        for run in inspect_runs:
            kinds = {check["id"] for check in run["checks"]}
            self.assertIn("digest_matches_file", kinds, run["name"])
            self.assertIn("page_signal_count", kinds, run["name"])
            self.assertIn("no_forbidden_content_fields", kinds, run["name"])
            self.assertIn("stdout_not_contains", kinds, run["name"])
            needles = {check.get("needle") for check in run["checks"]}
            self.assertIn("SENTINEL", needles, run["name"])
        content_runs = [run for run in extract_runs
                        if "content_units_contain" in
                        {check["id"] for check in run["checks"]} or
                        "content_units_text_equals" in
                        {check["id"] for check in run["checks"]} or
                        "content_units_text_equals_file" in
                        {check["id"] for check in run["checks"]}]
        self.assertGreaterEqual(len(content_runs), 5)
        for run in extract_runs:
            paths = {check.get("path") for check in run["checks"]}
            self.assertIn("selection.unit", paths, run["name"])
            self.assertIn("kind", paths, run["name"])

    def test_new_check_kinds_validate_v2_reports(self):
        """Verify the new apply_check kinds accept true reports and reject lies.

        Accepts no parameters beyond the unittest case.
        Returns unit after running each new check kind against synthetic
        reports and one real scratch file. Raises AssertionError when a
        passing report fails or a corrupted one passes.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True, exist_ok=True)
        fixture = scratch / "payload.txt"
        fixture.write_text("independent payload\n")
        import hashlib
        digest = hashlib.sha256(fixture.read_bytes()).hexdigest()
        inspect_report = {
            "schema_version": 2, "kind": "inspect",
            "metadata": {"status": "complete",
                         "digest": {"digest": digest,
                                    "bytes_hashed": {"value": fixture.stat().st_size}},
                         "native_page_signals": [{}, {}]},
        }
        extract_report = {
            "schema_version": 2, "kind": "extract",
            "content": {"units": [{"unit_index": 1, "text": "alpha beta"},
                                  {"unit_index": 2, "text": "gamma"}]},
        }
        record = {"status": 0, "timed_out": False}
        cases = [
            ({"id": "json_top_keys",
              "keys": ["schema_version", "kind", "metadata"]},
             inspect_report, True),
            ({"id": "json_top_keys", "keys": ["schema_version"]},
             inspect_report, False),
            ({"id": "digest_matches_file", "fixture": str(fixture)},
             inspect_report, True),
            ({"id": "bytes_hashed_equals_file_size", "fixture": str(fixture)},
             inspect_report, True),
            ({"id": "page_signal_count", "count": 2}, inspect_report, True),
            ({"id": "page_signal_count", "count": 3}, inspect_report, False),
            ({"id": "no_forbidden_content_fields"}, inspect_report, True),
            ({"id": "content_units_contain", "needles": ["alpha", "gamma"]},
             extract_report, True),
            ({"id": "content_units_contain", "needles": ["delta"]},
             extract_report, False),
            ({"id": "unit_count", "count": 2}, extract_report, True),
            ({"id": "unit_text", "index": 2, "equals": "gamma"},
             extract_report, True),
            ({"id": "unit_text", "index": 2, "equals": "wrong"},
             extract_report, False),
            ({"id": "content_units_text_equals", "texts": ["alpha beta", "gamma"]},
             extract_report, True),
            ({"id": "content_units_text_equals_file", "fixture": str(fixture)},
             extract_report, False),
            ({"id": "unit_texts_all_blank"}, extract_report, False),
        ]
        for check, report, expected in cases:
            ok, _ = driver.apply_check(check, record, json.dumps(report), report)
            self.assertEqual(ok, expected, check["id"])
        blank_report = {"schema_version": 2, "kind": "extract",
                        "content": {"units": [{"unit_index": 1, "text": " "}]}}
        ok, _ = driver.apply_check({"id": "unit_texts_all_blank"}, record,
                                   json.dumps(blank_report), blank_report)
        self.assertTrue(ok)
        body_report = {"schema_version": 2, "kind": "inspect",
                       "metadata": {"status": "complete"},
                       "text": {"content": "leak"}}
        ok, _ = driver.apply_check({"id": "no_forbidden_content_fields"}, record,
                                   json.dumps(body_report), body_report)
        self.assertFalse(ok)

    def test_all_case_propagates_fresh_baseline_to_parity(self):
        """Verify the aggregate run hands the fresh baseline to parity.

        Accepts no parameters beyond the unittest case.
        Returns unit after running run_all_case with patched case runners
        that capture the parity arguments. Raises AssertionError when the
        propagated prerequisite paths are not the fresh baseline's evidence.
        """
        evidence = new_evidence_dir()
        evidence.mkdir(parents=True, exist_ok=True)
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True, exist_ok=True)
        result_path = scratch / "result.json"
        oracle_path = scratch / "oracle.json"
        result_path.write_text("{}")
        oracle_path.write_text("{}")
        args = SimpleNamespace(skip_build=True, binary="/stub", run_timeout=30,
                               build_timeout=60)
        captured = {}

        def fake_case_runner(name):
            def runner(case_args, case_evidence):
                if name == "parity":
                    captured["baseline_result"] = case_args.baseline_result
                    captured["baseline_oracle"] = case_args.baseline_oracle
                if name == "baseline":
                    driver.LAST_BASELINE_PATHS["result"] = str(result_path)
                    driver.LAST_BASELINE_PATHS["oracle"] = str(oracle_path)
                return 0
            return runner

        try:
            with patch.object(driver, "run_case",
                              side_effect=lambda name, a, e:
                              fake_case_runner(name)(a, e)):
                exit_code = driver.run_all_case(args, evidence)
            self.assertEqual(exit_code, 0)
            self.assertEqual(captured["baseline_result"], str(result_path))
            self.assertEqual(captured["baseline_oracle"], str(oracle_path))
            aggregate = json.loads(
                next(evidence.glob("all-*/result.json")).read_text())
            self.assertEqual(aggregate["baseline_propagation"]["mode"],
                             "propagated_fresh_baseline")
        finally:
            driver.LAST_BASELINE_PATHS.clear()


class ParityPrerequisiteTests(unittest.TestCase):
    """Parity prerequisite resolution without fabricated receipts."""

    def test_explicit_parameters_require_a_passing_v2_baseline(self):
        """Verify explicit prerequisite validation accepts PASS and rejects rest.

        Accepts no parameters beyond the unittest case.
        Returns unit after validating a synthetic explicit baseline/oracle
        pair and rejecting failed records and fixture hash drift.
        Raises AssertionError when a failed baseline or changed fixture is
        accepted.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True, exist_ok=True)
        fixture = scratch / "digital-1p.pdf"
        fixture.write_bytes(b"%PDF-1.7 synthetic")
        oracle = {"fixtures": [
            {"name": name, "path": str(scratch / name), "sha256": "0" * 64}
            for name in driver.PARITY_REQUIRED_FIXTURES
        ]}
        oracle_path = scratch / "oracle.json"
        for record in oracle["fixtures"]:
            payload = scratch / record["name"]
            payload.write_bytes(b"synthetic " + record["name"].encode())
            record["sha256"] = driver.sha256_file(payload)
        oracle_path.write_text(json.dumps(oracle))
        result_path = scratch / "result.json"
        result_path.write_text(json.dumps(
            {"status": "PASS", "verification_id": "V-SETUP"}))
        args = SimpleNamespace(baseline_result=str(result_path),
                               baseline_oracle=str(oracle_path))
        resolved = driver.load_parity_prerequisites(args)
        self.assertEqual(resolved["mode"], "explicit_v2_baseline")
        result_path.write_text(json.dumps(
            {"status": "FAIL", "verification_id": "V-SETUP"}))
        with self.assertRaises(driver.DriverError):
            driver.load_parity_prerequisites(args)
        result_path.write_text(json.dumps(
            {"status": "PASS", "verification_id": "V-SETUP"}))
        record = next(r for r in oracle["fixtures"]
                      if r["name"] == "plain-lines.txt")
        record["sha256"] = "1" * 64
        oracle_path.write_text(json.dumps(oracle))
        with self.assertRaises(driver.DriverError):
            driver.load_parity_prerequisites(args)

    def test_missing_defaults_construct_fresh_independent_fixtures(self):
        """Verify absent retained defaults never block parity on a receipt.

        Accepts no parameters beyond the unittest case.
        Returns unit after patching the retained default paths away and the
        fixture builder to a synthetic oracle. Raises AssertionError when
        the resolver claims a V-SETUP receipt it never had.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True, exist_ok=True)
        synthetic_fixtures = []
        for name in driver.PARITY_REQUIRED_FIXTURES:
            payload = scratch / name
            payload.write_bytes(b"synthetic " + name.encode())
            synthetic_fixtures.append(
                {"name": name, "path": str(payload),
                 "sha256": driver.sha256_file(payload)})
        synthetic = {"oracle_path": str(scratch / "fresh-oracle.json"),
                     "fixtures": synthetic_fixtures}
        args = SimpleNamespace(baseline_result=None, baseline_oracle=None)
        with patch.object(driver, "BASELINE_RESULT", scratch / "absent.json"), \
                patch.object(driver, "BASELINE_ORACLE", scratch / "absent-oracle.json"), \
                patch.object(driver, "build_fixtures", return_value=synthetic):
            resolved = driver.load_parity_prerequisites(args)
        self.assertEqual(resolved["mode"], "fresh_independent_fixtures")
        self.assertIsNone(resolved["baseline"])
        self.assertIn("original V-SETUP evidence is absent", resolved["note"])

    def test_partial_explicit_parameters_are_rejected(self):
        """Verify one-sided explicit parameters fail closed.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking both one-sided combinations raise.
        Raises AssertionError when a half-supplied prerequisite is accepted.
        """
        args = SimpleNamespace(baseline_result="/absent.json",
                               baseline_oracle=None)
        with self.assertRaises(driver.DriverError):
            driver.load_parity_prerequisites(args)
        completed = run_driver("parity", new_evidence_dir(),
                               "--baseline-result", "/absent.json")
        self.assertEqual(completed.returncode, 2, completed.stderr)
        self.assertIn("supplied together", completed.stderr)


class DeliveryValidationTests(unittest.TestCase):
    """C-TEST delivery-validation mode gates and accepted-evidence chain."""

    def test_supplied_delivery_binary_must_match_the_cached_production_target(self):
        """Verify delivery validation never silently ignores --binary.

        Accepts no parameters beyond the unittest case and writes only small
        scratch files under the ignored sandbox tree.
        Returns unit after checking the supplied path matches by resolved
        path or byte identity, and a differing or missing path raises
        DriverError naming the delivered executable. Raises AssertionError
        when a different path could be validated silently.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        delivered = scratch / "glassbox"
        delivered.write_bytes(b"delivered-binary")
        record = driver.perf_delivery_supplied_binary(str(delivered), delivered)
        self.assertTrue(record["matches"])
        self.assertFalse(record.get("identical_sha256", False))
        copy = scratch / "copy-glassbox"
        copy.write_bytes(b"delivered-binary")
        self.assertTrue(driver.perf_delivery_supplied_binary(copy, delivered)["matches"])
        other = scratch / "other"
        other.write_bytes(b"different-binary")
        with self.assertRaises(driver.DriverError) as raised:
            driver.perf_delivery_supplied_binary(other, delivered)
        self.assertIn("never silently ignored", str(raised.exception))
        self.assertIn(str(delivered.resolve()), str(raised.exception))
        with self.assertRaises(driver.DriverError):
            driver.perf_delivery_supplied_binary(None, delivered)

    def test_delivery_cli_requires_performance_case_and_binary(self):
        """Verify the delivery mode cannot run outside its contract.

        Accepts no parameters beyond the unittest case.
        Returns unit after checking the CLI rejects the flag for other
        cases and without an explicit binary. Raises AssertionError when a
        delivery run could start without the production binary.
        """
        wrong_case = run_driver("metadata", new_evidence_dir(),
                                "--delivery-validation")
        self.assertEqual(wrong_case.returncode, 2, wrong_case.stderr)
        self.assertIn("applies only to the performance case", wrong_case.stderr)
        missing_binary = run_driver("performance", new_evidence_dir(),
                                    "--delivery-validation")
        self.assertEqual(missing_binary.returncode, 2, missing_binary.stderr)
        self.assertIn("requires --binary", missing_binary.stderr)

    def test_delivery_dispatch_reaches_its_runner(self):
        """Verify delivery mode dispatches to the delivery runner.

        Accepts no parameters beyond the unittest case.
        Returns unit after patching the delivery runner and calling
        run_case. Raises AssertionError when the default comparison runner
        receives a delivery-mode invocation.
        """
        args = SimpleNamespace(delivery_validation=True)
        with patch.object(driver, "run_performance_delivery_case",
                          return_value=0) as runner:
            code = driver.run_case("performance", args, None)
        self.assertEqual(code, 0)
        runner.assert_called_once()

    def test_release_profile_record_reads_the_canonical_profile(self):
        """Verify the live manifest profile is read, never assumed.

        Accepts no parameters beyond the unittest case.
        Returns unit after parsing the real downstream Cargo.toml and
        checking the canonical adopted tuple. Raises AssertionError when
        the record disagrees with the controller-adopted manifest.
        """
        record = driver.perf_release_profile_record()
        self.assertEqual(record["profile.release"],
                         driver.PERF_CANONICAL_RELEASE_PROFILE)
        self.assertEqual(record["manifest_sha256"],
                         driver.sha256_file(driver.EXT / "Cargo.toml"))

    def test_delivery_source_delta_accepts_only_the_profile_delta(self):
        """Verify unchanged Rust sources plus the profile-only Cargo delta.

        Accepts no parameters beyond the unittest case.
        Returns unit after comparing a synthetic prior manifest against the
        live tree. Raises AssertionError when a changed Rust source is
        accepted or the exact profile delta is rejected.
        """
        prior_manifest = []
        for scope in ("crates/liteparse/src/lib.rs",
                      "extensionalities/crates/litx/src/main.rs",
                      "extensionalities/Cargo.lock",
                      "Cargo.lock"):
            path = driver.REPO / scope
            prior_manifest.append({"path": scope,
                                   "sha256": driver.sha256_file(path)})
        prior_manifest.append({"path": "extensionalities/Cargo.toml",
                               "sha256": "0" * 64})
        delta = driver.perf_delivery_source_delta(prior_manifest)
        self.assertEqual(delta["changed"], ["extensionalities/Cargo.toml"])
        self.assertTrue(delta["profile_only_delta"]["current_profile"])
        corrupted = [dict(entry) for entry in prior_manifest]
        corrupted[0]["sha256"] = "1" * 64
        with self.assertRaises(driver.DriverError) as error:
            driver.perf_delivery_source_delta(corrupted)
        self.assertIn("crates/liteparse/src/lib.rs", str(error.exception))

    def test_delivery_pin_checker_validates_synthetic_accepted_evidence(self):
        """Verify the accepted-evidence chain checker accepts a matching chain.

        Accepts no parameters beyond the unittest case and writes only small
        synthetic files under the ignored sandbox tree; no retained
        gitignored evidence is read.
        Returns unit after building one synthetic accepted record, experiment
        spec, and approved candidate binary whose digests agree with a
        synthetic pin, then checking the chain record carries no blockers.
        Raises AssertionError when a fully matching accepted chain is
        blocked or the runner's checker defaults are not bound to the real
        retained evidence pins.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        accepted_dir = scratch / "accepted"
        accepted_dir.mkdir()
        candidate_target = scratch / "candidate-target"
        (candidate_target / "release").mkdir(parents=True)
        binary = candidate_target / "release" / "glassbox"
        binary.write_bytes(b"synthetic-approved-candidate")
        binary.chmod(0o755)
        pinned = driver.sha256_file(binary)
        (accepted_dir / "result.json").write_text(json.dumps({
            "status": "PASS",
            "candidate_builds": {
                "synthetic-candidate": {"binary": {"sha256": pinned}}}}))
        (accepted_dir / "experiment-spec.json").write_text(json.dumps(
            {"evaluator_clock": {"version": driver.PERF_EVALUATOR_VERSION}}))
        check = driver.perf_delivery_accepted_evidence(
            accepted_dir / "result.json", candidate_name="synthetic-candidate",
            candidate_sha=pinned, candidate_target=candidate_target)
        self.assertEqual(check["blockers"], [])
        self.assertEqual(check["accepted_payload"]["status"], "PASS")
        self.assertEqual(check["accepted_spec"]["evaluator_clock"]["version"],
                         driver.PERF_EVALUATOR_VERSION)
        self.assertEqual(check["approved_candidate_binary"]["sha256"], pinned)
        self.assertTrue(check["approved_candidate_binary"]["available"])
        # The delivery runner's checker defaults stay bound to the real
        # retained evidence pins, so explicit delivery verification keeps
        # every historical pin and immutable-evidence check intact.
        defaults = inspect.signature(
            driver.perf_delivery_accepted_evidence).parameters
        self.assertEqual(defaults["record_path"].default,
                         driver.PERF_ACCEPTED_CORRECTIVE_RESULT)
        self.assertEqual(defaults["candidate_name"].default,
                         driver.PERF_APPROVED_CANDIDATE_NAME)
        self.assertEqual(defaults["candidate_sha"].default,
                         driver.PERF_APPROVED_CANDIDATE_BINARY_SHA)
        self.assertEqual(defaults["candidate_target"].default,
                         driver.PERF_APPROVED_CANDIDATE_TARGET)
        self.assertEqual(defaults["evaluator_version"].default,
                         driver.PERF_EVALUATOR_VERSION)

    def test_delivery_pin_checker_fails_closed_on_absent_or_drifted_evidence(self):
        """Verify the chain checker blocks absent, incomplete, or drifted evidence.

        Accepts no parameters beyond the unittest case and writes only small
        synthetic files under the ignored sandbox tree.
        Returns unit after checking absent evidence, a non-PASS record, a
        record candidate digest disagreeing with the pin, a stale evaluator
        clock, and a drifted retained approved binary each produce distinct
        fail-closed blockers. Raises AssertionError when missing or
        mismatched evidence could pass silently.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        absent = driver.perf_delivery_accepted_evidence(
            scratch / "absent" / "result.json",
            candidate_target=scratch / "absent-target")
        self.assertIsNone(absent["accepted_payload"])
        details = " ".join(blocker["detail"] for blocker in absent["blockers"])
        self.assertIn("cannot read the accepted corrective-07 result", details)
        self.assertIn("candidate binary is missing", details)
        nopass_dir = scratch / "nopass"
        nopass_dir.mkdir()
        (nopass_dir / "result.json").write_text(json.dumps({"status": "FAIL"}))
        nopass = driver.perf_delivery_accepted_evidence(
            nopass_dir / "result.json", candidate_target=scratch / "absent-target")
        self.assertEqual(nopass["accepted_payload"]["status"], "FAIL")
        details = " ".join(blocker["detail"] for blocker in nopass["blockers"])
        self.assertIn("is not PASS", details)
        self.assertIn(f"lacks the {driver.PERF_APPROVED_CANDIDATE_NAME} "
                      "candidate binary identity", details)
        self.assertIn("cannot read the accepted experiment spec", details)
        record_dir = scratch / "record"
        record_dir.mkdir()
        (record_dir / "result.json").write_text(json.dumps({
            "status": "PASS",
            "candidate_builds": {
                driver.PERF_APPROVED_CANDIDATE_NAME: {
                    "binary": {"sha256": "f" * 64}}}}))
        (record_dir / "experiment-spec.json").write_text(json.dumps(
            {"evaluator_clock": {"version": driver.PERF_EVALUATOR_VERSION}}))
        drifted_target = scratch / "drifted-target"
        (drifted_target / "release").mkdir(parents=True)
        drifted_binary = drifted_target / "release" / "glassbox"
        drifted_binary.write_bytes(b"drifted-binary")
        drifted_binary.chmod(0o755)
        check = driver.perf_delivery_accepted_evidence(
            record_dir / "result.json", candidate_target=drifted_target)
        details = " ".join(blocker["detail"] for blocker in check["blockers"])
        self.assertIn("candidate digest disagrees with the pinned approved "
                      "digest", details)
        self.assertIn("approved candidate binary digest no longer matches",
                      details)
        (record_dir / "experiment-spec.json").write_text(json.dumps(
            {"evaluator_clock": {"version": 1}}))
        stale = driver.perf_delivery_accepted_evidence(
            record_dir / "result.json", candidate_target=drifted_target)
        details = " ".join(blocker["detail"] for blocker in stale["blockers"])
        self.assertIn("not measured with the v2 blocking-clock evaluator",
                      details)

    def test_delivery_mode_constants_stay_pinned_without_retained_files(self):
        """Verify the historical delivery pins without reading retained files.

        Accepts no parameters beyond the unittest case and touches no
        gitignored evidence: the retained files themselves are checked by
        the driver during explicit delivery verification, which fails
        closed when they are absent.
        Returns unit after checking the pinned accepted record path,
        candidate name, digest, and seven-attempt ledger keep the
        controller-accepted identifiers. Raises AssertionError when any
        historical pin drifts.
        """
        self.assertEqual(driver.PERF_DELIVERY_REQUIRED_PRIOR_ATTEMPTS, 7)
        self.assertEqual(driver.PERF_APPROVED_CANDIDATE_NAME, "thin-lto-cgu1")
        self.assertEqual(
            driver.PERF_APPROVED_CANDIDATE_BINARY_SHA,
            "71e5f0e75c35fe1d724b8851f582b363dfd8c6b"
            "664379bc61fbd20aded8f6533")
        self.assertEqual(
            driver.PERF_ACCEPTED_CORRECTIVE_RESULT,
            driver.VERIFICATION_ROOT / "settling-in-bperf-corrective-07" /
            "performance-20261122T170902Z-2955654-729295e6" / "result.json")
        self.assertEqual(
            driver.PERF_APPROVED_CANDIDATE_TARGET,
            driver.BUILD_ROOT /
            "settling-in-build-20261122T160941Z-2789210-e7450310-thin-lto-cgu1")


class BuildProcessGroupLifecycleTests(unittest.TestCase):
    """Common session process-group lifecycle shared by every build call site."""

    def wait_for_pid_exit(self, pid, bound_s=10.0):
        """Poll until a process id disappears from the OS.

        Accepts the process id and an optional polling bound in seconds.
        Returns True when the id disappeared within the bound and False
        otherwise; orphan reaping by the host init can take a short while.
        Raises no exception; the caller asserts on the returned value.
        """
        deadline = time.monotonic() + bound_s
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except OSError:
                return True
            time.sleep(0.1)
        return False

    def write_group_spawner(self, scratch, pid_path=None):
        """Write a script that spawns a sleeping grandchild and then hangs.

        Accepts the scratch directory and an optional pid-file path. Returns
        the script path; the spawner writes its grandchild pid to the pid
        file when one is given, so tests never depend on partial stdout
        surviving a timeout.
        """
        script = scratch / "spawn-and-hang.py"
        script.write_text(
            "import subprocess, sys, time\n"
            "from pathlib import Path\n"
            "child = subprocess.Popen(\n"
            "    [sys.executable, '-c', 'import time; time.sleep(30)'])\n"
            + (f"Path({str(pid_path)!r}).write_text(str(child.pid))\n"
               if pid_path is not None else "")
            + "time.sleep(30)\n")
        return script

    def test_launch_creates_session_and_stop_kills_the_whole_group(self):
        """Verify a timeout stops the session leader and its grandchildren.

        Accepts no parameters beyond the unittest case and writes only small
        scratch files under the ignored sandbox tree.
        Returns unit after checking the child leads a fresh session whose
        group contains the grandchild, and stop_session_process_group kills
        the whole group. Raises AssertionError when any group member
        survives the group stop.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        pid_path = scratch / "grandchild.pid"
        script = self.write_group_spawner(scratch, pid_path)
        process = driver.launch_session_process(
            [sys.executable, "-B", str(script)], stdin=subprocess.DEVNULL)
        try:
            deadline = time.monotonic() + 10
            while not pid_path.is_file() and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertTrue(pid_path.is_file(), "grandchild never started")
            grandchild_pid = int(pid_path.read_text())
            # The child leads its own session and the grandchild shares the
            # same process group, so one group signal reaches both.
            self.assertEqual(os.getpgid(process.pid), process.pid)
            self.assertEqual(os.getpgid(grandchild_pid), process.pid)
            stopped_pgid = driver.stop_session_process_group(process)
            self.assertEqual(stopped_pgid, process.pid)
            process.communicate(timeout=10)
            self.assertIsNotNone(process.returncode)
            self.assertTrue(self.wait_for_pid_exit(grandchild_pid),
                            "grandchild survived the group stop")
        finally:
            if process.poll() is None:
                driver.stop_session_process_group(process)
                process.communicate(timeout=10)

    def test_no_timeout_launch_waits_for_completion(self):
        """Verify an unbounded launch still waits for normal completion.

        Accepts no parameters beyond the unittest case.
        Returns unit after running a short-lived child without any timeout
        bound and checking its output and exit status. Raises AssertionError
        when the no-timeout behavior imposes a bound or loses output.
        """
        process = driver.launch_session_process(
            [sys.executable, "-B", "-c", "print('done')"],
            stdin=subprocess.DEVNULL)
        stdout, _ = process.communicate(timeout=None)
        self.assertEqual(process.returncode, 0)
        self.assertIn(b"done", stdout)

    def test_parity_execute_timeout_kills_group_and_retains_log(self):
        """Verify a parity timeout stops the group, retains the log, and errors.

        Accepts no parameters beyond the unittest case and writes only small
        scratch files under the ignored sandbox tree.
        Returns unit after checking a bounded parity execution that times out
        raises DriverError with the bound, writes a TIMEOUT marker into the
        retained log, and stops the grandchild with the session leader.
        Raises AssertionError when the timeout leaks the process group or
        loses the retained log.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        pid_path = scratch / "grandchild.pid"
        script = self.write_group_spawner(scratch, pid_path)
        log = scratch / "parity-timeout.log"
        with self.assertRaises(driver.DriverError) as raised:
            driver.parity_execute(
                [sys.executable, "-B", str(script)], log, 2, time.monotonic())
        self.assertIn("timed out after 2s", str(raised.exception))
        self.assertIn("TIMEOUT", log.read_text(errors="replace"))
        grandchild_pid = int(pid_path.read_text())
        self.assertTrue(self.wait_for_pid_exit(grandchild_pid),
                        "grandchild survived the parity timeout stop")

    def write_stub_cargo(self, scratch):
        """Write a stub cargo that probes cleanly and spawns a hanging group.

        Accepts the scratch directory. Returns the stub directory to prepend
        to PATH: the stub answers the offline metadata probe with exit 0,
        exits 7 for builds, and when GRANDCHILD_PID_FILE is set it first
        spawns a sleeping grandchild sharing its process group and hangs,
        so build-timeout group behavior is observable without any Rust
        compilation.
        """
        stub_dir = scratch / "stub-bin"
        stub_dir.mkdir()
        write_stub(stub_dir, "cargo", """
import os, subprocess, sys, time
if "metadata" in sys.argv:
    sys.exit(0)
pid_file = os.environ.get("GRANDCHILD_PID_FILE")
if pid_file:
    child = subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(30)"])
    with open(pid_file, "w") as stream:
        stream.write(str(child.pid))
    time.sleep(30)
sys.exit(7)
""")
        return stub_dir

    def test_do_build_timeout_stops_group_and_retains_log(self):
        """Verify a build timeout stops the group, retains the log, and errors.

        Accepts no parameters beyond the unittest case and writes only small
        scratch files under the ignored sandbox tree plus stub build-home
        directories under the ignored target tree; no Rust build runs.
        Returns unit after checking a build timeout raises a clear
        DriverError naming the retained log, the log records the timeout,
        the offline retry is not attempted after a timeout, and the stub's
        grandchild dies with the session leader. Raises AssertionError when
        a timed-out build leaks its process group, loses its log, or
        reports an unclear error.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        pid_path = scratch / "grandchild.pid"
        stub_dir = self.write_stub_cargo(scratch)
        run_dir = scratch / "run"
        run_dir.mkdir()
        args = SimpleNamespace(build_timeout=2)
        environment = dict(os.environ,
                           PATH=f"{stub_dir}{os.pathsep}{os.environ['PATH']}",
                           GRANDCHILD_PID_FILE=str(pid_path))
        with patch.dict(os.environ, environment):
            with self.assertRaises(driver.DriverError) as raised:
                driver.do_build(args, run_dir)
        message = str(raised.exception)
        self.assertIn("cargo build timed out after 2s", message)
        self.assertIn("the whole build process group was stopped", message)
        self.assertIn("cargo-build.log", message)
        log = (run_dir / "cargo-build.log").read_text()
        self.assertIn("# timed_out: True", log)
        self.assertIn("# timeout_s: 2", log)
        self.assertFalse((run_dir / "cargo-build-retry.log").exists(),
                         "a timed-out build must not be retried")
        grandchild_pid = int(pid_path.read_text())
        self.assertTrue(self.wait_for_pid_exit(grandchild_pid),
                        "grandchild survived the build timeout stop")

    def test_do_build_records_failed_attempts_and_no_timeout_runs(self):
        """Verify failed build attempts are recorded with retained logs.

        Accepts no parameters beyond the unittest case and writes only small
        scratch files under the ignored sandbox and target trees; no Rust
        build runs.
        Returns unit after checking a stub cargo that fails builds returns
        the last attempt's status, records both the offline attempt and the
        retry without --offline, and never marks a completed attempt as
        timed out. Raises AssertionError when attempt records, logs, or the
        returned status are inconsistent.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        stub_dir = self.write_stub_cargo(scratch)
        run_dir = scratch / "run"
        run_dir.mkdir()
        args = SimpleNamespace(build_timeout=30)
        environment = dict(os.environ,
                           PATH=f"{stub_dir}{os.pathsep}{os.environ['PATH']}")
        environment.pop("GRANDCHILD_PID_FILE", None)
        with patch.dict(os.environ, environment):
            record, build_target, status = driver.do_build(args, run_dir)
        self.assertEqual(status, 7)
        self.assertEqual(record["status"], 7)
        self.assertEqual(len(record["attempts"]), 2)
        self.assertTrue(all(attempt["timed_out"] is False
                            for attempt in record["attempts"]))
        self.assertTrue(all(attempt["status"] == 7
                            for attempt in record["attempts"]))
        self.assertTrue("--offline" in record["attempts"][0]["argv"])
        self.assertTrue("--offline" not in record["attempts"][1]["argv"])
        self.assertTrue((run_dir / "cargo-build.log").is_file())
        self.assertTrue((run_dir / "cargo-build-retry.log").is_file())


class PocrOracleManifestTests(unittest.TestCase):
    """Tracked portable P-OCR reuse oracle independence and fail-closed gates."""

    def test_tracked_manifest_agrees_with_driver_frozen_pins(self):
        """Verify the tracked oracle manifest matches the driver's frozen pins.

        Accepts no parameters beyond the unittest case and reads only the
        tracked fixture manifest, never the gitignored sandbox oracle.
        Returns unit after checking every frozen reuse fixture appears with
        its pinned digest, synthetic license, and recorded provenance.
        Raises AssertionError when the tracked expectations drift from the
        driver pins or lose their provenance records.
        """
        manifest = json.loads(
            driver.PERF_POCR_ORACLE_MANIFEST.read_text(encoding="utf-8"))
        self.assertEqual(manifest["schema"], driver.PERF_POCR_ORACLE_SCHEMA)
        records = {record["name"]: record for record in manifest["fixtures"]}
        self.assertEqual(set(records), set(driver.PERF_POCR_FROZEN_SHA))
        for name, pinned in driver.PERF_POCR_FROZEN_SHA.items():
            self.assertEqual(records[name]["sha256"], pinned)
        self.assertIn("spike-3", manifest["provenance"]["accepted_record"])
        for record in records.values():
            self.assertIn("Apache-2.0", record["license"])
            self.assertIn("original synthetic source text", record["license"])
            self.assertTrue(record["construction"])
            for key in ("expected_phrases", "native_phrases", "ocr_phrases"):
                self.assertIsInstance(record[key], list)
            self.assertIn("expected_pages", record)

    def test_missing_external_dataset_fails_distinctly(self):
        """Verify a missing external dataset fails with a named prerequisite.

        Accepts no parameters beyond the unittest case and uses only the
        tracked manifest plus an absent synthetic dataset directory.
        Returns unit after checking the distinct external-dataset error
        names the missing file, the pinned digest, and the tracked manifest
        so a clean checkout operator knows exactly which prerequisite is
        missing. Raises AssertionError when the missing dataset is faked,
        skipped, or reported through an unrelated error.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        with self.assertRaises(driver.DriverError) as raised:
            driver.perf_pocr_reuse_fixtures(
                dataset_dir=scratch / "absent-dataset")
        message = str(raised.exception)
        self.assertIn("external P-OCR dataset prerequisite is missing",
                      message)
        self.assertIn("scanned-4p.pdf", message)
        self.assertIn("pocr-reuse-oracle.json", message)
        self.assertIn("never fabricates the external dataset", message)

    def test_reuse_validator_returns_frozen_expectations_for_supplied_dataset(self):
        """Verify a supplied dataset validates and returns the frozen expectations.

        Accepts no parameters beyond the unittest case and writes one small
        synthetic dataset and manifest under the ignored sandbox tree.
        Returns unit after checking the validator returns the dataset path,
        digest, and tracked per-page expectations for a matching input.
        Raises AssertionError when a matching dataset is rejected or the
        tracked expectations are altered during reuse.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        payload = b"synthetic pocr reuse fixture"
        digest = driver.sha256_bytes(payload)
        manifest = {
            "schema": driver.PERF_POCR_ORACLE_SCHEMA,
            "fixtures": [{
                "name": "probe.pdf", "sha256": digest,
                "construction": "synthetic self-test fixture",
                "license": "Apache-2.0; original synthetic source text; "
                           "no third-party corpus content",
                "expected_pages": None,
                "expected_phrases": ["SYNTHETIC PROBE PHRASE"],
                "native_phrases": [],
                "ocr_phrases": ["SYNTHETIC PROBE PHRASE"]}]}
        manifest_path = scratch / "oracle.json"
        manifest_path.write_text(json.dumps(manifest))
        dataset = scratch / "dataset"
        dataset.mkdir()
        (dataset / "probe.pdf").write_bytes(payload)
        reused = driver.perf_pocr_reuse_fixtures(
            dataset_dir=dataset, manifest_path=manifest_path,
            frozen_sha={"probe.pdf": digest})
        self.assertEqual(set(reused), {"probe.pdf"})
        self.assertEqual(reused["probe.pdf"]["path"], str(dataset / "probe.pdf"))
        self.assertEqual(reused["probe.pdf"]["sha256"], digest)
        self.assertEqual(reused["probe.pdf"]["bytes"], len(payload))
        self.assertEqual(reused["probe.pdf"]["expected_phrases"],
                         ["SYNTHETIC PROBE PHRASE"])
        self.assertEqual(reused["probe.pdf"]["ocr_phrases"],
                         ["SYNTHETIC PROBE PHRASE"])
        self.assertEqual(reused["probe.pdf"]["expected_pages"], None)

    def test_manifest_and_dataset_disagreement_fails_closed(self):
        """Verify tampered manifests and drifted datasets are both rejected.

        Accepts no parameters beyond the unittest case and writes only small
        synthetic files under the ignored sandbox tree.
        Returns unit after checking an unreadable manifest, a manifest
        missing a frozen fixture, a manifest digest disagreeing with the
        pin, and a drifted dataset file each raise distinct DriverErrors.
        Raises AssertionError when a disagreement could pass silently.
        """
        scratch = new_sandbox_dir()
        scratch.mkdir(parents=True)
        with self.assertRaises(driver.DriverError) as raised:
            driver.perf_pocr_reuse_fixtures(
                dataset_dir=scratch, manifest_path=scratch / "absent.json",
                frozen_sha={})
        self.assertIn("cannot read the tracked P-OCR reuse oracle manifest",
                      str(raised.exception))
        schema_bad = scratch / "schema-bad.json"
        schema_bad.write_text(json.dumps({"schema": "unknown/0",
                                          "fixtures": []}))
        with self.assertRaises(driver.DriverError) as raised:
            driver.perf_pocr_reuse_fixtures(
                dataset_dir=scratch, manifest_path=schema_bad, frozen_sha={})
        self.assertIn("does not declare the expected schema",
                      str(raised.exception))
        payload = b"synthetic pocr reuse fixture"
        digest = driver.sha256_bytes(payload)
        manifest = {"schema": driver.PERF_POCR_ORACLE_SCHEMA,
                    "fixtures": [{"name": "probe.pdf", "sha256": digest}]}
        manifest_path = scratch / "oracle.json"
        manifest_path.write_text(json.dumps(manifest))
        with self.assertRaises(driver.DriverError) as raised:
            driver.perf_pocr_reuse_fixtures(
                dataset_dir=scratch, manifest_path=manifest_path,
                frozen_sha={"probe.pdf": digest, "other.pdf": digest})
        self.assertIn("missing the frozen fixtures", str(raised.exception))
        with self.assertRaises(driver.DriverError) as raised:
            driver.perf_pocr_reuse_fixtures(
                dataset_dir=scratch, manifest_path=manifest_path,
                frozen_sha={"probe.pdf": "1" * 64})
        self.assertIn("disagrees with the frozen probe.pdf digest",
                      str(raised.exception))
        dataset = scratch / "dataset"
        dataset.mkdir()
        (dataset / "probe.pdf").write_bytes(b"drifted payload")
        with self.assertRaises(driver.DriverError) as raised:
            driver.perf_pocr_reuse_fixtures(
                dataset_dir=dataset, manifest_path=manifest_path,
                frozen_sha={"probe.pdf": digest})
        self.assertIn("digest mismatch", str(raised.exception))


if __name__ == "__main__":
    unittest.main()

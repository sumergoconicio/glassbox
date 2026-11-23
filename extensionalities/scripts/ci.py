#!/usr/bin/env python3
"""Module: Glassbox downstream automation.
File: ci.py.
Responsibility: Run scoped formatting, native checks, parity, smoke, and workflow validation.

Downstream CI commands. Requires Python 3.11+, Git, and the native build tools.

Run from the repository root. Checks write only Cargo output and isolated
fixtures under extensionalities/target. No upstream tests or formatters run.
Command failures propagate; missing diff objects conservatively enable checks.
The workflows command additionally requires Go 1.24+ and network access.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = "extensionalities/Cargo.toml"


def relevant(path):
    return (
        path.startswith(("extensionalities/", "crates/", ".cargo/"))
        or path in {"Cargo.toml", "Cargo.lock"}
        or path.startswith("rust-toolchain")
        or path.startswith(".github/workflows/glassbox-")
    )


def needs_checks(root, base, head, force=False):
    if force or not base or not head or set(base) == {"0"}:
        return True
    result = subprocess.run(
        ["git", "-C", str(root), "diff", "--no-renames", "--name-only", "-z", base, head, "--"],
        capture_output=True,
        timeout=30,
    )
    if result.returncode:
        return True
    return any(relevant(os.fsdecode(p)) for p in result.stdout.split(b"\0") if p)


def run(*args):
    subprocess.run(args, cwd=ROOT, check=True)


def workflows():
    # Pin actionlint; isolate Go's module/build caches from application sources.
    cache = ROOT / "extensionalities/target/workflow-lint"
    env = {
        **os.environ, "GOPATH": str(cache / "go"),
        "GOMODCACHE": str(cache / "modules"), "GOCACHE": str(cache / "build"),
    }
    subprocess.run(
        ["go", "run", "github.com/rhysd/actionlint/cmd/actionlint@v1.7.12",
         "-shellcheck=", "-pyflakes=",
         ".github/workflows/glassbox-ci.yml",
         ".github/workflows/glassbox-upstream-sync.yml"],
        cwd=ROOT, env=env, check=True, timeout=240,
    )


def smoke():
    """Exercise release inspect/extract behavior, metadata batching, and offline OCR.

    Accepts no parameters and resolves release binaries through CARGO_TARGET_DIR.
    Returns unit after asserting v2 inspection and canonical native-unit extraction.
    Raises subprocess errors or AssertionError for contract, alias, or exit failures.
    """
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "extensionalities/target"))
    if not target.is_absolute():
        target = ROOT / target
    fixture = ROOT / "extensionalities/fixtures/hello.txt"
    reports = []
    for name in ("glassbox", "litx"):
        executable = target / "release" / name
        for command in ("inspect", "extract"):
            output = subprocess.check_output(
                [str(executable), command, str(fixture)], cwd=ROOT, timeout=30
            )
            report = json.loads(output)
            if command == "inspect":
                assert report["schema_version"] == 2 and report["kind"] == "inspect"
                assert report["metadata"]["status"] == "complete"
                assert not report["metadata"]["diagnostics"], report["metadata"]
                assert "text" not in report and "errors" not in report
            else:
                assert report["schema_version"] == 2 and report["kind"] == "extract"
                assert report["metadata"]["status"] == "complete"
                assert not report["metadata"]["diagnostics"], report["metadata"]
                content = "\n".join(
                    unit.get("text") or "" for unit in report["content"]["units"]
                )
                assert content.strip() == fixture.read_text().strip()
            reports.append(report)
        output = subprocess.check_output(
            [str(executable), "inspect", "--batch"],
            input=json.dumps({"id": "smoke", "path": str(fixture)}).encode() + b"\n",
            cwd=ROOT,
            timeout=30,
        )
        response = json.loads(output)
        assert response["id"] == "smoke"
        assert response["report"]["schema_version"] == 2
        assert "text" not in response["report"]
    assert reports[0] == reports[2] and reports[1] == reports[3]
    with tempfile.TemporaryDirectory(
        prefix="ci-ocr-", dir=ROOT / "extensionalities/target"
    ) as directory:
        # An explicitly missing model must never cause a runtime download.
        env = {**os.environ, "TESSDATA_PREFIX": str(Path(directory) / "missing")}
        completed = subprocess.run(
            [str(target / "release/glassbox"), "extract", "--identify",
             str(ROOT / "extensionalities/fixtures/native.pdf")],
            cwd=ROOT, env=env, timeout=30, capture_output=True, check=False,
        )
        assert completed.returncode == 2, completed.stderr
        report = json.loads(completed.stdout)
        assert report["kind"] == "extract" and report["completeness"] == "partial"
        assert any(item["code"] == "pdf_ocr"
                   for item in report["metadata"]["diagnostics"])
        assert "Native PDF evidence" in report["content"]["units"][0]["text"]
    print("PASS: metadata inspection, canonical extraction, batching, and offline OCR")



def ci_network():
    """Return the explicit CI-mode network argument for driver builds.

    Accepts no parameters and inspects the standard CI environment markers.
    Returns ["--ci-network"] only when running inside a CI runner, where the
    parity Cargo builds and inventory-probe lock generation may need to fetch
    crates that the runner cache has not warmed yet; local runs stay offline
    by default. An inherited CARGO_NET_OFFLINE=true still forces offline
    inside the driver, so this argument never overrides an explicit
    offline preference.
    """
    if os.environ.get("CI") == "true" or os.environ.get("GITHUB_ACTIONS") == "true":
        return ["--ci-network"]
    return []


def parity():
    """Run V-NAME against the release binaries in an exclusive evidence directory.

    Accepts no parameters and resolves CARGO_TARGET_DIR or the downstream target default.
    Returns unit after the tracked parity driver exits successfully; inside a
    CI runner the driver receives the explicit --ci-network permission for
    its Cargo subprocesses.
    Raises subprocess.CalledProcessError for parity failure, TimeoutExpired for its 30-minute bound, or OSError for setup.
    """
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "extensionalities/target"))
    if not target.is_absolute():
        target = ROOT / target
    verification = ROOT / "extensionalities/target/verification"
    verification.mkdir(parents=True, exist_ok=True)
    evidence = verification / f"settling-in-bname-ci-{os.getpid()}-{time.time_ns()}"
    subprocess.run([
        "python3", "-B", "extensionalities/scripts/verify-settling-in.py", "parity",
        "--evidence-dir", str(evidence), "--skip-build",
        "--binary", str(target / "release/glassbox"),
        *ci_network(),
    ], cwd=ROOT, check=True, timeout=1800)
    print(f"PASS: upstream parity and future-command checks; evidence={evidence}")


def metadata():
    """Run V-META against the release binary with isolated source instrumentation.

    Accepts no parameters and resolves CARGO_TARGET_DIR or the downstream default.
    Returns unit after the tracked metadata driver exits successfully; inside
    a CI runner the driver receives the explicit --ci-network permission for
    its instrumented-copy Cargo build.
    Raises subprocess.CalledProcessError for verification failure, TimeoutExpired for its 30-minute bound, or OSError for setup.
    """
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "extensionalities/target"))
    if not target.is_absolute():
        target = ROOT / target
    verification = ROOT / "extensionalities/target/verification"
    verification.mkdir(parents=True, exist_ok=True)
    evidence = verification / f"settling-in-bmeta-ci-{os.getpid()}-{time.time_ns()}"
    subprocess.run([
        "python3", "-B", "extensionalities/scripts/verify-settling-in.py", "metadata",
        "--evidence-dir", str(evidence), "--skip-build",
        "--binary", str(target / "release/glassbox"),
        *ci_network(),
    ], cwd=ROOT, check=True, timeout=1800)
    print(f"PASS: content-free metadata and forbidden-stage traps; evidence={evidence}")

def extract():
    """Run V-SELECT against the release binary and independent native-unit oracles.

    Accepts no parameters and resolves CARGO_TARGET_DIR or the downstream default.
    Returns unit after the tracked extraction driver exits successfully.
    Raises subprocess.CalledProcessError for verification failure, TimeoutExpired
    for its 30-minute bound, or OSError for setup and evidence creation.
    """
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "extensionalities/target"))
    if not target.is_absolute():
        target = ROOT / target
    verification = ROOT / "extensionalities/target/verification"
    verification.mkdir(parents=True, exist_ok=True)
    evidence = verification / f"settling-in-bextract-ci-{os.getpid()}-{time.time_ns()}"
    subprocess.run([
        "python3", "-B", "extensionalities/scripts/verify-settling-in.py", "extract",
        "--evidence-dir", str(evidence), "--skip-build",
        "--binary", str(target / "release/glassbox"),
    ], cwd=ROOT, check=True, timeout=1800)
    print(f"PASS: native-unit selection and public OCR; evidence={evidence}")

def main():
    """Dispatch one scoped downstream automation command.

    Accepts no parameters and reads the command name from process arguments.
    Returns unit after the selected command succeeds.
    Raises SystemExit for invalid arguments and propagates failed checks, missing tools, and timeouts.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "command",
        choices=("changes", "format", "check", "smoke", "parity", "metadata", "extract", "workflows"),
    )
    args = parser.parse_args()
    if args.command == "changes":
        required = needs_checks(
            ROOT, os.environ.get("BASE_SHA"), os.environ.get("HEAD_SHA"),
            os.environ.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
            or os.environ.get("HEAD_BRANCH", "").startswith("automation/upstream-sync/"),
        )
        value = str(required).lower()
        output = os.environ.get("GITHUB_OUTPUT")
        if output:
            with open(output, "a", encoding="utf-8") as stream:
                stream.write(f"run={value}\n")
        print(f"Downstream checks required: {value}")
    elif args.command == "format":
        # --all also follows local path dependencies; name only our packages.
        run("cargo", "fmt", "--manifest-path", MANIFEST,
            "-p", "glassbox-cli", "-p", "glassbox-inspect", "--check")
    elif args.command == "workflows":
        workflows()
    elif args.command == "parity":
        parity()
    elif args.command == "metadata":
        metadata()
    elif args.command == "extract":
        extract()
    elif args.command == "check":
        run("python3", "-B", "-m", "unittest", "discover",
            "-s", "extensionalities/scripts/tests", "-v")
        run("cargo", "clippy", "--locked", "--manifest-path", MANIFEST,
            "--workspace", "--all-targets", "--no-deps", "--", "-D", "warnings")
        run("cargo", "test", "--locked", "--manifest-path", MANIFEST, "--workspace")
        run("cargo", "build", "--locked", "--release", "--manifest-path", MANIFEST,
            "--workspace", "--bins")
        smoke()
        parity()
        metadata()
        extract()
    else:
        smoke()


if __name__ == "__main__":
    main()

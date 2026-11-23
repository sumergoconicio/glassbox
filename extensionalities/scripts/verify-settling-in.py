#!/usr/bin/env python3
"""Module: Glassbox settling-in verification.
File: verify-settling-in.py.
Responsibility: Run bounded fixture, parity, and future verification cases with exclusive evidence.

Bounded verification driver for the settling-in feature (Glassbox).

Every case launches fresh child processes with per-run timeouts and records the
command, actual exit status, captured output hashes, duration, and peak RSS
into a unique evidence run directory. Nothing outside the permitted trees is
ever written, deleted, or overwritten:

  evidence:  extensionalities/target/verification/settling-in-*/<case>-<run-id>
  fixtures:  extensionalities/sandbox/settling-in-test-<run-id>
  builds:    extensionalities/target/settling-in-build-<run-id>
  parity:    extensionalities/target/settling-in-production-native-arm64
  build home: extensionalities/target/settling-in-build-home (persistent,
             isolated build-time HOME; tesseract-rs reuses $HOME/.tesseract-rs
             native archives without an architecture check and the existing
             cache is x86-64 while this host is aarch64)

Implemented cases:
  baseline    V-SETUP (task A-SETUP, wave-1). Fresh native release build of the
             unchanged source with Tesseract enabled, isolated synthetic
             fixture oracle, source/toolchain/executable hashes, production
             safety-cap inventory, and fresh-process first-call plus
             heavier-input measurements with correctness checked against
             fixture construction, never against current output alone.

  parity      V-NAME (task B-NAME, wave-4). Unified Glassbox/litx versus
             real upstream lit, complete Clap inventories, isolated future
             command/flag mutation, file effects, and downstream regressions.

  metadata    V-META (task B-META, wave-6). Final-binary family coverage,
             schema/digest/count oracles, content sentinels, output modes,
             thumbnails, legacy flags, and isolated forbidden-stage traps.

  extract     V-SELECT (task B-EXTRACT, wave-7). Native-unit full/range
             selection, independent text oracles, partial/failed exits, OCR
             retention and word boxes, persistence, batch, and inspect isolation.

  inventory  V-INVENTORY (task B-INVENTORY, wave-8) and status V-STATUS.
  batch      V-BATCH (task B-JOBS, wave-9).
  performance V-PERF (task B-PERF, wave-10). Bounded serial comparison of the
             current default release profile against at most two fixed
             factor-only candidates (`--config profile.release.lto=...`
             and `profile.release.codegen-units=...`; the canonical
             Cargo.toml is never modified). Frozen independent oracles and
             input fixture hashes are recorded in experiment-spec.json before
             any comparative run. Every timed scenario uses fresh processes
             with per-child wait4 rusage (wall, CPU, peak RSS, /proc io
             read_bytes sampling), reports worst case alongside medians, and
             requires byte-identical outputs across profile variants for
             deterministic scenarios. The default-profile clean-build cost is
             either measured directly or reused from the retained controller
             evidence when the provided production binary is validated by a
             no-op incremental build. The historical Phase A baseline is
             reconstructed read-only from revision 57e3afb via `git show`
             into a unique sandbox copy, built in its own isolated target, and
             measured only on workloads both contracts share, with the changed
             inspect/hash contracts recorded explicitly; this stage is
             opt-in through --historical-reconstruction and its cost is
             charged and reported separately from the 30-minute active budget.
             Missing required coverage, metrology, or budget fails closed;
             no adoption is performed by this driver. After the controller
             adopted the canonical release profile, the default comparison
             premise is gone and this mode fails closed; use
             --delivery-validation instead.

  delivery   V-PERF delivery validation (--delivery-validation on the
             performance case). C-TEST final gate: exactly one canonical
             rebuild of the cached production native target (an incremental
             no-op proves an already-canonical binary), the accepted-evidence
             chain (controller-accepted corrective-07 result, all seven
             retained prior attempt results in the ledger, Rust sources and
             dependencies unchanged except the profile-only Cargo.toml
             delta, the accepted thin+CGU1 factor tuple), remeasurement of
             the delivered binary on the frozen scenario matrix with the v2
             blocking clock and five fresh processes per decisive scenario,
             and whole-byte output comparison with the approved candidate
             binary on identical inputs. No candidate matrix, historical
             rebuild, or counterfactual profile is constructed; the original
             campaign budget deviation stays disclosed, never PASS.

Baseline prerequisites: parity consumes explicit --baseline-result and
--baseline-oracle parameters, the retained original V-SETUP defaults when
still present, or freshly constructed independent fixtures. The `all` case
propagates the fresh current-source baseline run to parity automatically; a
new baseline never claims the deleted original Phase A timings.

Every registered case is implemented; no case silently skips. The `all`
case fails while any required case fails or is blocked.

Usage:
  python3 -B extensionalities/scripts/verify-settling-in.py CASE
      --evidence-dir ABSOLUTE_PATH
      [--binary PATH] [--skip-build] [--build-timeout SECONDS]
      [--run-timeout SECONDS]
      [--baseline-result RESULT_JSON --baseline-oracle ORACLE_JSON]
      [--require-pinned-resources] [--require-base-revision]
      [--ci-network]
      [--historical-reconstruction] [--perf-replicates N >= 5]
      [--perf-candidates thin-lto,thin-lto-cgu1]
      [--reuse-candidate NAME=TARGET_DIR ...]
      [--reuse-historical TARGET_DIR]
      [--prior-attempt-result RESULT_JSON ...] [--allow-first-attempt]
      [--corrective-budget-s SECONDS]
      [--delivery-validation]

Reproducibility guards: the pinned four-CPU/nice-10/job-limit resource pin and
the authorized 57e3afb base revision are benchmark-reproducibility devices.
They are opt-in through --require-pinned-resources and --require-base-revision;
the default functional verification records the observed limits and the actual
checkout revision as evidence and runs on standard CI runners. The performance
case keeps its own mandatory resource guard because it is itself opt-in.
Parity Cargo builds and inventory-probe lock generation stay offline by
default for the same reproducibility; --ci-network is the explicit CI-mode
network permission for those subprocesses, and an inherited
CARGO_NET_OFFLINE=true always forces offline over it.

Exit codes: 0 = PASS, 1 = FAIL, 2 = BLOCKED or invalid invocation,
            3 = registered but explicitly unimplemented case.
"""

from __future__ import annotations

import argparse
import ctypes
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import secrets
import selectors
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import threading
import time
import tomllib
import zlib

SCRIPT_PATH = Path(__file__).resolve()
EXT = SCRIPT_PATH.parents[1]
REPO = SCRIPT_PATH.parents[2]
VERIFICATION_ROOT = EXT / "target" / "verification"
BUILD_ROOT = EXT / "target"
SANDBOX_ROOT = EXT / "sandbox"
EVIDENCE_PREFIX = "settling-in-"
SANDBOX_PREFIX = "settling-in-test-"
BUILD_PREFIX = "settling-in-build-"
GENERATOR = EXT / "fixtures" / "generate.py"
DEFAULT_BUILD_TIMEOUT = 1200
DEFAULT_RUN_TIMEOUT = 120
MEASUREMENT_GRACE_S = 30
PARITY_ACTIVE_BUDGET_S = 30 * 60
PRODUCTION_BUILD_TARGET = BUILD_ROOT / "settling-in-production-native-arm64"
BASELINE_RESULT = (VERIFICATION_ROOT / "settling-in-asetup-20261122" /
                   "baseline-20261122T004537Z-738169-5a3fa02e/result.json")
BASELINE_ORACLE = (SANDBOX_ROOT / "settling-in-test-20261122T004537Z-738169-05076436" /
                   "oracle.json")

# Per-child measurement wrapper: runs the command with bounded timeout,
# streams its stdout/stderr into files, and reports the child's own exit
# status plus per-child peak RSS from getrusage(RUSAGE_CHILDREN). Standard
# library only; kills the child on timeout.
MEASURE_WRAPPER = r"""
import json, resource, subprocess, sys, time
stdout_path, stderr_path, timeout_s = sys.argv[1], sys.argv[2], float(sys.argv[3])
command = sys.argv[4:]
status = None
timed_out = False
started = time.monotonic()
with open(stdout_path, "wb") as out, open(stderr_path, "wb") as err:
    try:
        completed = subprocess.run(command, stdin=subprocess.DEVNULL,
                                   stdout=out, stderr=err, timeout=timeout_s)
        status = completed.returncode
    except subprocess.TimeoutExpired:
        timed_out = True
duration = time.monotonic() - started
peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
print(json.dumps({"status": status, "timed_out": timed_out,
                  "duration_s": round(duration, 4), "peak_rss_kb": peak}))
"""


class CaseSpec:
    def __init__(self, name, verification_id, task, wave, implemented=False):
        self.name = name
        self.verification_id = verification_id
        self.task = task
        self.wave = wave
        self.implemented = implemented


CASES = {
    "baseline": CaseSpec("baseline", "V-SETUP", "A-SETUP", "wave-1", implemented=True),
    "parity": CaseSpec("parity", "V-NAME", "B-NAME", "wave-4", implemented=True),
    "metadata": CaseSpec("metadata", "V-META", "B-META", "wave-6", implemented=True),
    "extract": CaseSpec("extract", "V-SELECT", "B-EXTRACT", "wave-7", implemented=True),
    "inventory": CaseSpec("inventory", "V-INVENTORY", "B-INVENTORY", "wave-8", implemented=True),
    "batch": CaseSpec("batch", "V-BATCH", "B-JOBS", "wave-9", implemented=True),
    "status": CaseSpec("status", "V-STATUS", "B-INVENTORY", "wave-8", implemented=True),
    "performance": CaseSpec("performance", "V-PERF", "B-PERF", "wave-10",
                            implemented=True),
}
ALL_SEQUENCE = ["baseline", "parity", "metadata", "extract", "inventory",
                "batch", "status", "performance"]

# Paths of the most recent completed baseline run, filled by
# run_baseline_case so run_all_case can propagate the fresh current-source
# V-SETUP prerequisite to parity without fabricating a historical receipt.
LAST_BASELINE_PATHS = {}

# Source scopes captured by the baseline source snapshot. The snapshot covers
# every regular file under these repository-relative roots.
SOURCE_SCOPES = (
    "extensionalities/Cargo.toml",
    "extensionalities/Cargo.lock",
    "extensionalities/crates",
    "extensionalities/scripts",
    "crates/liteparse",
    "crates/pdfium",
    "crates/pdfium-sys",
    "Cargo.toml",
    "Cargo.lock",
    ".gitignore",
)

# Production safety-cap constants inventoried from source. Values are read
# from the live files with their cfg attributes; nothing is assumed.
CAP_FILES = {
    "extensionalities/crates/litx-inspect/src/pdf.rs": (
        "MAX_PAGES", "MAX_PREFLIGHT", "MAX_TEXT_BYTES", "MAX_OCR_STAGING_BYTES"),
    "extensionalities/crates/litx-inspect/src/archive.rs": (
        "MAX_ENTRIES", "MAX_ENTRY_BYTES", "MAX_TOTAL_BYTES"),
    "extensionalities/crates/litx-inspect/src/profile.rs": (
        "MAX_COLUMNS", "MAX_HASHES", "WORKBOOK_CELL_CAP", "WORKBOOK_VALUE_BYTE_CAP"),
    "extensionalities/crates/litx-inspect/src/xml_profile.rs": (
        "INPUT_CAP", "ELEMENT_CAP", "DEPTH_CAP", "ATTRIBUTE_CAP", "NAME_CAP",
        "PATH_CAP", "PAIR_CAP", "RETAINED_BYTE_CAP", "PROFILE_BYTE_CAP",
        "SUMMARY_CAP"),
    "extensionalities/crates/litx-inspect/src/json_profile.rs": (
        "LOOP_FIELDS_CAP", "LOOP_FIELD_BYTES_CAP"),
    "extensionalities/crates/litx-inspect/src/spreadsheet.rs": (
        "MAX_ROWS", "WORKBOOK_SHEET_CAP"),
    "extensionalities/crates/litx-inspect/src/tabular.rs": ("MAX_BYTES",),
    "extensionalities/crates/litx-inspect/src/parquet_rows.rs": (
        "PAGE_CAP", "CHUNK_CAP", "READ_CAP"),
    "extensionalities/crates/litx-inspect/src/odf.rs": ("ODF_EXPANDED_CAP",),
    "crates/liteparse/src/config.rs": (
        "MAX_TARGET_PAGES", "DEFAULT_PAGE_BATCH_SIZE", "DEFAULT_DPI"),
}
CAP_CONST_RE = re.compile(
    r"^\s*(?:pub(?:\(crate\))?\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*"
    r"([A-Za-z0-9_:<>]+)\s*=\s*([^;]+);")
CFG_ATTR_RE = re.compile(r"#\[cfg\((.*)\)\]\s*$")

# Synthetic fixture oracle. Every expected value below is fixed by fixture
# construction and is independent of any implementation output.
OCR_TEXT = "GLASSBOX OCR SENTINEL EVIDENCE BOUND"
OCR_WORDS = OCR_TEXT.split()
MIXED_SENTINEL = "MIXED DIGITAL SENTINEL 2026"
DOCX_SENTINELS = ("native text layer with an ampersand", "Section one ends here.")
XLSX_SHEETS = ("Summary", "Notes")
PLAIN_LINES = tuple(f"PLAIN LINE {i} TOKEN 2026" for i in range(1, 13))
BIG_TXT_LINES = 100_000
DATA_JSON = (
    '{\n'
    '  "schema_name": "settlement_baseline_v1",\n'
    '  "customer_id": "cust-2026-001",\n'
    '  "records": [\n'
    '    {"order_id": 1, "status": "settled", "notes": "JSON BODY SENTINEL 2026"},\n'
    '    {"order_id": 2, "status": "pending", "notes": "Second record for profile loops."}\n'
    '  ]\n'
    '}\n'
)
DOC_XML = (
    '<?xml version="1.0" encoding="UTF-8"?>\n'
    '<settlement xmlns="urn:glassbox:settling-in">\n'
    '  <customer id="cust-2026-001">\n'
    '    <notes>XML BODY SENTINEL 2026</notes>\n'
    '    <status>settled</status>\n'
    '  </customer>\n'
    '</settlement>\n'
)


class DriverError(Exception):
    def __init__(self, message, exit_code=2):
        super().__init__(message)
        self.exit_code = exit_code


class ParityConflict(Exception):
    """Report a demonstrated output, status, inventory, or isolation mismatch."""


# ---------------------------------------------------------------- utilities

def utc_stamp():
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def run_id():
    return f"{utc_stamp()}-{os.getpid()}-{secrets.token_hex(4)}"


def sha256_bytes(data):
    if isinstance(data, str):
        data = data.encode()
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def write_exclusive(path, data):
    """Create a new file; refuse to overwrite anything that exists."""
    if isinstance(data, str):
        data = data.encode()
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    try:
        handle = os.open(path, flags, 0o600)
    except FileExistsError as error:
        raise DriverError(f"refusing to overwrite existing file: {path}") from error
    with os.fdopen(handle, "wb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def mkdir_new(path, mode=0o700):
    try:
        os.mkdir(path, mode)
    except FileExistsError as error:
        raise DriverError(f"refusing to reuse existing directory: {path}") from error
    return Path(path)


def ensure_dir(path, mode=0o700):
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(path, mode)
    except OSError:
        pass
    return path


def validate_evidence_dir(raw):
    """Require an absolute path inside the permitted verification tree."""
    verification = VERIFICATION_ROOT
    value = Path(raw)
    if not value.is_absolute():
        raise DriverError(
            f"evidence dir must be absolute: {raw!r} is relative")
    normalized = Path(os.path.normpath(str(value)))
    try:
        normalized.relative_to(verification)
    except ValueError as error:
        raise DriverError(
            f"evidence dir must live inside {verification}, got: {normalized}"
        ) from error
    if not normalized.name.startswith(EVIDENCE_PREFIX):
        raise DriverError(
            f"evidence dir name must start with {EVIDENCE_PREFIX!r}: "
            f"{normalized.name!r}")
    # Refuse symlink escapes: the deepest existing ancestor must resolve
    # inside the permitted verification tree.
    existing = normalized
    while not existing.exists() and existing != existing.parent:
        existing = existing.parent
    try:
        existing.resolve().relative_to(verification.resolve())
    except ValueError as error:
        raise DriverError(
            f"evidence dir resolves outside the permitted tree: {normalized}"
        ) from error
    return normalized


def capture_tool(argv, timeout=20):
    """One output line of a tool version command, or an explicit failure."""
    try:
        completed = subprocess.run(argv, capture_output=True, timeout=timeout,
                                   text=True, errors="replace")
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"unavailable: {error}"
    if completed.returncode != 0 and not (completed.stdout or completed.stderr):
        return f"unavailable: exit {completed.returncode}"
    text = (completed.stdout or completed.stderr).strip()
    return text.splitlines()[0] if text else f"empty output (exit {completed.returncode})"


def git_output(repo, *arguments):
    completed = subprocess.run(
        ["git", "-C", str(repo), *arguments], capture_output=True,
        timeout=30, text=True, errors="replace")
    if completed.returncode != 0:
        return None
    return completed.stdout


def git_state(repo):
    head = git_output(repo, "rev-parse", "HEAD")
    branch = git_output(repo, "rev-parse", "--abbrev-ref", "HEAD")
    status = git_output(repo, "status", "--porcelain")
    tracked = git_output(repo, "ls-files", "--")
    state = {
        "head": head.strip() if head else "unknown",
        "branch": branch.strip() if branch else "unknown",
        "dirty_files": [line for line in (status or "").splitlines() if line],
        "tracked_files": len(tracked.splitlines()) if tracked else None,
    }
    state["clean"] = state["dirty_files"] == [] and state["head"] != "unknown"
    return state


def source_snapshot(repo):
    """Hash every in-scope source file; return manifest and tree hash."""
    entries = []
    for scope in SOURCE_SCOPES:
        target = repo / scope
        if target.is_file():
            entries.append((scope, target))
        elif target.is_dir():
            for path in sorted(target.rglob("*")):
                if path.is_file() and not path.is_symlink():
                    entries.append((str(path.relative_to(repo)), path))
        else:
            raise DriverError(f"source scope missing: {target}")
    manifest = []
    digest = hashlib.sha256()
    for relative, path in sorted(entries):
        file_hash = sha256_file(path)
        manifest.append({"path": relative, "sha256": file_hash,
                         "size": path.stat().st_size})
        digest.update(f"{file_hash}  {relative}\n".encode())
    return {
        "tree_sha256": digest.hexdigest(),
        "files": len(manifest),
        "total_bytes": sum(entry["size"] for entry in manifest),
        "manifest": manifest,
    }


def cap_inventory(repo):
    """Read production cap constants and CLI default lines from source."""
    caps = []
    for relative, names in sorted(CAP_FILES.items()):
        path = repo / relative
        if not path.is_file():
            caps.append({"file": relative, "error": "source file missing"})
            continue
        lines = path.read_text().splitlines()
        wanted = set(names)
        previous_attribute = None
        for number, line in enumerate(lines, 1):
            match = CFG_ATTR_RE.search(line)
            if match:
                previous_attribute = match.group(1)
                continue
            found = CAP_CONST_RE.match(line)
            if not found:
                if line.strip() and not line.lstrip().startswith("//"):
                    previous_attribute = None
                continue
            name, kind, value = found.group(1), found.group(2), found.group(3).strip()
            if name not in wanted:
                continue
            caps.append({
                "file": relative, "name": name, "type": kind, "value": value,
                "cfg": previous_attribute, "line": number,
            })
            previous_attribute = None
        missing = wanted - {cap["name"] for cap in caps
                            if cap.get("file") == relative}
        for name in sorted(missing):
            caps.append({"file": relative, "name": name, "error": "not found"})
    cli_lines = []
    cli_path = repo / "extensionalities/crates/litx/src/main.rs"
    if cli_path.is_file():
        pattern = re.compile(r"default_value_t|range\(1\.\.=|unwrap_or\(|then_some\(|Some\(5\)|64 << 20|16_000")
        for number, line in enumerate(cli_path.read_text().splitlines(), 1):
            if pattern.search(line):
                cli_lines.append({"line": number, "text": line.strip()})
    else:
        cli_lines.append({"error": "main.rs missing"})
    return {"constants": caps, "cli_default_source_lines": cli_lines}


def environment_snapshot():
    home = Path.home()
    tessdata_default = home / ".tesseract-rs" / "tessdata"
    tessdata_env = os.environ.get("TESSDATA_PREFIX") or None
    def list_dir(path):
        try:
            return sorted(entry.name for entry in path.iterdir())
        except OSError:
            return None
    return {
        "observed_utc": datetime.now(timezone.utc).isoformat(),
        "hostname": platform.node() or "unknown",
        "machine": platform.machine(),
        "platform": platform.platform(),
        "python": sys.version.split()[0],
        "python_executable": sys.executable,
        "rustc": capture_tool(["rustc", "--version"]),
        "cargo": capture_tool(["cargo", "--version"]),
        "tesseract_cli": capture_tool(["tesseract", "--version"]),
        "pdftoppm": capture_tool(["pdftoppm", "-v"]),
        "git": capture_tool(["git", "--version"]),
        "tessdata_default_dir": str(tessdata_default),
        "tessdata_default_files": list_dir(tessdata_default),
        "TESSDATA_PREFIX": tessdata_env,
        "system_tessdata_files": list_dir(Path("/usr/share/tesseract-ocr/5/tessdata")),
    }


# ------------------------------------------------------------- measurement

def measure(argv, stdout_path, stderr_path, timeout_s):
    """Run one fresh child with a bounded timeout and capture its resources.

    The wrapper bounds the child with subprocess's own timeout, kills it on
    expiry, and reports the child's exit status plus per-child peak RSS. The
    wrapper itself is bounded by the child timeout plus a fixed grace period.
    """
    wrapper_argv = [sys.executable, "-B", "-c", MEASURE_WRAPPER,
                    str(stdout_path), str(stderr_path), str(timeout_s), *argv]
    try:
        completed = subprocess.run(
            wrapper_argv, capture_output=True,
            timeout=timeout_s + MEASUREMENT_GRACE_S,
            text=True, errors="replace")
    except subprocess.TimeoutExpired as error:
        raise DriverError(
            f"measurement wrapper exceeded its own bound for {argv[0]}") from error
    if completed.returncode != 0:
        text = (completed.stderr or completed.stdout).strip()
        lines = [line for line in text.splitlines() if line.strip()]
        tail = lines[-1][:500] if lines else "no wrapper output"
        raise DriverError(
            f"measurement wrapper failed for {Path(argv[0]).name}: {tail}")
    try:
        record = json.loads(completed.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError) as error:
        raise DriverError(f"unparseable measurement output: {error}") from error
    return record


# ------------------------------------------------------------ PDF fixtures

def _pdf_escape(text):
    return text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def _pdf_stream(data, extra=b""):
    return (b"<< /Length " + str(len(data)).encode() + b" " + extra
            + b" >>\nstream\n" + data + b"\nendstream")


def _pdf_text_content(text, font_size=20):
    escaped = _pdf_escape(text)
    return f"BT /F1 {font_size} Tf 50 700 Td ({escaped}) Tj ET".encode()


def _pdf_many_line_content(lines, font_size=14, start_y=740, leading=28):
    parts = [f"BT /F1 {font_size} Tf 40 {start_y} Td"]
    for index, line in enumerate(lines):
        escaped = _pdf_escape(line)
        if index:
            parts.append("0 -%d Td" % leading)
        parts.append(f"({escaped}) Tj")
    parts.append("ET")
    return " ".join(parts).encode()


def build_pdf(page_specs, title="Glassbox settling-in synthetic fixture",
              author="Settling-in harness"):
    """Deterministic minimal PDF writer for self-authored fixtures.

    Each page spec is a dict with optional keys:
      "text": str, "font_size": int, "lines": [str],
      "image": {"width": int, "height": int, "rgb": bytes},
      "media": (width_pt, height_pt) defaulting to US Letter (612, 792).
    No timestamps are embedded; identical inputs give identical bytes.
    """
    catalog_no, pages_no, font_no = 1, 2, 3
    next_no = 4
    entries = []
    for spec in page_specs:
        content_no = next_no
        next_no += 1
        image_no = None
        if spec.get("image"):
            image_no = next_no
            next_no += 1
        page_no = next_no
        next_no += 1
        entries.append((page_no, content_no, image_no, spec))
    info_no = next_no

    objects = {}
    objects[catalog_no] = f"<< /Type /Catalog /Pages {pages_no} 0 R >>".encode()
    kids = b" ".join(f"{entry[0]} 0 R".encode() for entry in entries)
    objects[pages_no] = (b"<< /Type /Pages /Kids [" + kids + b"] /Count "
                         + str(len(entries)).encode() + b" >>")
    objects[font_no] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
    for page_no, content_no, image_no, spec in entries:
        media = spec.get("media", (612, 792))
        media_box = f"[0 0 {media[0]} {media[1]}]".encode()
        resources = b"<< /Font << /F1 %d 0 R >>" % font_no
        if image_no:
            resources += b" /XObject << /Im1 %d 0 R >>" % image_no
        resources += b" >>"
        content_parts = []
        image = spec.get("image")
        if image:
            content_parts.append(
                b"q %d 0 0 %d 0 0 cm /Im1 Do Q" % (media[0], media[1]))
        if spec.get("lines"):
            content_parts.append(
                _pdf_many_line_content(spec["lines"], spec.get("font_size", 14)))
        elif spec.get("text"):
            content_parts.append(
                _pdf_text_content(spec["text"], spec.get("font_size", 20)))
        content = b"\n".join(part for part in content_parts if part)
        objects[content_no] = _pdf_stream(content)
        if image:
            rgb = image["rgb"]
            extra = (b"/Type /XObject /Subtype /Image /Width %d /Height %d "
                     % (image["width"], image["height"])
                     + b"/ColorSpace /DeviceRGB /BitsPerComponent 8")
            objects[image_no] = _pdf_stream(rgb, extra)
        objects[page_no] = (
            b"<< /Type /Page /Parent %d 0 R /MediaBox " % pages_no + media_box
            + b" /Resources " + resources
            + b" /Contents %d 0 R >>" % content_no)
    objects[info_no] = (b"<< /Title (" + _pdf_escape(title).encode()
                        + b") /Author (" + _pdf_escape(author).encode() + b") >>")

    data = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for number in range(1, info_no + 1):
        offsets.append(len(data))
        data.extend(f"{number} 0 obj\n".encode() + objects[number] + b"\nendobj\n")
    xref = len(data)
    data.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode())
    for offset in offsets[1:]:
        data.extend(f"{offset:010} 00000 n \n".encode())
    data.extend(
        f"trailer\n<< /Size {len(offsets)} /Root 1 0 R /Info {info_no} 0 R >>\n"
        f"startxref\n{xref}\n%%EOF\n".encode())
    return bytes(data)


# ------------------------------------------------------------ PNG decoding

def _paeth_predictor(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def decode_png_rgb(path):
    """Decode a non-interlaced 8-bit RGB(A) PNG to raw RGB bytes.

    Standard library only. Used to embed a rasterized, text-bearing image
    into an image-only scanned PDF whose expected OCR text is known.
    """
    data = Path(path).read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise DriverError(f"not a PNG file: {path}")
    position = 8
    header = None
    idat = bytearray()
    while position + 8 <= len(data):
        length = struct.unpack(">I", data[position:position + 4])[0]
        kind = data[position + 4:position + 8]
        chunk = data[position + 8:position + 8 + length]
        position += 12 + length
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", chunk)
        elif kind == b"IDAT":
            idat.extend(chunk)
        elif kind == b"IEND":
            break
    if header is None:
        raise DriverError(f"PNG missing IHDR: {path}")
    width, height, depth, color_type, _comp, _filter, interlace = header
    if depth != 8 or interlace != 0 or color_type not in (2, 6):
        raise DriverError(
            f"unsupported PNG for fixture embedding: depth={depth} "
            f"color_type={color_type} interlace={interlace}")
    channels = 3 if color_type == 2 else 4
    raw = zlib.decompress(bytes(idat))
    stride = width * channels
    row_bytes = bytearray(stride)
    output = bytearray(width * height * 3)
    for y in range(height):
        offset = y * (stride + 1)
        filter_type = raw[offset]
        row = bytearray(raw[offset + 1:offset + 1 + stride])
        if filter_type == 0:
            pass
        elif filter_type == 1:
            for i in range(channels, stride):
                row[i] = (row[i] + row[i - channels]) & 0xFF
        elif filter_type == 2:
            for i in range(stride):
                row[i] = (row[i] + row_bytes[i]) & 0xFF
        elif filter_type == 3:
            for i in range(stride):
                left = row[i - channels] if i >= channels else 0
                row[i] = (row[i] + ((left + row_bytes[i]) >> 1)) & 0xFF
        elif filter_type == 4:
            for i in range(stride):
                left = row[i - channels] if i >= channels else 0
                up = row_bytes[i]
                up_left = row_bytes[i - channels] if i >= channels else 0
                row[i] = (row[i] + _paeth_predictor(left, up, up_left)) & 0xFF
        else:
            raise DriverError(f"unsupported PNG filter {filter_type} in {path}")
        row_bytes = row
        base = y * stride
        if channels == 3:
            output[base:base + stride] = row
        else:
            for x in range(width):
                pixel = x * 4
                target = base + x * 3
                output[target:target + 3] = row[pixel:pixel + 3]
    return width, height, bytes(output)


def rasterize_with_pdftoppm(pdf_path, output_prefix, dpi=150, timeout=60):
    """Rasterize the first page of a self-authored PDF with the system tool.

    The raster's expected text is exactly the text drawn into the source PDF,
    which is independent of the implementation under test.
    """
    completed = subprocess.run(
        ["pdftoppm", "-png", "-r", str(dpi), "-f", "1", "-l", "1",
         str(pdf_path), str(output_prefix)],
        capture_output=True, timeout=timeout)
    if completed.returncode != 0:
        raise DriverError(
            "pdftoppm failed: " + completed.stderr.decode(errors="replace")[:2000])
    prefix_path = Path(str(output_prefix))
    produced = sorted(prefix_path.parent.glob(prefix_path.name + "*.png"),
                      key=lambda item: item.name)
    if not produced:
        raise DriverError(f"pdftoppm produced no PNG for {pdf_path}")
    return produced[0]


# --------------------------------------------------------- fixture building

def build_fixtures(repo, sandbox_dir, log_note=None):
    """Create the isolated synthetic fixture set plus its oracle manifest.

    Reuses the self-authored generator (extensionalities/fixtures/generate.py)
    as an unmodified copy so its Office/archive fixtures are reproducible
    without touching the tracked fixture directory. New fixtures are built by
    this driver; every expected value is fixed by construction.
    """
    ensure_dir(sandbox_dir)
    synthetic = ensure_dir(sandbox_dir / "synthetic")
    generated = ensure_dir(sandbox_dir / "generated")
    provenance = {
        "sandbox_dir": str(sandbox_dir),
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "pdf_writer": "verify-settling-in.py build_pdf (self-authored, deterministic)",
        "office_archive_set": "extensionalities/fixtures/generate.py copied unmodified",
        "generate_py_sha256": sha256_file(GENERATOR),
        "rasterizer": capture_tool(["pdftoppm", "-v"]),
        "notes": log_note or "",
    }

    # 1. Reuse the self-authored generator in an isolated copy.
    shutil.copyfile(GENERATOR, generated / "generate.py")
    gen_run = subprocess.run(
        [sys.executable, "-B", str(generated / "generate.py")],
        cwd=str(generated), capture_output=True, timeout=120)
    if gen_run.returncode != 0:
        raise DriverError(
            "generate.py copy failed with exit "
            f"{gen_run.returncode}: {gen_run.stderr.decode(errors='replace')[:2000]}")
    parity = []
    for path in sorted(generated.iterdir()):
        if path.name == "generate.py":
            continue
        tracked = EXT / "fixtures" / path.name
        entry = {"name": path.name,
                 "sandbox_sha256": sha256_file(path)}
        if tracked.is_file():
            entry["tracked_sha256"] = sha256_file(tracked)
            entry["identical_to_tracked"] = (
                entry["sandbox_sha256"] == entry["tracked_sha256"])
        else:
            entry["tracked_sha256"] = None
            entry["identical_to_tracked"] = None
        parity.append(entry)
    provenance["generator_parity"] = parity

    def register(name, data, expects, generator):
        path = synthetic / name
        write_exclusive(path, data)
        record = {
            "path": str(path), "name": name, "size": len(data),
            "sha256": sha256_bytes(data), "generator": generator,
            "expects": expects,
        }
        return record

    oracle = {"fixtures": [], "provenance": provenance}
    fixtures = oracle["fixtures"]

    # 2. Digital PDFs with page-count sentinels (0/1/4/5/6/many pages).
    def digital_page(index):
        return {"text": f"DIGITAL PAGE {index} SENTINEL 2026"}

    fixtures.append(register(
        "digital-1p.pdf",
        build_pdf([digital_page(1)]),
        {"pages": 1, "sentinels": ["DIGITAL PAGE 1 SENTINEL 2026"],
         "kind": "digital"},
        "build_pdf"))
    fixtures.append(register(
        "digital-4p.pdf",
        build_pdf([digital_page(i) for i in range(1, 5)]),
        {"pages": 4, "sentinels": [f"DIGITAL PAGE {i} SENTINEL 2026" for i in range(1, 5)],
         "kind": "digital"},
        "build_pdf"))
    fixtures.append(register(
        "digital-5p.pdf",
        build_pdf([digital_page(i) for i in range(1, 6)]),
        {"pages": 5, "sentinels": [f"DIGITAL PAGE {i} SENTINEL 2026" for i in range(1, 6)],
         "kind": "digital"},
        "build_pdf"))
    fixtures.append(register(
        "digital-6p.pdf",
        build_pdf([digital_page(i) for i in range(1, 7)]),
        {"pages": 6, "sentinels": [f"DIGITAL PAGE {i} SENTINEL 2026" for i in range(1, 7)],
         "kind": "digital"},
        "build_pdf"))
    many_pages = [{"lines": [f"MANY PAGE {i} LINE {line} SENTINEL 2026"
                             for line in range(1, 4)]} for i in range(1, 13)]
    many_sentinels = [f"MANY PAGE {i} LINE 1 SENTINEL 2026" for i in range(1, 13)]
    fixtures.append(register(
        "many-12p.pdf", build_pdf(many_pages),
        {"pages": 12, "sentinels": many_sentinels, "kind": "digital_heavy"},
        "build_pdf"))
    fixtures.append(register(
        "blank-1p.pdf", build_pdf([{}]),
        {"pages": 1, "sentinels": [], "text_empty": True, "kind": "blank"},
        "build_pdf"))
    fixtures.append(register(
        "zero-page.pdf", build_pdf([]),
        {"pages": 0, "kind": "edge",
         "allowed_outcomes": ["zero-page report", "clean diagnostic failure"],
         "never": ["crash", "timeout"]},
        "build_pdf"))

    # 3. Scanned fixtures: rasterize self-authored text, embed image-only.
    # 20pt Helvetica keeps the 36-character sentinel inside the 612pt page
    # width (28pt clipped the final word at the page edge).
    scan_source = synthetic / "scan-source-1p.pdf"
    write_exclusive(scan_source, build_pdf([{"text": OCR_TEXT, "font_size": 20}]))
    raster = rasterize_with_pdftoppm(scan_source, synthetic / "scan-raster", dpi=150)
    width, height, rgb = decode_png_rgb(raster)
    provenance["scan_raster"] = {
        "path": str(raster), "width": width, "height": height,
        "sha256": sha256_file(raster),
        "expected_text": OCR_TEXT,
        "dpi": 150,
    }
    image = {"width": width, "height": height, "rgb": rgb}
    fixtures.append(register(
        "scanned-1p.pdf", build_pdf([{"image": image}]),
        {"pages": 1, "kind": "scanned",
         "ocr_words": OCR_WORDS, "native_text_absent": True},
        "build_pdf + pdftoppm raster of scan-source-1p.pdf"))
    fixtures.append(register(
        "mixed-2p.pdf",
        build_pdf([{"text": MIXED_SENTINEL}, {"image": image}]),
        {"pages": 2, "kind": "mixed",
         "sentinels": [MIXED_SENTINEL], "ocr_words": OCR_WORDS},
        "build_pdf + pdftoppm raster of scan-source-1p.pdf"))
    fixtures.append({
        "path": str(scan_source), "name": "scan-source-1p.pdf",
        "size": scan_source.stat().st_size, "sha256": sha256_file(scan_source),
        "generator": "build_pdf",
        "expects": {"pages": 1, "sentinels": [OCR_TEXT], "kind": "digital"},
    })
    fixtures.append({
        "path": str(raster), "name": raster.name,
        "size": raster.stat().st_size, "sha256": sha256_file(raster),
        "generator": "pdftoppm -r 150",
        "expects": {"kind": "raster", "expected_text": OCR_TEXT},
    })

    # 4. Plain-text fixtures.
    plain_text = "\n".join(PLAIN_LINES) + "\n"
    fixtures.append(register(
        "plain-lines.txt", plain_text,
        {"kind": "text", "exact_content": plain_text, "lines": list(PLAIN_LINES)},
        "driver literal"))
    big_lines = [f"BIG TEXT LINE {i:06d} SETTLING IN BASELINE SYNTHETIC CONTENT 2026"
                 for i in range(1, BIG_TXT_LINES + 1)]
    big_text = "\n".join(big_lines) + "\n"
    fixtures.append(register(
        "big.txt", big_text,
        {"kind": "text_heavy", "exact_content": True,
         "first_line": big_lines[0], "last_line": big_lines[-1],
         "lines": BIG_TXT_LINES},
        "driver literal"))

    # 5. JSON/XML with body sentinels and schema names.
    fixtures.append(register(
        "data.json", DATA_JSON,
        {"kind": "json",
         "schema_names": ["customer_id", "schema_name", "records"],
         "body_sentinel": "JSON BODY SENTINEL 2026",
         "body_sentinel_assertion": "observation only in baseline; asserted by "
                                    "the extract case (V-SELECT) in later waves"},
        "driver literal"))
    fixtures.append(register(
        "doc.xml", DOC_XML,
        {"kind": "xml",
         "schema_names": ["settlement", "customer", "notes", "status"],
         "body_sentinel": "XML BODY SENTINEL 2026",
         "body_sentinel_assertion": "observation only in baseline; asserted by "
                                    "the extract case (V-SELECT) in later waves"},
        "driver literal"))

    oracle_path = sandbox_dir / "oracle.json"
    write_exclusive(oracle_path, json.dumps(oracle, indent=2, sort_keys=True))
    oracle["oracle_path"] = str(oracle_path)
    return oracle


def fixture_by_name(oracle, name):
    for record in oracle["fixtures"]:
        if record["name"] == name:
            return record
    raise DriverError(f"fixture missing from oracle: {name}")


# ------------------------------------------------------------------- checks

def json_field(document, dotted):
    value = document
    for part in dotted.split("."):
        if not isinstance(value, dict) or part not in value:
            raise KeyError(dotted)
        value = value[part]
    return value


def apply_check(check, record, stdout_text, parsed_json):
    """Return (ok, detail) for one expectation against one run record."""
    kind = check["id"]
    try:
        if kind == "exit_zero":
            return record["status"] == 0, f"exit status {record['status']}"
        if kind == "exit_in":
            codes = check["codes"]
            return (record["status"] in codes,
                    f"exit status {record['status']} (allowed {codes})")
        if kind == "no_timeout":
            return (not record["timed_out"],
                    "timed out" if record["timed_out"] else "within timeout")
        if kind == "json_stdout":
            if parsed_json is None:
                return False, "stdout is not a JSON object"
            return True, "stdout parsed as JSON object"
        if kind == "json_field":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read " + check["path"]
            value = json_field(parsed_json, check["path"])
            if "equals" in check:
                return value == check["equals"], f"{check['path']} = {value!r}"
            return value is not None, f"{check['path']} = {value!r}"
        if kind == "json_field_empty_after_strip":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read " + check["path"]
            value = json_field(parsed_json, check["path"])
            if not isinstance(value, str):
                return False, f"{check['path']} is not a string: {value!r}"
            return value.strip() == "", f"{check['path']!r} stripped is empty"
        if kind == "stdout_contains":
            needle = check["needle"]
            haystack = stdout_text.lower() if check.get("case_insensitive") else stdout_text
            found = (needle.lower() if check.get("case_insensitive") else needle) in haystack
            return found, f"{'found' if found else 'missing'} {needle!r}"
        if kind == "stdout_not_contains":
            needle = check["needle"]
            found = needle in stdout_text
            return not found, f"{'absent' if not found else 'unexpectedly present'} {needle!r}"
        if kind == "errors_empty":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read errors"
            errors = json_field(parsed_json, "errors")
            return errors == [], f"errors = {errors!r}"
        if kind == "text_content_equals_file":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read text.content"
            content = json_field(parsed_json, "text")["content"]
            expected = Path(check["fixture"]).read_text()
            equal = content.strip() == expected.strip()
            return (equal,
                    "text.content matches fixture content" if equal
                    else f"content {len(content)} chars vs fixture "
                         f"{len(expected)} chars (first divergence at "
                         f"{next((i for i, (a, b) in enumerate(zip(content, expected)) if a != b), 'end')})")
        if kind == "ocr_words":
            words = check["words"]
            missing = [word for word in words
                       if word.lower() not in stdout_text.lower()]
            return (not missing, f"OCR words present: {words}" if not missing
                    else f"OCR words missing from output: {missing}")
        if kind == "json_top_keys":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot compare top-level keys"
            actual = sorted(parsed_json)
            expected = sorted(check["keys"])
            return (actual == expected,
                    f"top-level keys {actual} (expected {expected})")
        if kind == "digest_matches_file":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read digest"
            digest = json_field(parsed_json, check.get("digest_path", "metadata.digest.digest"))
            expected = sha256_file(check["fixture"])
            return (digest == expected,
                    f"digest {digest} (independent {expected})")
        if kind == "bytes_hashed_equals_file_size":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read bytes_hashed"
            value = json_field(parsed_json, "metadata.digest.bytes_hashed.value")
            expected = Path(check["fixture"]).stat().st_size
            return (value == expected,
                    f"bytes_hashed {value} (file size {expected})")
        if kind == "page_signal_count":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read page signals"
            signals = json_field(parsed_json, "metadata.native_page_signals")
            actual = len(signals) if isinstance(signals, list) else None
            return (actual == check["count"],
                    f"native_page_signals count {actual} (expected {check['count']})")
        if kind == "no_forbidden_content_fields":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot scan content fields"
            found = metadata_content_fields(parsed_json)
            return (not found, f"forbidden content fields: {found or 'none'}")
        if kind == "content_units_contain":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read content units"
            units = json_field(parsed_json, "content.units")
            joined = "\n".join(unit.get("text") or "" for unit in units)
            missing = [needle for needle in check["needles"]
                       if needle not in joined]
            return (not missing, f"unit text contains every sentinel" if not missing
                    else f"unit text missing sentinels: {missing}")
        if kind == "content_units_text_equals":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read content units"
            units = json_field(parsed_json, "content.units")
            actual = [unit.get("text") for unit in units]
            expected = check["texts"]
            return (actual == expected,
                    f"unit texts equal the independent expectation ({len(expected)} units)"
                    if actual == expected
                    else f"unit texts diverge; first mismatch at index "
                         f"{next((i for i, (a, b) in enumerate(zip(actual, expected)) if a != b), 'end')}")
        if kind == "content_units_text_equals_file":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read content units"
            units = json_field(parsed_json, "content.units")
            joined = "\n".join(unit.get("text") or "" for unit in units)
            expected = Path(check["fixture"]).read_text(encoding="utf-8")
            equal = joined.strip() == expected.strip()
            return (equal, "joined unit text matches the fixture content" if equal
                    else f"joined unit text {len(joined)} chars vs fixture "
                         f"{len(expected)} chars")
        if kind == "unit_count":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read content units"
            units = json_field(parsed_json, "content.units")
            return (len(units) == check["count"],
                    f"unit count {len(units)} (expected {check['count']})")
        if kind == "unit_text":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read content units"
            units = json_field(parsed_json, "content.units")
            index = check["index"]
            actual = units[index - 1].get("text") if 0 < index <= len(units) else None
            return (actual == check["equals"],
                    f"unit {index} text {actual!r} (expected {check['equals']!r})")
        if kind == "unit_texts_all_blank":
            if parsed_json is None:
                return False, "stdout is not JSON; cannot read content units"
            units = json_field(parsed_json, "content.units")
            return (bool(units) and all(not (unit.get("text") or "").strip()
                                        for unit in units),
                    "every returned unit text is blank"
                    if units else "no units returned")
    except KeyError as error:
        return False, f"field missing from JSON report: {error}"
    return False, f"unknown check kind: {kind}"


# ------------------------------------------------------------- baseline case

def define_baseline_runs(binary, oracle):
    """The serialized v2 baseline program: first calls, metadata, content.

    Accepts the delivered binary path and the independent fixture oracle.
    Returns the ordered run list. Every assertion is fixed by fixture
    construction or by the frozen v2 report contract: inspect runs assert
    metadata-only fields (envelope, streamed digest, hashed byte count,
    native page signals, absence of body content) while extract runs assert
    returned native-unit content. Expected content never derives from
    current implementation output.
    """
    def path(name):
        return fixture_by_name(oracle, name)["path"]

    generated = str(Path(oracle["provenance"]["sandbox_dir"]) / "generated")
    digital_sentinels = fixture_by_name(oracle, "digital-4p.pdf")["expects"]["sentinels"]
    many_sentinels = fixture_by_name(oracle, "many-12p.pdf")["expects"]["sentinels"]
    plain_lines = fixture_by_name(oracle, "plain-lines.txt")["expects"]["lines"]

    def inspect_metadata_checks(name, pages):
        """Build independent v2 inspect assertions for one PDF fixture.

        Accepts the oracle fixture name and expected native page-signal count.
        Returns check dictionaries with a digest recomputed from the file.
        """
        return [
            {"id": "exit_zero"}, {"id": "json_stdout"},
            {"id": "json_top_keys", "keys": ["schema_version", "kind", "metadata"]},
            {"id": "json_field", "path": "schema_version", "equals": 2},
            {"id": "json_field", "path": "kind", "equals": "inspect"},
            {"id": "json_field", "path": "metadata.status", "equals": "complete"},
            {"id": "json_field", "path": "metadata.digest.algorithm", "equals": "sha256"},
            {"id": "digest_matches_file", "fixture": path(name)},
            {"id": "bytes_hashed_equals_file_size", "fixture": path(name)},
            {"id": "page_signal_count", "count": pages},
            {"id": "no_forbidden_content_fields"},
            {"id": "stdout_not_contains", "needle": "SENTINEL"},
        ]

    def extract_content_checks(unit, available, indices, extra=()):
        """Build independent v2 extract assertions shared by content runs.

        Accepts the native selection unit, available count, returned indices,
        and additional checks. Returns envelope and selection checks.
        """
        checks = [
            {"id": "exit_zero"}, {"id": "json_stdout"},
            {"id": "json_field", "path": "schema_version", "equals": 2},
            {"id": "json_field", "path": "kind", "equals": "extract"},
            {"id": "json_field", "path": "metadata.status", "equals": "complete"},
            {"id": "json_field", "path": "selection.unit", "equals": unit},
        ]
        if available is not None:
            checks.append({"id": "json_field", "path": "selection.available.value",
                           "equals": available})
        if indices is not None:
            checks.append({"id": "json_field", "path": "selection.returned_indices",
                           "equals": indices})
        return checks + list(extra)

    runs = [
        {"name": "defaults_first_call", "first_call": True,
         "argv": [binary, "defaults"],
         "checks": [{"id": "exit_zero"}, {"id": "json_stdout"},
                    {"id": "json_field", "path": "ocr_enabled", "equals": True},
                    {"id": "json_field", "path": "ocr_language", "equals": "eng"}]},
        {"name": "inspect_first_call_1p", "first_call": True,
         "argv": [binary, "inspect", path("digital-1p.pdf")],
         "checks": inspect_metadata_checks("digital-1p.pdf", 1)},
        {"name": "extract_first_ocr_call", "first_call": True,
         "argv": [binary, "extract", path("scanned-1p.pdf")],
         "checks": extract_content_checks("physical_page", 1, [1], [
             {"id": "content_units_contain", "needles": OCR_WORDS},
             {"id": "json_field", "path": "completeness", "equals": "exact"},
         ])},
        {"name": "inspect_digital_4p",
         "argv": [binary, "inspect", path("digital-4p.pdf")],
         "checks": inspect_metadata_checks("digital-4p.pdf", 4)},
        {"name": "extract_digital_4p_full",
         "argv": [binary, "extract", path("digital-4p.pdf"), "--pages", "1-4", "--no-ocr"],
         "checks": extract_content_checks("physical_page", 4, [1, 2, 3, 4], [
             {"id": "content_units_contain", "needles": digital_sentinels},
             {"id": "json_field", "path": "completeness", "equals": "exact"},
             {"id": "json_field", "path": "selection.omitted", "equals": "none"},
         ])},
        {"name": "inspect_scanned_1p",
         "argv": [binary, "inspect", path("scanned-1p.pdf")],
         "checks": inspect_metadata_checks("scanned-1p.pdf", 1)},
        {"name": "inspect_mixed_2p",
         "argv": [binary, "inspect", path("mixed-2p.pdf")],
         "checks": inspect_metadata_checks("mixed-2p.pdf", 2)},
        {"name": "extract_mixed_2p",
         "argv": [binary, "extract", path("mixed-2p.pdf")],
         "checks": extract_content_checks("physical_page", 2, [1, 2], [
             {"id": "content_units_contain",
              "needles": [MIXED_SENTINEL, *OCR_WORDS]},
         ])},
        {"name": "extract_plain_lines_full",
         "argv": [binary, "extract", path("plain-lines.txt"), "--pages", "1-12", "--no-ocr"],
         "checks": extract_content_checks("logical_line", 12, list(range(1, 13)), [
             {"id": "content_units_text_equals", "texts": list(plain_lines)},
             {"id": "json_field", "path": "completeness", "equals": "exact"},
         ])},
        {"name": "extract_data_json",
         "argv": [binary, "extract", path("data.json")],
         "checks": [{"id": "exit_in", "codes": [0, 2]},
                    {"id": "json_stdout"},
                    {"id": "json_field", "path": "kind", "equals": "extract"},
                    {"id": "json_field", "path": "metadata.status", "equals": "partial"},
                    {"id": "json_field", "path": "selection.unit", "equals": "dataset_row"},
                    {"id": "json_field", "path": "selection.omitted", "equals": "safety_limit"},
                    {"id": "json_field", "path": "completeness", "equals": "partial"},
                    {"id": "content_units_contain",
                     "needles": ["customer_id", "JSON BODY SENTINEL 2026"]}],
         "observe": ["JSON BODY SENTINEL 2026"]},
        {"name": "extract_doc_xml",
         "argv": [binary, "extract", path("doc.xml")],
         "checks": [{"id": "exit_in", "codes": [0, 2]},
                    {"id": "json_stdout"},
                    {"id": "json_field", "path": "kind", "equals": "extract"},
                    {"id": "json_field", "path": "metadata.status", "equals": "partial"},
                    {"id": "json_field", "path": "selection.unit", "equals": "dataset_row"},
                    {"id": "content_units_contain",
                     "needles": ["settlement", "notes", "status"]},
                    {"id": "stdout_not_contains", "needle": "XML BODY SENTINEL"}],
         "observe": ["XML BODY SENTINEL 2026"]},
        {"name": "extract_generated_notes_docx",
         "argv": [binary, "extract", f"{generated}/notes.docx"],
         "checks": extract_content_checks("document_block", None, None, [
             {"id": "content_units_contain", "needles": list(DOCX_SENTINELS)},
         ])},
        {"name": "extract_generated_grid_xlsx",
         "argv": [binary, "extract", f"{generated}/grid.xlsx"],
         "checks": [{"id": "exit_zero"}, {"id": "json_stdout"},
                    {"id": "json_field", "path": "kind", "equals": "extract"},
                    {"id": "json_field", "path": "selection.unit", "equals": "worksheet"}],
         "observe": ["We can inspect the original source"]},
        {"name": "inspect_blank_1p",
         "argv": [binary, "inspect", path("blank-1p.pdf")],
         "checks": inspect_metadata_checks("blank-1p.pdf", 1)},
        {"name": "extract_blank_1p",
         "argv": [binary, "extract", path("blank-1p.pdf"), "--no-ocr"],
         "checks": extract_content_checks("physical_page", 1, [1], [
             {"id": "unit_texts_all_blank"},
             {"id": "json_field", "path": "completeness", "equals": "exact"},
             {"id": "json_field", "path": "selection.omitted", "equals": "none"},
             {"id": "stdout_not_contains", "needle": "SENTINEL"},
         ])},
        {"name": "inspect_zero_page_pdf",
         "argv": [binary, "inspect", path("zero-page.pdf")],
         "qualified": True,
         "checks": [{"id": "exit_in", "codes": [0, 1, 2, 64]}]},
        {"name": "inspect_generated_broken_pdf",
         "argv": [binary, "inspect", f"{generated}/broken.pdf"],
         "qualified": True,
         "checks": [{"id": "exit_in", "codes": [0, 1, 2, 64]}]},
        {"name": "extract_many_12p", "heavy": True,
         "argv": [binary, "extract", path("many-12p.pdf"), "--pages", "1-12", "--no-ocr"],
         "checks": extract_content_checks("physical_page", 12, list(range(1, 13)), [
             {"id": "content_units_contain", "needles": many_sentinels},
             {"id": "json_field", "path": "completeness", "equals": "exact"},
         ])},
        {"name": "extract_big_txt", "heavy": True,
         "argv": [binary, "extract", path("big.txt"), "--pages", "1-100000", "--no-ocr"],
         "checks": [{"id": "exit_zero"}, {"id": "json_stdout"},
                    {"id": "json_field", "path": "schema_version", "equals": 2},
                    {"id": "json_field", "path": "kind", "equals": "extract"},
                    {"id": "json_field", "path": "metadata.status", "equals": "complete"},
                    {"id": "json_field", "path": "selection.unit", "equals": "logical_line"},
                    {"id": "unit_count", "count": BIG_TXT_LINES},
                    {"id": "unit_text", "index": 1,
                     "equals": fixture_by_name(oracle, "big.txt")["expects"]["first_line"]},
                    {"id": "unit_text", "index": BIG_TXT_LINES,
                     "equals": fixture_by_name(oracle, "big.txt")["expects"]["last_line"]},
                    {"id": "content_units_text_equals_file", "fixture": path("big.txt")},
                    {"id": "json_field", "path": "completeness", "equals": "exact"}]},
        {"name": "help_snapshot",
         "argv": [binary, "--help"],
         "checks": [{"id": "exit_zero"}], "artifact": "help.txt"},
        {"name": "inspect_help_snapshot",
         "argv": [binary, "inspect", "--help"],
         "checks": [{"id": "exit_zero"}], "artifact": "inspect-help.txt"},
        {"name": "litx_alias_defaults",
         "argv": [str(Path(binary).with_name("litx")), "defaults"],
         "checks": [{"id": "exit_zero"}, {"id": "json_stdout"},
                    {"id": "json_field", "path": "ocr_enabled", "equals": True}]},
    ]
    return runs


def build_environment(build_target):
    """Isolated build-time HOME with the shared download caches pinned.

    tesseract-rs (default `build-tesseract` feature) stores and reuses native
    leptonica/tesseract archives under a fixed `$HOME/.tesseract-rs` path with
    no architecture check. On this host that cache holds x86-64 objects that
    break the aarch64 link. Redirecting HOME to a persistent isolated home
    under the permitted target tree forces a native aarch64 build there and
    leaves the existing cache untouched. CARGO_HOME, RUSTUP_HOME, and
    XDG_CACHE_HOME stay pinned to the real shared caches so registry,
    toolchain, and pdfium downloads are reused rather than re-fetched.
    """
    real_home = Path(os.environ.get("HOME") or Path.home())
    build_home = BUILD_ROOT / "settling-in-build-home"
    ensure_dir(build_home / ".tesseract-rs")
    env = dict(os.environ)
    env["CARGO_TARGET_DIR"] = str(build_target)
    env["HOME"] = str(build_home)
    env["CARGO_HOME"] = os.environ.get("CARGO_HOME") or str(real_home / ".cargo")
    env["RUSTUP_HOME"] = os.environ.get("RUSTUP_HOME") or str(real_home / ".rustup")
    env["XDG_CACHE_HOME"] = os.environ.get("XDG_CACHE_HOME") or str(real_home / ".cache")
    return env, {
        "build_home": str(build_home),
        "HOME": str(build_home),
        "CARGO_HOME": env["CARGO_HOME"],
        "RUSTUP_HOME": env["RUSTUP_HOME"],
        "XDG_CACHE_HOME": env["XDG_CACHE_HOME"],
        "rationale": "tesseract-rs reuses $HOME/.tesseract-rs native archives "
                     "without an architecture check; the existing cache on "
                     "this host is x86-64 while the host is aarch64, so the "
                     "build HOME is isolated under the permitted target tree "
                     "and the existing cache is never modified",
    }


def launch_session_process(argv, cwd=None, env=None, stdin=None,
                            stderr=subprocess.STDOUT):
    """Start one build or probe command as its own session process group.

    Accepts the argv list plus an optional working directory, environment,
    stdin handle, and stderr target (merged into stdout by default). Returns
    the Popen child, which leads a fresh session so that a later timeout can
    stop the entire build process group, including grandchildren such as
    rustc under cargo, with one group signal. Raises DriverError when the
    command cannot be launched at all; a None timeout never expires and the
    caller may wait unbounded through communicate(timeout=None).
    """
    try:
        return subprocess.Popen(
            [str(item) for item in argv], cwd=cwd, env=env, stdin=stdin,
            stdout=subprocess.PIPE, stderr=stderr, start_new_session=True)
    except OSError as error:
        raise DriverError(f"cannot launch {argv[0]}: {error}") from error


def stop_session_process_group(process):
    """Stop a session child's whole process group after a timeout.

    Accepts one Popen child started through launch_session_process. Returns
    the process-group id that received SIGKILL, or None when the child had
    already exited. The whole group is killed first so build grandchildren
    cannot outlive the timeout and keep writing to the retained logs; the
    direct child is killed again as a last-resort fallback for a group that
    was already reaped. Raises no exception when the group or child
    disappears concurrently; the caller still reaps through communicate().
    """
    pgid = None
    try:
        pgid = os.getpgid(process.pid)
    except OSError:
        pass
    if pgid is not None:
        try:
            os.killpg(pgid, signal.SIGKILL)
        except OSError:
            pass
    try:
        process.kill()
    except OSError:
        pass
    return pgid


def do_build(args, run_dir):
    """Fresh release build of the unchanged source in a dedicated target dir.

    Reuses the shared Cargo download cache. The existing default target
    directory and any existing release binary are never touched: the build
    writes only to a new extensionalities/target/settling-in-build-* tree.
    Each attempt runs as its own session process group; on a build timeout
    the whole group is stopped, the captured output is retained in the
    attempt log, and a clear DriverError names the retained log path. A
    --build-timeout of None keeps the previous no-timeout behavior and waits
    unbounded instead of killing the build.
    """
    build_target = BUILD_ROOT / f"{BUILD_PREFIX}{run_id()}"
    manifest = EXT / "Cargo.toml"
    base_argv = ["cargo", "build", "--locked", "--release",
                 "--manifest-path", str(manifest), "--workspace", "--bins"]
    env, env_record = build_environment(build_target)

    offline_probe = subprocess.run(
        ["cargo", "metadata", "--locked", "--offline",
         "--manifest-path", str(manifest), "--format-version", "1"],
        cwd=str(REPO), capture_output=True, timeout=120)
    attempts = []
    argv = list(base_argv)
    if offline_probe.returncode == 0:
        argv.append("--offline")

    def run_attempt(command, log_name):
        """Run one bounded build attempt in its own session process group.

        Accepts the attempt's argv and the exclusive log file name inside
        the run directory. Returns the attempt record and whether the
        attempt hit the build timeout. On timeout the whole build process
        group is stopped with one group signal, the output captured before
        the kill is still retained in the log with a timeout marker, and
        the recorded status reflects the group kill. Raises no exception so
        the caller keeps the attempt record and decides the policy.
        """
        log_path = run_dir / log_name
        attempt_started = time.monotonic()
        process = launch_session_process(command, cwd=str(REPO), env=env,
                                        stdin=subprocess.DEVNULL,
                                        stderr=subprocess.PIPE)
        timed_out = False
        try:
            stdout_bytes, stderr_bytes = process.communicate(
                timeout=args.build_timeout)
        except subprocess.TimeoutExpired as error:
            timed_out = True
            stop_session_process_group(process)
            remainder = process.communicate()
            stdout_bytes = ((getattr(error, "stdout", None) or b"")
                            + (remainder[0] or b""))
            stderr_bytes = ((getattr(error, "stderr", None) or b"")
                            + (remainder[1] or b""))
        duration = time.monotonic() - attempt_started
        write_exclusive(
            log_path,
            f"# argv: {' '.join(command)}\n"
            f"# exit: {process.returncode}\n"
            f"# timed_out: {timed_out}\n"
            f"# timeout_s: {args.build_timeout}\n"
            f"# duration_s: {duration:.2f}\n"
            f"# stdout:\n{stdout_bytes.decode(errors='replace')}\n"
            f"# stderr:\n{stderr_bytes.decode(errors='replace')}\n")
        return ({"argv": command, "status": process.returncode,
                 "timed_out": timed_out,
                 "duration_s": round(duration, 2), "log": str(log_path)},
                timed_out)

    attempt, timed_out = run_attempt(argv, "cargo-build.log")
    attempts.append(attempt)
    if timed_out:
        raise DriverError(
            f"cargo build timed out after {args.build_timeout}s; the whole "
            "build process group was stopped and the retained log is "
            f"{attempt['log']}")
    if attempt["status"] != 0 and "--offline" in argv:
        retry = [part for part in argv if part != "--offline"]
        attempt, timed_out = run_attempt(retry, "cargo-build-retry.log")
        attempts.append(attempt)
        if timed_out:
            raise DriverError(
                "cargo build retry without --offline timed out after "
                f"{args.build_timeout}s; the whole build process group was "
                f"stopped and the retained log is {attempt['log']}")
    record = {
        "build_target_dir": str(build_target),
        "build_environment": env_record,
        "attempts": attempts,
        "status": attempts[-1]["status"],
        "duration_s": attempts[-1]["duration_s"],
        "offline_probe_status": offline_probe.returncode,
        "default_target_dir_untouched": True,
        "existing_tesseract_cache_untouched": True,
        "tesseract_feature_source": "extensionalities/crates/litx/Cargo.toml "
                                    'liteparse = { path = "...", features = ["tesseract"] }',
    }
    return record, build_target, attempts[-1]["status"]


def run_baseline_case(args, evidence_dir):
    """V-SETUP baseline: fresh build, oracle fixtures, hashes, measurements.

    Accepts parsed driver arguments and an authorized evidence root. Returns
    the case exit code. This runner produces a fresh current-source baseline:
    the original timestamped Phase A V-SETUP evidence was deleted from the
    retained tree, so a new run never claims the original Phase A timings or
    a historical receipt; it records its own measurements and can serve as
    the new current prerequisite baseline for parity through explicit
    --baseline-result/--baseline-oracle propagation.
    """
    failures = []
    blockers = []
    run_dir = mkdir_new(evidence_dir / f"baseline-{run_id()}")
    result = {
        "handoff": {
            "handoff_schema": "chai-handoff/v1",
            "identity": "20261121_settling_in; C-TEST prerequisite baseline "
                        "(V-SETUP semantics); driver baseline case",
            "state": "Fresh current-source baseline executed under the "
                     "approved C-TEST validation scope.",
            "source": "extensionalities/scripts/verify-settling-in.py; "
                      "fixture oracle built per run; source snapshot below",
            "authority": "Bounded validation only: no production edits, no "
                         "Git mutations, no deletion; delivery checks remain "
                         "gated on controller review.",
            "evidence": "See runs, hashes, caps, and fixtures below; raw "
                        "outputs retained in this run directory.",
            "receipt": {"consumer": "main controller (master-drive)",
                        "status": "pending"},
        },
        "case": "baseline",
        "verification_id": "V-SETUP",
        "task": "A-SETUP",
        "baseline_identity": {
            "is_original_phase_a_record": False,
            "original_phase_a_evidence": "the original timestamped V-SETUP "
                "record (settling-in-asetup-20261122) was deleted from the "
                "retained tree; its timings are not reproduced or claimed "
                "here",
            "current_record": "fresh current-source baseline with v2 "
                "inspect/extract semantics; safe to use as the new current "
                "prerequisite baseline for parity via explicit parameters",
        },
        "status": None,
        "exit_code": None,
        "run_dir": str(run_dir),
        "timing_serial": True,
    }

    result["environment"] = environment_snapshot()
    result["git"] = git_state(REPO)
    snapshot = source_snapshot(REPO)
    write_exclusive(run_dir / "source-manifest.json",
                    json.dumps(snapshot["manifest"], indent=2))
    result["source"] = {
        "revision": result["git"]["head"],
        "branch": result["git"]["branch"],
        "clean": result["git"]["clean"],
        "dirty_files": result["git"]["dirty_files"],
        "tree_sha256": snapshot["tree_sha256"],
        "files_hashed": snapshot["files"],
        "total_source_bytes": snapshot["total_bytes"],
        "scopes": list(SOURCE_SCOPES),
        "manifest": str(run_dir / "source-manifest.json"),
    }
    result["safety_caps"] = cap_inventory(REPO)

    # Isolated synthetic fixture oracle.
    sandbox_dir = SANDBOX_ROOT / f"{SANDBOX_PREFIX}{run_id()}"
    try:
        oracle = build_fixtures(REPO, sandbox_dir)
    except DriverError as error:
        blockers.append({"stage": "fixtures", "detail": str(error)})
        return finalize(result, run_dir, failures, blockers, evidence_dir)
    result["fixtures"] = {
        "sandbox_dir": str(sandbox_dir),
        "oracle": oracle["oracle_path"],
        "fixture_count": len(oracle["fixtures"]),
        "generator_parity_all_identical": all(
            entry.get("identical_to_tracked") for entry in
            oracle["provenance"]["generator_parity"]),
        "scan_raster": oracle["provenance"]["scan_raster"],
    }

    # Fresh native build of unchanged source, or an explicitly provided binary.
    if args.skip_build:
        result["build"] = {"skipped": True,
                          "provided_binary": args.binary,
                          "note": "no build performed (--skip-build); "
                                  "measurements apply to the provided binary"}
        binary = Path(args.binary).resolve() if args.binary else None
    else:
        build_record, build_target, build_status = do_build(args, run_dir)
        result["build"] = build_record
        binary = build_target / "release" / "glassbox" if build_status == 0 else None
    if binary is None or not binary.is_file():
        blockers.append({
            "stage": "binary",
            "detail": (f"glassbox executable missing at {binary}; "
                       "see cargo build log in this run directory"),
        })
        return finalize(result, run_dir, failures, blockers, evidence_dir)
    if not os.access(binary, os.X_OK):
        blockers.append({"stage": "binary",
                         "detail": f"glassbox executable not runnable: {binary}"})
        return finalize(result, run_dir, failures, blockers, evidence_dir)
    result["binary"] = {
        "path": str(binary),
        "sha256": sha256_file(binary),
        "size": binary.stat().st_size,
        "litx_alias": {
            "path": str(binary.with_name("litx")),
            "sha256": (sha256_file(binary.with_name("litx"))
                       if binary.with_name("litx").is_file() else None),
        },
    }

    # Serialized runs: fresh-process first calls, correctness, heavier inputs.
    outputs_dir = mkdir_new(run_dir / "runs")
    run_records = []
    observations = []
    for spec in define_baseline_runs(str(binary), oracle):
        name = spec["name"]
        stdout_path = outputs_dir / f"{name}.stdout"
        stderr_path = outputs_dir / f"{name}.stderr"
        try:
            measured = measure(spec["argv"], stdout_path, stderr_path,
                              args.run_timeout)
        except DriverError as error:
            failures.append({"run": name, "check": "measurement",
                             "detail": str(error)})
            continue
        stdout_bytes = stdout_path.read_bytes() if stdout_path.exists() else b""
        stderr_bytes = stderr_path.read_bytes() if stderr_path.exists() else b""
        stdout_text = stdout_bytes.decode(errors="replace")
        parsed = None
        try:
            candidate = json.loads(stdout_text)
            if isinstance(candidate, dict):
                parsed = candidate
        except ValueError:
            parsed = None
        checks = []
        for check in [{"id": "no_timeout"}] + spec["checks"]:
            ok, detail = apply_check(check, measured, stdout_text, parsed)
            checks.append({"id": check["id"], "ok": bool(ok), "detail": detail})
            if not ok:
                failures.append({"run": name, "check": check["id"],
                                 "detail": detail})
        record = {
            "name": name,
            "argv": spec["argv"],
            "first_call": bool(spec.get("first_call")),
            "heavy": bool(spec.get("heavy")),
            "qualified": bool(spec.get("qualified")),
            "timeout_s": args.run_timeout,
            "status": measured["status"],
            "timed_out": bool(measured["timed_out"]),
            "duration_s": measured["duration_s"],
            "peak_rss_kb": measured["peak_rss_kb"],
            "stdout_bytes": len(stdout_bytes),
            "stdout_sha256": sha256_bytes(stdout_bytes),
            "stderr_bytes": len(stderr_bytes),
            "stderr": stderr_bytes.decode(errors="replace")[:2000],
            "checks": checks,
        }
        if "artifact" in spec:
            record["artifact"] = str(run_dir / spec["artifact"])
            write_exclusive(run_dir / spec["artifact"], stdout_text)
        for needle in spec.get("observe", []):
            observations.append({
                "run": name, "needle": needle,
                "present": needle.lower() in stdout_text.lower(),
            })
        run_records.append(record)
    result["runs"] = run_records
    result["observations"] = observations
    result["measurements"] = {
        "note": "All timings serialized in one process; no concurrent timing "
                "measurements ran during this case.",
        "first_call_defaults_s": next(
            (r["duration_s"] for r in run_records if r["name"] == "defaults_first_call"), None),
        "first_inspect_s": next(
            (r["duration_s"] for r in run_records if r["name"] == "inspect_first_call_1p"), None),
        "first_ocr_extract_s": next(
            (r["duration_s"] for r in run_records if r["name"] == "extract_first_ocr_call"), None),
        "heavy_extract_many_12p_s": next(
            (r["duration_s"] for r in run_records if r["name"] == "extract_many_12p"), None),
        "heavy_extract_big_txt_s": next(
            (r["duration_s"] for r in run_records if r["name"] == "extract_big_txt"), None),
        "peak_rss_kb_by_run": {r["name"]: r["peak_rss_kb"] for r in run_records},
    }
    result["binary"]["size_bytes"] = binary.stat().st_size

    # Expose the completed run's evidence paths for `all`-case propagation.
    # A blocked or failed run still records its paths; parity's prerequisite
    # validation refuses a non-PASS baseline, so propagation stays fail-closed.
    LAST_BASELINE_PATHS["result"] = str(run_dir / "result.json")
    LAST_BASELINE_PATHS["oracle"] = result["fixtures"]["oracle"]

    return finalize(result, run_dir, failures, blockers, evidence_dir)


# --------------------------------------------------------------- V-NAME parity


def parity_remaining(started):
    """Return the seconds left in the serialized parity campaign.

    Accepts started as the campaign's time.monotonic baseline.
    Returns a positive floating-point timeout bounded by the 30-minute campaign budget.
    Raises DriverError when the campaign has exhausted its active execution budget.
    """
    remaining = PARITY_ACTIVE_BUDGET_S - (time.monotonic() - started)
    if remaining <= 1:
        raise DriverError("parity campaign exhausted its 30-minute active budget")
    return remaining


def write_json_exclusive(path, value):
    """Write formatted JSON without replacing prior evidence.

    Accepts path as a new evidence file and value as a JSON-serializable object.
    Returns the Path written with restrictive create-new semantics.
    Raises TypeError for unsupported values or DriverError/OSError for write failures.
    """
    write_exclusive(path, json.dumps(value, indent=2, sort_keys=True) + "\n")
    return Path(path)


PARITY_REQUIRED_FIXTURES = (
    "digital-1p.pdf", "digital-4p.pdf", "plain-lines.txt", "scanned-1p.pdf")


def parity_validate_oracle(oracle):
    """Verify the independent parity fixtures inside one oracle document.

    Accepts a decoded fixture oracle. Returns a name-to-record mapping after
    confirming every required fixture exists and matches its recorded digest.
    Raises DriverError for absent, changed, or incomplete fixtures.
    """
    names = {record["name"]: record for record in oracle.get("fixtures", [])}
    for name in PARITY_REQUIRED_FIXTURES:
        record = names.get(name)
        if not record or not Path(record["path"]).is_file():
            raise DriverError(f"required independent parity fixture is missing: {name}")
        if sha256_file(record["path"]) != record["sha256"]:
            raise DriverError(f"independent parity fixture hash changed: {name}")
    return names


def load_parity_prerequisites(args):
    """Resolve parity's independent fixtures without fabricated receipts.

    Accepts parsed driver arguments. Returns a record describing the
    resolution mode with the baseline result (when a receipt exists) and the
    validated fixture oracle. Resolution order: explicit --baseline-result
    and --baseline-oracle parameters (validated as a PASS V-SETUP record),
    then the retained original V-SETUP defaults when still present, and
    finally fresh in-run independent fixture construction. The fresh mode
    never claims a V-SETUP receipt or any original Phase A timing.
    Raises DriverError for unreadable, failed, or mismatched prerequisites.
    """
    explicit_result = getattr(args, "baseline_result", None)
    explicit_oracle = getattr(args, "baseline_oracle", None)
    if explicit_result or explicit_oracle:
        if not (explicit_result and explicit_oracle):
            raise DriverError("--baseline-result and --baseline-oracle must "
                              "be supplied together")
        try:
            baseline = json.loads(Path(explicit_result).read_text(encoding="utf-8"))
            oracle = json.loads(Path(explicit_oracle).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise DriverError(f"cannot read explicit parity prerequisites: {error}") from error
        if baseline.get("status") != "PASS" or baseline.get("verification_id") != "V-SETUP":
            raise DriverError("explicit baseline prerequisite is not a PASS V-SETUP record")
        parity_validate_oracle(oracle)
        return {"mode": "explicit_v2_baseline",
                "baseline_result": str(explicit_result),
                "oracle_path": str(explicit_oracle),
                "baseline": baseline, "oracle": oracle,
                "note": "explicitly supplied current-source V-SETUP baseline; "
                        "the original Phase A record is not claimed"}
    if BASELINE_RESULT.is_file() and BASELINE_ORACLE.is_file():
        try:
            baseline = json.loads(BASELINE_RESULT.read_text(encoding="utf-8"))
            oracle = json.loads(BASELINE_ORACLE.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise DriverError(f"cannot read retained parity prerequisites: {error}") from error
        if baseline.get("status") != "PASS" or baseline.get("verification_id") != "V-SETUP":
            raise DriverError("retained V-SETUP baseline is not PASS")
        parity_validate_oracle(oracle)
        return {"mode": "retained_asetup_default",
                "baseline_result": str(BASELINE_RESULT),
                "oracle_path": str(BASELINE_ORACLE),
                "baseline": baseline, "oracle": oracle,
                "note": "retained original V-SETUP default"}
    # The retained original V-SETUP evidence is absent: construct fresh
    # independent fixtures in-run. No V-SETUP receipt is claimed, and no
    # historical timing is inherited; parity consumes only the fresh oracle.
    sandbox = SANDBOX_ROOT / f"{SANDBOX_PREFIX}{run_id()}" / "parity-prerequisites"
    oracle = build_fixtures(
        REPO, sandbox,
        "Fresh independent parity prerequisites; the retained original "
        "V-SETUP evidence is absent, no V-SETUP receipt is claimed, and no "
        "original Phase A timing is inherited")
    parity_validate_oracle(oracle)
    return {"mode": "fresh_independent_fixtures",
            "baseline_result": None,
            "oracle_path": oracle["oracle_path"],
            "baseline": None, "oracle": oracle,
            "note": "original V-SETUP evidence is absent; parity runs on "
                    "freshly constructed independent fixtures"}


def upstream_source_snapshot():
    """Hash the real tracked upstream manifests, lockfile, and crate sources.

    Accepts no parameters and inspects the authoritative repository with read-only Git commands.
    Returns the revision, per-file hashes, aggregate hash, and scoped status output.
    Raises DriverError when Git cannot inspect the checkout or a listed file cannot be hashed.
    """
    listed = subprocess.run(
        ["git", "-C", str(REPO), "ls-files", "-z", "--", "Cargo.toml", "Cargo.lock", "crates"],
        capture_output=True, timeout=30, check=False)
    if listed.returncode != 0:
        raise DriverError(f"cannot list upstream files: {listed.stderr.decode(errors='replace')}")
    files = {}
    aggregate = hashlib.sha256()
    for raw in sorted(filter(None, listed.stdout.split(b"\0"))):
        relative = raw.decode("utf-8")
        digest = sha256_file(REPO / relative)
        files[relative] = digest
        aggregate.update(relative.encode() + b"\0" + digest.encode("ascii") + b"\n")
    revision = git_output(REPO, "rev-parse", "HEAD")
    status = git_output(REPO, "status", "--short", "--", "Cargo.toml", "Cargo.lock", "crates")
    if revision is None or status is None:
        raise DriverError("cannot inspect upstream revision or scoped status")
    return {"revision": revision.strip(), "tree_sha256": aggregate.hexdigest(),
            "files_hashed": len(files), "files": files, "status_short": status}


RESOURCE_ENV_EXPECTED = {
    "CARGO_BUILD_JOBS": "2",
    "RUST_TEST_THREADS": "2",
    "CMAKE_BUILD_PARALLEL_LEVEL": "2",
    "OMP_NUM_THREADS": "2",
    "RAYON_NUM_THREADS": "2",
    "NUM_JOBS": "2",
}


def resource_limit_snapshot():
    """Capture the mandatory process-tree CPU, priority, and parallelism limits.

    Accepts no parameters and inspects this process and its inherited environment.
    Returns effective values plus a strict validity result for the authorized guard.
    Raises OSError only if the host does not expose Linux process affinity.
    """
    affinity = sorted(os.sched_getaffinity(0))
    limits = {key: os.environ.get(key) for key in RESOURCE_ENV_EXPECTED}
    nice = os.nice(0)
    valid = (affinity == [0, 1, 2, 3] and nice == 10 and
             limits == RESOURCE_ENV_EXPECTED)
    return {"affinity": affinity, "nice": nice, "limits": limits, "valid": valid}


def enforce_resource_pin(result, args):
    """Enforce the pinned-resource benchmark guard only when explicitly requested.

    Accepts the case result carrying resource_limits and the parsed driver
    arguments. Returns True when the pin was recorded as required. Raises
    DriverError only when --require-pinned-resources is supplied and the
    authorized four-CPU, nice-10, or job-limit pin is not effective; the
    default verification records the observed limits as evidence and
    continues on standard CI runners.
    """
    required = bool(getattr(args, "require_pinned_resources", False))
    result["resource_pin_required"] = required
    if required and not result["resource_limits"]["valid"]:
        raise DriverError("mandatory four-CPU, nice-10, or job-limit guard is not effective")
    return required


def enforce_base_revision(result, args, case_label):
    """Enforce the authorized-base-revision guard only when explicitly requested.

    Accepts the case result carrying upstream_before and handoff records, the
    parsed driver arguments, and the case label for diagnostics. Returns True
    when the guard was recorded as required. Raises DriverError only when
    --require-base-revision is supplied and the real checkout revision differs
    from the authorized base revision in the case handoff; the default
    verification records the actually observed revision in upstream_before and
    continues on any checkout.
    """
    required = bool(getattr(args, "require_base_revision", False))
    result["base_revision_required"] = required
    if required and result["upstream_before"]["revision"] != result["handoff"]["source"]["revision"]:
        raise DriverError(f"real checkout revision differs from the authorized "
                          f"{case_label} base revision")
    return required


def launched_process_resources(pid):
    """Capture one launched child's actual affinity and scheduling priority.

    Accepts a live child process ID and uses Linux process inspection without changing it.
    Returns observed values, or an explicit unavailable record if a short-lived child exits.
    Raises no exception for process disappearance; unexpected OS errors become unavailable evidence.
    """
    try:
        return {"pid": pid, "observed": True,
                "affinity": sorted(os.sched_getaffinity(pid)),
                "nice": os.getpriority(os.PRIO_PROCESS, pid)}
    except OSError as error:
        return {"pid": pid, "observed": False, "error": str(error)}


def parity_execute(argv, log_path, timeout, started, cwd=None, environment=None, input_bytes=None):
    """Execute one bounded build or probe and retain output and resource evidence.

    Accepts argv, a new log path, timeout, campaign start, optional cwd/environment,
    and optional bounded stdin bytes. The child leads its own session process
    group; on timeout the whole group is stopped so build grandchildren cannot
    survive the bound, and the captured output plus a TIMEOUT marker are still
    retained in the log. Returns command, status, duration, paths, and child limits.
    Raises DriverError for launch failure, timeout, budget exhaustion, or log write failure.
    """
    effective_timeout = max(1, min(int(timeout), int(parity_remaining(started))))
    before = time.monotonic()
    process = launch_session_process(
        argv, cwd=cwd, env=environment,
        stdin=subprocess.PIPE if input_bytes is not None else None)
    launched_resources = launched_process_resources(process.pid)
    try:
        output = process.communicate(input=input_bytes, timeout=effective_timeout)[0]
    except subprocess.TimeoutExpired as error:
        stop_session_process_group(process)
        remainder = process.communicate()[0]
        output = ((getattr(error, "stdout", None) or b"") + (remainder or b""))
        write_exclusive(log_path, (output or b"") + b"\nTIMEOUT\n")
        raise DriverError(f"parity process timed out after {effective_timeout}s: {argv}") from error
    duration = time.monotonic() - before
    write_exclusive(log_path, output)
    return {"argv": [str(item) for item in argv], "status": process.returncode,
            "duration_s": round(duration, 4), "log": str(log_path),
            "cwd": str(cwd) if cwd else None, "timeout_s": effective_timeout,
            "launched_process": launched_resources}


def parity_build_target(evidence_dir, label):
    """Return a campaign-isolated Cargo target with bounded dependency reuse.

    Accepts the new parity evidence directory and a fixed upstream, Glassbox, or inventory label.
    Returns one target per campaign and build family; original builds precede mutated builds.
    Raises DriverError for an unknown family or escape from the authorized target root.
    """
    if "inventory" in label:
        family = "inventory"
    elif "upstream-lit" in label:
        family = "upstream"
    elif "glassbox" in label:
        family = "glassbox"
    else:
        raise DriverError(f"unknown parity build family: {label}")
    target = PRODUCTION_BUILD_TARGET / "parity" / Path(evidence_dir).name / family
    resolved_root = PRODUCTION_BUILD_TARGET.resolve()
    resolved_target = target.resolve()
    if resolved_root not in resolved_target.parents:
        raise DriverError(f"unsafe parity build target: {target}")
    return target


def parity_network_permission(args):
    """Resolve the explicit network permission for parity Cargo subprocesses.

    Accepts the parsed driver arguments. Returns (networked, reason) where
    networked is True only when --ci-network was explicitly supplied and the
    caller did not inherit an explicit offline preference. Parity Cargo builds
    stay offline by default for benchmark reproducibility; an inherited
    CARGO_NET_OFFLINE=true always forces offline over the CI permission.
    """
    inherited = os.environ.get("CARGO_NET_OFFLINE")
    if inherited is not None and inherited.strip().lower() in ("1", "true", "yes"):
        return False, "inherited_explicit_offline"
    if bool(getattr(args, "ci_network", False)):
        return True, "ci_network_opt_in"
    return False, "default_offline"


def parity_cargo_build(manifest, evidence_dir, label, started, timeout,
                       extra_env=None, networked=False):
    """Build one exact source snapshot without cross-workspace Cargo reuse.

    Accepts manifest, evidence directory, label, campaign start, timeout,
    optional environment overrides, and the resolved network permission.
    Returns the successful build record and its unique target with
    environment evidence. Offline is the reproducible default; the explicit
    CI-mode network permission drops --offline and CARGO_NET_OFFLINE so cold
    runners may fetch missing crates, while genuine build failures still
    fail closed and are never masked.
    Raises DriverError for a failed build, missing executable, or unsafe build setup.
    """
    target = parity_build_target(evidence_dir, label)
    ensure_dir(target)
    env, env_record = build_environment(target)
    env["CARGO_NET_OFFLINE"] = "false" if networked else "true"
    if extra_env:
        env.update(extra_env)
    upstream = "upstream-lit" in label
    argv = ["cargo", "build", "--locked", "--release",
            "--manifest-path", str(manifest)]
    if not networked:
        argv.append("--offline")
    argv.extend(["--bin", "lit"] if upstream else ["--workspace", "--bins"])
    record = parity_execute(argv, evidence_dir / f"{label}-build.log", timeout,
                            started, cwd=Path(manifest).parent, environment=env)
    if record["status"] != 0:
        raise DriverError(f"Cargo build failed for {label}; see {record['log']}")
    return {"build": record, "target_dir": str(target),
            "environment": {**env_record,
                            "CARGO_NET_OFFLINE": env["CARGO_NET_OFFLINE"],
                            "extra_keys": sorted((extra_env or {}).keys()),
                            "profile": "release", "tesseract": "enabled"}}


def copy_upstream_workspace(destination):
    """Copy the upstream workspace into an isolated mutation sandbox.

    Accepts destination as a new directory beneath the campaign sandbox.
    Returns the copied workspace root Path.
    Raises DriverError for an existing destination and OSError for copy failures.
    """
    if destination.exists():
        raise DriverError(f"upstream test copy already exists: {destination}")
    destination.mkdir(parents=True, mode=0o700)
    for name in ("Cargo.toml", "Cargo.lock"):
        shutil.copy2(REPO / name, destination / name)
    shutil.copytree(REPO / "crates", destination / "crates")
    return destination


def copy_downstream_workspace(destination):
    """Copy the downstream workspace for a unified mutated-source build.

    Accepts destination as a new extensionalities directory in the campaign sandbox.
    Returns the copied downstream root Path.
    Raises DriverError for an existing destination and OSError for copy failures.
    """
    if destination.exists():
        raise DriverError(f"downstream test copy already exists: {destination}")
    destination.mkdir(parents=True, mode=0o700)
    for name in ("Cargo.toml", "Cargo.lock"):
        shutil.copy2(EXT / name, destination / name)
    shutil.copytree(EXT / "crates", destination / "crates")
    return destination


def create_inventory_probe(crate_dir, upstream_root):
    """Create a temporary upstream-inventory crate using the production generator.

    Accepts crate_dir as a new sandbox path and upstream_root as the selected real or copied checkout.
    Returns the generated probe manifest Path.
    Raises DriverError for an existing crate and OSError for file generation failures.
    """
    if crate_dir.exists():
        raise DriverError(f"inventory probe already exists: {crate_dir}")
    (crate_dir / "src").mkdir(parents=True, mode=0o700)
    shutil.copy2(EXT / "crates/litx/build.rs", crate_dir / "build.rs")
    shutil.copy2(upstream_root / "Cargo.lock", crate_dir / "Cargo.lock")
    liteparse = upstream_root / "crates/liteparse"
    manifest = f'''[package]
name = "glassbox-upstream-inventory"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
liteparse = {{ path = {json.dumps(str(liteparse))}, features = ["tesseract"] }}
clap = {{ version = "4.5.55", features = ["derive"] }}
serde_json = "1.0.149"
tokio = {{ version = "1.52.3", features = ["rt-multi-thread", "macros"] }}
web-time = "1.1.0"

[[bin]]
name = "upstream-inventory"
path = "src/main.rs"
'''
    write_exclusive(crate_dir / "Cargo.toml", manifest)
    write_exclusive(crate_dir / "src/main.rs",
                    'include!(concat!(env!("OUT_DIR"), "/upstream-inventory.rs"));\n')
    return crate_dir / "Cargo.toml"


def prepare_inventory_probe(upstream_root, crate_dir, evidence_dir, label,
                            started, timeout, networked=False):
    """Resolve and build one isolated upstream-inventory probe.

    Accepts upstream root, new crate directory, evidence directory, label,
    campaign start, timeout, and the resolved network permission. Returns
    the probe build record and executable Path. Offline lock resolution and
    builds are the reproducible default; the explicit CI-mode network
    permission drops --offline from both the lock generation and the probe
    build so cold runners may fetch, without masking genuine failures.
    Raises DriverError when lock resolution, generation, compilation, or executable discovery fails.
    """
    manifest = create_inventory_probe(crate_dir, upstream_root)
    target = parity_build_target(evidence_dir, label)
    ensure_dir(target)
    env, _ = build_environment(target)
    env.update({"CARGO_NET_OFFLINE": "false" if networked else "true",
                "GLASSBOX_UPSTREAM_ROOT": str(upstream_root)})
    lock_argv = ["cargo", "generate-lockfile",
                 "--manifest-path", str(manifest)]
    if not networked:
        lock_argv.insert(2, "--offline")
    lock = parity_execute(lock_argv,
                          evidence_dir / f"{label}-lock.log", timeout, started,
                          cwd=crate_dir, environment=env)
    if lock["status"] != 0:
        raise DriverError(f"inventory lock generation failed; see {lock['log']}")
    build = parity_cargo_build(manifest, evidence_dir, label, started, timeout,
                               {"GLASSBOX_UPSTREAM_ROOT": str(upstream_root)},
                               networked=networked)
    build["lock_generation"] = lock
    return build, Path(build["target_dir"]) / "release/upstream-inventory"


def mutate_upstream(upstream_root):
    """Add a hidden aliased command and parse option only to an isolated upstream copy.

    Accepts upstream_root as a copied workspace beneath extensionalities/sandbox.
    Returns the mutated source hash and exact synthetic mutation descriptions.
    Raises DriverError if the path escapes the sandbox or any mutation anchor drifts.
    """
    resolved = upstream_root.resolve()
    if SANDBOX_ROOT.resolve() not in resolved.parents:
        raise DriverError("refusing to mutate upstream outside the isolated sandbox")
    path = resolved / "crates/liteparse/src/main.rs"
    source = path.read_text(encoding="utf-8")
    mutations = [
        ("    #[command(hide = true)]\n    Extract(ExtractCommand),",
         "    /// Future hidden probe command\n    #[command(hide = true, alias = \"future-alias\")]\n    FutureProbe(FutureProbeCommand),\n    #[command(hide = true)]\n    Extract(ExtractCommand),"),
        ("#[derive(Args, Debug)]\nstruct ExtractCommand {",
         "#[derive(Args, Debug)]\nstruct FutureProbeCommand {\n    /// Future probe value\n    #[arg(long)]\n    future_flag: Option<String>,\n}\n\n#[derive(Args, Debug)]\nstruct ExtractCommand {"),
        ("    /// Include per-page complexity signals (the same `is-complex` reports) as a",
         "    /// Future parse probe option\n    #[arg(long, default_value = \"future-default\")]\n    future_parse_option: String,\n\n    /// Include per-page complexity signals (the same `is-complex` reports) as a"),
        ("        Commands::Parse(cmd) => {\n            let format = parse_output_format(&cmd.format)?;",
         "        Commands::Parse(cmd) => {\n            if cmd.future_parse_option != \"future-default\" {\n                eprintln!(\"future-parse-option={}\", cmd.future_parse_option);\n            }\n            let format = parse_output_format(&cmd.format)?;"),
        ("        Commands::Extract(cmd) => {",
         "        Commands::FutureProbe(cmd) => {\n            println!(\"future-probe:{}\", cmd.future_flag.unwrap_or_else(|| \"default\".to_string()));\n        }\n\n        Commands::Extract(cmd) => {"),
    ]
    for needle, replacement in mutations:
        if source.count(needle) != 1:
            raise DriverError(f"future mutation anchor count is not one: {needle[:80]!r}")
        source = source.replace(needle, replacement, 1)
    path.write_text(source, encoding="utf-8")
    return {"path": str(path), "sha256": sha256_file(path),
            "mutations": ["hidden future-probe command with future-alias and --future-flag",
                          "parse --future-parse-option with future-default default"]}


def fixture_record(oracle, name):
    """Resolve one fixture from the independently constructed oracle.

    Accepts oracle as the loaded fixture dictionary and name as the fixture identifier.
    Returns the matching fixture record dictionary.
    Raises DriverError when the independent fixture is absent.
    """
    for record in oracle["fixtures"]:
        if record["name"] == name:
            return record
    raise DriverError(f"required fixture is absent from independent oracle: {name}")


def snapshot_tree(root):
    """Capture all file effects beneath one parity case working directory.

    Accepts root as the case directory to inspect.
    Returns a sorted mapping from relative filenames to SHA-256 hashes.
    Raises ParityConflict for symlinks and OSError for unreadable file effects.
    """
    files = {}
    for path in sorted(Path(root).rglob("*")):
        if path.is_symlink():
            raise ParityConflict(f"unexpected symlink in parity output: {path}")
        if path.is_file():
            files[str(path.relative_to(root))] = sha256_file(path)
    return files


def brand_expected(data):
    """Apply only the two approved command-identity transformations.

    Accepts data as upstream stdout or stderr bytes.
    Returns bytes with Usage: lit and a leading lit version prefix changed to Glassbox.
    Raises no expected exceptions and never rewrites processing content or attribution.
    """
    output = data.replace(b"Usage: lit", b"Usage: glassbox")
    if output.startswith(b"lit "):
        output = b"glassbox " + output[4:]
    return output


def classify_parity_stream(expected, observed, branding=False):
    """Classify exact, approved-branding, or meaningful stream differences.

    Accepts expected and observed bytes plus a flag enabling command-identity branding.
    Returns exact, command_identity_branding_only, or meaningful_difference.
    Raises no expected exceptions and never treats processing-output changes as branding.
    """
    if expected == observed:
        return "exact"
    if branding and brand_expected(expected) == observed:
        return "command_identity_branding_only"
    return "meaningful_difference"


def first_difference(expected, observed):
    """Locate the first differing byte in two byte strings.

    Accepts expected and observed as bytes.
    Returns offset, lengths, and bounded hexadecimal context for diagnosis.
    Raises no expected exceptions for bytes-like inputs.
    """
    limit = min(len(expected), len(observed))
    offset = next((index for index in range(limit) if expected[index] != observed[index]), limit)
    return {"offset": offset, "expected_hex": expected[max(0, offset - 24):offset + 48].hex(),
            "observed_hex": observed[max(0, offset - 24):offset + 48].hex(),
            "expected_bytes": len(expected), "observed_bytes": len(observed)}


def capture_inventory(binary, evidence_dir, label, started, arguments=()):
    """Capture the complete generated Clap command and argument model.

    Accepts binary, evidence directory, label, campaign start, and optional route arguments.
    Returns the decoded command inventory and writes its exact JSON output.
    Raises DriverError for execution failure, timeout, or invalid inventory JSON.
    """
    env = dict(os.environ)
    env["GLASSBOX_PROBE_INVENTORY"] = "1"
    completed = subprocess.run([str(binary), *arguments], env=env, capture_output=True,
                               timeout=min(60, parity_remaining(started)), check=False)
    path = evidence_dir / f"{label}-inventory.json"
    write_exclusive(path, completed.stdout)
    if completed.returncode != 0:
        raise DriverError(f"inventory probe failed for {label}: {completed.stderr.decode(errors='replace')}")
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise DriverError(f"inventory probe returned invalid JSON for {label}: {error}") from error

def parity_cases(common_absolute):
    """Define unchanged-upstream comparisons from the independent fixture contract.

    Accepts common_absolute as one shared collision fixture path usable by every fresh process.
    Returns upstream and Glassbox argument pairs with statuses and output expectations.
    Raises no expected exceptions; fixture resolution occurs during case execution.
    """
    digital = [("digital-1p.pdf", "digital-1p.pdf")]
    collision = [("digital-1p.pdf", "screenshot")]
    return [
        {"name": "version", "upstream": ["--version"], "bridge": ["--version"], "branding": True},
        {"name": "parse_help", "upstream": ["parse", "--help"], "bridge": ["parse", "--help"], "branding": True},
        {"name": "parse_unknown_flag", "upstream": ["parse", "digital-1p.pdf", "--definitely-invalid"], "bridge": ["parse", "digital-1p.pdf", "--definitely-invalid"], "fixtures": digital, "branding": True},
        {"name": "missing_parse_file", "upstream": ["parse"], "bridge": ["parse"], "branding": True},
        {"name": "parse_unknown_token_is_file", "upstream": ["parse", "not-a-command"], "bridge": ["parse", "not-a-command"]},
        {"name": "parse_defaults", "upstream": ["parse", "digital-1p.pdf", "--quiet"], "bridge": ["parse", "digital-1p.pdf", "--quiet"], "fixtures": digital, "contains": ["DIGITAL PAGE 1 SENTINEL 2026"]},
        {"name": "parse_json_flags", "upstream": ["parse", "digital-4p.pdf", "--no-ocr", "--format", "json", "--max-pages", "2", "--target-pages", "1,2", "--quiet"], "bridge": ["parse", "digital-4p.pdf", "--no-ocr", "--format", "json", "--max-pages", "2", "--target-pages", "1,2", "--quiet"], "fixtures": [("digital-4p.pdf", "digital-4p.pdf")], "contains": ["DIGITAL PAGE 1 SENTINEL 2026", "DIGITAL PAGE 2 SENTINEL 2026"]},
        {"name": "parse_output_file_rejected_txt", "upstream": ["parse", "plain-lines.txt", "--no-ocr", "--quiet", "--output", "result.txt"], "bridge": ["parse", "plain-lines.txt", "--no-ocr", "--quiet", "--output", "result.txt"], "fixtures": [("plain-lines.txt", "plain-lines.txt")]},
        {"name": "parse_output_file_success_selected_pages", "upstream": ["parse", "digital-4p.pdf", "--no-ocr", "--quiet", "--target-pages", "1,2", "--output", "selected.txt"], "bridge": ["parse", "digital-4p.pdf", "--no-ocr", "--quiet", "--target-pages", "1,2", "--output", "selected.txt"], "fixtures": [("digital-4p.pdf", "digital-4p.pdf")], "expected_status": 0, "expected_outputs": {"selected.txt": {"contains": ["DIGITAL PAGE 1 SENTINEL 2026", "DIGITAL PAGE 2 SENTINEL 2026"], "not_contains": ["DIGITAL PAGE 3 SENTINEL 2026"]}}},
        {"name": "nested_screenshot_help", "upstream": ["screenshot", "--help"], "bridge": ["parse", "screenshot", "--help"], "branding": True},
        {"name": "nested_screenshot_effect", "upstream": ["screenshot", "digital-1p.pdf", "--output-dir", "shots", "--target-pages", "1", "--dpi", "72", "--quiet"], "bridge": ["parse", "screenshot", "digital-1p.pdf", "--output-dir", "shots", "--target-pages", "1", "--dpi", "72", "--quiet"], "fixtures": digital},
        {"name": "nested_batch_effect", "upstream": ["batch-parse", "input", "output", "--no-ocr", "--quiet"], "bridge": ["parse", "batch-parse", "input", "output", "--no-ocr", "--quiet"], "fixtures": [("plain-lines.txt", "input/plain-lines.txt")]},
        {"name": "nested_batch_effect_success_selected_page", "upstream": ["batch-parse", "input", "output", "--no-ocr", "--quiet", "--max-pages", "1"], "bridge": ["parse", "batch-parse", "input", "output", "--no-ocr", "--quiet", "--max-pages", "1"], "fixtures": [("digital-1p.pdf", "input/digital-1p.pdf"), ("digital-4p.pdf", "input/digital-4p.pdf")], "expected_status": 0, "expected_outputs": {"output/digital-1p.txt": {"contains": ["DIGITAL PAGE 1 SENTINEL 2026"]}, "output/digital-4p.txt": {"contains": ["DIGITAL PAGE 1 SENTINEL 2026"], "not_contains": ["DIGITAL PAGE 2 SENTINEL 2026"]}}},
        {"name": "nested_is_complex", "upstream": ["is-complex", "digital-1p.pdf", "--quiet", "--compact"], "bridge": ["parse", "is-complex", "digital-1p.pdf", "--quiet", "--compact"], "fixtures": digital},
        {"name": "hidden_extract_help", "upstream": ["extract", "--help"], "bridge": ["parse", "extract", "--help"], "branding": True},
        {"name": "hidden_extract_actual", "upstream": ["extract", "--pdf-path", "digital-1p.pdf", "--page-num", "1"], "bridge": ["parse", "extract", "--pdf-path", "digital-1p.pdf", "--page-num", "1"], "fixtures": digital, "contains": ["DIGITAL PAGE 1 SENTINEL 2026"]},
        {"name": "hidden_image_bounds_help", "upstream": ["image-bounds", "--help"], "bridge": ["parse", "image-bounds", "--help"], "branding": True},
        {"name": "explicit_nested_parse_help", "upstream": ["parse", "--help"], "bridge": ["parse", "parse", "--help"], "branding": True},
        {"name": "explicit_nested_parse_collision", "upstream": ["parse", "screenshot", "--no-ocr", "--quiet"], "bridge": ["parse", "parse", "screenshot", "--no-ocr", "--quiet"], "fixtures": collision, "extensionless_error": True},
        {"name": "nested_downstream_inspect_collision", "upstream": ["parse", "inspect", "--no-ocr", "--quiet"], "bridge": ["parse", "inspect", "--no-ocr", "--quiet"], "fixtures": [("digital-1p.pdf", "inspect")], "extensionless_error": True},
        {"name": "dot_slash_collision", "upstream": ["parse", "./screenshot", "--no-ocr", "--quiet"], "bridge": ["parse", "./screenshot", "--no-ocr", "--quiet"], "fixtures": collision, "extensionless_error": True},
        {"name": "absolute_collision", "upstream": ["parse", str(common_absolute), "--no-ocr", "--quiet"], "bridge": ["parse", str(common_absolute), "--no-ocr", "--quiet"], "extensionless_error": True},
        {"name": "double_dash_collision", "upstream": ["parse", "--no-ocr", "--", "screenshot"], "bridge": ["parse", "--no-ocr", "--", "screenshot"], "fixtures": collision, "extensionless_error": True},
        {"name": "double_dash_hyphen_filename", "upstream": ["parse", "--no-ocr", "--", "--strange"], "bridge": ["parse", "--no-ocr", "--", "--strange"], "fixtures": [("digital-1p.pdf", "--strange")], "extensionless_error": True},
        {"name": "bare_collision_reserves_command", "upstream": ["screenshot"], "bridge": ["parse", "screenshot"], "branding": True},
        {"name": "default_ocr_scanned", "upstream": ["parse", "scanned-1p.pdf", "--quiet"], "bridge": ["parse", "scanned-1p.pdf", "--quiet"], "fixtures": [("scanned-1p.pdf", "scanned-1p.pdf")], "contains": ["GLASSBOX", "OCR", "SENTINEL", "EVIDENCE", "BOUND"], "timeout": 180},
    ]


def mutation_cases():
    """Define future-command, future-alias, future-option, and collision comparisons.

    Accepts no parameters because fixtures come from the same independent oracle.
    Returns case definitions for the isolated mutated upstream and unified downstream pair.
    Raises no expected exceptions.
    """
    return [
        {"name": "future_hidden_command", "upstream": ["future-probe", "--future-flag", "MUTATED"], "bridge": ["parse", "future-probe", "--future-flag", "MUTATED"], "contains": ["future-probe:MUTATED"]},
        {"name": "future_hidden_alias", "upstream": ["future-alias", "--future-flag", "ALIASED"], "bridge": ["parse", "future-alias", "--future-flag", "ALIASED"], "contains": ["future-probe:ALIASED"]},
        {"name": "future_hidden_help", "upstream": ["future-probe", "--help"], "bridge": ["parse", "future-probe", "--help"], "branding": True},
        {"name": "future_parse_option", "upstream": ["parse", "digital-1p.pdf", "--no-ocr", "--quiet", "--future-parse-option", "MUTATED"], "bridge": ["parse", "digital-1p.pdf", "--no-ocr", "--quiet", "--future-parse-option", "MUTATED"], "fixtures": [("digital-1p.pdf", "digital-1p.pdf")], "contains": ["future-parse-option=MUTATED", "DIGITAL PAGE 1 SENTINEL 2026"]},
        {"name": "future_parse_help", "upstream": ["parse", "--help"], "bridge": ["parse", "--help"], "branding": True, "contains": ["--future-parse-option"]},
        {"name": "future_explicit_parse_collision", "upstream": ["parse", "future-probe", "--no-ocr", "--quiet"], "bridge": ["parse", "parse", "future-probe", "--no-ocr", "--quiet"], "fixtures": [("digital-1p.pdf", "future-probe")], "extensionless_error": True},
    ]


def execute_parity_case(case, binaries, oracle, evidence_dir, started, run_timeout):
    """Run fresh upstream, Glassbox, and litx processes and compare every observable effect.

    Accepts the case definition, executable mapping, fixture oracle, evidence root, start, and timeout.
    Returns a comparison record with per-side checks, streams, statuses, files, and independent assertions.
    Raises DriverError for execution infrastructure failures and ParityConflict for unsafe file trees.
    """
    case_root = evidence_dir / "cases" / case["name"]
    records = {}
    for side in ("upstream", "glassbox", "litx"):
        work = case_root / side / "work"
        work.mkdir(parents=True, mode=0o700)
        for fixture_name, local_name in case.get("fixtures", []):
            source = Path(fixture_record(oracle, fixture_name)["path"])
            destination = work / local_name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination)
        argv = list(case["upstream"] if side == "upstream" else case["bridge"])
        before = time.monotonic()
        env = dict(os.environ)
        env.pop("GLASSBOX_PROBE_INVENTORY", None)
        try:
            completed = subprocess.run(
                [str(binaries[side]), *argv], cwd=work, env=env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                timeout=min(case.get("timeout", run_timeout), parity_remaining(started)), check=False)
        except subprocess.TimeoutExpired as error:
            raise DriverError(f"parity case timed out: {case['name']} {side}") from error
        duration = time.monotonic() - before
        write_exclusive(case_root / side / "stdout.bin", completed.stdout)
        write_exclusive(case_root / side / "stderr.bin", completed.stderr)
        files = snapshot_tree(work)
        records[side] = {"status": completed.returncode, "stdout": completed.stdout,
                         "stderr": completed.stderr, "files": files,
                         "duration_s": round(duration, 4),
                         "argv": [str(binaries[side]), *argv]}
        write_json_exclusive(case_root / side / "execution.json", {
            "status": completed.returncode, "files": files, "duration_s": round(duration, 4),
            "argv": [str(binaries[side]), *argv],
            "stdout": f"{len(completed.stdout)} bytes retained at stdout.bin",
            "stderr": f"{len(completed.stderr)} bytes retained at stderr.bin"})
    upstream = records["upstream"]
    conflicts = []
    checks = []
    stream_results = {}
    independent_output_assertions = 0
    generated_outputs = {}
    input_names = {local_name for _, local_name in case.get("fixtures", [])}
    for side in ("upstream", "glassbox", "litx"):
        generated_outputs[side] = {}
        if "expected_status" in case:
            independent_output_assertions += 1
            if records[side]["status"] != case["expected_status"]:
                conflicts.append({"independent_status_expectation": {"side": side,
                                  "expected": case["expected_status"], "observed": records[side]["status"]}})
        for relative_name, expectation in case.get("expected_outputs", {}).items():
            independent_output_assertions += 1
            if relative_name in input_names:
                conflicts.append({"output_case_configuration_error": relative_name, "side": side})
                continue
            output_path = case_root / side / "work" / relative_name
            if not output_path.is_file():
                conflicts.append({"missing_generated_output": relative_name, "side": side})
                continue
            output_data = output_path.read_bytes()
            generated_outputs[side][relative_name] = {"sha256": sha256_bytes(output_data),
                                                       "bytes": len(output_data)}
            for expected_text in expectation.get("contains", []):
                independent_output_assertions += 1
                if expected_text.encode() not in output_data:
                    conflicts.append({"missing_independent_output_text": expected_text,
                                      "file": relative_name, "side": side})
            for forbidden_text in expectation.get("not_contains", []):
                independent_output_assertions += 1
                if forbidden_text.encode() in output_data:
                    conflicts.append({"unexpected_unselected_output_text": forbidden_text,
                                      "file": relative_name, "side": side})
        if case.get("extensionless_error"):
            expected_error = b'Error: Conversion("unsupported file format: .")\n'
            if (records[side]["status"] != 1 or records[side]["stdout"] != b"" or
                    records[side]["stderr"] != expected_error):
                conflicts.append({"source_derived_extensionless_expectation": {
                    "side": side, "expected_status": 1, "expected_stdout": "empty",
                    "expected_stderr": expected_error.decode()}})
        for assertion in case.get("contains", []):
            if assertion.encode() not in records[side]["stdout"] + records[side]["stderr"]:
                conflicts.append({"missing_oracle_content": assertion, "side": side})
        checks.append({"id": f"{side}_status", "ok": records[side]["status"] == upstream["status"]})
        checks.append({"id": f"{side}_file_effects", "ok": records[side]["files"] == upstream["files"]})
    for stream in ("stdout", "stderr"):
        expected = upstream[stream]
        stream_results[stream] = {}
        for side in ("glassbox", "litx"):
            observed = records[side][stream]
            classification = classify_parity_stream(
                expected, observed, case.get("branding", False))
            if classification == "meaningful_difference":
                conflicts.append({"stream": stream, "side": side,
                                  "difference": first_difference(
                                      brand_expected(expected) if case.get("branding") else expected,
                                      observed)})
            stream_results[stream][side] = classification
            checks.append({"id": f"{side}_{stream}", "ok": classification != "meaningful_difference"})
    if upstream["status"] != records["glassbox"]["status"]:
        conflicts.append({"status": {"side": "glassbox", "expected": upstream["status"],
                                     "observed": records["glassbox"]["status"]}})
    if upstream["status"] != records["litx"]["status"]:
        conflicts.append({"status": {"side": "litx", "expected": upstream["status"],
                                     "observed": records["litx"]["status"]}})
    for side in ("glassbox", "litx"):
        if upstream["files"] != records[side]["files"]:
            conflicts.append({"file_effects": {"side": side, "expected": upstream["files"],
                                               "observed": records[side]["files"]}})
    record = {
        "name": case["name"], "result": "FAIL" if conflicts else "PASS",
        "expected_observed": stream_results,
        "status": {side: records[side]["status"] for side in records},
        "file_effect_counts": {side: len(records[side]["files"]) for side in records},
        "generated_output_counts": {side: len(generated_outputs[side]) for side in records},
        "generated_outputs": generated_outputs,
        "independent_output_assertions": independent_output_assertions,
        "durations_s": {side: records[side]["duration_s"] for side in records},
        "checks": checks, "conflicts": conflicts, "evidence": str(case_root),
    }
    write_json_exclusive(case_root / "comparison.json", record)
    return record


def verify_downstream_routes(binaries, evidence_dir, started, run_timeout):
    """Check unified help, errors, inspection, extraction, defaults, and litx equivalence.

    Accepts executable paths, evidence root, campaign start, and per-process timeout.
    Returns fresh-process run records with independent checks and retained streams.
    Raises DriverError for launch or timeout failures and never hides a failed assertion.
    """
    fixture = EXT / "fixtures/hello.txt"
    definitions = [
        ("top_level_help", ["--help"], "help"),
        ("no_arguments", [], "error"),
        ("inspect", ["inspect", str(fixture)], "json"),
        ("extract", ["extract", str(fixture)], "json"),
        ("defaults", ["defaults"], "defaults"),
    ]
    output_root = evidence_dir / "downstream"
    records = []
    for name, arguments, kind in definitions:
        sides = {}
        for side in ("glassbox", "litx"):
            side_root = output_root / name / side
            side_root.mkdir(parents=True, mode=0o700)
            try:
                completed = subprocess.run([str(binaries[side]), *arguments], capture_output=True,
                                           timeout=min(run_timeout, parity_remaining(started)), check=False)
            except subprocess.TimeoutExpired as error:
                raise DriverError(f"downstream route timed out: {name} {side}") from error
            write_exclusive(side_root / "stdout.bin", completed.stdout)
            write_exclusive(side_root / "stderr.bin", completed.stderr)
            sides[side] = completed
        checks = [{"id": "alias_status", "ok": sides["glassbox"].returncode == sides["litx"].returncode},
                  {"id": "alias_stdout", "ok": sides["glassbox"].stdout == sides["litx"].stdout},
                  {"id": "alias_stderr", "ok": sides["glassbox"].stderr == sides["litx"].stderr}]
        for side, completed in sides.items():
            if kind == "help":
                expected = completed.returncode == 0 and all(
                    command.encode() in completed.stdout for command in ("parse", "inspect", "extract", "defaults"))
            elif kind == "error":
                # Contract from crates/litx/src/main.rs: Clap usage errors
                # (including the bare no-argument invocation) print the usage
                # diagnostic and terminate with exit 64; only --help/--version
                # exit 0.
                expected = completed.returncode == 64 and \
                    b"Usage: glassbox" in completed.stderr
            else:
                try:
                    report = json.loads(completed.stdout)
                except json.JSONDecodeError:
                    report = None
                expected = completed.returncode == 0 and isinstance(report, dict)
                if kind == "json" and isinstance(report, dict):
                    expected = expected and not report.get("errors")
                if kind == "defaults" and isinstance(report, dict):
                    expected = expected and report.get("ocr_enabled") is True
            checks.append({"id": f"{side}_independent_contract", "ok": bool(expected)})
        records.append({"name": name, "argv": arguments, "checks": checks,
                        "status": {side: completed.returncode for side, completed in sides.items()},
                        "evidence": str(output_root / name)})
        write_json_exclusive(output_root / name / "checks.json", records[-1])
    return records


def verify_relocated_installation(binaries, oracle, sandbox, evidence_dir, started, run_timeout):
    """Prove relocated Glassbox needs no sibling lit executable for exact parsing.

    Accepts original binaries, fixture oracle, campaign sandbox, evidence root, start, and timeout.
    Returns the relocated comparison record with executable identity and no-sibling evidence.
    Raises DriverError for setup failures and ParityConflict for unexpected installation contents.
    """
    install = sandbox / "install"
    install.mkdir(mode=0o700)
    installed = install / "glassbox"
    shutil.copy2(binaries["glassbox"], installed)
    if (install / "lit").exists():
        raise ParityConflict("unexpected sibling lit in relocated installation")
    case = {"name": "relocated_single_binary_install",
            "upstream": ["parse", "digital-1p.pdf", "--no-ocr", "--quiet"],
            "bridge": ["parse", "digital-1p.pdf", "--no-ocr", "--quiet"],
            "fixtures": [("digital-1p.pdf", "digital-1p.pdf")],
            "contains": ["DIGITAL PAGE 1 SENTINEL 2026"]}
    record = execute_parity_case(case, {"upstream": binaries["upstream"], "glassbox": installed,
                                       "litx": installed}, oracle, evidence_dir, started, run_timeout)
    record["installed_binary"] = str(installed)
    record["installed_sha256"] = sha256_file(installed)
    record["sibling_lit_required"] = False
    return record


def verify_package_identity():
    """Check renamed packages, dependency keys, and the reviewed SHA-256 lock entry.

    Accepts no parameters and reads only downstream Cargo manifests and Cargo.lock.
    Returns named boolean checks and the selected package/dependency versions.
    Raises DriverError for unreadable or malformed TOML and never edits dependency state.
    """
    try:
        cli_manifest = tomllib.loads((EXT / "crates/litx/Cargo.toml").read_text(encoding="utf-8"))
        inspect_manifest = tomllib.loads((EXT / "crates/litx-inspect/Cargo.toml").read_text(encoding="utf-8"))
        with (EXT / "Cargo.lock").open("rb") as handle:
            lock = tomllib.load(handle)
        with (REPO / "crates/liteparse/Cargo.toml").open("rb") as handle:
            upstream_version = tomllib.load(handle)["package"]["version"]
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise DriverError(f"cannot inspect downstream package identity: {error}") from error
    packages = {package["name"]: package for package in lock["package"]}
    cli_lock = packages.get("glassbox-cli", {})
    inspect_lock = packages.get("glassbox-inspect", {})
    checks = [
        {"id": "cli_package_renamed", "ok": cli_manifest["package"]["name"] == "glassbox-cli" and "glassbox-cli" in packages},
        {"id": "inspect_package_renamed", "ok": inspect_manifest["package"]["name"] == "glassbox-inspect" and "glassbox-inspect" in packages},
        {"id": "legacy_packages_removed", "ok": "litx" not in packages and "litx-inspect" not in packages},
        {"id": "downstream_dependency_key", "ok": "glassbox-inspect" in cli_manifest["dependencies"] and "glassbox-inspect" in cli_lock.get("dependencies", [])},
        {"id": "downstream_feature_keys", "ok": (
            cli_manifest["features"].get("magika") == ["glassbox-inspect/magika"]
            and cli_manifest["features"].get("ffmpeg-link") == ["glassbox-inspect/ffmpeg-link"])},
        {"id": "sha2_manifest_version", "ok": inspect_manifest["dependencies"].get("sha2") == "=0.10.9"},
        {"id": "sha2_lock_version", "ok": packages.get("sha2", {}).get("version") == "0.10.9" and "sha2" in inspect_lock.get("dependencies", [])},
        {"id": "tokio_runtime_available", "ok": "tokio" in cli_lock.get("dependencies", [])},
        {"id": "upstream_version_source", "ok": packages.get("liteparse", {}).get("version") == upstream_version},
    ]
    return {"checks": checks, "glassbox_cli_version": cli_manifest["package"]["version"],
            "glassbox_inspect_version": inspect_manifest["package"]["version"],
            "upstream_version": upstream_version, "sha2_version": packages.get("sha2", {}).get("version")}


def metadata_content_fields(value, prefix="$"):
    """Return paths of forbidden body/content fields in a v2 JSON value.

    Accepts any decoded JSON value and an optional display prefix. Returns a
    sorted list of legacy body, row, cell, preview, and OCR-output field paths.
    Metadata OCR advice remains legal because it contains no OCR result payload.
    """
    forbidden = {"text", "content", "markdown", "rows", "cells", "ocr_text",
                 "sample_values", "preview", "pages", "claims", "packet"}
    found = []
    if isinstance(value, dict):
        for key, child in value.items():
            path = f"{prefix}.{key}"
            if key in forbidden:
                found.append(path)
            found.extend(metadata_content_fields(child, path))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.extend(metadata_content_fields(child, f"{prefix}[{index}]"))
    return sorted(set(found))


def verify_metadata_report(report, path, expected_status):
    """Check one v2 report against independent file bytes and expected status.

    Accepts a decoded report, source path, and complete/unsupported/failed status.
    Returns named boolean checks for envelope, digest, byte counts, content-field
    absence, status, and snapshot qualification. No assertion depends on a digest
    or count produced by the implementation under test.
    """
    metadata = report.get("metadata", {}) if isinstance(report, dict) else {}
    digest = metadata.get("digest", {})
    source = Path(path)
    expected_digest = sha256_file(source)
    expected_bytes = source.stat().st_size
    return [
        {"id": "top_level_v2_envelope", "ok": isinstance(report, dict) and set(report) == {"schema_version", "kind", "metadata"}},
        {"id": "schema_version_2", "ok": report.get("schema_version") == 2},
        {"id": "kind_inspect", "ok": report.get("kind") == "inspect"},
        {"id": "expected_operation_status", "ok": metadata.get("status") == expected_status},
        {"id": "independent_sha256", "ok": digest.get("digest") == expected_digest},
        {"id": "independent_hashed_byte_count", "ok": digest.get("bytes_hashed", {}).get("value") == expected_bytes},
        {"id": "hashed_byte_unit", "ok": digest.get("bytes_hashed", {}).get("unit") == "byte"},
        {"id": "not_immutable_snapshot", "ok": digest.get("snapshot_guarantee") == "not_immutable"},
        {"id": "no_body_or_legacy_fields", "ok": not metadata_content_fields(report)},
    ]


def metadata_case_fixtures(oracle, sandbox):
    """Prepare independent inputs for every supported metadata family.

    Accepts a freshly generated independent oracle and a new campaign sandbox.
    Returns fixture dictionaries with expected process/report statuses and
    oracle facts. Generated XLSM, iWork, CSV/TSV, Parquet, and unknown inputs
    stay below the sandbox; tracked and generated fixtures are opened read-only.
    No retained baseline result or timestamp-named evidence is required.
    """
    generated = sandbox / "inputs"
    generated.mkdir(parents=True, mode=0o700)
    sentinel = "V-META_BODY_SENTINEL_8f31c6"
    (generated / "plain.txt").write_text(f"first native line\n{sentinel}\nthird line\n", encoding="utf-8")
    (generated / "rows.csv").write_text(f"account_id,notes\n1,{sentinel}\n2,other\n", encoding="utf-8")
    (generated / "rows.tsv").write_text(f"account_id\tnotes\n1\t{sentinel}\n2\tother\n", encoding="utf-8")
    (generated / "identity.parquet").write_bytes(b"PAR1" + sentinel.encode() + b"PAR1")
    (generated / "unknown.vmeta-unknown").write_bytes(b"unknown family\x00" + sentinel.encode())

    import zipfile

    def source_path(name):
        """Resolve an oracle fixture or a tracked self-authored family fixture.

        Accepts one fixture basename. Returns its absolute read-only source path.
        Raises DriverError when neither independent source contains the fixture.
        """
        try:
            return Path(fixture_record(oracle, name)["path"])
        except DriverError:
            path = EXT / "fixtures" / name
            if not path.is_file():
                raise DriverError(f"metadata fixture is unavailable: {name}")
            return path

    xlsx = source_path("grid.xlsx")
    with zipfile.ZipFile(xlsx, "r") as source, zipfile.ZipFile(generated / "workbook.xlsm", "x") as target:
        for info in source.infolist():
            payload = source.read(info.filename)
            if info.filename == "[Content_Types].xml":
                payload = payload.replace(
                    b"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
                    b"application/vnd.ms-excel.sheet.macroEnabled.main+xml")
            target.writestr(info, payload)
    preview = source_path("image.png").read_bytes()
    shutil.copy2(EXT / "fixtures/prose_reference.go", generated / "prose_reference.go")
    for extension in ("pages", "numbers", "key"):
        with zipfile.ZipFile(generated / f"quicklook.{extension}", "x") as package:
            package.writestr("QuickLook/Preview.png", preview)

    def tracked(name, family, status=0, operation=None, **facts):
        operation = operation or ("complete" if status == 0 else "unsupported" if status == 2 else "failed")
        return {"name": name, "path": str(source_path(name)),
                "family": family, "expected_exit": status,
                "expected_status": operation, **facts}

    def local(name, family, status=0, operation=None, **facts):
        operation = operation or ("complete" if status == 0 else "unsupported" if status == 2 else "failed")
        return {"name": name, "path": str(generated / name), "family": family,
                "expected_exit": status, "expected_status": operation, **facts}

    fixtures = [
        tracked("digital-1p.pdf", "pdf", pages=1),
        tracked("notes.docx", "docx"),
        tracked("deck.pptx", "pptx"),
        tracked("grid.xlsx", "xlsx"),
        local("workbook.xlsm", "xlsm"),
        tracked("notes.odt", "odt"),
        tracked("deck.odp", "odp"),
        tracked("grid.ods", "ods"),
        tracked("native.epub", "epub"),
        tracked("native.mobi", "mobi"),
        local("quicklook.pages", "iwork-pages"),
        local("quicklook.numbers", "iwork-numbers"),
        local("quicklook.key", "iwork-key"),
        tracked("listing.zip", "zip"),
        tracked("pages.cbz", "cbz"),
        tracked("pages4.cbr", "cbr"),
        local("rows.csv", "csv", rows=2),
        local("rows.tsv", "tsv", rows=2),
        tracked("data.json", "json", scalars=8),
        tracked("doc.xml", "xml", elements=4),
        tracked("plain-lines.txt", "plain", lines=12),
        local("prose_reference.go", "source"),
        tracked("scan-raster-1.png", "raster"),
        local("plain.txt", "plain-sentinel", lines=3),
        tracked("single.djvu", "djvu", status=0),
        local("identity.parquet", "parquet", status=0),
        local("unknown.vmeta-unknown", "unknown", status=2),
        tracked("broken.pdf", "corrupt", status=1),
    ]
    return fixtures


def metadata_report_checks(fixture, report):
    """Add family-specific independent oracle checks to common v2 checks.

    Accepts one fixture dictionary and decoded report. Returns common checks
    plus expected PDF pages, JSON scalars, XML elements, delimited rows, plain
    lines, format extension, and identification-only support where applicable.
    """
    checks = verify_metadata_report(report, fixture["path"], fixture["expected_status"])
    metadata = report.get("metadata", {})
    counts = metadata.get("counts", [])
    expectations = [
        ("pages", "native_page_signals", fixture.get("pages")),
        ("scalars", "json_scalar", fixture.get("scalars")),
        ("elements", "xml_element", fixture.get("elements")),
        ("rows", "dataset_row", fixture.get("rows")),
        ("lines", "logical_line", fixture.get("lines")),
    ]
    for fact, unit, expected in expectations:
        if expected is None:
            continue
        if fact == "pages":
            actual = len(metadata.get("native_page_signals", []))
        else:
            actual = next((count.get("value") for count in counts
                           if count.get("unit") == unit), None)
        checks.append({"id": f"oracle_{fact}", "ok": actual == expected,
                       "detail": f"expected {expected}, got {actual}"})
    if fixture["family"] == "xlsm":
        checks.append({"id": "xlsm_package_identity",
                       "ok": metadata.get("format", {}).get("extension") == "xlsm"})
    if fixture["family"] in ("djvu", "parquet"):
        checks.append({"id": "identification_only_boundary",
                       "ok": metadata.get("extraction_support") == "identification_only"})
    if fixture["family"] == "pdf":
        pages = metadata.get("native_page_signals", [])
        checks.extend([
            {"id": "pdf_security_facts", "ok": metadata.get("pdf", {}).get("permissions") is not None},
            {"id": "pdf_geometry_present", "ok": bool(pages) and all(
                page.get("width_points") is not None and page.get("height_points") is not None
                for page in pages)},
            {"id": "pdf_text_layer_not_inferred", "ok": bool(pages) and all(
                page.get("text_layer") == "unknown" and page.get("ocr_recommended") == "unknown"
                for page in pages)},
        ])
    return checks


def metadata_execute(binary, argv, log_path, timeout, started, input_bytes=None):
    """Run one metadata child and decode its combined output when possible.

    Accepts executable, arguments, new log path, timeout, campaign start, and
    optional stdin bytes. Returns execution evidence, raw bytes, and decoded JSON
    or None. Process timeout/launch/log failures raise DriverError; expected
    nonzero inspect statuses remain ordinary evidence.
    """
    execution = parity_execute([binary, *argv], log_path, timeout, started,
                               input_bytes=input_bytes)
    raw = Path(log_path).read_bytes()
    try:
        parsed = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        parsed = None
    execution["output_sha256"] = sha256_bytes(raw)
    execution["output_bytes"] = len(raw)
    return execution, raw, parsed


def instrument_metadata_forbidden_stages(root):
    """Instrument extraction, OCR, and legacy inspection in an isolated copy.

    Accepts a copied upstream/downstream workspace root. Returns trap names and
    post-mutation source hashes. Missing or ambiguous anchors raise DriverError.
    The real checkout is never edited; each inserted panic has a V-META marker.
    """
    mutations = {
        root / "crates/liteparse/src/stages.rs": [
            ("pub fn extract(\n", "STAGES-EXTRACT", "    "),
            ("pub fn page_complexity(\n", "PAGE-COMPLEXITY", "    "),
            ("pub fn render_for_ocr(\n", "RENDER-FOR-OCR", "    "),
            ("pub fn merge_ocr(\n", "MERGE-OCR", "    "),
        ],
        root / "crates/liteparse/src/extract.rs": [
            ("pub fn extract_pages_from_input(\n", "EXTRACT-PAGES-INPUT", "    "),
            ("pub(crate) fn extract_pages_from_document(\n", "EXTRACT-PAGES-DOCUMENT", "    "),
        ],
        root / "crates/liteparse/src/ocr/tesseract.rs": [
            ("    fn recognize<'a, 'b: 'a, 'c: 'a>(\n", "TESSERACT-RECOGNIZE", "        "),
        ],
        root / "crates/pdfium/src/page.rs": [
            ("    pub fn text(&self)", "PAGE-TEXT", "        "),
        ],
        root / "extensionalities/crates/litx-inspect/src/lib.rs": [
            ("fn inspect_with_session(\n", "LEGACY-INSPECT-SESSION", "    "),
        ],
    }
    records = []

    def trap_function_body(text, path, anchor, marker, indentation):
        """Insert one panic after a Rust function signature's opening brace.

        Accepts source text, diagnostic path, unique anchor, marker, and indent.
        Returns mutated text with parameters and return type left byte-identical.
        Raises DriverError for duplicate anchors or an unbalanced/missing body.
        """
        if text.count(anchor) != 1:
            raise DriverError(
                f"instrumentation anchor count {text.count(anchor)}, expected 1: {path}: {anchor!r}")
        anchor_start = text.index(anchor)
        signature_end = anchor_start + len(anchor)
        parameters = text.rfind("(", 0, signature_end)
        if parameters < anchor_start:
            raise DriverError(f"instrumentation anchor has no parameter list: {path}: {anchor!r}")
        depth = 0
        body = None
        for index in range(parameters, len(text)):
            character = text[index]
            if character == "(":
                depth += 1
            elif character == ")":
                depth -= 1
                if depth < 0:
                    raise DriverError(f"unbalanced copied function signature: {path}: {anchor!r}")
            elif character == "{" and depth == 0:
                body = index
                break
            elif character == ";" and depth == 0:
                break
        if body is None:
            raise DriverError(f"copied function body not found: {path}: {anchor!r}")
        panic = f'\n{indentation}panic!("V-META-FORBIDDEN-{marker}");'
        return text[:body + 1] + panic + text[body + 1:]

    for path, traps in mutations.items():
        text = path.read_text(encoding="utf-8")
        for anchor, marker, indentation in traps:
            text = trap_function_body(text, path, anchor, marker, indentation)
            records.append({"trap": marker, "path": str(path), "anchor": anchor.strip()})
        path.write_text(text, encoding="utf-8")
        records.append({"path": str(path), "sha256": sha256_file(path)})
    return {"root": str(root), "records": records,
            "trap_count": sum("trap" in record for record in records)}


def extract_execute(binary, argv, log_path, timeout, started, input_bytes=None,
                    environment=None):
    """Run one canonical extraction child and decode the final JSON value.

    Accepts an executable, argument vector, exclusive log path, timeout,
    campaign start, optional stdin bytes, and optional environment. Returns
    execution evidence, raw combined output, and decoded JSON or None. Expected
    operational exits remain evidence; launch, timeout, and log errors raise
    DriverError. Deprecated-flag warnings before JSON are tolerated explicitly.
    """
    execution = parity_execute([binary, *argv], log_path, timeout, started,
                               environment=environment, input_bytes=input_bytes)
    raw = Path(log_path).read_bytes()
    parsed = None
    for line in reversed(raw.splitlines()):
        try:
            parsed = json.loads(line)
            break
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
    execution["output_sha256"] = sha256_bytes(raw)
    execution["output_bytes"] = len(raw)
    return execution, raw, parsed


def extract_report_checks(report, expected):
    """Check a v2 report against independent native-unit and content facts.

    Accepts a decoded report and an expectation dictionary with exit, status,
    native unit, available count, selected indices, and optional exact texts or
    completeness. Returns JSON-schema, metadata, selection, and content checks.
    Missing report fields produce failed checks rather than driver exceptions.
    """
    checks = [
        {"id": "expected_exit", "ok": expected["exit"] == expected["actual_exit"],
         "detail": f"expected {expected['exit']}, got {expected['actual_exit']}"},
        {"id": "v2_envelope", "ok": isinstance(report, dict) and
         report.get("schema_version") == 2 and report.get("kind") == "extract"},
    ]
    if not isinstance(report, dict):
        return checks
    metadata = report.get("metadata", {})
    selection = report.get("selection", {})
    units = report.get("content", {}).get("units", [])
    checks.extend([
        {"id": "operation_status", "ok": metadata.get("status") == expected["status"],
         "detail": f"expected {expected['status']}, got {metadata.get('status')}"},
        {"id": "native_unit", "ok": selection.get("unit") == expected["unit"]},
        {"id": "available_count", "ok": selection.get("available", {}).get("value") == expected["available"]},
        {"id": "returned_indices", "ok": selection.get("returned_indices") == expected["indices"],
         "detail": f"actual {selection.get('returned_indices')}"},
        {"id": "unit_identity", "ok": [unit.get("unit_index") for unit in units] == expected["indices"]},
    ])
    if "completeness" in expected:
        checks.append({"id": "report_completeness",
                       "ok": report.get("completeness") == expected["completeness"]})
    if "texts" in expected:
        actual = [unit.get("text") for unit in units]
        checks.append({"id": "independent_exact_text", "ok": actual == expected["texts"],
                       "detail": f"expected {expected['texts']!r}, got {actual!r}"})
    if "omitted" in expected:
        checks.append({"id": "omission_status", "ok": selection.get("omitted") == expected["omitted"]})
    return checks


def run_extract_case(args, evidence_dir):
    """Run V-SELECT native extraction and public OCR verification.

    Accepts parsed driver arguments and an authorized evidence root. Returns 0
    for PASS, 1 for an extraction contract failure, and 2 for blocked execution.
    Fresh bounded child evidence, independent content expectations, output
    artifacts, source recipe assertions, and unchanged-upstream hashes are
    retained in one exclusive run directory; no source fixture is modified.
    The pinned-resource and base-revision reproducibility guards are opt-in
    through --require-pinned-resources/--require-base-revision; by default the
    observed limits and actual revision are recorded as evidence.
    """
    started = time.monotonic()
    observed_start = datetime.now(timezone.utc).isoformat()
    run = run_id()
    run_dir = mkdir_new(evidence_dir / f"extract-{run}")
    failures = []
    blockers = []
    result = {
        "handoff": {
            "handoff_schema": "chai-handoff/v1",
            "identity": {"work_id": "20261121_settling_in", "task_ids": ["B-EXTRACT"],
                         "producer": "worker-build", "consumer": "master-drive",
                         "artifact": str(run_dir / "result.json"), "updated_at": observed_start},
            "state": {"objective": "Prove request completion distinct from source coverage, native selection, and bounded public OCR.",
                      "phase": "milestone-3 wave-7", "execution_status": "in_progress",
                      "pause_point": "V-SELECT campaign execution",
                      "next_action": "Controller reviews actual V-SELECT evidence.", "blockers": []},
            "source": {"predecessors": [str(EXT / "fixtures/generate.py")],
                       "workspace": str(REPO), "revision": "57e3afb7f1d59ca93811eda5748aa917613f64fc",
                       "uncommitted_changes": "Accepted B-NAME/B-SCHEMA/B-META plus scoped B-EXTRACT source",
                       "observed_at": observed_start},
            "authority": {"decisions": ["ADR-001", "P-OCR public stage recipe"],
                          "approval_source": "Approved spec and milestones; execution_ready true",
                          "scope": "B-EXTRACT only; upstream read-only; isolated evidence and sandbox",
                          "pending_approvals": ["independent audit", "Git publication"]},
            "evidence": {"verification_ids": ["V-SELECT"], "checks": [],
                         "artifacts": [str(run_dir)], "missing_coverage": ["ARM64 only", "No independent audit"]},
            "receipt": {"consumer": "master-drive", "status": "pending"},
        },
        "case": "extract", "verification_id": "V-SELECT", "task": "B-EXTRACT",
        "status": None, "exit_code": None, "run_dir": str(run_dir),
        "timing_serial": True, "active_budget_s": PARITY_ACTIVE_BUDGET_S,
        "runs": [], "native_families": [],
        "status_correction": {
            "supersedes": str(EXT / "target/verification/settling-in-bextract-release-20261122T1345Z/extract-20261122T133529Z-2415158-f29409c7/result.json"),
            "prior_claim": "Selection-only omissions made the requested operation partial with exit 2.",
            "corrected_contract": "Fully emitted selections complete with exit 0 while source coverage remains partial and omitted=selection.",
            "authority": str(EXT / "dev/PRDs/20261121_settling_in/design.md") + ":151-161",
            "original_evidence_retained": True,
        },
    }
    sandbox = SANDBOX_ROOT / f"{SANDBOX_PREFIX}{run}" / "extract"
    result["resource_limits"] = resource_limit_snapshot()

    def record_run(name, execution, checks):
        result["runs"].append({"name": name, "execution": execution, "checks": checks})
        for check in checks:
            if not check["ok"]:
                failures.append({"run": name, "check": check["id"],
                                 "detail": check.get("detail", "assertion failed")})

    def execute_report(name, argv, expected, environment=None):
        execution, raw, report = extract_execute(
            binary, argv, run_dir / f"{name}.log", args.run_timeout, started,
            environment=environment)
        expected = {**expected, "actual_exit": execution["status"]}
        record_run(name, execution, extract_report_checks(report, expected))
        return raw, report

    try:
        enforce_resource_pin(result, args)
        result["upstream_before"] = upstream_source_snapshot()
        enforce_base_revision(result, args, "B-EXTRACT")
        result["environment"] = environment_snapshot()
        result["git"] = git_state(REPO)
        sandbox.mkdir(parents=True, mode=0o700)
        result["sandbox"] = str(sandbox)
        plain = str(sandbox / "plain-lines.txt")
        write_exclusive(plain, "\n".join(PLAIN_LINES) + "\n")
        six = str(sandbox / "six-lines.txt")
        write_exclusive(six, "\n".join(PLAIN_LINES[:6]) + "\n")
        digital = {}
        for pages in (1, 4, 5, 6):
            path = sandbox / f"digital-{pages}p.pdf"
            write_exclusive(path, build_pdf([
                {"text": f"DIGITAL PAGE {index} SENTINEL 2026"}
                for index in range(1, pages + 1)
            ]))
            digital[pages] = str(path)
        scan_source = sandbox / "scan-source.pdf"
        write_exclusive(scan_source, build_pdf([{"text": OCR_TEXT, "font_size": 20}]))
        raster = rasterize_with_pdftoppm(scan_source, sandbox / "scan-raster", dpi=150)
        width, height, rgb = decode_png_rgb(raster)
        image = {"width": width, "height": height, "rgb": rgb}
        scanned = sandbox / "scanned-1p.pdf"
        write_exclusive(scanned, build_pdf([{"image": image}]))
        mixed = sandbox / "mixed-2p.pdf"
        write_exclusive(mixed, build_pdf([{"text": MIXED_SENTINEL}, {"image": image}]))
        result["fixture_oracle"] = {
            "plain_lines": list(PLAIN_LINES), "selection_lines": list(PLAIN_LINES[:6]),
            "digital_pages": [1, 4, 5, 6],
            "ocr_words": OCR_WORDS, "mixed_sentinel": MIXED_SENTINEL,
            "generator": "V-SELECT isolated build_pdf and pdftoppm",
        }
        if args.skip_build:
            binary = Path(args.binary).resolve()
            result["build"] = {"skipped": True, "provided_binary": str(binary)}
        else:
            build, target, status = do_build(args, run_dir)
            result["build"] = build
            binary = target / "release/glassbox"
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise DriverError(f"final Glassbox executable is missing or not runnable: {binary}")
        result["binary"] = {"path": str(binary), "sha256": sha256_file(binary),
                            "bytes": binary.stat().st_size}

        plain_expect = {"exit": 0, "status": "complete", "unit": "logical_line",
                        "available": 12, "indices": list(range(1, 13)),
                        "texts": list(PLAIN_LINES), "completeness": "exact", "omitted": "none"}
        six_expect = {**plain_expect, "available": 6, "indices": list(range(1, 7)),
                      "texts": list(PLAIN_LINES[:6])}
        execute_report("text-full-six", ["extract", six, "--no-ocr"], six_expect)
        execute_report("text-bare-pages-six", ["extract", six, "--pages", "--no-ocr"], {
            **six_expect, "indices": [1, 2, 3, 4, 5], "texts": list(PLAIN_LINES[:5]),
            "completeness": "partial", "omitted": "selection"})
        execute_report("text-first-two-six", ["extract", six, "--pages", "2", "--no-ocr"], {
            **six_expect, "indices": [1, 2], "texts": list(PLAIN_LINES[:2]),
            "completeness": "partial", "omitted": "selection"})
        execute_report("text-ranges-six", ["extract", six, "--pages", "3-4,6", "--no-ocr"], {
            **six_expect, "indices": [3, 4, 6],
            "texts": [PLAIN_LINES[index - 1] for index in (3, 4, 6)],
            "completeness": "partial", "omitted": "selection"})
        execute_report("text-beyond-total", ["extract", six, "--pages", "99", "--no-ocr"], six_expect)
        legacy_raw, _ = execute_report(
            "text-legacy-max-pages",
            ["extract", plain, "--max-pages", "99", "--packet", "--no-ocr"],
            plain_expect)
        legacy_text = legacy_raw.decode(errors="replace")
        record_run("legacy-warning-evidence", {"status": 0, "kind": "output_assertion"}, [
            {"id": "max_pages_deprecation", "ok": "--max-pages is deprecated" in legacy_text},
            {"id": "packet_deprecation", "ok": "--packet is deprecated" in legacy_text},
        ])
        for selector in ("0", "-1", "4-2", "1,,2", "not-a-range"):
            name = "invalid-" + selector.replace(",", "comma").replace("-", "minus")
            execution, raw, _ = extract_execute(
                binary, ["extract", plain, "--pages", selector], run_dir / f"{name}.log",
                args.run_timeout, started)
            record_run(name, execution, [{"id": "usage_exit_64", "ok": execution["status"] == 64},
                                         {"id": "usage_diagnostic", "ok": bool(raw.strip())}])
        for name, argv in (
            ("conflicting-pages", ["extract", plain, "--pages", "2", "--max-pages", "2"]),
            ("zero-legacy-max-pages", ["extract", plain, "--max-pages", "0"]),
        ):
            execution, raw, _ = extract_execute(binary, argv, run_dir / f"{name}.log",
                                                args.run_timeout, started)
            record_run(name, execution, [{"id": "usage_exit_64", "ok": execution["status"] == 64},
                                         {"id": "usage_diagnostic", "ok": bool(raw.strip())}])
        execute_report("out-of-range", ["extract", plain, "--pages", "99-100", "--no-ocr"], {
            "exit": 1, "status": "failed", "unit": "logical_line", "available": 12,
            "indices": [], "completeness": "unavailable"})

        native_cases = [
            ("epub", "native.epub", "spine_item", 2,
             ["First chapter & evidence.\n", "Second chapter.\n"]),
            ("mobi", "native.mobi", "mobi_text_record", 1, ["Native MOBI abcabc & evidence.\n"]),
            ("docx", "notes.docx", "document_block", 3,
             ["This is a native text layer with an ampersand & evidence.", "Section one ends here.", "Scan"]),
            ("pptx", "deck.pptx", "slide", 2,
             ["This is the first slide in presentation order.\n", "Scan\n"]),
            ("odt", "notes.odt", "document_block", 2,
             ["Café et preuves ici.\tTexte  natif.", "Scan"]),
            ("odp", "deck.odp", "slide", 2,
             ["This is the first native ODF presentation slide.", "Scan"]),
        ]
        for family, filename, unit, available, texts in native_cases:
            result["native_families"].append(family)
            execute_report(f"native-{family}", ["extract", str(EXT / "fixtures" / filename), "--no-ocr"], {
                "exit": 0, "status": "complete", "unit": unit, "available": available,
                "indices": list(range(1, available + 1)), "texts": texts,
                "completeness": "exact", "omitted": "none"})
        for family, filename, available in (("xlsx", "grid.xlsx", 2), ("ods", "grid.ods", 1)):
            result["native_families"].append(family)
            raw, report = execute_report(f"native-{family}", ["extract", str(EXT / "fixtures" / filename), "--no-ocr"], {
                "exit": 0, "status": "complete", "unit": "worksheet", "available": available,
                "indices": list(range(1, available + 1)), "completeness": "exact", "omitted": "none"})
            values = [value for unit in (report or {}).get("content", {}).get("units", [])
                      for row in unit.get("rows", []) for value in row.get("values", [])]
            record_run(f"native-{family}-oracle", {"status": 0, "kind": "content_assertion"}, [
                {"id": "independent_sheet_value", "ok": any("We can inspect" in value for value in values)}])

        for pages in (1, 4, 5, 6):
            result["native_families"].append(f"pdf-{pages}")
            execute_report(f"native-pdf-{pages}", ["extract", digital[pages], "--no-ocr"], {
                "exit": 0, "status": "complete", "unit": "physical_page", "available": pages,
                "indices": list(range(1, pages + 1)),
                "completeness": "exact", "omitted": "none"})
            execution, raw, report = extract_execute(
                binary, ["extract", digital[pages], "--pages", f"{pages}-{pages}", "--no-ocr"],
                run_dir / f"native-pdf-{pages}-last.log", args.run_timeout, started)
            units = (report or {}).get("content", {}).get("units", [])
            expected_text = f"DIGITAL PAGE {pages} SENTINEL 2026"
            record_run(f"native-pdf-{pages}-last-oracle", execution, [
                {"id": "complete_selection_exit_0", "ok": execution["status"] == 0},
                {"id": "complete_selection_status", "ok": (report or {}).get("metadata", {}).get("status") == "complete"},
                {"id": "truthful_source_coverage", "ok":
                 (report or {}).get("completeness") == ("exact" if pages == 1 else "partial") and
                 (report or {}).get("selection", {}).get("omitted") == ("none" if pages == 1 else "selection")},
                {"id": "last_page_selected", "ok": len(units) == 1 and units[0].get("unit_index") == pages},
                {"id": "independent_page_text", "ok": len(units) == 1 and
                 units[0].get("text", "").strip() == expected_text},
            ])

        execute_report("limit-chars", ["extract", plain, "--max-chars", "7", "--no-ocr"], {
            "exit": 2, "status": "partial", "unit": "logical_line", "available": 12,
            "indices": list(range(1, 13)), "completeness": "partial", "omitted": "safety_limit"})
        execute_report("limit-tokens", ["extract", plain, "--max-tokens", "2", "--no-ocr"], {
            "exit": 2, "status": "partial", "unit": "logical_line", "available": 12,
            "indices": list(range(1, 13)), "completeness": "partial", "omitted": "safety_limit"})

        missing = sandbox / "missing-tessdata"
        environment = dict(os.environ, TESSDATA_PREFIX=str(missing))
        raw, report = execute_report(
            "ocr-missing-model", ["extract", str(mixed)], {
                "exit": 2, "status": "partial", "unit": "physical_page", "available": 2,
                "indices": [1, 2], "completeness": "partial", "omitted": "partial_failure"},
            environment=environment)
        units = (report or {}).get("content", {}).get("units", [])
        diagnostics = (report or {}).get("metadata", {}).get("diagnostics", [])
        record_run("ocr-missing-model-retention", {"status": 0, "kind": "content_assertion"}, [
            {"id": "native_text_retained", "ok": bool(units) and MIXED_SENTINEL in units[0].get("text", "")},
            {"id": "ocr_diagnostic", "ok": any(item.get("code") == "pdf_ocr" for item in diagnostics)},
            {"id": "no_model_created", "ok": not missing.exists()},
        ])

        tessdata = Path(os.environ.get("TESSDATA_PREFIX", "/usr/share/tesseract-ocr/5/tessdata"))
        if not (tessdata / "eng.traineddata").is_file():
            blockers.append({"stage": "ocr-oracle", "detail": "local eng.traineddata is unavailable"})
        else:
            raw, report = execute_report(
                "ocr-public-stage", ["extract", str(scanned)], {
                    "exit": 0, "status": "complete", "unit": "physical_page", "available": 1,
                    "indices": [1], "completeness": "exact", "omitted": "none"})
            units = (report or {}).get("content", {}).get("units", [])
            text = units[0].get("text", "") if units else ""
            provenance = units[0].get("provenance", []) if units else []
            record_run("ocr-public-stage-oracle", {"status": 0, "kind": "content_assertion"}, [
                {"id": "independent_ocr_words", "ok": all(word in text for word in OCR_WORDS),
                 "detail": text},
                {"id": "ocr_process_provenance", "ok": any(
                    (item.get("ocr") or {}).get("engine") == "tesseract" and
                    (item.get("ocr") or {}).get("raster_long_edge") == 3072
                    for item in provenance)},
            ])
        pdf_source = (EXT / "crates/litx-inspect/src/pdf.rs").read_text(encoding="utf-8")
        report_source = (EXT / "crates/litx-inspect/src/reports.rs").read_text(encoding="utf-8")
        record_run("ocr-recipe-source", {"status": 0, "kind": "source_assertion"}, [
            {"id": "public_default_3072", "ok": "DEFAULT_OCR_RASTER_LONG_EDGE: u32 = 3072" in report_source},
            {"id": "approved_stage_wiring", "ok": all(fragment in pdf_source for fragment in (
                "emit_word_boxes: true", "max_long_edge: Some(options.ocr_raster_long_edge)",
                "stages::merge_ocr", "stages::project"))},
        ])

        execution, raw, report = extract_execute(
            binary, ["inspect", plain], run_dir / "inspect-isolation.log", args.run_timeout, started)
        record_run("inspect-isolation", execution, [
            {"id": "exit_zero", "ok": execution["status"] == 0},
            {"id": "metadata_only", "ok": isinstance(report, dict) and report.get("kind") == "inspect" and
             "content" not in report and "text" not in report},
            {"id": "body_absent", "ok": PLAIN_LINES[0] not in raw.decode(errors="replace")},
        ])

        batch_input = b"".join((json.dumps({"id": index, "path": plain}) + "\n").encode()
                               for index in ("first", "second"))
        output_dir = sandbox / "output"
        execution, raw, _ = extract_execute(
            binary, ["extract", "--batch", "--pages", "99", "--no-ocr", "--output-dir", str(output_dir)],
            run_dir / "batch-output.log", args.run_timeout, started, input_bytes=batch_input)
        responses = [json.loads(line) for line in raw.splitlines() if line.startswith(b"{")]
        checks = [{"id": "aggregate_complete", "ok": execution["status"] == 0},
                  {"id": "ordered_v2_responses", "ok": [response.get("id") for response in responses] == ["first", "second"] and
                   all(response.get("report", {}).get("kind") == "extract" for response in responses)}]
        for response in responses:
            report = response.get("report", {})
            checks.append({"id": f"twelve_units_{response.get('id')}",
                           "ok": len(report.get("content", {}).get("units", [])) == 12})
            artifacts = report.get("metadata", {}).get("artifacts", [])
            checks.append({"id": f"two_artifacts_{response.get('id')}",
                           "ok": sorted(item.get("kind") for item in artifacts) == ["extraction_json", "markdown_content"]})
            for artifact in artifacts:
                path = Path(artifact.get("path", ""))
                checks.append({"id": f"artifact_exists_{response.get('id')}_{artifact.get('kind')}",
                               "ok": path.is_file()})
                if path.is_file() and artifact.get("kind") == "extraction_json":
                    saved = json.loads(path.read_text(encoding="utf-8"))
                    checks.append({"id": f"saved_v2_{response.get('id')}",
                                   "ok": saved.get("kind") == "extract" and
                                   saved.get("selection") == report.get("selection")})
        record_run("batch-output", execution, checks)
    except (DriverError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        blockers.append({"stage": "extract", "detail": str(error)})
    except Exception as error:
        blockers.append({"stage": "internal", "detail": f"{type(error).__name__}: {error}"})
    try:
        result["upstream_after"] = upstream_source_snapshot()
        result["upstream_unchanged"] = result.get("upstream_before") == result["upstream_after"]
        if not result["upstream_unchanged"]:
            failures.append({"run": "upstream", "check": "unchanged",
                             "detail": "real upstream checkout changed during V-SELECT"})
    except Exception as error:
        blockers.append({"stage": "upstream-after", "detail": str(error)})
    result["native_families"] = sorted(set(result["native_families"]))
    elapsed = time.monotonic() - started
    result["costs"] = {"elapsed_active_s": round(elapsed, 4),
                       "within_30_minute_budget": elapsed <= PARITY_ACTIVE_BUDGET_S}
    if elapsed > PARITY_ACTIVE_BUDGET_S:
        blockers.append({"stage": "budget", "detail": "extract active-test budget exceeded"})
    result["checks_total"] = sum(len(run["checks"]) for run in result["runs"]) + (1 if "upstream_unchanged" in result else 0)
    result["handoff"]["state"]["execution_status"] = "completed" if not failures and not blockers else "failed"
    result["handoff"]["state"]["pause_point"] = "V-SELECT result ready for controller review"
    result["handoff"]["state"]["blockers"] = blockers
    result["handoff"]["evidence"]["checks"] = [{
        "verification_id": "V-SELECT",
        "command": f"python3 -B {SCRIPT_PATH} extract --evidence-dir {evidence_dir}",
        "result": "PASS" if not failures and not blockers else "FAIL" if failures else "BLOCKED",
        "observed_at": datetime.now(timezone.utc).isoformat(),
        "revision": result["handoff"]["source"]["revision"], "evidence": str(run_dir / "result.json")}]
    return finalize(result, run_dir, failures, blockers, evidence_dir)

def run_metadata_case(args, evidence_dir):
    """Run V-META against final and isolated trap-instrumented executables.

    Accepts parsed driver arguments and an authorized evidence root. Returns 0
    for PASS, 1 for contract failures, and 2 for blocked execution. Raw process
    logs, schema reports, independent oracle checks, instrumented source hashes,
    and unchanged-upstream evidence are retained in one unique run directory.
    The pinned-resource and base-revision reproducibility guards are opt-in
    through --require-pinned-resources/--require-base-revision; by default the
    observed limits and actual revision are recorded as evidence.
    """
    started = time.monotonic()
    observed_start = datetime.now(timezone.utc).isoformat()
    run = run_id()
    run_dir = mkdir_new(evidence_dir / f"metadata-{run}")
    failures = []
    blockers = []
    result = {
        "handoff": {
            "handoff_schema": "chai-handoff/v1",
            "identity": {"work_id": "20261121_settling_in", "task_ids": ["B-META"],
                         "producer": "worker-build", "consumer": "master-drive",
                         "artifact": str(run_dir / "result.json"), "updated_at": observed_start},
            "state": {"objective": "Prove content-free v2 metadata inspection across all supported families.",
                      "phase": "milestone-3 wave-6", "execution_status": "in_progress",
                      "pause_point": "V-META campaign execution",
                      "next_action": "Controller reviews actual V-META evidence.", "blockers": []},
            "source": {"predecessors": [str(GENERATOR)],
                       "workspace": str(REPO), "revision": "57e3afb7f1d59ca93811eda5748aa917613f64fc",
                       "uncommitted_changes": "Accepted B-NAME/B-SCHEMA plus scoped B-META source snapshot",
                       "observed_at": observed_start},
            "authority": {"decisions": ["ADR-001", "P-SIGNALS V2 native object recipe"],
                          "approval_source": "Approved spec and milestones; execution_ready true",
                          "scope": "B-META only; real upstream read-only; isolated instrumented copies",
                          "pending_approvals": ["independent audit", "Git publication"]},
            "evidence": {"verification_ids": ["V-META"], "checks": [],
                         "artifacts": [str(run_dir)], "missing_coverage": ["ARM64 only", "No independent audit"]},
            "receipt": {"consumer": "master-drive", "status": "pending"},
        },
        "case": "metadata", "verification_id": "V-META", "task": "B-META",
        "status": None, "exit_code": None, "run_dir": str(run_dir),
        "timing_serial": True, "active_budget_s": PARITY_ACTIVE_BUDGET_S,
        "runs": [], "fixture_count": 0,
    }
    sandbox = SANDBOX_ROOT / f"{SANDBOX_PREFIX}{run}" / "metadata"
    result["resource_limits"] = resource_limit_snapshot()

    def record_run(name, execution, checks):
        result["runs"].append({"name": name, "execution": execution, "checks": checks})
        for check in checks:
            if not check["ok"]:
                failures.append({"run": name, "check": check["id"],
                                 "detail": check.get("detail", "assertion failed")})

    try:
        enforce_resource_pin(result, args)
        networked, network_reason = parity_network_permission(args)
        result["build_network_mode"] = {"networked": networked,
                                        "reason": network_reason}
        result["upstream_before"] = upstream_source_snapshot()
        enforce_base_revision(result, args, "B-META")
        result["environment"] = environment_snapshot()
        result["git"] = git_state(REPO)
        sandbox.mkdir(parents=True, mode=0o700)
        result["sandbox"] = str(sandbox)
        oracle = build_fixtures(REPO, sandbox / "fixture-oracle",
                                "Fresh V-META oracle; no historical V-SETUP artifact is required")
        result["fixture_oracle"] = {
            "path": oracle["oracle_path"],
            "fixture_count": len(oracle["fixtures"]),
            "generator_sha256": oracle["provenance"]["generate_py_sha256"],
            "generator_parity_all_identical": all(
                entry.get("identical_to_tracked")
                for entry in oracle["provenance"]["generator_parity"]),
        }
        fixtures = metadata_case_fixtures(oracle, sandbox)
        result["fixture_count"] = len(fixtures)
        result["families"] = [fixture["family"] for fixture in fixtures]

        if args.skip_build:
            binary = Path(args.binary).resolve()
            result["build"] = {"skipped": True, "provided_binary": str(binary)}
        else:
            build, target, status = do_build(args, run_dir)
            result["build"] = build
            binary = target / "release/glassbox" if status == 0 else target / "release/glassbox"
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise DriverError(f"final Glassbox executable is missing or not runnable: {binary}")
        result["binary"] = {"path": str(binary), "sha256": sha256_file(binary),
                            "bytes": binary.stat().st_size}

        sentinels = ["V-META_BODY_SENTINEL_8f31c6", "JSON BODY SENTINEL 2026",
                     "XML BODY SENTINEL 2026", "PLAIN LINE 1 TOKEN 2026",
                     *DOCX_SENTINELS, "Final page confirms full-document extraction."]
        for fixture in fixtures:
            name = f"family-{fixture['family']}"
            execution, raw, report = metadata_execute(
                binary, ["inspect", fixture["path"]], run_dir / f"{name}.log",
                args.run_timeout, started)
            checks = [{"id": "expected_exit", "ok": execution["status"] == fixture["expected_exit"],
                       "detail": f"expected {fixture['expected_exit']}, got {execution['status']}"},
                      {"id": "json_stdout", "ok": isinstance(report, dict)}]
            if isinstance(report, dict):
                checks.extend(metadata_report_checks(fixture, report))
            serialized = raw.decode(errors="replace")
            checks.append({"id": "body_sentinel_absence",
                           "ok": not any(sentinel in serialized for sentinel in sentinels)})
            if fixture["family"] == "plain-sentinel":
                checks.append({"id": "default_thumbnail_absent",
                               "ok": not report["metadata"].get("artifacts")})
            record_run(name, execution, checks)

        for format_name in ("text", "markdown"):
            execution, raw, _ = metadata_execute(
                binary, ["inspect", fixtures[0]["path"], "--format", format_name],
                run_dir / f"format-{format_name}.log", args.run_timeout, started)
            text = raw.decode(errors="replace")
            record_run(f"format-{format_name}", execution, [
                {"id": "exit_zero", "ok": execution["status"] == 0},
                {"id": "metadata_summary", "ok": "Glassbox metadata inspection" in text and "SHA-256" in text},
                {"id": "no_body_sentinel", "ok": not any(sentinel in text for sentinel in sentinels)},
            ])
        execution, raw, report = metadata_execute(
            binary, ["inspect", fixtures[0]["path"], "--pretty"],
            run_dir / "format-pretty.log", args.run_timeout, started)
        record_run("format-pretty", execution, [
            {"id": "exit_zero", "ok": execution["status"] == 0},
            {"id": "pretty_json", "ok": raw.startswith(b"{\n") and isinstance(report, dict)},
            *(metadata_report_checks(fixtures[0], report) if isinstance(report, dict) else []),
        ])

        batch_requests = [fixtures[index] for index in (0, 16, 24)]
        batch_input = b"".join((json.dumps({"id": fixture["family"], "path": fixture["path"]}) + "\n").encode()
                               for fixture in batch_requests)
        execution, raw, _ = metadata_execute(
            binary, ["inspect", "--batch"], run_dir / "batch.log",
            args.run_timeout, started, input_bytes=batch_input)
        responses = [json.loads(line) for line in raw.splitlines() if line.strip()]
        batch_checks = [{"id": "aggregate_complete_exit_0", "ok": execution["status"] == 0},
                        {"id": "ordered_response_count", "ok": [response.get("id") for response in responses] == [f["family"] for f in batch_requests]}]
        for fixture, response in zip(batch_requests, responses):
            report = response.get("report")
            if isinstance(report, dict):
                batch_checks.extend(metadata_report_checks(fixture, report))
            else:
                batch_checks.append({"id": f"batch_report_{fixture['family']}", "ok": False})
        record_run("batch-jsonl", execution, batch_checks)

        output_dir = sandbox / "output-dir"
        execution, raw, report = metadata_execute(
            binary, ["inspect", fixtures[0]["path"], "--output-dir", str(output_dir)],
            run_dir / "output-dir.log", args.run_timeout, started)
        artifacts = report.get("metadata", {}).get("artifacts", []) if isinstance(report, dict) else []
        artifact_checks = [{"id": "exit_zero", "ok": execution["status"] == 0},
                           {"id": "two_metadata_artifacts", "ok": sorted(a.get("kind") for a in artifacts) == ["metadata_json", "metadata_summary"]}]
        for artifact in artifacts:
            path = Path(artifact.get("path", ""))
            artifact_checks.append({"id": f"artifact_exists_{artifact.get('kind')}", "ok": path.is_file()})
            if path.is_file() and artifact.get("kind") == "metadata_json":
                saved = json.loads(path.read_text(encoding="utf-8"))
                artifact_checks.extend(metadata_report_checks(fixtures[0], saved))
            if path.is_file() and artifact.get("kind") == "metadata_summary":
                text = path.read_text(encoding="utf-8")
                artifact_checks.append({"id": "summary_content_free", "ok": not any(s in text for s in sentinels)})
        record_run("output-dir", execution, artifact_checks)

        thumbnail = sandbox / "thumbnail.png"
        execution, raw, report = metadata_execute(
            binary, ["inspect", fixtures[0]["path"], "--thumbnail", str(thumbnail),
                     "--thumbnail-max-edge", "64"], run_dir / "thumbnail.log",
            args.run_timeout, started)
        thumbnail_checks = [{"id": "exit_zero", "ok": execution["status"] == 0},
                            {"id": "thumbnail_file", "ok": thumbnail.is_file()}]
        artifacts = report.get("metadata", {}).get("artifacts", []) if isinstance(report, dict) else []
        thumbnail_checks.append({"id": "thumbnail_dimensions", "ok": len(artifacts) == 1 and
                                 artifacts[0].get("width", 65) <= 64 and artifacts[0].get("height", 65) <= 64})
        if thumbnail.is_file():
            png = thumbnail.read_bytes()
            thumbnail_checks.append({"id": "thumbnail_png_bytes", "ok": png.startswith(b"\x89PNG\r\n\x1a\n") and len(png) > 24})
        thumbnail_checks.extend(metadata_report_checks(fixtures[0], report) if isinstance(report, dict) else [])
        record_run("thumbnail-explicit", execution, thumbnail_checks)

        for flag in ("--identify", "--max-pages", "--max-tokens", "--max-chars", "--probe-ocr"):
            execution, raw, _ = metadata_execute(
                binary, ["inspect", fixtures[0]["path"], flag], run_dir / f"legacy{flag}.log",
                args.run_timeout, started)
            text = raw.decode(errors="replace")
            record_run(f"legacy-{flag}", execution, [
                {"id": "exit_64", "ok": execution["status"] == 64},
                {"id": "migration_advice", "ok": "glassbox extract PATH" in text},
            ])

        copied = copy_upstream_workspace(sandbox / "instrumented")
        copy_downstream_workspace(copied / "extensionalities")
        result["instrumentation"] = instrument_metadata_forbidden_stages(copied)
        build = parity_cargo_build(copied / "extensionalities/Cargo.toml", run_dir,
                                   "metadata-instrumented-glassbox", started,
                                   args.build_timeout, networked=networked)
        result["instrumented_build"] = build
        instrumented = Path(build["target_dir"]) / "release/glassbox"
        for fixture in (fixtures[0], fixtures[1], fixtures[18], fixtures[19], fixtures[22]):
            execution, raw, report = metadata_execute(
                instrumented, ["inspect", fixture["path"]],
                run_dir / f"instrumented-{fixture['family']}.log", args.run_timeout, started)
            text = raw.decode(errors="replace")
            checks = [{"id": "forbidden_traps_not_reached", "ok": "V-META-FORBIDDEN" not in text},
                      {"id": "expected_exit", "ok": execution["status"] == fixture["expected_exit"]}]
            if isinstance(report, dict):
                checks.extend(metadata_report_checks(fixture, report))
            else:
                checks.append({"id": "json_report", "ok": False})
            record_run(f"instrumented-{fixture['family']}", execution, checks)
        execution, raw, _ = metadata_execute(
            instrumented, ["extract", fixtures[0]["path"], "--no-ocr"],
            run_dir / "instrumented-trap-efficacy.log", args.run_timeout, started)
        record_run("instrumented-trap-efficacy", execution, [
            {"id": "trap_is_active", "ok": "V-META-FORBIDDEN" in raw.decode(errors="replace")},
        ])
    except (DriverError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        blockers.append({"stage": "metadata", "detail": str(error)})
    except Exception as error:
        blockers.append({"stage": "internal", "detail": f"{type(error).__name__}: {error}"})
    try:
        result["upstream_after"] = upstream_source_snapshot()
        if "upstream_before" not in result:
            result["upstream_unchanged"] = None
            result["upstream_assessment"] = "not_assessed_missing_before_snapshot"
            blockers.append({"stage": "upstream-compare",
                             "detail": "upstream mutation was not assessed because the before snapshot is missing"})
        else:
            result["upstream_unchanged"] = result["upstream_before"] == result["upstream_after"]
            result["upstream_assessment"] = "verified" if result["upstream_unchanged"] else "changed"
            if not result["upstream_unchanged"]:
                failures.append({"run": "upstream", "check": "unchanged",
                                 "detail": "real upstream checkout changed between captured V-META snapshots"})
    except Exception as error:
        blockers.append({"stage": "upstream-after", "detail": str(error)})
    elapsed = time.monotonic() - started
    result["costs"] = {"elapsed_active_s": round(elapsed, 4),
                       "within_30_minute_budget": elapsed <= PARITY_ACTIVE_BUDGET_S}
    if elapsed > PARITY_ACTIVE_BUDGET_S:
        blockers.append({"stage": "budget", "detail": "metadata active-test budget exceeded"})
    result["checks_total"] = sum(len(run["checks"]) for run in result["runs"]) + (1 if "upstream_unchanged" in result else 0)
    result["handoff"]["state"]["execution_status"] = "completed" if not failures and not blockers else "failed"
    result["handoff"]["state"]["pause_point"] = "V-META result ready for controller review"
    result["handoff"]["state"]["blockers"] = blockers
    result["handoff"]["evidence"]["checks"] = [{
        "verification_id": "V-META",
        "command": f"python3 -B {SCRIPT_PATH} metadata --evidence-dir {evidence_dir}",
        "result": "PASS" if not failures and not blockers else "FAIL" if failures else "BLOCKED",
        "observed_at": datetime.now(timezone.utc).isoformat(),
        "revision": result["handoff"]["source"]["revision"], "evidence": str(run_dir / "result.json")}]
    return finalize(result, run_dir, failures, blockers, evidence_dir)

def run_parity_case(args, evidence_dir):
    """Run V-NAME against real, aliased, relocated, and future-mutated executables.

    Accepts parsed driver arguments and an authorized evidence directory.
    Returns process exit code 0 for PASS, 1 for parity failure, and 2 for blocked execution.
    Retains failures and blockers in result.json; unexpected infrastructure errors never become PASS.
    The pinned-resource and base-revision reproducibility guards are opt-in
    through --require-pinned-resources/--require-base-revision; by default the
    observed limits and actual revision are recorded as evidence.
    """
    started = time.monotonic()
    observed_start = datetime.now(timezone.utc).isoformat()
    run = run_id()
    run_dir = mkdir_new(evidence_dir / f"parity-{run}")
    failures = []
    blockers = []
    result = {
        "handoff": {
            "handoff_schema": "chai-handoff/v1",
            "identity": {"work_id": "20261121_settling_in", "task_ids": ["B-NAME"],
                         "producer": "worker-build", "consumer": "master-drive",
                         "artifact": str(run_dir / "result.json"), "updated_at": observed_start},
            "state": {"objective": "Prove unified Glassbox naming and upstream CLI parity.",
                      "phase": "milestone-2 wave-4", "execution_status": "in_progress",
                      "pause_point": "V-NAME campaign execution",
                      "next_action": "Controller reviews actual V-NAME evidence.", "blockers": []},
            "source": {"predecessors": "resolved in-case by load_parity_prerequisites "
                                       "(explicit parameters, retained defaults, or fresh "
                                       "independent fixtures); recorded in result.baseline",
                       "workspace": str(REPO), "revision": "57e3afb7f1d59ca93811eda5748aa917613f64fc",
                       "uncommitted_changes": "Scoped B-NAME source snapshot recorded below",
                       "observed_at": observed_start},
            "authority": {"decisions": ["ADR-001", "C1 generated upstream CLI adapter"],
                          "approval_source": "Sri: Approve and resume production (Recommended)",
                          "scope": "B-NAME exact target files and isolated test/evidence trees",
                          "pending_approvals": ["independent audit", "Git publication"]},
            "evidence": {"verification_ids": ["V-NAME"], "checks": [],
                         "artifacts": [str(run_dir)], "missing_coverage": ["ARM64 only", "No independent audit"]},
            "receipt": {"consumer": "master-drive", "status": "pending"},
        },
        "case": "parity", "verification_id": "V-NAME", "task": "B-NAME",
        "status": None, "exit_code": None, "run_dir": str(run_dir),
        "timing_serial": True, "active_budget_s": PARITY_ACTIVE_BUDGET_S,
        "comparisons": [], "builds": {}, "inventories": {}, "runs": [],
    }
    sandbox = SANDBOX_ROOT / f"{SANDBOX_PREFIX}{run}" / "parity"
    result["resource_limits"] = resource_limit_snapshot()
    try:
        enforce_resource_pin(result, args)
        networked, network_reason = parity_network_permission(args)
        result["build_network_mode"] = {"networked": networked,
                                        "reason": network_reason}
        prerequisites = load_parity_prerequisites(args)
        baseline = prerequisites["baseline"]
        oracle = prerequisites["oracle"]
        result["baseline"] = {
            "mode": prerequisites["mode"],
            "result": prerequisites["baseline_result"],
            "oracle": prerequisites["oracle_path"],
            "note": prerequisites["note"],
            "checks_summary": (baseline or {}).get("checks_summary"),
            "reused_read_only": prerequisites["mode"] != "fresh_independent_fixtures",
        }
        before = upstream_source_snapshot()
        result["upstream_before"] = before
        enforce_base_revision(result, args, "B-NAME")
        result["environment"] = environment_snapshot()
        result["git"] = git_state(REPO)
        result["package_identity"] = verify_package_identity()
        if not all(check["ok"] for check in result["package_identity"]["checks"]):
            raise ParityConflict("downstream package, feature, dependency, or lock identity mismatch")
        sandbox.mkdir(parents=True, mode=0o700)
        result["sandbox"] = str(sandbox)
        common_absolute = sandbox / "common/screenshot"
        common_absolute.parent.mkdir(mode=0o700)
        shutil.copy2(fixture_record(oracle, "digital-1p.pdf")["path"], common_absolute)

        result["builds"]["original_upstream"] = parity_cargo_build(
            REPO / "Cargo.toml", run_dir, "original-upstream-lit", started,
            args.build_timeout, networked=networked)
        if args.skip_build:
            supplied = Path(args.binary).resolve()
            supplied_litx = supplied.with_name("litx")
            if not supplied.is_file() or not supplied_litx.is_file():
                raise DriverError("--skip-build parity requires both glassbox and sibling litx executables")
            result["builds"]["original_glassbox"] = {"skipped": True,
                                                      "provided_binary": str(supplied)}
        else:
            result["builds"]["original_glassbox"] = parity_cargo_build(
                EXT / "Cargo.toml", run_dir, "original-glassbox", started,
                args.build_timeout, networked=networked)
            production_target = Path(result["builds"]["original_glassbox"]["target_dir"])
            supplied = production_target / "release/glassbox"
            supplied_litx = production_target / "release/litx"
        original_upstream = Path(result["builds"]["original_upstream"]["target_dir"]) / "release/lit"
        executable_dir = run_dir / "executables"
        executable_dir.mkdir(mode=0o700)
        originals = {}
        for side, source in (("upstream", original_upstream), ("glassbox", supplied), ("litx", supplied_litx)):
            if not source.is_file() or not os.access(source, os.X_OK):
                raise DriverError(f"required {side} executable is missing or not runnable: {source}")
            retained = executable_dir / ("lit" if side == "upstream" else side)
            shutil.copy2(source, retained)
            originals[side] = retained
        result["executables"] = {side: {"path": str(path), "sha256": sha256_file(path),
                                        "bytes": path.stat().st_size}
                                 for side, path in originals.items()}

        original_probe_build, original_probe = prepare_inventory_probe(
            REPO, sandbox / "original-inventory", run_dir, "original-upstream-inventory",
            started, args.build_timeout, networked=networked)
        result["builds"]["original_inventory"] = original_probe_build
        expected_inventory = capture_inventory(original_probe, run_dir, "original-upstream", started)
        glassbox_inventory = capture_inventory(originals["glassbox"], run_dir, "original-glassbox",
                                               started, ("parse",))
        litx_inventory = capture_inventory(originals["litx"], run_dir, "original-litx",
                                           started, ("parse",))
        inventory_checks = [
            {"id": "glassbox_inventory_exact", "ok": expected_inventory == glassbox_inventory},
            {"id": "litx_inventory_exact", "ok": expected_inventory == litx_inventory},
            {"id": "hidden_commands_present", "ok": {"extract", "image-bounds"}.issubset(
                {command["name"] for command in glassbox_inventory if command["hidden"]})},
        ]
        result["inventories"]["original"] = {
            "commands": len(glassbox_inventory),
            "arguments": sum(len(command["arguments"]) for command in glassbox_inventory),
            "names": [command["name"] for command in glassbox_inventory], "checks": inventory_checks}
        if not all(check["ok"] for check in inventory_checks):
            raise ParityConflict("original complete command/argument inventory differs")

        result["runs"] = verify_downstream_routes(originals, run_dir, started, args.run_timeout)
        if any(not check["ok"] for record in result["runs"] for check in record["checks"]):
            raise ParityConflict("downstream route or litx compatibility regression")
        for case in parity_cases(common_absolute):
            comparison = execute_parity_case(case, originals, oracle, run_dir, started, args.run_timeout)
            result["comparisons"].append(comparison)
            if comparison["result"] != "PASS":
                raise ParityConflict(f"parity conflict in {case['name']}; see {comparison['evidence']}")
        relocated = verify_relocated_installation(originals, oracle, sandbox, run_dir,
                                                  started, args.run_timeout)
        result["comparisons"].append(relocated)
        if relocated["result"] != "PASS":
            raise ParityConflict("relocated single-binary installation differs")

        mutated_root = copy_upstream_workspace(sandbox / "mutated")
        copy_downstream_workspace(mutated_root / "extensionalities")
        result["mutation"] = mutate_upstream(mutated_root)
        result["builds"]["mutated_upstream"] = parity_cargo_build(
            mutated_root / "Cargo.toml", run_dir, "mutated-upstream-lit", started,
            args.build_timeout, networked=networked)
        result["builds"]["mutated_glassbox"] = parity_cargo_build(
            mutated_root / "extensionalities/Cargo.toml", run_dir, "mutated-glassbox",
            started, args.build_timeout, networked=networked)
        mutated = {}
        mutated_executable_dir = executable_dir / "mutated"
        mutated_executable_dir.mkdir(mode=0o700)
        mutated_upstream_target = Path(result["builds"]["mutated_upstream"]["target_dir"])
        mutated_production_target = Path(result["builds"]["mutated_glassbox"]["target_dir"])
        for side, source in (("upstream", mutated_upstream_target / "release/lit"),
                             ("glassbox", mutated_production_target / "release/glassbox"),
                             ("litx", mutated_production_target / "release/litx")):
            retained = mutated_executable_dir / ("lit" if side == "upstream" else side)
            shutil.copy2(source, retained)
            mutated[side] = retained
        result["executables"].update({f"mutated_{side}": {"path": str(path),
                                      "sha256": sha256_file(path), "bytes": path.stat().st_size}
                                      for side, path in mutated.items()})
        mutated_probe_build, mutated_probe = prepare_inventory_probe(
            mutated_root, sandbox / "mutated-inventory", run_dir, "mutated-upstream-inventory",
            started, args.build_timeout, networked=networked)
        result["builds"]["mutated_inventory"] = mutated_probe_build
        mutated_expected = capture_inventory(mutated_probe, run_dir, "mutated-upstream", started)
        mutated_glassbox = capture_inventory(mutated["glassbox"], run_dir, "mutated-glassbox",
                                             started, ("parse",))
        mutated_litx = capture_inventory(mutated["litx"], run_dir, "mutated-litx",
                                         started, ("parse",))
        future = next((command for command in mutated_glassbox if command["name"] == "future-probe"), {})
        mutated_checks = [
            {"id": "mutated_glassbox_inventory_exact", "ok": mutated_expected == mutated_glassbox},
            {"id": "mutated_litx_inventory_exact", "ok": mutated_expected == mutated_litx},
            {"id": "future_command_hidden", "ok": future.get("hidden") is True},
            {"id": "future_alias_discovered", "ok": "future-alias" in future.get("aliases", [])},
            {"id": "future_parse_option_discovered", "ok": any(
                argument.get("long") == "future-parse-option"
                for command in mutated_glassbox if command["name"] == "parse"
                for argument in command["arguments"])},
        ]
        result["inventories"]["mutated"] = {
            "commands": len(mutated_glassbox),
            "arguments": sum(len(command["arguments"]) for command in mutated_glassbox),
            "names": [command["name"] for command in mutated_glassbox], "checks": mutated_checks}
        if not all(check["ok"] for check in mutated_checks):
            raise ParityConflict("mutated complete command/argument inventory differs")
        for case in mutation_cases():
            comparison = execute_parity_case(case, mutated, oracle, run_dir, started, args.run_timeout)
            result["comparisons"].append(comparison)
            if comparison["result"] != "PASS":
                raise ParityConflict(f"future parity conflict in {case['name']}; see {comparison['evidence']}")
    except ParityConflict as error:
        failures.append({"run": "parity", "check": "contract", "detail": str(error)})
    except (DriverError, OSError, subprocess.SubprocessError) as error:
        blockers.append({"stage": "parity", "detail": str(error)})
    except Exception as error:
        blockers.append({"stage": "internal", "detail": f"{type(error).__name__}: {error}"})
    try:
        result["upstream_after"] = upstream_source_snapshot()
        result["upstream_unchanged"] = result.get("upstream_before") == result["upstream_after"]
        if not result["upstream_unchanged"]:
            failures.append({"run": "upstream", "check": "unchanged",
                             "detail": "real upstream checkout changed during V-NAME"})
    except Exception as error:
        blockers.append({"stage": "upstream-after", "detail": str(error)})
    elapsed = time.monotonic() - started
    result["costs"] = {
        "elapsed_active_s": round(elapsed, 4),
        "build_s": round(sum(record.get("build", {}).get("duration_s", 0)
                             for record in result["builds"].values()), 4),
        "compared_process_s": round(sum(sum(case["durations_s"].values())
                                        for case in result["comparisons"]), 4),
        "within_30_minute_budget": elapsed <= PARITY_ACTIVE_BUDGET_S,
    }
    if elapsed > PARITY_ACTIVE_BUDGET_S:
        blockers.append({"stage": "budget", "detail": "parity active-test budget exceeded"})
    result["comparison_matrix"] = {
        "case_count": len(result["comparisons"]),
        "passed": sum(case["result"] == "PASS" for case in result["comparisons"]),
        "failed": sum(case["result"] != "PASS" for case in result["comparisons"]),
        "status_comparisons_per_alias": len(result["comparisons"]),
        "file_effect_comparisons_per_alias": len(result["comparisons"]),
        "meaningful_differences_accepted": 0,
        "independent_output_assertions": sum(case["independent_output_assertions"]
                                              for case in result["comparisons"]),
    }
    inventory_checks = sum((record["checks"] for record in result["inventories"].values()), [])
    result["checks_total"] = (
        sum(len(case["checks"]) + case["independent_output_assertions"]
            for case in result["comparisons"])
        + sum(len(record["checks"]) for record in result["runs"])
        + len(inventory_checks) + len(result.get("package_identity", {}).get("checks", []))
        + (1 if "upstream_unchanged" in result else 0))
    result["handoff"]["state"]["execution_status"] = "completed" if not failures and not blockers else "failed"
    result["handoff"]["state"]["pause_point"] = "V-NAME result ready for controller review"
    result["handoff"]["state"]["blockers"] = blockers
    result["handoff"]["evidence"]["checks"] = [{
        "verification_id": "V-NAME",
        "command": f"python3 -B {SCRIPT_PATH} parity --evidence-dir {evidence_dir}",
        "result": "PASS" if not failures and not blockers else "FAIL" if failures else "BLOCKED",
        "observed_at": datetime.now(timezone.utc).isoformat(),
        "revision": result["handoff"]["source"]["revision"], "evidence": str(run_dir / "result.json")}]
    return finalize(result, run_dir, failures, blockers, evidence_dir)


def finalize(result, run_dir, failures, blockers, evidence_dir):
    """Finalize one case with truthful status, check counts, evidence, and exit code.

    Accepts the mutable result, unique run directory, failure/blocker lists, and evidence root.
    Returns 0 for PASS, 1 for FAIL, or 2 for BLOCKED after writing result.json.
    Raises DriverError or OSError when the exclusive result evidence cannot be written.
    """
    if blockers:
        status, exit_code = "BLOCKED", 2
    elif failures:
        status, exit_code = "FAIL", 1
    else:
        status, exit_code = "PASS", 0
    result["status"] = status
    result["exit_code"] = exit_code
    result["failures"] = failures
    result["blockers"] = blockers
    result["checks_summary"] = {
        "total": result.get("checks_total", sum(
            len(r.get("checks", [])) for r in result.get("runs", []))),
        "failed": len(failures),
    }
    write_exclusive(run_dir / "result.json", json.dumps(result, indent=2))
    summary = (f"{result.get('verification_id', 'UNKNOWN')} "
               f"{result.get('case', 'case')}: {status} "
               f"evidence={run_dir / 'result.json'}")
    if failures:
        preview = "; ".join(f"{f['run']}/{f['check']}: {f['detail']}"
                            for f in failures[:8])
        summary += f" failures=[{preview}]"
    if blockers:
        summary += f" blockers={[b['detail'] for b in blockers][:2]}"
    print(summary)
    return exit_code


# ---------------------------------------------------- inventory and status

INVENTORY_TARGET_FILES = (
    "extensionalities/crates/litx-inspect/src/lib.rs",
    "extensionalities/crates/litx-inspect/src/hashing.rs",
    "extensionalities/crates/litx-inspect/src/reports.rs",
    "extensionalities/crates/litx/src/main.rs",
    "extensionalities/crates/litx/src/inventory.rs",
    "extensionalities/crates/litx/src/output.rs",
    "extensionalities/crates/litx/tests/inventory_cli.rs",
    "extensionalities/crates/litx-inspect/tests/hashing.rs",
    "extensionalities/crates/litx/tests/extract_cli.rs",
    "extensionalities/crates/litx/tests/inspect_cli.rs",
    "extensionalities/scripts/verify-settling-in.py",
    "extensionalities/scripts/tests/test_settling_in_driver.py",
)


JOBS_TARGET_FILES = (
    "extensionalities/crates/litx-inspect/src/lib.rs",
    "extensionalities/crates/litx-inspect/src/images.rs",
    "extensionalities/crates/litx-inspect/src/pdf.rs",
    "extensionalities/crates/litx/src/main.rs",
    "extensionalities/crates/litx/src/batch.rs",
    "extensionalities/crates/litx/src/inventory.rs",
    "extensionalities/crates/litx/tests/batch_jobs.rs",
    "extensionalities/crates/litx/tests/inventory_cli.rs",
    "extensionalities/scripts/verify-settling-in.py",
    "extensionalities/scripts/tests/test_settling_in_driver.py",
)


def source_manifest(target_files, label):
    """Hash one explicitly authorized source set without traversing other trees.

    Accepts relative target paths and a diagnostic label. Returns file hashes and
    a deterministic tree digest. Missing or linked sources raise DriverError.
    """
    manifest = {}
    for relative in target_files:
        path = REPO / relative
        if not path.is_file() or path.is_symlink():
            raise DriverError(f"authorized {label} source is missing or linked: {path}")
        manifest[relative] = sha256_file(path)
    tree = hashlib.sha256()
    for relative in sorted(manifest):
        tree.update(f"{manifest[relative]}  {relative}\n".encode())
    return {"tree_sha256": tree.hexdigest(), "files": manifest}


def inventory_source_snapshot():
    """Hash exactly the authorized B-INVENTORY source and test paths."""
    return source_manifest(INVENTORY_TARGET_FILES, "inventory")


def jobs_source_snapshot():
    """Hash exactly the authorized B-JOBS source and test paths."""
    return source_manifest(JOBS_TARGET_FILES, "jobs")


def operation_result(case, verification_id, run_dir, observed_start):
    """Create the shared six-group handoff for an inventory/status campaign.

    Accepts case name, verification ID, exclusive run directory, and UTC start.
    Returns a mutable result with pending controller receipt and explicit scope.
    No command runs and no evidence claim is finalized by this constructor.
    """
    return {
        "handoff": {
            "handoff_schema": "chai-handoff/v1",
            "identity": {"work_id": "20261121_settling_in", "task_ids": ["B-INVENTORY"],
                         "producer": "worker-build", "consumer": "master-drive",
                         "artifact": str(run_dir / "result.json"), "updated_at": observed_start},
            "state": {"objective": "Prove safe deterministic inventory, qualified hashes, inert advice, artifacts, and exit severity.",
                      "phase": "milestone-3 wave-8", "execution_status": "in_progress",
                      "pause_point": f"{verification_id} campaign execution",
                      "next_action": "Controller reviews evidence; B-JOBS remains pending.", "blockers": []},
            "source": {"predecessors": [str(EXT / "dev/PRDs/20261121_settling_in/spec.md")],
                       "workspace": str(REPO), "revision": "57e3afb7f1d59ca93811eda5748aa917613f64fc",
                       "uncommitted_changes": "Accepted B-NAME/B-SCHEMA/B-META/B-EXTRACT plus scoped B-INVENTORY",
                       "observed_at": observed_start},
            "authority": {"decisions": ["ADR-001", "A-GATE approved production"],
                          "approval_source": "Approved spec, milestones, and B-INVENTORY dispatch",
                          "scope": "B-INVENTORY only; no mounts, NAS, installs, Git mutations, deletion, or publication",
                          "pending_approvals": ["independent audit", "native x64", "Git publication"]},
            "evidence": {"verification_ids": [verification_id], "checks": [],
                         "artifacts": [str(run_dir)],
                         "missing_coverage": ["Live bind-mount behavior is parser-tested only; no privileged mount was attempted",
                                              "Best-effort consistency is not an adversarial immutable snapshot",
                                              "Native x64 and independent audit remain pending"]},
            "receipt": {"consumer": "master-drive", "status": "pending"},
        },
        "case": case, "verification_id": verification_id, "task": "B-INVENTORY",
        "status": None, "exit_code": None, "run_dir": str(run_dir),
        "timing_serial": True, "runs": [], "source_snapshot": inventory_source_snapshot(),
        "resource_limits": resource_limit_snapshot(),
        "batch_jobs_coverage": "NOT IMPLEMENTED: reserved for B-JOBS wave-9",
    }


def operation_binary(args, run_dir, result):
    """Select or build the real release executable for one operation campaign.

    Accepts parsed driver arguments, exclusive run directory, and mutable result.
    Returns an executable Path after recording build or provided-binary evidence.
    Missing executables and failed builds raise DriverError; Tesseract remains enabled.
    """
    if args.skip_build:
        binary = Path(args.binary).resolve()
        result["build"] = {"skipped": True, "provided_binary": str(binary)}
    else:
        build, target, status = do_build(args, run_dir)
        result["build"] = build
        if status != 0:
            raise DriverError(f"release build failed with status {status}")
        binary = target / "release/glassbox"
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise DriverError(f"Glassbox executable is missing or not runnable: {binary}")
    result["binary"] = {"path": str(binary), "sha256": sha256_file(binary),
                        "bytes": binary.stat().st_size}
    return binary


def finish_operation_result(result, run_dir, failures, blockers, evidence_dir, started):
    """Finalize an inventory/status campaign with counts and actual outcome.

    Accepts the mutable result, run/evidence paths, failure/blocker lists, and
    monotonic start. Returns finalize's 0 PASS, 1 FAIL, or 2 BLOCKED status.
    Result write failures propagate; no failed or missing check becomes PASS.
    """
    elapsed = time.monotonic() - started
    result["costs"] = {"elapsed_active_s": round(elapsed, 4),
                       "within_30_minute_budget": elapsed <= PARITY_ACTIVE_BUDGET_S}
    if elapsed > PARITY_ACTIVE_BUDGET_S:
        blockers.append({"stage": "budget", "detail": "operation campaign exceeded 30 minutes"})
    result["checks_total"] = sum(len(run["checks"]) for run in result["runs"])
    result["handoff"]["state"]["execution_status"] = "completed" if not failures and not blockers else "failed"
    result["handoff"]["state"]["pause_point"] = f"{result['verification_id']} result ready for controller review"
    result["handoff"]["state"]["blockers"] = blockers
    result["handoff"]["evidence"]["checks"] = [{
        "verification_id": result["verification_id"],
        "command": f"python3 -B {SCRIPT_PATH} {result['case']} --evidence-dir {evidence_dir}",
        "result": "PASS" if not failures and not blockers else "FAIL" if failures else "BLOCKED",
        "observed_at": datetime.now(timezone.utc).isoformat(),
        "revision": result["handoff"]["source"]["revision"],
        "source_tree_sha256": result["source_snapshot"]["tree_sha256"],
        "evidence": str(run_dir / "result.json")}]
    return finalize(result, run_dir, failures, blockers, evidence_dir)


def decode_jsonl(raw):
    """Decode strict JSONL while rejecting empty informational output lines.

    Accepts raw combined child output bytes. Returns ordered Python values.
    Invalid UTF-8, blank lines, or malformed JSON raise DriverError; no partial
    parse is accepted as evidence.
    """
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise DriverError(f"child JSONL is not UTF-8: {error}") from error
    lines = text.splitlines()
    if any(not line.strip() for line in lines):
        raise DriverError("child JSONL contains an informational blank line")
    try:
        return [json.loads(line) for line in lines]
    except json.JSONDecodeError as error:
        raise DriverError(f"child JSONL is malformed: {error}; output={text[:500]!r}") from error


def run_inventory_case(args, evidence_dir):
    """Run V-INVENTORY against the real release CLI and independent oracles.

    Accepts parsed arguments and an authorized evidence root. Returns 0 PASS,
    1 FAIL, or 2 BLOCKED after exclusive evidence publication. Fresh bounded
    children cover deterministic immediate/recursive traversal, links, full
    hashes, advice, permissions, non-Unicode paths, and external artifacts.
    The pinned-resource reproducibility guard is opt-in through
    --require-pinned-resources; by default observed limits are recorded.
    """
    started = time.monotonic()
    observed_start = datetime.now(timezone.utc).isoformat()
    run_dir = mkdir_new(evidence_dir / f"inventory-{run_id()}")
    failures, blockers = [], []
    result = operation_result("inventory", "V-INVENTORY", run_dir, observed_start)
    result["mount_coverage"] = {
        "parser": "Rust unit tests cover escaped paths, overmounts, network types, and same-device bind IDs",
        "live_bind_mount": "UNTESTED: no privileged mount, unshare, or user-namespace action was authorized",
        "network_traversal": "No network or NAS root was opened or scanned",
    }

    def record_run(name, execution, checks):
        """Retain one command and fold failed assertions into campaign outcome.

        Accepts a unique run name, execution evidence, and boolean check list.
        Returns unit after appending evidence. No assertion is hidden or rewritten.
        """
        result["runs"].append({"name": name, "execution": execution, "checks": checks})
        for check in checks:
            if not check["ok"]:
                failures.append({"run": name, "check": check["id"],
                                 "detail": check.get("detail", "assertion failed")})

    def execute(name, argv, input_bytes=None):
        """Execute one measured release command and decode strict JSONL.

        Accepts a run name, argument vector, and optional stdin bytes. Returns
        execution, raw bytes, and decoded records. Operational exits remain
        evidence; transport and malformed JSON failures raise DriverError.
        """
        execution = parity_execute([binary, *argv], run_dir / f"{name}.log",
                                   args.run_timeout, started, input_bytes=input_bytes)
        raw = Path(execution["log"]).read_bytes()
        return execution, raw, decode_jsonl(raw)

    try:
        enforce_resource_pin(result, args)
        result["environment"] = environment_snapshot()
        binary = operation_binary(args, run_dir, result)
        sandbox = SANDBOX_ROOT / f"{SANDBOX_PREFIX}{run_dir.name.removeprefix('inventory-')}" / "inventory"
        root = sandbox / "root"
        root.mkdir(parents=True, mode=0o700)
        result["sandbox"] = str(sandbox)
        fixtures = {
            ".hidden.txt": b"hidden\n",
            "a.txt": b"alpha\n",
            "binary.bin": bytes((0, 1, 2, 255)),
            "empty.bin": b"",
            "large.bin": (bytes(range(251)) * 8400)[:2 * 1024 * 1024 + 17],
            "space ;$(not-run).txt": b"metacharacters\n",
        }
        for name, data in fixtures.items():
            write_exclusive(root / name, data)
        (root / "sub").mkdir(mode=0o700)
        write_exclusive(root / "sub/nested.txt", b"nested\n")
        os.symlink(root / "a.txt", root / "file-link")
        os.symlink(root / "sub", root / "dir-link")
        os.symlink(root, root / "loop")
        result["fixture_manifest"] = [
            {"path": str(root / name), "sha256": sha256_bytes(data), "bytes": len(data)}
            for name, data in sorted(fixtures.items())
        ] + [{"path": str(root / "sub/nested.txt"),
              "sha256": sha256_file(root / "sub/nested.txt"), "bytes": 7}]
        immediate_names = sorted([*fixtures, "dir-link", "file-link", "loop"], key=os.fsencode)
        first, first_raw, first_records = execute("immediate-first", ["inspect", str(root)])
        second, second_raw, second_records = execute("immediate-second", ["inspect", str(root)])
        checks = [
            {"id": "partial_links_exit_2_first", "ok": first["status"] == 2},
            {"id": "partial_links_exit_2_second", "ok": second["status"] == 2},
            {"id": "byte_deterministic_jsonl", "ok": first_raw == second_raw},
            {"id": "ordered_record_paths", "ok": [record.get("path") for record in first_records] ==
             [str(root / name) for name in immediate_names]},
            {"id": "ordered_unique_ids", "ok": all(
                record.get("sequence") == index and record.get("id") == index
                for index, record in enumerate(first_records, 1))},
            {"id": "immediate_excludes_nested", "ok": all(
                record.get("path") != str(root / "sub/nested.txt") for record in first_records)},
            {"id": "three_link_diagnostics", "ok": sum(
                (record.get("error") or {}).get("code") == "symlink_skipped"
                for record in first_records) == 3},
        ]
        for record in first_records:
            report = record.get("report")
            if not report:
                continue
            source = Path(record["path"])
            metadata = report.get("metadata", {})
            checks.extend([
                {"id": f"sha256_oracle_{source.name}", "ok":
                 metadata.get("digest", {}).get("digest") == sha256_file(source)},
                {"id": f"full_bytes_{source.name}", "ok":
                 metadata.get("digest", {}).get("bytes_hashed", {}).get("value") == source.stat().st_size},
                {"id": f"consistent_{source.name}", "ok":
                 metadata.get("digest", {}).get("consistency") == "consistent"},
                {"id": f"not_immutable_{source.name}", "ok":
                 metadata.get("digest", {}).get("snapshot_guarantee") == "not_immutable"},
                {"id": f"metadata_envelope_{source.name}", "ok":
                 report.get("schema_version") == 2 and report.get("kind") == "inspect" and
                 "content" not in report and "text" not in report},
            ])
        record_run("immediate-inventory", first, checks)
        record_run("immediate-determinism", second, [
            {"id": "same_record_count", "ok": len(second_records) == len(first_records)}])

        recursive, _, recursive_records = execute("recursive", ["inspect", str(root), "--recursive"])
        record_run("recursive-inventory", recursive, [
            {"id": "partial_links_exit_2", "ok": recursive["status"] == 2},
            {"id": "nested_exactly_once", "ok": sum(
                record.get("path") == str(root / "sub/nested.txt")
                for record in recursive_records) == 1},
            {"id": "no_loop_growth", "ok": len(recursive_records) == len(first_records) + 1},
            {"id": "nested_sha256", "ok": next(
                record["report"]["metadata"]["digest"]["digest"]
                for record in recursive_records
                if record.get("path") == str(root / "sub/nested.txt")) ==
             sha256_file(root / "sub/nested.txt")},
        ])

        advice_execution, _, advice_records = execute("advice", ["inspect", str(root / "space ;$(not-run).txt")])
        advice = advice_records[0].get("metadata", {}).get("advice", [])
        record_run("inert-advice", advice_execution, [
            {"id": "complete_exit_0", "ok": advice_execution["status"] == 0},
            {"id": "exact_argument_vector", "ok": bool(advice) and advice[0].get("arguments") ==
             ["extract", str(root / "space ;$(not-run).txt")]},
            {"id": "unknown_cost", "ok": bool(advice) and advice[0].get("cost") == "unknown"},
            {"id": "ocr_unassessed", "ok": advice_records[0].get("metadata", {}).get("ocr_recommended") == "unknown"},
            {"id": "no_shell_command", "ok": bool(advice) and "command" not in advice[0] and
             "shell" not in advice[0]},
        ])

        artifact_dir = sandbox / "artifacts"
        artifact_execution, _, artifact_records = execute(
            "artifacts", ["inspect", str(root), "--output-dir", str(artifact_dir)])
        artifact_checks = [{"id": "partial_links_exit_2", "ok": artifact_execution["status"] == 2}]
        artifacts = [artifact for record in artifact_records
                     for artifact in (record.get("report") or {}).get("metadata", {}).get("artifacts", [])]
        artifact_checks.append({"id": "two_artifacts_per_regular_file", "ok": len(artifacts) == 2 * len(fixtures)})
        artifact_checks.append({"id": "unique_ordered_artifact_paths", "ok":
                                len({artifact.get("path") for artifact in artifacts}) == len(artifacts)})
        for artifact in artifacts:
            path = Path(artifact.get("path", ""))
            artifact_checks.extend([
                {"id": f"private_file_{path.name}", "ok": path.is_file() and
                 (path.stat().st_mode & 0o777) == 0o600},
                {"id": f"absolute_artifact_{path.name}", "ok": path.is_absolute()},
            ])
            if path.suffix == ".json" and path.is_file():
                saved = json.loads(path.read_text(encoding="utf-8"))
                artifact_checks.append({"id": f"saved_v2_{path.name}", "ok":
                                        saved.get("schema_version") == 2 and
                                        saved.get("kind") == "inspect" and "content" not in saved})
            elif path.is_file():
                summary = path.read_text(encoding="utf-8")
                artifact_checks.append({"id": f"metadata_summary_{path.name}", "ok":
                                        "Glassbox metadata inspection" in summary and
                                        "alpha" not in summary and "metacharacters" not in summary})
        artifact_checks.append({"id": "private_output_directory", "ok":
                                (artifact_dir.stat().st_mode & 0o777) == 0o700})
        record_run("inventory-artifacts", artifact_execution, artifact_checks)

        conflict = parity_execute([binary, "inspect", str(root), "--output-dir", str(root / "output")],
                                  run_dir / "output-inside.log", args.run_timeout, started)
        record_run("output-inside-rejected", conflict, [
            {"id": "invalid_exit_64", "ok": conflict["status"] == 64},
            {"id": "no_source_artifact_directory", "ok": not (root / "output").exists()},
        ])

        opaque_root = sandbox / "opaque"
        opaque_root.mkdir(mode=0o700)
        descriptor = os.open(os.path.join(os.fsencode(opaque_root), b"a\xff"),
                             os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(b"opaque")
        write_exclusive(opaque_root / "z.txt", b"valid\n")
        opaque_execution, _, opaque_records = execute("non-utf8", ["inspect", str(opaque_root)])
        record_run("non-utf8-continuation", opaque_execution, [
            {"id": "operational_exit_1", "ok": opaque_execution["status"] == 1},
            {"id": "two_valid_jsonl_records", "ok": len(opaque_records) == 2},
            {"id": "explicit_path_error", "ok":
             (opaque_records[0].get("error") or {}).get("code") == "non_utf8_path_unsupported"},
            {"id": "later_file_complete", "ok":
             opaque_records[1].get("report", {}).get("metadata", {}).get("status") == "complete"},
        ])

        permission_root = sandbox / "permission"
        permission_root.mkdir(mode=0o700)
        denied = permission_root / "denied.txt"
        write_exclusive(denied, b"denied\n")
        write_exclusive(permission_root / "z.txt", b"available\n")
        os.chmod(denied, 0)
        try:
            if os.geteuid() == 0:
                blockers.append({"stage": "permission-fixture",
                                 "detail": "UID 0 cannot establish an unprivileged read-denial oracle"})
            else:
                denied_execution, _, denied_records = execute(
                    "permission", ["inspect", str(permission_root)])
                record_run("permission-continuation", denied_execution, [
                    {"id": "operational_exit_1", "ok": denied_execution["status"] == 1},
                    {"id": "read_failure_record", "ok": any(
                        (record.get("error") or {}).get("code") == "file_open_failed" or
                        record.get("report", {}).get("metadata", {}).get("status") == "failed"
                        for record in denied_records)},
                    {"id": "later_file_complete", "ok":
                     denied_records[-1].get("report", {}).get("metadata", {}).get("status") == "complete"},
                ])
        finally:
            os.chmod(denied, 0o600)
    except (DriverError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        blockers.append({"stage": "inventory", "detail": str(error)})
    except Exception as error:
        blockers.append({"stage": "internal", "detail": f"{type(error).__name__}: {error}"})
    return finish_operation_result(result, run_dir, failures, blockers, evidence_dir, started)


def run_status_case(args, evidence_dir):
    """Run V-STATUS through real release entrypoints and saved artifacts.

    Accepts parsed driver arguments and an authorized evidence root. Returns 0
    PASS, 1 FAIL, or 2 BLOCKED after checking 0/2/1/64 outcomes, 64-over-1-over-2
    aggregation, continuation, truncation, private no-overwrite saves, concurrent
    collisions, symlink rejection, and optional thumbnails. The pinned-resource
    reproducibility guard is opt-in through --require-pinned-resources; by
    default observed limits are recorded.
    """
    started = time.monotonic()
    observed_start = datetime.now(timezone.utc).isoformat()
    run_dir = mkdir_new(evidence_dir / f"status-{run_id()}")
    failures, blockers = [], []
    result = operation_result("status", "V-STATUS", run_dir, observed_start)
    result["signal_coverage"] = {
        "broken_pipe": "tested as operational exit 1",
        "sigint": "UNTESTED live cancellation; serial unnamed temporary files close on process termination; B-JOBS owns worker cancellation",
    }

    def record_run(name, execution, checks):
        """Retain one status command and collect failed assertions.

        Accepts a unique run name, execution evidence, and boolean checks.
        Returns unit after appending results. No expected nonzero child exit is
        treated as a driver transport error or hidden from the evidence.
        """
        result["runs"].append({"name": name, "execution": execution, "checks": checks})
        for check in checks:
            if not check["ok"]:
                failures.append({"run": name, "check": check["id"],
                                 "detail": check.get("detail", "assertion failed")})

    def report_run(name, argv, expected_exit, expected_status, kind):
        """Run and validate one single-document v2 JSON command.

        Accepts run name, argument vector, expected exit/status, and report kind.
        Returns execution, raw bytes, and decoded report. Wrong envelope/status
        becomes a failed check; malformed JSON yields explicit failed checks.
        """
        execution, raw, report = extract_execute(
            binary, argv, run_dir / f"{name}.log", args.run_timeout, started)
        checks = [
            {"id": "expected_exit", "ok": execution["status"] == expected_exit,
             "detail": f"expected {expected_exit}, got {execution['status']}"},
            {"id": "v2_kind", "ok": isinstance(report, dict) and
             report.get("schema_version") == 2 and report.get("kind") == kind},
            {"id": "operation_status", "ok": isinstance(report, dict) and
             report.get("metadata", {}).get("status") == expected_status,
             "detail": f"actual {(report or {}).get('metadata', {}).get('status')}"},
        ]
        record_run(name, execution, checks)
        return execution, raw, report

    def batch_run(name, operation, requests, expected_exit, expected_ids):
        """Run one ordered JSONL batch and check aggregate status and recovery.

        Accepts run name, inspect/extract operation, raw request lines, expected
        exit, and ordered IDs. Returns execution and decoded responses. Invalid
        JSONL raises DriverError instead of creating a false PASS.
        """
        execution = parity_execute([binary, operation, "--batch", "--no-ocr"] if operation == "extract"
                                   else [binary, operation, "--batch"],
                                   run_dir / f"{name}.log", args.run_timeout, started,
                                   input_bytes=requests)
        raw = Path(execution["log"]).read_bytes()
        responses = decode_jsonl(raw)
        record_run(name, execution, [
            {"id": "aggregate_exit", "ok": execution["status"] == expected_exit,
             "detail": f"expected {expected_exit}, got {execution['status']}"},
            {"id": "ordered_ids", "ok": [response.get("id") for response in responses] == expected_ids},
            {"id": "response_count", "ok": len(responses) == len(expected_ids)},
        ])
        return execution, responses

    def artifact_pair(name, operation, source):
        """Verify one successful save and one no-overwrite operational collision.

        Accepts a run prefix, inspect/extract operation, and source path.
        Returns unit after checking two typed 0600 artifacts, a 0700 directory,
        saved v2 JSON, identical retry names, failed retry, and preserved bytes.
        Filesystem decoding failures become DriverError or failed checks.
        """
        output = sandbox / name
        argv = [operation, str(source), "--output-dir", str(output)]
        if operation == "extract":
            argv.append("--no-ocr")
        first_execution, _, first_report = report_run(
            f"{name}-first", argv, 0, "complete", operation)
        artifacts = (first_report or {}).get("metadata", {}).get("artifacts", [])
        checks = [{"id": "two_artifacts", "ok": len(artifacts) == 2}]
        before = {}
        for artifact in artifacts:
            path = Path(artifact.get("path", ""))
            if path.is_file():
                before[path] = sha256_file(path)
            checks.extend([
                {"id": f"private_0600_{path.name}", "ok": path.is_file() and
                 (path.stat().st_mode & 0o777) == 0o600},
                {"id": f"absolute_path_{path.name}", "ok": path.is_absolute()},
            ])
            if path.suffix == ".json" and path.is_file():
                saved = json.loads(path.read_text(encoding="utf-8"))
                checks.append({"id": f"saved_v2_{path.name}", "ok":
                               saved.get("schema_version") == 2 and
                               saved.get("kind") == operation})
        checks.extend([
            {"id": "private_0700_directory", "ok": output.is_dir() and
             (output.stat().st_mode & 0o777) == 0o700},
            {"id": "no_named_temporary_files", "ok": len(list(output.iterdir())) == 2},
        ])
        record_run(f"{name}-artifact-contract", first_execution, checks)
        second_execution, _, second_report = report_run(
            f"{name}-collision", argv, 1, "failed", operation)
        record_run(f"{name}-collision-preservation", second_execution, [
            {"id": "artifact_diagnostic", "ok": any(
                diagnostic.get("stage") == "artifact"
                for diagnostic in (second_report or {}).get("metadata", {}).get("diagnostics", []))},
            {"id": "existing_bytes_preserved", "ok": bool(before) and all(
                path.is_file() and sha256_file(path) == digest for path, digest in before.items())},
            {"id": "no_extra_files", "ok": len(list(output.iterdir())) == 2},
        ])

    try:
        enforce_resource_pin(result, args)
        result["environment"] = environment_snapshot()
        binary = operation_binary(args, run_dir, result)
        sandbox = SANDBOX_ROOT / f"{SANDBOX_PREFIX}{run_dir.name.removeprefix('status-')}" / "status"
        sandbox.mkdir(parents=True, mode=0o700)
        result["sandbox"] = str(sandbox)
        plain = sandbox / "plain.txt"
        lines = [f"line-{index}" for index in range(12)]
        write_exclusive(plain, "\n".join(lines) + "\n")
        unknown = sandbox / "unknown.glassbox-unknown"
        write_exclusive(unknown, b"unknown\x00format")
        parquet = sandbox / "identity.parquet"
        write_exclusive(parquet, b"PAR1 identification only PAR1")
        djvu = EXT / "fixtures/single.djvu"
        missing = sandbox / "missing.txt"
        result["fixture_manifest"] = [
            {"path": str(path), "sha256": sha256_file(path), "bytes": path.stat().st_size}
            for path in (plain, unknown, parquet, djvu)
        ]

        report_run("complete-inspect", ["inspect", str(plain)], 0, "complete", "inspect")
        report_run("complete-extract", ["extract", str(plain), "--no-ocr"], 0, "complete", "extract")
        _, _, partial = report_run("partial-truncated", ["extract", str(plain), "--max-chars", "7", "--no-ocr"],
                                   2, "partial", "extract")
        record_run("partial-truncation-coverage", {"status": 0, "kind": "report_assertion"}, [
            {"id": "safety_limit_omission", "ok":
             (partial or {}).get("selection", {}).get("omitted") == "safety_limit"},
            {"id": "partial_source_coverage", "ok": (partial or {}).get("completeness") == "partial"},
        ])
        report_run("unsupported-djvu-extract", ["extract", str(djvu), "--no-ocr"],
                   2, "unsupported", "extract")
        report_run("unsupported-parquet-extract", ["extract", str(parquet), "--no-ocr"],
                   2, "unsupported", "extract")
        report_run("complete-djvu-inspect", ["inspect", str(djvu)], 0, "complete", "inspect")
        report_run("complete-parquet-inspect", ["inspect", str(parquet)], 0, "complete", "inspect")
        report_run("operational-missing", ["inspect", str(missing)], 1, "failed", "inspect")
        invalid = parity_execute([binary, "extract", str(plain), "--pages", "0"],
                                 run_dir / "invalid-selector.log", args.run_timeout, started)
        record_run("invalid-selector", invalid, [
            {"id": "usage_exit_64", "ok": invalid["status"] == 64},
            {"id": "no_json_success", "ok": not Path(invalid["log"]).read_bytes().startswith(b"{")},
        ])
        jobs = parity_execute([binary, "inspect", str(plain), "--jobs", "2"],
                              run_dir / "invalid-jobs.log", args.run_timeout, started)
        record_run("jobs-enabled-for-single-file", jobs, [
            {"id": "complete_exit_0", "ok": jobs["status"] == 0},
            {"id": "v2_json_report", "ok": Path(jobs["log"]).read_bytes().startswith(b"{")},
        ])

        valid = (json.dumps({"id": "valid", "path": str(plain)}) + "\n").encode()
        unsupported = (json.dumps({"id": "unsupported", "path": str(unknown)}) + "\n").encode()
        absent = (json.dumps({"id": "missing", "path": str(missing)}) + "\n").encode()
        after = (json.dumps({"id": "after", "path": str(plain)}) + "\n").encode()
        batch_run("batch-complete", "inspect", valid + after, 0, ["valid", "after"])
        batch_run("batch-partial", "inspect", valid + unsupported, 2, ["valid", "unsupported"])
        batch_run("batch-operational", "inspect", valid + absent + unsupported + after,
                  1, ["valid", "missing", "unsupported", "after"])
        _, invalid_responses = batch_run("batch-invalid-precedence", "inspect",
                                         valid + b"not-json\n" + absent + unsupported + after,
                                         64, ["valid", None, "missing", "unsupported", "after"])
        record_run("batch-invalid-record-contract", {"status": 0, "kind": "report_assertion"}, [
            {"id": "invalid_item_status", "ok": invalid_responses[1].get("status") == "invalid"},
            {"id": "valid_after_invalid_complete", "ok":
             invalid_responses[-1].get("report", {}).get("metadata", {}).get("status") == "complete"},
            {"id": "missing_still_failed", "ok":
             invalid_responses[2].get("report", {}).get("metadata", {}).get("status") == "failed"},
        ])

        artifact_pair("inspect-artifacts", "inspect", plain)
        artifact_pair("extract-artifacts", "extract", plain)

        real = sandbox / "linked-real"
        real.mkdir(mode=0o700)
        linked = sandbox / "linked-output"
        os.symlink(real, linked)
        symlink_execution, _, _ = report_run("symlink-output", ["inspect", str(plain), "--output-dir", str(linked)],
                                             1, "failed", "inspect")
        record_run("symlink-output-isolation", symlink_execution, [
            {"id": "no_files_through_link", "ok": list(real.iterdir()) == []},
        ])

        concurrent_dir = sandbox / "concurrent"
        concurrent_dir.mkdir(mode=0o700)
        children = []
        logs = []
        for index in (1, 2):
            log = run_dir / f"concurrent-{index}.log"
            stream = open(log, "xb")
            logs.append((log, stream))
            children.append(subprocess.Popen(
                [str(binary), "inspect", str(plain), "--output-dir", str(concurrent_dir)],
                stdout=stream, stderr=subprocess.STDOUT))
        concurrent_statuses = []
        try:
            for child in children:
                concurrent_statuses.append(child.wait(timeout=args.run_timeout))
        except subprocess.TimeoutExpired as error:
            for child in children:
                if child.poll() is None:
                    child.kill()
            raise DriverError("concurrent artifact writer timed out") from error
        finally:
            for _, stream in logs:
                stream.close()
        concurrent_files = list(concurrent_dir.iterdir())
        record_run("concurrent-writers", {"status": concurrent_statuses,
                                          "logs": [str(log) for log, _ in logs]}, [
            {"id": "one_success_one_collision", "ok": sorted(concurrent_statuses) == [0, 1]},
            {"id": "exactly_two_complete_files", "ok": len(concurrent_files) == 2 and all(
                path.is_file() and path.stat().st_size > 0 for path in concurrent_files)},
            {"id": "private_concurrent_files", "ok": all(
                (path.stat().st_mode & 0o777) == 0o600 for path in concurrent_files)},
        ])

        ordinary_execution, _, ordinary = report_run(
            "thumbnail-off", ["inspect", str(EXT / "fixtures/native.pdf")], 0, "complete", "inspect")
        record_run("thumbnail-off-contract", ordinary_execution, [
            {"id": "no_artifacts", "ok": (ordinary or {}).get("metadata", {}).get("artifacts", []) == []},
        ])
        thumbnail = sandbox / "thumbnail.png"
        thumbnail_execution, _, thumbnail_report = report_run(
            "thumbnail-on", ["inspect", str(EXT / "fixtures/native.pdf"), "--thumbnail", str(thumbnail),
                             "--thumbnail-max-edge", "64"], 0, "complete", "inspect")
        thumbnail_artifacts = (thumbnail_report or {}).get("metadata", {}).get("artifacts", [])
        record_run("thumbnail-on-contract", thumbnail_execution, [
            {"id": "one_explicit_thumbnail", "ok": len(thumbnail_artifacts) == 1 and
             thumbnail_artifacts[0].get("kind") == "thumbnail"},
            {"id": "bounded_dimensions", "ok": bool(thumbnail_artifacts) and
             thumbnail_artifacts[0].get("width", 65) <= 64 and
             thumbnail_artifacts[0].get("height", 65) <= 64},
            {"id": "thumbnail_bytes", "ok": thumbnail.is_file() and thumbnail.stat().st_size > 0},
        ])

        process = subprocess.Popen([str(binary), "inspect", str(plain)],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if process.stdout:
            process.stdout.close()
        _, broken_stderr = process.communicate(timeout=args.run_timeout)
        record_run("broken-stdout", {"status": process.returncode,
                                     "stderr_sha256": sha256_bytes(broken_stderr)}, [
            {"id": "operational_exit_1", "ok": process.returncode == 1},
            {"id": "io_diagnostic", "ok": bool(broken_stderr)},
        ])
    except (DriverError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        blockers.append({"stage": "status", "detail": str(error)})
    except Exception as error:
        blockers.append({"stage": "internal", "detail": f"{type(error).__name__}: {error}"})
    return finish_operation_result(result, run_dir, failures, blockers, evidence_dir, started)


# --------------------------------------------------------------------- batch

class OpenWatch:
    """Observe real file-open admission with Linux inotify, without dependencies.

    Accepts one private fixture directory. Returns timestamped OPEN events from
    nonblocking reads. The caller owns and closes the descriptor; unavailable
    inotify support raises DriverError instead of fabricating admission evidence.
    """

    HEADER = struct.Struct("iIII")

    def __init__(self, directory):
        self.libc = ctypes.CDLL(None, use_errno=True)
        self.fd = self.libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
        if self.fd < 0:
            error = ctypes.get_errno()
            raise DriverError(f"inotify_init1 failed: {os.strerror(error)}")
        self.wd = self.libc.inotify_add_watch(
            self.fd, os.fsencode(directory), 0x20)  # IN_OPEN
        if self.wd < 0:
            error = ctypes.get_errno()
            os.close(self.fd)
            self.fd = None
            raise DriverError(f"inotify_add_watch failed: {os.strerror(error)}")

    def events(self):
        """Drain currently available OPEN events and return filename/timestamp records."""
        records = []
        while True:
            try:
                data = os.read(self.fd, 65536)
            except BlockingIOError:
                break
            if not data:
                break
            position = 0
            while position + self.HEADER.size <= len(data):
                wd, mask, cookie, length = self.HEADER.unpack_from(data, position)
                position += self.HEADER.size
                raw_name = data[position:position + length].split(b"\0", 1)[0]
                position += length
                if wd == self.wd and mask & 0x20:
                    records.append({"name": os.fsdecode(raw_name),
                                    "observed_at": time.monotonic()})
        return records

    def close(self):
        """Close the owned watch descriptor once."""
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None


def live_thread_resources(pid):
    """Capture affinity and nice for every currently visible thread in one child."""
    threads = []
    task_root = Path(f"/proc/{pid}/task")
    for task in sorted(task_root.iterdir(), key=lambda path: int(path.name)):
        if not task.name.isdigit():
            continue
        tid = int(task.name)
        try:
            threads.append({"tid": tid, "affinity": sorted(os.sched_getaffinity(tid)),
                            "nice": os.getpriority(os.PRIO_PROCESS, tid)})
        except (FileNotFoundError, ProcessLookupError):
            continue
    valid = bool(threads) and all(
        thread["affinity"] == [0, 1, 2, 3] and thread["nice"] == 10
        for thread in threads)
    return {"pid": pid, "threads": threads, "valid": valid}


def process_group_absent(pgid):
    """Return whether a launched process group has no remaining members."""
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return True
    return False


def read_stream_jsonl(process, buffered, timeout):
    """Read one bounded nonblocking JSONL response from a live child.

    Accepts the process, a reusable byte buffer, and a timeout. Returns a decoded
    object while leaving later lines buffered. Timeout, EOF, malformed JSON, and
    responses above 16 MiB raise DriverError; stdin remains caller-controlled.
    """
    deadline = time.monotonic() + timeout
    while True:
        newline = buffered.find(b"\n")
        if newline >= 0:
            raw = bytes(buffered[:newline])
            del buffered[:newline + 1]
            try:
                return json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise DriverError(f"live child returned malformed JSONL: {error}") from error
        if len(buffered) > 16 * 1024 * 1024:
            raise DriverError("live child JSONL response exceeded 16 MiB")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise DriverError("timed out waiting for live child JSONL")
        selected = selectors.DefaultSelector()
        try:
            selected.register(process.stdout, selectors.EVENT_READ)
            if not selected.select(min(remaining, 1.0)):
                continue
            chunk = os.read(process.stdout.fileno(), 65536)
        finally:
            selected.close()
        if not chunk:
            raise DriverError("live child closed stdout before the expected response")
        buffered.extend(chunk)


def close_live_stdin(process):
    """Close a live child's stdin without communicate() flushing it again."""
    if process.stdin is not None:
        process.stdin.close()
        process.stdin = None


def run_batch_case(args, evidence_dir):
    """Run V-BATCH against the real release binary and live OS observations.

    Accepts parsed driver arguments and an authorized evidence root. Returns 0
    PASS, 1 FAIL, or 2 BLOCKED. Serial probes cover jobs/status matrices, ordered
    external IDs, line recovery, bounded admission, open-stdin flushing, broken
    stdout, live process-group SIGINT, resource limits, and worker cleanup.
    Both batch operations share one missing-path contract: a missing request
    yields metadata status failed and the operational aggregate exit 1, while
    genuinely unsupported fixtures keep their per-item unsupported status.
    The pinned-resource reproducibility guard is opt-in through
    --require-pinned-resources; by default observed thread affinity and nice
    values are recorded and the live probe checks worker count only.
    """
    started = time.monotonic()
    observed_start = datetime.now(timezone.utc).isoformat()
    run_dir = mkdir_new(evidence_dir / f"batch-{run_id()}")
    failures, blockers = [], []
    result = operation_result("batch", "V-BATCH", run_dir, observed_start)
    result["task"] = "B-JOBS"
    result["source_snapshot"] = jobs_source_snapshot()
    handoff = result["handoff"]
    handoff["identity"]["task_ids"] = ["B-JOBS"]
    handoff["state"].update({
        "objective": "Prove bounded ordered parallelism, cancellation, and deterministic streaming on real files.",
        "phase": "milestone-3 wave-9",
        "next_action": "Controller reviews V-BATCH evidence; B-PERF remains pending."})
    handoff["source"]["uncommitted_changes"] = (
        "Accepted waves through B-INVENTORY plus scoped B-JOBS")
    handoff["authority"].update({
        "approval_source": "Approved spec, milestones, and B-JOBS dispatch",
        "scope": "B-JOBS only; no mounts, NAS, installs, Git mutations, deletion, or publication"})
    handoff["evidence"]["missing_coverage"] = [
        "Native x64 and independent audit remain pending",
        "Rust instrumented tests supplement release observations for byte staging and peak counters"]
    result.pop("batch_jobs_coverage", None)

    def record_run(name, execution, checks):
        """Append one transparent run and fold every failed check into the outcome."""
        result["runs"].append({"name": name, "execution": execution, "checks": checks})
        for check in checks:
            if not check["ok"]:
                failures.append({"run": name, "check": check["id"],
                                 "detail": check.get("detail", "assertion failed")})

    def execute(name, argv, input_bytes=None):
        """Run one finite release JSONL child and strictly decode all responses."""
        execution = parity_execute([binary, *argv], run_dir / f"{name}.log",
                                   args.run_timeout, started, input_bytes=input_bytes)
        raw = Path(execution["log"]).read_bytes()
        return execution, raw, decode_jsonl(raw)

    try:
        enforce_resource_pin(result, args)
        result["environment"] = environment_snapshot()
        binary = operation_binary(args, run_dir, result)
        sandbox = SANDBOX_ROOT / f"{SANDBOX_PREFIX}{run_dir.name.removeprefix('batch-')}" / "batch"
        live = sandbox / "live"
        finite = sandbox / "finite"
        live.mkdir(parents=True, mode=0o700)
        finite.mkdir(mode=0o700)
        result["sandbox"] = str(sandbox)
        plain = finite / "plain.txt"
        second = finite / "second.txt"
        unknown = finite / "unknown.bin"
        missing = finite / "missing.txt"
        write_exclusive(plain, b"alpha beta gamma\n")
        write_exclusive(second, b"second valid document\n")
        write_exclusive(unknown, bytes((0, 1, 2, 255)))
        requests = [
            {"id": "alpha", "path": str(plain)},
            {"id": 17, "path": str(unknown)},
            {"id": "missing", "path": str(missing)},
            {"id": "omega", "path": str(second)},
        ]
        request_bytes = b"".join(
            (json.dumps(request, separators=(",", ":")) + "\n").encode()
            for request in requests)
        result["fixture_manifest"] = [
            {"path": str(path), "sha256": sha256_file(path), "bytes": path.stat().st_size}
            for path in (plain, second, unknown)]

        for value in ("0", "-1", "184467440737095516160"):
            execution = parity_execute(
                [binary, "inspect", str(plain), "--jobs", value],
                run_dir / f"invalid-jobs-{value}.log", args.run_timeout, started)
            record_run(f"invalid-jobs-{value}", execution, [
                {"id": "usage_exit_64", "ok": execution["status"] == 64,
                 "detail": f"observed {execution['status']}"}])

        expected_ids = ["alpha", 17, "missing", "omega"]
        # Both operations share one missing-path contract: a missing request
        # yields metadata status failed and the operational aggregate exit 1,
        # while the genuinely unsupported fixture keeps its per-item
        # unsupported status.
        for operation in ("inspect", "extract"):
            outputs = []
            for jobs in (1, 2, 4):
                argv = [operation, "--batch", "--jobs", str(jobs)]
                if operation == "extract":
                    argv.append("--no-ocr")
                execution, raw, responses = execute(
                    f"{operation}-jobs-{jobs}", argv, request_bytes)
                outputs.append(raw)
                record_run(f"{operation}-jobs-{jobs}", execution, [
                    {"id": "operation_severity", "ok": execution["status"] == 1},
                    {"id": "ordered_external_ids", "ok":
                     [response.get("id") for response in responses] == expected_ids},
                    {"id": "complete_unsupported_failed_complete_contract", "ok":
                     len(responses) == 4 and
                     responses[0].get("report", {}).get("metadata", {}).get("status") == "complete" and
                     responses[1].get("report", {}).get("metadata", {}).get("status") == "unsupported" and
                     responses[2].get("report", {}).get("metadata", {}).get("status") == "failed" and
                     responses[3].get("report", {}).get("metadata", {}).get("status") == "complete"},
                ])
            record_run(f"{operation}-jobs-determinism", {"status": 0, "kind": "comparison"}, [
                {"id": "byte_identical_jobs_1_2_4", "ok": outputs[0] == outputs[1] == outputs[2]}])

        directory_outputs = []
        for jobs in (1, 2, 4):
            execution, raw, responses = execute(
                f"directory-jobs-{jobs}", ["inspect", str(finite), "--jobs", str(jobs)])
            directory_outputs.append(raw)
            record_run(f"directory-jobs-{jobs}", execution, [
                {"id": "partial_exit_2", "ok": execution["status"] == 2},
                {"id": "sorted_paths", "ok": [response.get("path") for response in responses] ==
                 sorted([response.get("path") for response in responses], key=os.fsencode)},
                {"id": "three_directory_records", "ok": len(responses) == 3},
            ])
        record_run("directory-determinism", {"status": 0, "kind": "comparison"}, [
            {"id": "byte_identical_jobs_1_2_4", "ok":
             directory_outputs[0] == directory_outputs[1] == directory_outputs[2]}])

        malformed = b'{not-json}\n'
        oversized = b'{"id":"' + (b"x" * 70000) + b'","path":"x"}\n'
        recovery_input = malformed + request_bytes.splitlines(keepends=True)[0] + oversized + \
            request_bytes.splitlines(keepends=True)[3]
        execution, _, responses = execute(
            "line-recovery", ["inspect", "--batch", "--jobs", "4"], recovery_input)
        record_run("line-recovery", execution, [
            {"id": "invalid_dominates_with_64", "ok": execution["status"] == 64},
            {"id": "four_ordered_responses", "ok": len(responses) == 4},
            {"id": "malformed_then_oversized_errors", "ok":
             bool(responses[0].get("error")) and bool(responses[2].get("error"))},
            {"id": "valid_records_continue", "ok":
             responses[1].get("id") == "alpha" and responses[3].get("id") == "omega" and
             all(response.get("report", {}).get("metadata", {}).get("status") == "complete"
                 for response in (responses[1], responses[3]))},
            {"id": "oversized_payload_not_reflected", "ok": len(json.dumps(responses[2])) < 4096},
        ])

        scan_source = sandbox / "scan-source.pdf"
        write_exclusive(scan_source, build_pdf([{"text": OCR_TEXT, "font_size": 20}]))
        raster = rasterize_with_pdftoppm(scan_source, sandbox / "scan-raster", dpi=72)
        width, height, rgb = decode_png_rgb(raster)
        slow = live / "slow.pdf"
        write_exclusive(slow, build_pdf([
            {"image": {"width": width, "height": height, "rgb": rgb}}
            for _ in range(10)
        ]))
        fast = []
        for index in range(1, 5):
            path = live / f"fast-{index}.txt"
            write_exclusive(path, f"fast document {index}\n".encode())
            fast.append(path)
        live_paths = [slow, *fast]
        live_requests = b"".join(
            (json.dumps({"id": index, "path": str(path)}, separators=(",", ":")) + "\n").encode()
            for index, path in enumerate(live_paths))
        result["slow_fixture"] = {"path": str(slow), "pages": 10,
                                  "bytes": slow.stat().st_size, "sha256": sha256_file(slow)}

        watch = OpenWatch(live)
        process = None
        pgid = None
        buffered = bytearray()
        pre_open_names = []
        try:
            process = subprocess.Popen(
                [str(binary), "extract", "--batch", "--jobs", "4"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                start_new_session=True)
            pgid = os.getpgid(process.pid)
            process.stdin.write(live_requests)
            process.stdin.flush()
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                pre_open_names = [event["name"] for event in watch.events()]
                if len(set(pre_open_names)) >= 4:
                    break
                time.sleep(0.02)
            still_slow = process.poll() is None
            resources = live_thread_resources(process.pid)
            responses = [read_stream_jsonl(process, buffered, args.run_timeout)]
            first_response_before_eof = responses[0]
            while len(responses) < len(live_paths):
                responses.append(read_stream_jsonl(process, buffered, args.run_timeout))
            flushed_with_open_stdin = process.poll() is None
            post_open_names = [event["name"] for event in watch.events()]
            close_live_stdin(process)
            stderr = process.stderr.read()
            status = process.wait(timeout=10)
            execution = {"status": status, "pid": process.pid, "pgid": pgid,
                         "resources": resources, "pre_completion_opens": pre_open_names,
                         "post_completion_opens": post_open_names,
                         "stderr_sha256": sha256_bytes(stderr),
                         "process_group_absent": process_group_absent(pgid)}
            record_run("live-backpressure-flush", execution, [
                {"id": "four_worker_threads", "ok": len(resources["threads"]) >= 4},
                {"id": "four_cpu_nice_10_threads",
                 "ok": resources["valid"] if result["resource_pin_required"] else True,
                 "detail": None if result["resource_pin_required"] else
                           "affinity/nice pin recorded as observed; enforced only "
                           "through --require-pinned-resources"},
                {"id": "only_first_four_admitted", "ok": still_slow and
                 set(pre_open_names) == {path.name for path in live_paths[:4]} and
                 fast[3].name not in pre_open_names,
                 "detail": f"opens={pre_open_names}"},
                {"id": "ordered_first_response", "ok": first_response_before_eof.get("id") == 0},
                {"id": "all_responses_before_stdin_eof", "ok": flushed_with_open_stdin and
                 [response.get("id") for response in responses] == list(range(5))},
                {"id": "fifth_admitted_after_credit", "ok": fast[3].name in post_open_names},
                {"id": "complete_exit_0", "ok": status == 0},
                {"id": "no_remaining_process_group", "ok": execution["process_group_absent"]},
            ])
            result["live_stream_responses"] = responses
        finally:
            watch.close()
            if process is not None and process.poll() is None:
                if pgid is not None:
                    os.killpg(pgid, signal.SIGKILL)
                else:
                    process.kill()
                process.wait(timeout=10)
            if process is not None:
                close_live_stdin(process)
                if process.stdout is not None:
                    process.stdout.close()
                if process.stderr is not None:
                    process.stderr.close()

        signal_watch = OpenWatch(live)
        process = subprocess.Popen(
            [str(binary), "extract", "--batch", "--jobs", "4"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True)
        pgid = os.getpgid(process.pid)
        try:
            process.stdin.write(live_requests)
            process.stdin.flush()
            signal_deadline = time.monotonic() + 10
            while time.monotonic() < signal_deadline:
                if any(event["name"] == slow.name for event in signal_watch.events()):
                    break
                if process.poll() is not None:
                    raise DriverError("SIGINT child exited before the slow input opened")
                time.sleep(0.005)
            else:
                raise DriverError("SIGINT child did not open the slow input within 10s")
            signal_started = time.monotonic()
            os.killpg(pgid, signal.SIGINT)
            close_live_stdin(process)
            stdout, stderr = process.communicate(timeout=30)
            signal_duration = time.monotonic() - signal_started
            write_exclusive(run_dir / "sigint-stdout.log", stdout)
            write_exclusive(run_dir / "sigint-stderr.log", stderr)
            responses = decode_jsonl(stdout) if stdout.strip() else []
            execution = {"status": process.returncode, "pid": process.pid, "pgid": pgid,
                         "duration_s": round(signal_duration, 4),
                         "process_group_absent": process_group_absent(pgid),
                         "stdout_sha256": sha256_bytes(stdout),
                         "stderr_sha256": sha256_bytes(stderr)}
            record_run("live-process-group-sigint", execution, [
                {"id": "operational_cancelled_exit_1", "ok": process.returncode == 1},
                {"id": "bounded_shutdown", "ok": signal_duration < 30},
                {"id": "cancelled_record", "ok": any(
                    response.get("cancelled") is True for response in responses)},
                {"id": "no_remaining_process_group", "ok": execution["process_group_absent"]},
            ])
        finally:
            if process.poll() is None:
                os.killpg(pgid, signal.SIGKILL)
                process.wait(timeout=10)
            close_live_stdin(process)
            process.stdout.close()
            process.stderr.close()
            signal_watch.close()

        full_output = open("/dev/full", "wb")
        process = subprocess.Popen(
            [str(binary), "extract", "--batch", "--jobs", "4"],
            stdin=subprocess.PIPE, stdout=full_output, stderr=subprocess.PIPE,
            start_new_session=True)
        pgid = os.getpgid(process.pid)
        try:
            process.stdin.write(request_bytes.splitlines(keepends=True)[0])
            process.stdin.flush()
            status = process.wait(timeout=20)
            close_live_stdin(process)
            stderr = process.stderr.read()
            write_exclusive(run_dir / "broken-stdout-stderr.log", stderr)
            execution = {"status": status, "pid": process.pid, "pgid": pgid,
                         "stdin_open_during_wait": True,
                         "process_group_absent": process_group_absent(pgid),
                         "stderr_sha256": sha256_bytes(stderr)}
            record_run("broken-stdout-open-stdin", execution, [
                {"id": "operational_exit_1", "ok": status == 1},
                {"id": "io_diagnostic", "ok": bool(stderr)},
                {"id": "no_remaining_process_group", "ok": execution["process_group_absent"]},
            ])
        finally:
            if process.poll() is None:
                os.killpg(pgid, signal.SIGKILL)
                process.wait(timeout=10)
            close_live_stdin(process)
            full_output.close()
            process.stderr.close()
    except (DriverError, OSError, subprocess.SubprocessError, json.JSONDecodeError) as error:
        blockers.append({"stage": "batch", "detail": str(error)})
    except Exception as error:
        blockers.append({"stage": "internal", "detail": f"{type(error).__name__}: {error}"})
    return finish_operation_result(result, run_dir, failures, blockers, evidence_dir, started)

# ------------------------------------------------------------- performance

PERF_ACTIVE_BUDGET_S = PARITY_ACTIVE_BUDGET_S
PERF_MEASUREMENT_RESERVE_S = 300
PERF_SCENARIO_CAP_S = 240
PERF_MIN_REPLICATES = 5
PERF_JOBS_VALUES = (1, 2, 4)
PERF_HISTORICAL_REVISION = "57e3afb7f1d59ca93811eda5748aa917613f64fc"
PERF_HISTORICAL_BUILD_TIMEOUT_S = 1200
# The reused read-only P-OCR fixtures are an explicit external dataset
# prerequisite: the scanned PDFs accepted by the spike-3 final results are
# retained at their original evidence location under the ignored sandbox
# tree (renaming real retained evidence is not permitted). Their safe,
# non-private expectations (synthetic Apache-2.0 probe text, per-page
# phrase expectations, construction provenance) are tracked portably in
# the oracle manifest below, so performance no longer depends on an
# unexplained ignored oracle file. A missing or drifted external dataset
# fails distinctly and is never fabricated or faked.
PERF_POCR_SANDBOX = SANDBOX_ROOT / "settling-in-test-20261122T013932Z-972916-85cb2f/ocr"
PERF_POCR_DATASET_DIR = PERF_POCR_SANDBOX / "fixtures"
PERF_POCR_ORACLE_MANIFEST = EXT / "fixtures" / "pocr-reuse-oracle.json"
PERF_POCR_ORACLE_SCHEMA = "glassbox-pocr-reuse-oracle/1"
# Frozen SHA-256 values transcribed from the accepted spike-3 final results
# record (spikes/spike-3-ocr-performance-results.md). The driver refuses to
# use any reused read-only fixture whose digest differs from these values,
# and the tracked oracle manifest above must agree with every pinned digest.
PERF_POCR_FROZEN_SHA = {
    "scanned-4p.pdf": "af937311d99d05b472be51caf995b1fd75b0f797de728564749783a76008b22e",
    "dense-large-small-text.pdf": "ed7a63d4f0a96f848f789e736e329727ac812070e6cbb39e383141d40c2ceef0",
    "mixed-same-page.pdf": "eda8c7d0a14fd9eeb191d2becd051a683c00fc7f7060017b93134ff555d5e777",
}
PERF_CLEAN_BUILD_REFERENCE = (
    VERIFICATION_ROOT / "settling-in-controller-bjobs-metadata" /
    "metadata-20261122T151639Z-2674260-a303dd42/result.json")
# SHA-256 of the production binary accepted by the controller's B-JOBS
# revalidation evidence (extract 279 checks + inventory 96 checks).
PERF_ACCEPTED_PRODUCTION_SHA = ("c72c9928550cf28ae14f7a349ad1975788be"
                                "86842cf9b44c3f0852bdb6af97c8")
# Fixed factor-only release-profile candidates. Values are passed as cargo
# --config arguments; the canonical Cargo.toml stays untouched and no
# candidate changes panic behavior (panic=abort is never introduced).
PERF_CANDIDATE_FACTORS = {
    "thin-lto": ('profile.release.lto="thin"',),
    "thin-lto-cgu1": ('profile.release.lto="thin"',
                      "profile.release.codegen-units=1"),
}
PERF_DEFAULT_CANDIDATES = ("thin-lto", "thin-lto-cgu1")
# Evaluator contract version 2: the measured wall clock changed from a
# 20 ms WNOHANG polling loop (v1, quantized and overshooting short runs) to
# a blocking wait4 with an independent sampler/timeout thread. All v1
# latency claims from the retained final-01..04 attempts are invalidated;
# classification is recomputed only from v2 measurements.
PERF_EVALUATOR_VERSION = 2
# Wall-clock resolution floor: gains or regressions at or below this margin
# are never classified as beyond noise regardless of observed spread.
PERF_TIME_RESOLUTION_FLOOR_S = 0.005
# Tolerance for the independent blocking subprocess clock crosscheck.
PERF_CLOCK_CROSSCHECK_TOLERANCE_S = 0.05

# --------------------------------------------------------------------
# V-PERF delivery-validation mode (C-TEST final gate). The controller
# adopted the canonical release profile in extensionalities/Cargo.toml
# (lto = "thin", codegen-units = 1; no panic change). This mode rebuilds
# the shipped production native target once with that canonical profile,
# proves the accepted-evidence chain (controller-accepted corrective-07
# result, unchanged Rust sources/deps except the profile-only delta, and
# the accepted thin+CGU1 factor tuple), remeasures the delivered binary
# on the frozen scenario matrix with the v2 blocking clock, and compares
# byte outputs with the approved candidate binary on identical inputs.
# No candidate matrix, no historical rebuild, and no counterfactual
# profile is constructed in this mode.
PERF_DELIVERY_MODE_VERSION = 1
PERF_ACCEPTED_CORRECTIVE_RESULT = (
    VERIFICATION_ROOT / "settling-in-bperf-corrective-07" /
    "performance-20261122T170902Z-2955654-729295e6" / "result.json")
PERF_APPROVED_CANDIDATE_NAME = "thin-lto-cgu1"
PERF_APPROVED_CANDIDATE_BINARY_SHA = ("71e5f0e75c35fe1d724b8851f582b363dfd8c6b"
                                      "664379bc61fbd20aded8f6533")
PERF_APPROVED_CANDIDATE_TARGET = (
    BUILD_ROOT / "settling-in-build-20261122T160941Z-2789210-e7450310-thin-lto-cgu1")
# Every retained performance attempt (four final + three corrective) must be
# supplied to the next attempt ledger; blocked corrective-05/06 costs are
# never silently dropped.
PERF_DELIVERY_REQUIRED_PRIOR_ATTEMPTS = 7
# The canonical profile adopted by the controller; the control factor tuple
# accepted in the corrective-07 evidence.
PERF_CANONICAL_RELEASE_PROFILE = {"lto": "thin", "codegen-units": 1}
PERF_PROFILE_ONLY_DELTA_FILE = "extensionalities/Cargo.toml"
PERF_PROFILE_ONLY_DELTA_TEXT = (
    '\n[profile.release]\nlto = "thin"\ncodegen-units = 1\n')
# One rebuild of the shipped production target is authorized; a fresh build
# measured 353.07s for the same factors, so the bound allows ~12 minutes.
PERF_DELIVERY_REBUILD_TIMEOUT_S = 720
# Rust sources and dependency manifests that must be unchanged relative to
# the accepted corrective-07 evidence. Scripts are excluded because they are
# not compiled into the delivered binary and are this task's authorized
# automation edits; their current state is recorded separately.
PERF_DELIVERY_SOURCE_PREFIXES = ("crates/", "extensionalities/crates/")
PERF_DELIVERY_SOURCE_FILES = ("Cargo.toml", "Cargo.lock",
                              "extensionalities/Cargo.toml",
                              "extensionalities/Cargo.lock")


def perf_parse_candidates(raw):
    """Validate the bounded explicit candidate list from the command line.

    Accepts one comma-separated string of candidate names. Returns the ordered
    tuple of selected candidate names. Unknown names, duplicates, empty lists,
    and more than the two permitted candidates raise DriverError exit 2.
    """
    names = [part.strip() for part in raw.split(",") if part.strip()]
    if not names:
        raise DriverError("--perf-candidates requires at least one of: "
                          + ", ".join(PERF_CANDIDATE_FACTORS))
    if len(names) > len(PERF_DEFAULT_CANDIDATES):
        raise DriverError(f"at most {len(PERF_DEFAULT_CANDIDATES)} profile "
                          f"candidates are permitted, got {len(names)}")
    for name in names:
        if name not in PERF_CANDIDATE_FACTORS:
            raise DriverError(f"unknown profile candidate {name!r}; permitted: "
                              + ", ".join(PERF_CANDIDATE_FACTORS))
    if len(set(names)) != len(names):
        raise DriverError("duplicate profile candidates are not permitted")
    return tuple(names)


def perf_parse_reuse_candidates(raw_pairs):
    """Validate explicit reuse of previously built candidate targets.

    Accepts the optional list of NAME=TARGET_DIR strings from the command
    line. Returns a mapping of candidate names to absolute prior target
    directories whose release/glassbox binary exists. Malformed entries,
    unknown candidate names, relative directories, and missing binaries
    raise DriverError so a silent wrong-binary reuse is impossible.
    """
    reuse = {}
    if not raw_pairs:
        return reuse
    for entry in raw_pairs:
        name, separator, target = entry.partition("=")
        name = name.strip()
        if not separator or name not in PERF_CANDIDATE_FACTORS:
            raise DriverError(
                f"--reuse-candidate expects one of "
                f"{', '.join(PERF_CANDIDATE_FACTORS)}=ABSOLUTE_TARGET_DIR, "
                f"got {entry!r}")
        path = Path(target.strip())
        if not path.is_absolute():
            raise DriverError(f"reuse target must be absolute: {target!r}")
        binary = path / "release/glassbox"
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise DriverError(f"reuse candidate binary is missing or not "
                              f"runnable: {binary}")
        reuse[name] = path
    return reuse


def perf_attempt_history(paths):
    """Read retained prior attempt evidence for cumulative cost accounting.

    Accepts a list of absolute prior result.json paths. Returns one record
    per attempt with its case, verification status, active seconds,
    separately charged historical seconds, and total wall seconds, plus
    exact sums. Missing files, unparsable JSON, or absent cost fields raise
    DriverError; failed attempt costs are never reset or silently dropped.
    """
    attempts = []
    for raw in paths or ():
        path = Path(raw)
        if not path.is_absolute():
            raise DriverError(f"prior attempt path must be absolute: {raw!r}")
        if not path.is_file():
            raise DriverError(f"prior attempt evidence is missing: {path}")
        payload = json.loads(path.read_text(encoding="utf-8"))
        costs = payload.get("costs", {})
        wall = costs.get("total_wall_s")
        if wall is None:
            raise DriverError(f"prior attempt {path} records no total wall "
                              "cost; its cost cannot be accounted")
        historical = payload.get("historical_stage") or {}
        attempts.append({
            "path": str(path),
            "case": payload.get("case"),
            "status": payload.get("status"),
            "evaluator_version": (payload.get("experiment_spec") and 1) or
                                 payload.get("evaluator_version", 1),
            "active_s": costs.get("elapsed_active_s"),
            "historical_separately_s":
                costs.get("historical_charged_separately_s"),
            "total_wall_s": wall,
            "historical_files_copied":
                (historical.get("source_copy") or {}).get("files_copied"),
            "historical_binary_sha256":
                (historical.get("binary") or {}).get("sha256"),
            "historical_revision":
                (historical.get("source_copy") or {}).get("revision"),
            "historical_upstream_drift":
                (historical.get("source_copy") or {}).get("upstream_drift"),
        })
    return {
        "attempts": attempts,
        "count": len(attempts),
        "known_total_wall_s": round(sum(item["total_wall_s"]
                                        for item in attempts), 4),
        "note": "known wall cost of retained attempts only; unmeasured "
                "prelude engineering, preparation, and setup elapsed before "
                "each attempt remains unknown and is never claimed as "
                "compliant",
    }


def perf_budget_accounting(history, corrective_budget_s, this_wall_s):
    """Compute honest cumulative budget compliance across all attempts.

    Accepts the accumulated attempt history, the optional corrective-pass
    budget in seconds, and this run's total wall seconds. Returns the
    original 30-minute campaign budget status computed from all known
    attempt walls (never reset after evaluator fixes) and the explicit
    corrective-pass status. The original campaign budget claim is computed
    only from known walls plus the explicit unknown-preparation caveat.
    """
    known = history["known_total_wall_s"] + this_wall_s
    original_excess = max(0.0, round(known - PERF_ACTIVE_BUDGET_S, 4))
    accounting = {
        "original_campaign_budget_s": PERF_ACTIVE_BUDGET_S,
        "known_total_wall_all_attempts_s": round(known, 4),
        "original_campaign_budget_exceeded_by_s": original_excess,
        "original_campaign_compliant": original_excess <= 0,
        "original_campaign_note": "the original 30-minute campaign budget "
            f"was exceeded by at least {original_excess}s of known wall "
            "time across all retained attempts plus unmeasured "
            "preparation; per-run resets or separately charged historical "
            "reconstructions never make the original campaign compliant",
        "unmeasured_preparation": "unknown",
        "corrective_pass": None,
    }
    if corrective_budget_s is not None:
        accounting["corrective_pass"] = {
            "budget_s": corrective_budget_s,
            "known_wall_s": round(this_wall_s, 4),
            "within_budget": this_wall_s <= corrective_budget_s,
            "note": "within_budget refers only to this explicitly approved "
                    "corrective pass, never to the original campaign",
        }
    return accounting


def perf_release_profile_record():
    """Read the effective release profile from the live workspace manifest.

    Accepts no parameters and parses extensionalities/Cargo.toml with the
    standard library. Returns the declared [profile.release] table plus the
    manifest's profile sha256. Raises DriverError when the manifest cannot be
    read or parsed; an absent profile section returns an empty table and is
    recorded, never silently defaulted.
    """
    manifest_path = EXT / "Cargo.toml"
    try:
        document = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise DriverError(f"cannot read the downstream release profile: {error}") from error
    profile = document.get("profile", {}).get("release", {})
    return {
        "manifest": str(manifest_path),
        "manifest_sha256": sha256_file(manifest_path),
        "profile.release": profile,
    }


def perf_delivery_source_delta(prior_manifest):
    """Validate the delivery source scope against accepted prior evidence.

    Accepts the accepted corrective run's source-manifest.json payload (a
    list of {path, sha256, size} records). Returns a record with per-file
    comparison for the Rust sources and dependency manifests: every file
    must be byte-identical to the accepted evidence except
    extensionalities/Cargo.toml, whose only permitted change is the exact
    canonical profile section. Raises DriverError when files outside the
    delivery scope changed or the Cargo.toml delta is not profile-only.
    """
    prior = {entry["path"]: entry["sha256"] for entry in prior_manifest}
    changed = []
    missing = []
    for relative, want in sorted(prior.items()):
        if not (relative.startswith(PERF_DELIVERY_SOURCE_PREFIXES) or
                relative in PERF_DELIVERY_SOURCE_FILES):
            continue
        current = REPO / relative
        if not current.is_file():
            missing.append(relative)
            continue
        if sha256_file(current) != want:
            changed.append(relative)
    if missing:
        raise DriverError("delivery-scope files from the accepted evidence "
                          "are missing: " + ", ".join(missing[:5]))
    unexpected = [name for name in changed
                  if name != PERF_PROFILE_ONLY_DELTA_FILE]
    if unexpected:
        raise DriverError(
            "Rust sources or dependency files changed relative to the "
            "accepted corrective evidence: " + ", ".join(unexpected[:5]))
    profile_delta_ok = False
    detail = None
    if PERF_PROFILE_ONLY_DELTA_FILE in prior:
        prior_toml_sha = prior[PERF_PROFILE_ONLY_DELTA_FILE]
        current_text = (REPO / PERF_PROFILE_ONLY_DELTA_FILE).read_text(encoding="utf-8")
        expected_current = None
        # The prior manifest stores only a digest, so the delta is validated
        # structurally: the current manifest must contain exactly the
        # canonical profile keys and nothing else beyond the prior table.
        record = perf_release_profile_record()
        profile = record["profile.release"]
        profile_delta_ok = (
            profile == PERF_CANONICAL_RELEASE_PROFILE and
            current_text.endswith(PERF_PROFILE_ONLY_DELTA_TEXT))
        detail = {
            "prior_cargo_toml_sha256": prior_toml_sha,
            "current_cargo_toml_sha256": record["manifest_sha256"],
            "current_profile": profile,
            "expected_profile": PERF_CANONICAL_RELEASE_PROFILE,
        }
    else:
        detail = {"note": "prior manifest lacks the downstream Cargo.toml"}
    if not profile_delta_ok:
        raise DriverError(
            "the downstream Cargo.toml delta is not the exact canonical "
            "profile-only change: " + json.dumps(detail))
    return {
        "files_compared": sum(1 for name in prior
                              if name.startswith(PERF_DELIVERY_SOURCE_PREFIXES)
                              or name in PERF_DELIVERY_SOURCE_FILES),
        "changed": [PERF_PROFILE_ONLY_DELTA_FILE],
        "profile_only_delta": detail,
        "scope": "Rust sources, crate manifests, and dependency lockfiles; "
                 "scripts are excluded because they are not compiled into "
                 "the delivered binary",
    }


def perf_parse_reuse_historical(raw):
    """Validate an explicit reused historical target directory.

    Accepts one TARGET_DIR string from the command line. Returns the
    absolute Path whose release/glassbox binary exists and is executable.
    Relative paths and missing binaries raise DriverError; the binary's
    provenance is validated in-case against the retained prior attempt
    records, so an unrecorded historical binary can never be reused.
    """
    path = Path(raw.strip())
    if not path.is_absolute():
        raise DriverError(f"--reuse-historical expects an absolute target "
                          f"directory, got {raw!r}")
    binary = path / "release/glassbox"
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise DriverError(f"reused historical binary is missing or not "
                          f"runnable: {binary}")
    return path


def perf_clean_build_reference_record():
    """Read the retained controller clean-build measurement when present.

    Accepts no parameters and reads the recorded B-JOBS metadata evidence.
    Returns a record with the default-profile clean build duration, binary
    identity, and the documented missing build-RSS limitation, or an explicit
    unavailable record when the reference file is absent. Nothing is invented.
    """
    path = PERF_CLEAN_BUILD_REFERENCE
    if not path.is_file():
        return {"available": False, "path": str(path),
                "reason": "retained controller clean-build evidence is absent"}
    payload = json.loads(path.read_text(encoding="utf-8"))
    build = payload.get("build", {})
    binary = payload.get("binary", {})
    duration = build.get("duration_s")
    return {
        "available": isinstance(duration, (int, float)),
        "path": str(path),
        "clean_build_duration_s": duration,
        "build_status": build.get("status"),
        "clean_binary_sha256": binary.get("sha256"),
        "clean_binary_bytes": binary.get("bytes"),
        "build_peak_rss_kb": None,
        "build_rss_note": "clean-build peak RSS was not measured by the "
                          "controller run; only duration and binary identity "
                          "are reused",
        "assertion_source": "controller B-JOBS dispatch: same current source, "
                            "correct isolated build HOME, all upstream hashes "
                            "stable",
    }


def perf_sample_child(pid, stop, state, deadline):
    """Sample one measured child's /proc counters and enforce its deadline.

    Accepts the child process id, a threading.Event stop signal, a shared
    mutable state dict, and the monotonic kill deadline. Returns unit after
    recording the last cumulative read_bytes and the maximum observed VmHWM
    every 20 ms; on deadline the child is SIGKILLed and state["timed_out"]
    is set before the kill so the blocking waiter records the timeout.
    Sampling resolution affects only the RSS/io trace, never the wall clock.
    """
    while not stop.is_set():
        try:
            with open(f"/proc/{pid}/io", "rb") as stream:
                match = re.search(rb"read_bytes:\s+(\d+)", stream.read())
                if match:
                    state["io_read_bytes"] = int(match.group(1))
        except OSError:
            pass
        try:
            with open(f"/proc/{pid}/status", "rb") as stream:
                match = re.search(rb"VmHWM:\s+(\d+) kB", stream.read())
                if match:
                    state["vmhwm_kb"] = max(state["vmhwm_kb"],
                                            int(match.group(1)))
        except OSError:
            pass
        if time.monotonic() >= deadline:
            state["timed_out"] = True
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
            return
        time.sleep(0.02)


def perf_run_measured(argv, stdout_path, stderr_path, timeout_s, input_bytes=None):
    """Run one fresh measured child with a blocking wait and exact rusage.

    Accepts an argument vector, new stdout/stderr paths, a bounded timeout in
    seconds, and optional bounded stdin bytes. Returns the child's exit
    status, wall/CPU seconds, exact wait4 peak RSS, sampled VmHWM and
    /proc/<pid>/io read_bytes, and output paths. The main thread blocks in
    wait4 until the child truly exits, so the wall clock is not quantized by
    polling; an independent sampler thread records the RSS/io trace and
    enforces the deadline with SIGKILL, recording timed_out. Timeout kills
    the child instead of inventing a status. The direct child covers its
    threads; subprocess-spawning routes (LibreOffice conversion) are not
    exercised by any measured scenario. Short children may exit before the
    first sample; the RSS trace is then unobserved, which is not a zero-
    memory claim.
    """
    command = [str(item) for item in argv]
    started = time.monotonic()
    state = {"io_read_bytes": 0, "vmhwm_kb": 0, "timed_out": False}
    stop = threading.Event()
    with open(stdout_path, "wb") as out, open(stderr_path, "wb") as err:
        try:
            process = subprocess.Popen(
                command, stdin=subprocess.PIPE if input_bytes is not None
                else subprocess.DEVNULL, stdout=out, stderr=err)
        except OSError as error:
            raise DriverError(f"cannot launch measured child {command[0]}: "
                              f"{error}") from error
        if input_bytes is not None:
            process.stdin.write(input_bytes)
            process.stdin.close()
        pid = process.pid
        launched = launched_process_resources(pid)
        sampler = threading.Thread(
            target=perf_sample_child,
            args=(pid, stop, state, started + max(1.0, float(timeout_s))),
            daemon=True)
        sampler.start()
        done, status, usage = os.wait4(pid, 0)
        wall = time.monotonic() - started
        stop.set()
        sampler.join(timeout=1)
    process.returncode = os.waitstatus_to_exitcode(status)
    return {"argv": command, "status": process.returncode,
            "timed_out": state["timed_out"],
            "wall_s": round(wall, 4),
            "cpu_s": round(usage.ru_utime + usage.ru_stime, 4),
            "peak_rss_kb": usage.ru_maxrss, "vmhwm_kb": state["vmhwm_kb"],
            "io_read_bytes": state["io_read_bytes"],
            "rss_trace_observed": state["vmhwm_kb"] > 0,
            "launched_process": launched,
            "stdout": str(stdout_path), "stderr": str(stderr_path)}


def perf_sample_tree_rss(pid, stop, peak):
    """Sample the summed resident set of one build process tree.

    Accepts the cargo process id, a threading.Event stop signal, and a
    one-element peak holder in KiB. Returns unit after sampling /proc for
    cargo and all descendants every 250 ms until stopped. Shared pages are
    double counted across processes; the value is recorded as an upper-bound
    sampled trace, never as an exact allocator accounting.
    """
    page_kib = os.sysconf("SC_PAGE_SIZE") // 1024
    while not stop.is_set():
        total = 0
        parents = {}
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            try:
                with open(f"/proc/{entry}/stat", "rb") as stream:
                    fields = stream.read().rsplit(b")", 1)
                parents[int(entry)] = int(fields[1].split()[1])
            except (OSError, IndexError, ValueError):
                continue
        members = set()
        for entry, parent in parents.items():
            walker = entry
            while walker > 1:
                if walker == pid:
                    members.add(entry)
                    break
                walker = parents.get(walker, 0)
        for member in members:
            try:
                with open(f"/proc/{member}/statm", "rb") as stream:
                    total += int(stream.read().split()[1]) * page_kib
            except (OSError, IndexError, ValueError):
                continue
        if total > peak[0]:
            peak[0] = total
        time.sleep(0.25)


def perf_cargo_build(manifest, run_dir, label, timeout_s, config_values=(),
                     build_target=None):
    """Build one isolated release target with fixed factors and measured cost.

    Accepts a manifest path, exclusive run directory, unique label, bounded
    timeout, optional cargo --config factor values, and an optional explicit
    build target directory. Without an explicit target a new isolated tree
    under extensionalities/target/settling-in-build-<run-id>-<label> is
    created; the explicit target is used only for the single permitted
    incremental build against the cached production target. The build reuses
    the shared isolated build HOME and download caches, probes offline
    metadata first, and samples the cargo process tree's summed RSS.
    """
    if build_target is None:
        build_target = BUILD_ROOT / f"{BUILD_PREFIX}{run_id()}-{label}"
    env, env_record = build_environment(build_target)
    offline_probe = subprocess.run(
        ["cargo", "metadata", "--locked", "--offline",
         "--manifest-path", str(manifest), "--format-version", "1"],
        cwd=str(REPO), capture_output=True, timeout=120)
    argv = ["cargo", "build", "--locked", "--release",
            "--manifest-path", str(manifest), "--workspace", "--bins"]
    for value in config_values:
        argv.extend(["--config", value])
    if offline_probe.returncode == 0:
        argv.append("--offline")
    build_log = run_dir / f"cargo-build-{label}.log"
    peak = [0]
    stop = threading.Event()
    watcher = None
    started = time.monotonic()
    timed_out = False
    try:
        process = launch_session_process(argv, cwd=str(REPO), env=env)
        watcher = threading.Thread(
            target=perf_sample_tree_rss, args=(process.pid, stop, peak),
            daemon=True)
        watcher.start()
        output, _ = process.communicate(timeout=timeout_s)
        status = process.returncode
    except subprocess.TimeoutExpired as error:
        timed_out = True
        stop_session_process_group(process)
        output = ((getattr(error, "stdout", None) or b"")
                  + (process.communicate()[0] or b""))
        status = process.returncode
        output = (output or b"") + b"\nBUILD TIMEOUT\n"
    finally:
        stop.set()
        if watcher is not None:
            watcher.join(timeout=1)
    duration = time.monotonic() - started
    write_exclusive(build_log,
                    f"# argv: {' '.join(str(item) for item in argv)}\n"
                    f"# exit: {status}\n# timed_out: {timed_out}\n"
                    f"# timeout_s: {timeout_s}\n"
                    f"# duration_s: {duration:.2f}\n"
                    f"# config_values: {list(config_values)}\n"
                    f"# stdout/stderr:\n"
                    f"{output.decode(errors='replace')}\n".encode())
    return {"label": label, "argv": [str(item) for item in argv],
            "config_values": list(config_values),
            "build_target_dir": str(build_target),
            "build_environment": env_record,
            "offline_probe_status": offline_probe.returncode,
            "status": status, "duration_s": round(duration, 2),
            "timed_out": timed_out,
            "peak_tree_rss_kib_sampled": peak[0],
            "log": str(build_log)}, build_target, status


def perf_normalized(text):
    """Collapse whitespace for containment checks against constructed text.

    Accepts one extracted text value. Returns the whitespace-normalized form
    used only for phrase-containment oracle checks, never for byte
    comparisons between profile variants.
    """
    return " ".join(text.split())


def perf_inspect_checks(report, record, expected_status):
    """Check one inspect report against the frozen fixture manifest oracle.

    Accepts a decoded v2 report, a fixture record with the independently
    Python-computed SHA-256 and byte size, and the expected operation status.
    Returns envelope, digest, byte-count, qualification, status, and
    body-absence checks. Every expected value is frozen before runs.
    """
    metadata = report.get("metadata", {}) if isinstance(report, dict) else {}
    digest = metadata.get("digest", {})
    return [
        {"id": "v2_envelope", "ok": isinstance(report, dict) and
         set(report) == {"schema_version", "kind", "metadata"}},
        {"id": "schema_version_2", "ok": report.get("schema_version") == 2},
        {"id": "kind_inspect", "ok": report.get("kind") == "inspect"},
        {"id": "expected_operation_status",
         "ok": metadata.get("status") == expected_status,
         "detail": f"expected {expected_status}, got {metadata.get('status')}"},
        {"id": "independent_sha256",
         "ok": digest.get("digest") == record["sha256"]},
        {"id": "independent_hashed_byte_count",
         "ok": digest.get("bytes_hashed", {}).get("value") == record["bytes"]},
        {"id": "hashed_byte_unit",
         "ok": digest.get("bytes_hashed", {}).get("unit") == "byte"},
        {"id": "not_immutable_snapshot",
         "ok": digest.get("snapshot_guarantee") == "not_immutable"},
        {"id": "no_body_or_legacy_fields",
         "ok": isinstance(report, dict) and not metadata_content_fields(report)},
    ]


def perf_extract_checks(report, expect, indices):
    """Check one extract report against frozen independent content oracles.

    Accepts a decoded report, an expectation dictionary with status, unit,
    available count, indices, optional exact texts, per-page contained lines,
    empty-page positions, exact single-unit text, completeness, expected page
    count, and OCR phrases. Returns v2, selection, identity, and content
    checks. Missing report fields produce failed checks, never exceptions.
    """
    checks = [
        {"id": "v2_envelope", "ok": isinstance(report, dict) and
         report.get("schema_version") == 2 and report.get("kind") == "extract"},
    ]
    if not isinstance(report, dict):
        return checks
    metadata = report.get("metadata", {})
    selection = report.get("selection", {})
    units = report.get("content", {}).get("units", [])
    checks.extend([
        {"id": "operation_status", "ok": metadata.get("status") == expect["status"],
         "detail": f"expected {expect['status']}, got {metadata.get('status')}"},
        {"id": "native_unit", "ok": selection.get("unit") == expect["unit"]},
        {"id": "available_count",
         "ok": selection.get("available", {}).get("value") == expect["available"]},
        {"id": "returned_indices",
         "ok": selection.get("returned_indices") == indices,
         "detail": f"actual {selection.get('returned_indices')}"},
        {"id": "unit_identity",
         "ok": [unit.get("unit_index") for unit in units] == indices},
    ])
    if "completeness" in expect:
        checks.append({"id": "report_completeness",
                       "ok": report.get("completeness") == expect["completeness"]})
    if "exact_texts" in expect:
        actual = [unit.get("text") for unit in units]
        checks.append({"id": "independent_exact_text",
                       "ok": actual == expect["exact_texts"],
                       "detail": "constructed page texts differ"})
    if "exact_text" in expect:
        actual = units[0].get("text") if units else None
        checks.append({"id": "independent_exact_full_text",
                       "ok": actual == expect["exact_text"],
                       "detail": f"actual length {len(actual) if actual else 0}"})
    for page_index, lines in expect.get("contain_lines", {}).items():
        unit = next((u for u in units if u.get("unit_index") == page_index), {})
        page_text = perf_normalized(unit.get("text") or "")
        missing = [line for line in lines
                   if perf_normalized(line) not in page_text]
        checks.append({"id": f"contained_lines_page_{page_index}",
                       "ok": not missing, "detail": f"missing {missing[:3]}"})
    for page_index in expect.get("empty_pages", ()):  # zero-based positions
        unit = next((u for u in units if u.get("unit_index") == page_index + 1), {})
        checks.append({"id": f"empty_page_{page_index + 1}",
                       "ok": not (unit.get("text") or "").strip()})
    if "phrases" in expect:
        full = perf_normalized(" ".join(unit.get("text") or "" for unit in units))
        missing = [phrase for phrase in expect["phrases"]
                   if perf_normalized(phrase) not in full]
        checks.append({"id": "independent_ocr_phrases", "ok": not missing,
                       "detail": f"missing {missing[:3]}"})
    return checks


def perf_decode_report(raw):
    """Decode the child's report JSON from raw stdout bytes.

    Accepts raw child output bytes. Returns the decoded JSON value or None
    when no parsable JSON exists. Pretty-printed multi-document JSON is
    parsed as a whole first; single-line JSON with preceding warnings falls
    back to the last parsable line.
    """
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        pass
    for line in reversed(raw.splitlines()):
        try:
            return json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
    return None


def perf_scenario_checks(scenario, execution, raw, fixtures):
    """Evaluate one measured run against its frozen independent oracle.

    Accepts a scenario definition, measured execution, raw stdout bytes, and
    the frozen fixture map. Returns the list of boolean checks for this run.
    Any operational mismatch is a failed check; transport problems raise
    DriverError so no failed check can be silently dropped.
    """
    expect = scenario["expect"]
    checks = [
        {"id": "expected_exit", "ok": execution["status"] == expect["exit"],
         "detail": f"expected {expect['exit']}, got {execution['status']}"},
        {"id": "no_timeout", "ok": not execution["timed_out"]},
    ]
    family = scenario["family"]
    if family == "inspect":
        checks.extend(perf_inspect_checks(perf_decode_report(raw),
                                          fixtures[expect["fixture"]],
                                          expect["status"]))
    elif family in ("extract-pdf", "extract-plain", "extract-ocr"):
        checks.extend(perf_extract_checks(perf_decode_report(raw), expect,
                                          expect["indices"]))
    elif family == "parse":
        report = perf_decode_report(raw)
        pages = report.get("pages", []) if isinstance(report, dict) else []
        checks.extend([
            {"id": "parse_total_pages",
             "ok": isinstance(report, dict) and
             report.get("total_pages") == expect["total_pages"]},
            {"id": "parse_page_texts",
             "ok": len(pages) == expect["total_pages"] and
             all(expect["phrases"][index] in (page.get("text") or "")
                 for index, page in enumerate(pages))},
        ])
    elif family == "batch":
        responses = decode_jsonl(raw)
        checks.extend([
            {"id": "ordered_ids", "ok": [r.get("id") for r in responses] ==
             expect["ids"]},
            {"id": "response_count", "ok": len(responses) == len(expect["ids"])},
            {"id": "ordered_statuses",
             "ok": [r.get("report", {}).get("metadata", {}).get("status")
                    for r in responses] == expect["statuses"]},
        ])
        for request_id, key in expect.get("retention", {}).items():
            response = next((r for r in responses if r.get("id") == request_id), {})
            units = (response.get("report", {}).get("content", {})
                     .get("units", []))
            actual = units[0].get("text") if units else None
            checks.append({"id": f"retention_{request_id}",
                           "ok": actual == fixtures[key]["content"]})
    elif family == "inventory":
        responses = decode_jsonl(raw)
        paths = [r.get("path") for r in responses]
        checks.extend([
            {"id": "response_count", "ok": len(responses) == expect["count"]},
            {"id": "sorted_paths", "ok": paths ==
             sorted(paths, key=os.fsencode)},
            {"id": "ordered_statuses",
             "ok": [r.get("report", {}).get("metadata", {}).get("status")
                    for r in responses] == expect["statuses"]},
        ])
    elif family == "historical":
        checks.append({"id": "body_sentinels_present",
                       "ok": all(sentinel.encode() in raw
                                 for sentinel in expect["sentinels"]),
                       "detail": "old-contract body sentinel missing"})
        if "full_content" in expect:
            checks.append({"id": "full_content_retained",
                           "ok": expect["full_content"].encode() in raw})
    return checks


def perf_summarize(metrics):
    """Aggregate one scenario's measured replicates with worst-case retention.

    Accepts the list of measured execution records for one scenario and one
    binary. Returns replicate count, median/min/max/mean wall seconds, max
    CPU seconds, max exact peak RSS, and max sampled io read bytes. The
    worst case is always reported alongside the median.
    """
    walls = sorted(record["wall_s"] for record in metrics)
    count = len(walls)
    return {
        "replicates": count,
        "median_wall_s": round(statistics.median(walls), 4) if walls else None,
        "min_wall_s": walls[0] if walls else None,
        "max_wall_s": walls[-1] if walls else None,
        "mean_wall_s": round(sum(walls) / count, 4) if walls else None,
        "max_cpu_s": max((record["cpu_s"] for record in metrics), default=None),
        "max_peak_rss_kb": max((record["peak_rss_kb"] for record in metrics),
                               default=None),
        "max_io_read_bytes": max((record["io_read_bytes"] for record in metrics),
                                 default=None),
    }


def perf_scenario_noise(summary):
    """Compute the observed wall spread (max minus min) of one scenario.

    Accepts one scenario summary. Returns the observed noise in seconds used
    to classify candidate gains as within or beyond noise, or None when the
    summary has no replicates.
    """
    if not summary or summary.get("max_wall_s") is None:
        return None
    return summary["max_wall_s"] - summary["min_wall_s"]


def perf_candidate_verdict(baseline, candidate, heavy_scenarios,
                           resolution_floor_s=PERF_TIME_RESOLUTION_FLOOR_S):
    """Classify one candidate against the baseline with noise and floor rules.

    Accepts the per-scenario summaries of the baseline and the candidate, the
    list of heavy scenario names, and the wall-clock resolution floor. Returns
    within-floor gains, beyond-floor gains, beyond-floor regressions, explicit
    worst-case tradeoffs for every heavy scenario, and a bounded
    recommendation string for the controller. A median gain is beyond noise
    only when it exceeds both the observed baseline spread and the resolution
    floor; worst-case regressions beyond the floor never stay hidden behind
    "zero regressions from medians" — they surface as tradeoffs and downgrade
    the verdict. No adoption decision is made by this function.
    """
    within, improved, regressed, tradeoffs = [], [], [], []
    for name in heavy_scenarios:
        base = baseline.get(name)
        other = candidate.get(name)
        if not base or not other or base.get("median_wall_s") is None:
            continue
        noise = perf_scenario_noise(base) or 0.0
        margin = max(noise, resolution_floor_s)
        median_gain = base["median_wall_s"] - other["median_wall_s"]
        worst_delta = (other["max_wall_s"] - base["max_wall_s"]
                       if base.get("max_wall_s") is not None and
                       other.get("max_wall_s") is not None else None)
        entry = {"scenario": name,
                 "baseline_median_s": base["median_wall_s"],
                 "candidate_median_s": other["median_wall_s"],
                 "median_gain_s": round(median_gain, 4),
                 "baseline_noise_s": round(noise, 4),
                 "resolution_floor_s": resolution_floor_s}
        if median_gain > margin:
            improved.append(entry)
        elif median_gain < -margin:
            regressed.append(entry)
        else:
            within.append(entry)
        if worst_delta is not None:
            tradeoffs.append({
                "scenario": name,
                "baseline_worst_s": base["max_wall_s"],
                "candidate_worst_s": other["max_wall_s"],
                "worst_delta_s": round(worst_delta, 4),
                "worst_regression_beyond_floor":
                    worst_delta > resolution_floor_s})
    verdict = "retain-baseline"
    if improved and not regressed:
        verdict = "consider-adoption"
        if any(item["worst_regression_beyond_floor"] for item in tradeoffs):
            verdict = "consider-adoption-with-worst-case-tradeoffs"
    return {"verdict": verdict,
            "rule": "adoption may be considered only when at least one heavy "
                    "scenario improves beyond max(observed baseline spread, "
                    f"{resolution_floor_s}s resolution floor) on medians "
                    "and none regresses beyond it on medians; every "
                    "worst-case "
                    "regression beyond the floor is disclosed as a tradeoff "
                    "and downgrades the verdict; the controller owns the "
                    "adoption decision",
            "improved_beyond_margin": improved,
            "regressed_beyond_margin": regressed,
            "within_margin": within,
            "worst_case_tradeoffs": tradeoffs}


def perf_pocr_reuse_fixtures(dataset_dir=PERF_POCR_DATASET_DIR,
                             manifest_path=PERF_POCR_ORACLE_MANIFEST,
                             frozen_sha=PERF_POCR_FROZEN_SHA,
                             schema=PERF_POCR_ORACLE_SCHEMA):
    """Validate and return the reused read-only P-OCR fixture records.

    Accepts the external dataset directory, the tracked portable oracle
    manifest, the driver-side frozen digest pins, and the manifest schema id;
    the defaults bind to the real pinned constants so explicit performance
    and delivery verification always checks the retained evidence. Returns
    the reuse record map consumed by the frozen scenario matrix. Raises
    DriverError, without ever fabricating the external dataset, when the
    tracked manifest is unreadable, disagrees with the frozen pins, or when
    the external scanned dataset prerequisite is missing or drifted.
    """
    manifest_path = Path(manifest_path)
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise DriverError(
            f"cannot read the tracked P-OCR reuse oracle manifest "
            f"{manifest_path}: {error}") from error
    if not isinstance(manifest, dict) or manifest.get("schema") != schema:
        raise DriverError(
            f"the tracked P-OCR reuse oracle manifest {manifest_path} does "
            f"not declare the expected schema {schema}")
    records = {}
    for record in manifest.get("fixtures", ()):
        if isinstance(record, dict) and record.get("name"):
            records[record["name"]] = record
    missing = [name for name in frozen_sha if name not in records]
    if missing:
        raise DriverError(
            f"tracked P-OCR reuse oracle manifest {manifest_path} is "
            f"missing the frozen fixtures: {missing}")
    reused = {}
    for name, pinned in frozen_sha.items():
        record = records[name]
        if record.get("sha256") != pinned:
            raise DriverError(
                f"the tracked P-OCR reuse oracle manifest disagrees with "
                f"the frozen {name} digest: {record.get('sha256')}")
        path = Path(dataset_dir) / name
        if not path.is_file():
            raise DriverError(
                f"external P-OCR dataset prerequisite is missing: {path}; "
                f"the frozen scanned fixture {name} (sha256 {pinned}, "
                f"provenance tracked in {manifest_path}) must be supplied "
                f"under {Path(dataset_dir)}; the performance case never "
                "fabricates the external dataset")
        observed = sha256_file(path)
        if observed != pinned:
            raise DriverError(
                f"external P-OCR dataset fixture {name} digest mismatch: "
                f"{observed} != {pinned}")
        reused[name] = {"path": str(path), "bytes": path.stat().st_size,
                        "sha256": observed,
                        "expected_phrases": record["expected_phrases"],
                        "expected_pages": record["expected_pages"],
                        "native_phrases": record["native_phrases"],
                        "ocr_phrases": record["ocr_phrases"]}
    return reused


def perf_build_fixtures(sandbox):
    """Construct the frozen performance fixture set with independent oracles.

    Accepts one new sandbox directory. Returns a fixture map whose records
    carry Python-computed SHA-256 digests, byte sizes, and independently
    constructed expected content. Every fixture is written once, before any
    comparative run; generation cost is excluded from timed measurements and
    charged to preparation. Rasterization reuses the harness's deterministic
    pdftoppm path. Read-only reused P-OCR fixtures are hash-verified against
    the accepted spike record before use.
    """
    inputs = sandbox / "inputs"
    inputs.mkdir(parents=True, mode=0o700)
    directory = sandbox / "dir"
    directory.mkdir(mode=0o700)
    fixtures = {}

    def register(name, path, content=None):
        path = Path(path)
        record = {"path": str(path), "bytes": path.stat().st_size,
                  "sha256": sha256_file(path)}
        if content is not None:
            record["content"] = content
        fixtures[name] = record
        return record

    def single_line(total, token):
        payload = token.encode()
        repeats = total // len(payload)
        pad = total - repeats * len(payload)
        return (payload * repeats + b"Z" * pad).decode()

    tiny_pages = [f"PERF TINY PAGE {index} NATIVE 2026" for index in range(1, 7)]
    tiny = inputs / "tiny6.pdf"
    write_exclusive(tiny, build_pdf([{"text": text} for text in tiny_pages]))
    register("tiny6", tiny)

    scan_source = inputs / "scan-source.pdf"
    write_exclusive(scan_source, build_pdf([{"text": "PERF SCAN SOURCE 2026"}]))
    raster = rasterize_with_pdftoppm(scan_source, inputs / "scan-raster", dpi=72)
    width, height, rgb = decode_png_rgb(raster)
    mixed_pages = []
    mixed_expectation = []
    for index in range(1, 17):
        if index <= 6:
            mixed_pages.append({"text": f"PERF MIXED NATIVE {index} 2026"})
            mixed_expectation.append(("exact", f"PERF MIXED NATIVE {index} 2026"))
        elif index <= 11:
            lines = [f"PERF DENSE PAGE {index} LINE {line} TOKEN 2026"
                     for line in range(1, 26)]
            mixed_pages.append({"lines": lines, "font_size": 14})
            mixed_expectation.append(("lines", lines))
        elif index <= 15:
            mixed_pages.append({"image": {"width": width, "height": height,
                                           "rgb": rgb}})
            mixed_expectation.append(("empty", None))
        else:
            mixed_pages.append({})
            mixed_expectation.append(("empty", None))
    mixed = inputs / "mixed16.pdf"
    write_exclusive(mixed, build_pdf(mixed_pages))
    fixtures["mixed16"] = {"path": str(mixed), "bytes": mixed.stat().st_size,
                           "sha256": sha256_file(mixed),
                           "pages": mixed_expectation}

    paged = inputs / "pages4096.pdf"
    write_exclusive(paged, build_pdf(
        [{"text": f"PERF PAGE {index} OF 4096"} for index in range(1, 4097)]))
    register("pages4096", paged)

    big14_content = single_line(14 * 1024 * 1024, "PERFPLAIN7Q ")
    big14 = inputs / "big14mib.txt"
    write_exclusive(big14, big14_content.encode())
    register("big14mib", big14, content=big14_content)

    six_content = single_line(6 * 1024 * 1024, "PERFSIX7W ")
    six = inputs / "sixmib.txt"
    write_exclusive(six, six_content.encode())
    register("sixmib", six, content=six_content)

    block = bytes(range(256)) * (1024 * 1024 // 256)
    huge = inputs / "huge255mib.bin"
    with open(huge, "xb") as stream:
        for _ in range(255):
            stream.write(block)
    register("huge255mib", huge)

    small = [("plain.txt", b"perf small plain document\n"),
             ("data.json", b'{"perf": true, "rows": [1, 2, 3]}\n'),
             ("doc.xml", b'<?xml version="1.0"?><perf><row>1</row></perf>\n'),
             ("unknown.bin", bytes((0, 1, 2, 255)))]
    for name, payload in small:
        member = directory / name
        write_exclusive(member, payload)
        fixtures[f"small-{name}"] = {"path": str(member),
                                     "bytes": len(payload),
                                     "sha256": sha256_bytes(payload)}

    # Reused read-only P-OCR fixtures: portable tracked expectations plus an
    # explicit external dataset prerequisite that fails distinctly instead of
    # depending on an unexplained ignored oracle file.
    fixtures["pocr-reuse"] = perf_pocr_reuse_fixtures()
    return fixtures


def perf_scenario_definitions(fixtures, replicates):
    """Define the frozen bounded scenario matrix for every binary variant.

    Accepts the frozen fixture map and the replicate count (>= 5). Returns the
    ordered scenario list covering final default behavior, first calls, the
    heaviest inputs, jobs N, hash-inclusive inspect, worst-capacity OCR, paged
    coverage, and parse parity. Scenario arguments and expectations are fixed
    here before any comparative execution.
    """
    inputs = Path(fixtures["tiny6"]["path"]).parent
    directory = Path(fixtures["small-plain.txt"]["path"]).parent
    tiny_pages = [f"PERF TINY PAGE {index} NATIVE 2026" for index in range(1, 7)]
    scenarios = []

    def add(name, argv, family, replicates_count, expect, input_bytes=None):
        scenarios.append({"name": name, "argv": argv, "family": family,
                          "replicates": replicates_count, "expect": expect,
                          "input_bytes": input_bytes})

    add("inspect-tiny6", ["inspect", str(inputs / "tiny6.pdf")], "inspect",
        replicates, {"exit": 0, "status": "complete", "fixture": "tiny6"})
    add("extract-tiny6-default", ["extract", str(inputs / "tiny6.pdf"),
                                  "--no-ocr"], "extract-pdf", replicates,
        {"exit": 0, "status": "complete", "unit": "physical_page",
         "available": 6, "indices": list(range(1, 7)),
         "exact_texts": tiny_pages})
    add("extract-tiny6-full", ["extract", str(inputs / "tiny6.pdf"),
                               "--pages", "1-6", "--no-ocr"], "extract-pdf",
        replicates, {"exit": 0, "status": "complete",
                     "unit": "physical_page", "available": 6,
                     "indices": list(range(1, 7)),
                     "exact_texts": tiny_pages})
    mixed_contain = {}
    mixed_empty = []
    for position, (kind, value) in enumerate(fixtures["mixed16"]["pages"],
                                             start=1):
        if kind == "exact":
            mixed_contain[position] = [value]
        elif kind == "lines":
            mixed_contain[position] = value
        else:
            mixed_empty.append(position - 1)
    add("inspect-mixed16", ["inspect", str(inputs / "mixed16.pdf")], "inspect",
        replicates, {"exit": 0, "status": "complete", "fixture": "mixed16"})
    add("extract-mixed16-full", ["extract", str(inputs / "mixed16.pdf"),
                                 "--pages", "1-16", "--no-ocr"], "extract-pdf",
        replicates, {"exit": 0, "status": "complete", "unit": "physical_page",
                     "available": 16, "indices": list(range(1, 17)),
                     "contain_lines": mixed_contain,
                     "empty_pages": mixed_empty})
    add("extract-paged4096", ["extract", str(inputs / "pages4096.pdf"),
                              "--pages", "1-4096", "--no-ocr"], "extract-pdf",
        replicates, {"exit": 0, "status": "complete", "unit": "physical_page",
                     "available": 4096, "indices": list(range(1, 4097)),
                     "contain_lines": {1: ["PERF PAGE 1 OF 4096"],
                                       4096: ["PERF PAGE 4096 OF 4096"]},
                     "completeness": "exact"})
    add("extract-big14mib", ["extract", str(inputs / "big14mib.txt"),
                             "--no-ocr"], "extract-plain", replicates,
        {"exit": 0, "status": "complete", "unit": "logical_line",
         "available": 1, "indices": [1],
         "exact_text": fixtures["big14mib"]["content"]})
    add("inspect-big14mib", ["inspect", str(inputs / "big14mib.txt")],
        "inspect", replicates,
        {"exit": 0, "status": "complete", "fixture": "big14mib"})
    add("inspect-huge255mib", ["inspect", str(inputs / "huge255mib.bin")],
        "inspect", replicates,
        {"exit": 2, "status": "unsupported", "fixture": "huge255mib"})
    dense = fixtures["pocr-reuse"]["dense-large-small-text.pdf"]
    add("extract-ocr-dense", ["extract", dense["path"]], "extract-ocr",
        replicates, {"exit": 0, "status": "complete", "unit": "physical_page",
                     "available": 1, "indices": [1],
                     "phrases": dense["expected_phrases"]})
    scanned = fixtures["pocr-reuse"]["scanned-4p.pdf"]
    add("extract-ocr-scanned4p", ["extract", scanned["path"]], "extract-ocr",
        replicates, {"exit": 0, "status": "complete", "unit": "physical_page",
                     "available": 4, "indices": [1, 2, 3, 4],
                     "phrases": scanned["expected_phrases"]})
    add("parse-tiny6", ["parse", str(inputs / "tiny6.pdf"), "--format", "json"],
        "parse", replicates,
        {"exit": 0, "total_pages": 6, "phrases": tiny_pages})

    for jobs in PERF_JOBS_VALUES:
        add(f"extract-ocr-scanned4p-jobs{jobs}",
            ["extract", scanned["path"], "--jobs", str(jobs)], "extract-ocr",
            1, {"exit": 0, "status": "complete", "unit": "physical_page",
                "available": 4, "indices": [1, 2, 3, 4],
                "phrases": scanned["expected_phrases"]})
    small_names = ("plain.txt", "data.json", "doc.xml", "unknown.bin")
    requests = b"".join(
        (json.dumps({"id": index, "path": str(directory / name)},
                    separators=(",", ":")) + "\n").encode()
        for index, name in enumerate(small_names))
    for jobs in PERF_JOBS_VALUES:
        add(f"extract-batch-smallset-jobs{jobs}",
            ["extract", "--batch", "--jobs", str(jobs), "--no-ocr"], "batch",
            1, {"exit": 2, "ids": [0, 1, 2, 3],
                "statuses": ["complete", "partial", "partial",
                             "unsupported"],
                "status_note": "plain text extracts complete; JSON and XML "
                               "extracts are bounded dataset previews with "
                               "partial status and disclosed omissions; the "
                               "unknown binary is unsupported"},
            input_bytes=requests)
    six_requests = (json.dumps({"id": 7, "path": fixtures["sixmib"]["path"]},
                               separators=(",", ":")) + "\n").encode()
    add("extract-batch-sixmib-jobs1", ["extract", "--batch", "--jobs", "1",
                                       "--no-ocr"], "batch", 1,
        {"exit": 0, "ids": [7], "statuses": ["complete"],
         "retention": {7: "sixmib"}},
        input_bytes=six_requests)
    for jobs in PERF_JOBS_VALUES:
        add(f"inspect-inventory-jobs{jobs}",
            ["inspect", str(directory), "--jobs", str(jobs)], "inventory", 1,
            {"exit": 2, "count": 4,
             "statuses": ["complete", "complete", "complete", "unsupported"]})
    return scenarios


def perf_historical_scenarios(fixtures):
    """Define the like-for-like historical workloads for the old contract.

    Accepts the frozen fixture map. Returns the historical scenario list. The
    reconstructed binary speaks the old 57e3afb contract: default inspect is
    a body-preview workload (five pages, 500 tokens, 16000 characters) and
    extract reads whole documents natively. Sentinels and retained content
    come from fixture construction; contract differences are recorded in the
    result rather than normalized away.
    """
    inputs = Path(fixtures["tiny6"]["path"]).parent
    tiny_sentinels = [f"PERF TINY PAGE {index} NATIVE 2026"
                      for index in range(1, 6)]
    return [
        {"name": "historical-inspect-tiny6",
         "argv": ["inspect", str(inputs / "tiny6.pdf")],
         "family": "historical", "replicates": 5,
         "expect": {"exit": 0, "sentinels": tiny_sentinels}},
        {"name": "historical-inspect-mixed16",
         "argv": ["inspect", str(inputs / "mixed16.pdf")],
         "family": "historical", "replicates": 5,
         "expect": {"exit": 0,
                    "sentinels": [f"PERF MIXED NATIVE {index} 2026"
                                  for index in range(1, 6)]}},
        {"name": "historical-extract-tiny6-full",
         "argv": ["extract", str(inputs / "tiny6.pdf"), "--no-ocr"],
         "family": "historical", "replicates": 5,
         "expect": {"exit": 0,
                    "sentinels": [f"PERF TINY PAGE {index} NATIVE 2026"
                                  for index in range(1, 7)]}},
        {"name": "historical-extract-big14mib",
         "argv": ["extract", str(inputs / "big14mib.txt"), "--no-ocr"],
         "family": "historical", "replicates": 5,
         "expect": {"exit": 0, "sentinels": ["PERFPLAIN7Q"],
                    "full_content": fixtures["big14mib"]["content"]}},
    ]


def perf_copy_historical_sources(destination, log_path):
    """Copy the frozen historical revision read-only into a new sandbox tree.

    Accepts a new destination directory under the permitted sandbox tree and
    an exclusive log path. Reads `git ls-tree`/`git show` only (no checkout,
    no worktree, no index change) and writes every tracked file of revision
    57e3afb into the destination. Returns the copied file count and a
    manifest mapping repository-relative paths to SHA-256 digests. Upstream
    drift against the live tree is reported by the caller.
    """
    listing = subprocess.run(
        ["git", "-C", str(REPO), "ls-tree", "-r", "--name-only",
         PERF_HISTORICAL_REVISION],
        capture_output=True, timeout=60)
    if listing.returncode != 0:
        raise DriverError("git ls-tree failed for "
                           f"{PERF_HISTORICAL_REVISION}: "
                           f"{listing.stderr.decode(errors='replace')[:200]}")
    names = listing.stdout.decode("utf-8").splitlines()
    if not names:
        raise DriverError("historical revision tree listing is empty")
    destination.mkdir(parents=True, mode=0o700)
    manifest = {}
    for name in names:
        payload = subprocess.run(
            ["git", "-C", str(REPO), "show", f"{PERF_HISTORICAL_REVISION}:{name}"],
            capture_output=True, timeout=60)
        if payload.returncode != 0:
            raise DriverError(f"git show failed for historical {name}")
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        write_exclusive(target, payload.stdout)
        manifest[name] = sha256_bytes(payload.stdout)
    write_exclusive(log_path, json.dumps(
        {"revision": PERF_HISTORICAL_REVISION, "files_copied": len(names),
         "manifest": manifest}, indent=2))
    upstream_scope = ("crates/", "Cargo.toml", "Cargo.lock", ".gitignore")
    drift = []
    for name in sorted(manifest):
        if not name.startswith(upstream_scope):
            continue
        live = REPO / name
        if not live.is_file() or sha256_file(live) != manifest[name]:
            drift.append(name)
    return {"revision": PERF_HISTORICAL_REVISION, "files_copied": len(names),
            "manifest_path": str(log_path), "upstream_drift": drift}


def perf_delivery_supplied_binary(supplied, production_binary):
    """Verify the supplied delivery --binary names the delivered executable.

    Accepts the supplied --binary value (Path, string, or None) and the cached
    production target's release glassbox path. Returns an evidence record
    with the resolved paths and the match result when the supplied path is
    the delivered executable itself (same resolved path) or byte-identical
    to it. Raises DriverError when the supplied path is missing or differs,
    so delivery validation never silently validates a different executable
    than the caller pointed at; the retained acceptance gates, accepted
    records, and target paths are unchanged.
    """
    if supplied is None:
        raise DriverError("--delivery-validation requires --binary pointing "
                          "at the cached production target's release "
                          "glassbox executable")
    supplied_resolved = Path(supplied).resolve()
    delivered_resolved = Path(production_binary).resolve()
    record = {"supplied_path": str(supplied),
              "supplied_resolved": str(supplied_resolved),
              "delivered_resolved": str(delivered_resolved)}
    matched = supplied_resolved == delivered_resolved
    if not matched and supplied_resolved.is_file() and delivered_resolved.is_file():
        matched = sha256_file(supplied_resolved) == sha256_file(delivered_resolved)
        record["identical_sha256"] = bool(matched)
    record["matches"] = bool(matched)
    if not matched:
        raise DriverError(
            "--delivery-validation --binary must be the cached production "
            f"target's release glassbox executable ({delivered_resolved}); "
            f"the supplied path ({supplied_resolved}) differs and is never "
            "silently ignored")
    return record


def perf_delivery_accepted_evidence(
        record_path=PERF_ACCEPTED_CORRECTIVE_RESULT,
        candidate_name=PERF_APPROVED_CANDIDATE_NAME,
        candidate_sha=PERF_APPROVED_CANDIDATE_BINARY_SHA,
        candidate_target=PERF_APPROVED_CANDIDATE_TARGET,
        evaluator_version=PERF_EVALUATOR_VERSION):
    """Check the accepted corrective record and the approved candidate chain.

    Accepts the accepted result.json path plus the candidate name, digest,
    target, and evaluator version; the defaults bind to the real pinned
    constants, so the delivery runner always checks the retained evidence
    with its historical pins intact. Returns a record with the fail-closed
    blockers, the accepted payload, the accepted experiment spec, the
    accepted record's candidate identity, and the approved candidate binary
    identity. Raises no exception: every defect (an unreadable accepted
    record, a non-PASS status, a missing candidate identity, a candidate
    digest disagreeing with the pinned approved digest, an experiment spec
    measured with a stale evaluator clock, or a missing or drifted retained
    approved candidate binary) is returned as a blocker so delivery
    validation aggregates findings and never passes silently.
    """
    record_path = Path(record_path)
    blockers = []
    accepted_payload = None
    accepted_spec = None
    accepted_candidate = None
    try:
        accepted_payload = json.loads(record_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        blockers.append({"stage": "accepted-evidence",
                         "detail": f"cannot read the accepted corrective-07 "
                                   f"result: {error}"})
    if accepted_payload is not None:
        if accepted_payload.get("status") != "PASS":
            blockers.append({"stage": "accepted-evidence",
                             "detail": "the retained corrective-07 record "
                                       "is not PASS"})
        accepted_candidate = (accepted_payload.get("candidate_builds", {})
                               .get(candidate_name, {}).get("binary"))
        if not accepted_candidate:
            blockers.append({"stage": "accepted-evidence",
                             "detail": "the accepted record lacks the "
                                       f"{candidate_name} "
                                       "candidate binary identity"})
        elif accepted_candidate.get("sha256") != candidate_sha:
            blockers.append({"stage": "accepted-evidence",
                             "detail": f"the accepted record's {candidate_name} "
                                       "candidate digest disagrees with the "
                                       "pinned approved digest"})
        spec_path = record_path.parent / "experiment-spec.json"
        try:
            accepted_spec = json.loads(spec_path.read_text(encoding="utf-8"))
            if (accepted_spec.get("evaluator_clock", {}).get("version")
                    != evaluator_version):
                blockers.append({"stage": "accepted-evidence",
                                 "detail": "the accepted record was not "
                                           "measured with the v2 "
                                           "blocking-clock evaluator"})
        except (OSError, json.JSONDecodeError) as error:
            blockers.append({"stage": "accepted-evidence",
                             "detail": f"cannot read the accepted "
                                       f"experiment spec: {error}"})
    approved_binary = Path(candidate_target) / "release" / "glassbox"
    approved_available = approved_binary.is_file() and \
        os.access(approved_binary, os.X_OK)
    approved_record = {
        "name": candidate_name,
        "target_dir": str(candidate_target),
        "available": bool(approved_available),
        "sha256": sha256_file(approved_binary) if approved_available else None,
        "bytes": approved_binary.stat().st_size if approved_available else None,
    }
    if not approved_available:
        blockers.append({
            "stage": "approved-candidate",
            "detail": f"the approved {candidate_name} candidate binary is "
                      f"missing: {approved_binary}"})
    elif approved_record["sha256"] != candidate_sha:
        blockers.append({
            "stage": "approved-candidate",
            "detail": "the retained approved candidate binary digest no "
                      "longer matches the accepted corrective-07 record"})
    return {"blockers": blockers, "accepted_payload": accepted_payload,
            "accepted_spec": accepted_spec,
            "accepted_candidate": accepted_candidate,
            "approved_candidate_binary": approved_record}


def run_performance_delivery_case(args, evidence_dir):
    """Run V-PERF delivery validation on the shipped production binary.

    Accepts parsed driver arguments (requires --binary pointing at the
    cached production target's release glassbox) and an authorized evidence
    root. Returns 0 PASS, 1 FAIL, or 2 BLOCKED. This mode performs exactly
    one canonical-profile rebuild of the cached production target (an
    incremental no-op proves an already-canonical binary), proves the
    accepted-evidence chain from the controller-accepted corrective-07
    result (all retained prior attempts supplied, Rust sources and
    dependencies unchanged except the profile-only Cargo.toml delta, and
    the accepted thin+CGU1 factor tuple), remeasures the delivered binary
    on the frozen scenario matrix with the v2 blocking clock and at least
    five fresh processes per decisive scenario, and compares whole byte
    outputs against the approved candidate binary on identical inputs. The
    supplied --binary must resolve to (or be byte-identical with) the cached
    production target's delivered executable; a differing path fails closed
    and is never silently ignored. No candidate matrix, historical rebuild,
    or counterfactual profile is
    constructed; missing evidence fails closed instead of skipping.
    """
    started = time.monotonic()
    observed_start = datetime.now(timezone.utc).isoformat()
    run = run_id()
    run_dir = mkdir_new(evidence_dir / f"performance-{run}")
    failures = []
    blockers = []
    observations = []
    ledger = {"prerequisite_chain_s": None, "canonical_rebuild_s": None,
              "fixtures_s": None, "delivered_measurements_s": None,
              "candidate_comparison_s": None, "synthesis_s": None}
    result = {
        "handoff": {
            "handoff_schema": "chai-handoff/v1",
            "identity": {"work_id": "20261121_settling_in",
                         "task_ids": ["C-TEST"], "producer": "worker-build",
                         "consumer": "master-drive",
                         "artifact": str(run_dir / "result.json"),
                         "updated_at": observed_start},
            "state": {"objective": "Validate the delivered production binary "
                                   "after controller adoption of the canonical "
                                   "release profile without weakening any "
                                   "required contract.",
                      "phase": "milestone-4 wave-11",
                      "execution_status": "in_progress",
                      "pause_point": "V-PERF delivery validation execution",
                      "next_action": "Controller reviews delivery evidence "
                                     "before C-DOCS/C-FINAL.",
                      "blockers": []},
            "source": {"workspace": str(REPO),
                       "revision": "57e3afb7f1d59ca93811eda5748aa917613f64fc",
                       "uncommitted_changes": "Accepted waves through B-PERF "
                                              "plus the controller-adopted "
                                              "release profile and C-TEST "
                                              "automation edits",
                       "observed_at": observed_start},
            "authority": {"approval_source": "B-PERF controller receipt: "
                          "corrective-07 accepted with 2203 checks; Sri "
                          "approved the 15-minute corrective pass and the "
                          "controller adopted the canonical release profile",
                          "scope": "One canonical rebuild of the cached "
                                   "production target and delivered-binary "
                                   "remeasurement; no new candidate matrix, "
                                   "no historical rebuild, no counterfactual "
                                   "profile",
                          "limits": "No installs, network, mounts, "
                                    "deletions, Git mutations, publication, "
                                    "or audit clearance"},
            "evidence": {"verification_ids": ["V-PERF"], "checks": [],
                         "artifacts": [str(run_dir)],
                         "missing_coverage": ["Native x64 and independent "
                                              "audit remain outstanding"]},
            "receipt": {"consumer": "master-drive", "status": "pending"},
        },
        "case": "performance", "verification_id": "V-PERF", "task": "C-TEST",
        "mode": {"kind": "delivery-validation",
                 "version": PERF_DELIVERY_MODE_VERSION,
                 "evaluator_clock_version": PERF_EVALUATOR_VERSION,
                 "note": "reuses the frozen v2 blocking-clock evaluator; no "
                         "metrology change accompanies this mode"},
        "status": None, "exit_code": None, "run_dir": str(run_dir),
        "timing_serial": True, "active_budget_s": PERF_ACTIVE_BUDGET_S,
        "runs": [], "cost_ledger_s": ledger,
    }
    result["resource_limits"] = resource_limit_snapshot()
    attempt_history = {"attempts": [], "count": 0,
                       "known_total_wall_s": 0.0,
                       "note": "attempt history not yet recorded"}

    def record_run(name, execution, checks):
        result["runs"].append({"name": name, "execution": execution,
                               "checks": checks})
        for check in checks:
            if not check["ok"]:
                failures.append({"run": name, "check": check["id"],
                                 "detail": check.get("detail",
                                                     "assertion failed")})

    try:
        if not result["resource_limits"]["valid"]:
            raise DriverError("mandatory four-CPU, nice-10, or job-limit "
                              "guard is not effective")
        if getattr(args, "perf_replicates", None) is not None and \
                args.perf_replicates < PERF_MIN_REPLICATES:
            raise DriverError(f"--perf-replicates must be at least "
                              f"{PERF_MIN_REPLICATES} for delivery validation")
        chain_started = time.monotonic()

        # 1. Retained attempt ledger: every prior attempt must be supplied.
        attempt_history = perf_attempt_history(
            getattr(args, "prior_attempt_result", None) or ())
        result["attempt_history"] = attempt_history
        supplied = {record["path"] for record in attempt_history["attempts"]}
        if attempt_history["count"] < PERF_DELIVERY_REQUIRED_PRIOR_ATTEMPTS:
            blockers.append({
                "stage": "attempt-ledger",
                "detail": f"delivery validation requires all "
                          f"{PERF_DELIVERY_REQUIRED_PRIOR_ATTEMPTS} retained "
                          f"prior performance results (four final plus "
                          f"three corrective, including the blocked "
                          f"corrective-05/06 costs); only "
                          f"{attempt_history['count']} were supplied"})
        if str(PERF_ACCEPTED_CORRECTIVE_RESULT) not in supplied:
            blockers.append({
                "stage": "attempt-ledger",
                "detail": "the controller-accepted corrective-07 result "
                          f"({PERF_ACCEPTED_CORRECTIVE_RESULT}) was not "
                          "supplied to the attempt ledger"})

        # 2. Accepted-evidence chain from the corrective-07 record. The
        # checker runs on the real pinned constants in this mode, so absent
        # or drifted retained evidence fails closed exactly as before.
        accepted = perf_delivery_accepted_evidence()
        blockers.extend(accepted["blockers"])
        accepted_payload = accepted["accepted_payload"]
        accepted_candidate = accepted["accepted_candidate"]
        result["approved_candidate_binary"] = accepted["approved_candidate_binary"]

        # 3. Rust sources and dependencies unchanged; profile-only delta.
        source_delta = None
        try:
            prior_manifest = json.loads(
                (PERF_ACCEPTED_CORRECTIVE_RESULT.parent /
                 "source-manifest.json").read_text(encoding="utf-8"))
            source_delta = perf_delivery_source_delta(prior_manifest)
        except (OSError, json.JSONDecodeError) as error:
            blockers.append({"stage": "source-unchanged",
                             "detail": f"cannot compare delivery sources with "
                                       f"the accepted evidence: {error}"})
        except DriverError as error:
            blockers.append({"stage": "source-unchanged", "detail": str(error)})
        result["source_delta"] = source_delta
        result["release_profile"] = perf_release_profile_record()
        ledger["prerequisite_chain_s"] = round(
            time.monotonic() - chain_started, 2)

        # 4. One canonical rebuild of the shipped production target.
        production_binary = PRODUCTION_BUILD_TARGET / "release/glassbox"
        production_litx = PRODUCTION_BUILD_TARGET / "release/litx"
        # The supplied --binary must be the delivered executable; a
        # differing path fails closed instead of being silently ignored.
        result["supplied_binary"] = perf_delivery_supplied_binary(
            getattr(args, "binary", None), production_binary)
        before = {"exists": production_binary.is_file()}
        if before["exists"]:
            before.update({"sha256": sha256_file(production_binary),
                           "bytes": production_binary.stat().st_size})
        rebuild_started = time.monotonic()
        rebuild_record, _, rebuild_status = perf_cargo_build(
            EXT / "Cargo.toml", run_dir, "canonical-production-delivery",
            PERF_DELIVERY_REBUILD_TIMEOUT_S,
            build_target=PRODUCTION_BUILD_TARGET)
        rebuild_record["canonical_profile_factors"] = (
            "Cargo.toml [profile.release] lto=thin, codegen-units=1; no "
            "--config overrides and no panic setting change")
        rebuild_record["binary_before"] = before
        rebuild_record["binary_after"] = {
            "sha256": sha256_file(production_binary),
            "bytes": production_binary.stat().st_size}
        rebuild_record["binary_changed"] = (
            not before["exists"] or
            rebuild_record["binary_after"]["sha256"] != before.get("sha256"))
        rebuild_record["incremental_no_op"] = not rebuild_record["binary_changed"]
        result["canonical_rebuild"] = rebuild_record
        ledger["canonical_rebuild_s"] = round(
            time.monotonic() - rebuild_started, 2)
        if rebuild_status != 0:
            blockers.append({"stage": "canonical-rebuild",
                             "detail": f"canonical production rebuild failed "
                                       f"with status {rebuild_status}"})
        delivered_binary = production_binary
        if not delivered_binary.is_file() or not os.access(delivered_binary,
                                                           os.X_OK):
            raise DriverError(f"delivered glassbox executable is missing or "
                              f"not runnable: {delivered_binary}")
        if not production_litx.is_file() or not os.access(production_litx,
                                                           os.X_OK):
            raise DriverError(f"delivered litx alias executable is missing "
                              f"or not runnable: {production_litx}")
        result["delivered_binary"] = {
            "path": str(delivered_binary),
            "sha256": sha256_file(delivered_binary),
            "bytes": delivered_binary.stat().st_size,
            "alias": {"path": str(production_litx),
                      "sha256": sha256_file(production_litx),
                      "bytes": production_litx.stat().st_size},
            "actual_profile": result["release_profile"]["profile.release"],
        }

        # 5. Approved candidate binary for byte-output comparison. Its
        # availability, pinned digest, and accepted-record identity were
        # already checked against the real pins by the step-2 chain checker.
        approved_binary = PERF_APPROVED_CANDIDATE_TARGET / "release/glassbox"
        approved_available = result["approved_candidate_binary"]["available"]
        if accepted_candidate and approved_available:
            result["approved_candidate_binary"]["accepted_record_sha256"] = \
                accepted_candidate.get("sha256")
        observations.append({
            "observation": "delivered_binary_identical_to_approved_candidate",
            "value": bool(approved_available and
                          result["delivered_binary"]["sha256"] ==
                          result["approved_candidate_binary"]["sha256"]),
            "note": "byte-identical binaries transfer the accepted "
                    "measurement provenance directly; when unequal, the "
                    "required per-scenario byte-output comparison below "
                    "carries the behavioral equality proof",
        })

        # 6. Entrypoint validation of the exact delivered binaries.
        for label, argv in (("delivered-help", [str(delivered_binary), "--help"]),
                            ("delivered-defaults", [str(delivered_binary), "defaults"]),
                            ("alias-help", [str(production_litx), "--help"])):
            execution = perf_run_measured(
                argv, run_dir / f"entrypoint-{label}.stdout",
                run_dir / f"entrypoint-{label}.stderr", args.run_timeout)
            record_run(f"entrypoint-{label}", execution, [
                {"id": "entrypoint_exit_zero",
                 "ok": execution["status"] == 0,
                 "detail": f"status {execution['status']}"}])

        # 7. Fresh frozen fixtures and scenario matrix.
        fixtures_started = time.monotonic()
        sandbox = SANDBOX_ROOT / f"settling-in-ctest-delivery-{run}"
        sandbox.mkdir(parents=True, mode=0o700)
        fixtures = perf_build_fixtures(sandbox)
        fixture_manifest = {
            key: {field: value for field, value in record.items()
                  if field in ("path", "bytes", "sha256")}
            for key, record in fixtures.items() if key != "pocr-reuse"}
        fixture_manifest["pocr-reuse"] = {
            key: {"path": record["path"], "bytes": record["bytes"],
                  "sha256": record["sha256"]}
            for key, record in fixtures["pocr-reuse"].items()}
        write_exclusive(run_dir / "fixture-manifest.json",
                        json.dumps(fixture_manifest, indent=2))
        scenarios = perf_scenario_definitions(fixtures, args.perf_replicates)
        heavy_scenarios = ["extract-ocr-dense", "extract-ocr-scanned4p",
                           "extract-big14mib", "inspect-huge255mib",
                           "extract-paged4096", "extract-mixed16-full"]
        ledger["fixtures_s"] = round(time.monotonic() - fixtures_started, 2)

        # 8. Remeasure the delivered binary with the blocking clock.
        measure_started = time.monotonic()
        summaries = {}
        reference_outputs = {}
        for scenario in scenarios:
            metrics = []
            cumulative = 0.0
            achieved = 0
            for replicate in range(1, scenario["replicates"] + 1):
                if (time.monotonic() - started) > PERF_ACTIVE_BUDGET_S - \
                        PERF_MEASUREMENT_RESERVE_S:
                    blockers.append({
                        "stage": "budget",
                        "detail": "active delivery budget exhausted during "
                                  f"scenario {scenario['name']}"})
                    break
                label = f"delivered-{scenario['name']}-{replicate}"
                execution = perf_run_measured(
                    [delivered_binary, *scenario["argv"]],
                    run_dir / f"{label}.stdout",
                    run_dir / f"{label}.stderr",
                    args.run_timeout, scenario.get("input_bytes"))
                raw = Path(execution["stdout"]).read_bytes()
                execution["stdout_sha256"] = sha256_bytes(raw)
                execution["stdout_bytes"] = len(raw)
                checks = perf_scenario_checks(scenario, execution, raw, fixtures)
                record_run(label, execution, checks)
                metrics.append(execution)
                achieved = replicate
                cumulative += execution["wall_s"]
                if execution["timed_out"]:
                    break
                if cumulative >= PERF_SCENARIO_CAP_S:
                    break
            summary = perf_summarize(metrics)
            summary["achieved_replicates"] = achieved
            summaries[scenario["name"]] = summary
            if (scenario["replicates"] >= PERF_MIN_REPLICATES and
                    achieved < scenario["replicates"]):
                blockers.append({
                    "stage": "coverage",
                    "detail": f"scenario {scenario['name']} on the delivered "
                              f"binary achieved {achieved} of "
                              f"{scenario['replicates']} required replicates"})
            if achieved:
                reference_outputs[scenario["name"]] = metrics[0]["stdout_sha256"]
        ledger["delivered_measurements_s"] = round(
            time.monotonic() - measure_started, 2)
        result["delivered_summaries"] = summaries

        # 9. Approved candidate comparison on identical inputs.
        if approved_available:
            compare_started = time.monotonic()
            for scenario in scenarios:
                label = f"approved-candidate-{scenario['name']}"
                execution = perf_run_measured(
                    [approved_binary, *scenario["argv"]],
                    run_dir / f"{label}.stdout",
                    run_dir / f"{label}.stderr",
                    args.run_timeout, scenario.get("input_bytes"))
                raw = Path(execution["stdout"]).read_bytes()
                execution["stdout_sha256"] = sha256_bytes(raw)
                checks = [
                    {"id": "candidate_run_exit_matches_expectation",
                     "ok": execution["status"] == scenario["expect"]["exit"],
                     "detail": f"status {execution['status']}"},
                    {"id": "byte_identical_with_approved_candidate",
                     "ok": (scenario["name"] in reference_outputs and
                            execution["stdout_sha256"] ==
                            reference_outputs[scenario["name"]]),
                     "detail": "whole stdout bytes compared on identical "
                               "inputs; no path normalization applied"},
                ]
                record_run(label, execution, checks)
            ledger["candidate_comparison_s"] = round(
                time.monotonic() - compare_started, 2)

        # 10. Accepted-evidence context and synthesis.
        synth_started = time.monotonic()
        accepted_context = None
        if accepted_payload is not None:
            accepted_summaries = accepted_payload.get("scenario_summaries", {})
            accepted_candidate_summaries = accepted_summaries.get(
                PERF_APPROVED_CANDIDATE_NAME, {})
            delivered_vs_accepted = {}
            for scenario_name in heavy_scenarios:
                delivered_median = summaries.get(scenario_name, {}).get("median_wall_s")
                accepted_median = accepted_candidate_summaries.get(
                    scenario_name, {}).get("median_wall_s")
                if delivered_median is None or accepted_median is None:
                    continue
                delivered_vs_accepted[scenario_name] = {
                    "delivered_median_s": delivered_median,
                    "accepted_candidate_median_s": accepted_median,
                    "delta_s": round(delivered_median - accepted_median, 4),
                }
            accepted_context = {
                "corrective_result": str(PERF_ACCEPTED_CORRECTIVE_RESULT),
                "status": accepted_payload.get("status"),
                "approved_candidate": accepted_candidate,
                "accepted_baseline_note":
                    "the accepted lab comparison measured the old v2-source "
                    "default (no [profile] section, lto off, codegen-units "
                    "16) as its baseline; the delivered default now inherits "
                    "thin+CGU1, so the accepted comparison is retained "
                    "read-only and is never presented as an identical "
                    "comparative baseline for this delivery",
                "heavy_scenario_medians": delivered_vs_accepted,
                "known_wall_all_attempts_s": attempt_history[
                    "known_total_wall_s"],
            }
        result["accepted_evidence"] = accepted_context
        record_run("delivery-coverage", {"kind": "comparison"}, [
            {"id": "all_scenarios_measured_on_delivered_binary",
             "ok": all(scenario["name"] in summaries for scenario in scenarios)},
            {"id": "worst_case_reported",
             "ok": all(summary.get("max_wall_s") is not None
                       for summary in summaries.values())},
            {"id": "rss_recorded",
             "ok": all(run["execution"].get("peak_rss_kb")
                       for run in result["runs"]
                       if run["execution"].get("argv"))},
            {"id": "binary_size_and_profile_recorded",
             "ok": bool(result["delivered_binary"]["bytes"]) and
             result["delivered_binary"]["actual_profile"] ==
             PERF_CANONICAL_RELEASE_PROFILE},
            {"id": "build_time_recorded",
             "ok": rebuild_record.get("duration_s") is not None},
            {"id": "attempt_history_recorded",
             "ok": attempt_history["count"] >=
             PERF_DELIVERY_REQUIRED_PRIOR_ATTEMPTS,
             "detail": f"{attempt_history['count']} prior attempts "
                       "accounted"},
            {"id": "delivery_mode_version_disclosed",
             "ok": PERF_DELIVERY_MODE_VERSION >= 1 and
             PERF_EVALUATOR_VERSION >= 2,
             "detail": f"delivery mode v{PERF_DELIVERY_MODE_VERSION} on "
                       f"evaluator v{PERF_EVALUATOR_VERSION}"},
        ])
        ledger["synthesis_s"] = round(time.monotonic() - synth_started, 2)
    except (DriverError, OSError, subprocess.SubprocessError,
            json.JSONDecodeError) as error:
        blockers.append({"stage": "delivery", "detail": str(error)})
    except Exception as error:
        blockers.append({"stage": "internal",
                         "detail": f"{type(error).__name__}: {error}"})

    total_wall = time.monotonic() - started
    accounting = perf_budget_accounting(attempt_history, None, total_wall)
    accounting["original_campaign_note"] += (
        "; this delivery validation keeps that deviation disclosed and "
        "never resets it: the per-run wall below is only this run's cost")
    result["costs"] = {
        "elapsed_active_s": round(total_wall, 4),
        "total_wall_s": round(total_wall, 4),
        "within_30_minute_budget": total_wall <= PERF_ACTIVE_BUDGET_S,
        "budget_scope": "this delivery run's wall only; the cumulative honest "
                        "accounting across all retained attempts lives in "
                        "budget_accounting and the disclosed original "
                        "campaign deviation is never claimed compliant",
        "budget_accounting": accounting,
    }
    if total_wall > PERF_ACTIVE_BUDGET_S:
        blockers.append({"stage": "budget",
                         "detail": "delivery validation exceeded the "
                                   "30-minute active budget"})
    result["observations"] = observations
    result["checks_total"] = sum(len(run["checks"]) for run in result["runs"])
    result["handoff"]["state"]["execution_status"] = (
        "completed" if not failures and not blockers else "failed")
    result["handoff"]["state"]["pause_point"] = (
        "V-PERF delivery validation ready for controller review before "
        "C-DOCS/C-FINAL")
    result["handoff"]["state"]["blockers"] = blockers
    result["handoff"]["evidence"]["checks"] = [{
        "verification_id": "V-PERF",
        "command": f"python3 -B {SCRIPT_PATH} performance "
                   f"--delivery-validation --skip-build --binary "
                   f"{getattr(args, 'binary', '')} --evidence-dir "
                   f"{evidence_dir}",
        "result": ("PASS" if not failures and not blockers else
                   "FAIL" if failures else "BLOCKED"),
        "observed_at": datetime.now(timezone.utc).isoformat(),
        "revision": result["handoff"]["source"]["revision"],
        "delivered_binary_sha256": result.get("delivered_binary", {}).get("sha256"),
        "evidence": str(run_dir / "result.json")}]
    return finalize(result, run_dir, failures, blockers, evidence_dir)


def run_performance_case(args, evidence_dir):
    """Run V-PERF: bounded serial profile comparison with frozen oracles.

    Accepts parsed driver arguments and an authorized evidence root. Returns
    0 PASS, 1 FAIL, or 2 BLOCKED. The campaign validates guards, freezes the
    experiment specification and fixture hashes before comparative runs,
    resolves the current default binary with provenance evidence, builds at
    most two fixed factor-only candidates in isolated targets, measures every
    scenario with fresh processes and exact per-child rusage, compares
    byte-identical outputs across variants, optionally reconstructs the
    historical Phase A baseline read-only, and fails closed on any missing
    coverage, metrology, or budget.
    """
    started = time.monotonic()
    observed_start = datetime.now(timezone.utc).isoformat()
    run = run_id()
    run_dir = mkdir_new(evidence_dir / f"performance-{run}")
    failures = []
    blockers = []
    historical_spent = [0.0]
    ledger = {"preparation_s": None, "incremental_build_s": None,
              "candidate_builds_s": None, "measurements_s": None,
              "historical_stage_s": None, "synthesis_s": None}
    result = {
        "handoff": {
            "handoff_schema": "chai-handoff/v1",
            "identity": {"work_id": "20261121_settling_in",
                         "task_ids": ["B-PERF"], "producer": "worker-build",
                         "consumer": "master-drive",
                         "artifact": str(run_dir / "result.json"),
                         "updated_at": observed_start},
            "state": {"objective": "Measure final default behavior and fixed "
                                   "profile candidates without weakening any "
                                   "required safety or quality contract.",
                      "phase": "milestone-4 wave-10",
                      "execution_status": "in_progress",
                      "pause_point": "V-PERF campaign execution",
                      "next_action": "Controller reviews evidence and owns "
                                     "any Cargo.toml profile adoption.",
                      "blockers": []},
            "source": {"workspace": str(REPO),
                       "revision": "57e3afb7f1d59ca93811eda5748aa917613f64fc",
                       "uncommitted_changes": "Accepted waves through B-JOBS "
                                              "plus the B-PERF driver case",
                       "observed_at": observed_start},
            "authority": {"approval_source": "Approved spec, milestones, "
                          "A-GATE production approval, and B-PERF dispatch; "
                          "Sri approved production execution",
                          "scope": "Driver and its self-tests only; canonical "
                                   "Cargo.toml stays untouched; controller "
                                   "owns evidence-based adoption",
                          "limits": "No installs, network, mounts, deletions, "
                                    "Git mutations, publication, or audit "
                                    "clearance"},
            "evidence": {"verification_ids": ["V-PERF"], "checks": [],
                         "artifacts": [str(run_dir)],
                         "missing_coverage": ["Native x64 and independent "
                                              "audit remain outstanding"]},
            "receipt": {"consumer": "master-drive", "status": "pending"},
        },
        "case": "performance", "verification_id": "V-PERF", "task": "B-PERF",
        "status": None, "exit_code": None, "run_dir": str(run_dir),
        "timing_serial": True, "active_budget_s": PERF_ACTIVE_BUDGET_S,
        "runs": [], "cost_ledger_s": ledger,
    }
    result["resource_limits"] = resource_limit_snapshot()

    def record_run(name, execution, checks):
        """Retain one transparent run and fold every failed check in.

        Accepts a unique run name, execution evidence, and boolean checks.
        Returns unit; no assertion is hidden or rewritten.
        """
        result["runs"].append({"name": name, "execution": execution,
                               "checks": checks})
        for check in checks:
            if not check["ok"]:
                failures.append({"run": name, "check": check["id"],
                                 "detail": check.get("detail",
                                                     "assertion failed")})

    def active_remaining():
        """Return remaining active budget seconds, historical stage excluded.

        Accepts no parameters. The historical reconstruction cost is charged
        and reported separately from the 30-minute active budget.
        """
        elapsed = time.monotonic() - started - historical_spent[0]
        return PERF_ACTIVE_BUDGET_S - elapsed

    corrective_budget_s = getattr(args, "corrective_budget_s", None)
    attempt_history = {"attempts": [], "count": 0,
                      "known_total_wall_s": 0.0,
                      "note": "attempt history not yet recorded"}
    try:
        if not result["resource_limits"]["valid"]:
            raise DriverError("mandatory four-CPU, nice-10, or job-limit "
                              "guard is not effective")
        if getattr(args, "perf_replicates", None) is not None and \
                args.perf_replicates < PERF_MIN_REPLICATES:
            raise DriverError(f"--perf-replicates must be at least "
                              f"{PERF_MIN_REPLICATES}")
        candidates = perf_parse_candidates(getattr(
            args, "perf_candidates", ",".join(PERF_DEFAULT_CANDIDATES)))
        attempt_history = perf_attempt_history(
            getattr(args, "prior_attempt_result", None) or ())
        result["attempt_history"] = attempt_history
        if not attempt_history["count"] and not getattr(
                args, "allow_first_attempt", False):
            raise DriverError(
                "cumulative prior attempt accounting is required for the "
                "performance case: supply --prior-attempt-result for every "
                "retained earlier attempt, or pass --allow-first-attempt to "
                "declare explicitly that this is the first attempt and no "
                "earlier cost exists")
        result["environment"] = environment_snapshot()
        result["git"] = git_state(REPO)
        snapshot = source_snapshot(REPO)
        write_exclusive(run_dir / "source-manifest.json",
                        json.dumps(snapshot["manifest"], indent=2))
        result["source_snapshot"] = {
            "revision": result["git"]["head"],
            "branch": result["git"]["branch"],
            "clean": result["git"]["clean"],
            "dirty_files": result["git"]["dirty_files"],
            "tree_sha256": snapshot["tree_sha256"],
            "total_source_bytes": snapshot["total_bytes"],
            "scopes": list(SOURCE_SCOPES),
            "manifest": str(run_dir / "source-manifest.json"),
        }
        profile_record = perf_release_profile_record()
        adopted = (profile_record["profile.release"] ==
                   PERF_CANONICAL_RELEASE_PROFILE)
        result["release_profile_baseline"] = {
            "canonical_manifest_profiles": profile_record,
            "candidates": {name: list(PERF_CANDIDATE_FACTORS[name])
                           for name in candidates},
            "cargo_toml_unchanged": True,
            "note": ("the canonical Cargo.toml now declares the "
                     "controller-adopted release profile (lto thin, "
                     "codegen-units 1); the no-profile default this mode "
                     "compared no longer exists" if adopted else
                     "the canonical Cargo.toml declares no [profile] "
                     "section; the default release profile (opt-level 3, "
                     "lto off, codegen-units 16) is the compared baseline"),
        }
        if adopted:
            blockers.append({
                "stage": "obsolete-mode",
                "detail": "the default-profile comparison premise is broken: "
                          "the canonical release profile is the adopted "
                          "thin+CGU1 tuple, so rebuilding the same factors as "
                          "counterfactual candidates is refused; run the "
                          "performance case with --delivery-validation"})

        prep_started = time.monotonic()
        sandbox = SANDBOX_ROOT / f"settling-in-bperf-{run}"
        sandbox.mkdir(parents=True, mode=0o700)
        fixtures = perf_build_fixtures(sandbox)
        fixture_manifest = {
            key: {field: value for field, value in record.items()
                  if field in ("path", "bytes", "sha256")}
            for key, record in fixtures.items() if key != "pocr-reuse"}
        fixture_manifest["pocr-reuse"] = {
            key: {"path": record["path"], "bytes": record["bytes"],
                  "sha256": record["sha256"]}
            for key, record in fixtures["pocr-reuse"].items()}
        write_exclusive(run_dir / "fixture-manifest.json",
                        json.dumps(fixture_manifest, indent=2))
        scenarios = perf_scenario_definitions(fixtures, args.perf_replicates)
        heavy_scenarios = ["extract-ocr-dense", "extract-ocr-scanned4p",
                           "extract-big14mib", "inspect-huge255mib",
                           "extract-paged4096", "extract-mixed16-full"]
        experiment_spec = {
            "frozen_before_comparative_runs": True,
            "hypotheses": [
                "H1: thin LTO improves heavy OCR/hash/extraction wall times "
                "beyond run noise at the cost of longer builds; tiny first "
                "calls may stay within noise.",
                "H2: codegen-units=1 on top of thin LTO adds marginal gain "
                "beyond thin LTO alone at a further build-cost increase.",
                "H3: every deterministic scenario produces byte-identical "
                "output across baseline and both candidate variants.",
                "H4: the reconstructed historical binary differs on shared "
                "workloads mainly through its changed inspect/body and "
                "hashing contracts; timing deltas are attributed only to the "
                "shared native extraction workloads, never presented as a "
                "generic compiler speedup.",
                "H5: jobs 1/2/4 lower wall time with growing peak RSS on "
                "multi-page OCR workloads; this is a scaling observation, "
                "not a speed claim.",
            ],
            "stop_conditions": [
                f"active budget {PERF_ACTIVE_BUDGET_S}s excluding the "
                "separately charged historical stage",
                "a candidate build is skipped with explicit blocked coverage "
                "when remaining active budget cannot cover its build plus "
                f"{PERF_MEASUREMENT_RESERVE_S}s measurement reserve",
                f"per-run child timeout {args.run_timeout}s",
                f"per-scenario cumulative wall cap {PERF_SCENARIO_CAP_S}s "
                "stops further replicates and records the achieved count",
                "any oracle failure, timed-out run, or non-byte-identical "
                "deterministic output fails the campaign",
            ],
            "measurement_caps": {
                "replicates_decisive": args.perf_replicates,
                "replicates_jobs_observations": 1,
                "scenario_cumulative_cap_s": PERF_SCENARIO_CAP_S,
                "run_timeout_s": args.run_timeout,
            },
            "metrology": {
                "fresh_process_every_run": "every measured run is a first "
                "call in a fresh process",
                "wall": "monotonic around the direct child",
                "cpu": "wait4 ru_utime + ru_stime of the direct child "
                       "including its threads",
                "peak_rss": "exact wait4 ru_maxrss of the direct child, "
                            "cross-checked by sampled VmHWM",
                "io_read": "sampled cumulative /proc/<pid>/io read_bytes",
                "worst_case": "max replicate reported alongside median",
                "build_rss": "sampled summed RSS of the cargo process tree "
                             "(upper bound; shared pages double counted)",
                "no_subprocess_routes": "LibreOffice conversion is not "
                                        "exercised by any measured scenario",
            },
            "evaluator_clock": {
                "version": PERF_EVALUATOR_VERSION,
                "method": "blocking wait4 until the child truly exits; the "
                          "wall clock is not quantized by polling",
                "sampler": "an independent thread samples /proc/<pid>/io "
                           "read_bytes and VmHWM every 20ms and enforces "
                           "the deadline with SIGKILL; sampling resolution "
                           "affects only the RSS/io trace, never the wall",
                "resolution_floor_s": PERF_TIME_RESOLUTION_FLOOR_S,
                "clock_crosscheck": "an independent blocking subprocess "
                                    "clock re-times one real parse scenario "
                                    "and must agree within "
                                    f"{PERF_CLOCK_CROSSCHECK_TOLERANCE_S}s",
                "supersedes": "evaluator v1 (20ms WNOHANG polling loop) "
                              "latency claims retained in the earlier "
                              "attempt evidence are invalidated by the "
                              "clock change and reclassified only from "
                              "these v2 measurements",
            },
            "scenario_notes": {
                "extract-mixed16-full": "measured with --no-ocr: scanned-"
                    "image pages contribute no text and OCR cost is "
                    "excluded; the scenario measures native extraction over "
                    "a mixed 16-page document only; OCR cost is measured "
                    "separately by extract-ocr-dense and "
                    "extract-ocr-scanned4p",
                "extract-tiny6-default": "measured with --no-ocr; tiny "
                    "native-only pages; not an OCR-cost measurement",
                "extract-tiny6-full": "measured with --no-ocr; tiny "
                    "native-only pages; not an OCR-cost measurement",
                "extract-paged4096": "measured with --no-ocr; native-text "
                    "paged coverage only",
                "extract-big14mib": "measured with --no-ocr; single-line "
                    "full-retention workload",
                "extract-batch-smallset": "measured with --no-ocr; dataset "
                    "JSON/XML entries are bounded previews with partial "
                    "status and disclosed omissions, preserved as designed",
            },
            "scenarios": [{"name": s["name"], "replicates": s["replicates"],
                           "family": s["family"],
                           "argv": [str(part) for part in s["argv"]]}
                          for s in scenarios],
            "heavy_scenarios": heavy_scenarios,
            "fixture_manifest": str(run_dir / "fixture-manifest.json"),
            "clean_build_reference": perf_clean_build_reference_record(),
            "historical": {
                "requested": bool(getattr(args, "historical_reconstruction",
                                          False)),
                "revision": PERF_HISTORICAL_REVISION,
                "method": "read-only git ls-tree/git show copy into a new "
                          "sandbox tree; isolated target; separately charged",
            },
        }
        write_exclusive(run_dir / "experiment-spec.json",
                        json.dumps(experiment_spec, indent=2))
        result["experiment_spec"] = str(run_dir / "experiment-spec.json")
        ledger["preparation_s"] = round(time.monotonic() - prep_started, 2)

        # Baseline binary resolution and provenance.
        if args.skip_build:
            baseline_binary = Path(args.binary).resolve()
            result["baseline_binary"] = {
                "provided": True, "path": str(baseline_binary),
                "sha256": sha256_file(baseline_binary),
                "bytes": baseline_binary.stat().st_size}
            production_binary = (PRODUCTION_BUILD_TARGET /
                                 "release/glassbox")
            if baseline_binary != production_binary.resolve():
                result["baseline_binary"]["incremental_validation"] = {
                    "performed": False,
                    "reason": "provided binary is not the cached production "
                              "target binary"}
            else:
                if result["baseline_binary"]["sha256"] != \
                        PERF_ACCEPTED_PRODUCTION_SHA:
                    blockers.append({
                        "stage": "provenance",
                        "detail": "provided production binary does not match "
                                  "the controller-accepted B-JOBS hash "
                                  f"{PERF_ACCEPTED_PRODUCTION_SHA}"})
                inc_started = time.monotonic()
                inc_record, inc_target, inc_status = perf_cargo_build(
                    EXT / "Cargo.toml", run_dir, "incremental-production",
                    args.build_timeout,
                    build_target=PRODUCTION_BUILD_TARGET)
                inc_record["incremental"] = True
                inc_record["target_dir_is_cached_production_target"] = True
                rebuilt_sha = sha256_file(production_binary)
                inc_record["binary_sha256_before"] = \
                    result["baseline_binary"]["sha256"]
                inc_record["binary_sha256_after"] = rebuilt_sha
                inc_record["binary_unchanged"] = (
                    rebuilt_sha == result["baseline_binary"]["sha256"])
                ledger["incremental_build_s"] = round(
                    time.monotonic() - inc_started, 2)
                result["incremental_build"] = inc_record
                if inc_status != 0:
                    blockers.append({
                        "stage": "incremental",
                        "detail": f"incremental production build failed with "
                                  f"status {inc_status}"})
                elif not inc_record["binary_unchanged"]:
                    blockers.append({
                        "stage": "incremental",
                        "detail": "incremental build rebuilt the production "
                                  "binary: the provided binary was stale "
                                  "relative to the current source and the "
                                  "controller-accepted hash no longer "
                                  "matches; baseline measurements below use "
                                  "the rebuilt current-source binary"})
                    baseline_binary = production_binary
                    result["baseline_binary"]["sha256"] = rebuilt_sha
                    result["baseline_binary"]["rebuilt_by_incremental"] = True
                else:
                    result["baseline_binary"]["incremental_validation"] = {
                        "performed": True,
                        "no_op": True,
                        "detail": "incremental build against the cached "
                                  "production target was a no-op, proving "
                                  "the provided binary matches the current "
                                  "source fingerprints"}
            reference = perf_clean_build_reference_record()
            result["clean_build_cost"] = {
                "route": "reused retained controller measurement",
                "reference": reference,
            }
            if not reference.get("available"):
                blockers.append({
                    "stage": "clean-build-cost",
                    "detail": "no fresh default build ran and the retained "
                              "clean-build reference is unavailable"})
        else:
            build_record, build_target, build_status = perf_cargo_build(
                EXT / "Cargo.toml", run_dir, "baseline-default",
                args.build_timeout)
            result["baseline_build"] = build_record
            if build_status != 0:
                raise DriverError(f"baseline release build failed with "
                                   f"status {build_status}")
            baseline_binary = build_target / "release/glassbox"
            result["baseline_binary"] = {
                "provided": False, "path": str(baseline_binary),
                "sha256": sha256_file(baseline_binary),
                "bytes": baseline_binary.stat().st_size}
            result["clean_build_cost"] = {
                "route": "measured fresh default build",
                "duration_s": build_record["duration_s"],
                "peak_tree_rss_kib_sampled":
                    build_record["peak_tree_rss_kib_sampled"]}
        if not baseline_binary.is_file() or not os.access(baseline_binary,
                                                          os.X_OK):
            raise DriverError("baseline glassbox executable is missing or "
                              f"not runnable: {baseline_binary}")

        # Candidate builds with budget stop condition.
        reuse_map = perf_parse_reuse_candidates(
            getattr(args, "reuse_candidate", None))
        candidate_records = {}
        build_clock = time.monotonic()
        for name in candidates:
            if name in reuse_map:
                candidate_records[name] = {
                    "label": name,
                    "config_values": list(PERF_CANDIDATE_FACTORS[name]),
                    "build_target_dir": str(reuse_map[name]),
                    "reused": True,
                    "status": 0,
                    "duration_s": None,
                    "duration_source": "prior campaign measured fresh "
                                       "build of the same unchanged Rust "
                                       "source and the same fixed factors; "
                                       "see the retained prior evidence",
                    "peak_tree_rss_kib_sampled": None,
                    "log": None}
                continue
            previous = max([record["duration_s"]
                            for record in candidate_records.values()] or
                           [args.build_timeout * 0.6])
            reserve = max(600.0, previous * 1.3) + PERF_MEASUREMENT_RESERVE_S
            if active_remaining() < reserve:
                blockers.append({
                    "stage": "candidate-build",
                    "detail": f"candidate {name} skipped: remaining active "
                              f"budget {active_remaining():.0f}s is below the "
                              f"required reserve {reserve:.0f}s "
                              "(build plus measurement reserve)"})
                continue
            record, target, status = perf_cargo_build(
                EXT / "Cargo.toml", run_dir, name, args.build_timeout,
                PERF_CANDIDATE_FACTORS[name])
            candidate_records[name] = record
            if status != 0:
                blockers.append({
                    "stage": "candidate-build",
                    "detail": f"candidate {name} build failed with status "
                              f"{status}"})
        ledger["candidate_builds_s"] = round(time.monotonic() - build_clock, 2)
        result["candidate_builds"] = candidate_records

        binaries = [("baseline-default", baseline_binary)]
        for name, record in candidate_records.items():
            binary = Path(record["build_target_dir"]) / "release/glassbox"
            if not binary.is_file():
                blockers.append({"stage": "candidate-build",
                                  "detail": f"candidate {name} binary is "
                                            f"missing: {binary}"})
                continue
            binaries.append((name, binary))
            record["binary"] = {"path": str(binary),
                                "sha256": sha256_file(binary),
                                "bytes": binary.stat().st_size}
            record["factor_effective"] = (
                record["binary"]["sha256"] !=
                result["baseline_binary"]["sha256"])

        # Comparative measurement phase.
        measure_clock = time.monotonic()
        summaries = {name: {} for name, _ in binaries}
        reference_outputs = {}
        for variant_name, binary in binaries:
            for scenario in scenarios:
                metrics = []
                cumulative = 0.0
                achieved = 0
                for replicate in range(1, scenario["replicates"] + 1):
                    if active_remaining() <= 0:
                        blockers.append({
                            "stage": "budget",
                            "detail": "active budget exhausted during "
                                      f"scenario {scenario['name']}"})
                        break
                    label = f"{variant_name}-{scenario['name']}-{replicate}"
                    execution = perf_run_measured(
                        [binary, *scenario["argv"]],
                        run_dir / f"{label}.stdout",
                        run_dir / f"{label}.stderr",
                        min(args.run_timeout, max(30, int(active_remaining()))),
                        scenario.get("input_bytes"))
                    raw = Path(execution["stdout"]).read_bytes()
                    execution["stdout_sha256"] = sha256_bytes(raw)
                    execution["stdout_bytes"] = len(raw)
                    checks = perf_scenario_checks(scenario, execution, raw,
                                                   fixtures)
                    record_run(label, execution, checks)
                    metrics.append(execution)
                    achieved = replicate
                    cumulative += execution["wall_s"]
                    if execution["timed_out"]:
                        break
                    if cumulative >= PERF_SCENARIO_CAP_S:
                        break
                summary = perf_summarize(metrics)
                summary["achieved_replicates"] = achieved
                if (scenario["replicates"] >= PERF_MIN_REPLICATES and
                        achieved < scenario["replicates"]):
                    blockers.append({
                        "stage": "coverage",
                        "detail": f"scenario {scenario['name']} on "
                                  f"{variant_name} achieved {achieved} of "
                                  f"{scenario['replicates']} required "
                                  f"replicates"})
                summaries[variant_name][scenario["name"]] = summary
                if scenario["family"] != "historical" and achieved:
                    reference_outputs.setdefault(
                        scenario["name"], {})[variant_name] = \
                        metrics[0]["stdout_sha256"]
        ledger["measurements_s"] = round(time.monotonic() - measure_clock, 2)
        result["scenario_summaries"] = summaries

        for scenario_name, by_variant in reference_outputs.items():
            values = list(by_variant.values())
            record_run(f"cross-profile-{scenario_name}",
                       {"kind": "comparison", "variants": by_variant}, [
                           {"id": "byte_identical_across_variants",
                            "ok": len(values) == len(binaries) and
                            len(set(values)) == 1,
                            "detail": f"observed {by_variant}"}])

        # Independent blocking-clock crosscheck on a real parse workload.
        cross_scenario = next(item for item in scenarios
                              if item["name"] == "parse-tiny6")
        cross_argv = [str(binaries[0][1])] + [
            str(part) for part in cross_scenario["argv"]]
        ind_started = time.monotonic()
        subprocess.run(cross_argv, stdin=subprocess.DEVNULL,
                       stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=args.run_timeout)
        independent_wall = time.monotonic() - ind_started
        cross_execution = perf_run_measured(
            cross_argv, run_dir / "clock-crosscheck.stdout",
            run_dir / "clock-crosscheck.stderr", args.run_timeout)
        cross_delta = abs(cross_execution["wall_s"] - independent_wall)
        record_run("clock-crosscheck",
                   {"kind": "metrology",
                    "independent_blocking_wall_s": round(independent_wall, 4),
                    "measured_wall_s": cross_execution["wall_s"],
                    "delta_s": round(cross_delta, 4),
                    "tolerance_s": PERF_CLOCK_CROSSCHECK_TOLERANCE_S},
                   [{"id": "independent_clock_agreement",
                     "ok": cross_delta <=
                     PERF_CLOCK_CROSSCHECK_TOLERANCE_S,
                     "detail": f"delta {cross_delta:.4f}s vs tolerance "
                               f"{PERF_CLOCK_CROSSCHECK_TOLERANCE_S}s"}])

        # Historical comparison stage (opt-in; validated reuse preferred).
        result["historical_stage"] = {"requested": bool(
            getattr(args, "historical_reconstruction", False))}
        if not args.historical_reconstruction:
            blockers.append({
                "stage": "coverage",
                "detail": "the original Phase A baseline evidence "
                          f"({BASELINE_RESULT}) is absent and the historical "
                          "reconstruction stage was not requested; the "
                          "required Phase A baseline comparison is missing"})
        else:
            hist_clock = time.monotonic()
            hist_binary = None
            reuse_dir = getattr(args, "reuse_historical", None)
            if reuse_dir is not None:
                # Validated reuse: no source copy, no compile; the retained
                # prior attempt already copied the unchanged 57e3afb source
                # and compiled it. The binary's provenance must match that
                # retained record exactly, or reuse is refused.
                reused_binary = reuse_dir / "release/glassbox"
                reused_sha = sha256_file(reused_binary)
                matching = [
                    record for record in attempt_history["attempts"]
                    if record["historical_binary_sha256"] == reused_sha
                    and record["historical_revision"] ==
                    PERF_HISTORICAL_REVISION
                    and record["historical_upstream_drift"] == []
                    and record["historical_files_copied"]]
                if not matching:
                    raise DriverError(
                        "the reused historical binary "
                        f"{reused_binary} (sha256 {reused_sha}) is not "
                        "covered by any supplied prior attempt record with "
                        "the same binary hash, revision "
                        f"{PERF_HISTORICAL_REVISION}, and zero upstream "
                        "drift; refusing to measure an unprovenanced "
                        "historical binary")
                prior = matching[0]
                hist_binary = reused_binary
                result["historical_stage"]["source_copy"] = {
                    "revision": PERF_HISTORICAL_REVISION,
                    "files_copied": prior["historical_files_copied"],
                    "upstream_drift": prior["historical_upstream_drift"],
                    "method": "validated reuse of the historical source "
                              "copy and build recorded by a prior attempt; "
                              "no source copy or compile performed in this "
                              "run",
                    "recorded_by_prior_attempt": prior["path"],
                }
                result["historical_stage"]["build"] = {
                    "reused_prior_build": True,
                    "compile_performed": False,
                    "target_dir": str(reuse_dir),
                    "charged_separately": False,
                    "note": "rebuilding byte-identical historical sources "
                            "is not repeated; the binary hash is validated "
                            "against the retained prior attempt record",
                }
            else:
                hist_sandbox = SANDBOX_ROOT / \
                    f"settling-in-bperf-{run}-history"
                copy_record = perf_copy_historical_sources(
                    hist_sandbox, run_dir / "historical-source-manifest.json")
                if copy_record["upstream_drift"]:
                    blockers.append({
                        "stage": "historical",
                        "detail": "live upstream sources drifted from the "
                                  "historical revision; attribution premise "
                                  "broken: "
                                  + ", ".join(
                                      copy_record["upstream_drift"][:5])})
                hist_build_record, hist_target, hist_status = perf_cargo_build(
                    hist_sandbox / "extensionalities/Cargo.toml", run_dir,
                    "historical-57e3afb", PERF_HISTORICAL_BUILD_TIMEOUT_S)
                hist_build_record["charged_separately"] = True
                result["historical_stage"]["source_copy"] = copy_record
                result["historical_stage"]["build"] = hist_build_record
                if hist_status != 0:
                    blockers.append({
                        "stage": "historical",
                        "detail": f"historical build failed with status "
                                  f"{hist_status}"})
                else:
                    hist_binary = hist_target / "release/glassbox"
            if hist_binary is not None:
                result["historical_stage"]["binary"] = {
                    "path": str(hist_binary),
                    "sha256": sha256_file(hist_binary),
                    "bytes": hist_binary.stat().st_size}
                hist_summaries = {}
                for scenario in perf_historical_scenarios(fixtures):
                    metrics = []
                    for replicate in range(1, scenario["replicates"] + 1):
                        label = (f"historical-57e3afb-{scenario['name']}-"
                                 f"{replicate}")
                        execution = perf_run_measured(
                            [hist_binary, *scenario["argv"]],
                            run_dir / f"{label}.stdout",
                            run_dir / f"{label}.stderr",
                            args.run_timeout)
                        raw = Path(execution["stdout"]).read_bytes()
                        execution["stdout_sha256"] = sha256_bytes(raw)
                        execution["stdout_bytes"] = len(raw)
                        checks = perf_scenario_checks(scenario, execution,
                                                       raw, fixtures)
                        record_run(label, execution, checks)
                        metrics.append(execution)
                    hist_summaries[scenario["name"]] = perf_summarize(metrics)
                result["historical_stage"]["summaries"] = hist_summaries
                result["historical_stage"]["contract_notes"] = {
                    "historical_inspect": "the old default inspect is a body "
                    "preview workload (first five pages, 500 word tokens, "
                    "16000 characters) without full SHA-256 hashing",
                    "current_inspect": "the current default inspect is "
                    "metadata-only with a streamed full SHA-256 digest and "
                    "qualified counts",
                    "comparable": "whole-document native extraction runs "
                    "(tiny6 full, big14mib single line) are the only "
                    "like-for-like workloads; inspect timings are recorded "
                    "as different workloads and no generic speedup is "
                    "claimed from them",
                    "ocr": "the old 2048-pixel OCR recipe and the selected "
                    "3072-pixel recipe are different accuracy workloads; "
                    "historical OCR was not measured and no OCR delta is "
                    "attributed to the compiler profile",
                }
            historical_spent[0] = time.monotonic() - hist_clock
            ledger["historical_stage_s"] = round(historical_spent[0], 2)

        # Synthesis.
        synth_clock = time.monotonic()
        baseline_name = binaries[0][0]
        heavy = experiment_spec["heavy_scenarios"]
        result["candidate_comparison"] = {}
        for name, record in candidate_records.items():
            if name not in summaries:
                continue
            verdict = perf_candidate_verdict(
                summaries[baseline_name], summaries[name], heavy)
            verdict["binary_bytes"] = record.get("binary", {}).get("bytes")
            verdict["binary_sha256"] = record.get("binary", {}).get("sha256")
            verdict["baseline_binary_bytes"] = \
                result["baseline_binary"]["bytes"]
            verdict["build_duration_s"] = record["duration_s"]
            verdict["build_duration_source"] = record.get(
                "duration_source", "measured fresh build in this campaign")
            verdict["baseline_clean_build_duration_s"] = (
                result["clean_build_cost"].get("duration_s") or
                result["clean_build_cost"].get("reference", {}).get(
                    "clean_build_duration_s"))
            verdict["build_peak_tree_rss_kib_sampled"] = \
                record["peak_tree_rss_kib_sampled"]
            verdict["adoption_performed"] = False
            result["candidate_comparison"][name] = verdict
        if "historical_stage" in result and "summaries" in \
                result.get("historical_stage", {}):
            hist = result["historical_stage"]["summaries"]
            result["historical_stage"]["baseline_comparison"] = {
                name: {"current_baseline": summaries[baseline_name].get(
                           mapping[0]),
                       "historical_57e3afb": hist.get(name),
                       "workload": mapping[1]}
                for name, mapping in (
                    ("historical-extract-tiny6-full",
                     ("extract-tiny6-full", "like-for-like whole-document "
                                            "native extraction")),
                    ("historical-extract-big14mib",
                     ("extract-big14mib", "like-for-like full plaintext "
                                          "single-line extraction")),
                    ("historical-inspect-tiny6",
                     ("inspect-tiny6", "different contracts: old body "
                                      "preview versus current metadata-only "
                                      "with full hash")),
                ) if mapping[0] in summaries[baseline_name]}
        record_run("coverage-metrology", {"kind": "comparison"}, [
            {"id": "all_scenarios_measured_on_every_built_variant",
             "ok": all(scenario["name"] in summaries[variant]
                       for variant, _ in binaries
                       for scenario in scenarios)},
            {"id": "worst_case_reported", "ok": all(
                summary.get("max_wall_s") is not None
                for variant in summaries.values()
                for summary in variant.values())},
            {"id": "build_times_recorded", "ok": bool(
                result.get("clean_build_cost") and
                (args.skip_build or
                 result.get("baseline_build", {}).get("duration_s"))) and
             all(record.get("duration_s") or record.get("reused")
                 for record in candidate_records.values())},
            {"id": "rss_recorded", "ok": all(
                run["execution"].get("peak_rss_kb")
                for run in result["runs"]
                if run["execution"].get("argv"))},
            {"id": "attempt_history_recorded",
             "ok": bool(attempt_history["count"]) or bool(getattr(
                 args, "allow_first_attempt", False)),
             "detail": f"{attempt_history['count']} prior attempts "
                       "accounted" if attempt_history["count"] else
                       "first attempt explicitly declared; no prior cost "
                       "exists to account"},
            {"id": "evaluator_version_disclosed",
             "ok": PERF_EVALUATOR_VERSION >= 2,
             "detail": f"evaluator v{PERF_EVALUATOR_VERSION} blocking-clock "
                       f"contract with resolution floor "
                       f"{PERF_TIME_RESOLUTION_FLOOR_S}s"},
        ])
        ledger["synthesis_s"] = round(time.monotonic() - synth_clock, 2)
    except (DriverError, OSError, subprocess.SubprocessError,
            json.JSONDecodeError) as error:
        blockers.append({"stage": "performance", "detail": str(error)})
    except Exception as error:
        blockers.append({"stage": "internal",
                         "detail": f"{type(error).__name__}: {error}"})

    elapsed_active = (time.monotonic() - started - historical_spent[0])
    total_wall = time.monotonic() - started
    ledger["active_elapsed_s"] = round(elapsed_active, 2)
    accounting = perf_budget_accounting(
        attempt_history, corrective_budget_s, total_wall)
    if "attempt_history" not in result:
        result["attempt_history"] = attempt_history
    result["costs"] = {
        "elapsed_active_s": round(elapsed_active, 4),
        "historical_charged_separately_s": round(historical_spent[0], 4),
        "total_wall_s": round(total_wall, 4),
        "within_30_minute_budget": elapsed_active <= PERF_ACTIVE_BUDGET_S,
        "within_30_minute_budget_scope": "this run's active seconds only; "
            "the cumulative honest accounting across all retained attempts "
            "lives in budget_accounting and never resets failed attempt "
            "costs",
        "budget_accounting": accounting,
    }
    if elapsed_active > PERF_ACTIVE_BUDGET_S:
        blockers.append({"stage": "budget",
                         "detail": "performance campaign exceeded the "
                                   "30-minute active budget"})
    if corrective_budget_s is not None and total_wall > corrective_budget_s:
        blockers.append({
            "stage": "budget",
            "detail": f"this corrective pass exceeded its explicitly "
                      f"approved {corrective_budget_s}s allowance by "
                      f"{total_wall - corrective_budget_s:.1f}s"})
    result["checks_total"] = sum(len(run["checks"]) for run in result["runs"])
    result["handoff"]["state"]["execution_status"] = (
        "completed" if not failures and not blockers else "failed")
    result["handoff"]["state"]["pause_point"] = (
        "V-PERF result ready for controller review and evidence-based "
        "profile adoption decision")
    result["handoff"]["state"]["blockers"] = blockers
    result["handoff"]["evidence"]["checks"] = [{
        "verification_id": "V-PERF",
        "command": f"python3 -B {SCRIPT_PATH} performance --evidence-dir "
                   f"{evidence_dir}",
        "result": ("PASS" if not failures and not blockers else
                   "FAIL" if failures else "BLOCKED"),
        "observed_at": datetime.now(timezone.utc).isoformat(),
        "revision": result["handoff"]["source"]["revision"],
        "source_tree_sha256": result.get("source_snapshot", {}).get(
            "tree_sha256"),
        "evidence": str(run_dir / "result.json")}]
    return finalize(result, run_dir, failures, blockers, evidence_dir)


# ---------------------------------------------------------------- dispatch

def run_case(name, args, evidence_dir):
    """Run one implemented case and reject every missing implementation explicitly.

    Accepts a registered case name, parsed driver arguments, and authorized evidence directory.
    Returns the owning case runner's documented process exit code.
    Raises DriverError with exit 3 for UNIMPLEMENTED cases or exit 2 for a missing runner.
    """
    spec = CASES[name]
    if not spec.implemented:
        raise DriverError(
            f"case '{name}' ({spec.verification_id}, task {spec.task}, "
            f"{spec.wave}) is explicitly UNIMPLEMENTED by A-SETUP; refusing "
            "to skip silently or to claim PASS; expected to be implemented "
            "by its owning task in a later wave", exit_code=3)
    if name == "baseline":
        return run_baseline_case(args, evidence_dir)
    if name == "parity":
        return run_parity_case(args, evidence_dir)
    if name == "metadata":
        return run_metadata_case(args, evidence_dir)
    if name == "extract":
        return run_extract_case(args, evidence_dir)
    if name == "inventory":
        return run_inventory_case(args, evidence_dir)
    if name == "status":
        return run_status_case(args, evidence_dir)
    if name == "batch":
        return run_batch_case(args, evidence_dir)
    if name == "performance":
        if getattr(args, "delivery_validation", False):
            return run_performance_delivery_case(args, evidence_dir)
        return run_performance_case(args, evidence_dir)
    raise DriverError(f"case '{name}' has no runner", exit_code=2)


def run_all_case(args, evidence_dir):
    """Run every required case; any missing or failing case fails the whole.

    Accepts parsed driver arguments and an authorized evidence root. After the
    baseline case completes, its fresh result and oracle paths are propagated
    to parity as explicit prerequisites when the caller supplied none; a
    non-PASS baseline is refused by parity's own validation, keeping the
    chain fail-closed. Returns 2 for blocked, 1 for failed, 0 for PASS.
    """
    per_case = []
    blocked = False
    failed = False
    for name in ALL_SEQUENCE:
        spec = CASES[name]
        if name == "parity" and not getattr(args, "baseline_result", None) \
                and not getattr(args, "baseline_oracle", None):
            propagated_result = LAST_BASELINE_PATHS.get("result")
            propagated_oracle = LAST_BASELINE_PATHS.get("oracle")
            if propagated_result and propagated_oracle and \
                    Path(propagated_result).is_file() and \
                    Path(propagated_oracle).is_file():
                args.baseline_result = propagated_result
                args.baseline_oracle = propagated_oracle
        try:
            code = run_case(name, args, evidence_dir)
            per_case.append({"case": name, "verification_id": spec.verification_id,
                             "status": "PASS" if code == 0 else "FAIL",
                             "exit_code": code})
            if code != 0:
                failed = True
        except DriverError as error:
            code = error.exit_code
            per_case.append({"case": name, "verification_id": spec.verification_id,
                             "status": "UNIMPLEMENTED" if code == 3 else "BLOCKED",
                             "exit_code": code, "detail": str(error)})
            failed = True
            if code == 2:
                blocked = True
    exit_code = 2 if blocked else (1 if failed else 0)
    aggregate_dir = mkdir_new(evidence_dir / f"all-{run_id()}")
    write_exclusive(aggregate_dir / "result.json",
                    json.dumps({"case": "all", "verification_ids": [
                        CASES[name].verification_id for name in ALL_SEQUENCE],
                        "baseline_propagation": {
                            "mode": "explicit" if getattr(args, "baseline_result", None)
                                    and args.baseline_result != LAST_BASELINE_PATHS.get("result")
                                    else ("propagated_fresh_baseline"
                                          if LAST_BASELINE_PATHS.get("result")
                                          and getattr(args, "baseline_result", None)
                                          == LAST_BASELINE_PATHS.get("result") else "none"),
                            "baseline_result": getattr(args, "baseline_result", None),
                            "baseline_oracle": getattr(args, "baseline_oracle", None),
                        },
                        "performance_mode": ("delivery-validation"
                                             if getattr(args, "delivery_validation", False)
                                             else "default-comparison"),
                        "per_case": per_case,
                        "exit_code": exit_code}, indent=2))
    print(f"all: {'PASS' if exit_code == 0 else 'FAIL'} "
          f"per_case={ {entry['case']: entry['status'] for entry in per_case} } "
          f"evidence={aggregate_dir / 'result.json'}")
    return exit_code


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Bounded verification driver for the settling-in feature "
                    "(Glassbox). See the module docstring for cases, permitted "
                    "trees, and exit codes.")
    parser.add_argument("case", choices=list(CASES) + ["all"],
                        help="verification case to run")
    parser.add_argument("--evidence-dir", required=True,
                        help="absolute path under "
                             "extensionalities/target/verification/settling-in-*")
    parser.add_argument("--binary", default=None,
                        help="explicit glassbox executable (implies --skip-build)")
    parser.add_argument("--baseline-result", default=None, metavar="RESULT_JSON",
                        help="parity case and `all`: explicit current-source "
                             "V-SETUP baseline result.json prerequisite")
    parser.add_argument("--baseline-oracle", default=None, metavar="ORACLE_JSON",
                        help="parity case and `all`: explicit fixture oracle "
                             "matching --baseline-result; both must be "
                             "supplied together")
    parser.add_argument("--delivery-validation", action="store_true",
                        help="performance case only: validate the delivered "
                             "production binary after profile adoption (one "
                             "canonical rebuild, accepted-evidence chain, "
                             "delivered remeasurement, approved-candidate "
                             "byte-output comparison; requires --binary)")
    parser.add_argument("--require-pinned-resources", action="store_true",
                        help="opt-in reproducibility guard: block any "
                             "functional case unless the authorized four-CPU "
                             "affinity, nice 10, and job-limit environment "
                             "is effective; by default the observed limits "
                             "are recorded as evidence and standard CI "
                             "runners proceed")
    parser.add_argument("--require-base-revision", action="store_true",
                        help="opt-in reproducibility guard: block the parity, "
                             "metadata, and extract cases unless the real "
                             "checkout revision equals the authorized base "
                             "revision recorded in each case handoff; by "
                             "default the actually observed revision is "
                             "recorded as evidence")
    parser.add_argument("--ci-network", action="store_true",
                        help="parity and metadata cases only (or `all`): "
                             "explicitly permit network access for the parity "
                             "Cargo builds and inventory-probe lock "
                             "generation on cold CI runners; an inherited "
                             "CARGO_NET_OFFLINE=true still forces offline; "
                             "default parity builds stay offline for "
                             "benchmark reproducibility")
    parser.add_argument("--skip-build", action="store_true",
                        help="do not build; requires --binary")
    parser.add_argument("--build-timeout", type=int, default=DEFAULT_BUILD_TIMEOUT,
                        help=f"cargo build timeout seconds (default {DEFAULT_BUILD_TIMEOUT})")
    parser.add_argument("--run-timeout", type=int, default=DEFAULT_RUN_TIMEOUT,
                        help=f"per-run child timeout seconds (default {DEFAULT_RUN_TIMEOUT})")
    parser.add_argument("--historical-reconstruction", action="store_true",
                        help="performance case only: reconstruct the Phase A "
                             "baseline read-only from revision 57e3afb and "
                             "measure shared workloads (charged separately "
                             "from the 30-minute active budget)")
    parser.add_argument("--perf-replicates", type=int, default=PERF_MIN_REPLICATES,
                        help=f"performance case only: fresh-process replicates "
                             f"per decisive scenario, minimum "
                             f"{PERF_MIN_REPLICATES} (default "
                             f"{PERF_MIN_REPLICATES})")
    parser.add_argument("--perf-candidates",
                        default=",".join(PERF_DEFAULT_CANDIDATES),
                        help="performance case only: comma-separated fixed "
                             "factor-only candidates among "
                             + ", ".join(PERF_CANDIDATE_FACTORS)
                             + " (at most two)")
    parser.add_argument("--reuse-candidate", action="append", default=None,
                        metavar="NAME=TARGET_DIR",
                        help="performance case only: reuse a previously "
                             "built candidate release binary from its "
                             "absolute isolated target directory instead of "
                             "rebuilding (repeatable; the prior measured "
                             "build duration stays in the retained prior "
                             "evidence)")
    parser.add_argument("--reuse-historical", default=None,
                        metavar="TARGET_DIR",
                        help="performance case only: with "
                             "--historical-reconstruction, reuse the "
                             "previously built historical 57e3afb binary "
                             "from its absolute target directory instead of "
                             "rebuilding identical sources; its sha256 must "
                             "match a supplied --prior-attempt-result record "
                             "with zero upstream drift")
    parser.add_argument("--prior-attempt-result", action="append",
                        default=None, metavar="RESULT_JSON",
                        help="performance case only: absolute path of a "
                             "retained earlier attempt's result.json for "
                             "cumulative cost accounting (repeatable); the "
                             "performance case fails closed without at "
                             "least one unless --allow-first-attempt is "
                             "explicitly declared")
    parser.add_argument("--allow-first-attempt", action="store_true",
                        help="performance case only: explicitly declare that "
                             "no earlier attempt exists, so no prior cost "
                             "can be accounted")
    parser.add_argument("--corrective-budget-s", type=float, default=None,
                        metavar="SECONDS",
                        help="performance case only: record this explicitly "
                             "approved corrective-pass allowance; "
                             "within_budget refers to this pass only, never "
                             "to the original campaign")
    args = parser.parse_args(argv)
    if args.delivery_validation and args.case not in ("performance", "all"):
        parser.error("--delivery-validation applies only to the performance "
                    "case (or `all`, which propagates it to performance)")
    if args.delivery_validation and not args.binary:
        parser.error("--delivery-validation requires --binary pointing at "
                    "the cached production target's release glassbox "
                    "executable")
    if args.ci_network and args.case not in ("parity", "metadata", "all"):
        parser.error("--ci-network applies only to the parity and metadata "
                    "cases (or `all`, which runs them)")
    if bool(args.baseline_result) != bool(args.baseline_oracle):
        parser.error("--baseline-result and --baseline-oracle must be "
                    "supplied together")
    if args.perf_replicates < PERF_MIN_REPLICATES:
        parser.error(f"--perf-replicates must be at least "
                     f"{PERF_MIN_REPLICATES}")
    if args.reuse_historical and not args.historical_reconstruction:
        parser.error("--reuse-historical requires "
                     "--historical-reconstruction")
    if args.reuse_historical:
        args.reuse_historical = perf_parse_reuse_historical(
            args.reuse_historical)
    if args.binary and not args.skip_build:
        args.skip_build = True
    if args.skip_build and not args.binary:
        parser.error("--skip-build requires --binary")
    try:
        evidence_dir = validate_evidence_dir(args.evidence_dir)
        ensure_dir(evidence_dir)
        if args.case == "all":
            return run_all_case(args, evidence_dir)
        return run_case(args.case, args, evidence_dir)
    except DriverError as error:
        print(f"verify-settling-in: {error}", file=sys.stderr)
        return error.exit_code
    except Exception as error:  # internal driver error; never claim PASS
        import traceback
        traceback.print_exc()
        print(f"verify-settling-in: internal error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

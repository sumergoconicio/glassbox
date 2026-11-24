#!/usr/bin/env python3
"""Module: Glassbox inspect/extract outputs v3 verification driver.
File: verify-inspect-extract-outputs.py.
Responsibility: Execute Frank's mixed-folder E2E workflow (V-IO-01..V-IO-09) and standalone docs checks (V-IO-10).

Bounded runtime verification driver for feature `20261123_inspect-extract-outputs`.

Modes:
  Default (full E2E):
    Verifies V-IO-01..V-IO-09 against freshly built release `glassbox` and `litx`
    binaries using isolated synthetic fixtures created under
    `extensionalities/sandbox/io-verify-<pid>-<nonce>/` (deleted on teardown
    after recording fixture digests) and retains structured evidence under
    `--evidence-dir`.

  --docs-only:
    Verifies V-IO-10 across `extensionalities/README.md`, `extensionalities/docs/*.md`,
    `extensionalities/crates/`, and `extensionalities/fixtures/`: relative Markdown
    link integrity, presence of all 23 `format.type` IDs, sampled fingerprint spec,
    `bodymatter:v1` consumer recipe, CLI flags, and zero sibling-project references.

Exit codes:
  0 = PASS
  1 = FAIL
  2 = BLOCKED or invalid invocation
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import gzip
import hashlib
import io
import json
import math
import os
from pathlib import Path
import platform
import re
import secrets
import shutil
import stat
import statistics
import struct
import subprocess
import sys
import tarfile
import time
from urllib.parse import unquote
import xml.etree.ElementTree as ET
import zipfile

SCRIPT_PATH = Path(__file__).resolve()
EXT_DIR = SCRIPT_PATH.parents[1]
REPO_ROOT = SCRIPT_PATH.parents[2]
VERIFICATION_ROOT = EXT_DIR / "target" / "verification"
SANDBOX_ROOT = EXT_DIR / "sandbox"
DEFAULT_EVIDENCE_BASE = VERIFICATION_ROOT / "inspect-extract-outputs"
DEFAULT_E2E_PREFIX = "run-e2e"
DEFAULT_DOCS_PREFIX = "run-docs"
BUILD_RECEIPT_FILENAME = ".glassbox-build-receipt.json"
HASH_BENCH_RECEIPT = DEFAULT_EVIDENCE_BASE / "hash-bench.json"

UPSTREAM_PROTECTED_PATHS: tuple[str, ...] = (
    "crates/",
    "Cargo.toml",
    "Cargo.lock",
    "README.md",
)

BENCH_FIXTURE_BYTES = 268_435_456  # 256 MiB
BENCH_HIGH_THRESHOLD_BYTES = 268_435_456  # 256 MiB
BENCH_LOW_THRESHOLD_BYTES = 134_217_728  # 128 MiB
BENCH_BUDGET_MS = 200.0
BENCH_MIN_PASSES = 5

DEFAULT_FULL_HASH_MAX_BYTES = 128 * 1024 * 1024  # 134_217_728 bytes (128 MiB)
SAMPLE_WINDOW_BYTES = 1024 * 1024  # 1 MiB
SAMPLED_SCHEME = "glassbox-sampled-sha256/v1"
SAMPLED_LATENCY_TARGET_MS = 50.0

NATIVE_PDF_FIXTURE_SHA256 = (
    "ce222c741783bc231892fd661cc9872cd48598e46cf230022bc1015a45a665ac"
)
NATIVE_PDF_PARSE_BASELINE_TEXT = (
    "\n--- Page 1 ---\nNative PDF evidence for bounded preview.\n\n\n--- Page 2 ---\n\n"
)
NATIVE_PDF_PARSE_BASELINE_SHA256 = (
    "be908f7562914a0991d54c9c3540987feda3aa591f87df03b3ffd6151aaf6080"
)

MARKER_LINE_RE = re.compile(r"^<!-- bodymatter:v1 (<.*/>) -->$")
MD_LINK_RE = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
FENCE_OPEN_RE = re.compile(r"^(`{3,}|~{3,})(.*)$")
ATX_HEADING_RE = re.compile(r"^\s{0,3}#{1,6}[ \t]+(.+?)(?:[ \t]+#+[ \t]*)?$")
HEX_COMMIT_RE = re.compile(r"^[0-9a-f]{7,40}$")
HEX_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")

ALL_23_FORMAT_TYPES: tuple[str, ...] = (
    "pdf",
    "office_docs",
    "presentations",
    "spreadsheets",
    "text",
    "tabular_data",
    "structured_data",
    "images",
    "vector_graphics",
    "ebooks",
    "comics",
    "audio",
    "video",
    "archives",
    "source_code",
    "web",
    "databases",
    "personal_info",
    "subtitles",
    "fonts",
    "3d_cad",
    "applications",
    "other",
)

FORGED_BODY_MARKER = '<!-- bodymatter:v1 <page number="99" origin="native"/> -->'
NEUTRALIZED_PREFIX = "&lt;!-- bodymatter:v1"
HOSTILE_SHEET_NAME = 'Q1--Draft <x attr="1">&'
HOSTILE_SHEET_NAME_2 = "Notes --> End"
LONG_XLSX_HEADER = (
    "This genuine source header goes well beyond sixty bytes and must remain intact"
)
RAW_HTML_PAYLOAD = "<script>alert(1)</script>"
MINIMAL_PSD_BYTES = (
    b"8BPS"
    + struct.pack(">H", 1)
    + b"\x00" * 6
    + struct.pack(">H", 3)
    + struct.pack(">II", 1, 1)
    + struct.pack(">HH", 8, 3)
    + struct.pack(">IIIH", 0, 0, 0, 0)
    + b"\x00\x00\x00"
)


class DriverError(Exception):
    """Operational or invocation error that blocks execution."""

    def __init__(self, message: str, exit_code: int = 2) -> None:
        super().__init__(message)
        self.exit_code = exit_code


def utc_now() -> str:
    """Return an ISO-8601 UTC timestamp string."""
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def forbidden_sibling_tokens() -> tuple[str, ...]:
    """Construct lowercase sibling-project guard tokens without embedding literals.

    Returns a tuple of ASCII tokens that must never appear in standalone
    Glassbox sources, public documentation, or fixtures.
    """
    encoded: tuple[tuple[int, ...], ...] = (
        (97, 115, 107, 99, 108, 97, 105, 114, 101),
        (97, 115, 107, 45, 99, 108, 97, 105, 114, 101),
        (99, 108, 97, 105, 114, 101, 45, 109, 111, 110, 111),
    )
    return tuple("".join(chr(code) for code in codes) for codes in encoded)


def resolve_repo_path(raw: str | Path) -> Path:
    """Resolve a path relative to the repository root when not absolute."""
    path = Path(raw)
    if not path.is_absolute():
        path = REPO_ROOT / path
    return Path(os.path.normpath(str(path)))


def _assert_no_symlink_components(path: Path, anchor: Path) -> None:
    """Reject symlinks at path or any ancestor component down to anchor.

    Parameters:
      path: Candidate path to inspect component by component.
      anchor: Ancestor directory where symlink inspection starts.

    Returns:
      None when no inspected component is a symlink.

    Failure modes:
      Raises DriverError if any component under anchor is a symlink.
    """
    anchor_norm = Path(os.path.normpath(str(anchor)))
    path_norm = Path(os.path.normpath(str(path)))
    try:
        rel_parts = path_norm.relative_to(anchor_norm).parts
    except ValueError:
        return
    probe = anchor_norm
    for part in rel_parts:
        probe = probe / part
        if probe.is_symlink():
            raise DriverError(
                f"evidence dir contains symlink component: {probe}"
            )


def validate_evidence_dir(
    raw: str | Path, permitted_root: Path = VERIFICATION_ROOT
) -> Path:
    """Validate that the requested evidence directory lives strictly under permitted_root without symlinks.

    Parameters:
      raw: Relative or absolute candidate evidence directory path.
      permitted_root: Containment root inside which the evidence directory must reside.

    Returns:
      Resolved Path inside permitted_root.

    Failure modes:
      Raises DriverError if raw escapes permitted_root, equals permitted_root,
      contains any symlink component, or cannot be resolved.
    """
    normalized = resolve_repo_path(raw)
    permitted = Path(os.path.normpath(str(permitted_root)))
    if normalized.is_symlink():
        raise DriverError(f"evidence dir must not be a symlink: {normalized}")
    _assert_no_symlink_components(normalized, permitted)
    try:
        permitted_resolved = permitted.resolve()
        resolved = normalized.resolve()
    except (OSError, RuntimeError) as exc:
        raise DriverError(
            f"evidence dir could not be resolved safely: {normalized} ({exc})"
        ) from exc
    _assert_no_symlink_components(normalized, permitted_resolved)
    try:
        rel_parts = resolved.relative_to(permitted_resolved).parts
    except ValueError as exc:
        raise DriverError(
            f"evidence dir must reside inside {permitted_resolved}, got {resolved}"
        ) from exc
    if resolved == permitted_resolved:
        raise DriverError(
            f"evidence dir must be a subdirectory inside {permitted_resolved}, not root itself"
        )
    probe = permitted_resolved
    for part in rel_parts:
        probe = probe / part
        if probe.is_symlink():
            raise DriverError(
                f"evidence dir contains unresolvable symlink component: {probe}"
            )
    return resolved


def default_evidence_dir_candidate(
    *,
    docs_only: bool,
    permitted_root: Path = VERIFICATION_ROOT,
) -> Path:
    """Generate a unique per-run default evidence directory path for E2E or docs-only runs.

    Parameters:
      docs_only: True for `--docs-only` (`run-docs-...`), False for E2E (`run-e2e-...`).
      permitted_root: Verification containment root under which the run directory lives.

    Returns:
      Non-colliding candidate Path with UTC timestamp, PID, and 8-hex-char random nonce.

    Failure modes:
      None under normal OS entropy availability.
    """
    prefix = DEFAULT_DOCS_PREFIX if docs_only else DEFAULT_E2E_PREFIX
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_name = f"{prefix}-{stamp}-{os.getpid()}-{secrets.token_hex(4)}"
    return permitted_root / "inspect-extract-outputs" / run_name


def check_evidence_dir_unused(evidence_dir: Path) -> None:
    """Fail closed if an explicit evidence directory is a symlink or already has prior outputs.

    Parameters:
      evidence_dir: Candidate evidence directory path.

    Returns:
      None when evidence_dir does not exist or is a real directory without `result.json` or `logs`.

    Failure modes:
      Raises DriverError if evidence_dir is a symlink, a non-directory path, or already
      contains `result.json` or `logs` (including dangling symlinks).
    """
    if evidence_dir.is_symlink():
        raise DriverError(f"evidence dir must not be a symlink: {evidence_dir}")
    if evidence_dir.exists():
        if not evidence_dir.is_dir():
            raise DriverError(
                f"evidence dir path exists and is not a directory: {evidence_dir}"
            )
        for reserved in ("result.json", "logs"):
            entry = evidence_dir / reserved
            if entry.exists() or entry.is_symlink():
                raise DriverError(
                    f"evidence dir already contains {reserved!r}; choose a fresh directory: {evidence_dir}"
                )


def resolve_evidence_dir(
    raw: str | Path | None,
    *,
    docs_only: bool,
    permitted_root: Path = VERIFICATION_ROOT,
) -> Path:
    """Resolve and validate the evidence directory with exclusive per-run defaults.

    Parameters:
      raw: Explicit `--evidence-dir` path, or None to generate an exclusive per-run path.
      docs_only: True for `--docs-only` mode, False for full E2E mode.
      permitted_root: Containment root inside which evidence directories must reside.

    Returns:
      Validated Path inside permitted_root.

    Failure modes:
      Raises DriverError if validation fails, a symlink component is present, or an
      explicit directory already contains `result.json` or `logs`.
    """
    if raw is None:
        chosen: str | Path = default_evidence_dir_candidate(
            docs_only=docs_only, permitted_root=permitted_root
        )
        return validate_evidence_dir(chosen, permitted_root=permitted_root)
    resolved = validate_evidence_dir(raw, permitted_root=permitted_root)
    check_evidence_dir_unused(resolved)
    return resolved


def prepare_evidence_dir(
    evidence_dir: Path,
    *,
    require_new_dir: bool = False,
    create_logs: bool = False,
) -> Path | None:
    """Allocate or verify an exclusive evidence directory and optional exclusive `logs/` subdir.

    Parameters:
      evidence_dir: Target evidence directory path.
      require_new_dir: When True, create `evidence_dir` strictly via `mkdir(exist_ok=False)`.
      create_logs: When True, create `evidence_dir / "logs"` via `mkdir(exist_ok=False)`.

    Returns:
      Path to the created `logs/` directory when `create_logs=True`, otherwise None.

    Failure modes:
      Raises DriverError if `evidence_dir`, its parent, or `logs/` is a symlink, already
      exists when exclusive creation is required, or already contains `result.json` or `logs`.
    """
    if evidence_dir.is_symlink():
        raise DriverError(f"evidence dir must not be a symlink: {evidence_dir}")
    parent = evidence_dir.parent
    if parent.is_symlink():
        raise DriverError(f"evidence dir parent must not be a symlink: {parent}")
    try:
        parent.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise DriverError(
            f"failed to prepare evidence parent directory {parent}: {exc}"
        ) from exc

    if require_new_dir:
        try:
            evidence_dir.mkdir(exist_ok=False)
        except OSError as exc:
            raise DriverError(
                f"failed to allocate exclusive evidence dir {evidence_dir}: {exc}"
            ) from exc
    else:
        check_evidence_dir_unused(evidence_dir)
        if not evidence_dir.exists():
            try:
                evidence_dir.mkdir(exist_ok=False)
            except OSError as exc:
                raise DriverError(
                    f"failed to create evidence dir {evidence_dir}: {exc}"
                ) from exc

    if not create_logs:
        return None

    log_dir = evidence_dir / "logs"
    if log_dir.is_symlink():
        raise DriverError(f"evidence logs dir must not be a symlink: {log_dir}")
    try:
        log_dir.mkdir(exist_ok=False)
    except OSError as exc:
        raise DriverError(
            f"failed to allocate exclusive logs dir {log_dir}: {exc}"
        ) from exc
    return log_dir


def write_exclusive_bytes(path: Path, data: bytes) -> None:
    """Create a new regular file exclusively without following symlinks and write data.

    Parameters:
      path: Destination file path that must not already exist or be a symlink.
      data: Byte payload to write with file mode 0o600.

    Returns:
      None on success.

    Failure modes:
      Raises DriverError if path already exists, is a symlink, or cannot be opened/written.
    """
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(str(path), flags, 0o600)
    except OSError as exc:
        raise DriverError(
            f"refused non-exclusive or unsafe write to {path}: {exc}"
        ) from exc
    try:
        handle = os.fdopen(fd, "wb")
    except OSError as exc:
        os.close(fd)
        raise DriverError(
            f"failed opening exclusive file descriptor for {path}: {exc}"
        ) from exc
    try:
        with handle:
            handle.write(data)
            handle.flush()
    except OSError as exc:
        raise DriverError(f"failed writing exclusive file {path}: {exc}") from exc


def build_receipt_path(target_dir: Path) -> Path:
    """Return the cryptographic build receipt path beside the release binaries.

    Parameters:
      target_dir: Cargo target directory containing `release/`.

    Returns:
      Path to `<target_dir>/release/.glassbox-build-receipt.json`.

    Failure modes:
      None.
    """
    return target_dir / "release" / BUILD_RECEIPT_FILENAME


def write_build_receipt(
    receipt_path: Path,
    *,
    source_manifest_sha256: str,
    glassbox_sha256: str,
    litx_sha256: str,
    rustc_version: str | None = None,
    cargo_version: str | None = None,
    built_at_utc: str | None = None,
) -> dict:
    """Atomically write `.glassbox-build-receipt.json` binding source manifest and release binaries.

    Parameters:
      receipt_path: Destination receipt file path inside `<target_dir>/release/`.
      source_manifest_sha256: Deterministic SHA-256 of the downstream Rust source manifest.
      glassbox_sha256: SHA-256 hex digest of the freshly built `glassbox` binary.
      litx_sha256: SHA-256 hex digest of the freshly built `litx` binary.
      rustc_version: Optional `rustc --version` string (auto-captured when None).
      cargo_version: Optional `cargo --version` string (auto-captured when None).
      built_at_utc: Optional ISO-8601 UTC timestamp string (auto-captured when None).

    Returns:
      The written build receipt dictionary.

    Failure modes:
      Raises DriverError if `receipt_path` or its parent is a symlink, or if atomic
      temp-file creation, fsync, or replacement fails.
    """
    if receipt_path.is_symlink():
        raise DriverError(f"build receipt path must not be a symlink: {receipt_path}")
    parent = receipt_path.parent
    if parent.is_symlink() or not parent.is_dir():
        raise DriverError(
            f"build receipt parent must be a real directory: {parent}"
        )
    receipt = {
        "schema": "glassbox-build-receipt/v1",
        "source_manifest_sha256": source_manifest_sha256,
        "glassbox_sha256": glassbox_sha256,
        "litx_sha256": litx_sha256,
        "rustc_version": (
            rustc_version
            if rustc_version is not None
            else capture_tool(["rustc", "--version"])
        ),
        "cargo_version": (
            cargo_version
            if cargo_version is not None
            else capture_tool(["cargo", "--version"])
        ),
        "built_at_utc": built_at_utc if built_at_utc is not None else utc_now(),
    }
    payload_bytes = (json.dumps(receipt, indent=2) + "\n").encode("utf-8")
    tmp_path = parent / (
        f".glassbox-build-receipt.{os.getpid()}.{secrets.token_hex(4)}.tmp"
    )
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(str(tmp_path), flags, 0o600)
    except OSError as exc:
        raise DriverError(
            f"failed to create temp build receipt {tmp_path}: {exc}"
        ) from exc
    try:
        try:
            handle = os.fdopen(fd, "wb")
        except OSError as exc:
            os.close(fd)
            raise DriverError(
                f"failed to open temp build receipt {tmp_path}: {exc}"
            ) from exc
        with handle:
            handle.write(payload_bytes)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp_path, receipt_path)
    except OSError as exc:
        try:
            if tmp_path.exists():
                tmp_path.unlink()
        except OSError:
            pass
        raise DriverError(
            f"failed to atomically replace build receipt {receipt_path}: {exc}"
        ) from exc
    return receipt


def _read_regular_file_nofollow(path: Path) -> tuple[str | None, str | None]:
    """Read a UTF-8 regular file without following symlinks.

    Parameters:
      path: File path to read.

    Returns:
      Tuple `(text, None)` on success, or `(None, error_message)` on failure.

    Failure modes:
      Never raises on filesystem or symlink errors; returns the error string instead.
    """
    if path.is_symlink():
        return None, f"refused symlinked build receipt: {path}"
    flags = (
        os.O_RDONLY
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    try:
        fd = os.open(str(path), flags)
    except FileNotFoundError:
        return None, f"missing build receipt: {path}"
    except OSError as exc:
        return None, f"cannot open build receipt {path}: {exc}"
    handle = None
    try:
        stat_info = os.fstat(fd)
        if not stat.S_ISREG(stat_info.st_mode):
            return None, f"build receipt is not a regular file: {path}"
        handle = os.fdopen(fd, "rb")
        with handle:
            raw = handle.read()
    except OSError as exc:
        return None, f"failed reading build receipt {path}: {exc}"
    finally:
        if handle is None:
            try:
                os.close(fd)
            except OSError:
                pass
    return raw.decode("utf-8", errors="replace"), None


def resolve_target_dir(raw: str | None) -> Path:
    """Resolve Cargo target directory from explicit flag, CARGO_TARGET_DIR, or default."""
    candidate = raw or os.environ.get("CARGO_TARGET_DIR") or str(EXT_DIR / "target")
    return resolve_repo_path(candidate)


def sha256_file(path: Path) -> str:
    """Stream full SHA-256 hex digest of a local file."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _iso_from_mtime_ns(mtime_ns: int) -> str:
    """Format a POSIX nanosecond mtime as an ISO-8601 UTC timestamp."""
    return datetime.fromtimestamp(mtime_ns / 1e9, tz=timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%S.%fZ"
    )


def compute_source_manifest(ext_dir: Path = EXT_DIR) -> dict:
    """Compute a deterministic SHA-256 manifest and newest mtime over downstream Rust sources."""
    candidates: list[Path] = []
    for root_file in (ext_dir / "Cargo.toml", ext_dir / "Cargo.lock"):
        if root_file.is_file() and not root_file.is_symlink():
            candidates.append(root_file)
    crates_dir = ext_dir / "crates"
    if crates_dir.is_dir():
        for entry in crates_dir.rglob("*"):
            if entry.is_file() and not entry.is_symlink():
                candidates.append(entry)

    sorted_files = sorted(
        candidates, key=lambda p: p.relative_to(ext_dir).as_posix()
    )
    manifest_hasher = hashlib.sha256()
    newest_mtime_ns: int | None = None
    newest_path: str | None = None
    for path in sorted_files:
        rel = path.relative_to(ext_dir).as_posix()
        digest = sha256_file(path)
        manifest_hasher.update(f"{rel}\0{digest}\n".encode("utf-8"))
        mtime_ns = path.stat().st_mtime_ns
        if newest_mtime_ns is None or mtime_ns > newest_mtime_ns:
            newest_mtime_ns = mtime_ns
            newest_path = rel

    return {
        "manifest_sha256": manifest_hasher.hexdigest(),
        "file_count": len(sorted_files),
        "newest_mtime_ns": newest_mtime_ns,
        "newest_mtime": (
            _iso_from_mtime_ns(newest_mtime_ns)
            if newest_mtime_ns is not None
            else None
        ),
        "newest_path": newest_path,
    }


def check_binary_freshness(
    ext_dir: Path,
    binaries: dict[str, Path],
    *,
    source_manifest: dict | None = None,
    receipt_path: Path | None = None,
) -> dict:
    """Verify release binaries against both source mtimes and `.glassbox-build-receipt.json`.

    Parameters:
      ext_dir: Downstream `extensionalities/` directory containing Cargo manifests and crates.
      binaries: Mapping of binary names (`glassbox`, `litx`) to release executable Paths.
      source_manifest: Optional precomputed manifest from `compute_source_manifest(ext_dir)`.
      receipt_path: Optional explicit receipt path (defaults to `<binary_parent>/.glassbox-build-receipt.json`).

    Returns:
      Structured provenance dictionary recording `source_manifest`, per-binary digests and mtimes,
      `mtime_fresh`, `receipt_valid`, `receipt_errors`, `receipt`, `all_fresh`, and `stale_binaries`.

    Failure modes:
      Returns `all_fresh=False` with concrete `stale_binaries` and/or `receipt_errors` whenever
      any binary is older than the newest source file, `.glassbox-build-receipt.json` is missing,
      symlinked, or malformed, or any recorded SHA-256 differs from live source/binary digests.
    """
    manifest = (
        source_manifest
        if source_manifest is not None
        else compute_source_manifest(ext_dir)
    )
    newest_src_ns = manifest.get("newest_mtime_ns")
    records: dict[str, dict] = {}
    stale_binaries: list[str] = []

    for name, bin_path in binaries.items():
        stat = bin_path.stat()
        mtime_ns = stat.st_mtime_ns
        is_fresh = newest_src_ns is not None and mtime_ns >= newest_src_ns
        lag_seconds = (
            round((newest_src_ns - mtime_ns) / 1e9, 3)
            if (newest_src_ns is not None and not is_fresh)
            else 0.0
        )
        if not is_fresh:
            stale_binaries.append(name)
        records[name] = {
            "path": str(bin_path),
            "sha256": sha256_file(bin_path),
            "bytes": stat.st_size,
            "mtime_ns": mtime_ns,
            "mtime": _iso_from_mtime_ns(mtime_ns),
            "fresh": is_fresh,
            "stale_lag_seconds": lag_seconds,
        }

    mtime_fresh = (
        manifest.get("file_count", 0) > 0
        and len(records) > 0
        and len(stale_binaries) == 0
    )

    resolved_receipt: Path | None = receipt_path
    if resolved_receipt is None and binaries:
        resolved_receipt = next(iter(binaries.values())).parent / BUILD_RECEIPT_FILENAME

    receipt_errors: list[str] = []
    parsed_receipt: dict | None = None
    if resolved_receipt is None:
        receipt_errors.append("no binaries supplied to locate build receipt")
    else:
        raw_text, read_err = _read_regular_file_nofollow(resolved_receipt)
        if read_err is not None or raw_text is None:
            receipt_errors.append(read_err or f"missing build receipt: {resolved_receipt}")
        else:
            parsed_receipt, json_err = safe_json_loads(raw_text)
            if json_err is not None or parsed_receipt is None:
                receipt_errors.append(f"invalid build receipt JSON: {json_err}")
            else:
                expected_manifest_sha = manifest.get("manifest_sha256")
                rec_manifest_sha = parsed_receipt.get("source_manifest_sha256")
                if not (
                    isinstance(rec_manifest_sha, str)
                    and HEX_SHA256_RE.fullmatch(rec_manifest_sha)
                    and rec_manifest_sha == expected_manifest_sha
                ):
                    receipt_errors.append(
                        f"source_manifest_sha256 mismatch (receipt={rec_manifest_sha!r}, live={expected_manifest_sha!r})"
                    )
                for req_bin in ("glassbox", "litx"):
                    field_name = f"{req_bin}_sha256"
                    rec_bin_sha = parsed_receipt.get(field_name)
                    live_bin_sha = (
                        records[req_bin]["sha256"] if req_bin in records else None
                    )
                    if not (
                        isinstance(rec_bin_sha, str)
                        and HEX_SHA256_RE.fullmatch(rec_bin_sha)
                        and live_bin_sha is not None
                        and rec_bin_sha == live_bin_sha
                    ):
                        receipt_errors.append(
                            f"{field_name} mismatch (receipt={rec_bin_sha!r}, live={live_bin_sha!r})"
                        )
                for meta_key in ("rustc_version", "cargo_version", "built_at_utc"):
                    val = parsed_receipt.get(meta_key)
                    if not isinstance(val, str) or not val.strip():
                        receipt_errors.append(
                            f"missing or empty build receipt field {meta_key!r}"
                        )

    receipt_valid = len(receipt_errors) == 0 and parsed_receipt is not None
    all_fresh = mtime_fresh and receipt_valid
    return {
        "source_manifest": manifest,
        "binaries": records,
        "mtime_fresh": mtime_fresh,
        "receipt_path": str(resolved_receipt) if resolved_receipt is not None else None,
        "receipt_valid": receipt_valid,
        "receipt_errors": receipt_errors,
        "receipt": parsed_receipt if receipt_valid else None,
        "all_fresh": all_fresh,
        "stale_binaries": stale_binaries,
    }


def _is_exact_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def _is_positive_number(value: object) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
        and float(value) > 0.0
    )


def validate_bench_receipt_payload(
    bench: object,
    *,
    repo_root: Path | None = REPO_ROOT,
    check_git_ancestry: bool = True,
    expected_default_bytes: int = DEFAULT_FULL_HASH_MAX_BYTES,
) -> list[dict]:
    """Strictly validate nested schema, types, median math, digests, commit lineage, and threshold decision."""
    if not isinstance(bench, dict):
        return [
            {
                "name": "hash_bench_receipt_valid_json",
                "ok": False,
                "detail": f"expected JSON object, got {type(bench).__name__}",
            }
        ]

    checks: list[dict] = []

    # 1. tested_commit format + git lineage check
    tested_commit = bench.get("tested_commit")
    commit_fmt_ok = isinstance(tested_commit, str) and bool(
        HEX_COMMIT_RE.fullmatch(tested_commit)
    )
    commit_ok = commit_fmt_ok
    commit_detail = f"tested_commit={tested_commit!r}"
    if commit_fmt_ok and check_git_ancestry and repo_root is not None:
        rev_res, rev_err = _run_git(
            ["rev-parse", "--verify", "--quiet", f"{tested_commit}^{{commit}}"],
            repo_root=repo_root,
        )
        head_res, head_err = _run_git(["rev-parse", "HEAD"], repo_root=repo_root)
        anc_res, anc_err = _run_git(
            ["merge-base", "--is-ancestor", tested_commit, "HEAD"],
            repo_root=repo_root,
        )
        if rev_res is None or head_res is None or anc_res is None:
            commit_ok = False
            commit_detail = (
                f"tested_commit={tested_commit}, git_error="
                f"{rev_err or head_err or anc_err}"
            )
        else:
            resolved_commit = rev_res.stdout.strip()
            head_commit = head_res.stdout.strip()
            is_ancestor = rev_res.returncode == 0 and anc_res.returncode == 0
            commit_ok = (
                rev_res.returncode == 0
                and head_res.returncode == 0
                and is_ancestor
            )
            commit_detail = (
                f"tested_commit={tested_commit}, resolved={resolved_commit}, "
                f"head={head_commit}, exact_match={resolved_commit == head_commit}, "
                f"is_ancestor={is_ancestor}"
            )
    checks.append(
        {
            "name": "tested_commit_valid_and_in_lineage",
            "ok": commit_ok,
            "detail": commit_detail,
        }
    )

    benchmark = bench.get("benchmark")
    bench_is_dict = isinstance(benchmark, dict)
    bench_dict = benchmark if bench_is_dict else {}

    # 2. benchmark.fixture_bytes == 268_435_456 (exact int, not bool)
    fixture_bytes = bench_dict.get("fixture_bytes")
    fixture_bytes_ok = (
        bench_is_dict
        and _is_exact_int(fixture_bytes)
        and fixture_bytes == BENCH_FIXTURE_BYTES
    )
    checks.append(
        {
            "name": "benchmark_fixture_bytes_256_mib",
            "ok": fixture_bytes_ok,
            "detail": f"fixture_bytes={fixture_bytes!r} (type={type(fixture_bytes).__name__})",
        }
    )

    # 3. benchmark.passes_ms (>= 5 positive numbers, not bool) & median_ms within 1e-3 ms
    passes_ms = bench_dict.get("passes_ms")
    median_ms = bench_dict.get("median_ms")
    passes_valid = (
        bench_is_dict
        and isinstance(passes_ms, list)
        and len(passes_ms) >= BENCH_MIN_PASSES
        and all(_is_positive_number(v) for v in passes_ms)
    )
    median_valid = bench_is_dict and _is_positive_number(median_ms)
    computed_median: float | None = None
    median_matches = False
    if passes_valid and median_valid:
        computed_median = float(statistics.median(passes_ms))
        median_matches = abs(float(median_ms) - computed_median) <= 1e-3
    checks.append(
        {
            "name": "benchmark_passes_and_median_consistent",
            "ok": passes_valid and median_valid and median_matches,
            "detail": (
                f"passes_count={len(passes_ms) if isinstance(passes_ms, list) else 'invalid'}, "
                f"median_ms={median_ms!r}, computed_median={computed_median}"
            ),
        }
    )

    # 4. benchmark.sha256sum_parity is True & equal 64-char lowercase hex digests
    parity_flag = bench_dict.get("sha256sum_parity")
    sha_rs = bench_dict.get("sha256_hashing_rs")
    sha_sys = bench_dict.get("sha256_system_sha256sum")
    sha_rs_ok = isinstance(sha_rs, str) and bool(HEX_SHA256_RE.fullmatch(sha_rs))
    sha_sys_ok = isinstance(sha_sys, str) and bool(HEX_SHA256_RE.fullmatch(sha_sys))
    parity_ok = (
        bench_is_dict
        and parity_flag is True
        and sha_rs_ok
        and sha_sys_ok
        and sha_rs == sha_sys
    )
    checks.append(
        {
            "name": "sha256sum_parity_and_digests_valid",
            "ok": parity_ok,
            "detail": (
                f"sha256sum_parity={parity_flag!r}, "
                f"sha_rs_valid={sha_rs_ok}, sha_sys_valid={sha_sys_ok}, "
                f"equal={sha_rs == sha_sys if (sha_rs_ok and sha_sys_ok) else False}"
            ),
        }
    )

    # 5. threshold_decision.chosen_bytes consistent with median_ms and DEFAULT_FULL_HASH_MAX_BYTES
    decision = bench.get("threshold_decision")
    decision_is_dict = isinstance(decision, dict)
    decision_dict = decision if decision_is_dict else {}
    chosen_bytes = decision_dict.get("chosen_bytes")
    chosen_const = decision_dict.get("chosen_constant_name")
    expected_from_median: int | None = None
    decision_ok = False
    if decision_is_dict and median_valid and _is_exact_int(chosen_bytes):
        expected_from_median = (
            BENCH_HIGH_THRESHOLD_BYTES
            if float(median_ms) <= BENCH_BUDGET_MS
            else BENCH_LOW_THRESHOLD_BYTES
        )
        decision_ok = (
            chosen_bytes == expected_from_median
            and chosen_bytes == expected_default_bytes
            and chosen_const == "DEFAULT_FULL_HASH_MAX_BYTES"
        )
    checks.append(
        {
            "name": "threshold_decision_consistent_with_median_and_constant",
            "ok": decision_ok,
            "detail": (
                f"chosen_bytes={chosen_bytes!r}, expected_from_median={expected_from_median}, "
                f"default_const={expected_default_bytes}, constant_name={chosen_const!r}, "
                f"median_ms={median_ms!r}"
            ),
        }
    )

    # 6. scratch_cleanup.removed_after_recording is True
    cleanup = bench.get("scratch_cleanup")
    cleanup_ok = (
        isinstance(cleanup, dict) and cleanup.get("removed_after_recording") is True
    )
    checks.append(
        {
            "name": "bench_scratch_removed",
            "ok": cleanup_ok,
            "detail": f"removed_after_recording={cleanup.get('removed_after_recording') if isinstance(cleanup, dict) else None!r}",
        }
    )

    return checks


def validate_bench_receipt_file(
    receipt_path: Path,
    *,
    repo_root: Path | None = REPO_ROOT,
    check_git_ancestry: bool = True,
    expected_default_bytes: int = DEFAULT_FULL_HASH_MAX_BYTES,
) -> list[dict]:
    """Read and strictly validate a hash benchmark receipt JSON file."""
    if not receipt_path.is_file():
        return [
            {
                "name": "hash_bench_receipt_exists",
                "ok": False,
                "detail": f"missing receipt: {receipt_path}",
            }
        ]
    try:
        rel_display = str(receipt_path.relative_to(REPO_ROOT))
    except ValueError:
        rel_display = str(receipt_path)

    parsed, err = safe_json_loads(
        receipt_path.read_text(encoding="utf-8", errors="replace")
    )
    if err is not None or parsed is None:
        return [
            {
                "name": "hash_bench_receipt_exists",
                "ok": True,
                "detail": rel_display,
            },
            {
                "name": "hash_bench_receipt_valid_json",
                "ok": False,
                "detail": f"{rel_display}: {err}",
            },
        ]

    return [
        {
            "name": "hash_bench_receipt_exists",
            "ok": True,
            "detail": rel_display,
        },
        *validate_bench_receipt_payload(
            parsed,
            repo_root=repo_root,
            check_git_ancestry=check_git_ancestry,
            expected_default_bytes=expected_default_bytes,
        ),
    ]


def compute_sampled_fingerprint_oracle(path: Path) -> str:
    """Compute independent `glassbox-sampled-sha256/v1` hex digest per design.md §4.2."""
    size = path.stat().st_size
    if size < 3 * SAMPLE_WINDOW_BYTES:
        raise ValueError(f"file too small for sampled fingerprint oracle: {size}")
    offsets = (
        0,
        (size - SAMPLE_WINDOW_BYTES) // 2,
        size - SAMPLE_WINDOW_BYTES,
    )
    hasher = hashlib.sha256()
    hasher.update(SAMPLED_SCHEME.encode("ascii"))
    hasher.update(b"\x00")
    hasher.update(struct.pack(">Q", size))
    with path.open("rb") as handle:
        for offset in offsets:
            handle.seek(offset)
            data = handle.read(SAMPLE_WINDOW_BYTES)
            if len(data) != SAMPLE_WINDOW_BYTES:
                raise ValueError(
                    f"short sample read at offset {offset}: {len(data)} bytes"
                )
            hasher.update(struct.pack(">Q", offset))
            hasher.update(struct.pack(">Q", len(data)))
            hasher.update(data)
    return hasher.hexdigest()


def parse_bodymatter_markers(markdown: str) -> list[dict]:
    """Parse every `^<!-- bodymatter:v1 (<.*/>) -->$` line with strict XML 1.0 parser.

    Raises ValueError if any matched marker contains `--` inside its HTML comment
    payload or fails `xml.etree.ElementTree.fromstring`.
    """
    markers: list[dict] = []
    for line_no, raw_line in enumerate(markdown.splitlines(), start=1):
        match = MARKER_LINE_RE.match(raw_line)
        if not match:
            continue
        comment_body = raw_line[len("<!-- ") : -len(" -->")]
        if "--" in comment_body:
            raise ValueError(
                f"line {line_no}: illegal `--` inside HTML comment payload: {raw_line!r}"
            )
        xml_fragment = match.group(1)
        try:
            element = ET.fromstring(xml_fragment)
        except ET.ParseError as exc:
            raise ValueError(
                f"line {line_no}: strict XML parse failed on {xml_fragment!r}: {exc}"
            ) from exc
        if len(list(element)) != 0 or (element.text and element.text.strip()):
            raise ValueError(
                f"line {line_no}: marker element must be self-closing empty element: {xml_fragment!r}"
            )
        markers.append(
            {
                "line": line_no,
                "tag": element.tag,
                "attrib": dict(element.attrib),
                "raw": raw_line,
            }
        )
    return markers


def strip_bodymatter_blocks(markdown: str) -> list[str]:
    """Strip bodymatter:v1 marker lines and split Markdown into non-empty content blocks."""
    normalized = markdown.replace("\r\n", "\n").strip("\n")
    if not normalized:
        return []
    blocks: list[str] = []
    for raw_block in normalized.split("\n\n"):
        block = raw_block.strip("\n")
        if not block:
            continue
        if MARKER_LINE_RE.match(block):
            continue
        blocks.append(block)
    return blocks


def check_marker_strip_equivalence(
    with_markers: str, without_markers: str
) -> tuple[bool, dict]:
    """Verify `--no-markers` output equals default output after stripping bodymatter markers."""
    details: dict = {}
    legacy_headings = [
        heading
        for heading in ("# Glassbox extraction", "## Unit ")
        if heading in with_markers or heading in without_markers
    ]
    details["legacy_headings_found"] = legacy_headings

    try:
        with_marker_matches = parse_bodymatter_markers(with_markers)
        details["with_marker_matches_count"] = len(with_marker_matches)
    except ValueError as exc:
        with_marker_matches = []
        details["with_marker_matches_count"] = 0
        details["with_markers_parse_error"] = str(exc)

    try:
        no_marker_matches = parse_bodymatter_markers(without_markers)
        details["no_marker_matches_count"] = len(no_marker_matches)
    except ValueError as exc:
        no_marker_matches = []
        details["no_marker_matches_count"] = -1
        details["no_markers_parse_error"] = str(exc)

    details["no_marker_contains_prefix"] = "<!-- bodymatter:v1" in without_markers
    stripped_default = strip_bodymatter_blocks(with_markers)
    stripped_nomarkers = strip_bodymatter_blocks(without_markers)
    details["default_block_count"] = len(stripped_default)
    details["no_markers_block_count"] = len(stripped_nomarkers)
    blocks_equal = stripped_default == stripped_nomarkers
    details["blocks_equal"] = blocks_equal
    ok = (
        not legacy_headings
        and "with_markers_parse_error" not in details
        and "no_markers_parse_error" not in details
        and len(with_marker_matches) > 0
        and len(stripped_default) > 0
        and not no_marker_matches
        and not details["no_marker_contains_prefix"]
        and blocks_equal
    )
    return ok, details


def _strip_fenced_code_blocks(markdown: str) -> str:
    """Remove fenced code blocks while preserving line numbers and matching fence lengths."""
    out_lines: list[str] = []
    in_fence = False
    fence_char = ""
    fence_len = 0
    for line in markdown.splitlines():
        stripped = line.lstrip()
        if not in_fence:
            open_match = FENCE_OPEN_RE.match(stripped)
            if open_match:
                fence = open_match.group(1)
                info = open_match.group(2)
                if not (fence[0] == "`" and "`" in info):
                    in_fence = True
                    fence_char = fence[0]
                    fence_len = len(fence)
                    out_lines.append("")
                    continue
            out_lines.append(line)
            continue
        close_re = rf"^{re.escape(fence_char)}{{{fence_len},}}[ \t]*$"
        if re.match(close_re, stripped):
            in_fence = False
            fence_char = ""
            fence_len = 0
        out_lines.append("")
    return "\n".join(out_lines)


def _strip_fenced_and_inline_code(markdown: str) -> str:
    """Remove fenced code blocks and inline code spans before Markdown link extraction."""
    without_fences = _strip_fenced_code_blocks(markdown)
    return "\n".join(
        re.sub(r"(`+)(?!`)(?:(?!\1).)*?\1(?!`)", "", line)
        for line in without_fences.splitlines()
    )


def _github_heading_slug(heading_text: str) -> str:
    """Compute GitHub-style heading anchor slug from raw Markdown heading text."""
    text = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", heading_text)
    text = re.sub(r"<[^>]+>", "", text)
    text = text.replace("`", "").strip().lower()
    cleaned_chars = [ch for ch in text if ch.isalnum() or ch in ("_", "-", " ")]
    return "".join(cleaned_chars).replace(" ", "-")


def extract_markdown_anchors(markdown: str) -> set[str]:
    """Extract GitHub-style heading anchor slugs, ignoring fenced code blocks and handling duplicates."""
    cleaned = _strip_fenced_code_blocks(markdown)
    anchors: set[str] = set()
    counts: dict[str, int] = {}
    for line in cleaned.splitlines():
        match = ATX_HEADING_RE.match(line)
        if not match:
            continue
        base = _github_heading_slug(match.group(1))
        if not base:
            continue
        if base not in counts and base not in anchors:
            counts[base] = 1
            anchors.add(base)
        else:
            suffix = counts.get(base, 1)
            candidate = f"{base}-{suffix}"
            while candidate in anchors:
                suffix += 1
                candidate = f"{base}-{suffix}"
            counts[base] = suffix + 1
            anchors.add(candidate)
    return anchors


def validate_docs_tree(
    ext_dir: Path, forbidden_tokens: tuple[str, ...] | None = None
) -> dict:
    """Validate V-IO-10 documentation links, required v3/bodymatter tokens, and standalone hygiene."""
    tokens = (
        forbidden_tokens if forbidden_tokens is not None else forbidden_sibling_tokens()
    )
    doc_files: list[Path] = []
    readme = ext_dir / "README.md"
    if readme.is_file():
        doc_files.append(readme)
    docs_dir = ext_dir / "docs"
    if docs_dir.is_dir():
        doc_files.extend(sorted(docs_dir.glob("*.md")))

    broken_links: list[dict] = []
    links_checked = 0
    combined_doc_text_parts: list[str] = []
    anchors_cache: dict[Path, set[str]] = {}

    def get_anchors(target_md: Path) -> set[str]:
        key = target_md.resolve()
        if key not in anchors_cache:
            target_text = target_md.read_text(encoding="utf-8", errors="replace")
            anchors_cache[key] = extract_markdown_anchors(target_text)
        return anchors_cache[key]

    for doc_path in doc_files:
        text = doc_path.read_text(encoding="utf-8", errors="replace")
        combined_doc_text_parts.append(text)
        anchors_cache[doc_path.resolve()] = extract_markdown_anchors(text)
        cleaned = _strip_fenced_and_inline_code(text)
        for line_no, line in enumerate(cleaned.splitlines(), start=1):
            for match in MD_LINK_RE.finditer(line):
                raw_target = match.group(1).strip()
                if not raw_target:
                    continue
                if raw_target.startswith("<") and raw_target.endswith(">"):
                    raw_target = raw_target[1:-1].strip()
                # Strip optional link title after whitespace
                parts = raw_target.split()
                if not parts:
                    continue
                target_part = parts[0]
                if target_part.startswith(("http://", "https://", "mailto:")):
                    continue
                if not target_part.startswith("#") and ":" in target_part.split("/")[0]:
                    continue
                if "#" in target_part:
                    file_part, fragment = target_part.split("#", 1)
                else:
                    file_part, fragment = target_part, ""

                if not file_part:
                    if not fragment:
                        continue
                    links_checked += 1
                    decoded_fragment = unquote(fragment)
                    if decoded_fragment not in get_anchors(doc_path):
                        broken_links.append(
                            {
                                "source": str(doc_path.relative_to(ext_dir)),
                                "line": line_no,
                                "target": target_part,
                                "resolved": str(doc_path),
                                "reason": f"missing anchor #{decoded_fragment}",
                            }
                        )
                    continue

                links_checked += 1
                resolved = Path(os.path.normpath(str(doc_path.parent / file_part)))
                if not resolved.exists():
                    broken_links.append(
                        {
                            "source": str(doc_path.relative_to(ext_dir)),
                            "line": line_no,
                            "target": target_part,
                            "resolved": str(resolved),
                            "reason": "missing target path",
                        }
                    )
                elif (
                    fragment
                    and resolved.is_file()
                    and resolved.suffix.lower() in (".md", ".markdown")
                ):
                    decoded_fragment = unquote(fragment)
                    if decoded_fragment not in get_anchors(resolved):
                        broken_links.append(
                            {
                                "source": str(doc_path.relative_to(ext_dir)),
                                "line": line_no,
                                "target": target_part,
                                "resolved": str(resolved),
                                "reason": f"missing anchor #{decoded_fragment}",
                            }
                        )

    combined_docs = "\n".join(combined_doc_text_parts)
    missing_format_types = [
        fmt
        for fmt in ALL_23_FORMAT_TYPES
        if not re.search(rf"(?<![a-z0-9_]){re.escape(fmt)}(?![a-z0-9_])", combined_docs)
    ]

    required_tokens: dict[str, bool] = {
        "sampled_scheme": SAMPLED_SCHEME in combined_docs,
        "bodymatter_v1": "bodymatter:v1" in combined_docs,
        "bodymatter_regex_or_recipe": (
            "^<!-- bodymatter:v1 (<.*/>) -->$" in combined_docs
            or ("bodymatter:v1" in combined_docs and "XML" in combined_docs)
        ),
        "flag_full_hash": "--full-hash" in combined_docs,
        "flag_verbose": "--verbose" in combined_docs,
        "flag_no_markers": "--no-markers" in combined_docs,
        "schema_version_3": bool(
            re.search(r'schema_version["`]?\s*:\s*`?3\b', combined_docs)
            or "schema v3" in combined_docs.lower()
        ),
    }
    missing_tokens = [key for key, present in required_tokens.items() if not present]

    # Standalone check across crates/, docs/, README.md, fixtures/
    scan_roots: list[Path] = [
        ext_dir / "README.md",
        ext_dir / "docs",
        ext_dir / "crates",
        ext_dir / "fixtures",
    ]
    standalone_matches: list[dict] = []
    scanned_files = 0
    lower_tokens = [tok.lower().encode("ascii") for tok in tokens if tok]
    for root in scan_roots:
        if not root.exists():
            continue
        candidates = [root] if root.is_file() else sorted(root.rglob("*"))
        for candidate in candidates:
            if not candidate.is_file() or candidate.is_symlink():
                continue
            scanned_files += 1
            raw_bytes = candidate.read_bytes().lower()
            for token_bytes in lower_tokens:
                if token_bytes in raw_bytes:
                    standalone_matches.append(
                        {
                            "file": str(candidate.relative_to(ext_dir)),
                            "token_length": len(token_bytes),
                        }
                    )

    checks = [
        {
            "name": "doc_files_present",
            "ok": len(doc_files) > 0,
            "detail": f"checked {len(doc_files)} markdown files",
        },
        {
            "name": "relative_markdown_links",
            "ok": links_checked > 0 and len(broken_links) == 0,
            "detail": f"checked {links_checked} links, broken={len(broken_links)}",
        },
        {
            "name": "all_23_format_types_documented",
            "ok": len(missing_format_types) == 0,
            "detail": (
                "all 23 format.type IDs present"
                if not missing_format_types
                else f"missing: {missing_format_types}"
            ),
        },
        {
            "name": "required_v3_and_bodymatter_tokens",
            "ok": len(missing_tokens) == 0,
            "detail": (
                "all required contract tokens present"
                if not missing_tokens
                else f"missing: {missing_tokens}"
            ),
        },
        {
            "name": "standalone_zero_sibling_refs",
            "ok": len(standalone_matches) == 0,
            "detail": f"scanned {scanned_files} files across crates/docs/README/fixtures, matches={len(standalone_matches)}",
        },
    ]
    status = "PASS" if all(c["ok"] for c in checks) else "FAIL"
    return {
        "ver_id": "V-IO-10",
        "status": status,
        "checks": checks,
        "doc_files": [str(p.relative_to(ext_dir)) for p in doc_files],
        "links_checked": links_checked,
        "broken_links": broken_links,
        "missing_format_types": missing_format_types,
        "missing_tokens": missing_tokens,
        "standalone_matches": standalone_matches,
    }


# ------------------------------------------------------------ Fixture builders


def _pdf_escape(text: str) -> str:
    return text.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def build_synthetic_pdf(
    pages: list[str],
    page_labels_dict: str | None = None,
    info_entries: dict[str, str] | None = None,
) -> bytes:
    """Build a valid deterministic PDF 1.7 file with optional /PageLabels and /Info."""
    catalog = (
        f"<< /Type /Catalog /Pages 2 0 R /PageLabels {page_labels_dict} >>"
        if page_labels_dict
        else "<< /Type /Catalog /Pages 2 0 R >>"
    )
    objects: list[bytes] = [
        catalog.encode("latin-1"),
        b"",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]
    kids: list[int] = []
    for text in pages:
        escaped = _pdf_escape(text)
        content = f"BT /F1 14 Tf 50 700 Td ({escaped}) Tj ET".encode("latin-1")
        stream_obj = (
            f"<< /Length {len(content)} >>\nstream\n".encode("latin-1")
            + content
            + b"\nendstream"
        )
        objects.append(stream_obj)
        content_no = len(objects)
        page_obj = (
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            f"/Resources << /Font << /F1 3 0 R >> >> /Contents {content_no} 0 R >>"
        ).encode("latin-1")
        objects.append(page_obj)
        kids.append(len(objects))

    kids_str = " ".join(f"{n} 0 R" for n in kids)
    objects[1] = f"<< /Type /Pages /Kids [{kids_str}] /Count {len(kids)} >>".encode(
        "latin-1"
    )

    info_no: int | None = None
    if info_entries:
        parts = [f"/{k} ({_pdf_escape(v)})" for k, v in info_entries.items()]
        objects.append(("<< " + " ".join(parts) + " >>").encode("latin-1"))
        info_no = len(objects)

    data = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for idx, obj in enumerate(objects, start=1):
        offsets.append(len(data))
        data.extend(f"{idx} 0 obj\n".encode("latin-1"))
        data.extend(obj)
        data.extend(b"\nendobj\n")

    xref_pos = len(data)
    data.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode("latin-1"))
    for offset in offsets[1:]:
        data.extend(f"{offset:010} 00000 n \n".encode("latin-1"))
    info_ref = f" /Info {info_no} 0 R" if info_no is not None else ""
    data.extend(
        f"trailer\n<< /Size {len(offsets)} /Root 1 0 R{info_ref} >>\n"
        f"startxref\n{xref_pos}\n%%EOF\n".encode("latin-1")
    )
    return bytes(data)


def _xml_attr_escape(text: str) -> str:
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def write_zip_entries(path: Path, entries: list[tuple[str, str]]) -> None:
    """Create a stored ZIP archive with UTF-8 XML entries."""
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as zf:
        for name, content in entries:
            zf.writestr(name, content.encode("utf-8"))


def build_synthetic_xlsx(path: Path) -> None:
    """Build Frank's 3-sheet synthetic XLSX with hostile names, forged marker, raw HTML, and long row-1 + numeric header."""
    ns_s = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
    ns_r = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    ns_pkg = "http://schemas.openxmlformats.org/package/2006/relationships"
    ns_ct = "http://schemas.openxmlformats.org/package/2006/content-types"

    forged_xml = _xml_attr_escape(FORGED_BODY_MARKER)
    raw_html_xml = _xml_attr_escape(RAW_HTML_PAYLOAD)
    long_header_xml = _xml_attr_escape(LONG_XLSX_HEADER)
    write_zip_entries(
        path,
        [
            (
                "[Content_Types].xml",
                f'<Types xmlns="{ns_ct}">'
                '<Override PartName="/xl/workbook.xml" '
                'ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
                "</Types>",
            ),
            (
                "xl/workbook.xml",
                f'<workbook xmlns="{ns_s}" xmlns:r="{ns_r}"><sheets>'
                '<sheet name="Summary" sheetId="1" r:id="rId1"/>'
                f'<sheet name="{_xml_attr_escape(HOSTILE_SHEET_NAME)}" sheetId="2" r:id="rId2"/>'
                f'<sheet name="{_xml_attr_escape(HOSTILE_SHEET_NAME_2)}" sheetId="3" r:id="rId3"/>'
                "</sheets></workbook>",
            ),
            (
                "xl/_rels/workbook.xml.rels",
                f'<Relationships xmlns="{ns_pkg}">'
                f'<Relationship Id="rId1" Type="{ns_r}/worksheet" Target="worksheets/sheet1.xml"/>'
                f'<Relationship Id="rId2" Type="{ns_r}/worksheet" Target="worksheets/sheet2.xml"/>'
                f'<Relationship Id="rId3" Type="{ns_r}/worksheet" Target="worksheets/sheet3.xml"/>'
                "</Relationships>",
            ),
            (
                "xl/worksheets/sheet1.xml",
                f'<worksheet xmlns="{ns_s}"><sheetData>'
                '<row r="1">'
                '<c r="A1" t="inlineStr"><is><t>Item</t></is></c>'
                '<c r="B1" t="inlineStr"><is><t>Qty</t></is></c>'
                "</row>"
                '<row r="2">'
                '<c r="A2" t="inlineStr"><is><t>Widget</t></is></c>'
                '<c r="B2"><v>42</v></c>'
                "</row>"
                "</sheetData></worksheet>",
            ),
            (
                "xl/worksheets/sheet2.xml",
                f'<worksheet xmlns="{ns_s}"><sheetData>'
                '<row r="1"><c r="A1" t="inlineStr"><is><t>Note</t></is></c></row>'
                f'<row r="2"><c r="A2" t="inlineStr"><is><t>{forged_xml}</t></is></c></row>'
                f'<row r="3"><c r="A3" t="inlineStr"><is><t>{raw_html_xml}</t></is></c></row>'
                "</sheetData></worksheet>",
            ),
            (
                "xl/worksheets/sheet3.xml",
                f'<worksheet xmlns="{ns_s}"><sheetData>'
                '<row r="1">'
                f'<c r="A1" t="inlineStr"><is><t>{long_header_xml}</t></is></c>'
                '<c r="B1"><v>2026</v></c>'
                "</row>"
                '<row r="2">'
                '<c r="A2" t="inlineStr"><is><t>Reviewed</t></is></c>'
                '<c r="B2"><v>7</v></c>'
                "</row>"
                "</sheetData></worksheet>",
            ),
        ],
    )


def build_synthetic_docx(path: Path) -> None:
    """Build Frank's synthetic DOCX with 2 tables, 2 explicit page breaks, ignored soft break, forged marker, and raw HTML."""
    ns_w = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
    ns_ct = "http://schemas.openxmlformats.org/package/2006/content-types"
    forged_xml = _xml_attr_escape(FORGED_BODY_MARKER)
    raw_html_xml = _xml_attr_escape(RAW_HTML_PAYLOAD)

    write_zip_entries(
        path,
        [
            (
                "[Content_Types].xml",
                f'<Types xmlns="{ns_ct}">'
                '<Override PartName="/word/document.xml" '
                'ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
                "</Types>",
            ),
            (
                "word/document.xml",
                f'<w:document xmlns:w="{ns_w}"><w:body>'
                "<w:p><w:r><w:t>Intro paragraph with </w:t><w:lastRenderedPageBreak/><w:t>soft break.</w:t></w:r></w:p>"
                "<w:tbl><w:tr>"
                "<w:tc><w:p><w:r><w:t>ColA</w:t></w:r></w:p></w:tc>"
                "<w:tc><w:p><w:r><w:t>ColB</w:t></w:r></w:p></w:tc>"
                "</w:tr></w:tbl>"
                '<w:p><w:r><w:t>Before explicit break.</w:t><w:br w:type="page"/><w:t>After explicit break.</w:t></w:r></w:p>'
                "<w:tbl><w:tr>"
                "<w:tc><w:p><w:r><w:t>SecA</w:t></w:r></w:p></w:tc>"
                "<w:tc><w:p><w:r><w:t>SecB</w:t></w:r></w:p></w:tc>"
                "</w:tr></w:tbl>"
                '<w:p><w:r><w:t>Before second break.</w:t><w:br w:type="page"/><w:t>After second break.</w:t></w:r></w:p>'
                f"<w:p><w:r><w:t>Forged cell: {forged_xml} and {raw_html_xml}</w:t></w:r></w:p>"
                "</w:body></w:document>",
            ),
        ],
    )


def build_sparse_media_file(path: Path, total_bytes: int, seed_tag: bytes) -> None:
    """Create a sparse MP4-header file of exact size `total_bytes` with realistic head/mid/tail bytes."""
    mp4_header = (
        b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2"
        b"\x00\x00\x00\x08free"
    )
    head_block = (mp4_header + seed_tag + b"HEAD" * 256).ljust(4096, b"\x11")
    mid_block = (b"MID:" + seed_tag + b"\x22" * 512).ljust(4096, b"\x22")
    tail_block = (b"TAIL:" + seed_tag + b"\x33" * 512).ljust(4096, b"\x33")

    with path.open("wb") as handle:
        handle.truncate(total_bytes)
        handle.seek(0)
        handle.write(head_block)
        mid_offset = (total_bytes - SAMPLE_WINDOW_BYTES) // 2
        handle.seek(mid_offset)
        handle.write(mid_block)
        handle.seek(total_bytes - len(tail_block))
        handle.write(tail_block)


def build_tar_gz(path: Path) -> None:
    """Create a minimal valid .tar.gz archive fixture."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tf:
        payload = b"archive entry payload\n"
        info = tarfile.TarInfo(name="readme.txt")
        info.size = len(payload)
        info.mtime = 1700000000
        tf.addfile(info, io.BytesIO(payload))
    with path.open("wb") as raw, gzip.GzipFile(
        filename="", mode="wb", fileobj=raw, mtime=0
    ) as gz:
        gz.write(buf.getvalue())


# ------------------------------------------------------------ Execution helpers


def _to_bytes(payload: bytes | str | None) -> bytes:
    if payload is None:
        return b""
    if isinstance(payload, bytes):
        return payload
    return payload.encode("utf-8", errors="replace")


def safe_json_loads(text: str) -> tuple[dict | None, str | None]:
    """Parse a JSON object safely, returning (obj, None) or (None, error_message)."""
    try:
        parsed = json.loads(text)
    except json.JSONDecodeError as exc:
        return None, f"JSON decode error: {exc}"
    if not isinstance(parsed, dict):
        return None, f"expected JSON object, got {type(parsed).__name__}"
    return parsed, None


def safe_jsonl_loads(text: str) -> tuple[list[dict], list[str]]:
    """Parse non-empty JSONL lines safely, returning (records, parse_errors)."""
    records: list[dict] = []
    errors: list[str] = []
    for line_no, raw_line in enumerate(text.splitlines(), start=1):
        if not raw_line.strip():
            continue
        try:
            parsed = json.loads(raw_line)
        except json.JSONDecodeError as exc:
            errors.append(f"line {line_no}: {exc}")
            continue
        if not isinstance(parsed, dict):
            errors.append(
                f"line {line_no}: expected JSON object, got {type(parsed).__name__}"
            )
            continue
        records.append(parsed)
    return records, errors


def safe_parse_markers(markdown: str) -> tuple[list[dict], str | None]:
    """Parse bodymatter:v1 markers safely, returning (markers, None) or ([], error_message)."""
    try:
        return parse_bodymatter_markers(markdown), None
    except ValueError as exc:
        return [], str(exc)


def run_cmd(
    argv: list[str],
    log_dir: Path,
    label: str,
    *,
    cwd: Path = REPO_ROOT,
    input_bytes: bytes | None = None,
    env: dict[str, str] | None = None,
    timeout: int = 120,
) -> dict:
    """Execute one command, save stdout/stderr logs exclusively without following symlinks, and return metrics.

    Parameters:
      argv: Command and arguments list.
      log_dir: Non-symlink directory where `<label>.stdout` and `<label>.stderr` are created exclusively.
      label: Unique command label used to name log files.
      cwd: Working directory for the subprocess.
      input_bytes: Optional raw stdin bytes.
      env: Optional environment mapping.
      timeout: Maximum wall-clock seconds before terminating the subprocess.

    Returns:
      Dictionary containing label, argv, returncode, elapsed_ms, stdout/stderr bytes & text,
      and relative log paths.

    Failure modes:
      Raises DriverError if `log_dir` is a symlink, log creation fails (e.g., duplicate label
      or symlink collision), the executable cannot be launched, or the command times out.
    """
    started = time.perf_counter()
    if log_dir.is_symlink():
        raise DriverError(f"log_dir must not be a symlink: {log_dir}")
    if not log_dir.exists():
        try:
            log_dir.mkdir(exist_ok=False)
        except OSError as exc:
            raise DriverError(f"failed to create log_dir {log_dir}: {exc}") from exc
    elif not log_dir.is_dir():
        raise DriverError(f"log_dir is not a directory: {log_dir}")
    stdout_path = log_dir / f"{label}.stdout"
    stderr_path = log_dir / f"{label}.stderr"
    try:
        completed = subprocess.run(
            argv,
            cwd=cwd,
            input=input_bytes,
            env=env,
            capture_output=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        write_exclusive_bytes(stdout_path, _to_bytes(exc.stdout))
        write_exclusive_bytes(stderr_path, _to_bytes(exc.stderr))
        raise DriverError(
            f"command {label!r} timed out after {timeout}s "
            f"(partial logs: {stdout_path}, {stderr_path})"
        ) from exc
    except OSError as exc:
        raise DriverError(
            f"command {label!r} failed to launch ({argv[0] if argv else ''}): {exc}"
        ) from exc
    elapsed_ms = round((time.perf_counter() - started) * 1000.0, 3)
    write_exclusive_bytes(stdout_path, completed.stdout)
    write_exclusive_bytes(stderr_path, completed.stderr)
    return {
        "label": label,
        "argv": argv,
        "returncode": completed.returncode,
        "elapsed_ms": elapsed_ms,
        "stdout_bytes": completed.stdout,
        "stderr_bytes": completed.stderr,
        "stdout_text": completed.stdout.decode("utf-8", errors="replace"),
        "stderr_text": completed.stderr.decode("utf-8", errors="replace"),
        "stdout_log": str(stdout_path.relative_to(REPO_ROOT)),
        "stderr_log": str(stderr_path.relative_to(REPO_ROOT)),
    }


def command_summary(rec: dict) -> dict:
    """Return a JSON-serializable summary of a run_cmd record."""
    return {
        "label": rec["label"],
        "argv": rec["argv"],
        "returncode": rec["returncode"],
        "elapsed_ms": rec["elapsed_ms"],
        "stdout_log": rec["stdout_log"],
        "stderr_log": rec["stderr_log"],
    }


def capture_tool(argv: list[str]) -> str:
    """Return the first line of a tool version command."""
    try:
        completed = subprocess.run(
            argv, capture_output=True, text=True, errors="replace", timeout=20
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return f"unavailable: {exc}"
    text = (completed.stdout or completed.stderr).strip()
    return text.splitlines()[0] if text else f"exit {completed.returncode}"


def _run_git(
    args: list[str],
    *,
    repo_root: Path = REPO_ROOT,
    timeout: int = 30,
) -> tuple[subprocess.CompletedProcess[str] | None, str | None]:
    """Run one git subcommand under repo_root without raising on OS or timeout errors."""
    try:
        completed = subprocess.run(
            ["git", "-C", str(repo_root), *args],
            capture_output=True,
            text=True,
            errors="replace",
            timeout=timeout,
        )
        return completed, None
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, str(exc)


def resolve_base_ref(
    base_ref: str | None = None, *, repo_root: Path = REPO_ROOT
) -> tuple[str | None, str | None, str | None]:
    """Resolve the upstream comparison base ref (defaulting to origin/main, fallback main).

    Returns (ref_name, commit_sha, error_message).
    """
    candidates = [base_ref] if base_ref else ["origin/main", "main"]
    last_err: str | None = None
    for candidate in candidates:
        res, err = _run_git(
            ["rev-parse", "--verify", "--quiet", f"{candidate}^{{commit}}"],
            repo_root=repo_root,
        )
        if res is None:
            last_err = err or f"git rev-parse failed for {candidate}"
            continue
        if res.returncode == 0 and res.stdout.strip():
            return candidate, res.stdout.strip(), None
        last_err = f"ref not found: {candidate}"
    if base_ref:
        return None, None, f"explicit base ref unavailable: {base_ref} ({last_err})"
    return None, None, f"neither origin/main nor main ref resolved ({last_err})"


def check_upstream_isolation(
    *,
    repo_root: Path = REPO_ROOT,
    base_ref: str | None = None,
) -> dict:
    """Verify upstream paths are clean across unstaged, staged, committed vs base_ref, and untracked vectors."""
    paths = list(UPSTREAM_PROTECTED_PATHS)
    errors: dict[str, str] = {}

    def _non_empty_lines(text: str) -> list[str]:
        return [line.strip() for line in text.splitlines() if line.strip()]

    # 1. Unstaged diff
    unstaged_res, unstaged_err = _run_git(
        ["diff", "--name-only", "--", *paths], repo_root=repo_root
    )
    if unstaged_res is None or unstaged_res.returncode != 0:
        errors["unstaged"] = (
            unstaged_err
            or (unstaged_res.stderr.strip() if unstaged_res else "git diff failed")
        )
        unstaged_files: list[str] = []
    else:
        unstaged_files = _non_empty_lines(unstaged_res.stdout)

    # 2. Staged diff
    staged_res, staged_err = _run_git(
        ["diff", "--cached", "--name-only", "--", *paths], repo_root=repo_root
    )
    if staged_res is None or staged_res.returncode != 0:
        errors["staged"] = (
            staged_err
            or (staged_res.stderr.strip() if staged_res else "git diff --cached failed")
        )
        staged_files: list[str] = []
    else:
        staged_files = _non_empty_lines(staged_res.stdout)

    # 3. Committed branch diff vs base_ref
    resolved_ref, resolved_sha, base_err = resolve_base_ref(
        base_ref, repo_root=repo_root
    )
    committed_files: list[str] = []
    if base_err is not None or resolved_ref is None:
        errors["committed"] = base_err or "base ref unavailable"
    else:
        committed_res, committed_err = _run_git(
            ["diff", "--name-only", f"{resolved_ref}...HEAD", "--", *paths],
            repo_root=repo_root,
        )
        if committed_res is None or committed_res.returncode != 0:
            errors["committed"] = (
                committed_err
                or (
                    committed_res.stderr.strip()
                    if committed_res
                    else "git diff base...HEAD failed"
                )
            )
        else:
            committed_files = _non_empty_lines(committed_res.stdout)

    # 4. Untracked (and porcelain status) upstream files
    untracked_res, untracked_err = _run_git(
        ["status", "--porcelain", "--untracked-files=all", "--", *paths],
        repo_root=repo_root,
    )
    untracked_files: list[str] = []
    status_lines: list[str] = []
    if untracked_res is None or untracked_res.returncode != 0:
        errors["untracked"] = (
            untracked_err
            or (
                untracked_res.stderr.strip()
                if untracked_res
                else "git status --untracked-files=all failed"
            )
        )
    else:
        for line in untracked_res.stdout.splitlines():
            if not line.strip():
                continue
            status_lines.append(line)
            if line.startswith("?? "):
                untracked_files.append(line[3:].strip())

    all_empty = (
        not errors
        and not unstaged_files
        and not staged_files
        and not committed_files
        and not untracked_files
        and not status_lines
    )
    return {
        "paths": paths,
        "base_ref": resolved_ref,
        "base_commit": resolved_sha,
        "unstaged_files": unstaged_files,
        "staged_files": staged_files,
        "committed_files": committed_files,
        "untracked_files": untracked_files,
        "status_lines": status_lines,
        "errors": errors,
        "all_empty": all_empty,
    }


def git_snapshot(
    *, repo_root: Path = REPO_ROOT, base_ref: str | None = None
) -> dict:
    """Capture current HEAD, branch, dirty file SHA-256s, 4-vector upstream isolation, and gitignore check."""
    head = capture_tool(["git", "-C", str(repo_root), "rev-parse", "HEAD"])
    branch = capture_tool(
        ["git", "-C", str(repo_root), "rev-parse", "--abbrev-ref", "HEAD"]
    )
    errors: dict[str, str] = {}

    status, status_err = _run_git(["status", "--porcelain"], repo_root=repo_root)
    dirty_entries: list[dict] = []
    if status is None:
        errors["status"] = status_err or "git status failed"
    else:
        for line in status.stdout.splitlines():
            if not line.strip():
                continue
            code = line[:2]
            rel_path = line[3:].strip()
            if " -> " in rel_path:
                rel_path = rel_path.split(" -> ", 1)[1]
            full = repo_root / rel_path
            entry: dict = {"status": code.strip(), "path": rel_path}
            if full.is_file() and not full.is_symlink():
                entry["sha256"] = sha256_file(full)
            dirty_entries.append(entry)

    upstream = check_upstream_isolation(repo_root=repo_root, base_ref=base_ref)
    if upstream["errors"]:
        errors["upstream_isolation"] = "; ".join(
            f"{k}: {v}" for k, v in upstream["errors"].items()
        )
    upstream_diff_empty = upstream["all_empty"]
    if upstream_diff_empty:
        upstream_diff_preview = ""
    else:
        summary_parts = []
        for label, items in (
            ("unstaged", upstream["unstaged_files"]),
            ("staged", upstream["staged_files"]),
            ("committed", upstream["committed_files"]),
            ("untracked", upstream["untracked_files"]),
        ):
            if items:
                summary_parts.append(f"{label}={items}")
        if upstream["errors"]:
            summary_parts.append(f"errors={upstream['errors']}")
        upstream_diff_preview = "; ".join(summary_parts)[:500]

    gitignore_check, gitignore_err = _run_git(
        [
            "check-ignore",
            "--no-index",
            "-v",
            "extensionalities/dev/README.md",
        ],
        repo_root=repo_root,
    )
    if gitignore_check is None:
        errors["gitignore_check"] = gitignore_err or "git check-ignore failed"
        gitignore_check_returncode = -1
        gitignore_check_output = f"error: {errors['gitignore_check']}"
    else:
        gitignore_check_returncode = gitignore_check.returncode
        gitignore_check_output = gitignore_check.stdout.strip()

    snapshot: dict = {
        "head": head,
        "branch": branch,
        "dirty_files": dirty_entries,
        "upstream_isolation": upstream,
        "upstream_diff_empty": upstream_diff_empty,
        "upstream_diff_preview": upstream_diff_preview,
        "gitignore_check_returncode": gitignore_check_returncode,
        "gitignore_check_output": gitignore_check_output,
    }
    if errors:
        snapshot["errors"] = errors
    return snapshot


def toolchain_snapshot() -> dict:
    """Capture host and Rust/Python toolchain metadata."""
    return {
        "hostname": platform.node() or "unknown",
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": sys.version.split()[0],
        "rustc": capture_tool(["rustc", "--version"]),
        "cargo": capture_tool(["cargo", "--version"]),
    }


def build_result_payload(
    *,
    mode: str,
    verification_matrix: dict[str, dict],
    git_info: dict,
    toolchain_info: dict,
    binaries_info: dict,
    timings_ms: dict,
    scratch_cleanup: dict,
    evidence_dir: Path,
    source_before: dict | None = None,
    source_after: dict | None = None,
) -> dict:
    """Build the canonical structured result.json evidence document.

    Parameters:
      mode: Verification mode (`e2e` or `docs-only`).
      verification_matrix: Per-verification-ID check and command results.
      git_info: Git state and upstream isolation snapshot.
      toolchain_info: Host and Rust/Python toolchain metadata.
      binaries_info: Release binary hashes, build receipt provenance, and freshness info.
      timings_ms: Wall-clock timing measurements in milliseconds.
      scratch_cleanup: Synthetic sandbox cleanup receipt.
      evidence_dir: Evidence directory Path inside the repository.
      source_before: Optional downstream source manifest computed before E2E commands/build.
      source_after: Optional downstream source manifest computed after all E2E commands finish.

    Returns:
      JSON-serializable result payload dictionary.

    Failure modes:
      Raises ValueError if evidence_dir is not relative to REPO_ROOT.
    """
    overall = (
        "PASS"
        if verification_matrix
        and all(entry["status"] == "PASS" for entry in verification_matrix.values())
        else "FAIL"
    )
    payload: dict = {
        "schema": "glassbox-verify-inspect-extract-outputs/v1",
        "feature": "20261123_inspect-extract-outputs",
        "mode": mode,
        "recorded_at": utc_now(),
        "overall_status": overall,
        "evidence_dir": str(evidence_dir.relative_to(REPO_ROOT)),
        "git": git_info,
        "toolchain": toolchain_info,
        "binaries": binaries_info,
        "timings_ms": timings_ms,
        "scratch_cleanup": scratch_cleanup,
        "verification_matrix": verification_matrix,
    }
    if source_before is not None or source_after is not None:
        payload["source_before"] = source_before
        payload["source_after"] = source_after
        payload["source_snapshot_unchanged"] = (
            source_before is not None
            and source_after is not None
            and source_before.get("manifest_sha256")
            == source_after.get("manifest_sha256")
        )
    return payload


# ------------------------------------------------------- E2E gate helpers


def _prepare_release_binaries(
    target_dir: Path,
    ext_dir: Path,
    log_dir: Path,
    source_before: dict,
    *,
    build_binaries: bool,
    allow_stale_binary: bool,
) -> tuple[Path, Path, dict, dict]:
    """Optionally build release binaries and verify freshness and build-receipt provenance.

    Parameters:
      target_dir: Cargo target directory containing `release/glassbox` and `release/litx`.
      ext_dir: Downstream `extensionalities/` root directory.
      log_dir: Non-symlink directory where build subprocess logs are written.
      source_before: Source manifest snapshot computed before any build or E2E command.
      build_binaries: When True, build release binaries and write `.glassbox-build-receipt.json`.
      allow_stale_binary: When True, allow execution past stale/unreceipted checks
        (V-IO-08 still records failure).

    Returns:
      Tuple `(glassbox_bin, litx_bin, freshness, binaries_info)`.

    Failure modes:
      Raises DriverError if `--build` fails, source mutates during build, either release binary
      is missing/non-executable, or freshness/provenance checks fail without `allow_stale_binary`.
    """
    glassbox_bin = target_dir / "release" / "glassbox"
    litx_bin = target_dir / "release" / "litx"
    receipt_file = build_receipt_path(target_dir)

    build_record: dict | None = None
    written_receipt: dict | None = None
    if build_binaries:
        build_env = {**os.environ, "CARGO_TARGET_DIR": str(target_dir)}
        c_build = run_cmd(
            [
                "cargo",
                "build",
                "--release",
                "--locked",
                "--manifest-path",
                str(ext_dir / "Cargo.toml"),
                "--bins",
            ],
            log_dir,
            "build_release_bins",
            env=build_env,
            timeout=900,
        )
        if c_build["returncode"] != 0:
            raise DriverError(
                f"--build failed with exit code {c_build['returncode']} "
                f"(see {c_build['stderr_log']})"
            )
        build_record = command_summary(c_build)
        for binary in (glassbox_bin, litx_bin):
            if not binary.is_file() or not os.access(binary, os.X_OK):
                raise DriverError(
                    f"missing executable release binary after --build: {binary}"
                )
        source_post_build = compute_source_manifest(ext_dir)
        if source_post_build["manifest_sha256"] != source_before["manifest_sha256"]:
            raise DriverError(
                "downstream source manifest changed during --build "
                f"(before={source_before['manifest_sha256']}, after={source_post_build['manifest_sha256']}); "
                "refusing to write .glassbox-build-receipt.json"
            )
        written_receipt = write_build_receipt(
            receipt_file,
            source_manifest_sha256=source_before["manifest_sha256"],
            glassbox_sha256=sha256_file(glassbox_bin),
            litx_sha256=sha256_file(litx_bin),
        )

    for binary in (glassbox_bin, litx_bin):
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise DriverError(f"missing executable release binary: {binary}")

    freshness = check_binary_freshness(
        ext_dir,
        {"glassbox": glassbox_bin, "litx": litx_bin},
        source_manifest=source_before,
        receipt_path=receipt_file,
    )
    if not freshness["all_fresh"] and not allow_stale_binary:
        reasons: list[str] = []
        if not freshness["mtime_fresh"]:
            stale_desc = ", ".join(
                f"{name} (mtime={freshness['binaries'][name]['mtime']}, lag={freshness['binaries'][name]['stale_lag_seconds']}s)"
                for name in freshness["stale_binaries"]
            )
            reasons.append(
                f"stale release binary detected against source {source_before['newest_path']} "
                f"(mtime={source_before['newest_mtime']}): {stale_desc}"
            )
        if not freshness["receipt_valid"]:
            reasons.append(
                "build receipt provenance check failed: "
                + "; ".join(freshness["receipt_errors"])
            )
        raise DriverError(
            "; ".join(reasons)
            + ". Re-run with --build to build fresh release binaries and bind .glassbox-build-receipt.json."
        )

    binaries_info: dict = {
        "glassbox": freshness["binaries"]["glassbox"],
        "litx": freshness["binaries"]["litx"],
        "source_manifest": source_before,
        "source_before": source_before,
        "mtime_fresh": freshness["mtime_fresh"],
        "receipt_path": freshness["receipt_path"],
        "receipt_valid": freshness["receipt_valid"],
        "receipt_errors": freshness["receipt_errors"],
        "receipt": freshness["receipt"] or written_receipt,
        "all_fresh": freshness["all_fresh"],
        "stale_binaries": freshness["stale_binaries"],
        "built_by_driver": build_binaries,
        "allow_stale_binary": allow_stale_binary,
    }
    if build_record is not None:
        binaries_info["build"] = build_record
    return glassbox_bin, litx_bin, freshness, binaries_info


def _build_e2e_fixtures(
    sandbox_dir: Path,
    intake_dir: Path,
    boundary_dir: Path,
    output_root: Path,
) -> tuple[dict[str, Path], dict[str, dict]]:
    """Create isolated synthetic intake and boundary fixtures and record their digests.

    Parameters:
      sandbox_dir: Root sandbox directory for relative digest keys.
      intake_dir: Directory for Frank's 5-file mixed intake folder.
      boundary_dir: Directory for threshold, format-edge, and regression fixtures.
      output_root: Directory for CLI `--output-dir` targets (created with mode 0o700).

    Returns:
      Tuple `(fixtures, created_digests)` mapping logical fixture keys to Paths and
      relative sandbox paths to size/digest metadata dicts.

    Failure modes:
      Raises OSError if sandbox directories already exist or fixture files cannot be written.
    """
    intake_dir.mkdir(parents=True, exist_ok=False)
    boundary_dir.mkdir(parents=True, exist_ok=False)
    output_root.mkdir(parents=True, exist_ok=False)
    os.chmod(output_root, 0o700)

    # Build Frank's 5-file mixed intake folder
    pdf_path = intake_dir / "01-labeled-report.pdf"
    pdf_path.write_bytes(
        build_synthetic_pdf(
            pages=[
                f"Preface Roman page four {FORGED_BODY_MARKER}",
                f"Chapter one decimal page one body text. {RAW_HTML_PAYLOAD}",
            ],
            page_labels_dict="<< /Nums [ 0 << /S /r /St 4 >> 1 << /S /D /St 1 >> ] >>",
            info_entries={
                "Title": "Frank Quarterly Report",
                "Author": "Frank Rivera",
                "Creator": "Glassbox Test Authoring Tool",
                "Producer": "Glassbox Synthetic PDF Engine",
                "CreationDate": "D:20261123101530Z",
                "ModDate": "D:20261123120000Z",
            },
        )
    )

    xlsx_path = intake_dir / "02-financials.xlsx"
    build_synthetic_xlsx(xlsx_path)

    docx_path = intake_dir / "03-brief.docx"
    build_synthetic_docx(docx_path)

    misnamed_path = intake_dir / "report.pdf"
    misnamed_path.write_text(
        "<!DOCTYPE html><html><head><title>Portal Export</title></head>"
        "<body><h1>HTML body sentinel</h1><p>Misnamed download.</p></body></html>\n",
        encoding="utf-8",
    )

    large_size = DEFAULT_FULL_HASH_MAX_BYTES + 4096
    large_path = intake_dir / "05-large-capture.mp4"
    build_sparse_media_file(large_path, large_size, b"FRANK-MEDIA-V3")

    # Boundary fixtures outside intake/
    exact_thresh_path = boundary_dir / "exact-threshold.mp4"
    build_sparse_media_file(
        exact_thresh_path, DEFAULT_FULL_HASH_MAX_BYTES, b"EXACT-T"
    )
    above_thresh_path = boundary_dir / "threshold-plus-one.mp4"
    build_sparse_media_file(
        above_thresh_path, DEFAULT_FULL_HASH_MAX_BYTES + 1, b"PLUS-ONE"
    )
    bad_date_pdf = boundary_dir / "bad-date.pdf"
    bad_date_pdf.write_bytes(
        build_synthetic_pdf(
            pages=["Malformed date check"],
            info_entries={
                "Title": "Malformed Date Doc",
                "CreationDate": "not-a-valid-date",
            },
        )
    )
    tar_gz_path = boundary_dir / "bundle.tar.gz"
    build_tar_gz(tar_gz_path)
    extensionless_path = boundary_dir / "README"
    extensionless_path.write_text("Extensionless plain text file.\n", encoding="utf-8")

    # Audit Wave-1 regression fixtures (GB-AUD-04, GB-AUD-06, GB-AUD-07)
    csv_budget_path = boundary_dir / "headers-budget.csv"
    csv_budget_path.write_text(
        "Alpha Beta Gamma Delta,Long descriptive column header\n"
        "ValueOne,ValueTwo\n",
        encoding="utf-8",
    )
    zip_as_docx_path = boundary_dir / "listing.docx"
    write_zip_entries(zip_as_docx_path, [("notes.txt", "plain archive entry\n")])
    minimal_psd_path = boundary_dir / "minimal.psd"
    minimal_psd_path.write_bytes(MINIMAL_PSD_BYTES)
    dash_named_path = boundary_dir / "--no-markers"
    dash_named_path.write_bytes(
        build_synthetic_pdf(pages=["Option terminator literal filename probe."])
    )

    fixtures = {
        "pdf_path": pdf_path,
        "xlsx_path": xlsx_path,
        "docx_path": docx_path,
        "misnamed_path": misnamed_path,
        "large_path": large_path,
        "exact_thresh_path": exact_thresh_path,
        "above_thresh_path": above_thresh_path,
        "bad_date_pdf": bad_date_pdf,
        "tar_gz_path": tar_gz_path,
        "extensionless_path": extensionless_path,
        "csv_budget_path": csv_budget_path,
        "zip_as_docx_path": zip_as_docx_path,
        "minimal_psd_path": minimal_psd_path,
        "dash_named_path": dash_named_path,
    }
    created_digests: dict[str, dict] = {}
    for fpath in fixtures.values():
        rel = str(fpath.relative_to(sandbox_dir))
        entry = {"bytes": fpath.stat().st_size}
        if fpath.stat().st_size <= DEFAULT_FULL_HASH_MAX_BYTES:
            entry["sha256"] = sha256_file(fpath)
        else:
            entry["sampled_fingerprint"] = compute_sampled_fingerprint_oracle(fpath)
        created_digests[rel] = entry

    return fixtures, created_digests


def _run_v_io_01(bench_receipt: Path) -> tuple[list[dict], list[dict]]:
    """Run V-IO-01 benchmark receipt validation against the 256 MiB hash receipt.

    Parameters:
      bench_receipt: Path to the V-IO-01 hash benchmark receipt JSON.

    Returns:
      Tuple `(checks, commands)` for the `V-IO-01` verification matrix entry.

    Failure modes:
      Returns failing check items rather than raising on invalid/missing receipts.
    """
    v1_checks = validate_bench_receipt_file(
        bench_receipt, repo_root=REPO_ROOT, check_git_ancestry=True
    )
    return v1_checks, []


def _run_v_io_02(
    glassbox_bin: Path,
    log_dir: Path,
    fixtures: dict[str, Path],
    created_digests: dict[str, dict],
    timings_ms: dict[str, float],
) -> tuple[list[dict], list[dict], dict]:
    """Run V-IO-02 sampled fingerprinting, `--full-hash`, and 128 MiB threshold boundary checks.

    Parameters:
      glassbox_bin: Path to the release `glassbox` executable.
      log_dir: Log directory for subprocess stdout/stderr capture.
      fixtures: Synthetic fixture map from `_build_e2e_fixtures`.
      created_digests: Mutable fixture digest map updated with `full_sha256` when computed.
      timings_ms: Mutable timings dictionary updated with sampled and full-hash latencies.

    Returns:
      Tuple `(checks, commands, context)` where `context` provides `large_report`,
      `large_err`, and `expected_fp` for downstream gates (V-IO-03 and V-IO-04).

    Failure modes:
      Raises DriverError if any verification subprocess fails to spawn or times out.
    """
    large_path = fixtures["large_path"]
    exact_thresh_path = fixtures["exact_thresh_path"]
    above_thresh_path = fixtures["above_thresh_path"]
    v2_cmds: list[dict] = []
    v2_checks: list[dict] = []

    # Default sampled inspect on large media file (run twice to capture cold & warm timings)
    c_large_cold = run_cmd(
        [str(glassbox_bin), "inspect", str(large_path)], log_dir, "v02_large_cold"
    )
    c_large_warm = run_cmd(
        [str(glassbox_bin), "inspect", str(large_path)], log_dir, "v02_large_warm"
    )
    v2_cmds.extend([c_large_cold, c_large_warm])
    timings_ms["large_media_sampled_cold_ms"] = c_large_cold["elapsed_ms"]
    timings_ms["large_media_sampled_warm_ms"] = c_large_warm["elapsed_ms"]

    large_parsed, large_err = safe_json_loads(c_large_warm["stdout_text"])
    large_report = large_parsed or {}
    expected_fp = compute_sampled_fingerprint_oracle(large_path)
    large_file_obj = (
        large_report.get("file") if isinstance(large_report.get("file"), dict) else {}
    )
    fp_obj = (
        large_file_obj.get("fingerprint")
        if isinstance(large_file_obj.get("fingerprint"), dict)
        else {}
    )
    diag_codes = [
        d.get("code")
        for d in large_report.get("diagnostics", [])
        if isinstance(d, dict)
    ]
    v2_checks.extend(
        [
            {
                "name": "large_default_status_and_exit",
                "ok": (
                    large_err is None
                    and c_large_warm["returncode"] == 2
                    and large_report.get("status") == "unsupported"
                ),
                "detail": (
                    f"parse_err={large_err}"
                    if large_err
                    else f"rc={c_large_warm['returncode']}, status={large_report.get('status')}, cold_ms={c_large_cold['elapsed_ms']}, warm_ms={c_large_warm['elapsed_ms']}"
                ),
            },
            {
                "name": "large_default_sampled_latency_under_50ms",
                "ok": c_large_warm["elapsed_ms"] < SAMPLED_LATENCY_TARGET_MS,
                "detail": (
                    f"cold_ms={c_large_cold['elapsed_ms']}, warm_ms={c_large_warm['elapsed_ms']} "
                    f"(budget<{SAMPLED_LATENCY_TARGET_MS}ms)"
                ),
            },
            {
                "name": "large_default_emits_fingerprint_omits_sha256",
                "ok": (
                    large_err is None
                    and "sha256" not in large_file_obj
                    and fp_obj.get("scheme") == SAMPLED_SCHEME
                    and fp_obj.get("value") == expected_fp
                ),
                "detail": f"scheme={fp_obj.get('scheme')}, oracle_match={fp_obj.get('value') == expected_fp}",
            },
            {
                "name": "large_default_emits_fingerprint_sampled_diagnostic",
                "ok": large_err is None and "fingerprint_sampled" in diag_codes,
                "detail": f"diagnostics={diag_codes}",
            },
        ]
    )

    # --full-hash on large media file vs system sha256sum
    c_large_full = run_cmd(
        [str(glassbox_bin), "inspect", "--full-hash", str(large_path)],
        log_dir,
        "v02_large_full_hash",
    )
    c_sha256sum = run_cmd(
        ["sha256sum", str(large_path)], log_dir, "v02_large_sha256sum"
    )
    v2_cmds.extend([c_large_full, c_sha256sum])
    timings_ms["large_media_full_hash_ms"] = c_large_full["elapsed_ms"]
    large_full_parsed, large_full_err = safe_json_loads(c_large_full["stdout_text"])
    large_full_report = large_full_parsed or {}
    large_full_file = (
        large_full_report.get("file")
        if isinstance(large_full_report.get("file"), dict)
        else {}
    )
    full_diag_codes = [
        d.get("code")
        for d in large_full_report.get("diagnostics", [])
        if isinstance(d, dict)
    ]
    sha_parts = c_sha256sum["stdout_text"].split()
    sys_sha = sha_parts[0] if sha_parts else ""
    if sys_sha:
        created_digests["intake/05-large-capture.mp4"]["full_sha256"] = sys_sha
    v2_checks.extend(
        [
            {
                "name": "large_full_hash_matches_sha256sum_and_omits_fingerprint",
                "ok": (
                    large_full_err is None
                    and bool(sys_sha)
                    and c_large_full["returncode"] == 2
                    and large_full_report.get("status") == "unsupported"
                    and c_sha256sum["returncode"] == 0
                    and "fingerprint" not in large_full_file
                    and large_full_file.get("sha256") == sys_sha
                    and "fingerprint_sampled" not in full_diag_codes
                ),
                "detail": (
                    f"parse_err={large_full_err}"
                    if large_full_err
                    else f"sha256_match={large_full_file.get('sha256') == sys_sha}, diagnostics={full_diag_codes}, elapsed_ms={c_large_full['elapsed_ms']}"
                ),
            },
        ]
    )

    # Threshold boundary: exact T (128 MiB) vs T+1 (128 MiB + 1)
    c_exact_t = run_cmd(
        [str(glassbox_bin), "inspect", str(exact_thresh_path)],
        log_dir,
        "v02_exact_threshold",
    )
    c_plus_one = run_cmd(
        [str(glassbox_bin), "inspect", str(above_thresh_path)],
        log_dir,
        "v02_threshold_plus_one",
    )
    v2_cmds.extend([c_exact_t, c_plus_one])
    exact_t_parsed, exact_t_err = safe_json_loads(c_exact_t["stdout_text"])
    plus_one_parsed, plus_one_err = safe_json_loads(c_plus_one["stdout_text"])
    exact_t_rep = exact_t_parsed or {}
    plus_one_rep = plus_one_parsed or {}
    exact_t_file = (
        exact_t_rep.get("file") if isinstance(exact_t_rep.get("file"), dict) else {}
    )
    plus_one_file = (
        plus_one_rep.get("file") if isinstance(plus_one_rep.get("file"), dict) else {}
    )
    plus_one_fp = (
        plus_one_file.get("fingerprint")
        if isinstance(plus_one_file.get("fingerprint"), dict)
        else {}
    )
    v2_checks.extend(
        [
            {
                "name": "threshold_exact_T_uses_full_sha256",
                "ok": (
                    exact_t_err is None
                    and exact_t_file.get("sha256")
                    == created_digests["boundary/exact-threshold.mp4"]["sha256"]
                    and "fingerprint" not in exact_t_file
                ),
                "detail": (
                    f"parse_err={exact_t_err}"
                    if exact_t_err
                    else f"bytes={exact_t_file.get('bytes')}"
                ),
            },
            {
                "name": "threshold_T_plus_1_uses_sampled_fingerprint_distinct_from_T_plus_4096",
                "ok": (
                    plus_one_err is None
                    and "sha256" not in plus_one_file
                    and plus_one_fp.get("value")
                    == compute_sampled_fingerprint_oracle(above_thresh_path)
                    and plus_one_fp.get("value") != expected_fp
                ),
                "detail": (
                    f"parse_err={plus_one_err}"
                    if plus_one_err
                    else f"bytes={plus_one_file.get('bytes')}"
                ),
            },
        ]
    )
    context = {
        "large_report": large_report,
        "large_err": large_err,
        "expected_fp": expected_fp,
    }
    return v2_checks, v2_cmds, context


def _run_v_io_03(
    glassbox_bin: Path,
    log_dir: Path,
    fixtures: dict[str, Path],
    boundary_dir: Path,
    output_root: Path,
    expected_fp: str,
) -> tuple[list[dict], list[dict]]:
    """Run V-IO-03 schema v3 inspect envelope, summaries, flags, and `--output-dir` checks.

    Parameters:
      glassbox_bin: Path to the release `glassbox` executable.
      log_dir: Log directory for subprocess stdout/stderr capture.
      fixtures: Synthetic fixture map from `_build_e2e_fixtures`.
      boundary_dir: Directory containing the literal `--no-markers` filename fixture.
      output_root: Sandbox root directory for `--output-dir` artifact checks.
      expected_fp: Expected sampled fingerprint hex digest for `05-large-capture.mp4`.

    Returns:
      Tuple `(checks, commands)` for the `V-IO-03` verification matrix entry.

    Failure modes:
      Raises DriverError if any verification subprocess fails to spawn or times out.
    """
    pdf_path = fixtures["pdf_path"]
    bad_date_pdf = fixtures["bad_date_pdf"]
    large_path = fixtures["large_path"]
    v3_cmds: list[dict] = []
    v3_checks: list[dict] = []

    c_pdf_inspect = run_cmd(
        [str(glassbox_bin), "inspect", str(pdf_path)], log_dir, "v03_pdf_inspect"
    )
    c_pdf_verbose = run_cmd(
        [str(glassbox_bin), "inspect", "--verbose", str(pdf_path)],
        log_dir,
        "v03_pdf_inspect_verbose",
    )
    c_pdf_text = run_cmd(
        [str(glassbox_bin), "inspect", "--format", "text", str(pdf_path)],
        log_dir,
        "v03_pdf_inspect_text",
    )
    c_pdf_md = run_cmd(
        [str(glassbox_bin), "inspect", "--format", "markdown", str(pdf_path)],
        log_dir,
        "v03_pdf_inspect_md",
    )
    c_bad_date = run_cmd(
        [str(glassbox_bin), "inspect", str(bad_date_pdf)],
        log_dir,
        "v03_bad_date_inspect",
    )
    c_inspect_nomarkers = run_cmd(
        [str(glassbox_bin), "inspect", "--no-markers", str(pdf_path)],
        log_dir,
        "v03_inspect_no_markers_reject",
    )
    c_inspect_dash_filename = run_cmd(
        [str(glassbox_bin), "inspect", "--", "--no-markers"],
        log_dir,
        "v03_inspect_option_terminator_dash_filename",
        cwd=boundary_dir,
    )
    v3_cmds.extend(
        [
            c_pdf_inspect,
            c_pdf_verbose,
            c_pdf_text,
            c_pdf_md,
            c_bad_date,
            c_inspect_nomarkers,
            c_inspect_dash_filename,
        ]
    )

    pdf_parsed, pdf_err = safe_json_loads(c_pdf_inspect["stdout_text"])
    pdf_verbose_parsed, pdf_verbose_err = safe_json_loads(c_pdf_verbose["stdout_text"])
    bad_date_parsed, bad_date_err = safe_json_loads(c_bad_date["stdout_text"])
    dash_parsed, dash_err = safe_json_loads(c_inspect_dash_filename["stdout_text"])
    pdf_rep = pdf_parsed or {}
    pdf_verbose_rep = pdf_verbose_parsed or {}
    bad_date_rep = bad_date_parsed or {}
    dash_rep = dash_parsed or {}
    pdf_file = pdf_rep.get("file") if isinstance(pdf_rep.get("file"), dict) else {}
    dash_file = dash_rep.get("file") if isinstance(dash_rep.get("file"), dict) else {}
    pdf_desc = (
        pdf_rep.get("description")
        if isinstance(pdf_rep.get("description"), dict)
        else {}
    )
    pdf_software = (
        pdf_desc.get("software") if isinstance(pdf_desc.get("software"), dict) else {}
    )
    bad_date_desc = (
        bad_date_rep.get("description")
        if isinstance(bad_date_rep.get("description"), dict)
        else {}
    )

    required_top = {
        "schema_version",
        "kind",
        "status",
        "file",
        "format",
        "description",
        "structure",
        "readiness",
    }
    forbidden_default = {"next_step", "advice", "provenance", "diagnostics", "artifacts"}
    v3_checks.extend(
        [
            {
                "name": "v3_envelope_and_omissions",
                "ok": (
                    pdf_err is None
                    and c_pdf_inspect["returncode"] == 0
                    and pdf_rep.get("schema_version") == 3
                    and pdf_rep.get("kind") == "inspect"
                    and pdf_rep.get("status") == "complete"
                    and required_top.issubset(pdf_rep.keys())
                    and forbidden_default.isdisjoint(pdf_rep.keys())
                    and "consistent" not in pdf_file
                ),
                "detail": (
                    f"parse_err={pdf_err}"
                    if pdf_err
                    else f"keys={sorted(pdf_rep.keys())}"
                ),
            },
            {
                "name": "v3_description_rfc3339_and_software_separation",
                "ok": (
                    pdf_err is None
                    and pdf_desc.get("title") == "Frank Quarterly Report"
                    and pdf_desc.get("authors") == ["Frank Rivera"]
                    and pdf_desc.get("created_at") == "2026-11-23T10:15:30Z"
                    and pdf_desc.get("modified_at") == "2026-11-23T12:00:00Z"
                    and pdf_software.get("creator_tool")
                    == "Glassbox Test Authoring Tool"
                    and pdf_software.get("producer")
                    == "Glassbox Synthetic PDF Engine"
                    and "Preface Roman" not in c_pdf_inspect["stdout_text"]
                    and "bodymatter:v1" not in c_pdf_inspect["stdout_text"]
                ),
                "detail": f"description={pdf_rep.get('description')}",
            },
            {
                "name": "v3_malformed_date_emits_date_unnormalized",
                "ok": (
                    bad_date_err is None
                    and bad_date_desc.get("created_at") == "not-a-valid-date"
                    and any(
                        isinstance(d, dict)
                        and d.get("code") == "date_unnormalized"
                        and d.get("severity") == "info"
                        for d in bad_date_rep.get("diagnostics", [])
                    )
                ),
                "detail": (
                    f"parse_err={bad_date_err}"
                    if bad_date_err
                    else f"diagnostics={bad_date_rep.get('diagnostics')}"
                ),
            },
            {
                "name": "v3_verbose_includes_provenance",
                "ok": (
                    pdf_verbose_err is None
                    and c_pdf_verbose["returncode"] == 0
                    and isinstance(pdf_verbose_rep.get("provenance"), list)
                    and len(pdf_verbose_rep["provenance"]) > 0
                ),
                "detail": (
                    f"parse_err={pdf_verbose_err}"
                    if pdf_verbose_err
                    else f"provenance_count={len(pdf_verbose_rep.get('provenance') or [])}"
                ),
            },
            {
                "name": "v3_text_and_markdown_summaries_content_free",
                "ok": (
                    c_pdf_text["returncode"] == 0
                    and c_pdf_md["returncode"] == 0
                    and "Preface Roman" not in c_pdf_text["stdout_text"]
                    and "Preface Roman" not in c_pdf_md["stdout_text"]
                    and "Schema: 3" in c_pdf_text["stdout_text"]
                    and "# Glassbox inspection" in c_pdf_md["stdout_text"]
                    and "Schema: 3" in c_pdf_md["stdout_text"]
                    and "01-labeled-report.pdf" in c_pdf_md["stdout_text"]
                ),
                "detail": f"text_rc={c_pdf_text['returncode']}, md_rc={c_pdf_md['returncode']}, md_len={len(c_pdf_md['stdout_text'])}",
            },
            {
                "name": "inspect_rejects_no_markers_exit_64",
                "ok": c_inspect_nomarkers["returncode"] == 64
                and "extract" in c_inspect_nomarkers["stderr_text"].lower(),
                "detail": f"rc={c_inspect_nomarkers['returncode']}",
            },
            {
                "name": "inspect_option_terminator_allows_dash_prefixed_filename",
                "ok": (
                    dash_err is None
                    and c_inspect_dash_filename["returncode"] == 0
                    and dash_rep.get("status") == "complete"
                    and dash_file.get("name") == "--no-markers"
                ),
                "detail": (
                    f"parse_err={dash_err}"
                    if dash_err
                    else f"rc={c_inspect_dash_filename['returncode']}, status={dash_rep.get('status')}, name={dash_file.get('name')!r}"
                ),
            },
        ]
    )

    # --output-dir stems (`fp-<prefix>` vs `<sha256_prefix>`) and no-overwrite
    out_large = output_root / "inspect-large"
    c_out_large = run_cmd(
        [
            str(glassbox_bin),
            "inspect",
            str(large_path),
            "--output-dir",
            str(out_large),
        ],
        log_dir,
        "v03_output_dir_large",
    )
    large_saved_before = (
        {p.name: sha256_file(p) for p in sorted(out_large.iterdir()) if p.is_file()}
        if out_large.is_dir()
        else {}
    )
    c_out_large_retry = run_cmd(
        [
            str(glassbox_bin),
            "inspect",
            str(large_path),
            "--output-dir",
            str(out_large),
        ],
        log_dir,
        "v03_output_dir_large_retry",
    )
    large_saved_after = (
        {p.name: sha256_file(p) for p in sorted(out_large.iterdir()) if p.is_file()}
        if out_large.is_dir()
        else {}
    )
    out_pdf = output_root / "inspect-pdf"
    c_out_pdf = run_cmd(
        [
            str(glassbox_bin),
            "inspect",
            str(pdf_path),
            "--output-dir",
            str(out_pdf),
        ],
        log_dir,
        "v03_output_dir_pdf",
    )
    v3_cmds.extend([c_out_large, c_out_large_retry, c_out_pdf])

    large_out_files = sorted(large_saved_before.keys())
    pdf_out_files = (
        sorted(p.name for p in out_pdf.iterdir()) if out_pdf.is_dir() else []
    )
    fp_stem_token = f"-fp-{expected_fp[:12]}."
    pdf_sha_value = (
        pdf_file.get("sha256") if isinstance(pdf_file.get("sha256"), str) else ""
    )
    sha_stem_token = f"-{pdf_sha_value[:12]}." if len(pdf_sha_value) >= 12 else ""
    large_json_files = list(out_large.glob("*.json")) if out_large.is_dir() else []
    retry_parsed, retry_err = safe_json_loads(c_out_large_retry["stdout_text"])
    retry_rep = retry_parsed or {}
    saved_large_json: dict = {}
    saved_large_json_err: str | None = "no saved json file"
    if large_json_files:
        saved_parsed, saved_large_json_err = safe_json_loads(
            large_json_files[0].read_text(encoding="utf-8", errors="replace")
        )
        saved_large_json = saved_parsed or {}
    v3_checks.extend(
        [
            {
                "name": "output_dir_sampled_uses_fp_prefix",
                "ok": (
                    c_out_large["returncode"] == 2
                    and len(large_out_files) == 2
                    and all(fp_stem_token in name for name in large_out_files)
                ),
                "detail": f"rc={c_out_large['returncode']}, files={large_out_files}",
            },
            {
                "name": "output_dir_normal_uses_sha256_prefix",
                "ok": (
                    bool(sha_stem_token)
                    and c_out_pdf["returncode"] == 0
                    and len(pdf_out_files) == 2
                    and all(sha_stem_token in name for name in pdf_out_files)
                    and not any("-fp-" in name for name in pdf_out_files)
                ),
                "detail": f"files={pdf_out_files}",
            },
            {
                "name": "output_dir_no_overwrite_on_rerun",
                "ok": (
                    retry_err is None
                    and saved_large_json_err is None
                    and c_out_large_retry["returncode"] == 1
                    and retry_rep.get("status") == "failed"
                    and any(
                        isinstance(d, dict)
                        and d.get("code") == "metadata_output_failed"
                        for d in retry_rep.get("diagnostics", [])
                    )
                    and bool(large_saved_before)
                    and large_saved_before == large_saved_after
                    and saved_large_json.get("schema_version") == 3
                ),
                "detail": (
                    f"retry_rc={c_out_large_retry['returncode']}, retry_err={retry_err}, "
                    f"saved_json_err={saved_large_json_err}, bytes_unchanged={large_saved_before == large_saved_after}"
                ),
            },
        ]
    )
    return v3_checks, v3_cmds


def _run_v_io_04(
    glassbox_bin: Path,
    log_dir: Path,
    fixtures: dict[str, Path],
    large_report: dict,
    large_err: str | None,
) -> tuple[list[dict], list[dict]]:
    """Run V-IO-04 format classification, compound extensions, and mismatch checks.

    Parameters:
      glassbox_bin: Path to the release `glassbox` executable.
      log_dir: Log directory for subprocess stdout/stderr capture.
      fixtures: Synthetic fixture map from `_build_e2e_fixtures`.
      large_report: Parsed V-IO-02 inspect report for `05-large-capture.mp4`.
      large_err: JSON parse error string from V-IO-02 (or None).

    Returns:
      Tuple `(checks, commands)` for the `V-IO-04` verification matrix entry.

    Failure modes:
      Raises DriverError if any verification subprocess fails to spawn or times out.
    """
    v4_cmds: list[dict] = []
    v4_checks: list[dict] = []
    djvu_fixture = EXT_DIR / "fixtures" / "single.djvu"
    c_misnamed = run_cmd(
        [str(glassbox_bin), "inspect", str(fixtures["misnamed_path"])],
        log_dir,
        "v04_misnamed_html_as_pdf",
    )
    c_targz = run_cmd(
        [str(glassbox_bin), "inspect", str(fixtures["tar_gz_path"])],
        log_dir,
        "v04_compound_tar_gz",
    )
    c_extless = run_cmd(
        [str(glassbox_bin), "inspect", str(fixtures["extensionless_path"])],
        log_dir,
        "v04_extensionless",
    )
    c_djvu = run_cmd(
        [str(glassbox_bin), "inspect", str(djvu_fixture)],
        log_dir,
        "v04_djvu_ebook",
    )
    c_zip_as_docx = run_cmd(
        [str(glassbox_bin), "inspect", str(fixtures["zip_as_docx_path"])],
        log_dir,
        "v04_plain_zip_named_docx",
    )
    c_minimal_psd = run_cmd(
        [str(glassbox_bin), "inspect", str(fixtures["minimal_psd_path"])],
        log_dir,
        "v04_minimal_psd",
    )
    v4_cmds.extend(
        [c_misnamed, c_targz, c_extless, c_djvu, c_zip_as_docx, c_minimal_psd]
    )

    mis_parsed, mis_err = safe_json_loads(c_misnamed["stdout_text"])
    tgz_parsed, tgz_err = safe_json_loads(c_targz["stdout_text"])
    extless_parsed, extless_err = safe_json_loads(c_extless["stdout_text"])
    djvu_parsed, djvu_err = safe_json_loads(c_djvu["stdout_text"])
    zip_docx_parsed, zip_docx_err = safe_json_loads(c_zip_as_docx["stdout_text"])
    psd_parsed, psd_err = safe_json_loads(c_minimal_psd["stdout_text"])
    mis_rep = mis_parsed or {}
    tgz_rep = tgz_parsed or {}
    extless_rep = extless_parsed or {}
    djvu_rep = djvu_parsed or {}
    zip_docx_rep = zip_docx_parsed or {}
    psd_rep = psd_parsed or {}
    mis_fmt = mis_rep.get("format") if isinstance(mis_rep.get("format"), dict) else {}
    tgz_fmt = tgz_rep.get("format") if isinstance(tgz_rep.get("format"), dict) else {}
    extless_fmt = (
        extless_rep.get("format")
        if isinstance(extless_rep.get("format"), dict)
        else {}
    )
    djvu_fmt = (
        djvu_rep.get("format") if isinstance(djvu_rep.get("format"), dict) else {}
    )
    zip_docx_fmt = (
        zip_docx_rep.get("format")
        if isinstance(zip_docx_rep.get("format"), dict)
        else {}
    )
    psd_fmt = psd_rep.get("format") if isinstance(psd_rep.get("format"), dict) else {}
    djvu_ready = (
        djvu_rep.get("readiness")
        if isinstance(djvu_rep.get("readiness"), dict)
        else {}
    )
    large_fmt = (
        large_report.get("format")
        if isinstance(large_report.get("format"), dict)
        else {}
    )
    large_ready = (
        large_report.get("readiness")
        if isinstance(large_report.get("readiness"), dict)
        else {}
    )

    v4_checks.extend(
        [
            {
                "name": "misnamed_html_as_pdf_detected_as_web_with_mismatch",
                "ok": (
                    mis_err is None
                    and c_misnamed["returncode"] == 2
                    and mis_rep.get("status") == "unsupported"
                    and mis_fmt.get("type") == "web"
                    and mis_fmt.get("extension") == "pdf"
                    and mis_fmt.get("extension_mismatch") is True
                    and "HTML body sentinel" not in c_misnamed["stdout_text"]
                ),
                "detail": (
                    f"parse_err={mis_err}"
                    if mis_err
                    else f"rc={c_misnamed['returncode']}, format={mis_rep.get('format')}"
                ),
            },
            {
                "name": "compound_tar_gz_preserves_tar_gz_extension",
                "ok": (
                    tgz_err is None
                    and c_targz["returncode"] == 2
                    and tgz_rep.get("status") == "unsupported"
                    and tgz_fmt.get("type") == "archives"
                    and tgz_fmt.get("extension") == "tar.gz"
                    and "extension_mismatch" not in tgz_fmt
                ),
                "detail": (
                    f"parse_err={tgz_err}"
                    if tgz_err
                    else f"rc={c_targz['returncode']}, format={tgz_rep.get('format')}"
                ),
            },
            {
                "name": "extensionless_file_omits_extension_and_maps_to_other",
                "ok": (
                    extless_err is None
                    and c_extless["returncode"] == 2
                    and extless_rep.get("status") == "unsupported"
                    and extless_fmt.get("type") == "other"
                    and "extension" not in extless_fmt
                ),
                "detail": (
                    f"parse_err={extless_err}"
                    if extless_err
                    else f"rc={c_extless['returncode']}, format={extless_rep.get('format')}"
                ),
            },
            {
                "name": "djvu_classified_as_ebooks_identification_only",
                "ok": (
                    djvu_err is None
                    and c_djvu["returncode"] == 0
                    and djvu_fmt.get("type") == "ebooks"
                    and djvu_fmt.get("extension") == "djvu"
                    and djvu_ready.get("extractable") == "identification_only"
                ),
                "detail": (
                    f"parse_err={djvu_err}"
                    if djvu_err
                    else f"format={djvu_rep.get('format')}, readiness={djvu_rep.get('readiness')}"
                ),
            },
            {
                "name": "large_mp4_classified_as_video_not_applicable_ocr",
                "ok": (
                    large_err is None
                    and large_fmt.get("type") == "video"
                    and large_fmt.get("extension") == "mp4"
                    and large_ready.get("text_layer") == "not_applicable"
                    and large_ready.get("ocr") == "not_needed"
                ),
                "detail": (
                    f"parse_err={large_err}"
                    if large_err
                    else f"format={large_report.get('format')}, readiness={large_report.get('readiness')}"
                ),
            },
            {
                "name": "plain_zip_named_docx_classified_as_archives_with_mismatch",
                "ok": (
                    zip_docx_err is None
                    and zip_docx_fmt.get("type") == "archives"
                    and zip_docx_fmt.get("extension") == "docx"
                    and zip_docx_fmt.get("extension_mismatch") is True
                ),
                "detail": (
                    f"parse_err={zip_docx_err}"
                    if zip_docx_err
                    else f"rc={c_zip_as_docx['returncode']}, status={zip_docx_rep.get('status')}, format={zip_docx_rep.get('format')}"
                ),
            },
            {
                "name": "minimal_psd_classified_as_images_and_not_failed",
                "ok": (
                    psd_err is None
                    and psd_fmt.get("type") == "images"
                    and psd_fmt.get("extension") == "psd"
                    and psd_rep.get("status") != "failed"
                ),
                "detail": (
                    f"parse_err={psd_err}"
                    if psd_err
                    else f"rc={c_minimal_psd['returncode']}, status={psd_rep.get('status')}, format={psd_rep.get('format')}"
                ),
            },
        ]
    )
    return v4_checks, v4_cmds


def _run_v_io_05(
    glassbox_bin: Path,
    log_dir: Path,
    fixtures: dict[str, Path],
) -> tuple[list[dict], list[dict], dict]:
    """Run V-IO-05 strict XML marker round-trip, anti-forgery, and raw HTML escaping checks.

    Parameters:
      glassbox_bin: Path to the release `glassbox` executable.
      log_dir: Log directory for subprocess stdout/stderr capture.
      fixtures: Synthetic fixture map from `_build_e2e_fixtures`.

    Returns:
      Tuple `(checks, commands, shared_extracts)` where `shared_extracts` holds the
      PDF/XLSX/DOCX Markdown command outputs and parsed markers reused by V-IO-06.

    Failure modes:
      Raises DriverError if any verification subprocess fails to spawn or times out.
    """
    c_pdf_md_ext = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--format",
            "markdown",
            str(fixtures["pdf_path"]),
        ],
        log_dir,
        "v06_pdf_extract_md",
    )
    c_xlsx_md = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--format",
            "markdown",
            str(fixtures["xlsx_path"]),
        ],
        log_dir,
        "v05_xlsx_extract_md",
    )
    c_docx_md = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--format",
            "markdown",
            str(fixtures["docx_path"]),
        ],
        log_dir,
        "v06_docx_extract_md",
    )
    v5_cmds = [c_pdf_md_ext, c_xlsx_md, c_docx_md]

    pdf_markers, pdf_markers_err = safe_parse_markers(c_pdf_md_ext["stdout_text"])
    xlsx_markers, xlsx_markers_err = safe_parse_markers(c_xlsx_md["stdout_text"])
    docx_markers, docx_markers_err = safe_parse_markers(c_docx_md["stdout_text"])

    v5_checks = [
        {
            "name": "xlsx_hostile_sheet_names_strict_xml_roundtrip",
            "ok": (
                xlsx_markers_err is None
                and c_xlsx_md["returncode"] == 0
                and [(m["tag"], m["attrib"]) for m in xlsx_markers]
                == [
                    ("sheet", {"number": "1", "name": "Summary"}),
                    ("sheet", {"number": "2", "name": HOSTILE_SHEET_NAME}),
                    ("sheet", {"number": "3", "name": HOSTILE_SHEET_NAME_2}),
                ]
            ),
            "detail": (
                f"parse_err={xlsx_markers_err}"
                if xlsx_markers_err
                else f"parsed_markers={[(m['tag'], m['attrib']) for m in xlsx_markers]}"
            ),
        },
        {
            "name": "forged_markers_neutralized_across_pdf_xlsx_docx",
            "ok": (
                pdf_markers_err is None
                and xlsx_markers_err is None
                and docx_markers_err is None
                and NEUTRALIZED_PREFIX in c_pdf_md_ext["stdout_text"]
                and NEUTRALIZED_PREFIX in c_xlsx_md["stdout_text"]
                and NEUTRALIZED_PREFIX in c_docx_md["stdout_text"]
                and not any(
                    m["attrib"].get("number") == "99"
                    for m in (pdf_markers + xlsx_markers + docx_markers)
                )
            ),
            "detail": (
                f"parse_errs={{pdf={pdf_markers_err}, xlsx={xlsx_markers_err}, docx={docx_markers_err}}}"
                if any((pdf_markers_err, xlsx_markers_err, docx_markers_err))
                else "all forged bodymatter:v1 payloads neutralized with zero forged marker matches"
            ),
        },
        {
            "name": "raw_html_escaped_in_markdown_prose_and_cells",
            "ok": (
                "&lt;script>" in c_pdf_md_ext["stdout_text"]
                and "<script>" not in c_pdf_md_ext["stdout_text"]
                and "&lt;script>" in c_docx_md["stdout_text"]
                and "<script>" not in c_docx_md["stdout_text"]
                and "&lt;script>" in c_xlsx_md["stdout_text"]
                and "<script>" not in c_xlsx_md["stdout_text"]
            ),
            "detail": (
                f"escaped={{pdf={'&lt;script>' in c_pdf_md_ext['stdout_text']}, "
                f"docx={'&lt;script>' in c_docx_md['stdout_text']}, "
                f"xlsx={'&lt;script>' in c_xlsx_md['stdout_text']}}}, "
                f"raw_leaked={{pdf={'<script>' in c_pdf_md_ext['stdout_text']}, "
                f"docx={'<script>' in c_docx_md['stdout_text']}, "
                f"xlsx={'<script>' in c_xlsx_md['stdout_text']}}}"
            ),
        },
    ]
    shared_extracts = {
        "c_pdf_md_ext": c_pdf_md_ext,
        "c_xlsx_md": c_xlsx_md,
        "c_docx_md": c_docx_md,
        "pdf_markers": pdf_markers,
        "pdf_markers_err": pdf_markers_err,
        "docx_markers": docx_markers,
        "docx_markers_err": docx_markers_err,
    }
    return v5_checks, v5_cmds, shared_extracts


def _run_v_io_06(
    glassbox_bin: Path,
    log_dir: Path,
    fixtures: dict[str, Path],
    output_root: Path,
    shared_extracts: dict,
) -> tuple[list[dict], list[dict]]:
    """Run V-IO-06 per-format bodymatter markers, GFM tables, limits, and `--no-markers` parity.

    Parameters:
      glassbox_bin: Path to the release `glassbox` executable.
      log_dir: Log directory for subprocess stdout/stderr capture.
      fixtures: Synthetic fixture map from `_build_e2e_fixtures`.
      output_root: Sandbox root directory for `--output-dir` extraction artifacts.
      shared_extracts: Reusable PDF/XLSX/DOCX Markdown command outputs and parsed markers from V-IO-05.

    Returns:
      Tuple `(checks, commands)` for the `V-IO-06` verification matrix entry.

    Failure modes:
      Raises DriverError if any verification subprocess fails to spawn or times out.
    """
    pdf_path = fixtures["pdf_path"]
    xlsx_path = fixtures["xlsx_path"]
    docx_path = fixtures["docx_path"]
    csv_budget_path = fixtures["csv_budget_path"]

    c_pdf_md_ext = shared_extracts["c_pdf_md_ext"]
    c_xlsx_md = shared_extracts["c_xlsx_md"]
    c_docx_md = shared_extracts["c_docx_md"]
    pdf_markers = shared_extracts["pdf_markers"]
    pdf_markers_err = shared_extracts["pdf_markers_err"]
    docx_markers = shared_extracts["docx_markers"]
    docx_markers_err = shared_extracts["docx_markers_err"]

    c_pdf_md_nomarkers = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--format",
            "markdown",
            "--no-markers",
            str(pdf_path),
        ],
        log_dir,
        "v06_pdf_extract_md_nomarkers",
    )
    c_pdf_page2 = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--pages",
            "2-2",
            "--format",
            "markdown",
            str(pdf_path),
        ],
        log_dir,
        "v06_pdf_extract_page2",
    )
    c_pdf_page2_trunc = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--pages",
            "2-2",
            "--max-chars",
            "10",
            "--format",
            "markdown",
            str(pdf_path),
        ],
        log_dir,
        "v06_pdf_extract_page2_trunc",
    )
    c_xlsx_md_nomarkers = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--format",
            "markdown",
            "--no-markers",
            str(xlsx_path),
        ],
        log_dir,
        "v06_xlsx_extract_md_nomarkers",
    )
    c_xlsx_json = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--format",
            "json",
            str(xlsx_path),
        ],
        log_dir,
        "v06_xlsx_extract_json",
    )
    c_docx_md_nomarkers = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--format",
            "markdown",
            "--no-markers",
            str(docx_path),
        ],
        log_dir,
        "v06_docx_extract_md_nomarkers",
    )
    c_docx_break2 = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--pages",
            "8-8",
            "--format",
            "markdown",
            str(docx_path),
        ],
        log_dir,
        "v06_docx_extract_break2",
    )
    c_docx_table2 = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--pages",
            "6-6",
            "--format",
            "markdown",
            str(docx_path),
        ],
        log_dir,
        "v06_docx_extract_table2",
    )
    c_csv_budget = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--format",
            "markdown",
            "--max-chars",
            "5",
            "--max-tokens",
            "1",
            str(csv_budget_path),
        ],
        log_dir,
        "v06_csv_extract_budget",
    )

    # Saved --output-dir .md with and without --no-markers
    out_ext_default = output_root / "extract-default"
    out_ext_nomarkers = output_root / "extract-nomarkers"
    c_ext_out_default = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--output-dir",
            str(out_ext_default),
            str(pdf_path),
        ],
        log_dir,
        "v06_extract_output_dir_default",
    )
    c_ext_out_nomarkers = run_cmd(
        [
            str(glassbox_bin),
            "extract",
            "--no-ocr",
            "--no-markers",
            "--output-dir",
            str(out_ext_nomarkers),
            str(pdf_path),
        ],
        log_dir,
        "v06_extract_output_dir_nomarkers",
    )

    v6_cmds = [
        c_pdf_md_ext,
        c_pdf_md_nomarkers,
        c_pdf_page2,
        c_pdf_page2_trunc,
        c_xlsx_md,
        c_xlsx_md_nomarkers,
        c_xlsx_json,
        c_docx_md,
        c_docx_md_nomarkers,
        c_docx_break2,
        c_docx_table2,
        c_csv_budget,
        c_ext_out_default,
        c_ext_out_nomarkers,
    ]

    pdf_p2_markers, pdf_p2_err = safe_parse_markers(c_pdf_page2["stdout_text"])
    pdf_p2_trunc_markers, pdf_p2_trunc_err = safe_parse_markers(
        c_pdf_page2_trunc["stdout_text"]
    )
    docx_break2_markers, docx_break2_err = safe_parse_markers(
        c_docx_break2["stdout_text"]
    )
    docx_table2_markers, docx_table2_err = safe_parse_markers(
        c_docx_table2["stdout_text"]
    )
    xlsx_json_parsed, xlsx_json_err = safe_json_loads(c_xlsx_json["stdout_text"])
    xlsx_json_rep = xlsx_json_parsed or {}
    xlsx_content = (
        xlsx_json_rep.get("content")
        if isinstance(xlsx_json_rep.get("content"), dict)
        else {}
    )
    xlsx_units = (
        xlsx_content.get("units")
        if isinstance(xlsx_content.get("units"), list)
        else []
    )
    xlsx_sheet3_row1: list = []
    if len(xlsx_units) >= 3 and isinstance(xlsx_units[2], dict):
        sheet3_rows = xlsx_units[2].get("rows")
        if (
            isinstance(sheet3_rows, list)
            and len(sheet3_rows) >= 1
            and isinstance(sheet3_rows[0], dict)
            and isinstance(sheet3_rows[0].get("values"), list)
        ):
            xlsx_sheet3_row1 = sheet3_rows[0]["values"]

    pdf_eq_ok, pdf_eq_detail = check_marker_strip_equivalence(
        c_pdf_md_ext["stdout_text"], c_pdf_md_nomarkers["stdout_text"]
    )
    xlsx_eq_ok, xlsx_eq_detail = check_marker_strip_equivalence(
        c_xlsx_md["stdout_text"], c_xlsx_md_nomarkers["stdout_text"]
    )
    docx_eq_ok, docx_eq_detail = check_marker_strip_equivalence(
        c_docx_md["stdout_text"], c_docx_md_nomarkers["stdout_text"]
    )
    saved_default_files = (
        sorted(out_ext_default.glob("*.md")) if out_ext_default.is_dir() else []
    )
    saved_nomarkers_files = (
        sorted(out_ext_nomarkers.glob("*.md")) if out_ext_nomarkers.is_dir() else []
    )
    if len(saved_default_files) == 1 and len(saved_nomarkers_files) == 1:
        saved_default_md = saved_default_files[0].read_text(
            encoding="utf-8", errors="replace"
        )
        saved_nomarkers_md = saved_nomarkers_files[0].read_text(
            encoding="utf-8", errors="replace"
        )
        saved_eq_ok, saved_eq_detail = check_marker_strip_equivalence(
            saved_default_md, saved_nomarkers_md
        )
    else:
        saved_eq_ok = False
        saved_eq_detail = {
            "error": "expected exactly one .md file in each --output-dir",
            "default_md_files": [p.name for p in saved_default_files],
            "nomarkers_md_files": [p.name for p in saved_nomarkers_files],
        }

    v6_checks = [
        {
            "name": "pdf_page_labels_and_origin_markers",
            "ok": (
                pdf_markers_err is None
                and c_pdf_md_ext["returncode"] == 0
                and [(m["tag"], m["attrib"]) for m in pdf_markers]
                == [
                    ("page", {"number": "1", "label": "iv", "origin": "native"}),
                    ("page", {"number": "2", "label": "1", "origin": "native"}),
                ]
            ),
            "detail": (
                f"parse_err={pdf_markers_err}"
                if pdf_markers_err
                else f"markers={[(m['tag'], m['attrib']) for m in pdf_markers]}"
            ),
        },
        {
            "name": "pdf_partial_pages_and_truncation_completeness",
            "ok": (
                pdf_p2_err is None
                and pdf_p2_trunc_err is None
                and c_pdf_page2["returncode"] == 0
                and c_pdf_page2_trunc["returncode"] == 2
                and [(m["tag"], m["attrib"]) for m in pdf_p2_markers]
                == [("page", {"number": "2", "label": "1", "origin": "native"})]
                and [(m["tag"], m["attrib"]) for m in pdf_p2_trunc_markers]
                == [
                    (
                        "page",
                        {
                            "number": "2",
                            "label": "1",
                            "origin": "native",
                            "completeness": "partial",
                        },
                    )
                ]
            ),
            "detail": (
                f"parse_errs={{page2={pdf_p2_err}, trunc={pdf_p2_trunc_err}}}"
                if (pdf_p2_err or pdf_p2_trunc_err)
                else f"page2_rc={c_pdf_page2['returncode']}, trunc_rc={c_pdf_page2_trunc['returncode']}, page2={pdf_p2_markers}, trunc={pdf_p2_trunc_markers}"
            ),
        },
        {
            "name": "xlsx_gfm_pipe_table_and_no_duplicate_tsv",
            "ok": (
                c_xlsx_md["returncode"] == 0
                and "| Item | Qty |\n| --- | --- |\n| Widget | 42 |"
                in c_xlsx_md["stdout_text"]
                and "Item\tQty" not in c_xlsx_md["stdout_text"]
                and "Widget\t42" not in c_xlsx_md["stdout_text"]
            ),
            "detail": "GFM pipe table verified and duplicate TSV suppressed",
        },
        {
            "name": "xlsx_row1_long_header_and_numeric_preserved_json_and_md",
            "ok": (
                xlsx_json_err is None
                and c_xlsx_json["returncode"] == 0
                and xlsx_json_rep.get("status") == "complete"
                and xlsx_sheet3_row1 == [LONG_XLSX_HEADER, "2026"]
                and f"| {LONG_XLSX_HEADER} | 2026 |" in c_xlsx_md["stdout_text"]
            ),
            "detail": (
                f"parse_err={xlsx_json_err}"
                if xlsx_json_err
                else f"rc={c_xlsx_json['returncode']}, row1={xlsx_sheet3_row1}"
            ),
        },
        {
            "name": "docx_table_and_explicit_page_break_ignoring_soft_break",
            "ok": (
                docx_markers_err is None
                and c_docx_md["returncode"] == 0
                and [(m["tag"], m["attrib"]) for m in docx_markers]
                == [
                    ("table", {"number": "1"}),
                    ("page-break", {"number": "1"}),
                    ("table", {"number": "2"}),
                    ("page-break", {"number": "2"}),
                ]
                and "Intro paragraph with soft break." in c_docx_md["stdout_text"]
                and "Before explicit break." in c_docx_md["stdout_text"]
                and "After explicit break." in c_docx_md["stdout_text"]
                and "Before second break." in c_docx_md["stdout_text"]
                and "After second break." in c_docx_md["stdout_text"]
            ),
            "detail": (
                f"parse_err={docx_markers_err}"
                if docx_markers_err
                else f"markers={[(m['tag'], m['attrib']) for m in docx_markers]}"
            ),
        },
        {
            "name": "docx_partial_selection_preserves_source_ordinals",
            "ok": (
                docx_break2_err is None
                and docx_table2_err is None
                and c_docx_break2["returncode"] == 0
                and c_docx_table2["returncode"] == 0
                and [(m["tag"], m["attrib"]) for m in docx_break2_markers]
                == [("page-break", {"number": "2"})]
                and [(m["tag"], m["attrib"]) for m in docx_table2_markers]
                == [("table", {"number": "2"})]
            ),
            "detail": (
                f"parse_errs={{break2={docx_break2_err}, table2={docx_table2_err}}}"
                if (docx_break2_err or docx_table2_err)
                else f"break2={[(m['tag'], m['attrib']) for m in docx_break2_markers]}, table2={[(m['tag'], m['attrib']) for m in docx_table2_markers]}"
            ),
        },
        {
            "name": "csv_markdown_headers_respect_content_limits",
            "ok": (
                c_csv_budget["returncode"] == 2
                and "Alpha Beta Gamma Delta" not in c_csv_budget["stdout_text"]
                and "Long descriptive column header"
                not in c_csv_budget["stdout_text"]
                and "ValueOne" not in c_csv_budget["stdout_text"]
                and "ValueTwo" not in c_csv_budget["stdout_text"]
            ),
            "detail": f"rc={c_csv_budget['returncode']}, stdout={c_csv_budget['stdout_text']!r}",
        },
        {
            "name": "no_markers_strip_equivalence_stdout_and_output_dir",
            "ok": (
                c_pdf_md_ext["returncode"] == 0
                and c_pdf_md_nomarkers["returncode"] == 0
                and c_xlsx_md["returncode"] == 0
                and c_xlsx_md_nomarkers["returncode"] == 0
                and c_docx_md["returncode"] == 0
                and c_docx_md_nomarkers["returncode"] == 0
                and c_ext_out_default["returncode"] == 0
                and c_ext_out_nomarkers["returncode"] == 0
                and len(saved_default_files) == 1
                and len(saved_nomarkers_files) == 1
                and pdf_eq_ok
                and xlsx_eq_ok
                and docx_eq_ok
                and saved_eq_ok
            ),
            "detail": (
                f"rcs={{pdf_nm={c_pdf_md_nomarkers['returncode']}, "
                f"xlsx_nm={c_xlsx_md_nomarkers['returncode']}, "
                f"docx_nm={c_docx_md_nomarkers['returncode']}, "
                f"out_def={c_ext_out_default['returncode']}, "
                f"out_nm={c_ext_out_nomarkers['returncode']}}}, "
                f"md_counts={{def={len(saved_default_files)}, nm={len(saved_nomarkers_files)}}}, "
                f"pdf={pdf_eq_detail}, xlsx={xlsx_eq_detail}, docx={docx_eq_detail}, saved={saved_eq_detail}"
            ),
        },
    ]
    return v6_checks, v6_cmds


def _run_v_io_07(
    glassbox_bin: Path,
    log_dir: Path,
    intake_dir: Path,
    sandbox_dir: Path,
    fixtures: dict[str, Path],
) -> tuple[list[dict], list[dict], list[dict], list[str]]:
    """Run V-IO-07 directory inventory and batch inspect/extract JSONL recovery checks.

    Parameters:
      glassbox_bin: Path to the release `glassbox` executable.
      log_dir: Log directory for subprocess stdout/stderr capture.
      intake_dir: Directory containing Frank's 5-file mixed intake folder.
      sandbox_dir: Sandbox root used to construct an absent-file negative path.
      fixtures: Synthetic fixture map from `_build_e2e_fixtures`.

    Returns:
      Tuple `(checks, commands, inv_records, inv_parse_errs)` where `inv_records` and
      `inv_parse_errs` are reused by V-IO-09 single-hop routing synthesis.

    Failure modes:
      Raises DriverError if any verification subprocess fails to spawn or times out.
    """
    c_inv = run_cmd(
        [str(glassbox_bin), "inspect", str(intake_dir)],
        log_dir,
        "v07_inventory_intake",
    )
    inv_records, inv_parse_errs = safe_jsonl_loads(c_inv["stdout_text"])

    batch_inspect_input = (
        json.dumps({"id": "pdf-1", "path": str(fixtures["pdf_path"])})
        + "\nnot-valid-json\n"
        + json.dumps({"id": "media-1", "path": str(fixtures["large_path"])})
        + "\n"
        + json.dumps(
            {"id": "missing-1", "path": str(sandbox_dir / "absent-file.pdf")}
        )
        + "\n"
    ).encode("utf-8")
    c_batch_inspect = run_cmd(
        [str(glassbox_bin), "inspect", "--batch", "--verbose"],
        log_dir,
        "v07_batch_inspect",
        input_bytes=batch_inspect_input,
    )
    batch_inspect_recs, batch_inspect_errs = safe_jsonl_loads(
        c_batch_inspect["stdout_text"]
    )

    batch_extract_input = (
        json.dumps({"id": "xlsx-1", "path": str(fixtures["xlsx_path"])})
        + "\n{broken-jsonl\n"
        + json.dumps({"id": "docx-1", "path": str(fixtures["docx_path"])})
        + "\n"
    ).encode("utf-8")
    c_batch_extract = run_cmd(
        [str(glassbox_bin), "extract", "--batch", "--no-ocr"],
        log_dir,
        "v07_batch_extract",
        input_bytes=batch_extract_input,
    )
    batch_extract_recs, batch_extract_errs = safe_jsonl_loads(
        c_batch_extract["stdout_text"]
    )
    v7_cmds = [c_inv, c_batch_inspect, c_batch_extract]

    def _rec_report(recs: list[dict], idx: int) -> dict:
        if idx >= len(recs):
            return {}
        rep = recs[idx].get("report")
        return rep if isinstance(rep, dict) else {}

    bi0_rep = _rec_report(batch_inspect_recs, 0)
    bi2_rep = _rec_report(batch_inspect_recs, 2)
    bi2_file = bi2_rep.get("file") if isinstance(bi2_rep.get("file"), dict) else {}
    bi2_fp = (
        bi2_file.get("fingerprint")
        if isinstance(bi2_file.get("fingerprint"), dict)
        else {}
    )
    be0_rep = _rec_report(batch_extract_recs, 0)
    be0_meta = (
        be0_rep.get("metadata") if isinstance(be0_rep.get("metadata"), dict) else {}
    )
    be2_rep = _rec_report(batch_extract_recs, 2)

    v7_checks = [
        {
            "name": "directory_inventory_5_files_schema_v3",
            "ok": (
                not inv_parse_errs
                and c_inv["returncode"] == 2
                and len(inv_records) == 5
                and [r.get("sequence") for r in inv_records] == [1, 2, 3, 4, 5]
                and all(
                    isinstance(r.get("report"), dict)
                    and r["report"].get("schema_version") == 3
                    and r["report"].get("kind") == "inspect"
                    for r in inv_records
                )
            ),
            "detail": (
                f"parse_errs={inv_parse_errs}"
                if inv_parse_errs
                else f"count={len(inv_records)}, rc={c_inv['returncode']} (mixed complete+unsupported aggregate)"
            ),
        },
        {
            "name": "batch_inspect_recovery_schema_v3_and_verbose",
            "ok": (
                not batch_inspect_errs
                and c_batch_inspect["returncode"] == 64
                and len(batch_inspect_recs) == 4
                and [r.get("sequence") for r in batch_inspect_recs] == [1, 2, 3, 4]
                and batch_inspect_recs[0].get("id") == "pdf-1"
                and bi0_rep.get("schema_version") == 3
                and bool(bi0_rep.get("provenance"))
                and batch_inspect_recs[1].get("status") == "invalid"
                and isinstance(batch_inspect_recs[1].get("error"), dict)
                and batch_inspect_recs[2].get("id") == "media-1"
                and bi2_rep.get("schema_version") == 3
                and bi2_fp.get("scheme") == SAMPLED_SCHEME
                and batch_inspect_recs[3].get("id") == "missing-1"
                and batch_inspect_recs[3].get("status") == "failed"
            ),
            "detail": (
                f"parse_errs={batch_inspect_errs}"
                if batch_inspect_errs
                else f"rc={c_batch_inspect['returncode']}, statuses={[r.get('status') for r in batch_inspect_recs]}"
            ),
        },
        {
            "name": "batch_extract_recovery_schema_v3",
            "ok": (
                not batch_extract_errs
                and c_batch_extract["returncode"] == 64
                and len(batch_extract_recs) == 3
                and [r.get("sequence") for r in batch_extract_recs] == [1, 2, 3]
                and batch_extract_recs[0].get("id") == "xlsx-1"
                and be0_rep.get("schema_version") == 3
                and be0_rep.get("kind") == "extract"
                and be0_meta.get("schema_version") == 3
                and batch_extract_recs[1].get("status") == "invalid"
                and batch_extract_recs[2].get("id") == "docx-1"
                and be2_rep.get("schema_version") == 3
            ),
            "detail": (
                f"parse_errs={batch_extract_errs}"
                if batch_extract_errs
                else f"rc={c_batch_extract['returncode']}, statuses={[r.get('status') for r in batch_extract_recs]}"
            ),
        },
    ]
    return v7_checks, v7_cmds, inv_records, inv_parse_errs


def _run_v_io_08(
    glassbox_bin: Path,
    litx_bin: Path,
    log_dir: Path,
    target_dir: Path,
    ext_dir: Path,
    base_ref: str | None,
    source_before: dict,
    freshness: dict,
    binaries_info: dict,
) -> tuple[list[dict], list[dict], dict]:
    """Run V-IO-08 binary provenance, upstream isolation, parse baseline, and `litx` parity checks.

    Parameters:
      glassbox_bin: Path to the release `glassbox` executable.
      litx_bin: Path to the release `litx` alias executable.
      log_dir: Log directory for subprocess stdout/stderr capture.
      target_dir: Cargo target directory forwarded via `CARGO_TARGET_DIR` to `ci.py`.
      ext_dir: Downstream `extensionalities/` root directory for post-run source manifest check.
      base_ref: Optional git base ref for upstream isolation diff checks.
      source_before: Pre-run source manifest snapshot.
      freshness: Freshness/receipt validation dict from `_prepare_release_binaries`.
      binaries_info: Mutable metadata dict updated with `source_after` and `source_unchanged`.

    Returns:
      Tuple `(checks, commands, source_after)` for `V-IO-08` and payload reporting.

    Failure modes:
      Raises DriverError if any verification subprocess fails to spawn or times out.
    """
    native_pdf = EXT_DIR / "fixtures" / "native.pdf"
    hello_txt = EXT_DIR / "fixtures" / "hello.txt"

    c_parse_gb = run_cmd(
        [str(glassbox_bin), "parse", str(native_pdf)],
        log_dir,
        "v08_glassbox_parse",
    )
    c_parse_litx = run_cmd(
        [str(litx_bin), "parse", str(native_pdf)],
        log_dir,
        "v08_litx_parse",
    )
    c_insp_gb = run_cmd(
        [str(glassbox_bin), "inspect", str(hello_txt)],
        log_dir,
        "v08_glassbox_inspect_hello",
    )
    c_insp_litx = run_cmd(
        [str(litx_bin), "inspect", str(hello_txt)],
        log_dir,
        "v08_litx_inspect_hello",
    )
    ci_env = {**os.environ, "CARGO_TARGET_DIR": str(target_dir)}
    c_ci_smoke = run_cmd(
        [sys.executable, "-B", str(EXT_DIR / "scripts" / "ci.py"), "smoke"],
        log_dir,
        "v08_ci_smoke",
        env=ci_env,
    )
    c_ci_format = run_cmd(
        [sys.executable, "-B", str(EXT_DIR / "scripts" / "ci.py"), "format"],
        log_dir,
        "v08_ci_format",
        env=ci_env,
    )
    v8_cmds = [
        c_parse_gb,
        c_parse_litx,
        c_insp_gb,
        c_insp_litx,
        c_ci_smoke,
        c_ci_format,
    ]

    git_info = git_snapshot(base_ref=base_ref)
    source_after = compute_source_manifest(ext_dir)
    source_unchanged = (
        source_before["manifest_sha256"] == source_after["manifest_sha256"]
    )
    binaries_info["source_after"] = source_after
    binaries_info["source_unchanged"] = source_unchanged
    insp_gb_parsed, insp_gb_err = safe_json_loads(c_insp_gb["stdout_text"])
    insp_litx_parsed, insp_litx_err = safe_json_loads(c_insp_litx["stdout_text"])
    native_pdf_sha = sha256_file(native_pdf) if native_pdf.is_file() else ""
    parse_gb_sha = hashlib.sha256(c_parse_gb["stdout_bytes"]).hexdigest()
    upstream_iso = git_info.get("upstream_isolation", {})
    v8_checks = [
        {
            "name": "release_binaries_bound_to_source_manifest",
            "ok": (
                freshness["all_fresh"]
                and freshness["mtime_fresh"]
                and freshness["receipt_valid"]
            ),
            "detail": (
                f"all_fresh={freshness['all_fresh']}, "
                f"mtime_fresh={freshness['mtime_fresh']}, "
                f"receipt_valid={freshness['receipt_valid']}, "
                f"manifest_sha256={source_before['manifest_sha256'][:16]}, "
                f"newest_source={source_before['newest_path']}@{source_before['newest_mtime']}, "
                f"stale_binaries={freshness['stale_binaries']}, "
                f"receipt_errors={freshness['receipt_errors']}"
            ),
        },
        {
            "name": "source_snapshot_unchanged_during_run",
            "ok": source_unchanged,
            "detail": (
                f"before={source_before['manifest_sha256']}, "
                f"after={source_after['manifest_sha256']}, "
                f"files_before={source_before['file_count']}, "
                f"files_after={source_after['file_count']}"
            ),
        },
        {
            "name": "glassbox_parse_pinned_baseline",
            "ok": (
                native_pdf_sha == NATIVE_PDF_FIXTURE_SHA256
                and c_parse_gb["returncode"] == 0
                and c_parse_gb["stdout_text"] == NATIVE_PDF_PARSE_BASELINE_TEXT
                and parse_gb_sha == NATIVE_PDF_PARSE_BASELINE_SHA256
            ),
            "detail": (
                f"fixture_sha_match={native_pdf_sha == NATIVE_PDF_FIXTURE_SHA256}, "
                f"rc={c_parse_gb['returncode']}, parse_sha256={parse_gb_sha}, "
                f"expected_sha256={NATIVE_PDF_PARSE_BASELINE_SHA256}"
            ),
        },
        {
            "name": "glassbox_and_litx_parse_parity",
            "ok": (
                c_parse_gb["returncode"] == 0
                and c_parse_litx["returncode"] == 0
                and c_parse_gb["stdout_bytes"] == c_parse_litx["stdout_bytes"]
                and parse_gb_sha == NATIVE_PDF_PARSE_BASELINE_SHA256
            ),
            "detail": f"gb_rc={c_parse_gb['returncode']}, litx_rc={c_parse_litx['returncode']}, bytes={len(c_parse_gb['stdout_bytes'])}",
        },
        {
            "name": "glassbox_and_litx_inspect_v3_parity",
            "ok": (
                insp_gb_err is None
                and insp_litx_err is None
                and c_insp_gb["returncode"] == 0
                and c_insp_litx["returncode"] == 0
                and insp_gb_parsed == insp_litx_parsed
            ),
            "detail": (
                f"parse_errs={{gb={insp_gb_err}, litx={insp_litx_err}}}"
                if (insp_gb_err or insp_litx_err)
                else "glassbox and litx inspect outputs identical"
            ),
        },
        {
            "name": "ci_smoke_and_format_pass",
            "ok": c_ci_smoke["returncode"] == 0 and c_ci_format["returncode"] == 0,
            "detail": f"smoke_rc={c_ci_smoke['returncode']}, format_rc={c_ci_format['returncode']}",
        },
        {
            "name": "upstream_diff_empty",
            "ok": git_info["upstream_diff_empty"],
            "detail": (
                f"clean across unstaged/staged/committed({upstream_iso.get('base_ref')})/untracked for {UPSTREAM_PROTECTED_PATHS}"
                if git_info["upstream_diff_empty"]
                else f"upstream isolation failed (base_ref={upstream_iso.get('base_ref')}): {git_info['upstream_diff_preview']}"
            ),
        },
        {
            "name": "root_gitignore_preserves_extensionalities_dev",
            "ok": (
                git_info["gitignore_check_returncode"] == 0
                and "/extensionalities/dev/" in git_info["gitignore_check_output"]
            ),
            "detail": git_info["gitignore_check_output"],
        },
    ]
    return v8_checks, v8_cmds, source_after


def _run_v_io_09(
    inv_records: list[dict],
    inv_parse_errs: list[str],
    matrix: dict[str, dict],
) -> tuple[list[dict], list[dict]]:
    """Synthesize V-IO-09 end-to-end single-hop intake routing and gate aggregation checks.

    Parameters:
      inv_records: Parsed directory inventory JSONL records from V-IO-07.
      inv_parse_errs: Parse error strings from V-IO-07 directory inventory output.
      matrix: Verification matrix containing statuses for `V-IO-01` through `V-IO-08`.

    Returns:
      Tuple `(checks, commands)` for the `V-IO-09` verification matrix entry.

    Failure modes:
      Raises KeyError if any prerequisite gate (`V-IO-01`..`V-IO-08`) is absent from `matrix`.
    """
    by_name: dict[str, dict] = {}
    for rec in inv_records:
        rep = rec.get("report")
        if not isinstance(rep, dict):
            continue
        file_obj = rep.get("file")
        if not isinstance(file_obj, dict) or not isinstance(file_obj.get("path"), str):
            continue
        by_name[Path(file_obj["path"]).name] = rep

    def _fmt_of(name: str) -> dict:
        val = by_name.get(name, {}).get("format")
        return val if isinstance(val, dict) else {}

    def _counts_of(name: str) -> dict:
        struct_obj = by_name.get(name, {}).get("structure")
        if not isinstance(struct_obj, dict):
            return {}
        counts = struct_obj.get("counts")
        return counts if isinstance(counts, dict) else {}

    def _file_of(name: str) -> dict:
        val = by_name.get(name, {}).get("file")
        return val if isinstance(val, dict) else {}

    mp4_file = _file_of("05-large-capture.mp4")
    mp4_fp = (
        mp4_file.get("fingerprint")
        if isinstance(mp4_file.get("fingerprint"), dict)
        else {}
    )
    frank_routing_ok = (
        not inv_parse_errs
        and set(by_name.keys())
        == {
            "01-labeled-report.pdf",
            "02-financials.xlsx",
            "03-brief.docx",
            "report.pdf",
            "05-large-capture.mp4",
        }
        and _fmt_of("01-labeled-report.pdf").get("type") == "pdf"
        and _counts_of("01-labeled-report.pdf").get("pages") == 2
        and _fmt_of("02-financials.xlsx").get("type") == "spreadsheets"
        and _counts_of("02-financials.xlsx").get("sheets") == 3
        and _fmt_of("03-brief.docx").get("type") == "office_docs"
        and _counts_of("03-brief.docx").get("tables") == 2
        and _fmt_of("report.pdf").get("type") == "web"
        and _fmt_of("report.pdf").get("extension_mismatch") is True
        and _fmt_of("05-large-capture.mp4").get("type") == "video"
        and mp4_fp.get("scheme") == SAMPLED_SCHEME
        and "sha256" not in mp4_file
    )
    prereq_ids = (
        "V-IO-01",
        "V-IO-02",
        "V-IO-03",
        "V-IO-04",
        "V-IO-05",
        "V-IO-06",
        "V-IO-07",
        "V-IO-08",
    )
    v9_checks = [
        {
            "name": "frank_5_file_mixed_intake_single_hop_routing",
            "ok": frank_routing_ok,
            "detail": "all 5 heterogeneous intake files routed via v3 single-hop fields",
        },
        {
            "name": "all_supporting_ver_gates_pass",
            "ok": all(matrix[vid]["status"] == "PASS" for vid in prereq_ids),
            "detail": {vid: matrix[vid]["status"] for vid in prereq_ids},
        },
    ]
    return v9_checks, []


# ------------------------------------------------------------ Main E2E runner


def run_e2e(
    evidence_dir: Path,
    target_dir: Path,
    *,
    bench_receipt: Path = HASH_BENCH_RECEIPT,
    base_ref: str | None = None,
    build_binaries: bool = False,
    allow_stale_binary: bool = False,
    ext_dir: Path = EXT_DIR,
    exclusive_dir: bool = False,
) -> tuple[int, dict]:
    """Execute Frank's E2E dogfooding and boundary verification matrix (V-IO-01..V-IO-09).

    Parameters:
      evidence_dir: Destination evidence directory (must be non-symlink and unused).
      target_dir: Cargo target directory containing `release/glassbox` and `release/litx`.
      bench_receipt: Path to the V-IO-01 256 MiB hash benchmark receipt JSON.
      base_ref: Optional git base ref for upstream committed-diff isolation checks.
      build_binaries: When True, build release binaries and atomically write `.glassbox-build-receipt.json`.
      allow_stale_binary: Diagnostic/unit-test override to continue past stale/unreceipted binaries
        (V-IO-08 `release_binaries_bound_to_source_manifest` still fails).
      ext_dir: Downstream `extensionalities/` root directory.
      exclusive_dir: When True, require `evidence_dir` itself to be newly allocated via `mkdir(exist_ok=False)`.

    Returns:
      Tuple `(exit_code, result_payload)` where `exit_code` is 0 on PASS or 1 on FAIL.

    Failure modes:
      Raises DriverError if evidence allocation fails, release binaries are missing or fail
      provenance checks (mtime or `.glassbox-build-receipt.json`), source mutates during `--build`,
      or any subprocess fails to launch/times out.
    """
    started_all = time.perf_counter()
    log_dir = prepare_evidence_dir(
        evidence_dir, require_new_dir=exclusive_dir, create_logs=True
    )
    assert log_dir is not None

    # Compute source manifest BEFORE --build and BEFORE any E2E command runs.
    source_before = compute_source_manifest(ext_dir)
    glassbox_bin, litx_bin, freshness, binaries_info = _prepare_release_binaries(
        target_dir,
        ext_dir,
        log_dir,
        source_before,
        build_binaries=build_binaries,
        allow_stale_binary=allow_stale_binary,
    )

    SANDBOX_ROOT.mkdir(parents=True, exist_ok=True)
    sandbox_dir = SANDBOX_ROOT / f"io-verify-{os.getpid()}-{secrets.token_hex(4)}"
    intake_dir = sandbox_dir / "intake"
    boundary_dir = sandbox_dir / "boundary"
    output_root = sandbox_dir / "outputs"
    created_digests: dict[str, dict] = {}
    matrix: dict[str, dict] = {}
    timings_ms: dict[str, float] = {}
    source_after: dict | None = None

    def record_ver(ver_id: str, checks: list[dict], cmds: list[dict]) -> None:
        matrix[ver_id] = {
            "status": "PASS" if checks and all(c["ok"] for c in checks) else "FAIL",
            "checks": checks,
            "commands": [command_summary(c) for c in cmds],
        }

    try:
        fixtures, created_digests = _build_e2e_fixtures(
            sandbox_dir, intake_dir, boundary_dir, output_root
        )

        # V-IO-01: 256 MiB hash benchmark receipt validation
        v1_checks, v1_cmds = _run_v_io_01(bench_receipt)
        record_ver("V-IO-01", v1_checks, v1_cmds)

        # V-IO-02: sampled fingerprinting, --full-hash, and 128 MiB threshold boundary
        v2_checks, v2_cmds, v2_ctx = _run_v_io_02(
            glassbox_bin, log_dir, fixtures, created_digests, timings_ms
        )
        record_ver("V-IO-02", v2_checks, v2_cmds)

        # V-IO-03: schema v3 inspect envelope, summaries, flags, and --output-dir
        v3_checks, v3_cmds = _run_v_io_03(
            glassbox_bin,
            log_dir,
            fixtures,
            boundary_dir,
            output_root,
            v2_ctx["expected_fp"],
        )
        record_ver("V-IO-03", v3_checks, v3_cmds)

        # V-IO-04: format.type classification, compound extensions, and mismatches
        v4_checks, v4_cmds = _run_v_io_04(
            glassbox_bin,
            log_dir,
            fixtures,
            v2_ctx["large_report"],
            v2_ctx["large_err"],
        )
        record_ver("V-IO-04", v4_checks, v4_cmds)

        # V-IO-05: strict XML marker round-trip, anti-forgery, and raw HTML escaping
        v5_checks, v5_cmds, shared_extracts = _run_v_io_05(
            glassbox_bin, log_dir, fixtures
        )
        record_ver("V-IO-05", v5_checks, v5_cmds)

        # V-IO-06: per-format bodymatter markers, GFM tables, limits, and --no-markers parity
        v6_checks, v6_cmds = _run_v_io_06(
            glassbox_bin, log_dir, fixtures, output_root, shared_extracts
        )
        record_ver("V-IO-06", v6_checks, v6_cmds)

        # V-IO-07: directory inventory and batch inspect/extract JSONL recovery
        v7_checks, v7_cmds, inv_records, inv_parse_errs = _run_v_io_07(
            glassbox_bin, log_dir, intake_dir, sandbox_dir, fixtures
        )
        record_ver("V-IO-07", v7_checks, v7_cmds)

        # V-IO-08: binary provenance, upstream isolation, parse baseline, and litx parity
        v8_checks, v8_cmds, source_after = _run_v_io_08(
            glassbox_bin,
            litx_bin,
            log_dir,
            target_dir,
            ext_dir,
            base_ref,
            source_before,
            freshness,
            binaries_info,
        )
        record_ver("V-IO-08", v8_checks, v8_cmds)

        # V-IO-09: Frank end-to-end single-hop intake routing and gate aggregation
        v9_checks, v9_cmds = _run_v_io_09(inv_records, inv_parse_errs, matrix)
        record_ver("V-IO-09", v9_checks, v9_cmds)

    finally:
        if sandbox_dir.exists():
            shutil.rmtree(sandbox_dir)

    timings_ms["total_e2e_ms"] = round((time.perf_counter() - started_all) * 1000.0, 3)
    scratch_cleanup = {
        "sandbox_dir": str(sandbox_dir.relative_to(REPO_ROOT)),
        "created_fixtures_digests": created_digests,
        "removed": not sandbox_dir.exists(),
        "verified_absent": not sandbox_dir.exists(),
    }

    payload = build_result_payload(
        mode="e2e",
        verification_matrix=matrix,
        git_info=git_snapshot(base_ref=base_ref),
        toolchain_info=toolchain_snapshot(),
        binaries_info=binaries_info,
        timings_ms=timings_ms,
        scratch_cleanup=scratch_cleanup,
        evidence_dir=evidence_dir,
        source_before=source_before,
        source_after=source_after,
    )
    result_path = evidence_dir / "result.json"
    write_exclusive_bytes(
        result_path, (json.dumps(payload, indent=2) + "\n").encode("utf-8")
    )
    return (0 if payload["overall_status"] == "PASS" else 1), payload


def run_docs_only(
    evidence_dir: Path,
    *,
    base_ref: str | None = None,
    exclusive_dir: bool = False,
) -> tuple[int, dict]:
    """Execute V-IO-10 standalone documentation checks and retain exclusive evidence.

    Parameters:
      evidence_dir: Target evidence directory (non-symlink and unused).
      base_ref: Optional git base ref for upstream isolation snapshot.
      exclusive_dir: When True, allocate `evidence_dir` strictly via `mkdir(exist_ok=False)`.

    Returns:
      Tuple `(exit_code, result_payload)` where `exit_code` is 0 on PASS or 1 on FAIL.

    Failure modes:
      Raises DriverError if `evidence_dir` is unsafe/reused or exclusive `result.json` write fails.
    """
    started = time.perf_counter()
    prepare_evidence_dir(
        evidence_dir, require_new_dir=exclusive_dir, create_logs=False
    )
    doc_result = validate_docs_tree(EXT_DIR)
    matrix = {"V-IO-10": doc_result}
    payload = build_result_payload(
        mode="docs-only",
        verification_matrix=matrix,
        git_info=git_snapshot(base_ref=base_ref),
        toolchain_info=toolchain_snapshot(),
        binaries_info={},
        timings_ms={"docs_check_ms": round((time.perf_counter() - started) * 1000.0, 3)},
        scratch_cleanup={"sandbox_dir": None, "removed": True, "verified_absent": True},
        evidence_dir=evidence_dir,
    )
    result_path = evidence_dir / "result.json"
    write_exclusive_bytes(
        result_path, (json.dumps(payload, indent=2) + "\n").encode("utf-8")
    )
    return (0 if payload["overall_status"] == "PASS" else 1), payload


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Parse CLI arguments for the verification driver."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--evidence-dir",
        default=None,
        help=(
            "Evidence output directory inside extensionalities/target/verification/ "
            f"(default: unique {DEFAULT_EVIDENCE_BASE.relative_to(REPO_ROOT)}/"
            f"{DEFAULT_E2E_PREFIX}-<UTC>-<pid>-<rand8> for E2E or "
            f"{DEFAULT_DOCS_PREFIX}-<UTC>-<pid>-<rand8> for --docs-only)"
        ),
    )
    parser.add_argument(
        "--target-dir",
        default=None,
        help="Cargo target directory containing release/glassbox and release/litx",
    )
    parser.add_argument(
        "--bench-receipt",
        default=str(HASH_BENCH_RECEIPT),
        help="Path to the V-IO-01 256 MiB hash benchmark JSON receipt",
    )
    parser.add_argument(
        "--base-ref",
        default=None,
        help="Git base ref for upstream committed-diff check (default: origin/main, fallback main)",
    )
    parser.add_argument(
        "--build",
        action="store_true",
        help="Build release glassbox and litx binaries before running E2E verification",
    )
    parser.add_argument(
        "--allow-stale-binary",
        action="store_true",
        help="Unit-test/diagnostic override: do not abort early on stale binaries (V-IO-08 still fails)",
    )
    parser.add_argument(
        "--docs-only",
        action="store_true",
        help="Run V-IO-10 standalone documentation and link checks only",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    """Entry point for CLI execution."""
    try:
        args = parse_args(argv)
        is_default_evidence = args.evidence_dir is None
        evidence_dir = resolve_evidence_dir(args.evidence_dir, docs_only=args.docs_only)
        if args.docs_only:
            rc, payload = run_docs_only(
                evidence_dir,
                base_ref=args.base_ref,
                exclusive_dir=is_default_evidence,
            )
        else:
            target_dir = resolve_target_dir(args.target_dir)
            bench_receipt = resolve_repo_path(args.bench_receipt)
            rc, payload = run_e2e(
                evidence_dir,
                target_dir,
                bench_receipt=bench_receipt,
                base_ref=args.base_ref,
                build_binaries=args.build,
                allow_stale_binary=args.allow_stale_binary,
                exclusive_dir=is_default_evidence,
            )
        for ver_id, info in payload["verification_matrix"].items():
            print(f"{ver_id}: {info['status']}")
        print(
            f"OVERALL: {payload['overall_status']} "
            f"(evidence: {payload['evidence_dir']}/result.json)"
        )
        return rc
    except DriverError as exc:
        print(f"BLOCKED: {exc}", file=sys.stderr)
        return exc.exit_code


if __name__ == "__main__":
    sys.exit(main())

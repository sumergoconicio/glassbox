#!/usr/bin/env python3
"""Preview and optionally install selected Glassbox helper dependencies."""

import argparse
from dataclasses import dataclass
import errno
import hashlib
import hmac
import http.client
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from urllib.parse import urlparse
import urllib.request


TESSDATA_REVISION = "87416418657359cb625c412a48b6e1d6d41c29bd"
TESSDATA_BASE = f"https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/{TESSDATA_REVISION}"
MAX_PACK_BYTES = 64 * 1024 * 1024
DOWNLOAD_CHUNK_BYTES = 64 * 1024
TESSDATA_SHA256: dict[str, str] = {}
LANG_CODE_RE = re.compile(r"[a-z]{3}(?:_[A-Za-z]+)*")
SHA256_RE = re.compile(r"^[0-9a-fA-F]{64}$")
_VALUE_FLAGS = frozenset({"--receipt", "--tessdata-dir", "--tess-lang", "--tess-sha256"})


@dataclass(frozen=True)
class Step:
    helper: str
    action: str
    commands: tuple[tuple[str, ...], ...] = ()


def verified(argv, marker=None):
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=8)
        return result.returncode == 0 and (
            marker is None or marker in result.stdout or marker in result.stderr
        )
    except (OSError, subprocess.TimeoutExpired):
        return False


def package_command(system, helper):
    if system == "Linux" and (manager := shutil.which("apt-get")):
        packages = {"libreoffice": "libreoffice", "ffmpeg": "ffmpeg", "imagemagick": "imagemagick"}
        prefix = ()
        if os.geteuid() != 0:
            if not (sudo := shutil.which("sudo")):
                return None
            prefix = (sudo,)
        return (*prefix, manager, "install", packages[helper])
    if system == "Darwin" and (manager := shutil.which("brew")):
        if helper == "libreoffice":
            return (manager, "install", "--cask", "libreoffice")
        return (manager, "install", helper)
    if system == "Windows" and (manager := shutil.which("winget")):
        package = {
            "libreoffice": "TheDocumentFoundation.LibreOffice",
            "ffmpeg": "Gyan.FFmpeg",
            "imagemagick": "ImageMagick.ImageMagick",
        }
        return (manager, "install", "--source", "winget", "--exact", "--id", package[helper])
    return None


def find_libreoffice(system):
    candidates = [Path("/Applications/LibreOffice.app/Contents/MacOS/soffice")] if system == "Darwin" else []
    if system == "Windows":
        candidates = [Path(root) / "LibreOffice/program/soffice.com"
                      for key in ("ProgramFiles", "ProgramFiles(x86)")
                      if (root := os.environ.get(key))]
    for path in (shutil.which("libreoffice"), shutil.which("soffice"), *candidates):
        if path and verified((str(path), "--headless", "--version")):
            return str(Path(path).absolute())
    return None


def reject_symlinks(path: Path) -> Path:
    """Normalize a path lexically and reject any existing symlink component.

    Args:
        path: Candidate file or directory path, optionally starting with ``~``.

    Returns:
        The lexically normalized absolute ``Path`` without dereferencing symlinks.

    Raises:
        ValueError: If any existing path component from root down to ``path`` is a symlink.
    """
    normalized = Path(os.path.abspath(os.path.expanduser(str(path))))
    for prefix in (*reversed(normalized.parents), normalized):
        if prefix.is_symlink():
            raise ValueError(f"refusing symbolic-link path component: {prefix}")
    return normalized


def read_receipt(receipt: Path) -> dict:
    """Read and validate a helper receipt JSON object without following symlinks.

    Args:
        receipt: Path to the helper receipt JSON file.

    Returns:
        The parsed JSON object as a ``dict``.

    Raises:
        ValueError: If ``receipt`` traverses or is a symlink, is not a regular file,
            contains invalid JSON, or does not decode to a JSON object.
        OSError: If ``receipt`` does not exist or cannot be opened or read.
    """
    receipt = reject_symlinks(receipt)
    st = os.lstat(receipt)
    if stat.S_ISLNK(st.st_mode):
        raise ValueError(f"refusing symbolic-link receipt: {receipt}")
    if not stat.S_ISREG(st.st_mode):
        raise ValueError(f"helper receipt must be a regular file: {receipt}")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(receipt, flags)
    except OSError as error:
        if error.errno == errno.ELOOP:
            raise ValueError(f"refusing symbolic-link receipt: {receipt}") from error
        raise
    try:
        fd_stat = os.fstat(fd)
        if not stat.S_ISREG(fd_stat.st_mode):
            raise ValueError(f"helper receipt must be a regular file: {receipt}")
        with os.fdopen(fd, "r", encoding="utf-8") as handle:
            fd = -1
            raw = handle.read()
    finally:
        if fd >= 0:
            os.close(fd)
    settings = json.loads(raw)
    if not isinstance(settings, dict):
        raise ValueError("helper receipt must be a JSON object")
    return settings


def parse_sha256_overrides(entries) -> dict[str, str]:
    """Parse repeatable ``CODE=HEX`` SHA-256 override arguments.

    Args:
        entries: Iterable of ``CODE=HEX`` strings from ``--tess-sha256``.

    Returns:
        Mapping from language code to lowercase 64-character SHA-256 hex digest.

    Raises:
        ValueError: If any entry is missing ``=``, has an invalid language code,
            or has a digest that is not 64 hexadecimal characters.
    """
    overrides: dict[str, str] = {}
    for entry in entries:
        code, sep, digest = entry.partition("=")
        if not sep or not LANG_CODE_RE.fullmatch(code) or not SHA256_RE.fullmatch(digest):
            raise ValueError(
                "--tess-sha256 must use CODE=<64-hex> with a valid language code and SHA-256 digest"
            )
        overrides[code] = digest.lower()
    return overrides


def missing_languages(directory: Path, languages):
    """Return language codes whose ``.traineddata`` packs are missing or empty.

    Args:
        directory: Destination tessdata directory to inspect without following symlinks.
        languages: Iterable of Tesseract language codes to check.

    Returns:
        List of language codes that are not present as nonempty regular files.

    Raises:
        ValueError: If ``directory`` contains a symlink component or a target pack is a symlink.
    """
    directory = reject_symlinks(directory)
    missing = []
    for lang in languages:
        target = directory / f"{lang}.traineddata"
        try:
            st = target.lstat()
        except FileNotFoundError:
            missing.append(lang)
            continue
        if stat.S_ISLNK(st.st_mode):
            raise ValueError(f"refusing symbolic-link language pack: {target}")
        if not (stat.S_ISREG(st.st_mode) and st.st_size > 0):
            missing.append(lang)
    return missing


class _HttpsOnlyRedirectHandler(urllib.request.HTTPRedirectHandler):
    """HTTP redirect handler that permits redirects only to ``https://`` targets."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        """Validate a redirect target uses HTTPS before delegating to ``HTTPRedirectHandler``.

        Args:
            req: Original ``urllib.request.Request``.
            fp: Response file-like object.
            code: HTTP status code.
            msg: HTTP status reason phrase.
            headers: Response headers mapping.
            newurl: Resolved redirect destination URL.

        Returns:
            New ``urllib.request.Request`` built by the superclass.

        Raises:
            ValueError: If ``newurl`` does not use the ``https`` scheme.
        """
        if urlparse(newurl).scheme != "https":
            raise ValueError(f"refusing non-HTTPS redirect: {newurl}")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def fetch_language(directory: Path, language: str, expected_sha256: str | None = None, opener=None):
    """Download, SHA-256 verify, and atomically write a Tesseract language pack.

    Args:
        directory: Non-symlink destination directory for ``.traineddata`` files.
        language: Tesseract language code (such as ``eng`` or ``ara``).
        expected_sha256: Optional 64-character hex SHA-256 digest override. Falls back
            to ``TESSDATA_SHA256`` when omitted.
        opener: Optional ``urllib.request.OpenerDirector`` for testing; defaults to an
            HTTPS-only redirect opener.

    Returns:
        None.

    Raises:
        ValueError: If no valid SHA-256 digest is available, any URL or redirect is not
            HTTPS, the download is truncated or exceeds ``MAX_PACK_BYTES``, the
            ``Content-Length`` header is invalid or mismatched, the SHA-256 digest does
            not match, or a destination path component is a symlink.
        OSError: On network transport or filesystem errors.
    """
    expected = expected_sha256 or TESSDATA_SHA256.get(language)
    if not expected:
        raise ValueError(
            f"no pinned SHA-256 digest for {language}; pass --tess-sha256 {language}=<64-hex>"
        )
    if not isinstance(expected, str) or not SHA256_RE.fullmatch(expected):
        raise ValueError(f"invalid SHA-256 digest for {language}")

    directory = reject_symlinks(directory)
    target = reject_symlinks(directory / f"{language}.traineddata")
    url = f"{TESSDATA_BASE}/{target.name}"
    if urlparse(url).scheme != "https":
        raise ValueError(f"refusing non-HTTPS download URL: {url}")

    active_opener = opener if opener is not None else urllib.request.build_opener(_HttpsOnlyRedirectHandler)
    with active_opener.open(url, timeout=30) as response:
        final_url = response.geturl()
        if urlparse(final_url).scheme != "https":
            raise ValueError(f"refusing non-HTTPS final URL: {final_url}")
        chunks = []
        total = 0
        limit = MAX_PACK_BYTES + 1
        try:
            while total < limit:
                chunk = response.read(min(DOWNLOAD_CHUNK_BYTES, limit - total))
                if not chunk:
                    break
                chunks.append(chunk)
                total += len(chunk)
        except http.client.IncompleteRead as error:
            raise ValueError(f"incomplete download for {target.name}") from error
        data = b"".join(chunks)
        content_length = response.headers.get("Content-Length") if response.headers is not None else None

    if content_length is not None:
        try:
            expected_len = int(content_length.strip())
        except (AttributeError, TypeError, ValueError) as error:
            raise ValueError(f"invalid Content-Length header for {target.name}") from error
        if expected_len < 0:
            raise ValueError(f"invalid Content-Length header for {target.name}")
    else:
        expected_len = None

    if not data or len(data) > MAX_PACK_BYTES or (expected_len is not None and len(data) != expected_len):
        raise ValueError(f"invalid download size for {target.name}")

    digest = hashlib.sha256(data).hexdigest()
    if not hmac.compare_digest(digest, expected.lower()):
        raise ValueError(f"SHA-256 mismatch for {target.name}")

    reject_symlinks(directory)
    reject_symlinks(target)
    atomic_write(target, data)


def atomic_write(target: Path, data: bytes):
    """Write bytes atomically to a non-symlink target path via a sibling temp file.

    Args:
        target: Destination file path that must not traverse or be a symlink.
        data: Raw bytes to persist.

    Returns:
        None.

    Raises:
        ValueError: If ``target`` or any existing parent component is a symlink.
        OSError: If directory creation, temporary file writing, or replacement fails.
    """
    target = reject_symlinks(target)
    target.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=target.parent, delete=False) as handle:
        name = Path(handle.name)
        try:
            handle.write(data)
            handle.close()
            reject_symlinks(target)
            name.replace(target)
        finally:
            name.unlink(missing_ok=True)


def run_with_helpers(receipt: Path, command):
    """Run a command with validated LibreOffice and tessdata settings from a receipt.

    Args:
        receipt: Path to a non-symlink helper receipt JSON object file.
        command: Non-empty sequence of command arguments to execute.

    Returns:
        Exit status code returned by the child process.

    Raises:
        ValueError: If the receipt or its ``libreoffice``/``tessdata`` fields are
            invalid, relative, or traverse symlinks (for ``receipt`` or ``tessdata``).
        FileNotFoundError: If a recorded ``libreoffice`` file or ``tessdata`` directory
            does not exist.
        OSError: If the receipt cannot be read or the child process cannot be launched.
    """
    settings = read_receipt(receipt)
    env = dict(os.environ)
    if "libreoffice" in settings and settings["libreoffice"] is not None:
        office = settings["libreoffice"]
        if not isinstance(office, str) or not office:
            raise ValueError("receipt 'libreoffice' must be a non-empty string")
        office_path = Path(office)
        if not office_path.is_absolute():
            raise ValueError(f"recorded LibreOffice path must be absolute: {office}")
        if not office_path.is_file():
            raise FileNotFoundError(f"recorded LibreOffice executable is missing: {office_path}")
        existing_path = env.get("PATH", "")
        env["PATH"] = f"{office_path.parent}{os.pathsep}{existing_path}" if existing_path else str(office_path.parent)
    if "tessdata" in settings and settings["tessdata"] is not None:
        tessdata = settings["tessdata"]
        if not isinstance(tessdata, str) or not tessdata:
            raise ValueError("receipt 'tessdata' must be a non-empty string")
        tessdata_path = Path(tessdata)
        if not tessdata_path.is_absolute():
            raise ValueError(f"recorded tessdata path must be absolute: {tessdata}")
        tessdata_path = reject_symlinks(tessdata_path)
        if not tessdata_path.is_dir():
            raise FileNotFoundError(f"recorded tessdata directory is missing: {tessdata_path}")
        env["TESSDATA_PREFIX"] = str(tessdata_path)
    return subprocess.run(command, env=env).returncode


def find_imagemagick(system):
    # ImageMagick 6 uses convert on Unix. Windows also has a system convert command.
    names = ("magick",) if system == "Windows" else ("magick", "convert")
    for name in names:
        path = shutil.which(name)
        if path and verified((path, "-version"), marker="ImageMagick"):
            return path
    return None


def plan(helper, system):
    if helper == "libreoffice":
        executable = find_libreoffice(system)
    elif helper == "imagemagick":
        executable = find_imagemagick(system)
    else:
        executable = shutil.which("ffmpeg")
        if executable and not verified((executable, "-version")):
            executable = None
    if executable:
        return Step(helper, f"Available at {executable}")
    command = package_command(system, helper)
    if not command:
        return Step(helper, "Manual action required. This host needs a supported package manager")
    return Step(helper, "Install using the system package manager", (command,))


def _split_run_args(raw_argv: list[str]) -> tuple[list[str], list[str] | None]:
    """Split CLI arguments at the ``--run`` option and extract the trailing command.

    Walks ``raw_argv`` while skipping values consumed by value-taking flags
    (``--receipt``, ``--tessdata-dir``, ``--tess-lang``, ``--tess-sha256``),
    stops scanning if a bare ``--`` appears before ``--run``, and strips at
    most one leading ``--`` separator immediately after ``--run``.

    Args:
        raw_argv: Sequence of raw CLI argument strings without the program name.

    Returns:
        A ``(before, run_command)`` tuple where ``before`` contains tokens
        preceding ``--run`` and ``run_command`` is the command token list (or
        ``None`` when ``--run`` was not provided as an option).

    Raises:
        None.
    """
    index = 0
    while index < len(raw_argv):
        token = raw_argv[index]
        if token == "--":
            break
        if token in _VALUE_FLAGS:
            index += 2
            continue
        if token == "--run":
            before = raw_argv[:index]
            after = raw_argv[index + 1:]
            if after and after[0] == "--":
                after = after[1:]
            return before, after
        index += 1
    return raw_argv, None


def main(argv=None):
    """Parse CLI options, preview or apply helper installs, or run with saved helpers.

    Args:
        argv: Optional list of CLI argument strings; defaults to ``sys.argv[1:]``.

    Returns:
        Process exit status code (``0`` on success, non-zero on cancel or error).

    Raises:
        SystemExit: On argument parsing failures via ``argparse``.
        ValueError: If paths traverse symlinks, digests are missing or mismatched,
            or a receipt fails validation.
        OSError: On filesystem, network, or process execution failures.
        subprocess.CalledProcessError: If an approved installer command exits non-zero.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("helpers", nargs="*", choices=("libreoffice", "ffmpeg", "imagemagick"))
    parser.add_argument("--apply", action="store_true", help="Ask for approval before applying the displayed plan")
    parser.add_argument("--receipt", type=Path, default=Path.home() / ".config/glassbox/helpers.json")
    parser.add_argument("--tessdata-dir", type=Path, help="Destination for selected Tesseract language packs")
    parser.add_argument("--tess-lang", action="append", default=[], metavar="CODE",
                        help="Download a tessdata_fast language pack. Repeat for several languages")
    parser.add_argument("--tess-sha256", action="append", default=[], metavar="CODE=HEX",
                        help="Expected SHA-256 hex digest for a downloaded language pack. Repeat per language")
    parser.add_argument("--run", nargs=argparse.REMAINDER, help="Launch a command with the saved helper environment")
    raw_argv = list(sys.argv[1:] if argv is None else argv)
    before, run_command = _split_run_args(raw_argv)
    args = parser.parse_args(before)
    if run_command is not None:
        if (
            not run_command
            or args.helpers
            or args.apply
            or args.tess_lang
            or args.tess_sha256
            or args.tessdata_dir
        ):
            parser.error("--run needs a command and cannot be combined with installation")
        receipt = reject_symlinks(args.receipt)
        return run_with_helpers(receipt, run_command)
    receipt = reject_symlinks(args.receipt)
    if not (args.helpers or args.tess_lang):
        parser.error("select a helper or pass --tess-lang CODE")
    if any(not LANG_CODE_RE.fullmatch(code) for code in args.tess_lang):
        parser.error("--tess-lang must be a language code such as eng, ara or chi_sim")
    try:
        sha256_overrides = parse_sha256_overrides(args.tess_sha256)
    except ValueError as error:
        parser.error(str(error))

    system = platform.system()
    tessdata = reject_symlinks(args.tessdata_dir or receipt.parent / "tessdata")
    languages = list(dict.fromkeys(["eng", *args.tess_lang])) if args.tess_lang else []
    missing = missing_languages(tessdata, languages)
    steps = [plan(name, system) for name in dict.fromkeys(args.helpers)]
    commands = [command for step in steps for command in step.commands]
    for step in steps:
        print(f"{step.helper} — {step.action}")
        for command in step.commands:
            print("  " + repr(list(command)))
    for code in languages:
        suffix = f"download {TESSDATA_BASE}/{code}.traineddata" if code in missing else "already available"
        print(f"tessdata {code} — {suffix} in {tessdata}")
    record = "libreoffice" in args.helpers or bool(languages)
    if record:
        print(f"Save parse environment to {receipt}")
    if not args.apply:
        return 0
    if commands or missing or record:
        if not sys.stdin.isatty() or input("Type yes to apply the displayed plan: ") != "yes":
            print("Installation canceled.")
            return 1
    for command in commands:
        subprocess.run(command, check=True)
    for code in missing_languages(tessdata, missing):
        fetch_language(tessdata, code, expected_sha256=sha256_overrides.get(code))
    if record:
        settings = read_receipt(receipt) if (receipt.exists() or receipt.is_symlink()) else {}
        missing_office = False
        updated = False
        if "libreoffice" in args.helpers:
            office = find_libreoffice(system)
            if office is None:
                print("LibreOffice was not found after setup.", file=sys.stderr)
                missing_office = True
            else:
                settings["libreoffice"] = office
                updated = True
                print(f"LibreOffice executable: {office}")
        if languages:
            settings.update(tessdata=str(tessdata), tessdata_revision=TESSDATA_REVISION)
            updated = True
        if updated:
            atomic_write(receipt, (json.dumps(settings, indent=2) + "\n").encode())
        if missing_office:
            return 1
    manual = [step.helper for step in steps if step.action.startswith("Manual action required")]
    if manual:
        print(f"Manual setup still required for {', '.join(manual)}.")
        return 1
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)

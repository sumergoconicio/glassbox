"""Unit tests for security and validation invariants in install-helpers.py."""

from __future__ import annotations

import contextlib
import hashlib
import http.client
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import urllib.request


SCRIPT_PATH = Path(__file__).resolve().parents[1] / "install-helpers.py"


def load_install_helpers():
    """Load ``extensionalities/scripts/install-helpers.py`` as a Python module.

    Args:
        None.

    Returns:
        The loaded module object for ``install-helpers.py``.

    Raises:
        RuntimeError: If the module spec or loader cannot be created.
        OSError: If the script file cannot be read.
    """
    spec = importlib.util.spec_from_file_location("install_helpers", SCRIPT_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"unable to load module spec from {SCRIPT_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class _FakeResponse:
    """In-memory stand-in for a ``urllib`` HTTP response context manager."""

    def __init__(
        self,
        body: bytes = b"",
        *,
        url: str = "https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/rev/eng.traineddata",
        headers: dict[str, str] | None = None,
        raise_on_read: Exception | None = None,
    ):
        """Initialize a fake HTTP response for offline unit tests.

        Args:
            body: Raw response payload bytes.
            url: Final URL returned by ``geturl()``.
            headers: Optional mapping of HTTP response headers.
            raise_on_read: Optional exception to raise during ``read()``.

        Returns:
            None.

        Raises:
            None.
        """
        self._stream = io.BytesIO(body)
        self._url = url
        self.headers = {} if headers is None else dict(headers)
        self._raise_on_read = raise_on_read

    def __enter__(self):
        """Enter the context manager and return this fake response.

        Args:
            None.

        Returns:
            This ``_FakeResponse`` instance.

        Raises:
            None.
        """
        return self

    def __exit__(self, exc_type, exc, tb):
        """Exit the context manager without suppressing exceptions.

        Args:
            exc_type: Exception class if raised, else ``None``.
            exc: Exception instance if raised, else ``None``.
            tb: Traceback if raised, else ``None``.

        Returns:
            ``False`` so exceptions propagate normally.

        Raises:
            None.
        """
        return False

    def geturl(self) -> str:
        """Return the configured final response URL.

        Args:
            None.

        Returns:
            Final URL string.

        Raises:
            None.
        """
        return self._url

    def read(self, size: int = -1) -> bytes:
        """Read up to ``size`` bytes from the fake stream or raise a configured error.

        Args:
            size: Maximum number of bytes to return.

        Returns:
            Byte chunk read from the in-memory payload.

        Raises:
            Exception: Re-raises ``_raise_on_read`` when configured.
        """
        if self._raise_on_read is not None:
            raise self._raise_on_read
        return self._stream.read(size)


class _FakeOpener:
    """Minimal opener returning a predetermined fake response without network access."""

    def __init__(self, response: _FakeResponse):
        """Store the fake response to return from ``open()``.

        Args:
            response: ``_FakeResponse`` instance yielded on ``open()``.

        Returns:
            None.

        Raises:
            None.
        """
        self._response = response
        self.opened_urls: list[str] = []

    def open(self, url: str, timeout: int = 30) -> _FakeResponse:
        """Record the requested URL and return the fake response.

        Args:
            url: URL requested by ``fetch_language``.
            timeout: Socket timeout parameter (ignored in offline tests).

        Returns:
            The configured ``_FakeResponse``.

        Raises:
            None.
        """
        self.opened_urls.append(url)
        return self._response


class SymlinkRejectionTests(unittest.TestCase):
    """Verify SYS-T2-03 symlink rejection across receipt, tessdata, pack, and write paths."""

    @classmethod
    def setUpClass(cls):
        """Load the module under test once for this test case.

        Args:
            None.

        Returns:
            None.

        Raises:
            RuntimeError: If loading ``install-helpers.py`` fails.
        """
        cls.mod = load_install_helpers()

    def test_reject_symlinks_accepts_plain_path_without_resolving(self):
        """Confirm ``reject_symlinks`` normalizes lexically and accepts non-symlink paths.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If normalization alters a valid path unexpectedly.
        """
        with tempfile.TemporaryDirectory() as tmp:
            candidate = Path(tmp) / "sub" / ".." / "helpers.json"
            result = self.mod.reject_symlinks(candidate)
            self.assertEqual(result, Path(tmp) / "helpers.json")

    def test_rejects_symlinked_receipt_file_in_main_and_read_receipt(self):
        """Confirm symlinked ``--receipt`` files fail closed in ``read_receipt`` and ``main``.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If a symlinked receipt file is accepted.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            real_receipt = root / "real.json"
            real_receipt.write_text("{}\n", encoding="utf-8")
            link_receipt = root / "helpers.json"
            link_receipt.symlink_to(real_receipt)

            with self.assertRaises(ValueError):
                self.mod.read_receipt(link_receipt)
            with self.assertRaises(ValueError):
                self.mod.main(["--receipt", str(link_receipt), "--run", "true"])

    def test_rejects_symlinked_receipt_parent_directory(self):
        """Confirm symlinked parent directories of ``--receipt`` fail closed.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If a symlinked receipt parent directory is accepted.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            real_dir = root / "real_dir"
            real_dir.mkdir()
            link_dir = root / "link_dir"
            link_dir.symlink_to(real_dir)
            receipt = link_dir / "helpers.json"

            with self.assertRaises(ValueError):
                self.mod.reject_symlinks(receipt)
            with self.assertRaises(ValueError):
                self.mod.main(["--receipt", str(receipt), "--tess-lang", "eng"])

    def test_rejects_symlinked_tessdata_dir(self):
        """Confirm symlinked ``--tessdata-dir`` fails closed in ``missing_languages`` and ``main``.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If a symlinked tessdata directory is accepted.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            real_dir = root / "real_tessdata"
            real_dir.mkdir()
            link_dir = root / "link_tessdata"
            link_dir.symlink_to(real_dir)
            receipt = root / "helpers.json"

            with self.assertRaises(ValueError):
                self.mod.missing_languages(link_dir, ["eng"])
            with self.assertRaises(ValueError):
                self.mod.main([
                    "--receipt", str(receipt),
                    "--tessdata-dir", str(link_dir),
                    "--tess-lang", "eng",
                ])

    def test_rejects_symlinked_traineddata_pack_in_missing_languages(self):
        """Confirm ``missing_languages`` raises ``ValueError`` for valid or broken symlink packs.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If a symlinked ``.traineddata`` file is followed or ignored.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            tessdata = root / "tessdata"
            tessdata.mkdir()
            real_pack = root / "outside.traineddata"
            real_pack.write_bytes(b"payload")
            (tessdata / "eng.traineddata").symlink_to(real_pack)

            with self.assertRaises(ValueError):
                self.mod.missing_languages(tessdata, ["eng"])

            (tessdata / "ara.traineddata").symlink_to(root / "does-not-exist")
            with self.assertRaises(ValueError):
                self.mod.missing_languages(tessdata, ["ara"])

    def test_rejects_symlinked_target_and_parent_in_atomic_write(self):
        """Confirm ``atomic_write`` rejects both symlinked targets and symlinked parent directories.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If ``atomic_write`` writes through or replaces a symlink.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            outside = root / "outside.txt"
            outside.write_bytes(b"original")
            link_file = root / "target.txt"
            link_file.symlink_to(outside)

            with self.assertRaises(ValueError):
                self.mod.atomic_write(link_file, b"tampered")
            self.assertEqual(outside.read_bytes(), b"original")

            real_dir = root / "real_dir"
            real_dir.mkdir()
            link_dir = root / "link_dir"
            link_dir.symlink_to(real_dir)
            with self.assertRaises(ValueError):
                self.mod.atomic_write(link_dir / "file.txt", b"tampered")
            self.assertFalse((real_dir / "file.txt").exists())


class DownloadVerificationTests(unittest.TestCase):
    """Verify SYS-T2-04 HTTPS-only redirect, IncompleteRead, size, and SHA-256 enforcement."""

    @classmethod
    def setUpClass(cls):
        """Load the module under test once for this test case.

        Args:
            None.

        Returns:
            None.

        Raises:
            RuntimeError: If loading ``install-helpers.py`` fails.
        """
        cls.mod = load_install_helpers()

    def test_fetch_language_valid_sha256_writes_file_atomically(self):
        """Confirm ``fetch_language`` writes verified bytes when SHA-256 and headers match.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If the downloaded pack is missing or altered.
        """
        payload = b"verified-traineddata-bytes" * 10
        digest = hashlib.sha256(payload).hexdigest()
        opener = _FakeOpener(
            _FakeResponse(payload, headers={"Content-Length": str(len(payload))})
        )
        with tempfile.TemporaryDirectory() as tmp:
            tessdata = Path(tmp) / "tessdata"
            self.mod.fetch_language(tessdata, "eng", expected_sha256=digest.upper(), opener=opener)
            target = tessdata / "eng.traineddata"
            self.assertTrue(target.is_file())
            self.assertEqual(target.read_bytes(), payload)

    def test_fetch_language_sha256_mismatch_writes_nothing(self):
        """Confirm a mismatched SHA-256 raises ``ValueError`` and writes no output file.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If a file is written despite SHA-256 mismatch.
        """
        payload = b"unexpected-bytes"
        wrong_digest = "0" * 64
        opener = _FakeOpener(_FakeResponse(payload, headers={"Content-Length": str(len(payload))}))
        with tempfile.TemporaryDirectory() as tmp:
            tessdata = Path(tmp) / "tessdata"
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                self.mod.fetch_language(tessdata, "eng", expected_sha256=wrong_digest, opener=opener)
            self.assertFalse((tessdata / "eng.traineddata").exists())

    def test_fetch_language_missing_or_invalid_sha256_fails_closed(self):
        """Confirm missing or malformed SHA-256 digests fail closed before opening any URL.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If ``fetch_language`` proceeds without a valid digest.
        """
        opener = _FakeOpener(_FakeResponse(b"ignored"))
        with tempfile.TemporaryDirectory() as tmp:
            tessdata = Path(tmp) / "tessdata"
            with self.assertRaisesRegex(ValueError, "no pinned SHA-256 digest"):
                self.mod.fetch_language(tessdata, "eng", expected_sha256=None, opener=opener)
            with self.assertRaisesRegex(ValueError, "invalid SHA-256 digest"):
                self.mod.fetch_language(tessdata, "eng", expected_sha256="not-hex", opener=opener)
            self.assertEqual(opener.opened_urls, [])

    def test_https_only_redirect_handler_blocks_non_https_and_allows_https(self):
        """Confirm ``_HttpsOnlyRedirectHandler`` rejects non-HTTPS targets and permits HTTPS.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If non-HTTPS redirects are allowed or HTTPS redirects fail.
        """
        handler = self.mod._HttpsOnlyRedirectHandler()
        req = urllib.request.Request("https://example.com/start")
        for bad_url in ("http://example.com/downgrade", "file:///etc/passwd", "ftp://example.com/x"):
            with self.assertRaisesRegex(ValueError, "refusing non-HTTPS redirect"):
                handler.redirect_request(req, None, 302, "Found", {}, bad_url)

        redirected = handler.redirect_request(req, None, 302, "Found", {}, "https://example.com/next")
        self.assertIsNotNone(redirected)
        self.assertEqual(redirected.full_url, "https://example.com/next")

    def test_fetch_language_rejects_non_https_final_url(self):
        """Confirm ``fetch_language`` rejects responses whose final URL is not HTTPS.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If an HTTP final URL is accepted.
        """
        payload = b"abc"
        digest = hashlib.sha256(payload).hexdigest()
        opener = _FakeOpener(_FakeResponse(payload, url="http://example.com/eng.traineddata"))
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ValueError, "refusing non-HTTPS final URL"):
                self.mod.fetch_language(Path(tmp), "eng", expected_sha256=digest, opener=opener)

    def test_fetch_language_converts_incomplete_read_to_value_error(self):
        """Confirm ``http.client.IncompleteRead`` is translated into ``ValueError`` with no write.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If ``IncompleteRead`` escapes or writes a partial file.
        """
        digest = hashlib.sha256(b"complete").hexdigest()
        opener = _FakeOpener(
            _FakeResponse(raise_on_read=http.client.IncompleteRead(b"part", expected=8))
        )
        with tempfile.TemporaryDirectory() as tmp:
            tessdata = Path(tmp) / "tessdata"
            with self.assertRaisesRegex(ValueError, "incomplete download"):
                self.mod.fetch_language(tessdata, "eng", expected_sha256=digest, opener=opener)
            self.assertFalse((tessdata / "eng.traineddata").exists())

    def test_fetch_language_rejects_invalid_and_mismatched_content_length_and_oversized(self):
        """Confirm invalid ``Content-Length``, mismatched size, empty, and oversized bodies fail.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If malformed or oversized downloads are accepted.
        """
        payload = b"0123456789abcdef"
        digest = hashlib.sha256(payload).hexdigest()
        with tempfile.TemporaryDirectory() as tmp:
            tessdata = Path(tmp) / "tessdata"

            for bad_header in ("not-an-int", "-5", ""):
                opener = _FakeOpener(_FakeResponse(payload, headers={"Content-Length": bad_header}))
                with self.assertRaisesRegex(ValueError, "invalid Content-Length"):
                    self.mod.fetch_language(tessdata, "eng", expected_sha256=digest, opener=opener)

            mismatch_opener = _FakeOpener(_FakeResponse(payload, headers={"Content-Length": "999"}))
            with self.assertRaisesRegex(ValueError, "invalid download size"):
                self.mod.fetch_language(tessdata, "eng", expected_sha256=digest, opener=mismatch_opener)

            empty_opener = _FakeOpener(_FakeResponse(b""))
            with self.assertRaisesRegex(ValueError, "invalid download size"):
                self.mod.fetch_language(tessdata, "eng", expected_sha256=digest, opener=empty_opener)

            oversized = b"x" * 17
            oversized_digest = hashlib.sha256(oversized).hexdigest()
            oversized_opener = _FakeOpener(_FakeResponse(oversized))
            with mock.patch.object(self.mod, "MAX_PACK_BYTES", 16):
                with self.assertRaisesRegex(ValueError, "invalid download size"):
                    self.mod.fetch_language(
                        tessdata, "eng", expected_sha256=oversized_digest, opener=oversized_opener
                    )

    def test_parse_sha256_overrides(self):
        """Confirm ``parse_sha256_overrides`` normalizes valid entries and rejects bad ones.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If parsing accepts malformed input or alters valid digests.
        """
        digest = "A" * 64
        parsed = self.mod.parse_sha256_overrides([f"eng={digest}", f"chi_sim={'b' * 64}"])
        self.assertEqual(parsed, {"eng": "a" * 64, "chi_sim": "b" * 64})
        for invalid in ("eng", "123=" + "a" * 64, "eng=deadbeef", "=a" * 32):
            with self.assertRaises(ValueError):
                self.mod.parse_sha256_overrides([invalid])


class ReceiptValidationAndEnvTests(unittest.TestCase):
    """Verify SYS-T2-05 receipt JSON validation, path checks, and PATH construction."""

    @classmethod
    def setUpClass(cls):
        """Load the module under test once for this test case.

        Args:
            None.

        Returns:
            None.

        Raises:
            RuntimeError: If loading ``install-helpers.py`` fails.
        """
        cls.mod = load_install_helpers()

    def test_rejects_non_dict_receipt_json(self):
        """Confirm receipts containing JSON arrays, strings, or numbers raise ``ValueError``.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If a non-object JSON receipt is accepted.
        """
        with tempfile.TemporaryDirectory() as tmp:
            receipt = Path(tmp) / "helpers.json"
            for raw in ("[]", '"hello"', "42"):
                receipt.write_text(raw, encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "JSON object"):
                    self.mod.run_with_helpers(receipt, ["true"])

    def test_rejects_non_string_and_relative_and_missing_paths(self):
        """Confirm invalid types, empty strings, relative paths, and missing targets fail closed.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If malformed receipt fields pass validation.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            receipt = root / "helpers.json"

            for bad_payload in (
                {"libreoffice": 123},
                {"libreoffice": ""},
                {"libreoffice": "relative/soffice"},
                {"tessdata": ["not-a-str"]},
                {"tessdata": ""},
                {"tessdata": "relative/tessdata"},
            ):
                receipt.write_text(json.dumps(bad_payload), encoding="utf-8")
                with self.assertRaises(ValueError):
                    self.mod.run_with_helpers(receipt, ["true"])

            receipt.write_text(json.dumps({"libreoffice": str(root / "missing-soffice")}), encoding="utf-8")
            with self.assertRaises(FileNotFoundError):
                self.mod.run_with_helpers(receipt, ["true"])

            receipt.write_text(json.dumps({"tessdata": str(root / "missing-tessdata")}), encoding="utf-8")
            with self.assertRaises(FileNotFoundError):
                self.mod.run_with_helpers(receipt, ["true"])

    def test_rejects_symlinked_tessdata_in_receipt(self):
        """Confirm ``run_with_helpers`` rejects symlinked ``tessdata`` directories.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If a symlinked ``tessdata`` directory is accepted.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            real_tessdata = root / "real_tessdata"
            real_tessdata.mkdir()
            link_tessdata = root / "link_tessdata"
            link_tessdata.symlink_to(real_tessdata)
            receipt = root / "helpers.json"
            receipt.write_text(json.dumps({"tessdata": str(link_tessdata)}), encoding="utf-8")

            with self.assertRaises(ValueError):
                self.mod.run_with_helpers(receipt, ["true"])

    def test_path_construction_for_empty_unset_and_nonempty_path(self):
        """Confirm ``PATH`` has no trailing separator when empty/unset and prepends when non-empty.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If ``PATH`` or ``TESSDATA_PREFIX`` is constructed incorrectly.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            bin_dir = root / "office-bin"
            bin_dir.mkdir()
            office = bin_dir / "soffice"
            office.write_text("#!/bin/sh\n", encoding="utf-8")
            tessdata = root / "tessdata"
            tessdata.mkdir()
            receipt = root / "helpers.json"
            receipt.write_text(
                json.dumps({"libreoffice": str(office), "tessdata": str(tessdata)}),
                encoding="utf-8",
            )

            captured_envs: list[dict[str, str]] = []

            def fake_run(command, env):
                captured_envs.append(dict(env))
                return mock.Mock(returncode=0)

            with mock.patch.object(self.mod.subprocess, "run", side_effect=fake_run):
                with mock.patch.dict(os.environ, {"PATH": ""}, clear=True):
                    self.assertEqual(self.mod.run_with_helpers(receipt, ["cmd"]), 0)
                with mock.patch.dict(os.environ, {}, clear=True):
                    self.assertEqual(self.mod.run_with_helpers(receipt, ["cmd"]), 0)
                with mock.patch.dict(os.environ, {"PATH": "/usr/bin"}, clear=True):
                    self.assertEqual(self.mod.run_with_helpers(receipt, ["cmd"]), 0)

            self.assertEqual(captured_envs[0]["PATH"], str(bin_dir))
            self.assertFalse(captured_envs[0]["PATH"].endswith(os.pathsep))
            self.assertEqual(captured_envs[1]["PATH"], str(bin_dir))
            self.assertFalse(captured_envs[1]["PATH"].endswith(os.pathsep))
            self.assertEqual(captured_envs[2]["PATH"], f"{bin_dir}{os.pathsep}/usr/bin")
            self.assertEqual(captured_envs[0]["TESSDATA_PREFIX"], str(tessdata))


class RunArgumentAndPartialReceiptTests(unittest.TestCase):
    """Verify ISS-T5-02-01 ``--run --`` parsing and ISS-T5-02-02 partial receipt persistence."""

    @classmethod
    def setUpClass(cls):
        """Load the module under test once for this test case.

        Args:
            None.

        Returns:
            None.

        Raises:
            RuntimeError: If loading ``install-helpers.py`` fails.
        """
        cls.mod = load_install_helpers()

    def test_run_accepts_double_dash_separator_and_bare_command(self):
        """Confirm ``--run --`` strips one leading separator and ``--run <cmd>`` stays compatible.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If ``run_with_helpers`` receives the wrong receipt or command tokens.
        """
        with tempfile.TemporaryDirectory() as tmp:
            receipt = Path(tmp) / "helpers.json"
            calls: list[tuple[Path, list[str]]] = []

            def fake_run(path: Path, command: list[str]) -> int:
                calls.append((path, list(command)))
                return 0

            with mock.patch.object(self.mod, "run_with_helpers", side_effect=fake_run):
                self.assertEqual(self.mod.main(["--run", "--", "echo", "hi"]), 0)
                self.assertEqual(
                    self.mod.main(["--receipt", str(receipt), "--run", "--", "echo", "--flag"]),
                    0,
                )
                self.assertEqual(self.mod.main(["--run", "echo", "hi"]), 0)
                self.assertEqual(self.mod.main(["--run", "--", "--flag"]), 0)
                self.assertEqual(self.mod.main(["--run", "--", "--", "--flag"]), 0)

            self.assertEqual(calls[0][1], ["echo", "hi"])
            self.assertEqual(calls[1], (receipt, ["echo", "--flag"]))
            self.assertEqual(calls[2][1], ["echo", "hi"])
            self.assertEqual(calls[3][1], ["--flag"])
            self.assertEqual(calls[4][1], ["--", "--flag"])

    def test_split_run_args_skips_flag_values_and_stops_at_bare_double_dash(self):
        """Confirm ``_split_run_args`` ignores ``--run`` as a flag value or after a bare ``--``.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If ``_split_run_args`` misclassifies ``--run`` tokens.
        """
        before, cmd = self.mod._split_run_args(["--receipt", "--run", "--run", "--", "echo", "ok"])
        self.assertEqual(before, ["--receipt", "--run"])
        self.assertEqual(cmd, ["echo", "ok"])

        raw = ["--", "--run", "echo", "hi"]
        self.assertEqual(self.mod._split_run_args(raw), (raw, None))

    def test_run_rejects_empty_command_and_install_flag_combinations(self):
        """Confirm empty ``--run --`` and install option combinations exit with status 2.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If invalid ``--run`` invocations do not fail with argparse exit code 2.
        """
        invalid_invocations = [
            ["--run", "--"],
            ["--run"],
            ["--tessdata-dir", "/tmp", "--run", "--", "echo", "hi"],
            ["--apply", "--run", "--", "echo", "hi"],
            ["libreoffice", "--run", "--", "echo", "hi"],
            ["--tess-lang", "ara", "--run", "--", "echo", "hi"],
            ["--tess-sha256", f"ara={'a' * 64}", "--run", "--", "echo", "hi"],
            ["--", "--run", "echo", "hi"],
        ]
        for argv in invalid_invocations:
            with self.subTest(argv=argv):
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    with self.assertRaises(SystemExit) as ctx:
                        self.mod.main(argv)
                self.assertEqual(ctx.exception.code, 2)

    def test_partial_receipt_persists_tessdata_when_libreoffice_missing_after_setup(self):
        """Confirm completed tessdata settings persist when co-requested LibreOffice is missing.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If tessdata settings are dropped, existing keys are lost,
                stderr omits the LibreOffice error, or ``main`` does not return 1.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            receipt = root / "helpers.json"
            receipt.write_text(json.dumps({"existing": "keep"}) + "\n", encoding="utf-8")
            tessdata = root / "tessdata"
            fetched: list[tuple[Path, str, str | None]] = []

            def fake_fetch(directory: Path, language: str, expected_sha256=None, opener=None):
                fetched.append((directory, language, expected_sha256))
                directory.mkdir(parents=True, exist_ok=True)
                (directory / f"{language}.traineddata").write_bytes(b"pack")

            stdout = io.StringIO()
            stderr = io.StringIO()
            with (
                mock.patch.object(self.mod, "find_libreoffice", return_value=None),
                mock.patch.object(self.mod, "package_command", return_value=("echo", "install")),
                mock.patch.object(self.mod.subprocess, "run", return_value=mock.Mock(returncode=0)),
                mock.patch.object(self.mod, "fetch_language", side_effect=fake_fetch),
                mock.patch.object(self.mod.sys.stdin, "isatty", return_value=True),
                mock.patch("builtins.input", return_value="yes"),
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                rc = self.mod.main([
                    "libreoffice",
                    "--receipt", str(receipt),
                    "--tessdata-dir", str(tessdata),
                    "--tess-lang", "ara",
                    "--tess-sha256", f"eng={'1' * 64}",
                    "--tess-sha256", f"ara={'2' * 64}",
                    "--apply",
                ])

            self.assertEqual(rc, 1)
            self.assertIn("LibreOffice was not found after setup.", stderr.getvalue())
            self.assertEqual([lang for _, lang, _ in fetched], ["eng", "ara"])
            saved = json.loads(receipt.read_text(encoding="utf-8"))
            self.assertEqual(
                saved,
                {
                    "existing": "keep",
                    "tessdata": str(tessdata),
                    "tessdata_revision": self.mod.TESSDATA_REVISION,
                },
            )

    def test_libreoffice_only_missing_after_setup_writes_no_receipt(self):
        """Confirm requesting only LibreOffice writes no receipt when it is missing after setup.

        Args:
            None.

        Returns:
            None.

        Raises:
            AssertionError: If a spurious receipt is created or ``main`` does not return 1.
        """
        with tempfile.TemporaryDirectory() as tmp:
            receipt = Path(tmp) / "helpers.json"
            stdout = io.StringIO()
            stderr = io.StringIO()
            with (
                mock.patch.object(self.mod, "find_libreoffice", return_value=None),
                mock.patch.object(self.mod, "package_command", return_value=("echo", "install")),
                mock.patch.object(self.mod.subprocess, "run", return_value=mock.Mock(returncode=0)),
                mock.patch.object(self.mod.sys.stdin, "isatty", return_value=True),
                mock.patch("builtins.input", return_value="yes"),
                contextlib.redirect_stdout(stdout),
                contextlib.redirect_stderr(stderr),
            ):
                rc = self.mod.main(["libreoffice", "--receipt", str(receipt), "--apply"])

            self.assertEqual(rc, 1)
            self.assertIn("LibreOffice was not found after setup.", stderr.getvalue())
            self.assertFalse(receipt.exists())


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Preview and optionally install selected Glassbox helper dependencies."""

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from urllib.request import urlopen


TESSDATA_REVISION = "87416418657359cb625c412a48b6e1d6d41c29bd"
TESSDATA_BASE = f"https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/{TESSDATA_REVISION}"
MAX_PACK_BYTES = 64 * 1024 * 1024


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


def missing_languages(directory, languages):
    return [lang for lang in languages
            if not ((directory / f"{lang}.traineddata").is_file()
                    and (directory / f"{lang}.traineddata").stat().st_size > 0)]


def fetch_language(directory, language):
    target = directory / f"{language}.traineddata"
    directory.mkdir(parents=True, exist_ok=True)
    with urlopen(f"{TESSDATA_BASE}/{target.name}", timeout=30) as response:
        data = response.read(MAX_PACK_BYTES + 1)
        expected = response.headers.get("Content-Length")
    if not data or len(data) > MAX_PACK_BYTES or (expected and len(data) != int(expected)):
        raise ValueError(f"invalid download size for {target.name}")
    atomic_write(target, data)


def atomic_write(target, data):
    target.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=target.parent, delete=False) as handle:
        name = Path(handle.name)
        try:
            handle.write(data)
            handle.close()
            name.replace(target)
        finally:
            name.unlink(missing_ok=True)


def run_with_helpers(receipt, command):
    settings = json.loads(receipt.read_text())
    env = dict(os.environ)
    if office := settings.get("libreoffice"):
        office = Path(office)
        if not office.is_file():
            raise FileNotFoundError(f"recorded LibreOffice executable is missing: {office}")
        env["PATH"] = str(office.parent) + os.pathsep + env.get("PATH", "")
    if tessdata := settings.get("tessdata"):
        env["TESSDATA_PREFIX"] = tessdata
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("helpers", nargs="*", choices=("libreoffice", "ffmpeg", "imagemagick"))
    parser.add_argument("--apply", action="store_true", help="Ask for approval before applying the displayed plan")
    parser.add_argument("--receipt", type=Path, default=Path.home()/".config/glassbox/helpers.json")
    parser.add_argument("--tessdata-dir", type=Path, help="Destination for selected Tesseract language packs")
    parser.add_argument("--tess-lang", action="append", default=[], metavar="CODE",
                        help="Download a tessdata_fast language pack. Repeat for several languages")
    parser.add_argument("--run", nargs=argparse.REMAINDER, help="Launch a command with the saved helper environment")
    args = parser.parse_args()
    receipt = args.receipt.expanduser().resolve()
    if args.run is not None:
        if not args.run or args.helpers or args.apply or args.tess_lang:
            parser.error("--run needs a command and cannot be combined with installation")
        return run_with_helpers(receipt, args.run)
    if not (args.helpers or args.tess_lang):
        parser.error("select a helper or pass --tess-lang CODE")
    if any(not re.fullmatch(r"[a-z]{3}(?:_[A-Za-z]+)*", code) for code in args.tess_lang):
        parser.error("--tess-lang must be a language code such as eng, ara or chi_sim")

    system = platform.system()
    tessdata = (args.tessdata_dir or receipt.parent / "tessdata").expanduser().resolve()
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
        fetch_language(tessdata, code)
    if record:
        settings = json.loads(receipt.read_text()) if receipt.is_file() else {}
        if "libreoffice" in args.helpers:
            office = find_libreoffice(system)
            if office is None:
                print("LibreOffice was not found after setup.", file=sys.stderr)
                return 1
            settings["libreoffice"] = office
            print(f"LibreOffice executable: {office}")
        if languages:
            settings.update(tessdata=str(tessdata), tessdata_revision=TESSDATA_REVISION)
        atomic_write(receipt, (json.dumps(settings, indent=2) + "\n").encode())
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

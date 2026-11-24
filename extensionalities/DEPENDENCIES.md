# Optional dependencies

Install only the helpers needed for your documents. Glassbox and LiteParse use LibreOffice in headless mode for Office conversion. FFmpeg and `ffprobe` cover audio and video. ImageMagick handles still-image conversion and editing through a command-line executable. Tesseract language packs supply OCR data for the scripts you choose.

## Preview and install

Use Python 3 to run [the helper installer](scripts/install-helpers.py) from the repository root. On Windows, use your available Python command, such as `py -3`.

```sh
python3 extensionalities/scripts/install-helpers.py libreoffice imagemagick ffmpeg --tess-lang ara --tess-lang chi_sim
```

This prints the exact plan. Add `--apply` to review it and type `yes` in an interactive terminal. The plan skips working installations and nonempty packs already present in the selected directory. Linux package installation uses APT on Debian or Ubuntu. macOS uses Homebrew. Windows uses WinGet. ImageMagick 6 is detected through its `convert` command on Unix. ImageMagick 7 uses `magick`. Other Linux package managers need a manual plan. Container deployment is outside this installer.

Selecting language packs also includes English as the baseline. The installer downloads missing traineddata files from a pinned [Tesseract tessdata_fast revision](https://github.com/tesseract-ocr/tessdata_fast/tree/87416418657359cb625c412a48b6e1d6d41c29bd). Pass `--tess-sha256 CODE=<sha256>` for each downloaded language pack. The installer checks the SHA-256 digest over HTTPS before writing any pack. Use `--tessdata-dir` to choose the destination. By default, the data lives alongside the helper receipt at `~/.config/glassbox/tessdata`.

### Use the saved paths

After approval, the installer verifies LibreOffice in headless mode and records the executable path in `~/.config/glassbox/helpers.json`. Use `--receipt` before the helper arguments to select a different location. The launcher prepends the LibreOffice directory to the child process's `PATH` and sets `TESSDATA_PREFIX` to the saved language directory. This environment works for both parse commands without editing shell startup files.

```sh
python3 extensionalities/scripts/install-helpers.py --run glassbox parse document.docx --no-ocr
python3 extensionalities/scripts/install-helpers.py --run lit parse document.docx --no-ocr
python3 extensionalities/scripts/install-helpers.py --run glassbox parse scan.pdf --ocr-language ara
```

Use an absolute path in place of `glassbox` or `lit` if the CLI is outside your normal `PATH`. Pass the same `--receipt` option before `--run` if you saved it elsewhere.

## Copyable prompt for an installation agent

> Set up optional Glassbox dependencies on this computer. Start by asking which workflows and OCR languages I need. Detect the OS, architecture, available package managers, LibreOffice executable, Tesseract data, and any requested FFmpeg or ImageMagick installation. Reuse working helpers. Use the installer linked in this document to preview the exact commands, pack URLs, receipt path, and destination directory. Show me that plan and wait for my explicit approval before installing packages, downloading data, or changing any environment settings.
>
> Use native packages for LibreOffice, ImageMagick, and FFmpeg. Use the FFmpeg package for audio and video and its `ffprobe` tool for metadata. Skip Gotenberg and Docker. For Tesseract, download only the approved language packs and the documented English baseline from the pinned source.
>
> After approval, apply the displayed plan. Verify LibreOffice with `--headless --version` and capture its absolute executable path. Save it and the tessdata directory to the helper receipt. Apply the LibreOffice directory to the parse environment through the installer launcher. Ask separately before changing my persistent shell or system PATH.
>
> Verify the selected packs with Tesseract's language listing or a local OCR check. Smoke-test both `glassbox parse` and `lit parse` with a known local Office fixture if the CLIs are present. Use a scanned fixture for the requested OCR language when available. Report installed and reused helpers, versions, saved paths, the exact launch syntax, and any checks that could not run.

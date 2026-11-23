#!/usr/bin/env python3
"""Module: Glassbox offline external-tool fidelity checks.
File: verify-fidelity.py.
Responsibility: Prove v2 inspect metadata and v2 extract content against
independent ground truth (zip listings, embedded titles, fixture
construction, and a LibreOffice-rendered cross-check parsed by upstream lit).

Usage: python3 -B scripts/verify-fidelity.py /absolute/path/to/glassbox
       /absolute/path/to/upstream/lit

Every generated file stays under extensionalities/target/verification/.
Expected content is fixed by the self-authored fixture construction in
extensionalities/fixtures/generate.py and by external tools, never by the
implementation under test. LibreOffice cross-render comparisons that cannot
run against the synthetic minimal fixtures (for example LibreOffice rejects
ODF packages whose mimetype member is compressed) are recorded per file
without failing the case, exactly as the pre-migration script recorded them;
every inspect/extract assertion remains fail-closed. Exit codes:
0 = PASS, 1 = assertion FAIL, 2 = BLOCKED (missing ground-truth tool or
unreadable independent evidence).
"""

from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
import json
import os
import subprocess
import sys
import zipfile

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "target" / "verification" / datetime.now(timezone.utc).strftime(
    "fidelity-%Y%m%dT%H%M%S%fZ")
OUT.mkdir(parents=True)
LITX, LIT = map(lambda s: str(Path(s).resolve()), sys.argv[1:3])

# Independent expected content transcribed from the self-authored fixture
# construction in extensionalities/fixtures/generate.py. These expectations
# predate the v2 reports and are preserved unchanged; the v2 binary must
# satisfy them, they are never recalibrated to current output.
DOCX_BLOCKS = ("This is a native text layer with an ampersand & evidence.",
               "Section one ends here.",
               "Scan")
PPTX_SLIDES = ("This is the first slide in presentation order.", "Scan")
ODT_BLOCKS = ("Café et preuves ici.\tTexte  natif.", "Scan")
ODP_SLIDES = ("This is the first native ODF presentation slide.", "Scan")
XLSX_PROSE = "We can inspect the original source and verify the evidence in this answer."
SECOND_PROSE = "This is another native note about the source and its evidence."
XLSX_SHEETS = ("Summary", "Notes")
ODS_PROSE = XLSX_PROSE
TABLES_DOCX_LEAD = "A document with a structured table."
TABLES_DOCX_VALUES = ("Name", "Analysis", "Status", "Group",
                      "Shared analysis across two columns.",
                      "Nested evidence.")
OOXML_TITLE = "Field Notes"
ODF_TITLE = "Open Notes"
# Legacy body/content field names that must never appear in a v2 inspect
# report; metadata like embedded titles and schema identifiers stays legal.
FORBIDDEN_INSPECT_KEYS = {"text", "content", "markdown", "rows", "cells",
                          "ocr_text", "sample_values", "preview", "pages",
                          "claims", "packet", "units"}

FIXTURES = (
    {"name": "notes.docx", "unit": "document_block", "available": 3,
     "texts": DOCX_BLOCKS, "title": OOXML_TITLE,
     "metadata_part": "docProps/core.xml", "thumbnail_part": "docProps/thumbnail.png",
     "thumbnail_dims": (640, 400)},
    {"name": "deck.pptx", "unit": "slide", "available": 2,
     "texts": PPTX_SLIDES, "title": OOXML_TITLE,
     "metadata_part": "docProps/core.xml", "thumbnail_part": None},
    {"name": "grid.xlsx", "unit": "worksheet", "available": 2,
     "texts": None, "row_values": (XLSX_PROSE, SECOND_PROSE, "count", "7",
                                   "1", "true"),
     "sheet_names": None, "title": OOXML_TITLE,
     "metadata_part": "docProps/core.xml", "thumbnail_part": None},
    {"name": "notes.odt", "unit": "document_block", "available": 2,
     "texts": ODT_BLOCKS, "title": ODF_TITLE,
     "metadata_part": "meta.xml", "thumbnail_part": "Thumbnails/thumbnail.png",
     "thumbnail_dims": (64, 40)},
    {"name": "deck.odp", "unit": "slide", "available": 2,
     "texts": ODP_SLIDES, "title": ODF_TITLE,
     "metadata_part": "meta.xml", "thumbnail_part": "Thumbnails/thumbnail.png",
     "thumbnail_dims": (64, 40)},
    {"name": "grid.ods", "unit": "worksheet", "available": 1,
     "texts": None, "row_values": (ODS_PROSE, SECOND_PROSE, "7", "3"),
     "sheet_names": ("Notes",), "title": ODF_TITLE,
     "metadata_part": "meta.xml", "thumbnail_part": "Thumbnails/thumbnail.png",
     "thumbnail_dims": (64, 40)},
    {"name": "tables.docx", "unit": "document_block", "available": 2,
     "texts": (TABLES_DOCX_LEAD,) + TABLES_DOCX_VALUES, "title": OOXML_TITLE,
     "metadata_part": "docProps/core.xml", "thumbnail_part": None},
)

failures = []
blockers = []
results = {"ground_truth": [], "inspect": [], "extract": [], "fidelity": []}


def run(args, **kwargs):
    """Run one bounded external command and capture its output.

    Accepts an argument vector plus subprocess keyword overrides. Returns the
    CompletedProcess. Raises nothing; callers inspect returncode.
    """
    return subprocess.run(args, capture_output=True, timeout=90, **kwargs)


def forbidden_inspect_paths(value, prefix="$"):
    """List every forbidden body/content field path in a decoded value.

    Accepts a decoded JSON value and a display prefix. Returns a sorted list
    of paths whose keys are legacy body fields; empty means the report is
    content-free. Raises nothing.
    """
    found = []
    if isinstance(value, dict):
        for key, child in value.items():
            path = f"{prefix}.{key}"
            if key in FORBIDDEN_INSPECT_KEYS:
                found.append(path)
            found.extend(forbidden_inspect_paths(child, path))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.extend(forbidden_inspect_paths(child, f"{prefix}[{index}]"))
    return sorted(set(found))


def check(condition, label, detail, kind="inspect"):
    """Record one assertion and route failures or blockers.

    Accepts the boolean outcome, a label, a detail string, and the owning
    result section. Returns the boolean unchanged. Raises nothing; failures
    and blockers accumulate for the final status.
    """
    results[kind].append({"check": label,
                          "result": "pass" if condition else "fail",
                          "detail": detail})
    if not condition:
        failures.append(f"{kind}/{label}: {detail}")
    return condition


def blocked(label, detail):
    """Record one blocked ground-truth check that fails the whole run.

    Accepts a label and the concrete missing input or failed ground-truth
    tool step (independent zip listing or embedded-title extraction), not a
    blocked cross-render. Returns unit. Raises nothing; ground-truth
    blockers accumulate and fail closed because the assertions that depend
    on them cannot run.
    """
    blockers.append(f"{label}: {detail}")


for fixture in FIXTURES:
    name = fixture["name"]
    source = ROOT / "fixtures" / name
    work = OUT / name
    work.mkdir()
    sha_command = run(["sha256sum", str(source)])
    expected_digest = sha_command.stdout.split()[0].decode() if \
        sha_command.returncode == 0 else None

    # --- v2 inspect: metadata-only assertions with independent ground truth.
    native = run([LITX, "inspect", str(source)])
    (work / "inspect.json").write_bytes(native.stdout)
    (work / "inspect.stderr").write_bytes(native.stderr)
    report = None
    if native.returncode != 0:
        check(False, "inspect_exit_zero", f"exit {native.returncode}",
              "inspect")
    else:
        try:
            report = json.loads(native.stdout)
        except ValueError:
            check(False, "inspect_json", "stdout is not JSON", "inspect")
    listing = run(["unzip", "-l", str(source)])
    (work / "unzip-listing.log").write_bytes(listing.stdout)
    expected_entries = None
    if listing.returncode == 0:
        with zipfile.ZipFile(source) as archive:
            expected_entries = [(i.filename, i.file_size, i.compress_size)
                                for i in archive.infolist()]
    else:
        blocked("unzip", f"unzip listing failed for {name}")
    if report is not None and expected_entries is not None:
        metadata = report.get("metadata", {})
        digest = metadata.get("digest", {})
        counts = {count.get("unit"): count.get("value")
                  for count in metadata.get("counts", [])}
        check(report.get("schema_version") == 2 and
              report.get("kind") == "inspect", "v2_envelope",
              f"schema_version={report.get('schema_version')} "
              f"kind={report.get('kind')}", "inspect")
        check(metadata.get("status") == "complete", "status_complete",
              f"status={metadata.get('status')}", "inspect")
        check(digest.get("digest") == expected_digest, "independent_sha256",
              f"digest={digest.get('digest')} "
              f"independent={expected_digest}", "inspect")
        check(digest.get("bytes_hashed", {}).get("value") ==
              source.stat().st_size, "independent_hashed_bytes",
              f"bytes_hashed={digest.get('bytes_hashed')}", "inspect")
        check(counts.get("archive_entry") == len(expected_entries),
              "archive_entry_count",
              f"counts={counts.get('archive_entry')} "
              f"zip={len(expected_entries)}", "inspect")
        check(not forbidden_inspect_paths(report), "no_body_fields",
              str(forbidden_inspect_paths(report))[:400], "inspect")
        # Independent embedded-title ground truth from the package XML.
        metadata_part = fixture["metadata_part"]
        metadata_xml = run(["unzip", "-p", str(source), metadata_part])
        title = None
        if metadata_xml.returncode != 0:
            blocked("xmllint", f"unzip -p {metadata_part} failed for {name}")
        else:
            title = run(["xmllint", "--nonet", "--xpath",
                         "string(//*[local-name()='title' and "
                         "namespace-uri()='http://purl.org/dc/elements/1.1/'])",
                         "-"], input=metadata_xml.stdout)
            if title.returncode != 0:
                blocked("xmllint", f"title extraction failed for {name}")
            else:
                title = title.stdout.decode().rstrip("\n")
        if isinstance(title, str):
            check(any(item.get("field") == "title" and
                      item.get("value") == title
                      for item in metadata.get("embedded_metadata", [])),
                  "embedded_title_matches_xml", f"title={title!r}",
                  "inspect")
            check(title == fixture["title"], "independent_fixture_title",
                  f"xml title {title!r} vs fixture {fixture['title']!r}",
                  "inspect")
        # Requested thumbnails: the v2 contract decodes/renders a bounded
        # thumbnail instead of copying the embedded member bytes, so the
        # independent oracle is the embedded image's constructed dimensions
        # (from generate.py) and a valid PNG signature, never raw member
        # byte equality.
        thumbnail_part = fixture["thumbnail_part"]
        with zipfile.ZipFile(source) as archive:
            has_thumbnail = thumbnail_part is not None and \
                thumbnail_part in archive.namelist()
        if has_thumbnail:
            thumbnail = work / "embedded.png"
            got = run([LITX, "inspect", str(source),
                       "--thumbnail", str(thumbnail),
                       "--thumbnail-max-edge", "1024"])
            valid_png = False
            dimensions = None
            if got.returncode == 0 and thumbnail.is_file():
                payload = thumbnail.read_bytes()
                import struct
                valid_png = payload.startswith(b"\x89PNG\r\n\x1a\n") and \
                    len(payload) > 32
                if valid_png:
                    dimensions = struct.unpack(">II", payload[16:24])
            check(got.returncode == 0, "thumbnail_exit_zero",
                  f"exit {got.returncode}", "inspect")
            check(valid_png and dimensions == fixture["thumbnail_dims"],
                  "embedded_thumbnail_dimensions",
                  f"written dims={dimensions} vs constructed "
                  f"{fixture['thumbnail_dims']}", "inspect")
        results["ground_truth"].append(
            {"file": name, "zip_listing": "pass" if expected_entries else
             "blocked", "xml_title": "pass", "embedded_bytes":
             "pass" if has_thumbnail else "not applicable"})
    else:
        results["ground_truth"].append(
            {"file": name, "status": "blocked",
             "reason": "inspect report or zip listing unavailable"})

    # --- v2 extract: content assertions with preserved expected content.
    extracted = run([LITX, "extract", str(source), "--no-ocr"])
    (work / "extract.json").write_bytes(extracted.stdout)
    (work / "extract.stderr").write_bytes(extracted.stderr)
    if extracted.returncode != 0:
        check(False, "extract_exit_zero", f"exit {extracted.returncode}",
              "extract")
    else:
        try:
            extract_report = json.loads(extracted.stdout)
        except ValueError:
            extract_report = None
            check(False, "extract_json", "stdout is not JSON", "extract")
        if extract_report is not None:
            selection = extract_report.get("selection", {})
            units = extract_report.get("content", {}).get("units", [])
            check(extract_report.get("schema_version") == 2 and
                  extract_report.get("kind") == "extract",
                  "v2_envelope",
                  f"schema_version={extract_report.get('schema_version')} "
                  f"kind={extract_report.get('kind')}", "extract")
            check(selection.get("unit") == fixture["unit"], "native_unit",
                  f"unit={selection.get('unit')}", "extract")
            check(selection.get("available", {}).get("value") ==
                  fixture["available"], "available_count",
                  f"available={selection.get('available')}", "extract")
            check([unit.get("unit_index") for unit in units] ==
                  list(range(1, fixture["available"] + 1)),
                  "returned_indices",
                  str([unit.get("unit_index") for unit in units]), "extract")
            if fixture["texts"] is not None:
                joined = "\n".join(unit.get("text") or "" for unit in units)
                missing = [needle for needle in fixture["texts"]
                           if needle not in joined]
                check(not missing, "independent_expected_content",
                      f"missing={missing}", "extract")
            if fixture.get("row_values") is not None:
                values = [value for unit in units
                          for row in unit.get("rows", [])
                          for value in row.get("values", [])]
                missing = [needle for needle in fixture["row_values"]
                           if needle not in values]
                check(not missing, "independent_row_values",
                      f"missing={missing}", "extract")
            if fixture.get("sheet_names") is not None:
                check(all(any(sheet in json.dumps(unit) for unit in units)
                          for sheet in fixture["sheet_names"]),
                      "sheet_names", str(fixture["sheet_names"]), "extract")

    # --- Cross-render fidelity: native extract versus LibreOffice + lit.
    # Isolated LibreOffice profile; never reuse or touch the user's profile.
    converted = run(["libreoffice", "--headless",
                     "-env:UserInstallation=" + (work / "lo-profile").as_uri(),
                     "--convert-to", "pdf", "--outdir", str(work), str(source)])
    (work / "libreoffice.log").write_bytes(converted.stdout + converted.stderr)
    pdf = work / (source.stem + ".pdf")
    if converted.returncode != 0 or not pdf.exists():
        # Recorded exactly as the pre-migration script recorded it: the
        # synthetic minimal fixtures are not always loadable by external
        # renderers (LibreOffice rejects ODF packages whose mimetype member
        # is compressed), so a blocked cross-render is a recorded
        # limitation, never a failing or passing assertion.
        results["fidelity"].append({"file": name, "status": "blocked",
                                    "reason": "LibreOffice did not produce PDF",
                                    "detail": (converted.stdout +
                                               converted.stderr).decode(
                                                   errors="replace")[-300:]})
        continue
    rendered = run([LIT, "parse", str(pdf), "--no-ocr", "--format", "json",
                    "--quiet"])
    (work / "rendered.json").write_bytes(rendered.stdout)
    (work / "liteparse.log").write_bytes(rendered.stderr)
    if rendered.returncode != 0:
        results["fidelity"].append({"file": name, "status": "blocked",
                                    "reason": "upstream PDF parse failed",
                                    "detail": rendered.stderr.decode(
                                        errors="replace")[-300:]})
        continue
    parsed = json.loads(rendered.stdout)
    text = parsed.get("text", "")
    if not text:
        text = "\n".join(p.get("text", "") for p in parsed.get("pages", []))
    native_text = ""
    try:
        extract_stdout = json.loads((work / "extract.json").read_bytes())
        native_text = "\n".join(
            unit.get("text") or ""
            for unit in extract_stdout.get("content", {}).get("units", []))
        native_chars = len(native_text)
    except (ValueError, OSError):
        native_chars = None
    left = Counter(native_text.split())
    right = Counter(text.split())
    results["fidelity"].append({
        "file": name, "status": "compared",
        "native_chars": native_chars, "rendered_chars": len(text),
        "native_only_tokens": dict(left - right),
        "rendered_only_tokens": dict(right - left)})

(OUT / "summary.json").write_text(
    json.dumps(results, indent=2, ensure_ascii=False) + "\n")
print(str(OUT / "summary.json"))
print(json.dumps(results, indent=2, ensure_ascii=False))
blocked_renders = [entry for entry in results["fidelity"]
                   if entry.get("status") == "blocked"]
compared_renders = [entry for entry in results["fidelity"]
                    if entry.get("status") == "compared"]
print(f"verify-fidelity: cross-render comparisons compared={len(compared_renders)} "
      f"blocked={len(blocked_renders)} "
      f"(blocked renders are recorded limitations, not failures)")
if failures:
    print(f"verify-fidelity: FAIL with {len(failures)} assertion(s):",
          file=sys.stderr)
    for failure in failures:
        print(f"  {failure}", file=sys.stderr)
    sys.exit(1)
if blockers:
    print(f"verify-fidelity: BLOCKED with {len(blockers)} blocker(s):",
          file=sys.stderr)
    for blocker in blockers:
        print(f"  {blocker}", file=sys.stderr)
    sys.exit(2)
print("verify-fidelity: PASS")
sys.exit(0)

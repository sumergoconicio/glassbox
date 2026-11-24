#!/usr/bin/env python3
"""Plan or automate the Glassbox upstream overlay. Python 3.10+, Git, gh.

plan writes Git objects and an isolated ignored index, never working files or
refs. automate additionally creates a GitHub branch/PR and conditionally merges
it. Automation requires GitHub Actions, an explicit token, strict required CI,
and unchanged base/head SHAs. Network calls time out after 60 seconds; checks
have a 40-minute deadline. Failures leave published PRs open, without force
pushes, deletions, credential changes, or downstream lockfile updates.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time
from urllib.parse import urlencode

ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "sumergoconicio/glassbox"
GATE = "Glassbox downstream gate"
WORKFLOW = "Glassbox downstream CI"
PROTECTED = ("extensionalities", ".github/workflows")
PROTECTED_FILES = (".github/README.md", ".cargo/config.toml")
IGNORE = b"/extensionalities/dev/"
COAUTHOR = "Co-authored-by: factory-droid[bot] <138933559+factory-droid[bot]@users.noreply.github.com>"


class SyncError(RuntimeError):
    """Error raised when upstream sync planning, verification, or publication stops."""


class NotFoundError(SyncError):
    """GitHub API 404 Not Found error raised when a probed resource does not exist."""


def git(root, *args, data=None, env=None):
    result = subprocess.run(
        ["git", "-C", str(root), *args], input=data, capture_output=True,
        timeout=60, env=env,
    )
    if result.returncode:
        raise SyncError(f"Git command failed: {args[0]}")
    return result.stdout


def oid(root, ref):
    value = git(root, "rev-parse", "--verify", ref).decode().strip()
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise SyncError("Expected a SHA-1 Git object")
    return value


def tree_entries(root, ref):
    result = {}
    for record in git(root, "ls-tree", "-rz", "--full-tree", ref).split(b"\0"):
        if record:
            metadata, path = record.split(b"\t", 1)
            result[path] = metadata
    return result


def protected(path):
    """Report whether a Git tree path lies inside a protected directory or file slot.

    Accepts path (bytes): repository-relative Git tree entry path.
    Returns bool: True when path matches a protected directory, a protected file,
    or any nested path beneath either slot.
    Raises TypeError or AttributeError if path is not bytes.
    """
    return any(
        path == p.encode() or path.startswith(p.encode() + b"/")
        for p in (*PROTECTED, *PROTECTED_FILES)
    )


def plan(root, base, upstream):
    """Build a verified sync tree mirroring upstream while preserving protected overlays.

    Accepts root (Path): repository root, base (str): base commit ref, and
    upstream (str): upstream commit ref.
    Returns dict: resolved commit SHAs, resulting tree SHA, upstream tree SHA,
    GitHub tree overlay entries, and a boolean changed flag.
    Raises SyncError or subprocess.TimeoutExpired if refs are invalid, protected
    paths have wrong Git object types or modes, upstream .cargo, .github or .gitignore
    shapes are unsafe, .gitignore is not UTF-8, or protected overlays drift.
    """
    base = oid(root, base + "^{commit}")
    upstream = oid(root, upstream + "^{commit}")
    base_entries = tree_entries(root, base)
    overlays = []
    for path in PROTECTED:
        tree = oid(root, f"{base}:{path}")
        if git(root, "cat-file", "-t", tree).strip() != b"tree":
            raise SyncError(f"Protected path must be a directory: {path}")
        overlays.append({"path": path, "mode": "040000", "type": "tree", "sha": tree})
    for path in PROTECTED_FILES:
        blob = oid(root, f"{base}:{path}")
        if git(root, "cat-file", "-t", blob).strip() != b"blob":
            raise SyncError(f"Protected file must be a regular file: {path}")
        entry = base_entries.get(path.encode(), b"").split()
        if len(entry) != 3 or entry[0] not in (b"100644", b"100755") or entry[1] != b"blob":
            raise SyncError(f"Protected file must be a regular file: {path}")
        mode = entry[0]
        overlays.append({"path": path, "mode": mode.decode(), "type": "blob", "sha": blob})
    incoming = tree_entries(root, upstream)
    for directory in (".github", ".cargo"):
        if directory.encode() in incoming:
            raise SyncError(f"Upstream {directory} is not a directory")
    ignore = incoming.get(b".gitignore", b"100644 blob").split()
    if ignore[0] not in (b"100644", b"100755"):
        raise SyncError("Upstream .gitignore must be a regular file")
    content = git(root, "cat-file", "blob", ignore[2].decode()) if len(ignore) == 3 else b""
    if not content.splitlines() or content.splitlines()[-1] != IGNORE:
        content += (b"\n" if content and not content.endswith(b"\n") else b"") + IGNORE + b"\n"
    try:
        ignore_text = content.decode("utf-8")
    except UnicodeDecodeError as error:
        raise SyncError("Upstream .gitignore is not UTF-8") from error
    blob = git(root, "hash-object", "-w", "--stdin", data=content).strip()
    selected = {p: m for p, m in incoming.items() if not protected(p)}
    selected.update({p: m for p, m in base_entries.items() if protected(p)})
    selected[b".gitignore"] = ignore[0] + b" blob " + blob
    scratch = root / "extensionalities/target"
    scratch.mkdir(parents=True, exist_ok=True)
    records = b"".join(m + b"\t" + p + b"\0" for p, m in sorted(selected.items()))
    with tempfile.TemporaryDirectory(prefix="sync-plan-", dir=scratch) as tmpdir:
        index = Path(tmpdir) / "index"
        env = {**os.environ, "GIT_INDEX_FILE": str(index)}
        git(root, "update-index", "-z", "--index-info", data=records, env=env)
        tree = git(root, "write-tree", env=env).decode().strip()
    for overlay in overlays:
        if oid(root, f"{tree}:{overlay['path']}") != overlay["sha"]:
            raise SyncError("Protected overlay changed during planning")
    overlays.append({
        "path": ".gitignore", "mode": ignore[0].decode(),
        "type": "blob", "content": ignore_text,
    })
    return {
        "base": base, "upstream": upstream, "tree": tree,
        "upstream_tree": oid(root, upstream + "^{tree}"),
        "overlays": overlays,
        "changed": tree != oid(root, base + "^{tree}"),
    }


def _is_not_found(result):
    """Report whether a failed `gh api` subprocess result represents HTTP 404 Not Found.

    Accepts result (subprocess.CompletedProcess): failed `gh api` process output.
    Returns bool: True only when stderr or JSON stdout explicitly signals HTTP 404.
    Never raises on malformed stdout.
    """
    stderr = result.stderr or ""
    if "HTTP 404" in stderr:
        return True
    stdout = (result.stdout or "").strip()
    if stdout:
        try:
            body = json.loads(stdout)
        except json.JSONDecodeError:
            return False
        if isinstance(body, dict) and str(body.get("status", "")) == "404":
            return True
    return False


def api(repository, path="", payload=None, method="GET", paginate=False):
    """Execute one GitHub REST API call via `gh api` with a 60-second timeout.

    Accepts repository (str), path (str), payload (dict | None), method (str), and
    paginate (bool).
    Returns parsed JSON response body (dict or list).
    Raises NotFoundError on HTTP 404, SyncError on any other non-zero exit code,
    subprocess.TimeoutExpired on timeout, or json.JSONDecodeError on invalid JSON.
    """
    args = ["gh", "api", f"repos/{repository}/{path}".rstrip("/"),
            "--method", method]
    if paginate:
        args.extend(["--paginate", "--slurp"])
    if payload is not None:
        args.extend(["--input", "-"])
    result = subprocess.run(
        args, input=json.dumps(payload) if payload is not None else None,
        capture_output=True, text=True, timeout=60,
    )
    if result.returncode:
        if _is_not_found(result):
            raise NotFoundError(f"GitHub resource not found ({method} {path})")
        raise SyncError(f"GitHub request failed ({method} {path}); no retry attempted")
    return json.loads(result.stdout)


def find_existing_ref(repository, branch):
    """Probe GitHub for an exact `refs/heads/{branch}` reference without creating anything.

    Accepts repository (str): `owner/repo` slug, and branch (str): branch name without
    the `refs/heads/` prefix.
    Returns dict | None: exact matching GitHub ref payload when present, or None when the
    ref returns HTTP 404 or only prefix-matched sibling refs exist.
    Raises SyncError on non-404 API failures or unexpected non-ref response shapes.
    """
    expected_ref = f"refs/heads/{branch}"
    try:
        response = api(repository, f"git/ref/heads/{branch}")
    except NotFoundError:
        return None
    if isinstance(response, list):
        for item in response:
            if isinstance(item, dict) and item.get("ref") == expected_ref:
                return item
        return None
    if isinstance(response, dict) and response.get("ref") == expected_ref:
        return response
    raise SyncError(f"Unexpected GitHub ref response for {expected_ref}")


def verify_existing_branch_commit(repository, ref_obj, candidate):
    """Verify an existing upstream-sync branch matches the planned tree and parent SHAs.

    Accepts repository (str): `owner/repo` slug, ref_obj (dict): GitHub ref payload for
    `refs/heads/{branch}`, and candidate (dict): verified planning result from `plan()`.
    Returns str: verified 40-character commit SHA to reuse when creating the missing PR.
    Raises SyncError if the ref object is not a valid commit SHA or its commit's tree SHA
    or ordered parents differ from `candidate["tree"]` and `[candidate["base"], candidate["upstream"]]`.
    """
    obj = ref_obj.get("object")
    if not isinstance(obj, dict) or obj.get("type") != "commit":
        raise SyncError("Existing upstream-sync branch does not match verified tree/parents")
    existing_sha = obj.get("sha")
    if not isinstance(existing_sha, str) or not re.fullmatch(r"[0-9a-f]{40}", existing_sha):
        raise SyncError("Existing upstream-sync branch does not match verified tree/parents")
    existing_commit = api(repository, f"git/commits/{existing_sha}")
    if not isinstance(existing_commit, dict):
        raise SyncError("Existing upstream-sync branch does not match verified tree/parents")
    tree_obj = existing_commit.get("tree")
    tree_sha = tree_obj.get("sha") if isinstance(tree_obj, dict) else None
    parents = existing_commit.get("parents")
    parent_shas = (
        [p.get("sha") for p in parents if isinstance(p, dict)]
        if isinstance(parents, list)
        else []
    )
    expected_parents = [candidate["base"], candidate["upstream"]]
    if tree_sha != candidate["tree"] or parent_shas != expected_parents:
        raise SyncError("Existing upstream-sync branch does not match verified tree/parents")
    return existing_sha


def require_protection(settings):
    checks = settings.get("required_status_checks") or {}
    contexts = set(checks.get("contexts", []))
    contexts.update(c.get("context") for c in checks.get("checks", []))
    if (
        not checks.get("strict")
        or GATE not in contexts
        or not (settings.get("enforce_admins") or {}).get("enabled")
    ):
        raise SyncError("main must require the downstream gate, strict updates, and administrator enforcement")


def all_green(checks):
    """Missing/pending checks wait; failed, skipped, or neutral checks block."""
    if not checks:
        return False
    pending = {"PENDING", "QUEUED", "IN_PROGRESS", "WAITING", "REQUESTED", "EXPECTED"}
    ready = any(c.get("name") == GATE and c.get("workflow") == WORKFLOW for c in checks)
    for check in checks:
        state = check.get("state")
        if state in pending:
            ready = False
        elif state != "SUCCESS":
            raise SyncError("A PR check failed, was skipped/cancelled, or has an unknown result")
    return ready


def pr_checks(repository, number):
    result = subprocess.run(
        ["gh", "pr", "checks", str(number), "--repo", repository,
         "--json", "name,state,workflow"],
        capture_output=True, text=True, timeout=60,
    )
    if result.returncode not in (0, 1, 8):
        raise SyncError("Could not read PR checks")
    if not result.stdout.strip():
        if "no checks reported" in result.stderr.lower():
            return []
        raise SyncError("PR checks returned no readable result")
    checks = json.loads(result.stdout)
    if not isinstance(checks, list):
        raise SyncError("Unexpected PR check response")
    return checks


def verify_refs(repository, number, branch, head, base):
    pr = api(repository, f"pulls/{number}")
    current = api(repository, "branches/main")
    if (
        pr.get("state") != "open" or pr.get("draft") or pr.get("mergeable") is not True
        or pr["base"]["ref"] != "main" or pr["head"]["ref"] != branch
        or pr["base"]["repo"]["full_name"] != repository
        or pr["head"]["repo"]["full_name"] != repository
        or pr["head"]["sha"] != head or current["commit"]["sha"] != base
    ):
        raise SyncError("PR state, mergeability, or base/head changed; manual review required")


def merge_when_green(repository, number, branch, head, base, wait_seconds=2400):
    deadline = time.monotonic() + wait_seconds
    while time.monotonic() < deadline:
        if all_green(pr_checks(repository, number)):
            verify_refs(repository, number, branch, head, base)
            require_protection(api(repository, "branches/main/protection"))
            # Re-read all checks after state/protection verification.
            if all_green(pr_checks(repository, number)):
                result = api(repository, f"pulls/{number}/merge", {
                    "sha": head, "merge_method": "merge",
                    "commit_message": COAUTHOR,
                }, method="PUT")
                if not result.get("merged"):
                    raise SyncError("GitHub refused the merge; PR remains open")
                return result
        time.sleep(15)
    raise SyncError("Checks did not all pass within 40 minutes; PR remains open")


def automate(root, base, upstream):
    """Publish a verified upstream-sync PR and merge it when all checks and guards pass.

    Accepts root (Path): repository root, base (str): base commit ref on main, and
    upstream (str): upstream commit ref to mirror.
    Returns None after creating (or recovering after partial branch publication) and
    merging the sync PR, or exiting early when no upstream-owned files changed.
    Raises SyncError if environment guards, branch protection, open-PR locks, tree/ref
    verification, PR creation, or check/merge gates fail.
    """
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("GITHUB_REPOSITORY") != REPOSITORY:
        raise SyncError("Live automation is restricted to Glassbox GitHub Actions")
    if not os.environ.get("GH_TOKEN"):
        raise SyncError("Set the separately approved UPSTREAM_SYNC_TOKEN repository secret")
    repository = REPOSITORY
    require_protection(api(repository, "branches/main/protection"))
    metadata = api(repository)
    if metadata.get("default_branch") != "main" or not metadata.get("allow_merge_commit"):
        raise SyncError("Expected main and merge-commit support")
    query = urlencode({"state": "open", "base": "main", "per_page": 100})
    pages = api(repository, f"pulls?{query}", paginate=True)
    if any(pr["head"]["ref"].startswith("automation/upstream-sync/")
           for page in pages for pr in page):
        raise SyncError("An upstream-sync PR is already open; resolve it before another sync")
    candidate = plan(root, base, upstream)
    if api(repository, "branches/main")["commit"]["sha"] != candidate["base"]:
        raise SyncError("main moved during planning")
    if not candidate["changed"]:
        print("No upstream-owned file changes")
        return
    # Confirm the fork network can resolve the upstream parent before any write.
    api(repository, f"git/commits/{candidate['upstream']}")
    branch = f"automation/upstream-sync/{candidate['upstream'][:12]}-{candidate['base'][:12]}"
    existing_ref = find_existing_ref(repository, branch)
    if existing_ref is not None:
        head_sha = verify_existing_branch_commit(repository, existing_ref, candidate)
    else:
        tree = api(repository, "git/trees", {
            "base_tree": candidate["upstream_tree"], "tree": candidate["overlays"],
        }, method="POST")
        if tree["sha"] != candidate["tree"]:
            raise SyncError("GitHub tree differs from the verified local tree")
        commit = api(repository, "git/commits", {
            "message": f"chore: sync upstream LiteParse {candidate['upstream'][:12]}\n\n{COAUTHOR}",
            "tree": tree["sha"], "parents": [candidate["base"], candidate["upstream"]],
        }, method="POST")
        api(repository, "git/refs", {
            "ref": f"refs/heads/{branch}", "sha": commit["sha"],
        }, method="POST")
        head_sha = commit["sha"]
    pr = api(repository, "pulls", {
        "title": f"Sync upstream LiteParse {candidate['upstream'][:12]}",
        "base": "main", "head": branch,
        "body": (
            f"Upstream: `run-llama/liteparse@{candidate['upstream']}`.\n\n"
            "Upstream-owned paths mirror that snapshot. extensionalities and "
            ".github/workflows retain their exact base trees. The downstream "
            "ignore rule is preserved. Automatic merge requires every check "
            "to succeed, strict required CI, and unchanged base/head commits."
        ),
    }, method="POST")
    print(f"Created {pr['html_url']}", flush=True)
    merge_when_green(repository, pr["number"], branch, head_sha, candidate["base"])
    print(f"Merged {pr['html_url']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("plan", "automate"))
    parser.add_argument("--base", required=True)
    parser.add_argument("--upstream", required=True)
    args = parser.parse_args()
    try:
        if args.command == "plan":
            print(json.dumps(plan(ROOT, args.base, args.upstream), indent=2))
        else:
            automate(ROOT, args.base, args.upstream)
    except (SyncError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
        parser.exit(1, f"Upstream sync stopped: {error}\n")


if __name__ == "__main__":
    main()

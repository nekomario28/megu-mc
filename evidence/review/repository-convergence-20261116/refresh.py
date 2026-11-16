#!/usr/bin/env python3
"""Validate the local Git/PR convergence inventory against the live repository.

Read-only. This never rewrites a disposition, a SHA or a verdict: it reports
where the inventory has gone stale so a human re-checks the affected rows.

  python3 refresh.py                 # validate, human-readable
  python3 refresh.py --json          # machine-readable issues
  python3 refresh.py --repo <path>   # validate another checkout of the same repo

Exit codes: 0 = no FAIL, 1 = at least one FAIL, 2 = unusable input.

Only the Python standard library is used.
"""
import argparse
import json
import os
import subprocess
import sys

SCHEMA = "mc-spine/repository-convergence-inventory@1"
HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_INVENTORY = os.path.join(HERE, "inventory.json")


def git(repo, *args, timeout=90):
    p = subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True,
                       timeout=timeout)
    return p.returncode, p.stdout, p.stderr


def collect_facts(repo):
    """Live branch heads and worktree state. Ids are worktree basenames."""
    rc, out, err = git(repo, "for-each-ref", "--format=%(refname:short)%00%(objectname)",
                       "refs/heads")
    if rc != 0:
        raise SystemExit(f"cannot list refs: {err.strip()[:300]}")
    heads = {}
    for line in out.splitlines():
        if line.strip():
            name, sha = line.split("\0")
            heads[name] = sha

    rc, out, err = git(repo, "worktree", "list", "--porcelain")
    if rc != 0:
        raise SystemExit(f"cannot list worktrees: {err.strip()[:300]}")
    worktrees = {}
    cur = None
    for line in out.splitlines():
        if line.startswith("worktree "):
            path = line[len("worktree "):]
            # `git worktree list` puts the main worktree first; that is "root"
            wid = "root" if not worktrees else os.path.basename(path.rstrip("/"))
            cur = {"id": wid, "path": path,
                   "branch": None, "head": None, "dirty_count": 0}
            worktrees[cur["id"]] = cur
        elif line.startswith("HEAD ") and cur:
            cur["head"] = line[5:]
        elif line.startswith("branch ") and cur:
            cur["branch"] = line[7:].replace("refs/heads/", "")

    for wt in worktrees.values():
        rc, out, _ = git(repo, "-C", wt["path"], "status", "--porcelain=v1", timeout=60)
        if rc == 0:
            wt["dirty_rows"] = [r for r in out.splitlines() if r.strip()]
            wt["dirty_count"] = len(wt["dirty_rows"])
            wt["status_readable"] = True
        else:
            wt["status_readable"] = False
    return {"heads": heads, "worktrees": worktrees}


def _issue(level, code, target, detail):
    return {"level": level, "code": code, "target": target, "detail": detail}


def compare(inventory, facts):
    """Pure comparison: inventory rows against live facts -> issues."""
    issues = []
    snap = inventory.get("snapshot", {})
    canon_ref = snap.get("canonical_ref")
    expected = snap.get("canonical_sha")
    moving = bool(snap.get("canonical_moving"))
    actual = facts["heads"].get(canon_ref)
    if actual is None:
        issues.append(_issue("FAIL", "CANONICAL_MISSING", canon_ref,
                             f"{canon_ref} is not a local branch"))
    elif actual != expected:
        level = "WARN" if moving else "FAIL"
        code = "CANONICAL_MOVED" if moving else "CANONICAL_INVALID"
        issues.append(_issue(level, code, canon_ref,
                             f"recorded {expected[:12]}, live {actual[:12]}"))

    for row in inventory.get("refs", []):
        ref = row["ref"]
        live = facts["heads"].get(ref)
        if live is None:
            issues.append(_issue("FAIL", "REF_MISSING", ref,
                                 "recorded branch no longer exists"))
            continue
        if live != row["sha_observed"]:
            if row.get("moving"):
                issues.append(_issue("WARN", "REF_MOVED_ACTIVE", ref,
                                     f"recorded {row['sha_observed'][:12]}, live {live[:12]} "
                                     "(active ref: re-read its owning task)"))
            else:
                issues.append(_issue("FAIL", "REF_MOVED", ref,
                                     f"recorded {row['sha_observed'][:12]}, live {live[:12]} "
                                     "(row invalid: re-verify this disposition)"))

    for row in inventory.get("worktrees", []):
        wid = row["worktree_id"]
        live = facts["worktrees"].get(wid)
        if live is None:
            issues.append(_issue("FAIL", "WORKTREE_MISSING", wid,
                                 "recorded worktree no longer exists"))
            continue
        if live["branch"] != row["branch"]:
            issues.append(_issue("FAIL", "WORKTREE_BRANCH_MISMATCH", wid,
                                 f"recorded {row['branch']}, live {live['branch']}"))
        if not live.get("status_readable", True):
            issues.append(_issue("WARN", "WORKTREE_STATUS_UNREADABLE", wid,
                                 "git status failed; dirty state unverified"))
            continue
        exempt = tuple(row.get("dirty_exempt") or ())
        rows = live.get("dirty_rows", [])
        if exempt:
            rows = [r for r in rows if not any(p in r for p in exempt)]
        live_clean = not rows
        recorded_clean = bool(row["clean"])
        if live_clean != recorded_clean:
            kind = "became clean" if live_clean else "became dirty"
            issues.append(_issue("FAIL", "WORKTREE_DIRTY_MISMATCH", wid,
                                 f"recorded clean={recorded_clean}, live clean={live_clean} "
                                 f"({kind}, live dirty_count={live['dirty_count']})"))
        if row.get("head_observed") and live["head"] != row["head_observed"]:
            issues.append(_issue("WARN", "WORKTREE_HEAD_MOVED", wid,
                                 f"recorded {row['head_observed'][:12]}, live {live['head'][:12]}"))
    return issues


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", default=None, help="checkout to read (default: this worktree)")
    ap.add_argument("--inventory", default=DEFAULT_INVENTORY)
    ap.add_argument("--json", action="store_true", help="print issues as JSON")
    args = ap.parse_args(argv)

    repo = args.repo
    if repo is None:
        rc, out, _ = git(os.getcwd(), "rev-parse", "--show-toplevel")
        repo = out.strip() if rc == 0 else None
    if not repo or not os.path.exists(os.path.join(repo, ".git")):
        print("error: not a git checkout; pass --repo", file=sys.stderr)
        return 2
    try:
        inventory = json.load(open(args.inventory))
    except (OSError, ValueError) as exc:
        print(f"error: cannot read inventory: {exc}", file=sys.stderr)
        return 2
    if inventory.get("schema") != SCHEMA:
        print(f"error: unexpected schema {inventory.get('schema')!r}", file=sys.stderr)
        return 2

    issues = compare(inventory, collect_facts(repo))
    fails = [i for i in issues if i["level"] == "FAIL"]
    warns = [i for i in issues if i["level"] == "WARN"]

    if args.json:
        print(json.dumps({"repo": repo, "fails": len(fails), "warns": len(warns),
                          "issues": issues}, indent=1))
    else:
        print(f"repo: {repo}")
        print(f"inventory: {args.inventory}")
        for i in fails:
            print(f"FAIL {i['code']:<26} {i['target']}: {i['detail']}")
        for i in warns:
            print(f"warn {i['code']:<26} {i['target']}: {i['detail']}")
        print(f"{len(fails)} fail(s), {len(warns)} warning(s)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())

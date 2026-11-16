#!/usr/bin/env python3
"""Produce a source-bound test receipt, or fail closed.

The failure this exists for: a hand-written runner grepped Cargo's verbose output for
`Executable ...`, missed, then ran `"" --list` (exit 127), recorded `bin=`, `sha256=`,
`tests_total=0`, and still left a green-looking receipt that a later review quoted as
measured. Every one of those states is rejected here before a PASS is written.

What a PASS requires, all measured in this invocation:
  * HEAD equals `--expect-head` (a stale source tree cannot certify a candidate), and
    HEAD/dirty state are unchanged from start to finish;
  * exactly one executable from Cargo's JSON, existing, non-empty, with a SHA-256;
  * `--list` exits 0 and reports at least one test;
  * each requested filter runs at least one test, zero failed, zero ignored-result-only;
  * receipt.json is written and its `status` is PASS only when all of the above held.

The command is `cargo` by default; `--cargo` points at another one (the guard test uses a
fake). Nothing here is a general evidence platform: it stamps one run and refuses the
three shapes that produced the false green.
"""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import sys
import time


def utc_now():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def git(root, *args):
    result = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def source_identity(root):
    head = git(root, "rev-parse", "HEAD").strip()
    status = git(root, "status", "--porcelain")
    diff = git(root, "diff", "HEAD", "--binary")
    return {
        "head": head,
        "dirty": len(status.splitlines()),
        "status_sha256": hashlib.sha256(status.encode()).hexdigest(),
        "patch_sha256": hashlib.sha256(diff.encode()).hexdigest(),
    }


def run(command, cwd, log):
    """stdout and stderr stay separate: Cargo's JSON is stdout, progress is stderr."""
    started, status, out, err = utc_now(), None, "", ""
    try:
        result = subprocess.run(command, cwd=cwd, capture_output=True, text=True, check=False)
        out, err, status = result.stdout, result.stderr, result.returncode
    except OSError as error:
        status, err = 127, f"{command[0]}: {error}\n"
    log.append({"command": command, "start": started, "finish": utc_now(),
                "exit": status, "stdout": out, "stderr": err})
    return status, out, err


def cargo_executable(artifact_text):
    """Reuse the project resolver, imported from this directory, not a grep."""
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from cargo_executable import test_executable
    return test_executable(artifact_text.splitlines())


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--expect-head", required=True)
    parser.add_argument("--filter", action="append", default=[])
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args(argv)

    root, out = Path(args.root).resolve(), Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    log, problems = [], []
    source = source_identity(root)
    receipt = {"tool": "tools/evidence_receipt.py", "start": utc_now(),
               "source_at_start": source, "expect_head": args.expect_head,
               "filters": args.filter, "cargo": args.cargo, "problems": problems}

    def fail(message):
        problems.append(message)

    if source["head"] != args.expect_head:
        fail(f"stale source: HEAD {source['head']} != expected {args.expect_head}")

    # Cargo prints no compiler-artifact for an up-to-date unit, and a receipt that names no
    # freshly linked binary is exactly the `bin=` shape this tool rejects. Touch, do not edit:
    # the bytes stay identical and the patch hash still certifies the source.
    for anchor in ("src/main.rs", "src/lib.rs"):
        if (root / anchor).is_file():
            os.utime(root / anchor)
    artifact_status, artifact_text, artifact_err = run(
        [args.cargo, "test", "--offline", "--locked", "-j1", "--bin", "mc-spine",
         "--no-run", "--message-format=json-render-diagnostics"], root, log)
    if artifact_status != 0:
        fail(f"cargo --no-run exited {artifact_status}: {artifact_err.strip()[-400:]}")
    binary = None
    try:
        binary = cargo_executable(artifact_text)
    except Exception as error:  # the observed shape: grep found no Executable line
        fail(f"no unique Cargo executable: {error}")
    if binary is not None:
        size = binary.stat().st_size
        receipt["binary"] = {"path": str(binary), "bytes": size, "sha256": sha256_file(binary),
                             "mtime": time.strftime("%Y-%m-%dT%H:%M:%SZ",
                                                    time.gmtime(binary.stat().st_mtime))}
        if size <= 0:
            fail(f"binary is empty: {binary}")
        list_status, list_output, _ = run([str(binary), "--list"], root, log)
        receipt["list_exit"] = list_status
        tests = sorted(line.rsplit(": test", 1)[0] for line in list_output.splitlines()
                       if line.endswith(": test"))
        receipt["tests_total"] = len(tests)
        receipt["tests_sha256"] = hashlib.sha256("\n".join(tests).encode()).hexdigest()
        if list_status != 0:
            fail(f"--list exited {list_status}")
        if not tests:
            fail("--list reported zero tests")

    results = []
    for selector in args.filter:
        status, output, _ = run([str(binary), selector, "--test-threads=1"] if binary
                                else ["true-signal-no-binary"], root, log)
        summary = [line for line in output.splitlines() if line.startswith("test result:")]
        parsed = summary[-1] if summary else ""
        results.append({"filter": selector, "exit": status, "summary": parsed})
        if binary is None:
            continue
        if status != 0:
            fail(f"filter {selector!r} exited {status}")
        match = re.match(r"test result: ok\. ([1-9][0-9]*) passed; 0 failed; ([0-9]+) ignored;", parsed)
        if match is None:
            fail(f"filter {selector!r} did not run and pass a test: {parsed or 'no summary'}")
    receipt["filters_run"] = results

    source_end = source_identity(root)
    receipt["source_at_finish"] = source_end
    if source_end["head"] != source["head"] or source_end["patch_sha256"] != source["patch_sha256"]:
        fail("source changed during the run")

    receipt["finish"] = utc_now()
    receipt["status"] = "PASS" if not problems else "FAIL"
    (out / "runs.json").write_text(json.dumps(log, indent=1, ensure_ascii=False) + "\n")
    (out / "receipt.json").write_text(json.dumps(receipt, indent=1, ensure_ascii=False) + "\n")
    print(f"{receipt['status']} head={source['head']} dirty={source['dirty']} "
          f"tests={receipt.get('tests_total', 0)} problems={len(problems)}")
    for problem in problems:
        print(f"  FAIL: {problem}", file=sys.stderr)
    return 0 if not problems else 1


if __name__ == "__main__":
    sys.exit(main())

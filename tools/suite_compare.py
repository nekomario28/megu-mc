#!/usr/bin/env python3
"""Compare terminal Rust suite results, not nested child-process test summaries."""
import argparse
import json
from pathlib import Path
import re


def summary(text):
    matches = list(re.finditer(r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;.*$", text, re.M))
    if not matches:
        raise ValueError("missing terminal suite result")
    terminal = matches[-1]
    passed, failed, ignored = map(int, terminal.groups()[1:])
    # Cargo's final failed-name roster precedes the terminal summary; child
    # processes may have printed their own '1 passed' or failure blocks earlier.
    before = text[:terminal.start()]
    roster = before.rsplit("\nfailures:\n", 1)[-1] if failed else ""
    names = sorted(re.findall(r"^    (\S+)$", roster, re.M))
    if len(names) != failed or len(set(names)) != failed:
        raise ValueError("incomplete terminal failed-name roster")
    if (terminal.group(1) == "ok") != (failed == 0):
        raise ValueError("inconsistent terminal result")
    return dict(passed=passed, failed=failed, ignored=ignored, failures=names)


def compare(baseline, current, added_passes):
    old, new = summary(baseline), summary(current)
    result = dict(baseline=old, current=new,
                  added_failures=sorted(set(new["failures"]) - set(old["failures"])),
                  removed_failures=sorted(set(old["failures"]) - set(new["failures"])))
    result["unchanged_failure_baseline"] = (old["failures"] == new["failures"]
        and old["ignored"] == new["ignored"] and new["passed"] == old["passed"] + added_passes)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("current", type=Path)
    parser.add_argument("--added-passes", type=int, required=True)
    args = parser.parse_args()
    try:
        result = compare(args.baseline.read_text(), args.current.read_text(), args.added_passes)
    except (OSError, ValueError) as error:
        parser.exit(2, str(error) + "\n")
    print(json.dumps(result, indent=2))
    # This is attribution of a red baseline, never an overall acceptance verdict.
    raise SystemExit(0 if result["unchanged_failure_baseline"] else 1)


if __name__ == "__main__":
    main()

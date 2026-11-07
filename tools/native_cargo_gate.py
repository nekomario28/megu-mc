#!/usr/bin/env python3
"""Run exactly one CI-only Rust/Java paired test and prove it actually executed.

Cargo test exits 0 when a filter matches zero tests. A successful Cargo process
alone is therefore NOT sufficient parity evidence.
"""
import re
import subprocess
import sys

NAMES = {
    "native_survival_fragile_port_pair",
    "native_neighbor_shape_port_pair",
    "native_level_ticks_port_delivery_pair",
}
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def validate_output(output: str, name: str) -> None:
    if name not in NAMES:
        raise ValueError("unknown native pairing target")
    cleaned = ANSI.sub("", output)
    target = rf"^test tick_tests::{re.escape(name)} \.\.\. ok$"
    if re.search(target, cleaned, flags=re.MULTILINE) is None:
        raise ValueError(f"native pair {name} did not execute and pass exactly")
    summary = r"^test result: ok\. 1 passed; 0 failed; 0 ignored; \d+ (?:filtered out|measured)"
    if re.search(summary, cleaned, flags=re.MULTILINE) is None:
        raise ValueError("Cargo did not report exactly one passing native paired test")


def main() -> int:
    if len(sys.argv) != 2 or sys.argv[1] not in NAMES:
        print("usage: native_cargo_gate.py <one of the 3 native pair test names>", file=sys.stderr)
        return 2
    name = sys.argv[1]
    command = ["cargo", "test", "--locked", "--features", "native-oracle",
               f"tick_tests::{name}", "--", "--exact", "--nocapture"]
    result = subprocess.run(command, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, check=False)
    print(result.stdout, end="", flush=True)
    if result.returncode:
        print(f"Cargo native pairing failed: {result.returncode}", file=sys.stderr)
        return result.returncode
    try:
        validate_output(result.stdout, name)
    except ValueError as error:
        print(f"FAIL CLOSED: {error}", file=sys.stderr)
        return 1
    print(f"MANDATORY_NATIVE_PAIR {name} PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())

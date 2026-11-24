#!/usr/bin/env python3
"""Run independent, zero-build CI gates concurrently, with deterministic reports."""
import concurrent.futures
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
# Keep this list in sync with the former fast step in ci.yml. Shell ratchets and
# the node check remain in the workflow; live E2E is conditional on its file.
GATES = (
    "tools/provenance_check.py --strict-zero-gl",
    "tests/public_hygiene_test.py",
    "tests/release_workflow_test.py",
    "tests/ci_playability_gate_test.py",
    "tests/megu_tui_test.py",
    "tests/client_vertical_wiring_audit_test.py",
    "tools/client_vertical_wiring_audit.py",
    "tests/opt_wiring_audit_test.py",
    "tools/opt_wiring_audit.py",
    "tools/wiring_connectivity_audit.py",  # no --fail-on-stale, as before
    "tools/vanilla_mechanics_coverage_audit.py",
    "tools/docs_link_audit.py",
    "tools/hardcode_guard_audit.py",
    "tests/hardcode_guard_test.py",
    "tools/version_constant_audit.py",
    "tools/registry_table_parity.py",
    "tools/server_files_parity.py",
    "tests/java_toolchain_test.py",
    "tests/verify_gate_coverage_test.py",
    "tests/installer_cross_os_test.py",
    "tests/wiring_audit_test.py",
    "tests/public_export_test.py",
    "tests/release_bundle_test.py",
    "tests/item_interaction_audit_test.py",
    "website/check_site.py",
    "tools/security_invariants_audit.py",
    "tests/security_invariants_test.py",
)


def run(command):
    start = time.perf_counter()
    try:
        result = subprocess.run([sys.executable, *command.split()], cwd=ROOT,
                                text=True, capture_output=True, check=False)
        return command, result.returncode, time.perf_counter() - start, result.stdout, result.stderr
    except OSError as exc:
        return command, 1, time.perf_counter() - start, "", str(exc)


def main():
    gates = list(GATES)
    if (ROOT / "tests/live_playability_e2e_test.py").is_file():
        gates.append("tests/live_playability_e2e_test.py")
    start = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=min(8, os.cpu_count() or 4)) as pool:
        results = sorted(pool.map(run, gates), key=lambda entry: entry[0])
    print("STATUS   MS  GATE", flush=True)
    for command, status, elapsed, _, _ in results:
        print(f"{'PASS' if status == 0 else 'FAIL':6} {elapsed * 1000:6.0f}  python3 {command}", flush=True)
    failures = [item for item in results if item[1]]
    for command, status, _, stdout, stderr in failures:
        print(f"\n--- python3 {command} (exit {status}) stdout ---\n{stdout}", flush=True)
        print(f"--- python3 {command} stderr ---\n{stderr}", file=sys.stderr, flush=True)
    print(f"{len(results) - len(failures)}/{len(results)} passed in {time.perf_counter() - start:.2f}s", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())

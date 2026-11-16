#!/usr/bin/env python3
"""Report which contributor checks this checkout can actually run, without running them.

This answers three questions a newcomer has before any test: what works with only the
tracked tree, which prerequisite is missing and why, and which command to type next.
It never builds, downloads, installs, starts a server or executes a binary it found
in the tree: it reads source and files, and it runs exactly two bounded probes
(`rustup toolchain list`, `javac -version`) whose only effect is to print a version.

Why the probes are those two. Inside a repository with a `rust-toolchain.toml`,
`cargo`/`rustc` are rustup proxies: invoking them can try to satisfy the pinned
channel and its components over the network. So this script does not invoke them at
all. `rustup toolchain list` only reads the installed toolchain directories, and
`javac -version` compiles nothing. A pinned compiler that is not installed is
therefore reported as MISSING by this script and *not* verified against the source.

Statuses
  OK          the check ran and the requirement holds
  MISSING     the check ran and the thing is not there
  NOT_CHECKED the check could not establish anything (absent probe, timeout, or a
              check this preflight deliberately does not perform)
  ERROR       something is present and wrong (for example a java archive that is not
              the pinned build)

Exit status
  0  every required check is OK and nothing errored. Optional native capabilities may
     be missing: the report then says which routes are unavailable instead of
     pretending the environment is complete.
  1  a required check is MISSING, or any check is ERROR.
  2  the preflight itself could not run (no repository root, unreadable source).

Every line states whether it executed something (`ran:`) or skipped it (`not run:`),
so a report is never mistaken for work that did not happen.

  python3 tools/dev_check.py [--timeout-ms N]     # run from anywhere
"""
import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

DEFAULT_TIMEOUT_MS = 5000
MIN_TIMEOUT_MS = 100
MAX_TIMEOUT_MS = 60000

OK = "OK"
MISSING = "MISSING"
NOT_CHECKED = "NOT_CHECKED"
ERROR = "ERROR"

# The floor is set by the source, not by taste: tools/p0_2_audit.py and
# tools/feature_type_coverage.py call str.removeprefix (Python 3.9), and the
# fixtures tier imports those tools.
MIN_PYTHON = (3, 9)

# Data the crate reads at run time relative to the repository root. This is the
# coarse existence check: the producers below own the file-level truth.
RUNTIME_DATA = (
    "assets",
    "tests/data",
    "datapacks/vanilla",
    "config_packets.bin",
)

# Longest snippet of a subprocess message to quote back.
QUOTE = 300


class Result:
    def __init__(self, key, status, summary, executed, required=False, detail=""):
        self.key = key
        self.status = status
        self.summary = summary
        self.executed = executed
        self.required = required
        self.detail = detail


def read_text(path):
    return path.read_text(encoding="utf-8", errors="replace")


def quote(text):
    text = " ".join(text.split())
    return text[:QUOTE] + ("…" if len(text) > QUOTE else "")


def run(argv, timeout_ms):
    """Run one bounded read-only probe; return (exit, stdout+stderr, elapsed_ms, timed_out)."""
    started = time.monotonic()
    try:
        proc = subprocess.run(argv, cwd=str(ROOT), stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=timeout_ms / 1000.0)
    except subprocess.TimeoutExpired:
        elapsed = int((time.monotonic() - started) * 1000)
        return None, "", elapsed, True
    except OSError as exc:
        elapsed = int((time.monotonic() - started) * 1000)
        return None, str(exc), elapsed, False
    elapsed = int((time.monotonic() - started) * 1000)
    return proc.returncode, (proc.stdout or "") + (proc.stderr or ""), elapsed, False


def ran(argv, code, elapsed, timed_out):
    command = " ".join(argv)
    if timed_out:
        return f"ran: {command} (timed out after {elapsed} ms)"
    return f"ran: {command} (exit {code}, {elapsed} ms)"


def check_python():
    version = sys.version_info
    text = f"{version.major}.{version.minor}.{version.micro}"
    executed = "ran: the interpreter already running (no subprocess)"
    if (version.major, version.minor) >= MIN_PYTHON:
        return Result("python", OK, text, executed, required=True,
                      detail=f">= {MIN_PYTHON[0]}.{MIN_PYTHON[1]} (str.removeprefix in tools/)")
    return Result("python", MISSING, text, executed, required=True,
                  detail=f"needs >= {MIN_PYTHON[0]}.{MIN_PYTHON[1]}; this interpreter is too old")


def pinned_channel():
    """The channel from rust-toolchain.toml, or None when the file or field is absent."""
    path = ROOT / "rust-toolchain.toml"
    if not path.is_file():
        return None, "rust-toolchain.toml is absent"
    match = re.search(r'^\s*channel\s*=\s*"([^"]+)"', read_text(path), re.MULTILINE)
    if not match:
        return None, "rust-toolchain.toml has no channel line"
    return match.group(1), ""


def check_toolchain_pin():
    channel, problem = pinned_channel()
    executed = "ran: read rust-toolchain.toml"
    if channel is None:
        return Result("toolchain_pin", MISSING, problem, executed, required=True,
                      detail="cargo would fall back to a floating toolchain, so a build here "
                             "is not the build CI pins")
    return Result("toolchain_pin", OK, channel, executed, required=True,
                  detail="rust-toolchain.toml (the install state is a separate line)")


def check_toolchain_installed(timeout_ms):
    channel, problem = pinned_channel()
    if channel is None:
        return Result("toolchain_installed", NOT_CHECKED, "no pinned channel to look for",
                      f"not run: {problem}", detail="")
    rustup = shutil.which("rustup")
    if rustup is None:
        return Result("toolchain_installed", NOT_CHECKED, "rustup is not on PATH",
                      "not run: no rustup; cargo would have to resolve the channel itself")
    code, output, elapsed, timed_out = run([rustup, "toolchain", "list"], timeout_ms)
    executed = ran(["rustup", "toolchain", "list"], code, elapsed, timed_out)
    if timed_out:
        return Result("toolchain_installed", NOT_CHECKED, "listing timed out", executed,
                      detail="a timed-out probe is not an installed toolchain")
    if code != 0:
        return Result("toolchain_installed", NOT_CHECKED, "listing failed", executed,
                      detail=quote(output))
    installed = [line.split()[0] for line in output.splitlines() if line.strip()]
    match = [entry for entry in installed
             if entry == channel or entry.startswith(channel + "-")]
    if match:
        return Result("toolchain_installed", OK, match[0], executed,
                      detail="not invoked by this preflight: no compile, no network; "
                             "rustfmt/clippy components are not verified here")
    return Result("toolchain_installed", MISSING, f"{channel} is not installed", executed,
                  detail=f"installed: {', '.join(installed) or '(none)'}; "
                         f"install it yourself with: rustup toolchain install {channel}")


def check_build_inputs():
    """Every literal include_str!/include_bytes! path must exist, or the crate cannot build."""
    sources = sorted((ROOT / "src").glob("*.rs"))
    executed = f"ran: scanned {len(sources)} src/*.rs for include_str!/include_bytes!"
    if not sources:
        return Result("build_inputs", MISSING, "no src/*.rs", executed, required=True,
                      detail="this is not the mc-spine tree")
    pattern = re.compile(r'include_(?:str|bytes)!\s*\(\s*"([^"]+)"')
    sites = []
    missing = []
    outside = []
    for source in sources:
        for match in pattern.finditer(read_text(source)):
            literal = match.group(1)
            target = (source.parent / literal).resolve()
            try:
                relative = target.relative_to(ROOT)
            except ValueError:
                outside.append(f"{source.name}: {literal}")
                continue
            sites.append(relative)
            if not target.is_file():
                missing.append(f"{relative} (from {source.name})")
    if outside:
        return Result("build_inputs", ERROR, "include path escapes the repository", executed,
                      required=True, detail="; ".join(outside[:4]))
    if missing:
        return Result("build_inputs", MISSING,
                      f"{len({item.split(' (from')[0] for item in missing})} included file(s) absent",
                      executed, required=True,
                      detail="the crate will not compile without these: "
                             + "; ".join(missing[:8]))
    return Result("build_inputs", OK, f"{len(sites)} include sites resolve", executed,
                  required=True, detail="")


def check_runtime_data():
    parts = []
    absent = []
    for entry in RUNTIME_DATA:
        path = ROOT / entry
        if not path.exists():
            absent.append(entry)
            continue
        if path.is_dir():
            count = sum(1 for item in path.rglob("*") if item.is_file())
            if count == 0:
                absent.append(entry)
                continue
            parts.append(f"{entry} ({count} files)")
        else:
            size = path.stat().st_size
            if size == 0:
                absent.append(entry)
                continue
            parts.append(f"{entry} ({size} B)")
    executed = "ran: checked the runtime inputs src/ reads from the repository root"
    if absent:
        return Result("runtime_data", MISSING, ", ".join(absent), executed, required=True,
                      detail="tests and the binary open these relative to the repository root, "
                             "so both the files and the cwd matter")
    return Result("runtime_data", OK, ", ".join(parts), executed, required=True, detail="")


def check_crate_cache():
    return Result("crate_cache", NOT_CHECKED, "not inspected",
                  "not run: a crate cache can only be inspected by starting cargo, which may "
                  "fetch or install",
                  detail="the first build needs the crates in Cargo.lock available "
                         "(CI runs `cargo fetch --locked` before the offline build)")


def jar_pin():
    """(sha1, default path, problem) read from tools/vanilla_java.py, without importing it."""
    source = ROOT / "tools/vanilla_java.py"
    if not source.is_file():
        return None, None, "tools/vanilla_java.py is absent"
    text = read_text(source)
    digest = re.search(r'JAR_SHA1\s*=\s*"([0-9a-f]{40})"', text)
    default = re.search(r'Path\.home\(\)\s*/\s*"([^"]+)"', text)
    if not digest:
        return None, None, "tools/vanilla_java.py declares no JAR_SHA1"
    return digest.group(1), (Path.home() / default.group(1)) if default else None, ""


def sha1_of(path):
    digest = hashlib.sha1()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def check_vanilla_jar():
    pinned, default, problem = jar_pin()
    if pinned is None:
        return Result("vanilla_jar", ERROR, problem, "not run: cannot read the pin",
                      detail="without the pinned identity a jar cannot be judged")
    env = os.environ.get("MC_SPINE_VANILLA_SERVER_JAR")
    if env:
        path, origin = Path(env).expanduser(), "MC_SPINE_VANILLA_SERVER_JAR"
    elif default is not None:
        path, origin = default, "the default cache path in tools/vanilla_java.py"
    else:
        return Result("vanilla_jar", NOT_CHECKED, "no path to look at",
                      "not run: tools/vanilla_java.py declares no default path")
    if not path.is_file():
        return Result("vanilla_jar", MISSING, f"no file at {path}",
                      f"ran: looked for the pinned archive ({origin})",
                      detail="only the jar-side oracle and capture stage needs it; this "
                             "preflight does not download it, and it is never redistributed "
                             "or covered by an automatic EULA acceptance")
    try:
        actual = sha1_of(path)
    except OSError as exc:
        return Result("vanilla_jar", ERROR, "unreadable", f"ran: read {path}",
                      detail=quote(str(exc)))
    executed = f"ran: sha1 of {path}"
    if actual != pinned:
        return Result("vanilla_jar", ERROR, f"sha1 {actual}", executed,
                      detail=f"not the pinned 1.21.1 build (expected {pinned}); "
                             "do not run or extract this file")
    return Result("vanilla_jar", OK, f"pinned 1.21.1 build ({origin})", executed,
                  detail="used only by the jar-side oracle/capture tools")


def java_major_required():
    """The JDK major version the jar-side generator documents, or None when it cannot be read.

    Taken from the producer's own text so the number has one home; a hard-coded copy
    here would drift the next time that script is bumped.
    """
    source = ROOT / "tools/vanilla_data/gen_vanilla_data.sh"
    if not source.is_file():
        return None
    match = re.search(r"Java (\d+)", read_text(source))
    return int(match.group(1)) if match else None


def check_jdk(timeout_ms):
    javac = shutil.which("javac")
    java = shutil.which("java")
    missing = [name for name, found in (("javac", javac), ("java", java)) if found is None]
    if missing:
        return Result("jdk", MISSING, f"{', '.join(missing)} not on PATH",
                      "ran: looked for javac and java on PATH",
                      detail="needed only to compile the jar-side probes; the committed "
                             "reference fixtures do not need a JDK")
    code, output, elapsed, timed_out = run([javac, "-version"], timeout_ms)
    if timed_out:
        return Result("jdk", NOT_CHECKED, "javac present, version unverified",
                      ran(["javac", "-version"], code, elapsed, timed_out),
                      detail="a timeout is not a working JDK")
    executed = ran(["javac", "-version"], code, elapsed, timed_out)
    if code != 0:
        return Result("jdk", NOT_CHECKED, "javac present, version unverified", executed,
                      detail=quote(output))
    version = quote(output) or "javac reports no version"
    required = java_major_required()
    reported = re.search(r"(\d+)", output or "")
    major = int(reported.group(1)) if reported else None
    if required is not None and major is not None and major != required:
        return Result("jdk", NOT_CHECKED, version, executed,
                      detail=f"the jar-side generator documents Java {required}; whether the "
                             f"probes work on {major} was not established here")
    return Result("jdk", OK, version, executed,
                  detail="compiles nothing here; the jar-side probe stage is what needs it")


def check_depsgx():
    env = os.environ.get("MC_SPINE_DEPSGX")
    if env:
        path = Path(env).expanduser()
        if not path.is_file():
            return Result("depsgx", MISSING, f"MC_SPINE_DEPSGX is not a file: {path}",
                          "ran: checked MC_SPINE_DEPSGX")
        if not os.access(path, os.X_OK):
            return Result("depsgx", ERROR, "not executable", "ran: checked MC_SPINE_DEPSGX",
                          detail=str(path))
        return Result("depsgx", OK, f"MC_SPINE_DEPSGX={path}",
                      "ran: checked MC_SPINE_DEPSGX (the tool was not executed)",
                      detail="presence only: the version is unverified, so whether it is the "
                             "pinned build is unknown until tools/layers_check.sh runs")
    found = shutil.which("depsgx")
    if found is None:
        return Result("depsgx", MISSING, "not on PATH",
                      "ran: looked for depsgx on PATH",
                      detail="tests/layers_gate_test.py and tools/layers_check.sh fail without "
                             "it; CI installs the pinned revision named in "
                             ".github/workflows/ci.yml (GXKIT_REV), or set MC_SPINE_DEPSGX")
    return Result("depsgx", OK, found, "ran: looked for depsgx on PATH (not executed)",
                  detail="presence only: the version is unverified")


def render(results, timeout_ms):
    lines = ["mc-spine preflight (read-only: no build, no download, no install, no server)",
             f"repo: {ROOT}",
             f"probe timeout: {timeout_ms} ms     python: "
             f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}"]
    lines.append("")
    for result in results:
        label = f"[{result.status:<11}]"
        lines.append(f"{label} {result.key:<18} {result.summary}")
        if result.detail:
            lines.append(f"{'':<13} {'':<18} why: {result.detail}")
        lines.append(f"{'':<13} {'':<18} {result.executed}")
    by_key = {result.key: result for result in results}
    counts = {}
    for result in results:
        counts[result.status] = counts.get(result.status, 0) + 1
    lines.append("")
    lines.append("summary: " + ", ".join(f"{counts.get(status, 0)} {status}"
                                         for status in (OK, MISSING, NOT_CHECKED, ERROR)))
    python_route = all(by_key[key].status == OK
                       for key in ("python", "build_inputs", "runtime_data"))
    lines.append("route python/fixtures: " + ("READY" if python_route else "NOT READY"))
    lines.append("route rust build/test: " +
                 ("READY (toolchain installed)" if by_key["toolchain_installed"].status == OK
                  else "NOT READY (" + by_key["toolchain_installed"].summary + ")"))
    lines.append("route layers/dependency gate: " +
                 ("READY" if by_key["depsgx"].status == OK else "NOT READY (depsgx)"))
    lines.append("route jar/native oracle: " +
                 ("READY" if by_key["vanilla_jar"].status == OK and by_key["jdk"].status == OK
                  else "NOT READY (" + ", ".join(key for key in ("vanilla_jar", "jdk")
                                                 if by_key[key].status != OK) + ")"))
    lines.append("")
    lines.append("next checks (canonical commands; nothing below was run here):")
    lines.append("  exact file list for the no-asset python tier:")
    lines.append("    grep -E '^python3 tests/' tests/verify.sh")
    lines.append("  one file while working:  python3 tests/<name>_test.py")
    lines.append("  this preflight's own test:  python3 tests/dev_check_test.py")
    if by_key["depsgx"].status != OK:
        lines.append("  the python tier is not all green here: tests/layers_gate_test.py needs "
                     "the pinned depsgx")
    if by_key["toolchain_installed"].status == OK:
        lines.append("  Rust:  cargo test --offline --locked -j 2 -- --test-threads=1")
    else:
        lines.append("  Rust is unavailable until the pinned toolchain is installed by you "
                     "(this script did not install it)")
    lines.append("  full pre-merge gate (long: build, wire, adapters, mutation):  "
                 "bash tests/verify.sh")
    lines.append("")
    lines.append("notes:")
    lines.append("  - a green preflight is not a green suite: this inspects inputs, not behaviour.")
    lines.append("  - an optional route marked NOT READY does not make the repository broken; "
                 "it names what you cannot run yet and why.")
    lines.append("  - tests/README.md has the tiers and their prerequisites; tools/README.md "
                 "routes to the tools.")
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Report which contributor checks this checkout can run, without running "
                    "them. Read-only: no build, download, install or server.")
    parser.add_argument("--timeout-ms", type=int,
                        default=int(os.environ.get("MC_SPINE_DEV_CHECK_TIMEOUT_MS",
                                                   DEFAULT_TIMEOUT_MS)),
                        help="bound for each probe command (default %(default)s)")
    args = parser.parse_args(argv)
    timeout_ms = max(MIN_TIMEOUT_MS, min(MAX_TIMEOUT_MS, args.timeout_ms))

    if not (ROOT / "Cargo.toml").is_file() or not (ROOT / "src").is_dir():
        print(f"dev_check: {ROOT} is not the mc-spine root (no Cargo.toml or src/)",
              file=sys.stderr)
        return 2
    try:
        results = [
            check_python(),
            check_toolchain_pin(),
            check_toolchain_installed(timeout_ms),
            check_build_inputs(),
            check_runtime_data(),
            check_crate_cache(),
            check_vanilla_jar(),
            check_jdk(timeout_ms),
            check_depsgx(),
        ]
    except Exception as exc:  # noqa: BLE001 - a preflight crash must not look like a report
        print(f"dev_check: internal error: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 2

    print(render(results, timeout_ms))
    blocked = any(result.status == ERROR or (result.required and result.status != OK)
                  for result in results)
    if blocked:
        print("\nresult: BLOCKED (exit 1)")
        return 1
    print("\nresult: READY_FOR_PYTHON_CHECKS (exit 0)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

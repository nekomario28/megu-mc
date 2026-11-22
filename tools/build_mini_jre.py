#!/usr/bin/env python3
"""Opt-in ultra-small headless delegate JRE builder and low-RAM worker launcher aid.

  python3 tools/build_mini_jre.py --out <dir> \
      [--profile minimal|neoforge-delegate] [--xms-mib 32] [--xmx-mib 256] \
      [--jlink /path/to/jlink] [--dry-run] [--json]

megu's Rust core already owns chunks, lighting, palettes, network and save
storage, so a Tier 2/3 JVM delegation only needs a runtime large enough to boot
a headless mod loader worker. This tool builds that runtime on demand from the
operator's own installed JDK 21+ `jlink` — nothing JVM-related is ever shipped
inside the repository or the core binary, and nothing is built unless the
operator asks.

Curated headless module profiles:

  minimal            java.base, java.logging, jdk.unsupported
                     (~28-35 MB: simple delegate workers, no management/XML/SQL)
  neoforge-delegate  java.base, java.logging, java.management, java.naming,
                     java.xml, java.sql, java.instrument, jdk.unsupported,
                     jdk.crypto.ec
                     (covers NeoForge/Fabric/Netty/ASM delegate needs while
                     stripping java.desktop, java.compiler, jdk.javadoc,
                     jdk.jconsole, man pages, debug symbols and headers)

Build layout: `jlink` writes into a sibling staging directory
`.<out-name>.jlink-staging`, `mini-jre-manifest.json` (schema
`megu-mini-jre/1`) is generated inside it, and the finished tree is then
atomically renamed onto `--out`. Any failure removes the staging directory and
leaves `--out` untouched. A pre-existing `--out` or a leftover staging
directory is refused rather than overwritten.

`--compress=zip-6` is preferred; `jlink --help` is probed once for the `zip-N`
syntax and a build that still fails with a compress-related error retries once
with the legacy `--compress=2`, so older JDKs degrade gracefully.

`--dry-run --json` reports the exact jlink argv, module list, delegate JVM
flags and detected host jlink availability without writing anything to disk.

Exit status: 0 ok, 1 build/runtime failure (jlink missing or failed, output
refused), 2 usage error (bad arguments, unsafe --out, heap bounds violated).
Pure Python 3 stdlib, no shell.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

SCHEMA = "megu-mini-jre/1"
MANIFEST_NAME = "mini-jre-manifest.json"

PROFILES = {
    "minimal": (
        "java.base",
        "java.logging",
        "jdk.unsupported",
    ),
    "neoforge-delegate": (
        "java.base",
        "java.logging",
        "java.management",
        "java.naming",
        "java.xml",
        "java.sql",
        "java.instrument",
        "jdk.unsupported",
        "jdk.crypto.ec",
    ),
}

# Lean headless delegate flags: Rust owns chunks, lighting, palettes, network
# and save storage, so the worker only needs a bounded serial-GC heap.
DELEGATE_GC_FLAGS = {"serial": "-XX:+UseSerialGC"}
MIN_HEAP_MIB = 32
MAX_HEAP_MIB = 4096
MAX_METASPACE = "-XX:MaxMetaspaceSize=96m"


class UsageError(Exception):
    """Bad operator input; exits 2."""


class BuildError(Exception):
    """Environment or jlink failure; exits 1."""


def platform_tool_name(base):
    """`<base>.exe` on Windows (`os.name == 'nt'`), `<base>` elsewhere."""
    return f"{base}.exe" if os.name == "nt" else base


def validate_out_dir(raw):
    """Resolve `--out`, rejecting `..`, NUL/control chars and symlink escapes."""
    if not raw or not raw.strip():
        raise UsageError("--out must not be empty")
    if any(ord(ch) < 0x20 or ord(ch) == 0x7F for ch in raw):
        raise UsageError("--out contains a NUL or control character")
    candidate = Path(raw)
    if ".." in candidate.parts:
        raise UsageError("--out must not contain '..' path components")
    expected = Path(os.path.realpath(candidate.parent)) / candidate.name
    if Path(os.path.realpath(candidate)) != expected:
        raise UsageError("--out resolves through a symlink; refusing escape")
    return expected


def staging_dir_for(out_dir):
    return out_dir.parent / ("." + out_dir.name + ".jlink-staging")


def resolve_jlink(explicit):
    """Return the validated jlink path, or None when discovery fails."""
    if explicit is not None:
        candidate = Path(explicit)
        # Accept `...\bin\jlink` as well as `...\bin\jlink.exe` on Windows.
        if os.name == "nt" and not candidate.is_file():
            with_suffix = candidate.with_name(candidate.name + ".exe")
            if with_suffix.is_file():
                candidate = with_suffix
        if not candidate.is_file():
            raise UsageError(f"--jlink does not name a file: {explicit}")
        if not os.access(candidate, os.X_OK):
            raise UsageError(f"--jlink is not executable: {explicit}")
        return Path(os.path.realpath(candidate))
    # On Windows the JDK ships `jlink.exe`; `which` also resolves it via
    # PATHEXT, but probe the literal name first so a missing PATHEXT
    # entry cannot hide an installed JDK.
    names = ["jlink.exe", "jlink"] if os.name == "nt" else ["jlink"]
    for name in names:
        found = shutil.which(name)
        if found:
            return Path(found).resolve()
    return None


def detect_compress(jlink_bin):
    """Probe `jlink --help` for zip-N syntax; fall back to the legacy level 2."""
    try:
        proc = subprocess.run(
            [str(jlink_bin), "--help"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return "2"
    return "zip-6" if "zip-" in (proc.stdout + proc.stderr) else "2"


def build_jlink_argv(jlink_bin, modules, staging_dir, compress="zip-6"):
    """The exact jlink invocation for one curated module set."""
    return [
        str(jlink_bin),
        "--add-modules",
        ",".join(modules),
        "--strip-debug",
        "--no-man-pages",
        "--no-header-files",
        f"--compress={compress}",
        "--output",
        str(staging_dir),
    ]


def delegate_flags(xms_mib, xmx_mib, gc_mode="serial"):
    """The lean headless flag list the delegate worker is launched with."""
    if gc_mode not in DELEGATE_GC_FLAGS:
        raise UsageError(f"unsupported gc_mode: {gc_mode}")
    for name, value in (("xms_mib", xms_mib), ("xmx_mib", xmx_mib)):
        if not isinstance(value, int) or isinstance(value, bool):
            raise UsageError(f"{name} must be an integer")
    if not (MIN_HEAP_MIB <= xms_mib <= xmx_mib <= MAX_HEAP_MIB):
        raise UsageError(
            f"heap bounds violated: need "
            f"{MIN_HEAP_MIB} <= xms ({xms_mib}) <= xmx ({xmx_mib}) <= {MAX_HEAP_MIB}"
        )
    return [
        "-Djava.awt.headless=true",
        f"-Xms{xms_mib}m",
        f"-Xmx{xmx_mib}m",
        DELEGATE_GC_FLAGS[gc_mode],
        MAX_METASPACE,
        "-XX:+ExitOnOutOfMemoryError",
        "-XX:-UsePerfData",
    ]


def build_delegate_jvm_argv(jre_dir, xms_mib, xmx_mib, gc_mode="serial"):
    """Full worker argv: <jre>/bin/java[.exe] plus the lean headless flags."""
    java_bin = Path(jre_dir) / "bin" / platform_tool_name("java")
    return [str(java_bin)] + delegate_flags(xms_mib, xmx_mib, gc_mode)


def dir_size_bytes(root):
    """Sum st_size over regular files, never following symlinks."""
    total = 0
    for dirpath, _dirnames, filenames in os.walk(root, followlinks=False):
        for name in filenames:
            entry = Path(dirpath) / name
            if entry.is_symlink():
                continue
            total += entry.stat().st_size
    return total


def write_manifest(staging_dir, profile, modules, flags, xms_mib, xmx_mib,
                   gc_mode, compress):
    """Emit the deterministic manifest; size_bytes excludes the manifest itself."""
    manifest = {
        "schema": SCHEMA,
        "profile": profile,
        "modules": list(modules),
        "delegate_flags": flags,
        "xms_mib": xms_mib,
        "xmx_mib": xmx_mib,
        "gc_mode": gc_mode,
        "compress": compress,
        "size_bytes": dir_size_bytes(staging_dir),
    }
    target = staging_dir / MANIFEST_NAME
    target.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return manifest


def run_jlink(jlink_bin, modules, staging_dir, compress):
    """Run jlink once, retrying a compress-related failure at legacy level 2."""
    argv = build_jlink_argv(jlink_bin, modules, staging_dir, compress)
    proc = subprocess.run(
        argv, stdin=subprocess.DEVNULL, capture_output=True, text=True,
        timeout=900, check=False,
    )
    if (
        proc.returncode != 0
        and compress != "2"
        and "compress" in (proc.stderr or "").lower()
    ):
        argv = build_jlink_argv(jlink_bin, modules, staging_dir, "2")
        proc = subprocess.run(
            argv, stdin=subprocess.DEVNULL, capture_output=True, text=True,
            timeout=900, check=False,
        )
    if proc.returncode != 0:
        raise BuildError(
            f"jlink exited {proc.returncode}: {(proc.stderr or '').strip()}"
        )
    return argv


def build_jre(out_dir, profile, xms_mib, xmx_mib, jlink_bin, gc_mode="serial"):
    """Stage, manifest and atomically install a mini JRE onto out_dir."""
    modules = PROFILES[profile]
    flags = delegate_flags(xms_mib, xmx_mib, gc_mode)
    if out_dir.exists():
        raise BuildError(f"--out already exists, refusing to clobber: {out_dir}")
    staging_dir = staging_dir_for(out_dir)
    if staging_dir.exists():
        raise BuildError(
            f"leftover staging directory, refusing: {staging_dir}"
        )
    out_dir.parent.mkdir(parents=True, exist_ok=True)
    compress = detect_compress(jlink_bin)
    try:
        argv = run_jlink(jlink_bin, modules, staging_dir, compress)
        if not staging_dir.is_dir():
            raise BuildError("jlink exited 0 but produced no output directory")
        compress_used = next(
            arg.split("=", 1)[1] for arg in argv if arg.startswith("--compress=")
        )
        manifest = write_manifest(
            staging_dir, profile, modules, flags, xms_mib, xmx_mib,
            gc_mode, compress_used,
        )
        os.replace(staging_dir, out_dir)
    except BaseException:
        if staging_dir.exists():
            shutil.rmtree(staging_dir, ignore_errors=True)
        raise
    return argv, manifest


def plan(profile, out_dir, xms_mib, xmx_mib, jlink_bin, gc_mode="serial"):
    """The dry-run report: exact argv, modules, flags and host availability."""
    modules = PROFILES[profile]
    staging_dir = staging_dir_for(out_dir)
    compress = detect_compress(jlink_bin) if jlink_bin else "zip-6"
    return {
        "schema": SCHEMA,
        "dry_run": True,
        "profile": profile,
        "modules": list(modules),
        "out_dir": str(out_dir),
        "staging_dir": str(staging_dir),
        "jlink": {
            "bin": str(jlink_bin) if jlink_bin else None,
            "available": jlink_bin is not None,
            "compress": compress,
        },
        "jlink_argv": build_jlink_argv(
            jlink_bin if jlink_bin else platform_tool_name("jlink"),
            modules, staging_dir, compress
        ),
        "xms_mib": xms_mib,
        "xmx_mib": xmx_mib,
        "gc_mode": gc_mode,
        "delegate_flags": delegate_flags(xms_mib, xmx_mib, gc_mode),
        "delegate_argv": build_delegate_jvm_argv(
            out_dir, xms_mib, xmx_mib, gc_mode
        ),
    }


def parse_args(argv):
    parser = argparse.ArgumentParser(
        prog="build_mini_jre.py",
        description="Build an opt-in minimal headless delegate JRE via jlink.",
    )
    parser.add_argument("--out", required=True,
                        help="target directory for the mini JRE (must not exist)")
    parser.add_argument("--profile", choices=sorted(PROFILES),
                        default="minimal")
    parser.add_argument("--xms-mib", type=int, default=32)
    parser.add_argument("--xmx-mib", type=int, default=256)
    parser.add_argument("--jlink", default=None,
                        help="explicit path to a JDK 21+ jlink binary")
    parser.add_argument("--dry-run", action="store_true",
                        help="print the plan without writing to disk")
    parser.add_argument("--json", action="store_true",
                        help="emit machine-readable JSON")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    try:
        out_dir = validate_out_dir(args.out)
        delegate_flags(args.xms_mib, args.xmx_mib)
        jlink_bin = resolve_jlink(args.jlink)
    except UsageError as exc:
        print(f"build_mini_jre.py: error: {exc}", file=sys.stderr)
        return 2

    if args.dry_run:
        report = plan(args.profile, out_dir, args.xms_mib, args.xmx_mib,
                      jlink_bin)
        if args.json:
            print(json.dumps(report, indent=2, sort_keys=True))
        else:
            print(f"profile: {report['profile']}")
            print(f"jlink: {report['jlink']['bin'] or 'not found'} "
                  f"(compress={report['jlink']['compress']})")
            print("jlink argv: " + " ".join(report["jlink_argv"]))
            print("delegate flags: " + " ".join(report["delegate_flags"]))
            print(f"out: {out_dir} (dry-run: nothing written)")
        return 0

    if jlink_bin is None:
        print("build_mini_jre.py: error: no jlink found on PATH and no "
              "--jlink given; install a JDK 21+ or pass --jlink",
              file=sys.stderr)
        return 1
    try:
        argv_used, manifest = build_jre(
            out_dir, args.profile, args.xms_mib, args.xmx_mib, jlink_bin
        )
    except UsageError as exc:
        print(f"build_mini_jre.py: error: {exc}", file=sys.stderr)
        return 2
    except BuildError as exc:
        print(f"build_mini_jre.py: error: {exc}", file=sys.stderr)
        return 1

    if args.json:
        print(json.dumps({
            "schema": SCHEMA,
            "dry_run": False,
            "profile": args.profile,
            "out_dir": str(out_dir),
            "jlink_argv": argv_used,
            "manifest": manifest,
            "manifest_path": str(out_dir / MANIFEST_NAME),
            "delegate_argv": build_delegate_jvm_argv(
                out_dir, args.xms_mib, args.xmx_mib
            ),
        }, indent=2, sort_keys=True))
    else:
        print(f"mini JRE installed: {out_dir} "
              f"({manifest['size_bytes']} bytes, profile={args.profile})")
        print("delegate argv: "
              + " ".join(build_delegate_jvm_argv(
                  out_dir, args.xms_mib, args.xmx_mib)))
        print(f"manifest: {out_dir / MANIFEST_NAME}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

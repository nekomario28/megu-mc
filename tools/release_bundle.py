#!/usr/bin/env python3
"""Build and verify a deterministic, fail-closed mc-spine release bundle.

  tools/release_bundle.py build --binary <path> [--extra-binary <path>]... \
      --out-dir <dir> --version <ver> --target <triple> \
      [--repo-root <dir>] [--git-sha <40hex>]
  tools/release_bundle.py verify --bundle <tar.gz> \
      --manifest <release-manifest.json> --sums <SHA256SUMS>
  tools/release_bundle.py self-test            (or: --self-test)

`build` packages the primary release binary plus any companion binaries given
with the repeatable `--extra-binary` flag, the licence/legal/docs files and
the user-facing setup, translation, Modrinth and vanilla-data tools into

  <out-dir>/mc-spine-<version>-<target>.tar.gz   the bundle
  <out-dir>/release-manifest.json                machine-readable contents
  <out-dir>/SHA256SUMS                           sha256 of both artifacts

Inside the tarball every member lives under the single directory
`mc-spine-<version>-<target>/`:

  <prefix>/<binary basename>                     each release binary (0755)
  <prefix>/LICENSE, COPYING.LESSER, COPYING.AGPL, NOTICE,
      LICENSING.md, THIRD_PARTY.md, README.md    (0644)
  <prefix>/docs/mod-author-guide.md              (0644)
  <prefix>/tools/vanilla_data/gen_vanilla_data.sh
  <prefix>/tools/vanilla_data/version_pack.py
  <prefix>/tools/mod_launcher.py
  <prefix>/tools/megu_start.py
  <prefix>/tools/megu_translate.py
  <prefix>/tools/megu_mods.py                    public setup/mod management,
                                                translation and data helpers

The five-binary release suite is `mc-spine` plus `mc-spine-client`,
`mc-spine-mod-check`, `mc-spine-world-migrate` and `mc-spine-init`. Pass the
server as `--binary` and each of the other four as `--extra-binary`, for example
from the same Cargo release directory. Each binary is recorded independently
in `release-manifest.json` under `files`, with its SHA-256, size and mode.
The CLI also supports single-binary bundles and other companion binaries,
including `mc-spine-client-gpu` and `mc-spine-client-gpu.exe`.

Determinism: members are sorted by name, written to a USTAR archive with
mtime 0, uid/gid 0, empty uname/gname and modes normalised to 0644/0755, and
the gzip wrapper carries mtime 0 and no embedded filename, so the same inputs
produce a byte-identical `.tar.gz`.

Fail-closed purity guard (run inside `build` before anything is written and
again by `verify` over the actual tarball bytes): a bundle that contains any
Mojang-derived or local-only artifact is refused:

  *.jar / *.class                                JVM/vendor bytecode
  config_packets.bin                             captured wire data
  datapacks/vanilla/**                           the vanilla datapack
  assets/pblocks.json, assets/properties.json    removed: no provenance
  resourcepack.zip                               removed: no provenance
  any path component containing `_1211`          generated 1.21.1 data
  world_overlay*.bin*                            local server save state
  .git/** (any `.git` component)                 repository internals
  absolute paths, `..`/`.`/empty components,     unsafe tar members
  links or other non-regular members

`verify` additionally checks the manifest schema (format, pinned mc_targets,
licence identifiers), `manifest.bundle` against the real tarball bytes, every
`SHA256SUMS` line against the real file bytes, and the per-member
sha256/bytes/mode records against the tar as read back - so a tampered
tarball, manifest or sums file is rejected even when the other two agree.

Exit status: 0 ok, 1 validation/purity failure, 2 usage error. Pure Python 3
stdlib; `--self-test` proves determinism, a good verify, and fail-closed
rejection offline inside a TemporaryDirectory.
"""
import argparse
import fnmatch
import gzip
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

FORMAT = 1
PRODUCT = "mc-spine"
MC_TARGETS = ["1.21.1", "26.3"]
LICENSE_ID = "GPL-3.0-or-later"
SUBCRATE_LICENSE = "LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later"

MANIFEST_NAME = "release-manifest.json"
SUMS_NAME = "SHA256SUMS"

REQUIRED_DOCS = (
    "LICENSE",
    "COPYING.LESSER",
    "COPYING.AGPL",
    "NOTICE",
    "LICENSING.md",
    "THIRD_PARTY.md",
    "README.md",
    "docs/mod-author-guide.md",
)
REQUIRED_TOOLS = (
    "tools/vanilla_data/gen_vanilla_data.sh",
    "tools/vanilla_data/version_pack.py",
    "tools/mod_launcher.py",
    "tools/megu_start.py",
    "tools/megu_translate.py",
    "tools/megu_mods.py",
)

# Basenames that are forbidden wherever they appear in a bundle. The two
# `assets/` files and `resourcepack.zip` were removed for lack of provenance
# (see tools/provenance_check.py); the rest are captured or local data.
FORBIDDEN_BASENAMES = frozenset({
    "config_packets.bin",
    "resourcepack.zip",
    "pblocks.json",
    "properties.json",
})
_NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+~-]*$")
_GIT_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
_SUMS_LINE_RE = re.compile(r"^([0-9a-f]{64}) [ *](\S+)$")


class BundleError(Exception):
    """A validation or purity failure. Raised, never sys.exit, so self-test can catch it."""


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def read_bytes(path, what):
    try:
        return Path(path).read_bytes()
    except OSError as error:
        raise BundleError(f"{what}: cannot read {path}: {error}") from error


def forbidden_reason(name):
    """Why a bundle member path is forbidden, or None when it is clean."""
    if not name or name != name.strip():
        return "empty or whitespace-padded path"
    if name.startswith("/") or "\\" in name or "\x00" in name:
        return "not a relative POSIX path"
    parts = name.split("/")
    if any(part in ("", ".", "..") for part in parts):
        return "unsafe path component"
    if ".git" in parts:
        return ".git tree member"
    if any(a == "datapacks" and b == "vanilla" for a, b in zip(parts, parts[1:])):
        return "vanilla datapack member"
    if any("_1211" in part for part in parts):
        return "generated 1.21.1 data asset"
    base = parts[-1]
    if base.endswith((".jar", ".class")):
        return "JVM bytecode is vendor material"
    if base in FORBIDDEN_BASENAMES:
        return f"{base} has no redistributable provenance"
    if fnmatch.fnmatchcase(base, "world_overlay*.bin*"):
        return "local server save state"
    return None


def check_names(names, what):
    """Fail when any staged or archived name hits the purity rules."""
    for name in names:
        reason = forbidden_reason(name)
        if reason is not None:
            raise BundleError(f"{what}: forbidden member {name!r}: {reason}")


def bundle_stem(version, target):
    return f"{PRODUCT}-{version}-{target}"


def check_label(value, what):
    if not _NAME_RE.match(value or "") or ".." in value:
        raise BundleError(f"{what}: unsafe release label {value!r}")
    return value


def git_sha(repo_root, override):
    if override is not None:
        if not _GIT_SHA_RE.match(override):
            raise BundleError(f"--git-sha: expected 40 lowercase hex digits, got {override!r}")
        return override
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=repo_root,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    except OSError as error:
        raise BundleError(f"cannot run git to record the source sha: {error}") from error
    if result.returncode != 0:
        raise BundleError(f"git rev-parse HEAD failed: {result.stderr.strip()}")
    sha = result.stdout.strip()
    if not _GIT_SHA_RE.match(sha):
        raise BundleError(f"git rev-parse returned a non-sha1 {sha!r}; pass --git-sha")
    return sha


def stage_entries(repo_root, binary, extra_binaries=()):
    """The (arcname, data, mode) list the bundle is built from, before the prefix.

    Every binary ships under its own basename with mode 0755; docs ship 0644 and
    the tools keep their tracked executable bit (0755/0644) except for the
    public megu_mods.py CLI, which always ships as 0755. Two binaries may not
    share a basename, and no binary may shadow a required doc/tool path.
    """
    repo_root = Path(repo_root)
    entries = []
    seen_binaries = set()
    for flag, raw in [("--binary", binary)] + [
            ("--extra-binary", path) for path in extra_binaries]:
        path = Path(raw)
        if path.is_symlink() or not path.is_file():
            raise BundleError(f"{flag}: not a regular file: {path}")
        if path.name in seen_binaries:
            raise BundleError(f"duplicate binary basename {path.name!r}")
        if path.name in (MANIFEST_NAME, SUMS_NAME):
            raise BundleError(
                f"{flag}: {path.name!r} collides with a release artifact name")
        seen_binaries.add(path.name)
        entries.append((path.name, read_bytes(path, flag), 0o755))
    for rel in REQUIRED_DOCS:
        source = repo_root / rel
        if source.is_symlink() or not source.is_file():
            raise BundleError(f"required release document missing: {rel}")
        entries.append((rel, read_bytes(source, rel), 0o644))
    for rel in REQUIRED_TOOLS:
        source = repo_root / rel
        if source.is_symlink() or not source.is_file():
            raise BundleError(f"required release tool missing: {rel}")
        mode = 0o755 if rel == "tools/megu_mods.py" or source.stat().st_mode & 0o111 else 0o644
        entries.append((rel, read_bytes(source, rel), mode))
    names = [arcname for arcname, _, _ in entries]
    if len(set(names)) != len(names):
        raise BundleError("a staged path appears twice")
    check_names(names, "staged tree")
    return entries


def write_tarball(entries, prefix, dest):
    """A deterministic .tar.gz: sorted USTAR members, mtime/uid/gid normalised."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for arcname, data, mode in sorted(entries, key=lambda entry: entry[0]):
            info = tarfile.TarInfo(f"{prefix}/{arcname}")
            info.size = len(data)
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mode = mode
            tar.addfile(info, io.BytesIO(data))
    Path(dest).write_bytes(gzip.compress(raw.getvalue(), compresslevel=9, mtime=0))


def manifest_body(version, target, sha, prefix, tar_name, tar_bytes, entries):
    files = [
        {"path": f"{prefix}/{arcname}", "sha256": sha256_hex(data),
         "bytes": len(data), "mode": f"{mode:04o}"}
        for arcname, data, mode in sorted(entries, key=lambda entry: entry[0])
    ]
    return {
        "format": FORMAT,
        "product": PRODUCT,
        "version": version,
        "git_sha": sha,
        "target_triple": target,
        "mc_targets": list(MC_TARGETS),
        "license": LICENSE_ID,
        "subcrate_license": SUBCRATE_LICENSE,
        "bundle": {"name": tar_name, "sha256": sha256_hex(tar_bytes),
                   "bytes": len(tar_bytes)},
        "files": files,
    }


def dump_json(obj):
    return (json.dumps(obj, sort_keys=True, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def sums_body(records):
    """`sha256sum`-format lines, sorted by name: `<hex>  <basename>`."""
    return "".join(f"{digest}  {name}\n" for name, digest in sorted(records.items()))


def write_artifacts(entries, version, target, sha, out_dir):
    """Build the three release artifacts in `out_dir`; returns their paths."""
    prefix = bundle_stem(version, target)
    tar_name = f"{prefix}.tar.gz"
    tar_path = out_dir / tar_name
    write_tarball(entries, prefix, tar_path)
    tar_bytes = read_bytes(tar_path, tar_name)
    manifest = manifest_body(version, target, sha, prefix, tar_name, tar_bytes, entries)
    manifest_path = out_dir / MANIFEST_NAME
    manifest_path.write_bytes(dump_json(manifest))
    sums_path = out_dir / SUMS_NAME
    sums_path.write_text(sums_body({
        tar_name: sha256_hex(tar_bytes),
        MANIFEST_NAME: sha256_hex(manifest_path.read_bytes()),
    }))
    return tar_path, manifest_path, sums_path


# ---- verify -------------------------------------------------------------------

def _no_dup_object(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise BundleError(f"duplicate key {key!r} in JSON object")
        out[key] = value
    return out


def load_json(path, what):
    data = read_bytes(path, what)
    try:
        return json.loads(data.decode("utf-8"), object_pairs_hook=_no_dup_object)
    except UnicodeDecodeError as error:
        raise BundleError(f"{what}: {path}: not UTF-8: {error}") from error
    except BundleError as error:
        raise BundleError(f"{what}: {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise BundleError(f"{what}: malformed JSON in {path}: {error}") from error


def _require_str(value, what):
    if not isinstance(value, str) or not value:
        raise BundleError(f"{what}: expected a non-empty string, got {value!r}")
    return value


def _require_int(value, what, minimum=0):
    if isinstance(value, bool) or not isinstance(value, int):
        raise BundleError(f"{what}: expected an integer, got {value!r}")
    if value < minimum:
        raise BundleError(f"{what}: {value} < {minimum}")
    return value


def validate_manifest(manifest, what=MANIFEST_NAME):
    if not isinstance(manifest, dict):
        raise BundleError(f"{what}: expected a JSON object")
    _require_int(manifest.get("format"), f"{what}.format", minimum=1)
    if manifest["format"] != FORMAT:
        raise BundleError(f"{what}.format: expected {FORMAT}, got {manifest['format']}")
    if manifest.get("product") != PRODUCT:
        raise BundleError(f"{what}.product: expected {PRODUCT!r}, "
                          f"got {manifest.get('product')!r}")
    check_label(_require_str(manifest.get("version"), f"{what}.version"),
                f"{what}.version")
    sha = _require_str(manifest.get("git_sha"), f"{what}.git_sha")
    if not _GIT_SHA_RE.match(sha):
        raise BundleError(f"{what}.git_sha: expected 40 lowercase hex digits")
    check_label(_require_str(manifest.get("target_triple"), f"{what}.target_triple"),
                f"{what}.target_triple")
    if manifest.get("mc_targets") != MC_TARGETS:
        raise BundleError(f"{what}.mc_targets: expected {MC_TARGETS}, "
                          f"got {manifest.get('mc_targets')!r}")
    if manifest.get("license") != LICENSE_ID:
        raise BundleError(f"{what}.license: expected {LICENSE_ID!r}, "
                          f"got {manifest.get('license')!r}")
    if manifest.get("subcrate_license") != SUBCRATE_LICENSE:
        raise BundleError(f"{what}.subcrate_license: expected {SUBCRATE_LICENSE!r}, "
                          f"got {manifest.get('subcrate_license')!r}")
    bundle = manifest.get("bundle")
    if not isinstance(bundle, dict):
        raise BundleError(f"{what}.bundle: expected a JSON object")
    _require_str(bundle.get("name"), f"{what}.bundle.name")
    digest = _require_str(bundle.get("sha256"), f"{what}.bundle.sha256")
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise BundleError(f"{what}.bundle.sha256: expected a 64-hex-digit sha256")
    _require_int(bundle.get("bytes"), f"{what}.bundle.bytes")
    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        raise BundleError(f"{what}.files: expected a non-empty JSON array")
    seen = set()
    for index, record in enumerate(files):
        where = f"{what}.files[{index}]"
        if not isinstance(record, dict):
            raise BundleError(f"{where}: expected a JSON object")
        path = _require_str(record.get("path"), f"{where}.path")
        if path in seen:
            raise BundleError(f"{where}.path: {path!r} listed twice")
        seen.add(path)
        digest = _require_str(record.get("sha256"), f"{where}.sha256")
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise BundleError(f"{where}.sha256: expected a 64-hex-digit sha256")
        _require_int(record.get("bytes"), f"{where}.bytes")
        mode = _require_str(record.get("mode"), f"{where}.mode")
        if not re.fullmatch(r"[0-7]{4}", mode):
            raise BundleError(f"{where}.mode: expected 4 octal digits, got {mode!r}")
    return manifest


def read_tar_members(tar_path):
    """Every member of the tarball, fail-closed on purity, type and layout."""
    members = []
    try:
        with tarfile.open(tar_path, mode="r:gz") as tar:
            for info in tar:
                reason = forbidden_reason(info.name)
                if reason is not None:
                    raise BundleError(
                        f"forbidden member {info.name!r} in {tar_path}: {reason}")
                if not info.isreg():
                    raise BundleError(
                        f"{info.name}: not a regular file (tar type {info.type!r})")
                if info.uid != 0 or info.gid != 0 or info.mtime != 0:
                    raise BundleError(
                        f"{info.name}: metadata not normalised "
                        f"(uid={info.uid} gid={info.gid} mtime={info.mtime})")
                digest = hashlib.sha256()
                size = 0
                with tar.extractfile(info) as stream:
                    while True:
                        chunk = stream.read(1 << 20)
                        if not chunk:
                            break
                        digest.update(chunk)
                        size += len(chunk)
                members.append({"path": info.name, "sha256": digest.hexdigest(),
                                "bytes": size, "mode": f"{info.mode & 0o777:04o}"})
    except BundleError:
        raise
    except (tarfile.TarError, OSError, EOFError, zlib.error) as error:
        raise BundleError(f"{tar_path}: not a readable tar.gz: {error}") from error
    if not members:
        raise BundleError(f"{tar_path}: no members")
    names = [member["path"] for member in members]
    if names != sorted(names):
        raise BundleError(f"{tar_path}: members are not sorted (not a deterministic build)")
    if len(set(names)) != len(names):
        raise BundleError(f"{tar_path}: a member name appears twice")
    return members


def check_sums(sums_path, expected_files):
    """`expected_files`: {basename: absolute path}; every sums line must name one."""
    text = read_bytes(sums_path, SUMS_NAME)
    try:
        text = text.decode("utf-8")
    except UnicodeDecodeError as error:
        raise BundleError(f"{SUMS_NAME}: not UTF-8: {error}") from error
    listed = {}
    for lineno, line in enumerate(text.splitlines(), 1):
        match = _SUMS_LINE_RE.match(line)
        if match is None:
            raise BundleError(f"{sums_path}:{lineno}: malformed SHA256SUMS line {line!r}")
        digest, name = match.groups()
        if name in listed:
            raise BundleError(f"{sums_path}:{lineno}: {name!r} listed twice")
        listed[name] = digest
    if set(listed) != set(expected_files):
        raise BundleError(
            f"{sums_path}: lists {sorted(listed)}, expected exactly "
            f"{sorted(expected_files)}")
    for name, path in expected_files.items():
        actual = sha256_hex(read_bytes(path, name))
        if actual != listed[name]:
            raise BundleError(
                f"{name}: sha256 mismatch ({listed[name][:16]}... in SHA256SUMS, "
                f"actual {actual[:16]}...)")
    return listed


def verify_bundle(bundle_path, manifest_path, sums_path):
    bundle_path, manifest_path, sums_path = map(Path, (bundle_path, manifest_path, sums_path))
    check_sums(sums_path, {
        bundle_path.name: bundle_path,
        manifest_path.name: manifest_path,
    })
    manifest = validate_manifest(load_json(manifest_path, MANIFEST_NAME))
    tar_bytes = read_bytes(bundle_path, bundle_path.name)
    bundle = manifest["bundle"]
    if bundle["name"] != bundle_path.name:
        raise BundleError(
            f"manifest bundle.name {bundle['name']!r} does not match {bundle_path.name!r}")
    actual = sha256_hex(tar_bytes)
    if actual != bundle["sha256"]:
        raise BundleError(
            f"{bundle_path.name}: sha256 mismatch (manifest {bundle['sha256'][:16]}..., "
            f"actual {actual[:16]}...)")
    if len(tar_bytes) != bundle["bytes"]:
        raise BundleError(
            f"{bundle_path.name}: {len(tar_bytes)} bytes, manifest records {bundle['bytes']}")
    members = read_tar_members(bundle_path)
    prefix = bundle["name"][:-len(".tar.gz")] if bundle["name"].endswith(".tar.gz") else None
    if prefix is None:
        raise BundleError(f"manifest bundle.name {bundle['name']!r} is not a .tar.gz name")
    files = manifest["files"]
    recorded = {record["path"]: record for record in files}
    actual_paths = {member["path"] for member in members}
    if actual_paths != set(recorded):
        raise BundleError(
            f"{bundle_path.name}: members differ from manifest.files: "
            f"only in tar {sorted(actual_paths - set(recorded))[:4]}, "
            f"only in manifest {sorted(set(recorded) - actual_paths)[:4]}")
    for member in members:
        if not member["path"].startswith(prefix + "/"):
            raise BundleError(
                f"{member['path']}: not under the bundle directory {prefix!r}")
        record = recorded[member["path"]]
        for field in ("sha256", "bytes", "mode"):
            if member[field] != record[field]:
                raise BundleError(
                    f"{member['path']}: {field} mismatch (tar {member[field]}, "
                    f"manifest {record[field]})")
    return members


# ---- commands -----------------------------------------------------------------

def cmd_build(args):
    repo_root = Path(args.repo_root).resolve() if args.repo_root else ROOT
    check_label(args.version, "--version")
    check_label(args.target, "--target")
    if not repo_root.is_dir():
        raise BundleError(f"--repo-root: not a directory: {repo_root}")
    sha = git_sha(repo_root, args.git_sha)
    entries = stage_entries(repo_root, args.binary, args.extra_binary or ())
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    prefix = bundle_stem(args.version, args.target)
    outputs = [out_dir / f"{prefix}.tar.gz", out_dir / MANIFEST_NAME, out_dir / SUMS_NAME]
    existing = [str(path) for path in outputs if path.exists()]
    if existing:
        raise BundleError(f"refusing to overwrite existing release artifact(s): "
                          f"{', '.join(existing)}")
    tar_path, manifest_path, sums_path = write_artifacts(
        entries, args.version, args.target, sha, out_dir)
    # The guard runs on the staged names above and once more on the bytes on disk,
    # so a build that could not pass `verify` is never reported as written.
    members = verify_bundle(tar_path, manifest_path, sums_path)
    print(f"built {tar_path} ({tar_path.stat().st_size} bytes, "
          f"{len(members)} members), {MANIFEST_NAME}, {SUMS_NAME}")
    return 0


def cmd_verify(args):
    members = verify_bundle(args.bundle, args.manifest, args.sums)
    print(f"verify: clean - {len(members)} members, "
          f"{args.bundle} matches {SUMS_NAME} and {MANIFEST_NAME}")
    return 0


# ---- self-test ------------------------------------------------------------------

SELFTEST_GIT_SHA = "0" * 39 + "1"


def _fixture_repo(root):
    """A minimal stand-in repository: the docs/tools build requires, nothing else."""
    for rel in REQUIRED_DOCS:
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(f"stub {rel} for self-test\n")
    for rel in REQUIRED_TOOLS:
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(f"#!/bin/sh\n# stub {rel}\n")
        target.chmod(0o644 if rel == "tools/megu_mods.py" else 0o755)
    binary = root / "build" / PRODUCT
    binary.parent.mkdir(parents=True, exist_ok=True)
    binary.write_bytes(b"\x7fELF fake mc-spine binary v1\n")
    binary.chmod(0o755)
    return binary


def _build(tmp, name, repo, binary, version="0.1.0",
           target="x86_64-unknown-linux-gnu", extra=()):
    out = Path(tmp) / name
    rc = main(["build", "--binary", str(binary), *extra,
               "--out-dir", str(out), "--version", version,
               "--target", target, "--repo-root", str(repo),
               "--git-sha", SELFTEST_GIT_SHA])
    return rc, out


def _consistent_bad_bundle(tmp, name, extra=None, drop=(), mutate=None, sort=True):
    """A tar+manifest+sums triple that is internally consistent but built from a
    member list `mutate`/`extra` shapes - used to prove verify rejects the
    *content*, not a checksum slip. Members are written in `entries` order."""
    entries = [("mc-spine", b"\x7fELF fake\n", 0o755),
               ("LICENSE", b"stub LICENSE\n", 0o644)]
    entries = [entry for entry in entries if entry[0] not in drop]
    entries += extra or []
    prefix = bundle_stem("0.1.0", "x86_64-unknown-linux-gnu")
    tar_path = Path(tmp) / name / f"{prefix}.tar.gz"
    tar_path.parent.mkdir(parents=True, exist_ok=True)
    raw = io.BytesIO()
    ordered = sorted(entries, key=lambda entry: entry[0]) if sort else entries
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for arcname, data, mode in ordered:
            info = tarfile.TarInfo(f"{prefix}/{arcname}")
            info.size = len(data)
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mode = mode
            tar.addfile(info, io.BytesIO(data))
    tar_path.write_bytes(gzip.compress(raw.getvalue(), compresslevel=9, mtime=0))
    tar_bytes = tar_path.read_bytes()
    manifest = manifest_body("0.1.0", "x86_64-unknown-linux-gnu", SELFTEST_GIT_SHA,
                             prefix, tar_path.name, tar_bytes, entries)
    if mutate:
        mutate(manifest)
    manifest_path = tar_path.parent / MANIFEST_NAME
    manifest_path.write_bytes(dump_json(manifest))
    sums_path = tar_path.parent / SUMS_NAME
    sums_path.write_text(sums_body({
        tar_path.name: sha256_hex(tar_bytes),
        MANIFEST_NAME: sha256_hex(manifest_path.read_bytes()),
    }))
    return tar_path, manifest_path, sums_path


def _symlink_bundle(tmp, name):
    """A consistent bundle whose extra member is a symlink, not a regular file."""
    prefix = bundle_stem("0.1.0", "x86_64-unknown-linux-gnu")
    tar_path = Path(tmp) / name / f"{prefix}.tar.gz"
    tar_path.parent.mkdir(parents=True, exist_ok=True)
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        for arcname, data, mode in sorted(
                [("LICENSE", b"stub\n", 0o644), ("mc-spine", b"\x7fELF\n", 0o755)]):
            info = tarfile.TarInfo(f"{prefix}/{arcname}")
            info.size = len(data)
            info.mtime = 0
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mode = mode
            tar.addfile(info, io.BytesIO(data))
        link = tarfile.TarInfo(f"{prefix}/tools/evil.sh")
        link.type = tarfile.SYMTYPE
        link.linkname = "/etc/passwd"
        link.mtime = 0
        tar.addfile(link, io.BytesIO())
    tar_path.write_bytes(gzip.compress(raw.getvalue(), compresslevel=9, mtime=0))
    tar_bytes = tar_path.read_bytes()
    members = []
    for arcname, data, mode in [("LICENSE", b"stub\n", 0o644), ("mc-spine", b"\x7fELF\n", 0o755)]:
        members.append({"path": f"{prefix}/{arcname}", "sha256": sha256_hex(data),
                        "bytes": len(data), "mode": f"{mode:04o}"})
    members.append({"path": f"{prefix}/tools/evil.sh",
                    "sha256": sha256_hex(b""), "bytes": 0, "mode": "0777"})
    manifest = {"format": FORMAT, "product": PRODUCT, "version": "0.1.0",
                "git_sha": SELFTEST_GIT_SHA,
                "target_triple": "x86_64-unknown-linux-gnu",
                "mc_targets": list(MC_TARGETS), "license": LICENSE_ID,
                "subcrate_license": SUBCRATE_LICENSE,
                "bundle": {"name": tar_path.name, "sha256": sha256_hex(tar_bytes),
                           "bytes": len(tar_bytes)},
                "files": members}
    manifest_path = tar_path.parent / MANIFEST_NAME
    manifest_path.write_bytes(dump_json(manifest))
    sums_path = tar_path.parent / SUMS_NAME
    sums_path.write_text(sums_body({
        tar_path.name: sha256_hex(tar_bytes),
        MANIFEST_NAME: sha256_hex(manifest_path.read_bytes()),
    }))
    return tar_path, manifest_path, sums_path


def self_test():
    checks = []

    def check(label, cond):
        checks.append((label, cond))
        return cond

    def expect_build_fail(label, *argv):
        rc = main(["build", *argv])
        return check(label, rc == 1)

    def expect_verify_fail(label, tar, manifest, sums):
        rc = main(["verify", "--bundle", str(tar), "--manifest", str(manifest),
                   "--sums", str(sums)])
        return check(label, rc == 1)

    with tempfile.TemporaryDirectory(prefix="release-bundle-selftest-") as tmp:
        repo = Path(tmp) / "repo"
        repo.mkdir()
        binary = _fixture_repo(repo)

        # Determinism: two builds over identical sources at different mtimes are
        # byte-identical for all three artifacts.
        rc, out_a = _build(tmp, "a", repo, binary)
        check("build: first build succeeds", rc == 0)
        past = 1_700_000_000
        for path in repo.rglob("*"):
            if path.is_file():
                os.utime(path, (past + len(path.name), past))
        rc, out_b = _build(tmp, "b", repo, binary)
        check("build: second build succeeds after mtime change", rc == 0)
        for name in (f"{bundle_stem('0.1.0', 'x86_64-unknown-linux-gnu')}.tar.gz",
                     MANIFEST_NAME, SUMS_NAME):
            check(f"deterministic: {name} is byte-identical",
                  (out_a / name).read_bytes() == (out_b / name).read_bytes())

        tar_path = out_a / f"{bundle_stem('0.1.0', 'x86_64-unknown-linux-gnu')}.tar.gz"
        manifest_path = out_a / MANIFEST_NAME
        sums_path = out_a / SUMS_NAME
        rc = main(["verify", "--bundle", str(tar_path), "--manifest", str(manifest_path),
                   "--sums", str(sums_path)])
        check("verify: a freshly built bundle passes", rc == 0)

        # Layout: the manifest records every expected member exactly once.
        manifest = load_json(manifest_path, MANIFEST_NAME)
        prefix = bundle_stem("0.1.0", "x86_64-unknown-linux-gnu")
        expected = {f"{prefix}/{name}"
                    for name in [PRODUCT, *REQUIRED_DOCS, *REQUIRED_TOOLS]}
        check("manifest: files cover binary + docs + tools",
              {record["path"] for record in manifest["files"]} == expected)
        check("manifest: pinned targets and licences",
              manifest["mc_targets"] == MC_TARGETS
              and manifest["license"] == LICENSE_ID
              and manifest["subcrate_license"] == SUBCRATE_LICENSE)
        modes = {record["path"]: record["mode"] for record in manifest["files"]}
        check("manifest: public megu helpers record mode 0755",
              all(modes[f"{prefix}/{name}"] == "0755"
                  for name in ("tools/megu_start.py", "tools/megu_translate.py",
                               "tools/megu_mods.py")))

        # Multi-binary suite: repeatable --extra-binary stages each companion
        # under its own basename with mode 0755, keeps the whole artifact set
        # deterministic, and verify passes on the result unchanged.
        suite_dir = Path(tmp) / "suite-bin"
        suite_dir.mkdir()
        suite_names = ("mc-spine-client", "mc-spine-mod-check",
                       "mc-spine-world-migrate", "mc-spine-init")
        suite_flags = []
        for suite_name in suite_names:
            suite_path = suite_dir / suite_name
            suite_path.write_bytes(f"\x7fELF fake {suite_name} v1\n".encode())
            suite_path.chmod(0o755)
            suite_flags += ["--extra-binary", str(suite_path)]
        rc, out_m1 = _build(tmp, "multi-a", repo, binary, extra=suite_flags)
        check("build: multi-binary suite build succeeds", rc == 0)
        rc, out_m2 = _build(tmp, "multi-b", repo, binary, extra=suite_flags)
        check("build: second multi-binary suite build succeeds", rc == 0)
        for name in (f"{prefix}.tar.gz", MANIFEST_NAME, SUMS_NAME):
            check(f"deterministic: multi-binary {name} is byte-identical",
                  (out_m1 / name).read_bytes() == (out_m2 / name).read_bytes())
        rc = main(["verify", "--bundle", str(out_m1 / f"{prefix}.tar.gz"),
                   "--manifest", str(out_m1 / MANIFEST_NAME),
                   "--sums", str(out_m1 / SUMS_NAME)])
        check("verify: a multi-binary suite bundle passes", rc == 0)
        manifest = load_json(out_m1 / MANIFEST_NAME, MANIFEST_NAME)
        expected_suite = {f"{prefix}/{name}" for name in
                          [PRODUCT, *suite_names, *REQUIRED_DOCS, *REQUIRED_TOOLS]}
        check("manifest: files cover every suite binary + docs + tools",
              {record["path"] for record in manifest["files"]} == expected_suite)
        modes = {record["path"]: record["mode"] for record in manifest["files"]}
        check("manifest: every suite binary records mode 0755",
              all(modes[f"{prefix}/{name}"] == "0755"
                  for name in (PRODUCT, *suite_names)))
        check("manifest: mod author guide records mode 0644",
              modes[f"{prefix}/docs/mod-author-guide.md"] == "0644")

        # The same fail-closed guard covers companion binaries: duplicate
        # basenames, artifact-name collisions, doc shadows, forbidden names and
        # missing files are refused before anything is written.
        shadow = Path(tmp) / "shadow" / PRODUCT
        shadow.parent.mkdir(parents=True, exist_ok=True)
        shadow.write_bytes(b"shadow binary\n")
        expect_build_fail("build: refuses an extra binary named like the primary",
                          "--binary", str(binary), "--extra-binary", str(shadow),
                          "--out-dir", str(Path(tmp) / "out-shadow"),
                          "--version", "0.1.0",
                          "--target", "x86_64-unknown-linux-gnu",
                          "--repo-root", str(repo), "--git-sha", SELFTEST_GIT_SHA)
        dup_a = Path(tmp) / "dup-a" / "companion"
        dup_b = Path(tmp) / "dup-b" / "companion"
        for dup in (dup_a, dup_b):
            dup.parent.mkdir(parents=True, exist_ok=True)
            dup.write_bytes(b"companion\n")
        expect_build_fail("build: refuses two extra binaries sharing a basename",
                          "--binary", str(binary),
                          "--extra-binary", str(dup_a),
                          "--extra-binary", str(dup_b),
                          "--out-dir", str(Path(tmp) / "out-dup"),
                          "--version", "0.1.0",
                          "--target", "x86_64-unknown-linux-gnu",
                          "--repo-root", str(repo), "--git-sha", SELFTEST_GIT_SHA)
        jar_extra = Path(tmp) / "evil-extra.jar"
        jar_extra.write_bytes(b"PK\x03\x04 planted")
        expect_build_fail("build: refuses a .jar extra binary",
                          "--binary", str(binary),
                          "--extra-binary", str(jar_extra),
                          "--out-dir", str(Path(tmp) / "out-jar"),
                          "--version", "0.1.0",
                          "--target", "x86_64-unknown-linux-gnu",
                          "--repo-root", str(repo), "--git-sha", SELFTEST_GIT_SHA)
        expect_build_fail("build: refuses a missing extra binary",
                          "--binary", str(binary),
                          "--extra-binary", str(Path(tmp) / "no-such-binary"),
                          "--out-dir", str(Path(tmp) / "out-missing"),
                          "--version", "0.1.0",
                          "--target", "x86_64-unknown-linux-gnu",
                          "--repo-root", str(repo), "--git-sha", SELFTEST_GIT_SHA)
        doc_shadow = Path(tmp) / "doc-shadow" / "LICENSE"
        doc_shadow.parent.mkdir(parents=True, exist_ok=True)
        doc_shadow.write_bytes(b"not a licence\n")
        expect_build_fail("build: refuses an extra binary shadowing LICENSE",
                          "--binary", str(binary),
                          "--extra-binary", str(doc_shadow),
                          "--out-dir", str(Path(tmp) / "out-docshadow"),
                          "--version", "0.1.0",
                          "--target", "x86_64-unknown-linux-gnu",
                          "--repo-root", str(repo), "--git-sha", SELFTEST_GIT_SHA)
        reserved = Path(tmp) / "reserved" / SUMS_NAME
        reserved.parent.mkdir(parents=True, exist_ok=True)
        reserved.write_bytes(b"not a sums file\n")
        expect_build_fail(f"build: refuses an extra binary named {SUMS_NAME}",
                          "--binary", str(binary),
                          "--extra-binary", str(reserved),
                          "--out-dir", str(Path(tmp) / "out-reserved"),
                          "--version", "0.1.0",
                          "--target", "x86_64-unknown-linux-gnu",
                          "--repo-root", str(repo), "--git-sha", SELFTEST_GIT_SHA)

        # build refuses forbidden basenames handed in through --binary, and a
        # staged tree that grows a forbidden file can never pass the guard.
        for bad in ("evil.jar", "config_packets.bin", "world_overlay.bin",
                    "world_overlay_nether.bin.bak", "pblocks.json"):
            planted = Path(tmp) / f"bin-{bad}" / bad
            planted.parent.mkdir(parents=True, exist_ok=True)
            planted.write_bytes(b"planted")
            expect_build_fail(f"build: refuses a binary named {bad!r}",
                              "--binary", str(planted), "--out-dir",
                              str(Path(tmp) / f"out-{bad}"), "--version", "0.1.0",
                              "--target", "x86_64-unknown-linux-gnu",
                              "--repo-root", str(repo), "--git-sha", SELFTEST_GIT_SHA)
        try:
            check_names(["mc-spine", "datapacks/vanilla/pack.mcmeta"], "staged tree")
            staged_fail = False
        except BundleError:
            staged_fail = True
        check("guard: staged-tree scan rejects datapacks/vanilla", staged_fail)
        try:
            check_names(["mc-spine", "x/../y"], "staged tree")
            staged_fail = False
        except BundleError:
            staged_fail = True
        check("guard: staged-tree scan rejects `..` components", staged_fail)

        # build refuses a repo missing a required document.
        broken = Path(tmp) / "repo-broken"
        broken.mkdir()
        _fixture_repo(broken)
        (broken / "NOTICE").unlink()
        expect_build_fail("build: refuses a repo root missing NOTICE",
                          "--binary", str(broken / "build" / PRODUCT),
                          "--out-dir", str(Path(tmp) / "out-broken"),
                          "--version", "0.1.0", "--target", "x86_64-unknown-linux-gnu",
                          "--repo-root", str(broken), "--git-sha", SELFTEST_GIT_SHA)

        # verify rejects internally consistent bundles whose *content* is
        # forbidden or malformed - digests all check out, purity/layout does not.
        for bad_member in ("config_packets.bin", "evil.jar", "world_overlay.bin",
                           "assets/block_states_1211.json", ".git/config",
                           "datapacks/vanilla/pack.mcmeta",
                           "tests/data/dimension_terrain_1211/0-end.bin.gz"):
            tar, man, sums = _consistent_bad_bundle(
                tmp, f"bad-{bad_member.replace('/', '_')}",
                extra=[(bad_member, b"planted", 0o644)])
            expect_verify_fail(f"verify: rejects member {bad_member!r}", tar, man, sums)

        tar, man, sums = _symlink_bundle(tmp, "bad-symlink")
        expect_verify_fail("verify: rejects a symlink member", tar, man, sums)

        tar, man, sums = _consistent_bad_bundle(tmp, "bad-order", sort=False)
        expect_verify_fail("verify: rejects unsorted members", tar, man, sums)

        tar, man, sums = _consistent_bad_bundle(
            tmp, "bad-extra", mutate=lambda m: m["files"].append(
                {"path": f"{bundle_stem('0.1.0', 'x86_64-unknown-linux-gnu')}/EXTRA",
                 "sha256": sha256_hex(b""), "bytes": 0, "mode": "0644"}))
        expect_verify_fail("verify: rejects a manifest file absent from the tar",
                           tar, man, sums)

        tar, man, sums = _consistent_bad_bundle(
            tmp, "bad-license", mutate=lambda m: m.__setitem__("license", "MIT"))
        expect_verify_fail("verify: rejects a licence change", tar, man, sums)

        tar, man, sums = _consistent_bad_bundle(
            tmp, "bad-targets", mutate=lambda m: m.__setitem__("mc_targets", ["1.20.4"]))
        expect_verify_fail("verify: rejects changed mc_targets", tar, man, sums)

        # Tampering with one real artifact is caught even when the other two are
        # left untouched or recomputed to agree.
        tampered = Path(tmp) / "tampered"
        tampered.mkdir()
        for source in (tar_path, manifest_path, sums_path):
            (tampered / source.name).write_bytes(source.read_bytes())
        sums_t = tampered / SUMS_NAME
        sums_t.write_text(sums_t.read_text().replace("0", "1", 1))
        expect_verify_fail("verify: rejects a tampered SHA256SUMS",
                           tampered / tar_path.name, tampered / MANIFEST_NAME, sums_t)

        tampered2 = Path(tmp) / "tampered2"
        tampered2.mkdir()
        for source in (tar_path, manifest_path):
            (tampered2 / source.name).write_bytes(source.read_bytes())
        man_t = tampered2 / MANIFEST_NAME
        body = json.loads(man_t.read_text())
        body["files"][0]["sha256"] = "0" * 64
        man_t.write_bytes(dump_json(body))
        (tampered2 / SUMS_NAME).write_text(sums_body({
            tar_path.name: sha256_hex((tampered2 / tar_path.name).read_bytes()),
            MANIFEST_NAME: sha256_hex(man_t.read_bytes()),
        }))
        expect_verify_fail("verify: rejects a tampered member digest",
                           tampered2 / tar_path.name, man_t, tampered2 / SUMS_NAME)

        tampered3 = Path(tmp) / "tampered3"
        tampered3.mkdir()
        (tampered3 / tar_path.name).write_bytes(tar_path.read_bytes())
        raw = bytearray((tampered3 / tar_path.name).read_bytes())
        raw[len(raw) // 2] ^= 0xFF  # inside the deflate stream: member data changes
        (tampered3 / tar_path.name).write_bytes(bytes(raw))
        (tampered3 / MANIFEST_NAME).write_bytes(manifest_path.read_bytes())
        (tampered3 / SUMS_NAME).write_text(sums_body({
            tar_path.name: sha256_hex((tampered3 / tar_path.name).read_bytes()),
            MANIFEST_NAME: sha256_hex(manifest_path.read_bytes()),
        }))
        expect_verify_fail("verify: rejects a tampered tarball byte",
                           tampered3 / tar_path.name, tampered3 / MANIFEST_NAME,
                           tampered3 / SUMS_NAME)

        # The purity unit itself is also exercised directly.
        check("guard: unit check accepts the shipped layout",
              all(forbidden_reason(f"{bundle_stem('0.1.0', 'x86_64-unknown-linux-gnu')}/{name}")
                  is None for name in [PRODUCT, *REQUIRED_DOCS, *REQUIRED_TOOLS]))

    failed = [label for label, ok in checks if not ok]
    for label, ok in checks:
        print(f"  {'ok  ' if ok else 'FAIL'} {label}")
    if failed:
        print(f"self-test: {len(checks) - len(failed)}/{len(checks)} passed, "
              f"{len(failed)} FAILED: {failed}")
        return 1
    print(f"self-test: all {len(checks)} checks passed")
    return 0


# ---- cli ------------------------------------------------------------------------

def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="release_bundle.py",
        description="Build and verify deterministic, fail-closed mc-spine "
                    "release bundles.")
    parser.add_argument("--self-test", action="store_true",
                        help="run the offline self-test suite and exit")
    sub = parser.add_subparsers(dest="command")

    p_build = sub.add_parser("build", help="stage, package and self-verify a bundle")
    p_build.add_argument("--binary", required=True, help="release binary path")
    p_build.add_argument("--extra-binary", action="append", default=None,
                         metavar="PATH",
                         help="companion binary shipped alongside --binary "
                              "(repeatable, staged under its own basename)")
    p_build.add_argument("--out-dir", required=True, help="artifact output directory")
    p_build.add_argument("--version", required=True, help="release version, e.g. 0.1.0")
    p_build.add_argument("--target", required=True,
                         help="target triple, e.g. x86_64-unknown-linux-gnu")
    p_build.add_argument("--repo-root", default=None,
                         help="tree to take the docs/tools from (default: this repo)")
    p_build.add_argument("--git-sha", default=None,
                         help="source sha to record (default: git rev-parse HEAD)")

    p_verify = sub.add_parser("verify", help="fail-closed bundle verification")
    p_verify.add_argument("--bundle", required=True, help="the .tar.gz artifact")
    p_verify.add_argument("--manifest", required=True, help=MANIFEST_NAME)
    p_verify.add_argument("--sums", required=True, help=SUMS_NAME)

    sub.add_parser("self-test", help="run the offline self-test suite")

    args = parser.parse_args(argv)
    try:
        if args.self_test or args.command == "self-test":
            return self_test()
        if args.command == "build":
            return cmd_build(args)
        if args.command == "verify":
            return cmd_verify(args)
        parser.print_help()
        return 2
    except BundleError as error:
        print(f"release_bundle: error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Cross-platform vanilla-data generator: jar -> datapack + reports + state index.

  gen_vanilla_data.py --jar <server.jar> [--out-dir DIR]
  gen_vanilla_data.py --verify [--out-dir DIR] [--jar <server.jar>]
  gen_vanilla_data.py --self-test

The script uses only the Python 3 standard library plus the `java` executable;
it does not need Bash, `unzip`, `javac`, curl, or a POSIX shell, so the same
file runs on Windows, macOS and Linux.

What a normal run does:

1. Opens `--jar` with `zipfile`. For an official bundler jar it selects the
   bundled `META-INF/versions/*/server-*.jar`; for an already-unpacked server
   jar it reads `data/minecraft/` directly. Every member path is validated
   before a single byte is written: absolute paths, `..`, drive letters,
   backslashes, duplicate names and case-fold collisions fail closed.
2. Extracts `data/minecraft/**` into `<out-dir>/datapacks/vanilla/` and writes
   a deterministic `pack.mcmeta`.
3. Runs the vanilla data generator as a subprocess with `shell=False`:

       java -DbundlerMainClass=net.minecraft.data.Main -jar <server.jar> \
           --all --output <tmpdir>

   and installs the resulting `reports/` tree into `<out-dir>/reports/`.
4. Builds `<out-dir>/assets/block_states_1211.json` from `reports/blocks.json`.
   `--reports` output carries block/state ids but not the physical-facts
   columns (`flags`, `luminance`, collision shapes) that
   `gen_vanilla_data.sh` obtains from the obfuscated jar with Java probes.
   This script therefore emits a deterministic report-derived state index.
   If an existing canonical `assets/block_states_1211.json` is present, it is
   kept only when its state runs exactly match the reports; a mismatch fails
   closed rather than overwriting good data with a lesser file.
5. Writes `vanilla_data_manifest.json` with SHA-1/SHA-256 provenance for the
   jar, every extracted member, every report and both outputs. `--verify`
   re-hashes that tree without needing Java.

`--self-test` builds synthetic bundler/direct jars in a temporary directory,
proves traversal rejection, extraction, report installation, state-index
creation and verification - no Mojang jar or Java runtime required.

Exit status: 0 ok, 1 validation/runtime failure, 2 usage error.
"""

import argparse
import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

try:
    from version_pack import PackError, block_states_body, parse_blocks_report
except ImportError:  # pragma: no cover - only when imported as a module elsewhere
    PackError = ValueError
    block_states_body = None
    parse_blocks_report = None

TOOL = "tools/vanilla_data/gen_vanilla_data.py"
MANIFEST_NAME = "vanilla_data_manifest.json"
PACK_FORMAT = 48
DATA_PREFIX = "data/minecraft/"
REQUIRED_REPORTS = ("blocks.json", "registries.json", "packets.json")
ASSET_REL = "assets/block_states_1211.json"
PACK_REL = "datapacks/vanilla"
INTERMEDIARY_JAR = "intermediary-1.21.1-v2.jar"
INTERMEDIARY_URL = "https://maven.fabricmc.net/net/fabricmc/intermediary/1.21.1/" + INTERMEDIARY_JAR
INTERMEDIARY_SHA256 = "e8bd6a4c39e235ee2dc73063558610775c72a2572fa0c6b32915b149074012a6"
YARN_JAR = "yarn-1.21.1+build.3-v2.jar"
YARN_URL = "https://maven.fabricmc.net/net/fabricmc/yarn/1.21.1+build.3/" + YARN_JAR
YARN_SHA256 = "4ec570889585cb958b7d3c9dd94ce4064b16ab797dffec152bc8486c59a4010b"
PHYSICAL_ASSET_KEYS = (
    "blocks", "flags", "luminance", "light_block", "full_sides", "shapes", "collision_shapes"
)
DATAGEN_TIMEOUT_SECONDS = 600
RUNTIME_DATA_PREFIXES = (
    "dimension_type/",
    "structure/",
    "tags/block/",
    "tags/worldgen/",
    "worldgen/",
)


class VanillaDataError(Exception):
    """A fail-closed input, extraction or verification error."""


def fail(message):
    raise VanillaDataError(message)


def sha1_file(path):
    digest = hashlib.sha1()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_atomic(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=".vanilla-data-", dir=path.parent)
    tmp = Path(tmp_name)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
        os.replace(tmp, path)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


def write_json_atomic(path, obj):
    write_atomic(path, (json.dumps(obj, indent=1, sort_keys=True) + "\n").encode("utf-8"))


def posix_rel(path):
    return Path(path).as_posix()


def safe_data_member(name):
    """Return the `data/minecraft/`-relative path, or fail closed."""
    if not name.startswith(DATA_PREFIX):
        fail(f"member outside {DATA_PREFIX} was selected: {name}")
    rel = name[len(DATA_PREFIX):]
    if not rel or rel.endswith("/") or "//" in rel or "\\" in name or ":" in name:
        fail(f"unsafe vanilla data member path: {name}")
    parts = PurePosixPath(rel).parts
    if PurePosixPath(rel).is_absolute() or any(part in ("", ".", "..") for part in parts):
        fail(f"unsafe vanilla data member path: {name}")
    return rel


def data_members(archive):
    members = []
    for info in archive.infolist():
        if info.filename.startswith(DATA_PREFIX) and not info.is_dir():
            members.append(info)
    return members


def bundled_inner_name(outer):
    candidates = []
    for name in outer.namelist():
        parts = PurePosixPath(name).parts
        if (
            len(parts) == 4
            and parts[0] == "META-INF"
            and parts[1] == "versions"
            and parts[2]
            and parts[3].startswith("server-")
            and parts[3].endswith(".jar")
        ):
            candidates.append(name)
    return sorted(candidates)[0] if candidates else None


def extract_datapack(jar_path, out_dir, canonical_runtime=False):
    """Extract `data/minecraft/**` into `<out-dir>/datapacks/vanilla`.

    Returns the provenance record used by the manifest. Validation of every
    selected member happens before the datapack tree is touched, so a hostile
    or malformed jar cannot leave a partial extraction behind.
    """
    jar_path = Path(jar_path)
    if not jar_path.is_file():
        fail(f"--jar does not name a file: {jar_path}")
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    outer_sha1 = sha1_file(jar_path)
    with zipfile.ZipFile(jar_path) as outer:
        inner_name = bundled_inner_name(outer)
        if inner_name is not None:
            inner_bytes = outer.read(inner_name)
            inner_sha256 = sha256_bytes(inner_bytes)
            source = zipfile.ZipFile(io.BytesIO(inner_bytes))
            source_kind = "bundled-inner-jar"
        else:
            inner_bytes = None
            inner_sha256 = None
            source = outer
            source_kind = "direct-jar"
        try:
            infos = data_members(source)
            if not infos:
                fail(f"{jar_path} contains no {DATA_PREFIX} members")
            rels = []
            seen = set()
            for info in infos:
                rel = safe_data_member(info.filename)
                if canonical_runtime and not rel.startswith(RUNTIME_DATA_PREFIXES):
                    continue
                folded = rel.casefold()
                if rel in seen or folded in seen:
                    fail(f"duplicate or case-colliding vanilla data member: {info.filename}")
                seen.add(rel)
                seen.add(folded)
                rels.append((info, rel))

            required = sum(info.file_size for info, _ in rels) + 16 * 1024 * 1024
            free = shutil.disk_usage(out_dir).free
            if free < required:
                fail(
                    f"not enough free space under {out_dir}: need {required} bytes, "
                    f"have {free}; choose a larger TMPDIR/output filesystem"
                )

            pack_root = out_dir / PACK_REL
            data_root = pack_root / "data" / "minecraft"
            if data_root.exists():
                shutil.rmtree(data_root)
            members = {}
            for info, rel in rels:
                member = DATA_PREFIX + rel
                target = pack_root.joinpath(*PurePosixPath(member).parts)
                data = source.read(info)
                digest = sha256_bytes(data)
                write_atomic(target, data)
                members[member] = digest

            meta = {
                "pack": {
                    "pack_format": PACK_FORMAT,
                    "description": "vanilla data generated by tools/vanilla_data/gen_vanilla_data.py",
                }
            }
            write_json_atomic(pack_root / "pack.mcmeta", meta)
        finally:
            if inner_bytes is not None:
                source.close()

    return {
        "jar": {
            "sha1": outer_sha1,
            "source": source_kind,
            "inner_member": inner_name,
            "inner_sha256": inner_sha256,
        },
        "members": members,
        "pack_mcmeta_sha256": sha256_file(pack_root / "pack.mcmeta"),
    }


def datagen_command(jar_path, output_dir):
    return [
        "java",
        "-DbundlerMainClass=net.minecraft.data.Main",
        "-jar",
        str(jar_path),
        "--all",
        "--output",
        str(output_dir),
    ]


def run_datagen(jar_path, work_dir, runner=None):
    """Run the vanilla `--all` data generator and return its reports directory."""
    work_dir = Path(work_dir)
    run_dir = work_dir / "run"
    out_dir = work_dir / "generated"
    run_dir.mkdir(parents=True, exist_ok=True)
    out_dir.mkdir(parents=True, exist_ok=True)
    if runner is None:
        if shutil.which("java") is None:
            fail("java was not found on PATH; only a Java runtime is required")
        runner = subprocess.run
    command = datagen_command(jar_path, out_dir)
    try:
        result = runner(
            command,
            cwd=run_dir,
            capture_output=True,
            text=True,
            timeout=DATAGEN_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        fail(f"vanilla data generator timed out after {DATAGEN_TIMEOUT_SECONDS}s")
    returncode = getattr(result, "returncode", 0)
    if returncode != 0:
        stderr = getattr(result, "stderr", "") or ""
        stdout = getattr(result, "stdout", "") or ""
        tail = (stderr or stdout).strip().splitlines()[-20:]
        fail("vanilla data generator failed:\n" + "\n".join(tail))

    candidates = [out_dir / "reports", out_dir / "gen" / "reports"]
    reports = next((p for p in candidates if p.is_dir()), None)
    if reports is None:
        reports = next((p for p in sorted(out_dir.rglob("reports")) if p.is_dir()), None)
    if reports is None:
        fail(f"data generator produced no reports directory under {out_dir}")
    for name in REQUIRED_REPORTS:
        if not (reports / name).is_file():
            fail(f"data generator reports are missing {name} under {reports}")
    return reports


def install_reports(reports_src, out_dir):
    reports_src = Path(reports_src)
    out_dir = Path(out_dir)
    dest = out_dir / "reports"
    if dest.exists():
        shutil.rmtree(dest)
    shutil.copytree(reports_src, dest)
    hashes = {}
    for path in sorted(p for p in dest.rglob("*") if p.is_file()):
        hashes[posix_rel(path.relative_to(dest))] = sha256_file(path)
    return hashes


def report_state_runs(blocks_report):
    runs = []
    for name, record in blocks_report.items():
        first = record["first_state_id"]
        count = record["state_count"]
        runs.append((name, first, count))
    runs.sort(key=lambda row: row[1])
    return runs


def canonical_runs(asset):
    blocks = asset.get("blocks")
    if not isinstance(blocks, list):
        return None
    runs = []
    for row in blocks:
        if not (isinstance(row, list) and len(row) == 3):
            return None
        runs.append((row[0], row[1], row[2]))
    return runs


def is_report_state_asset(asset):
    return isinstance(asset.get("block_index"), dict) and "source_reports" in asset


def build_block_states_asset(reports_dir, out_dir, jar_sha1):
    """Write the deterministic report-derived `assets/block_states_1211.json`."""
    if block_states_body is None or parse_blocks_report is None:
        fail("version_pack.py helpers are unavailable")
    reports_dir = Path(reports_dir)
    out_dir = Path(out_dir)
    blocks_path = reports_dir / "blocks.json"
    try:
        report = json.loads(blocks_path.read_text(encoding="utf-8"))
        blocks = parse_blocks_report(report)
    except (OSError, json.JSONDecodeError, PackError, ValueError) as error:
        fail(f"cannot parse {blocks_path}: {error}")
    body = block_states_body(blocks)
    runs = report_state_runs(blocks)

    asset_path = out_dir / ASSET_REL
    if asset_path.exists():
        try:
            existing = json.loads(asset_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            fail(f"existing {ASSET_REL} is not valid JSON: {error}")
        existing_runs = canonical_runs(existing)
        if existing_runs is not None:
            if existing_runs != [(name, first, count) for name, first, count in runs]:
                fail(
                    f"existing {ASSET_REL} state runs do not match reports/blocks.json; "
                    "refusing to overwrite the canonical physical-facts asset"
                )
            return {"action": "kept-existing", "sha256": sha256_file(asset_path)}
        if not is_report_state_asset(existing):
            fail(f"existing {ASSET_REL} has an unknown schema; refusing to overwrite it")

    asset = {
        "format": 1,
        "minecraft": "1.21.1",
        "jar_sha1": jar_sha1,
        "generated_by": TOOL,
        "about": (
            "Report-derived block-state index from the vanilla --all data "
            "generator. It carries ids/defaults/state runs, not the obfuscated-"
            "jar physical-facts columns produced by gen_vanilla_data.sh."
        ),
        "source_reports": {"blocks.json": sha256_file(blocks_path)},
        "block_states_total": body["block_states_total"],
        "blocks": [[name, first, count] for name, first, count in runs],
        "block_index": body["blocks"],
    }
    write_json_atomic(asset_path, asset)
    return {"action": "written", "sha256": sha256_file(asset_path)}


def write_manifest(out_dir, jar_info, members, report_hashes, asset_info):
    out_dir = Path(out_dir)
    manifest = {
        "format": 1,
        "tool": TOOL,
        "minecraft": "1.21.1",
        "pack_format": PACK_FORMAT,
        "jar": jar_info,
        "datapack_members": members,
        "pack_mcmeta_sha256": jar_info.pop("pack_mcmeta_sha256", None),
        "reports": report_hashes,
        "outputs": {
            ASSET_REL: asset_info["sha256"],
            f"{PACK_REL}/pack.mcmeta": sha256_file(out_dir / PACK_REL / "pack.mcmeta"),
        },
    }
    # Keep the jar record immutable for callers; pack_mcmeta lives under outputs.
    manifest["jar"] = jar_info
    write_json_atomic(out_dir / MANIFEST_NAME, manifest)
    return manifest


def physical_asset_schema_ok(asset):
    """True only for the canonical physical-facts asset, never the report-only index."""
    blocks = asset.get("blocks")
    if not (isinstance(blocks, list) and blocks):
        return False
    if not all(
        isinstance(row, list) and len(row) == 3 and isinstance(row[0], str)
        and isinstance(row[1], (int, float)) and isinstance(row[2], (int, float))
        for row in blocks
    ):
        return False
    columns = []
    for key in ("flags", "luminance", "light_block", "full_sides"):
        column = asset.get(key)
        if not (isinstance(column, list) and column
                and all(isinstance(value, (int, float)) for value in column)):
            return False
        columns.append(column)
    if len({len(column) for column in columns}) != 1:
        return False
    state_count = len(columns[0])
    shapes = asset.get("shapes")
    if not (isinstance(shapes, list) and shapes and all(
        isinstance(shape, dict)
        and isinstance(shape.get("min"), list) and len(shape["min"]) == 3
        and isinstance(shape.get("max"), list) and len(shape["max"]) == 3
        for shape in shapes
    )):
        return False
    collision = asset.get("collision_shapes")
    return (isinstance(collision, list) and len(collision) == state_count and all(
        isinstance(entry, list) and all(isinstance(index, (int, float)) for index in entry)
        for entry in collision
    ))


def verify_output(out_dir, jar_path=None, require_physical=False):
    out_dir = Path(out_dir)
    manifest_path = out_dir / MANIFEST_NAME
    if not manifest_path.is_file():
        fail(f"missing {MANIFEST_NAME}; run generation first")
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"{MANIFEST_NAME} is not valid JSON: {error}")
    if manifest.get("format") != 1 or manifest.get("tool") != TOOL:
        fail(f"{MANIFEST_NAME} has an unexpected format/tool")
    jar = manifest.get("jar") or {}
    if jar_path is not None:
        got = sha1_file(Path(jar_path))
        if got != jar.get("sha1"):
            fail(f"--jar sha1 {got} does not match manifest {jar.get('sha1')}")

    members = manifest.get("datapack_members")
    if not isinstance(members, dict) or not members:
        fail("manifest has no datapack member hashes")
    data_root = out_dir / PACK_REL / "data" / "minecraft"
    if not data_root.is_dir():
        fail(f"missing extracted datapack tree {data_root}")
    expected = set()
    for rel, digest in sorted(members.items()):
        member_path = PurePosixPath(rel)
        if member_path.is_absolute() or ".." in member_path.parts:
            fail(f"manifest member path is unsafe: {rel}")
        target = out_dir / PACK_REL / member_path
        expected.add(target)
        if not target.is_file():
            fail(f"manifest member missing: {rel}")
        got = sha256_file(target)
        if got != digest:
            fail(f"manifest member hash mismatch: {rel}")
    installed = {p for p in data_root.rglob("*") if p.is_file()}
    extra = sorted(p.relative_to(out_dir / PACK_REL).as_posix() for p in installed - expected)
    missing = sorted(p.relative_to(out_dir / PACK_REL).as_posix() for p in expected - installed)
    if extra or missing:
        fail(f"datapack membership drift: extra={extra[:3]} missing={missing[:3]}")

    pack_meta = out_dir / PACK_REL / "pack.mcmeta"
    if not pack_meta.is_file():
        fail("missing datapacks/vanilla/pack.mcmeta")
    try:
        meta = json.loads(pack_meta.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        fail(f"pack.mcmeta is not valid JSON: {error}")
    if meta.get("pack", {}).get("pack_format") != PACK_FORMAT:
        fail("pack.mcmeta does not carry the expected pack_format")

    reports = manifest.get("reports")
    if not isinstance(reports, dict) or not reports:
        fail("manifest has no report hashes")
    reports_root = out_dir / "reports"
    report_expected = set()
    for rel, digest in sorted(reports.items()):
        member_path = PurePosixPath(rel)
        if member_path.is_absolute() or ".." in member_path.parts:
            fail(f"manifest report path is unsafe: {rel}")
        target = reports_root / member_path
        report_expected.add(target)
        if not target.is_file():
            fail(f"manifest report missing: {rel}")
        got = sha256_file(target)
        if got != digest:
            fail(f"manifest report hash mismatch: {rel}")
    report_installed = {p for p in reports_root.rglob("*") if p.is_file()}
    if report_installed != report_expected:
        fail("reports/ membership does not match the manifest")

    outputs = manifest.get("outputs") or {}
    asset_rel = outputs.get(ASSET_REL)
    if not asset_rel:
        fail("manifest has no block-state asset hash")
    asset_path = out_dir / ASSET_REL
    if not asset_path.is_file():
        fail(f"missing {ASSET_REL}")
    if sha256_file(asset_path) != asset_rel:
        fail(f"{ASSET_REL} hash does not match the manifest")
    try:
        asset = json.loads(asset_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        fail(f"{ASSET_REL} is not valid JSON: {error}")
    if "blocks" not in asset:
        fail(f"{ASSET_REL} has no block table")
    if require_physical and not physical_asset_schema_ok(asset):
        fail(f"{ASSET_REL} lacks the canonical physical-facts columns")

    return {
        "members": len(members),
        "reports": len(reports),
        "asset_bytes": asset_path.stat().st_size,
    }


def sha256_path(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def pinned_download(url, dest, expected_sha256):
    dest = Path(dest)
    part = dest.with_name(dest.name + ".part")
    try:
        with urllib.request.urlopen(url, timeout=30) as response, open(part, "wb") as out:
            shutil.copyfileobj(response, out, 1024 * 256)
        got = sha256_path(part)
        if got != expected_sha256:
            part.unlink(missing_ok=True)
            fail(f"{dest.name} sha256 is {got}, expected pinned {expected_sha256}")
        if dest.exists():
            part.unlink(missing_ok=True)
            fail(f"refusing to replace existing mapping jar {dest}")
        os.replace(part, dest)
    finally:
        if part.exists() and dest.exists():
            part.unlink(missing_ok=True)


def ensure_mapping_jars(mappings_dir, fetch_mappings):
    mappings_dir = Path(mappings_dir)
    mappings_dir.mkdir(parents=True, exist_ok=True)
    result = {}
    for filename, url, digest in (
        (INTERMEDIARY_JAR, INTERMEDIARY_URL, INTERMEDIARY_SHA256),
        (YARN_JAR, YARN_URL, YARN_SHA256),
    ):
        path = mappings_dir / filename
        if not path.is_file():
            if not fetch_mappings:
                fail(
                    f"missing {path}; pass --fetch-mappings or place the pinned Fabric CC0 mapping jar there"
                )
            pinned_download(url, path, digest)
        got = sha256_path(path)
        if got != digest:
            fail(f"{path} sha256 is {got}, expected pinned {digest}")
        result[filename] = path
    return result[INTERMEDIARY_JAR], result[YARN_JAR]


def extract_tiny_mapping(jar_path, out_path):
    out_path = Path(out_path)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        with zipfile.ZipFile(jar_path) as archive:
            data = archive.read("mappings/mappings.tiny")
    except (KeyError, zipfile.BadZipFile) as error:
        fail(f"{jar_path} does not contain mappings/mappings.tiny: {error}")
    out_path.write_bytes(data)
    return out_path


def run_checked(command, cwd=None, timeout=240, label="command"):
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if result.returncode != 0:
        tail = ((result.stderr or result.stdout or "").strip().splitlines())[-20:]
        fail(label + " failed:\n" + "\n".join(tail))
    return result


def runtime_tool(name):
    """Locate a generator helper in either the repository or installer-embedded layout."""
    here = Path(__file__).resolve().parent
    for candidate in (here / name, here.parent / name):
        if candidate.is_file():
            return candidate
    fail(f"required runtime-data helper is missing: {name}")


def prune_to_canonical_runtime_pack(pack_root):
    """Restrict a freshly generated staging pack to the canonical server runtime inventory."""
    data = Path(pack_root) / "data" / "minecraft"
    if not data.is_dir():
        fail(f"generated datapack is missing {data}")
    for child in list(data.iterdir()):
        if child.name not in {"dimension_type", "structure", "tags", "worldgen"}:
            if child.is_dir():
                shutil.rmtree(child)
            else:
                child.unlink()
    tags = data / "tags"
    if tags.is_dir():
        for child in list(tags.iterdir()):
            if child.name not in {"block", "worldgen"}:
                if child.is_dir():
                    shutil.rmtree(child)
                else:
                    child.unlink()


def complete_runtime_pack(jar_path, out_dir):
    """Add jar-member provenance and the climate tree, then verify the canonical inventory."""
    pack = Path(out_dir) / PACK_REL
    prune_to_canonical_runtime_pack(pack)
    jar_string = str(jar_path)
    run_checked(
        [sys.executable, str(runtime_tool("vanilla_structure_assets.py")),
         "--jar", jar_string, "--output", str(pack), "--write"],
        timeout=120,
        label="vanilla structure asset extraction",
    )
    run_checked(
        [sys.executable, str(runtime_tool("biome_tree_from_jar.py")),
         "--jar", jar_string, "--output", str(pack / "multi_noise_biome_tree.json")],
        timeout=300,
        label="vanilla biome-tree extraction",
    )
    run_checked(
        [sys.executable, str(runtime_tool("verify_pack.py")),
         "--pack", str(pack), "--jar-sha1", sha1_file(jar_path)],
        timeout=120,
        label="canonical vanilla datapack verification",
    )


def generate_physical_block_states(jar_path, work_dir, reports_dir, out_dir, mappings_dir, fetch_mappings):
    """Use Extract.java + the pinned CC0 mappings to replace the report-only state index."""
    tool_dir = Path(__file__).resolve().parent
    extract_java = tool_dir / "Extract.java"
    build_py = tool_dir / "build_vanilla_data.py"
    if not extract_java.is_file() or not build_py.is_file():
        fail("Extract.java and build_vanilla_data.py must be beside gen_vanilla_data.py for --full-runtime")
    if shutil.which("java") is None:
        fail("java was not found on PATH; physical block-state extraction requires Java 21")
    inter_jar, yarn_jar = ensure_mapping_jars(mappings_dir, fetch_mappings)
    map_dir = Path(work_dir) / "mappings"
    inter_tiny = extract_tiny_mapping(inter_jar, map_dir / "intermediary.tiny")
    yarn_tiny = extract_tiny_mapping(yarn_jar, map_dir / "yarn.tiny")

    run_dir = Path(work_dir) / "run"
    nested = run_dir / "versions/1.21.1/server-1.21.1.jar"
    if not nested.is_file():
        fail(f"the bundler did not unpack {nested}")
    libraries = sorted((run_dir / "libraries").rglob("*.jar"))
    if not libraries:
        fail("the bundler did not unpack its server libraries")
    extract_out = Path(work_dir) / "extract"
    extract_out.mkdir(parents=True, exist_ok=True)
    run_checked(
        [
            "java",
            str(extract_java),
            str(extract_out),
            str(inter_tiny),
            str(yarn_tiny),
            str(nested),
            *[str(path) for path in libraries],
        ],
        timeout=240,
        label="vanilla physical-facts extraction",
    )
    extraction_sha1 = sha1_file(jar_path)
    run_checked(
        [sys.executable, str(build_py), str(extract_out), str(reports_dir), str(out_dir), extraction_sha1],
        timeout=120,
        label="canonical block-state assembly",
    )
    asset_path = Path(out_dir) / ASSET_REL
    return {"action": "written-full", "sha256": sha256_file(asset_path)}


def finish_generation(jar_path, out_dir, extraction, reports_src):
    out_dir = Path(out_dir)
    report_hashes = install_reports(reports_src, out_dir)
    jar_sha1 = extraction["jar"]["sha1"]
    asset_info = build_block_states_asset(out_dir / "reports", out_dir, jar_sha1)
    manifest = write_manifest(
        out_dir,
        dict(extraction["jar"], pack_mcmeta_sha256=extraction["pack_mcmeta_sha256"]),
        extraction["members"],
        report_hashes,
        asset_info,
    )
    summary = verify_output(out_dir, jar_path=jar_path)
    return {"manifest": manifest, "summary": summary, "asset": asset_info}


def generate(jar_path, out_dir, full_runtime=False, mappings_dir=None, fetch_mappings=False):
    jar_path = Path(jar_path)
    out_dir = Path(out_dir)
    extraction = extract_datapack(jar_path, out_dir, canonical_runtime=full_runtime)
    with tempfile.TemporaryDirectory(prefix="mc-vanilla-data-") as tmp:
        work_dir = Path(tmp)
        reports_src = run_datagen(jar_path, work_dir)
        result = finish_generation(jar_path, out_dir, extraction, reports_src)
        if full_runtime:
            if mappings_dir is None:
                fail("--full-runtime requires --mappings-dir")
            complete_runtime_pack(jar_path, out_dir)
            reports_installed = out_dir / "reports"
            asset_info = generate_physical_block_states(
                jar_path, work_dir, reports_installed, out_dir, mappings_dir, fetch_mappings
            )
            manifest = write_manifest(
                out_dir,
                dict(extraction["jar"], pack_mcmeta_sha256=extraction["pack_mcmeta_sha256"]),
                extraction["members"],
                result["manifest"]["reports"],
                asset_info,
            )
            result["asset"] = asset_info
            result["manifest"] = manifest
            result["summary"] = verify_output(out_dir, jar_path=jar_path, require_physical=True)
        return result


# ---- --self-test ------------------------------------------------------------

def make_test_jar(path, inner_files):
    inner = io.BytesIO()
    with zipfile.ZipFile(inner, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in sorted(inner_files.items()):
            archive.writestr(name, data)
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("META-INF/MANIFEST.MF", "Manifest-Version: 1.0\n")
        archive.writestr("META-INF/versions/1.21.1/server-1.21.1.jar", inner.getvalue())
    return path


def mock_reports(path):
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    (path / "blocks.json").write_text(json.dumps({
        "minecraft:air": {"states": [{"id": 0, "default": True, "properties": {}}]},
        "minecraft:stone": {"states": [{"id": 1, "default": True, "properties": {}}]},
    }), encoding="utf-8")
    (path / "registries.json").write_text("{}\n", encoding="utf-8")
    (path / "packets.json").write_text("{}\n", encoding="utf-8")
    return path


def fake_datagen_runner(command, cwd, capture_output, text, timeout):
    output = Path(command[command.index("--output") + 1])
    reports = output / "reports"
    reports.mkdir(parents=True, exist_ok=True)
    (reports / "blocks.json").write_text(json.dumps({
        "minecraft:selftest_block": {"states": [{"id": 0, "default": True, "properties": {}}]},
    }), encoding="utf-8")
    (reports / "registries.json").write_text("{}\n", encoding="utf-8")
    (reports / "packets.json").write_text("{}\n", encoding="utf-8")
    return subprocess.CompletedProcess(command, 0, stdout="", stderr="")


def self_test():
    with tempfile.TemporaryDirectory(prefix="gen-vanilla-data-selftest-") as tmp:
        root = Path(tmp)
        clean_jar = make_test_jar(root / "clean.jar", {
            "data/minecraft/worldgen/biome/selftest.json": b'{"count":1}\n',
            "data/minecraft/tags/block/selftest.json": b'{"values":[]}\n',
            "version.json": b'{"id":"1.21.1"}\n',
        })
        evil_jar = make_test_jar(root / "evil.jar", {
            "data/minecraft/../evil.json": b"escaped\n",
            "data/minecraft/worldgen/biome/ok.json": b"{}\n",
        })
        direct_jar = root / "direct.jar"
        with zipfile.ZipFile(direct_jar, "w") as archive:
            archive.writestr("data/minecraft/direct.json", b'{"direct":true}\n')

        # Traversal fails before any datapack file is written.
        evil_out = root / "evil-out"
        try:
            extract_datapack(evil_jar, evil_out)
        except VanillaDataError:
            pass
        else:
            fail("self-test: path traversal member was not rejected")
        if (evil_out / "datapacks").exists():
            fail("self-test: rejected jar still wrote a datapack")

        # Bundled-inner and direct-jar sources both extract the same prefix.
        final_out = root / "final-out"
        bundled = extract_datapack(clean_jar, final_out)
        if bundled["jar"]["source"] != "bundled-inner-jar":
            fail("self-test: bundler inner jar was not selected")
        direct = extract_datapack(direct_jar, root / "direct-out")
        if direct["jar"]["source"] != "direct-jar":
            fail("self-test: direct jar source was not selected")
        got = (final_out / "datapacks/vanilla/data/minecraft/worldgen/biome/selftest.json").read_bytes()
        if got != b'{"count":1}\n':
            fail("self-test: extracted member bytes differ")
        if not (root / "direct-out/datapacks/vanilla/data/minecraft/direct.json").is_file():
            fail("self-test: direct jar member was not extracted")

        # The datagen runner is argv-only (shell=False) and locates reports.
        work = root / "datagen-work"
        reports_src = run_datagen(clean_jar, work, runner=fake_datagen_runner)
        if not (reports_src / "blocks.json").is_file():
            fail("self-test: fake datagen reports were not found")

        # Mock reports drive the same finish path a real --all run uses.
        reports = mock_reports(root / "mock-reports")
        finished = finish_generation(clean_jar, final_out, bundled, reports)
        asset = json.loads((final_out / "assets/block_states_1211.json").read_text())
        if asset.get("block_states_total") != 2 or asset.get("minecraft") != "1.21.1":
            fail("self-test: block-state asset has unexpected totals")
        if finished["summary"]["members"] != 2 or finished["summary"]["reports"] != 3:
            fail("self-test: manifest verification counted the wrong outputs")
        verify_output(final_out, jar_path=clean_jar)
        report_only = json.loads((final_out / ASSET_REL).read_text())
        if physical_asset_schema_ok(report_only):
            fail("self-test: report-only state index passed the physical-facts schema gate")
        canonical_probe = {
            "blocks": [["minecraft:stone", 1, 1]],
            "flags": [8],
            "luminance": [0],
            "light_block": [15],
            "full_sides": [63],
            "shapes": [{"min": [0, 0, 0], "max": [1, 1, 1]}],
            "collision_shapes": [[0]],
        }
        if not physical_asset_schema_ok(canonical_probe):
            fail("self-test: physical-facts schema probe was rejected")
        try:
            ensure_mapping_jars(root / "missing-mappings", False)
        except VanillaDataError:
            pass
        else:
            fail("self-test: missing pinned mappings did not fail closed")
        mapping_jar = root / "mapping.jar"
        with zipfile.ZipFile(mapping_jar, "w") as archive:
            archive.writestr("mappings/mappings.tiny", "tiny\t2\t0\tofficial\tintermediary\n")
        tiny = extract_tiny_mapping(mapping_jar, root / "tiny-out" / "mapping.tiny")
        if "intermediary" not in tiny.read_text():
            fail("self-test: mapping tiny extraction lost its payload")
    print("self-test: PASS (traversal rejection, bundled/direct extraction, reports, state index, schema/mapping gates, verify)")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--jar", type=Path, help="vanilla server jar (bundler or unpacked)")
    parser.add_argument("--out-dir", type=Path, default=Path("."), help="output root (default: .)")
    parser.add_argument("--verify", action="store_true", help="verify an existing output tree")
    parser.add_argument("--self-test", action="store_true", help="run the clean-room self-test")
    parser.add_argument("--full-runtime", action="store_true", help="also derive the canonical physical block-state asset through Extract.java")
    parser.add_argument("--mappings-dir", type=Path, help="directory containing pinned Fabric CC0 mapping jars")
    parser.add_argument("--fetch-mappings", action="store_true", help="download the pinned CC0 mapping jars when absent")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()
    if args.verify:
        try:
            summary = verify_output(args.out_dir, jar_path=args.jar)
        except (VanillaDataError, OSError, ValueError) as error:
            print(f"vanilla data: FAILED {error}", file=sys.stderr)
            return 1
        print(
            f"vanilla data: verified {summary['members']} datapack members, "
            f"{summary['reports']} reports, {summary['asset_bytes']}-byte state asset"
        )
        return 0
    if args.jar is None:
        parser.error("--jar is required unless --verify or --self-test is used")
    try:
        result = generate(
            args.jar,
            args.out_dir,
            full_runtime=args.full_runtime,
            mappings_dir=args.mappings_dir,
            fetch_mappings=args.fetch_mappings,
        )
    except (VanillaDataError, PackError, OSError, ValueError, zipfile.BadZipFile) as error:
        print(f"vanilla data: FAILED {error}", file=sys.stderr)
        return 1
    summary = result["summary"]
    print(
        f"vanilla data: wrote {summary['members']} datapack members, "
        f"{summary['reports']} reports and {ASSET_REL} under {args.out_dir}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

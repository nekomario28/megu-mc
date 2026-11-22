#!/usr/bin/env python3
"""Export a Prism instance and/or a deterministic Minecraft .mrpack."""

import argparse
import ctypes
import hashlib
import json
from pathlib import Path
import re
import shutil
import stat
import tempfile
import zipfile

from build_integrated_mod import build_jar as build_integrated_jar


HERE = Path(__file__).resolve().parent
ZIP_TIME = (1980, 1, 1, 0, 0, 0)
MINECRAFT = "1.21.1"
MC_VERSIONS = ("1.21.1", "26.3")
BOOTSTRAP_UID = "net.mcspine.bootstrapper"
BOOTSTRAP_LIBRARY = "net.mcspine:bootstrapper:1.0.0"
BOOTSTRAP_JAR = Path("net/mcspine/bootstrapper/1.0.0/bootstrapper-1.0.0.jar")
BOOTSTRAP_PATCH = "net.mcspine.bootstrapper.json"
MAX_BOOTSTRAP_JAR = 32 * 1024
MESH_ABI = 1
KERNEL_ABI = 1
LOADER_UID = {"fabric": "net.fabricmc.fabric-loader", "neoforge": "net.neoforged"}
LOADER_DEPENDENCY = {"fabric": "fabric-loader", "neoforge": "neoforge"}
MESH_NAME = "libmc_spine_client_mesh.so"
KERNEL_NAME = "libmc_spine_kernel.so"


def sha(path, algorithm):
    digest = hashlib.new(algorithm)
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def verify_executable(binary, expected):
    if binary is None:
        return
    if not binary.is_file() or not binary.stat().st_mode & 0o111:
        raise ValueError("--client-bin must be an executable native client")
    if expected is not None and sha(binary, "sha256") != expected.lower():
        raise ValueError("native client SHA-256 mismatch")


def verify_library_abi(library, label, symbol, expected):
    """Load a C library with ctypes and require symbol() to return the expected ABI."""
    if not library.is_file():
        raise ValueError(f"{label} library is missing")
    try:
        handle = ctypes.CDLL(str(library))
        abi = getattr(handle, symbol)
        abi.restype = ctypes.c_uint32
        abi.argtypes = []
        found = abi()
    except (OSError, AttributeError) as error:
        raise ValueError(f"{label} library is not loadable or lacks {symbol}") from error
    if found != expected:
        raise ValueError(f"unsupported {label} C ABI version {found}, expected {expected}")


def verify_mesh_abi(library):
    """The current client-port C ABI exposes mc_spine_mesh_abi_version (v1)."""
    verify_library_abi(library, "mesh", "mc_spine_mesh_abi_version", MESH_ABI)


def verify_kernel_abi(library):
    """The client-kernel C ABI exposes mc_spine_kernel_abi_version (v1)."""
    verify_library_abi(library, "kernel", "mc_spine_kernel_abi_version", KERNEL_ABI)


def write_text(path, contents):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents, encoding="utf-8")


def json_text(value):
    return json.dumps(value, indent=2, ensure_ascii=False) + "\n"


def component_list(loader, loader_version, mc_version=MINECRAFT):
    components = [{"uid": "net.minecraft", "version": mc_version, "important": True}]
    if loader:
        components.append({"uid": LOADER_UID[loader], "version": loader_version})
    return components


def bootstrap_patch(mc_version, client_sha256):
    """Prism custom patch: the bootstrapper jar becomes the game mainClass."""
    patch = {
        "formatVersion": 1,
        "uid": BOOTSTRAP_UID,
        "version": "1.0.0",
        "name": "mc-spine bootstrapper",
        "mainClass": "net.mcspine.launcher.Main",
        "inheritsFrom": mc_version,
        "libraries": [{"name": BOOTSTRAP_LIBRARY, "MMC-hint": "local"}],
    }
    if client_sha256:
        patch["+jvmArgs"] = [f"-Dmc.spine.clientSha256={client_sha256.lower()}"]
    return patch


def prism_export(directory, loader, loader_version, name, client_bin, client_sha256,
                 integrated_jar, native_libs, mc_version=MINECRAFT, bootstrap_jar=None):
    directory.mkdir(parents=True, exist_ok=True)
    if bootstrap_jar is not None:
        # Zero-shell-wrapper mode: Prism applies patches/*.json itself, so the
        # bootstrapper JAR is the mainClass on Windows, macOS and Linux alike.
        library = directory / "libraries" / BOOTSTRAP_JAR
        library.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(bootstrap_jar, library)
        patch = directory / "patches" / BOOTSTRAP_PATCH
        write_text(patch, json_text(bootstrap_patch(mc_version, client_sha256)))
        if client_bin:
            exe = ".exe" if client_bin.name.lower().endswith(".exe") else ""
            target = directory / f".minecraft/bin/mc-spine-client{exe}"
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(client_bin, target)
            target.chmod(0o755)
        write_text(directory / "mmc-pack.json", json_text({
            "formatVersion": 1,
            "components": component_list(loader, loader_version, mc_version),
        }))
        write_text(directory / "instance.cfg",
                   "[General]\nInstanceType=OneSix\n"
                   f"name={name}\nOverrideCommands=false\n")
    else:
        # Prism's wrapper command needs a fully resolved executable. Double quotes
        # preserve paths containing spaces in Prism's command-line splitter.
        wrapper = directory / "mc-spine-wrapper"
        shutil.copyfile(HERE / "mc-spine-wrapper", wrapper)
        wrapper.chmod(0o755)
        wrapper_command = f'"{wrapper.resolve()}"'
        if client_bin:
            target = directory / "bin/mc-spine-client"
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(client_bin, target)
            target.chmod(0o755)
            wrapper_command += f" --client-sha256 {client_sha256 or sha(target, 'sha256')}"
        elif client_sha256:
            wrapper_command += f" --client-sha256 {client_sha256.lower()}"

        write_text(directory / "mmc-pack.json", json_text({
            "formatVersion": 1,
            "components": component_list(loader, loader_version, mc_version),
        }))
        write_text(directory / "instance.cfg",
                   "[General]\nInstanceType=OneSix\n"
                   f"name={name}\nOverrideCommands=true\nWrapperCommand={wrapper_command}\n")
    if integrated_jar:
        destination = directory / ".minecraft/mods/mc-spine-integrated.jar"
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(integrated_jar, destination)
    for name, library in sorted(native_libs.items()):
        destination = directory / "native" / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(library, destination)


def zip_entry(archive, name, data, mode=0o644):
    info = zipfile.ZipInfo(name, ZIP_TIME)
    info.create_system = 3
    info.external_attr = (stat.S_IFREG | mode) << 16
    info.compress_type = zipfile.ZIP_DEFLATED
    archive.writestr(info, data, compresslevel=9)


def prism_zip_export(prism_dir, destination):
    """Package a Prism instance directory into a deterministic importable .zip."""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(destination, "w") as archive:
        for path in sorted(prism_dir.rglob("*")):
            if not path.is_file():
                continue
            rel = path.relative_to(prism_dir).as_posix()
            mode = 0o755 if (path.stat().st_mode & 0o111) else 0o644
            zip_entry(archive, rel, path.read_bytes(), mode=mode)
    return destination


def mrpack_export(directory, loader, loader_version, name, version_id, integrated_jar,
                  native_libs, mc_version=MINECRAFT, client_bin=None):
    directory.mkdir(parents=True, exist_ok=True)
    contents = {"overrides/mods/mc-spine-integrated.jar": (integrated_jar.read_bytes(), 0o644)}
    for lib_name, library in native_libs.items():
        contents[f"overrides/mc-spine/native/{lib_name}"] = (library.read_bytes(), 0o644)
    if client_bin is not None:
        exe = ".exe" if client_bin.name.lower().endswith(".exe") else ""
        rel_bin = f"bin/mc-spine-client{exe}"
        contents[f"overrides/{rel_bin}"] = (client_bin.read_bytes(), 0o755)
        cfg = (
            "# Auto-generated by mc-spine export_launcher_bundle.py for Modrinth .mrpack\n"
            "enabled=true\n"
            f"serverBin={rel_bin}\n"
            "world=mc-spine\n"
            "seed=0\n"
        ).encode("utf-8")
        contents["overrides/config/mc-spine-integrated.properties"] = (cfg, 0o644)
    # .mrpack files[] describes downloadable URLs. Local overrides are bundled,
    # so checksums are retained in a namespaced extension instead.
    bundled = {}
    for path, (content, _) in sorted(contents.items()):
        bundled[path.removeprefix("overrides/")] = {
            "hashes": {"sha1": hashlib.sha1(content).hexdigest(),
                       "sha512": hashlib.sha512(content).hexdigest()},
            "fileSize": len(content),
        }
    index = {
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": version_id,
        "name": name,
        "summary": f"mc-spine Integrated Server bridge for Minecraft {mc_version}",
        "files": [],
        "dependencies": {"minecraft": mc_version,
                         LOADER_DEPENDENCY[loader]: loader_version},
        "mcSpine": {"bundledOverrides": bundled},
    }
    destination = directory / f"mc-spine-{mc_version}.mrpack"
    with zipfile.ZipFile(destination, "w") as archive:
        zip_entry(archive, "modrinth.index.json", json_text(index).encode())
        for path, (content, mode) in sorted(contents.items()):
            zip_entry(archive, path, content, mode)
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--format", choices=("prism", "mrpack", "modrinth", "all"),
                        default="prism")
    parser.add_argument("--loader", choices=("fabric", "neoforge"))
    parser.add_argument("--loader-version",
                        help="exact Fabric Loader or NeoForge version for this Minecraft release")
    parser.add_argument("--mc-version", choices=MC_VERSIONS, default=MINECRAFT,
                        help="Minecraft release for Prism components and .mrpack dependencies")
    parser.add_argument("--name", help="instance/pack name (default: mc-spine <mc-version>)")
    parser.add_argument("--version-id", default="0.1.0")
    parser.add_argument("--client-bin", type=Path,
                        help="optional native executable to bundle into the Prism instance")
    parser.add_argument("--client-sha256", help="expected SHA-256 of native executable")
    parser.add_argument("--integrated-jar", type=Path,
                        help="existing integrated JAR (otherwise build it for loader packs)")
    parser.add_argument("--mesh-library", type=Path,
                        help="optional client-port cdylib (C ABI v1 required)")
    parser.add_argument("--kernel-library", type=Path,
                        help="optional client-kernel cdylib (C ABI v1 required)")
    parser.add_argument("--include-ffi-libs", action="store_true",
                        help="also bundle the client-port/client-kernel release cdylibs "
                             "(libmc_spine_client_mesh.so / libmc_spine_kernel.so) found "
                             "under this repo's target/release dirs - explicit opt-in "
                             "for external JVM FFI consumers; statically linked clients "
                             "never need them, so exports omit them by default")
    parser.add_argument("--bootstrap-jar", type=Path,
                        help="mc-spine-bootstrapper.jar (<32 KiB) for a native Prism "
                             "patches/*.json install (no shell wrapper)")
    parser.add_argument("--prism-zip", action="store_true",
                        help="also emit a deterministic Prism import archive mc-spine-prism-<mc>.zip")
    args = parser.parse_args()
    exporting_mrpack = args.format in ("mrpack", "modrinth", "all")
    if args.name is None:
        args.name = f"mc-spine {args.mc_version}"
    if args.bootstrap_jar is not None and args.format not in ("prism", "all"):
        parser.error("--bootstrap-jar requires --format prism or all")
    if bool(args.loader) != bool(args.loader_version):
        parser.error("--loader and --loader-version must be provided together")
    if exporting_mrpack and not args.loader:
        parser.error(".mrpack requires --loader and --loader-version (Fabric/NeoForge)")
    if args.loader_version and (not re.fullmatch(r"[0-9A-Za-z._+~-]+", args.loader_version)
                                or args.loader_version in (".", "..")):
        parser.error("--loader-version must be a concrete version")
    if not args.name or any(char in args.name for char in "\n\r[]=\\"):
        parser.error("--name must be a single-line instance name")
    if not re.fullmatch(r"[0-9A-Za-z._-]+", args.version_id):
        parser.error("--version-id must be a safe version identifier")
    if args.client_sha256 and not re.fullmatch(r"[0-9a-fA-F]{64}", args.client_sha256):
        parser.error("--client-sha256 must be 64 hexadecimal characters")

    client_bin = args.client_bin.resolve() if args.client_bin else None
    bootstrap_jar = args.bootstrap_jar.resolve() if args.bootstrap_jar else None
    native_libs = {}
    for name, option in ((MESH_NAME, args.mesh_library), (KERNEL_NAME, args.kernel_library)):
        if option:
            native_libs[name] = option.resolve()
    if args.include_ffi_libs:
        # Explicit bulk opt-in: pick up the sibling crates' release cdylibs so
        # release pipelines can bundle the FFI surface without spelling out
        # each path. An explicit --mesh-library/--kernel-library wins, and a
        # missing crate target is skipped rather than failing the export.
        repo = HERE.parents[1]
        for crate, name in (("client-port", MESH_NAME), ("client-kernel", KERNEL_NAME)):
            if name in native_libs:
                continue
            candidate = repo / crate / "target" / "release" / name
            if candidate.is_file():
                native_libs[name] = candidate.resolve()
    integrated_jar = args.integrated_jar.resolve() if args.integrated_jar else None
    try:
        verify_executable(client_bin, args.client_sha256)
        if MESH_NAME in native_libs:
            verify_mesh_abi(native_libs[MESH_NAME])
        if KERNEL_NAME in native_libs:
            verify_kernel_abi(native_libs[KERNEL_NAME])
        if integrated_jar and not integrated_jar.is_file():
            raise ValueError("--integrated-jar must be an existing JAR")
        if integrated_jar and not args.loader:
            raise ValueError("--integrated-jar requires --loader and --loader-version")
        if bootstrap_jar is not None:
            if not bootstrap_jar.is_file():
                raise ValueError("--bootstrap-jar must be an existing JAR")
            if bootstrap_jar.stat().st_size >= MAX_BOOTSTRAP_JAR:
                raise ValueError("--bootstrap-jar must stay under 32 KiB")
    except (OSError, ValueError) as error:
        parser.error(str(error))

    output = args.out_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="mc-spine-cf4-") as scratch:
        if args.loader and integrated_jar is None:
            integrated_jar = build_integrated_jar(Path(scratch))
        if args.format in ("prism", "all"):
            prism_dir = output / "prism" if args.format == "all" else output
            prism_export(prism_dir, args.loader, args.loader_version, args.name,
                         client_bin, args.client_sha256, integrated_jar, native_libs,
                         mc_version=args.mc_version, bootstrap_jar=bootstrap_jar)
            print(f"Prism instance: {prism_dir}")
            if args.prism_zip:
                zip_dest = output / f"mc-spine-prism-{args.mc_version}.zip"
                prism_zip_export(prism_dir, zip_dest)
                print(f"Prism archive: {zip_dest} (sha256={sha(zip_dest, 'sha256')})")
        if exporting_mrpack:
            bundle = mrpack_export(output, args.loader, args.loader_version, args.name,
                                   args.version_id, integrated_jar, native_libs,
                                   mc_version=args.mc_version, client_bin=client_bin)
            print(f"Modrinth bundle: {bundle} (sha256={sha(bundle, 'sha256')})")


if __name__ == "__main__":
    main()

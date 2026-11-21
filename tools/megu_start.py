#!/usr/bin/env python3
"""Start a local mc-spine server with an explicit EULA gate and safe preflight.

The modpack checker classifies referenced files but does not download them.
No server process is started for --dry-run, including with --json.
"""

import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SCHEMA = "megu-start-plan/1"
DEFAULT_PORT = 25565
MAX_ARCHIVE_MEMBERS = 8192
MAX_ARCHIVE_MEMBER_BYTES = 128 * 1024 * 1024
MAX_ARCHIVE_TOTAL_BYTES = 1024 * 1024 * 1024
SENSITIVE_KEY = re.compile(r"password|secret|token|credential|private[._-]?key|authorization", re.I)


MESSAGES = {
    "eula": {
        "en": ("Minecraft's EULA has not been accepted.",
               "Read https://aka.ms/MinecraftEULA and run again with --accept-eula, or set eula=true in eula.txt."),
        "ja": ("Minecraft の利用規約（EULA）への同意が確認できません。",
               "https://aka.ms/MinecraftEULA を読み、--accept-eula を付けて再実行するか、eula.txt に eula=true を設定してください。"),
    },
    "port": {
        "en": ("This server's network port is already in use.",
               "Stop the other server or choose another port with --port."),
        "ja": ("サーバーが使う通信ポートは既に使用されています。",
               "ほかのサーバーを停止するか、--port で別の番号を指定してください。"),
    },
    "mixin": {
        "en": ("A mod uses a change to Minecraft that this server cannot run yet.",
               "Remove that mod or check its supported version before starting again."),
        "ja": ("このサーバーで実行できない変更方式を使う Mod が見つかりました。",
               "該当 Mod を外すか、対応バージョンを確認してから再起動してください。"),
    },
    "corrupt": {
        "en": ("A game data or world save file could not be read.",
               "Keep a backup, then restore the damaged file from a known-good copy."),
        "ja": ("ゲームデータまたはワールドの保存ファイルを読み取れません。",
               "バックアップを取り、正常なコピーから破損したファイルを復元してください。"),
    },
    "data": {
        "en": ("Minecraft's generated game data is missing.",
               "Generate it using your own official server.jar with tools/vanilla_data/gen_vanilla_data.sh."),
        "ja": ("Minecraft の生成済みゲームデータがありません。",
               "公式 server.jar を用意し、tools/vanilla_data/gen_vanilla_data.sh で生成してください。"),
    },
    "java": {
        "en": ("The installed Java version cannot run this Minecraft component.",
               "Check the component's Java requirement and select the matching Java installation."),
        "ja": ("使用中の Java のバージョンでは、この Minecraft コンポーネントを実行できません。",
               "必要な Java バージョンを確認し、対応する Java を選択してください。"),
    },
    "version": {
        "en": ("This modpack targets a different Minecraft version.",
               "Choose the matching version with --mc-version, or use a compatible modpack."),
        "ja": ("Modpack と Minecraft のバージョンが一致しません。",
               "--mc-version で対応版を選ぶか、互換性のある Modpack を使用してください。"),
    },
    "archive": {
        "en": ("The modpack file is damaged or contains an unsafe path.",
               "Use a trusted, unmodified modpack archive and try again."),
        "ja": ("Modpack が破損しているか、危険なファイル名が含まれています。",
               "信頼できる元の Modpack ファイルを用意して再実行してください。"),
    },
    "input": {
        "en": ("A directory, file, or option could not be used safely.",
               "Check the path and permissions, then try again."),
        "ja": ("指定したフォルダー、ファイル、または設定を安全に使用できません。",
               "パスとアクセス権を確認して再実行してください。"),
    },
    "inspection": {
        "en": ("Some Java mods still need a compatibility check.",
               "Inspect those mods before starting the server."),
        "ja": ("一部の Java Mod は互換性の確認が必要です。",
               "対象 Mod の互換性を確認してから起動してください。"),
    },
    "staging": {
        "en": ("The modpack was checked, but its mods have not been installed.",
               "Install the supported server mods into the server's mods folder before launching."),
        "ja": ("Modpack の検査は完了しましたが、Mod のインストールは完了していません。",
               "対応するサーバー用 Mod を mods フォルダーに配置してから起動してください。"),
    },
    "runtime": {
        "en": ("The server stopped before it could start normally.",
               "Check the installation and try starting again."),
        "ja": ("サーバーが正常に起動する前に停止しました。",
               "インストール状態を確認して再起動してください。"),
    },
}


def diagnose_error(stderr_text, lang="en"):
    """Return two operator-focused lines without echoing untrusted diagnostics."""
    language = "ja" if lang == "ja" else "en"
    raw = stderr_text.lower()
    patterns = (
        ("eula", r"eula|agree to the.*license"),
        ("port", r"address already in use|addrinuse|os error 98|error 10048|game listener bind failed|port.*in use"),
        ("java", r"unsupportedclassversion|class file version|java version|requires java|java runtime"),
        ("mixin", r"mixin|access.transformer|unsupported mod|incompatible mod"),
        ("data", r"vanilla data missing|block_states_1211|mc_spine_version_dir|missing generated data|config_packets\.bin"),
        ("corrupt", r"corrupt|invalid json|serde_json|json parse|level\.dat|save file|unexpected end of file"),
    )
    for category, pattern in patterns:
        if re.search(pattern, raw):
            return "\n".join(MESSAGES[category][language])
    return "\n".join(MESSAGES["runtime"][language])


def language_for(setting):
    if setting != "auto":
        return setting
    return "ja" if (os.environ.get("LC_ALL") or os.environ.get("LANG") or "").lower().startswith("ja") else "en"


def checked_path(value, label):
    """Allow absolute/relative operator paths, rejecting traversal and NUL."""
    raw = os.fspath(value)
    if not raw or "\x00" in raw or re.match(r"^[a-zA-Z]:", raw):
        raise ValueError(f"unsafe {label}")
    if any(part == ".." for part in re.split(r"[/\\]", raw)):
        raise ValueError(f"unsafe {label}")
    return Path(raw).expanduser().absolute()


def check_no_symlink(path):
    """Avoid silently writing through an operator-controlled path alias."""
    for part in (path, *path.parents):
        if part.is_symlink():
            raise ValueError("symlink path is not accepted")


def properties_from(text):
    values = {}
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped[0] in "#!":
            continue
        match = re.match(r"([^=:]+)\s*[=:]\s*(.*)", stripped)
        if match:
            values[match.group(1).strip()] = match.group(2).strip()
    return values


def sanitized_properties(values):
    return {key: "[REDACTED]" if SENSITIVE_KEY.search(key) else value
            for key, value in sorted(values.items())}


def ensure_eula(directory, accept):
    path = directory / "eula.txt"
    check_no_symlink(path)
    if not path.exists():
        path.write_text("eula=true\n" if accept else "eula=false\n", encoding="utf-8")
    elif accept:
        original = path.read_text(encoding="utf-8")
        if not eula_accepted(original):
            lines = original.splitlines(keepends=True)
            found = False
            for index, line in enumerate(lines):
                if re.match(r"^\s*eula\s*=", line):
                    lines[index] = "eula=true\n"
                    found = True
            if not found:
                lines.append("\neula=true\n")
            path.write_text("".join(lines), encoding="utf-8")
    return eula_accepted(path.read_text(encoding="utf-8"))


def eula_accepted(text):
    return properties_from(text).get("eula", "").lower() == "true"


def bootstrap(directory, port, accept):
    check_no_symlink(directory)
    directory.mkdir(parents=True, exist_ok=True)
    props_file = directory / "server.properties"
    check_no_symlink(props_file)
    if not props_file.exists():
        props_file.write_text(f"server-port={port or DEFAULT_PORT}\nserver-ip=127.0.0.1\n", encoding="utf-8")
    properties = properties_from(props_file.read_text(encoding="utf-8"))
    return properties, ensure_eula(directory, accept)


def validate_archive(path):
    """Inspect every ZIP member before passing an archive to another tool."""
    if path.suffix.lower() not in (".mrpack", ".zip"):
        return
    try:
        with zipfile.ZipFile(path) as archive:
            members = archive.infolist()
            if len(members) > MAX_ARCHIVE_MEMBERS:
                raise ValueError("too many files")
            total = 0
            for member in members:
                name = member.filename
                segments = name.rstrip("/").split("/")
                if (not name or "\x00" in name or "\\" in name or name.startswith("/")
                        or re.match(r"^[a-zA-Z]:", name)
                        or any(part in ("", ".", "..") for part in segments)
                        or stat.S_ISLNK(member.external_attr >> 16)):
                    raise ValueError("unsafe archive member")
                total += member.file_size
                if (member.flag_bits & 1 or member.file_size > MAX_ARCHIVE_MEMBER_BYTES
                        or total > MAX_ARCHIVE_TOTAL_BYTES):
                    raise ValueError("unsafe archive size or encryption")
    except (OSError, zipfile.BadZipFile, zipfile.LargeZipFile) as error:
        raise ValueError("invalid archive") from error


def mod_preflight(args, plan, lang):
    if args.modpack is None and args.mods_dir is None:
        return 0
    source = checked_path(args.modpack or args.mods_dir, "mod input")
    check_no_symlink(source)
    if args.modpack is not None:
        try:
            validate_archive(source)
        except ValueError:
            plan["action_items"].append("\n".join(MESSAGES["archive"][lang]))
            return 1
        if not source.is_file():
            raise ValueError("missing modpack")
        runner = ([str(checked_path(args.mod_check_bin, "mod checker"))]
                  if args.mod_check_bin and ("/" in args.mod_check_bin or "\\" in args.mod_check_bin)
                  else [args.mod_check_bin] if args.mod_check_bin
                  else [sys.executable, str(ROOT / "tools" / "mod_launcher.py")])
        command = runner + ["--modpack", str(source), "--mc-version", args.mc_version, "--json"]
    else:
        if not source.is_dir():
            raise ValueError("missing mods directory")
        command = [sys.executable, str(ROOT / "tools" / "mod_launcher.py"),
                   "--mods-dir", str(source), "--mc-version", args.mc_version,
                   "--fallback", "strict", "--dry-run", "--json"]
    try:
        result = subprocess.run(command, shell=False, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", timeout=60, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ValueError("mod checker could not run") from error
    if result.returncode == 1:
        category = "archive" if args.modpack is not None else "input"
        plan["action_items"].append("\n".join(MESSAGES[category][lang]))
        return 1
    if result.returncode not in (0, 2):
        plan["action_items"].append("\n".join(MESSAGES["input"][lang]))
        return 1
    try:
        report = json.loads(result.stdout)
        if not isinstance(report, dict):
            raise ValueError("invalid report")
    except (ValueError, TypeError):
        plan["action_items"].append("\n".join(MESSAGES["input"][lang]))
        return 1
    if args.modpack is not None:
        if report.get("schema") != "mc-spine-modpack-preflight/1":
            raise ValueError("unexpected mod report")
        plan["skipped_perf_mods"] = safe_names(report.get("skipped_perf_mods", []))
        plan["native_mods"] = safe_names(report.get("native_mods", []))
        plan["client_only_mods"] = safe_names(report.get("client_only_mods", []))
        plan["needs_inspection_mods"] = safe_names(report.get("needs_inspection_mods", []))
        if result.returncode == 2 or report.get("readiness") == "version_mismatch":
            plan["action_items"].append("\n".join(MESSAGES["version"][lang]))
            return 2
        if plan["needs_inspection_mods"]:
            plan["action_items"].append("\n".join(MESSAGES["inspection"][lang]))
        if plan["native_mods"]:
            plan["action_items"].append("\n".join(MESSAGES["staging"][lang]))
    else:
        if report.get("schema") != "mc-spine-mod-launcher/1":
            raise ValueError("unexpected mod report")
        plan["skipped_perf_mods"] = safe_names(report.get("superseded_perf_mods", []))
        plan["native_mods"] = safe_names([entry.get("mod_id") for entry in report.get("assignments", [])
                                          if isinstance(entry, dict) and entry.get("tier") in (0, 1)])
        if result.returncode == 2 or report.get("plan") != "RunNative":
            plan["action_items"].append("\n".join(MESSAGES["mixin"][lang]))
            return 2
    return 0


def safe_names(names):
    if not isinstance(names, list):
        return []
    return sorted({name for name in names if isinstance(name, str)
                   and len(name) <= 96 and re.fullmatch(r"[a-zA-Z0-9_. -]+", name)})


def runtime_settings(directory, properties, requested_port):
    settings = {}
    config_file = directory / "server.toml"
    if config_file.is_file():
        check_no_symlink(config_file)
        try:
            import tomllib
            parsed = tomllib.loads(config_file.read_text(encoding="utf-8"))
            settings = parsed.get("server", {})
        except (ImportError, ValueError, OSError):
            settings = {}
    port = requested_port if requested_port is not None else (
        os.environ.get("MC_SPINE_PORT") or settings.get("port")
        or properties.get("server-port") or DEFAULT_PORT)
    try:
        port = int(port)
    except (ValueError, TypeError) as error:
        raise ValueError("invalid server port") from error
    if not 1 <= port <= 65535:
        raise ValueError("invalid server port")
    bind = (os.environ.get("MC_SPINE_BIND_ADDR") or settings.get("bind_addr")
            or properties.get("server-ip") or "127.0.0.1")
    if not isinstance(bind, str) or "\x00" in bind or ".." in bind:
        raise ValueError("invalid bind address")
    try:
        ipaddress.ip_address(bind)
    except ValueError as error:
        raise ValueError("invalid bind address") from error
    return bind, port


def data_ready(mc_version):
    override = os.environ.get("MC_SPINE_VERSION_DIR")
    if override:
        directory = checked_path(override, "version data directory")
        check_no_symlink(directory)
        return ((directory / "block_states.json").is_file()
                or (directory / "block_states_1211.json").is_file())
    return (mc_version == "1.21.1"
            and (ROOT / "assets" / "block_states_1211.json").is_file())


def data_command(accept):
    command = "bash tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar --fetch-fabric-mappings"
    return command + (" --accept-eula" if accept else "")


def launch_command():
    binary = ROOT / "target" / "debug" / "mc-spine"
    if binary.is_file() and os.access(binary, os.X_OK):
        return [str(binary)]
    return ["cargo", "run", "--manifest-path", str(ROOT / "Cargo.toml"), "--bin", "mc-spine"]


def setup_runtime_data(directory):
    """Give the isolated server directory read-only access to bundled game data."""
    for filename in ("assets", "config_packets.bin"):
        destination = directory / filename
        if not destination.exists() and not destination.is_symlink():
            destination.symlink_to(ROOT / filename, target_is_directory=filename == "assets")
    datapacks = directory / "datapacks"
    if not datapacks.exists():
        datapacks.mkdir()
    check_no_symlink(datapacks)
    vanilla = datapacks / "vanilla"
    if not vanilla.exists() and not vanilla.is_symlink():
        vanilla.symlink_to(ROOT / "datapacks" / "vanilla", target_is_directory=True)


def output(plan, json_mode, lang, code):
    plan["ready"] = code == 0 and not plan["action_items"]
    if json_mode:
        print(json.dumps(plan, ensure_ascii=False, indent=2))
    else:
        if plan["ready"]:
            print("Megu: " + ("起動準備ができました。" if lang == "ja" else "Ready to start."))
        if plan["skipped_perf_mods"]:
            label = "自動的に省略した Mod" if lang == "ja" else "Automatically skipped mods"
            print(f"{label}: {', '.join(plan['skipped_perf_mods'])}")
        for item in plan["action_items"]:
            print(item)
    return code


def main(argv=None):
    parser = argparse.ArgumentParser(description="Start mc-spine with a guided, safe setup")
    parser.add_argument("--dir", required=True, help="server directory")
    parser.add_argument("--modpack", help="Modrinth or CurseForge modpack for pre-flight checks")
    parser.add_argument("--mods-dir", help="existing installed mods directory")
    parser.add_argument("--mod-check-bin", help="optional pre-built mc-spine-mod-check binary")
    parser.add_argument("--mc-version", choices=("1.21.1", "26.3"), default="1.21.1")
    parser.add_argument("--port", type=int)
    parser.add_argument("--accept-eula", action="store_true")
    parser.add_argument("--lang", choices=("auto", "en", "ja"), default="auto")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    lang = language_for(args.lang)
    plan = {"schema": SCHEMA, "ready": False, "bind_address": "127.0.0.1",
            "port": args.port or DEFAULT_PORT, "mc_version": args.mc_version,
            "skipped_perf_mods": [], "native_mods": [], "client_only_mods": [],
            "needs_inspection_mods": [], "action_items": [], "launch_argv": launch_command(),
            "server_properties": {}, "dry_run": bool(args.dry_run)}
    try:
        if args.modpack and args.mods_dir:
            raise ValueError("select one mod input")
        directory = checked_path(args.dir, "server directory")
        if args.port is not None and not 1 <= args.port <= 65535:
            raise ValueError("invalid port")
        if args.mod_check_bin:
            if "\x00" in args.mod_check_bin or ".." in re.split(r"[/\\]", args.mod_check_bin):
                raise ValueError("unsafe mod checker")
        properties, accepted = bootstrap(directory, args.port, args.accept_eula)
        plan["server_properties"] = sanitized_properties(properties)
        if not accepted:
            plan["action_items"].append("\n".join(MESSAGES["eula"][lang]))
            return output(plan, args.json, lang, 2)
        bind, port = runtime_settings(directory, properties, args.port)
        plan["bind_address"], plan["port"] = bind, port

        status = mod_preflight(args, plan, lang)
        if status:
            return output(plan, args.json, lang, status)
        if not data_ready(args.mc_version):
            plan["action_items"].append("\n".join(MESSAGES["data"][lang]))
            plan["action_items"].append(data_command(args.accept_eula))
            return output(plan, args.json, lang, 2)

        # A checked archive is a manifest, not an installation. A real launch
        # must not silently ignore the requested mods.
        if args.modpack and (plan["native_mods"] or plan["needs_inspection_mods"]):
            if not args.dry_run:
                return output(plan, args.json, lang, 2)
        if plan["needs_inspection_mods"]:
            if not args.dry_run:
                return output(plan, args.json, lang, 2)
        if args.dry_run:
            return output(plan, args.json, lang, 0)

        setup_runtime_data(directory)
        env = os.environ.copy()
        env["MC_SPINE_PORT"] = str(port)
        env["MC_SPINE_BIND_ADDR"] = bind
        if env.get("MC_SPINE_VERSION_DIR"):
            env["MC_SPINE_VERSION_DIR"] = str(checked_path(env["MC_SPINE_VERSION_DIR"], "version directory"))
        if not args.json:
            notice = "サーバーを起動します。" if lang == "ja" else "Starting server."
            print(f"Megu: {notice}", flush=True)
        try:
            # Drain error output as the server runs, retaining only a bounded
            # suffix for diagnosis. Raw logs may contain credentials.
            with subprocess.Popen(plan["launch_argv"], shell=False, cwd=directory,
                                  env=env, stdin=None, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.PIPE) as process:
                tail = b""
                for chunk in iter(lambda: process.stderr.read(4096), b""):
                    tail = (tail + chunk)[-65536:]
                result_code = process.wait()
        except OSError:
            plan["action_items"].append("\n".join(MESSAGES["runtime"][lang]))
            return output(plan, args.json, lang, 1)
        if result_code:
            plan["action_items"].append(diagnose_error(tail.decode("utf-8", errors="replace"), lang))
            return output(plan, args.json, lang, 1)
        return output(plan, args.json, lang, 0)
    except (OSError, ValueError, UnicodeError) as error:
        # Do not include exception text: it can contain operator credentials.
        plan["action_items"].append("\n".join(MESSAGES["input"][lang]))
        return output(plan, args.json, lang, 1)


if __name__ == "__main__":
    sys.exit(main())

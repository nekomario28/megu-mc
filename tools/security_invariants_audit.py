#!/usr/bin/env python3
"""Fail-closed, stdlib-only structural checks for security-critical Rust paths.

These checks complement (do not replace) runtime tests and code review. Patterns
are tied to the current implementations so a refactor needs deliberate review.
"""
import argparse
from collections import Counter
import hashlib
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
NETWORK_FILES = (
    "src/play_protocol.rs", "src/connection_limits.rs", "src/ingress_guard.rs",
    "src/server_reliability.rs", "server-files/src/claims.rs",
    "mod-adapter-core/src/mod_sync.rs",
)
FILES = (
    *NETWORK_FILES,
    "src/nbt.rs",
    "client-app/src/integrated.rs",
    "src/main.rs",
    "proto/src/wire.rs",
    "proto/src/session.rs",
    "wasm-mod-runtime/src/exec.rs",
    "wasm-mod-runtime/src/wasm_mod.rs",
    "server-files/src/bin/megu_installer.rs",
    "server-files/src/modrinth_sync.rs",
    "core/src/crash_report.rs",
)
# Pre-existing gameplay mutex unwraps in play_protocol.rs. Not auth/claims,
# but these must be removed in Rust before this file can be zero-baseline.
# Exact surrounding-code fingerprints: any *new* occurrence fails closed.
LEGACY_PLAY_LOCKS = Counter("""b37c67ec1bca037e 52c475e0851aafdf c7eb60730e260c33
0e1a82ccffd4bbe9 da498f673f217bcb 9208cb2b4fc41a95 0d2c273335499452
e58a054f9bfb6df7 37cea4f4c2a9503c b867ab2b7ea199c2 8635a6eb5275198b
fe395cd81242303f 83d76a9ce6ed9b11 280eeb3de973779d 96d7b448768e71aa
8d8bf6461f39ae8c d8a9b0438ce4dfc4 7ec61ca0f9ea7391 4d886f7cafa05277
71ed447d31317e3f 71ed447d31317e3f 22f4f72e131dd720""".split())


def production(text):
    """Exclude test-only items/modules and line comments, keeping production code."""
    # The #[cfg(test)] attribute can decorate either an item or a whole module.
    lines = text.splitlines(keepends=True)
    output = []
    pending = False
    skipping = False
    depth = 0
    for line in lines:
        if re.match(r"^\s*#\[cfg\(test\)\]", line):
            pending = True
            continue
        if pending:
            if not line.strip() or line.lstrip().startswith(("//", "#[")):
                continue
            pending = False
            skipping = True
            depth = 0
        if skipping:
            # Attributes may decorate a single statement, not just a function.
            depth += line.count("{") - line.count("}")
            if (depth <= 0 and "{" in line) or (depth == 0 and ";" in line):
                skipping = False
            continue
        output.append(re.sub(r"//.*$", "", line))
    return "".join(output)


def audit(root):
    sources = {}
    errors = {}
    for name in FILES:
        try:
            text = (root / name).read_text(encoding="utf-8")
            sources[name] = text if name == "src/main.rs" else production(text)
        except (OSError, UnicodeError) as exc:
            sources[name] = ""
            errors[name] = str(exc)

    def has(path, pattern):
        return bool(re.search(pattern, sources[path], re.S))

    play = "src/play_protocol.rs"
    limits = "src/connection_limits.rs"
    reliability = "src/server_reliability.rs"
    mod = "mod-adapter-core/src/mod_sync.rs"
    checks = {
        "offline public bind refusal": (
            has(play, r"fn offline_public_bind_check\s*\([^)]*allow_insecure_offline_public:\s*bool[^)]*\)\s*->\s*Result<\(\), String>\s*\{\s*if\s+connection_limits::is_loopback_bind\(bind\)\s*\|\|\s*online_mode\s*\|\|\s*velocity_forwarding\s*\|\|\s*allow_insecure_offline_public\s*\{\s*return Ok\(\(\)\);\s*\}\s*Err\(")
            and has(play, r"MEGU_ALLOW_INSECURE_OFFLINE_PUBLIC=1")
            and has("src/main.rs", r"if let Err\(why\)\s*=\s*offline_public_bind_check\([^;]*env_flag\(\"MEGU_ALLOW_INSECURE_OFFLINE_PUBLIC\"\)")),
        "constant-time Velocity HMAC": (
            has(reliability, r"fn ct_tag_eq\([\s\S]*?\.zip\(b\.iter\(\)\)\s*\.fold\(0u8,\s*\|acc,\s*\(x, y\)\|\s*acc\s*\|\s*\(x\s*\^\s*y\)\)")
            and has(reliability, r"if\s+!ct_tag_eq\(tag,\s*&hmac_sha256\(secret,\s*body\)\)")),
        "constant-time RCON comparison": (
            has(limits, r"fn ct_eq\([\s\S]*?diff\s*\|=\s*a\.get\(i\)[^;]*\^\s*b\.get\(i\)[^;]*;\s*\}\s*std::hint::black_box\(diff\)\s*==\s*0")
            and has("src/main.rs", r"connection_limits::ct_eq\(cmd\.as_bytes\(\),\s*pass\.as_bytes\(\)\)")),
        "bounded play frame before allocation": (
            has("src/ingress_guard.rs", r"const MAX_PLAY_FRAME:\s*usize\s*=\s*2\s*\*\s*1024\s*\*\s*1024\s*-\s*1\s*;")
            and has(play, r"if len == 0 \|\| len as usize > ingress_guard::MAX_PLAY_FRAME\s*\{")),
        "exact bounded decompression": has(limits, r"fn inflate_exact\([\s\S]*?reserve_spare\(length\.saturating_add\(1\)\)[\s\S]*?status\s*!=\s*Status::StreamEnd\s*\|\|\s*self\.inflate\.total_out\(\)\s*!=\s*length as u64\s*\|\|\s*self\.inflate\.total_in\(\)\s*!=\s*body\.len\(\) as u64"),
        "NBT container depth cap": len(re.findall(r"if depth >= 512\s*\{\s*return Err\(\"NBT exceeds container depth\"\)", sources["src/nbt.rs"])) >= 4,
        "server push rejects JARs": has(mod, r"if is_server_push_url\(&entry\.download_url\)\s*\{\s*if !has_suffix_ci\(&entry\.file_name, \"\.mmod\"\)\s*\{\s*return Err\([^;]*;\s*\}\s*validate_mmod_file_name\(&entry\.file_name\)\?;\s*return Ok\(\(ModSourceKind::ServerDeclarativeMmod, true\)\)"),
        "integrated loopback ephemeral port": has("client-app/src/integrated.rs", r"INTEGRATED_BIND_IP:\s*Ipv4Addr\s*=\s*Ipv4Addr::LOCALHOST\s*;") and has("client-app/src/integrated.rs", r"TcpListener::bind\(\(INTEGRATED_BIND_IP,\s*0\)\)"),
        "canonical protocol 767 frame cap": (
            has("proto/src/wire.rs", r"PROTOCOL_767_MAX_FRAME_BYTES:\s*usize\s*=\s*2\s*\*\s*1024\s*\*\s*1024\s*-\s*1\s*;")
            and has("proto/src/wire.rs", r"MAX_FRAME_LENGTH:\s*usize\s*=\s*PROTOCOL_767_MAX_FRAME_BYTES\s*;")
            and has("proto/src/wire.rs", r"VANILLA_FRAME_BYTES:\s*usize\s*=\s*PROTOCOL_767_MAX_FRAME_BYTES\s*;")
            and has("proto/src/wire.rs", r"MAX_STRING_BYTES:\s*usize\s*=\s*MAX_STRING_CHARS\s*\*\s*4\s*;")),
        "pre-login frame cap before allocation": (
            has(play, r"MAX_LOGIN_FRAME:\s*usize\s*=\s*64\s*\*\s*1024\s*;")
            and has(play, r"len\s*>\s*MAX_LOGIN_FRAME as u64[\s\S]{0,180}?vec!\[0u8; len as usize\]")
            and has(limits, r"MAX_PRELOGIN_FRAME_BYTES:\s*usize\s*=\s*64\s*\*\s*1024\s*;")),
        "absolute 8 MiB decompression ceiling": (
            has(limits, r"MAX_UNCOMPRESSED_PACKET_BYTES:\s*usize\s*=\s*8\s*\*\s*1024\s*\*\s*1024\s*;")
            and has(limits, r"max_uncompressed\.min\(MAX_UNCOMPRESSED_PACKET_BYTES\)")
            and has(limits, r"length\s*<\s*threshold\s*\|\|\s*length\s*>\s*max_uncompressed")),
        "pre-login packet rate and slowloris deadlines": (
            has(play, r"fn read_preplay_frame\([\s\S]*?rate\.allow\(now\)[\s\S]*?rate\.should_disconnect\(now\)")
            and has(play, r"set_read_timeout\(Some\(Duration::from_secs\(10\)\)\)[\s\S]{0,120}?set_write_timeout\(Some\(Duration::from_secs\(10\)\)\)")
            and has(play, r"Instant::now\(\)\s*\+\s*connection_limits::login_timeout\(\)")
            and has(limits, r"fn login_timeout\(\)[\s\S]*?unwrap_or\(15\)[\s\S]*?\.max\(1\)")
            and has(limits, r"fn idle_timeout_secs\(\)[\s\S]*?MEGU_IDLE_TIMEOUT_SECS")),
        "verified UUID binding before admission": (
            has(play, r"let mut authenticated\s*=\s*false\s*;")
            and len(re.findall(r"authenticated\s*=\s*true\s*;", sources[play])) == 2
            and has(play, r"check_login_admission_verified\([\s\S]*?authenticated\s*,\s*\)")),
        "login state machine cannot skip success": (
            has("proto/src/session.rs", r"duplicate login_start")
            and has("proto/src/session.rs", r"login_acknowledged before login_success")
            and has("proto/src/session.rs", r"awaiting_ack:\s*true")),
        "WASM fuel ceiling enforcement": has("wasm-mod-runtime/src/exec.rs", r"self\.fuel\s*=\s*self\.fuel\s*\.checked_sub\(1\)\s*\.ok_or\(Trap::FuelExhausted\)\?"),
        "WASM memory hard ceiling": (
            has("wasm-mod-runtime/src/wasm_mod.rs", r"MAX_MEM_PAGES_HARD_CAP:\s*u32\s*=\s*1024\s*;")
            and has("wasm-mod-runtime/src/wasm_mod.rs", r"config\.max_mem_pages\.min\(MAX_MEM_PAGES_HARD_CAP\)")
            and has("wasm-mod-runtime/src/wasm_mod.rs", r"decl\.limits\.min\s*>\s*hard_max")
            and has("wasm-mod-runtime/src/exec.rs", r"if\s+new\s*>\s*self\.max_pages\s*\{\s*return -1;\s*\}")),
        "WASM hostcall memory and string caps": (
            len(re.findall(r"if\s+len\s*>\s*self\.max_string\s*\{\s*return Err\(Trap::HostMemoryAccess", sources["wasm-mod-runtime/src/exec.rs"])) >= 2
            and len(re.findall(r"pub fn read\(&self, ptr: u32, len: u32\)[\s\S]*?end > self\.data\.len\(\) as u64[\s\S]*?Trap::HostMemoryAccess", sources["wasm-mod-runtime/src/exec.rs"])) >= 1),
        "installer hash verification fail closed": (
            has("server-files/src/bin/megu_installer.rs", r"parse_expected_sha256\(&sums_text, fname\)")
            and has("server-files/src/bin/megu_installer.rs", r"installer::sha256_file\(path\)")
            and has("server-files/src/bin/megu_installer.rs", r"if\s+actual\s*!=\s*expected[\s\S]*?std::fs::remove_file\(path\)")
            and has("server-files/src/bin/megu_installer.rs", r"installer::normalize_sha256_hex\(value\)")),
        "mrpack/installer path traversal rejection": (
            has("server-files/src/modrinth_sync.rs", r"fn validate_mrpack_path\([\s\S]*?NulByte[\s\S]*?Backslash[\s\S]*?AbsolutePath[\s\S]*?ParentTraversal")
            and has("server-files/src/modrinth_sync.rs", r"validate_mrpack_path\(path\)\.map_err")),
        "crash report redacts PII before output/fingerprint": (
            len(re.findall(r"let\s+r\s*=\s*self\.redacted\(\)\s*;", sources["core/src/crash_report.rs"])) >= 3
            and has("core/src/crash_report.rs", r"let mut out = redact_user_paths\(input\);\s*out = redact_bearer_tokens\(&out\);\s*out = redact_rcon_password\(&out\);\s*out = redact_ipv4\(&out\);")),
    }
    forbidden = re.compile(r"\bSystemTime\s*::\s*now\s*\(\s*\)|\.lock\s*\(\s*\)\s*\.unwrap\s*\(\s*\)")
    for name in NETWORK_FILES:
        text = sources[name]
        matches = list(forbidden.finditer(text))
        if name == "src/play_protocol.rs":
            known = Counter(hashlib.sha256(text[m.start()-65:m.end()+45].encode()).hexdigest()[:16]
                            for m in matches)
            checks[f"no new wall-clock/panic locks: {name}"] = not (known - LEGACY_PLAY_LOCKS)
        else:
            checks[f"no wall-clock/panic locks: {name}"] = not matches
    for name, _error in errors.items():
        checks[f"readable: {name}"] = False
    return checks, errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    checks, errors = audit(args.root)
    if args.json:
        print(json.dumps({"checks": [{"name": key, "pass": value} for key, value in checks.items()], "errors": errors}, indent=2))
    else:
        for name, passed in checks.items():
            print(f"{'PASS' if passed else 'FAIL'} {name}{': ' + errors[name[10:]] if name.startswith('readable: ') else ''}")
    return 0 if all(checks.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())

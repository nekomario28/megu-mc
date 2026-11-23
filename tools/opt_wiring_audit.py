#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Optimization hot-path wiring audit — permanent regression gate.

Every CPU/GPU/server optimization Megu relies on is only real while some
non-test production code actually calls it. These optimizations have a
recurring failure mode: the kernel survives, its tests stay green, and the
call site silently retreats into ``#[cfg(test)]`` code or behind a comment —
the feature becomes dead code that still compiles. This gate mechanically
proves each wired optimization still has a live non-test call site, so the
regression fails CI instead of shipping.

All checks run on masked Rust source: comments, string/char literals and
``#[cfg(test)]`` items are blanked out first (same technique as
``tools/wiring_audit.py`` / ``tools/client_vertical_wiring_audit.py``), so a
call site that only exists under test can never satisfy a check.

Check kinds:

* ``contains`` — every regex in ``patterns`` must match the masked file.
* ``call`` — ``name`` must have at least one non-test call site
  (``name(`` occurrences that are not the item's own ``fn`` definition).
* ``simd_guards`` — every ``#[target_feature]`` kernel in the file must
  (a) still have a non-test call site and (b) have every call site guarded
  by ``is_x86_feature_detected!`` (directly, through a same-file detector
  fn, or — for ``neon`` — inside an ``aarch64``-gated item where NEON is
  architecturally baseline). An unguarded call is the Core 2 Duo ``SIGILL``
  regression class; a kernel with zero callers is a dead optimization.

Usage:
    tools/opt_wiring_audit.py            # human PASS/FAIL summary
    tools/opt_wiring_audit.py --json     # machine-readable report
    tools/opt_wiring_audit.py --root DIR
"""

import argparse
import json
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parents[1]

WINDOW_RS = "client-gpu/src/window.rs"
RENDERER_RS = "client-gpu/src/renderer.rs"
LIVE_SESSION_RS = "client-gpu/src/live_session.rs"
MESH_RS = "client-port/src/mesh.rs"
SIMD_VOXEL_RS = "core/src/simd_voxel.rs"
WORLDGEN_RS = "src/worldgen.rs"
GENERATION_MATH_RS = "src/generation_math.rs"
PLAY_PROTOCOL_RS = "src/play_protocol.rs"
MAIN_RS = "src/main.rs"

# ---------------------------------------------------------------------------
# Rust source masking — mirrors tools/client_vertical_wiring_audit.py so
# comments, literals and cfg(test) items can never satisfy a check.
# ---------------------------------------------------------------------------

RAW_STRING = re.compile(r'(?:b|c)?r(#{0,255})"')
CHARACTER = re.compile(r"'(?:[^'\\\n]|\\(?:.|u\{[A-Fa-f0-9_]+\}))'")
CFG_TEST = re.compile(r"\bcfg\s*\(\s*(?:test\b|all\s*\(\s*test\b)")


def _balanced_end(code, start, opening, closing):
    depth = 0
    for pos in range(start, len(code)):
        if code[pos] == opening:
            depth += 1
        elif code[pos] == closing:
            depth -= 1
            if depth == 0:
                return pos + 1
    return len(code)


def _mask(code, start, end):
    code[start:end] = " " * (end - start)


def _skip_space(code, pos):
    while pos < len(code) and code[pos].isspace():
        pos += 1
    return pos


def production_code(text):
    """Mask Rust comments/literals and cfg(test) items, preserving offsets."""
    code = list(text)
    pos = 0
    while pos < len(text):
        start = pos
        if text.startswith("//", pos):
            end = text.find("\n", pos)
            pos = len(text) if end < 0 else end
        elif text.startswith("/*", pos):
            pos += 2
            depth = 1
            while pos < len(text) and depth:
                if text.startswith("/*", pos):
                    depth += 1
                    pos += 2
                elif text.startswith("*/", pos):
                    depth -= 1
                    pos += 2
                else:
                    pos += 1
        elif match := RAW_STRING.match(text, pos):
            terminator = '"' + match.group(1)
            end = text.find(terminator, match.end())
            pos = len(text) if end < 0 else end + len(terminator)
        elif text[pos] == '"':
            pos += 1
            while pos < len(text):
                if text[pos] == "\\":
                    pos += 2
                elif text[pos] == '"':
                    pos += 1
                    break
                else:
                    pos += 1
        elif match := CHARACTER.match(text, pos):
            pos = match.end()
        else:
            pos += 1
            continue
        _mask(code, start, min(pos, len(text)))

    clean = "".join(code)
    # cfg(test) functions, inline modules, and uses are compile-time-only.
    # Do not mask cfg(not(test)) or cfg(any(test, feature = ...)).
    for match in re.finditer(r"#\s*\[", clean):
        start = match.start()
        if code[start] == " ":
            continue
        attr_end = _balanced_end(clean, clean.index("[", start), "[", "]")
        if not CFG_TEST.search(clean[start:attr_end]):
            continue
        item_start = attr_end
        while True:
            item_start = _skip_space(clean, item_start)
            if not clean.startswith("#[", item_start):
                break
            item_start = _balanced_end(clean, item_start + 1, "[", "]")
        paren = bracket = 0
        item_end = len(clean)
        for idx in range(item_start, len(clean)):
            char = clean[idx]
            if char == "(":
                paren += 1
            elif char == ")":
                paren -= 1
            elif char == "[":
                bracket += 1
            elif char == "]":
                bracket -= 1
            elif not paren and not bracket and char == "{":
                item_end = _balanced_end(clean, idx, "{", "}")
                break
            elif not paren and not bracket and char == ";":
                item_end = idx + 1
                break
        _mask(code, start, item_end)
    return "".join(code)


# ---------------------------------------------------------------------------
# Rust item helpers (operate on masked code)
# ---------------------------------------------------------------------------

FN_HEAD = re.compile(
    r"\b(pub(?:\s*\([^)]*\))?)?\s*"
    r"(?:async\s+|unsafe\s+|const\s+|extern\s+\"[A-Za-z]+\"\s+)*"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def fn_ranges(code):
    """`(name, start, body_start, end)` for every `fn` with a `{…}` body."""
    fns = []
    for match in FN_HEAD.finditer(code):
        name = match.group(2)
        depth = 0
        brace = -1
        for pos in range(match.end(), len(code)):
            char = code[pos]
            if char in "([":
                depth += 1
            elif char in ")]":
                depth -= 1
            elif char == "{" and depth == 0:
                brace = pos
                break
            elif char == ";" and depth == 0:
                break
        if brace < 0:
            continue
        end = _balanced_end(code, brace, "{", "}")
        fns.append((name, match.start(), brace, end))
    return fns


def _is_fn_definition(code, match_start):
    """True when `name(` at match_start is the `fn name(` definition head."""
    prefix = code[max(0, match_start - 160):match_start]
    return bool(re.search(r"\bfn\s+$", prefix))


def call_sites(code, name):
    """Positions of non-definition `name(` occurrences in masked code."""
    out = []
    for match in re.finditer(rf"(?<!\w){re.escape(name)}\s*\(", code):
        if not _is_fn_definition(code, match.start()):
            out.append(match.start())
    return out


def _skip_space_back(code, pos):
    while pos > 0 and code[pos - 1].isspace():
        pos -= 1
    return pos


def preceding_attribute_spans(code, pos):
    """`(inner_start, inner_end)` spans of `#[...]` attrs before `pos`.

    Operates on masked code (comments/strings are spaces, so brackets inside
    them can never confuse the walk). Offsets are preserved by masking, so
    the same span sliced out of the *raw* text yields the attribute with its
    string literals intact — needed because `"aarch64"`/`"neon"` are masked.
    """
    spans = []
    cursor = _skip_space_back(code, pos)
    while cursor > 0 and code[cursor - 1] == "]":
        depth = 0
        idx = cursor - 1
        while idx >= 0:
            if code[idx] == "]":
                depth += 1
            elif code[idx] == "[":
                depth -= 1
                if depth == 0:
                    break
            idx -= 1
        if idx < 0:
            break
        hash_pos = _skip_space_back(code, idx)
        if hash_pos <= 0 or code[hash_pos - 1] != "#":
            break
        spans.append((idx + 1, cursor - 1))
        cursor = _skip_space_back(code, hash_pos - 1)
    return spans


X86_DETECT = "is_x86_feature_detected"
TARGET_FEATURE_ATTR = re.compile(
    r"#\s*\[\s*target_feature\s*\(\s*enable\s*=\s*\"([^\"]+)\"")
AARCH64_CFG = re.compile(r'target_arch\s*=\s*"aarch64"')
CFG_ARCH_CALL = re.compile(r"cfg!\s*\(\s*target_arch[^)]*\)")


def target_feature_kernels(raw, code):
    """`{"name", "features", "fn_start"}` per `#[target_feature]` fn.

    Attributes are located in the raw text (feature strings survive there)
    and resolved against masked fn ranges (offsets are identical).
    """
    kernels = []
    for match in TARGET_FEATURE_ATTR.finditer(raw):
        tail = code[match.end():match.end() + 800]
        fn_match = re.search(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)", tail)
        if fn_match is None:
            continue
        name = fn_match.group(1)
        fn_pos = match.end() + fn_match.start()
        # The fn keyword sits a few chars before the name; find the fn range
        # whose head contains this position.
        for fn_name, start, body_start, end in fn_ranges(code):
            if start <= fn_pos < end and fn_name == name:
                kernels.append({
                    "name": name,
                    "features": match.group(1),
                    "fn_start": start,
                })
                break
    return kernels


def simd_guard_violations(code, raw):
    """Violations for the SIGILL/dead-SIMD invariant in one file.

    `code` is masked production source, `raw` the same file unmasked (equal
    length — masking only blanks spans). A call site of a `#[target_feature]`
    kernel is safe when its enclosing fn is a *guarded context*:

    * the fn body performs ``is_x86_feature_detected!`` or calls a same-file
      detector fn (``df4_avx2()``, ``get_4()``, …) — the usual dispatch shape;
    * the fn is itself ``#[cfg(target_arch = "aarch64")]`` or contains a
      ``cfg!(target_arch = "aarch64")`` arm — NEON is architecturally
      baseline there, so no runtime probe is needed;
    * the fn is another target_feature kernel (its own callers are checked);
    * or, transitively, every non-test call site of the fn lives inside an
      already-safe context — this covers the ``batch_cull_into_words_*`` /
      ``unpack_palette_dispatch_*`` intermediate unsafe dispatchers, which
      are only invoked under the runtime probe one frame up.

    A kernel with zero non-test call sites is a dead optimization and also
    fails. Guards are checked on masked code so test-only or commented-out
    probes cannot satisfy them; arch strings are read from `raw` at the same
    offsets because masking blanks string literals.
    """
    kernels = target_feature_kernels(raw, code)
    if not kernels:
        return ["no #[target_feature] kernels found — SIMD path is gone"]
    fns = fn_ranges(code)
    kernel_ids = set()
    for k in kernels:
        for idx, (name, start, _, _) in enumerate(fns):
            if name == k["name"] and start == k["fn_start"]:
                kernel_ids.add(idx)
                break

    detectors = {
        name for name, _, body_start, end in fns
        if X86_DETECT in code[body_start:end]
    }
    detector_re = re.compile(
        r"\b(?:" + "|".join(re.escape(d) for d in sorted(detectors)) + r")\s*\(") \
        if detectors else None

    def has_aarch64_gate(fn_start):
        for inner_start, inner_end in preceding_attribute_spans(code, fn_start):
            if AARCH64_CFG.search(raw[inner_start:inner_end]):
                return True
        return False

    def directly_guarded(idx):
        name, start, body_start, end = fns[idx]
        body = code[body_start:end]
        if X86_DETECT in body:
            return True
        if detector_re is not None and detector_re.search(body):
            return True
        if has_aarch64_gate(start):
            return True
        for match in CFG_ARCH_CALL.finditer(body):
            if AARCH64_CFG.search(raw[body_start + match.start():
                                        body_start + match.end()]):
                return True
        return False

    def enclosing_fn(pos):
        best = None
        for idx, (name, start, _, end) in enumerate(fns):
            if start <= pos < end and (best is None or start > fns[best][1]):
                best = idx
        return best

    # Least fixed point: a fn is a safe context when it is directly guarded,
    # is itself a kernel, or every non-test call site of its name sits inside
    # an already-safe fn (and it has at least one such site — dead helpers
    # must not launder a dead kernel call).
    safe = {idx for idx in range(len(fns))
            if idx in kernel_ids or directly_guarded(idx)}
    changed = True
    while changed:
        changed = False
        for idx, (name, start, _, end) in enumerate(fns):
            if idx in safe:
                continue
            sites = [pos for pos in call_sites(code, name)
                     if enclosing_fn(pos) != idx]
            if sites and all(enclosing_fn(pos) in safe for pos in sites):
                safe.add(idx)
                changed = True

    violations = []
    for kernel in kernels:
        name = kernel["name"]
        sites = call_sites(code, name)
        if not sites:
            violations.append(
                f"{name}: no non-test call site — dead #[target_feature] kernel")
            continue
        if "neon" in kernel["features"]:
            gated = any(
                AARCH64_CFG.search(raw[s:e])
                for s, e in preceding_attribute_spans(code, kernel["fn_start"]))
            if not gated:
                violations.append(
                    f"{name}: neon kernel is not #[cfg(target_arch = \"aarch64\")] "
                    "gated — would not compile / could SIGILL off-ARM")
        for pos in sites:
            enc = enclosing_fn(pos)
            if enc is None or enc not in safe:
                violations.append(
                    f"{name}: call site at offset {pos} is not inside a "
                    "guarded context — SIGILL risk")
    return violations


# ---------------------------------------------------------------------------
# Check table — one entry per optimization invariant.
# ---------------------------------------------------------------------------

def _c(*patterns):
    return {"kind": "contains", "patterns": list(patterns)}


def _call(name):
    return {"kind": "call", "name": name}


CHECKS = [
    # -- Client GPU / rendering hot paths -------------------------------
    {"id": "opt-gpu-compute-strategy",
     "summary": "GPU Indirect / Hi-Z / MDI compute strategy applied per connect",
     "file": WINDOW_RS,
     "requirements": [_call("apply_compute_strategy")]},
    {"id": "opt-gpu-render-into",
     "summary": "pooled UiState::render_into draw list (no per-frame ui.render())",
     "file": WINDOW_RS,
     "requirements": [_call("render_into")]},
    {"id": "opt-external-sections-delta-sync",
     "summary": "external section delta sync (no full-map clone per frame)",
     "file": WINDOW_RS,
     "requirements": [_call("sync_external_sections_from")]},
    {"id": "opt-entity-ber-mesh-upload",
     "summary": "entity + BER instanced meshes uploaded every frame",
     "file": WINDOW_RS,
     "requirements": [_call("set_entity_and_ber_meshes")]},
    {"id": "opt-particle-billboards",
     "summary": "particle billboard quads uploaded every frame",
     "file": WINDOW_RS,
     "requirements": [_call("set_particle_billboards")]},
    {"id": "opt-soa-frustum-cull",
     "summary": "Zen 5 AVX-512 / AVX2 / NEON SoA SIMD frustum culling",
     "file": RENDERER_RS,
     "requirements": [_call("batch_aabb_frustum_cull_soa")]},
    {"id": "opt-depth-store-discard",
     "summary": "TBDR StoreOp::Discard depth selection (Apple Silicon / Adreno)",
     "file": RENDERER_RS,
     "requirements": [
         _call("select_depth_store_op"),
         _c(r"StoreOp\s*::\s*Discard"),
     ]},
    {"id": "opt-shader-f16-subgroup-probe",
     "summary": "SHADER_F16 + SUBGROUP feature probing for shader variants",
     "file": RENDERER_RS,
     "requirements": [
         _c(r"Features\s*::\s*SHADER_F16", r"Features\s*::\s*SUBGROUP"),
     ]},
    {"id": "opt-warp-fallback-adapter",
     "summary": "force_fallback_adapter WARP retry path",
     "file": RENDERER_RS,
     "requirements": [_c(r"\bforce_fallback_adapter\b")]},
    {"id": "opt-section-mesh-offthread-submit",
     "summary": "MeshJob::SectionMesh submitted to off-thread meshing lane",
     "file": LIVE_SESSION_RS,
     "requirements": [_c(r"MeshJob\s*::\s*SectionMesh")]},
    {"id": "opt-section-mesh-offthread-drain",
     "summary": "MeshResult::SectionMesh completions drained into uploads",
     "file": LIVE_SESSION_RS,
     "requirements": [_c(r"MeshResult\s*::\s*SectionMesh")]},
    # -- Client meshing / SIMD ------------------------------------------
    {"id": "opt-pack-strips4-bitboard",
     "summary": "Binary Greedy Meshing 4-strip bitboard packing live",
     "file": MESH_RS,
     "requirements": [_call("pack_strips4")]},
    {"id": "opt-simd-guard-simd-voxel",
     "summary": "core/src/simd_voxel.rs target_feature kernels all guarded",
     "file": SIMD_VOXEL_RS,
     "requirements": [{"kind": "simd_guards"}]},
    {"id": "opt-simd-guard-worldgen",
     "summary": "src/worldgen.rs target_feature kernels all guarded",
     "file": WORLDGEN_RS,
     "requirements": [{"kind": "simd_guards"}]},
    {"id": "opt-simd-guard-generation-math",
     "summary": "src/generation_math.rs target_feature kernels all guarded",
     "file": GENERATION_MATH_RS,
     "requirements": [{"kind": "simd_guards"}]},
    # -- Server hot paths ------------------------------------------------
    {"id": "opt-spawn-descriptor-lazy",
     "summary": "SpawnDescriptor lazy packet materialization on interest gain",
     "file": PLAY_PROTOCOL_RS,
     "requirements": [
         _c(r"\bSpawnDescriptor\s*::"),
         _c(r"\bspawn\s*\.\s*packets\s*\("),
     ]},
    {"id": "opt-interest-chunk-index",
     "summary": "InterestChunkIndex chunk-cell grouping for interest sync",
     "file": PLAY_PROTOCOL_RS,
     "requirements": [_c(r"\bInterestChunkIndex\s*::\s*new")]},
    {"id": "opt-shared-broadcast-frame",
     "summary": "SharedBroadcastFrame encode-once fanout",
     "file": PLAY_PROTOCOL_RS,
     "requirements": [_c(r"\bSharedBroadcastFrame\s*::\s*new")]},
    {"id": "opt-stream-gen-tickets",
     "summary": "stream_gen worker-pool tickets for chunk delivery",
     "file": PLAY_PROTOCOL_RS,
     "requirements": [_c(r"\bstream_gen\s*::")]},
    {"id": "opt-tick-workers",
     "summary": "tick_workers mob plan/env offload on server tick",
     "file": MAIN_RS,
     "requirements": [_c(r"\btick_workers\s*::")]},
]

CHECK_IDS = [check["id"] for check in CHECKS]


# ---------------------------------------------------------------------------
# Audit driver
# ---------------------------------------------------------------------------

def _eval_requirement(requirement, code, raw):
    """Return (ok, detail) for one requirement against masked file text.

    `raw` is the unmasked source at identical offsets — only the simd_guards
    check needs it, to read `"aarch64"`/`"neon"` strings that masking blanks.
    """
    kind = requirement["kind"]
    if kind == "contains":
        missing = [p for p in requirement["patterns"]
                   if not re.search(p, code)]
        if missing:
            return False, "missing pattern(s): " + ", ".join(missing)
        return True, ""
    if kind == "call":
        name = requirement["name"]
        if call_sites(code, name):
            return True, ""
        return False, f"{name}: no non-test call site"
    if kind == "simd_guards":
        violations = simd_guard_violations(code, raw)
        if violations:
            return False, "; ".join(violations)
        return True, ""
    return False, f"unknown requirement kind {kind!r}"


def audit_sources(raw_sources):
    """Evaluate every check against `{relpath: raw rust source}`.

    Missing files mask to empty text, which fails contains/call checks but
    also fails simd_guards (no kernels found) — a deleted file can never
    silently pass.
    """
    masked = {rel: production_code(text)
              for rel, text in raw_sources.items()}
    results = {}
    for check in CHECKS:
        code = masked.get(check["file"], "")
        raw = raw_sources.get(check["file"], "")
        problems = []
        for requirement in check["requirements"]:
            ok, detail = _eval_requirement(requirement, code, raw)
            if not ok:
                problems.append(detail)
        results[check["id"]] = {
            "ok": not problems,
            "file": check["file"],
            "summary": check["summary"],
            "detail": "; ".join(problems),
        }
    return results


def run_audit(root):
    files = sorted({check["file"] for check in CHECKS})
    raw = {}
    for rel in files:
        path = root / rel
        if path.is_file():
            raw[rel] = path.read_text(encoding="utf-8")
    return audit_sources(raw)


def _print_human(results):
    passed = sum(1 for r in results.values() if r["ok"])
    for check in CHECKS:
        result = results[check["id"]]
        status = "PASS" if result["ok"] else "FAIL"
        line = f"{status} {check['id']}  ({result['file']}) — {check['summary']}"
        print(line)
        if not result["ok"]:
            for problem in result["detail"].split("; "):
                print(f"     {problem}")
    total = len(results)
    print(f"opt_wiring_audit: {passed}/{total} invariants wired"
          + ("" if passed == total else f" — {total - passed} REGRESSED"))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=REPO_ROOT)
    parser.add_argument("--json", action="store_true",
                        help="emit the machine-readable report")
    args = parser.parse_args(argv)

    results = run_audit(args.root)
    failed = [cid for cid, r in results.items() if not r["ok"]]

    if args.json:
        print(json.dumps({
            "checks": results,
            "passed": len(results) - len(failed),
            "failed": len(failed),
            "failed_ids": failed,
        }, indent=2, sort_keys=True))
    else:
        _print_human(results)

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

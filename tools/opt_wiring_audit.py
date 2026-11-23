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
SUBWORLD_RENDER_RS = "client-gpu/src/subworld_render.rs"
MESH_RS = "client-port/src/mesh.rs"
SIMD_VOXEL_RS = "core/src/simd_voxel.rs"
WORLDGEN_RS = "src/worldgen.rs"
GENERATION_MATH_RS = "src/generation_math.rs"
PLAY_PROTOCOL_RS = "src/play_protocol.rs"
MAIN_RS = "src/main.rs"
CONNECTION_LIMITS_RS = "src/connection_limits.rs"
LIVE_MOD_BRIDGE_RS = "src/live_mod_bridge.rs"
PROTO_WIRE_RS = "proto/src/wire.rs"
WORLD_INTEROP_RS = "state-store/src/world_interop.rs"
REGION_SCHEDULER_RS = "mod-offload/src/region_scheduler.rs"
FACTORY_RS = "create-adapter/src/factory.rs"
GZIP_RS = "client-kernel/src/gzip.rs"
PACK_LOADER_RS = "server-files/src/pack_loader.rs"
ANVIL_INSPECT_RS = "state-store/src/anvil_inspect.rs"
VISIBILITY_RS = "client-port/src/visibility.rs"
PNG_DECODE_RS = "client-port/src/png_decode.rs"
INFLATE_RS = "client-port/src/inflate.rs"
PALETTED_RS = "src/paletted.rs"
LIGHTING_RS = "src/lighting.rs"
ENTITY_SPATIAL_RS = "src/entity_spatial.rs"
SERVER_RELIABILITY_RS = "src/server_reliability.rs"
MENUS_AND_ITEMS_RS = "src/menus_and_items.rs"
COMBAT_AND_HEALTH_RS = "src/combat_and_health.rs"

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
    # -- client-port SIMD/math kernels ------------------------------------
    {"id": "opt-visibility-non-air-bitmask",
     "summary": "visibility graph packs closed cells via build_non_air_bitmask_64",
     "file": VISIBILITY_RS,
     "requirements": [_call("build_non_air_bitmask_64")]},
    {"id": "opt-png-palette-4bit-unpack",
     "summary": "PNG indexed decode uses unpack_palette_4bit_to_u16",
     "file": PNG_DECODE_RS,
     "requirements": [_call("unpack_palette_4bit_to_u16")]},
    {"id": "opt-mesh-light-merge-adaptive",
     "summary": "mesher merges block/sky nibbles via max_nibble_arrays_adaptive",
     "file": MESH_RS,
     "requirements": [_call("max_nibble_arrays_adaptive")]},
    {"id": "opt-client-port-zlib-consumed-inflate",
     "summary": "client-port zlib wrapper uses inflate_raw_rfc1951_with_consumed",
     "file": INFLATE_RS,
     "requirements": [_call("inflate_raw_rfc1951_with_consumed")]},
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
    # -- Wave 4/5 client rendering ---------------------------------------
    {"id": "opt-ao-lut-branchless-shading",
     "summary": "branchless L1 AO LUT (AO_LUT) live in mesh shading",
     "file": MESH_RS,
     "requirements": [
         _c(r"\bAO_LUT\s*\["),
         _call("shade_fast"),
     ]},
    {"id": "opt-subworld-lit-wgsl-f16",
     "summary": "f16 64-light lit shader variant (lit_wgsl_f16) wired",
     "file": SUBWORLD_RENDER_RS,
     "requirements": [
         _call("lit_wgsl_f16"),
         _c(r"\bLIT_FRAGMENT_F16_WGSL\b"),
     ]},
    {"id": "opt-renderer-lit-wgsl-f16-consumer",
     "summary": "renderer selects lit_wgsl by the SHADER_F16 capability",
     "file": RENDERER_RS,
     "requirements": [
         _call("lit_wgsl"),
         _c(r"\bshader_f16\b"),
     ]},
    {"id": "opt-terrain-vertex-packed-32b",
     "summary": "32B TerrainVertex packed normal/albedo codec live",
     "file": SUBWORLD_RENDER_RS,
     "requirements": [
         _call("pack_normal_octahedral"),
         _call("unpack_normal_octahedral"),
         _call("pack_albedo_unorm8x4"),
         _call("unpack_albedo_unorm8x4"),
         _c(r"size_of\s*::<\s*TerrainVertex\s*>\s*\(\s*\)\s*==\s*32"),
     ]},
    # -- Wave 6 client GPU compact stream + CPU RAM drop -----------------
    {"id": "opt-wave6-compact-vertex-ram-drop",
     "summary": "12B CompactVoxelVertex stream + cpu_mesh_bytes_freed telemetry",
     "file": RENDERER_RS,
     "requirements": [
         _c(r"\bCompactVoxelVertex\b"),
         _c(r"\bcpu_mesh_bytes_freed\b"),
     ]},
    # -- Client math / feedback wiring (L7) -------------------------------
    {"id": "opt-kernel-challenger-probe",
     "summary": "KernelAutotuner challenger probing (dispatch_kernel_mode)",
     "file": LIVE_SESSION_RS,
     "requirements": [_call("dispatch_kernel_mode")]},
    {"id": "opt-ducb-select-arm",
     "summary": "DiscountedUcbAutotuner select_arm consumed each poll",
     "file": LIVE_SESSION_RS,
     "requirements": [_call("select_arm")]},
    {"id": "opt-prefetch-boost-map",
     "summary": "SpeculativeChunkPrefetcher boost map consumed in drain",
     "file": LIVE_SESSION_RS,
     "requirements": [_call("prefetch_boost_map")]},
    {"id": "opt-frame-timing-push-tick",
     "summary": "FrameTimingStats push_tick telemetry producer live",
     "file": LIVE_SESSION_RS,
     "requirements": [_call("push_tick")]},
    # -- Server perf ------------------------------------------------------
    {"id": "opt-sharded-chunks",
     "summary": "64-shard cache-line RwLock chunk maps (ShardedChunks)",
     "file": MAIN_RS,
     "requirements": [
         _c(r"struct\s+ShardedChunks"),
         _c(r"ShardedChunks\s*::\s*new"),
         _call("chunk_shard_of"),
         _c(r"MAP_SHARDS\s*:\s*usize\s*=\s*64"),
     ]},
    {"id": "opt-refresh-hot-sig-skip",
     "summary": "O(1) idle refresh_hot skip via HotSig signature",
     "file": MAIN_RS,
     "requirements": [
         _c(r"struct\s+HotSig"),
         _c(r"sig\s*:\s*Option\s*<\s*HotSig"),
         _c(r"sig\.warm_radius\s*==\s*warm_radius"),
         _c(r"sig\.centers\s*==\s*centers"),
         _c(r"sig\.forced\s*==\s*forced_set"),
         _c(r"\bHOT_REBUILDS\b"),
     ]},
    {"id": "opt-zlib-scratch-pool",
     "summary": "per-thread ZlibCompress/DecompressScratch in encode/decode",
     "file": CONNECTION_LIMITS_RS,
     "requirements": [
         _c(r"struct\s+ZlibCompressScratch"),
         _c(r"struct\s+ZlibDecompressScratch"),
         _call("with_zlib_scratch"),
         _call("with_zlib_inflate"),
         _call("compress_at"),
         _call("inflate_exact"),
     ]},
    {"id": "opt-live-server-budget-scale",
     "summary": "LIVE_SERVER_BUDGET_SCALE PID mirror + getter live",
     "file": LIVE_MOD_BRIDGE_RS,
     "requirements": [
         _c(r"\bLIVE_SERVER_BUDGET_SCALE\b"),
         _c(r"LIVE_SERVER_BUDGET_SCALE\s*\.\s*store"),
         _c(r"fn\s+live_server_budget_scale"),
     ]},
    {"id": "opt-scaled-tick-budget",
     "summary": "live_scaled_tick_budget sheds interest burst by PID scale",
     "file": PLAY_PROTOCOL_RS,
     "requirements": [
         _call("live_scaled_tick_budget"),
         _call("live_server_budget_scale"),
     ]},
    # -- L6 canonical server bits / light / Morton / CRC ---------------
    {"id": "opt-paletted-core-bits",
     "summary": "paletted section storage uses megu_core::bits primitives",
     "file": PALETTED_RS,
     "requirements": [_c(r"megu_core::bits::")]},
    {"id": "opt-lighting-morton-lightarray",
     "summary": "lighting patch order + packed sections use Morton/LightArray",
     "file": LIGHTING_RS,
     "requirements": [
         _call("morton3_encode_u4"),
         _c(r"megu_core::bits::"),
     ]},
    {"id": "opt-entity-spatial-bitmask-morton",
     "summary": "entity spatial index uses bitmask occupancy + Morton order",
     "file": ENTITY_SPATIAL_RS,
     "requirements": [
         _call("build_non_air_bitmask_64"),
         _call("morton2_encode_i16"),
     ]},
    {"id": "opt-server-reliability-core-crc32",
     "summary": "delta WAL frames use canonical crc32_ieee_update",
     "file": SERVER_RELIABILITY_RS,
     "requirements": [
         _call("crc32_ieee_update"),
         _call("crc32_ieee_parts"),
     ]},
    # -- Stage 2 sleeping furnaces / live Mixin dispatch ------------------
    {"id": "opt-stage2-furnace-due-queue",
     "summary": "furnaces sleep in FURNACE_DUE_QUEUE behind wake_furnace_at",
     "file": MENUS_AND_ITEMS_RS,
     "requirements": [
         _c(r"\bFURNACE_DUE_QUEUE\b"),
         _c(r"\bfn\s+wake_furnace_at\b"),
         _call("furnace_due_queue"),
     ]},
    {"id": "opt-live-mod-entity-damage-pre",
     "summary": "combat damage paths dispatch live_mod_entity_damage_pre",
     "file": COMBAT_AND_HEALTH_RS,
     "requirements": [_call("live_mod_entity_damage_pre")]},
    # -- Canonical DoS-proof NBT wire skipper ------------------------------
    {"id": "opt-nbt-wire-skip-play",
     "summary": "play_protocol NBT skipping delegates to megu_core::nbt_wire",
     "file": PLAY_PROTOCOL_RS,
     "requirements": [
         _call("nbt_wire_skip"),
         _call("nbt_wire_value"),
         _call("skip_nbt_payload"),
     ]},
    {"id": "opt-nbt-wire-skip-proto",
     "summary": "proto wire reader uses canonical skip_nbt_payload",
     "file": PROTO_WIRE_RS,
     "requirements": [_call("skip_nbt_payload")]},
    {"id": "opt-nbt-wire-skip-world-interop",
     "summary": "world_interop raw-NBT skipper uses canonical skip_nbt_payload",
     "file": WORLD_INTEROP_RS,
     "requirements": [_call("skip_nbt_payload")]},
    # -- Resident task pool / parallel region stepping ---------------------
    {"id": "opt-bg-task-pool",
     "summary": "resident MeguTaskPool bg_scope! dispatch in every build",
     "file": WORLDGEN_RS,
     "requirements": [
         _c(r"\bMeguTaskPool\b"),
         _c(r"\bbg_task_pool\b"),
         _c(r"bg_scope!\s*\("),
     ]},
    {"id": "opt-bg-pool-prefetch-consumers",
     "summary": "main.rs terrain prefetch consumes the bg pool batch API",
     "file": MAIN_RS,
     "requirements": [
         _call("prefetch_cols_with"),
         _call("prefetch_terrain"),
     ]},
    {"id": "opt-awake-region-parallel-step",
     "summary": "selective awake+due region parallel step with idle sleep",
     "file": REGION_SCHEDULER_RS,
     "requirements": [
         _call("advance_active_parallel_for"),
         _call("refresh_region_sleep"),
         _c(r"\bWorkerPool\b"),
         _c(r"\bawake\b"),
         _c(r"\bsleeping\b"),
     ]},
    {"id": "opt-create-factory-parallel-step",
     "summary": "CreateFactory step_tick_parallel -> advance_active_parallel_for",
     "file": FACTORY_RS,
     "requirements": [
         _call("advance_active_parallel_for"),
         _call("step_tick_parallel"),
         _c(r"\bidle_export_skips\b"),
     ]},
    {"id": "opt-create-factory-server-step",
     "summary": "server live_create tick steps CreateFactory regions",
     "file": LIVE_MOD_BRIDGE_RS,
     "requirements": [_call("step_tick")]},
    # -- Canonical core inflate ------------------------------------------------
    {"id": "opt-gzip-core-inflate",
     "summary": "client-kernel gzip uses inflate_raw_rfc1951_with_consumed",
     "file": GZIP_RS,
     "requirements": [_call("inflate_raw_rfc1951_with_consumed")]},
    {"id": "opt-pack-loader-core-inflate",
     "summary": "server-files pack loader uses inflate_raw_rfc1951_with_consumed",
     "file": PACK_LOADER_RS,
     "requirements": [_call("inflate_raw_rfc1951_with_consumed")]},
    {"id": "opt-anvil-inspect-core-inflate",
     "summary": "state-store anvil inspector uses inflate_raw_rfc1951_with_consumed",
     "file": ANVIL_INSPECT_RS,
     "requirements": [_call("inflate_raw_rfc1951_with_consumed")]},
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

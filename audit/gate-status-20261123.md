# Gate-status + lint survey — 2026-11-23

Repo: `<repo>` @ `8b00b7d2` (megu-mc, clean-room Minecraft 1.21.1).
Read-only survey: no source modified, no `cargo test` run, no commits.

## 1. `python3 tools/dev_check.py` — exit 0

Summary line: `7 OK, 2 MISSING, 2 NOT_CHECKED, 0 ERROR` → `READY_FOR_PYTHON_CHECKS`.

Per-check results:

| check | status | detail |
|---|---|---|
| python | OK | 3.14.10 |
| toolchain_pin | OK | 1.98.0 per rust-toolchain.toml |
| toolchain_installed | OK | 1.98.0-x86_64-unknown-linux-gnu (rustfmt/clippy components NOT verified) |
| build_inputs | OK | 130 include sites resolve across 113 src/*.rs |
| runtime_data | OK | assets (10), tests/data (100), datapacks/vanilla (2350), config_packets.bin (105095 B) |
| **crate_cache** | **NOT_CHECKED** | "not inspected — a crate cache can only be inspected by starting cargo, which may fetch or install" |
| **vanilla_jar** | **MISSING** | no file at `~/.cache/megu-audit/load/vanilla/server.jar` (jar-side oracle only; never auto-downloaded) |
| jdk | **NOT_CHECKED** | javac 27.0.1.1 present, but generator documents Java 21; compatibility "not established" |
| **depsgx** | **MISSING** | not on PATH; `tests/layers_gate_test.py` and `tools/layers_check.sh` fail without it (CI pins GXKIT_REV `d6b9ad8d…`) |
| workspace | OK | 12 crates + `[profile.fast]` + 3 workspace files; standalone-crate check now covers 10 crates named (see note below) |
| disk_headroom | OK (warning) | target 75.2 GiB free; **/tmp only 1.0 GiB free — under 5 GiB** (non-fatal unless `MEGU_DEV_CHECK_DISK_FATAL=1`) |

Routes:

- `python/fixtures`: **READY**
- `rust build/test`: **READY** (prerequisites inspected only; crate cache NOT_CHECKED)
- `layers/dependency gate`: **NOT READY** (depsgx MISSING)
- `jar/native oracle`: **NOT READY** (vanilla_jar MISSING, jdk NOT_CHECKED)

Explicit skip/not-run lines: the four bold rows above; plus the tool's own disclaimer "a green preflight is not a green suite: this inspects inputs, not behaviour."

Note: `dev_check.py`'s `STANDALONE_CRATES` still names 10 crates — `client-ui` and `client-gpu` are absent from its workspace check (flagged in `audit/remaining-gaps-20261122.md` §44 and still true).

## 2. `python3 tools/wiring_audit.py` — exit 0

Config files read first: `tools/wiring_audit_allowlist.json` (`version` + `entries`) and `tools/wiring_audit_features.json` (per-symbol consumer maps, e.g. `BUILTIN_SHADOW_SHADERS` → 5 consumers).

Output (complete, 15 lines):

```
Module wiring: 281/294 wired, 13 unwired
Wiring contracts: 17/17 wired, 0 pending
UNWIRED module proto/src/channel_router.rs: no production consumers
UNWIRED module proto/src/resolver.rs: no production consumers
UNWIRED module proto/src/session.rs: no production consumers
UNWIRED module proto/src/voice_relay.rs: no production consumers
UNWIRED module state-store/src/audit_log.rs: no production consumers
UNWIRED module state-store/src/world_interop.rs: no production consumers
UNWIRED module server-files/src/claims_wire.rs: no production consumers
UNWIRED module server-files/src/installer.rs: no production consumers
UNWIRED module server-files/src/mods_dir.rs: no production consumers
UNWIRED module server-files/src/pack_loader.rs: no production consumers
UNWIRED module mod-offload/src/ffi.rs: no production consumers
UNWIRED module client-port/src/world.rs: no production consumers
UNWIRED module client-kernel/src/ffi.rs: no production consumers
```

Verdict: **PASS (exit 0)** but informational — 13 modules have zero production consumers and the gate still exits 0. Whether those 13 are allowlisted or simply reported-and-ignored should be checked against `wiring_audit_allowlist.json` entries; the script does not fail on them.

## 3. `bash tools/debt_ratchet.sh` — exit 0

```
ok    wall-clock reads (clock RNG must not return): 0
ok    ignored tests (parked gates): 1
ok    lock/rwlock unwraps (no new poison cascades): 8 / 8

debt ratchet: 0 over limit
```

Verdict: **PASS**. Counts: 0 wall-clock reads, **1 ignored/parked test gate** (a deliberately parked check — see "silently blind"), 8/8 lock unwraps at limit.

## 4. `python3 tools/provenance_scan.py` — exit 1 (usage, not a verdict)

With no arguments it prints its usage/help text and exits 1. It is an ad-hoc pairwise code-similarity probe (`provenance_scan.py <ours.rs> <theirs_dir_or_file> [--top N]`), **not a runnable gate** — it requires a named comparison pair. Its sibling `tools/provenance_check.py` (used by CI with `--strict-zero-gl`) is the actual gate and was not part of this survey's command list.

## 5. `python3 tools/check_layers.py` — exit 1, FAIL

```
layers check: FAIL: depsgx is required; an absent inspector cannot satisfy the gate.
Install the pinned revision (d6b9ad8db4774919c8b4fb032342e53b2548b660) as CI does,
set MEGU_DEPSGX to an existing build, or put one on PATH
```

Verdict: **FAIL / cannot run locally** — depsgx not installed. Fail-closed by design (good), but locally this gate contributes zero coverage until GXKIT is installed.

## 6. `cargo clippy --all-targets` (root crate `megu`, dev profile, `--locked`)

Finished in 30.49 s (incremental artifacts present). Exit 0 — warnings only, no `-D warnings`.

Totals: `megu (bin "megu")` **42 warnings**; `megu (bin "megu" test)` **44 warnings (42 duplicates)** → 2 test-only warnings.

### Warnings per file (top 15; only 9 files have any)

| count | file |
|---|---|
| 21 | src/main.rs |
| 8 | src/play_protocol.rs |
| 6 | src/menus_and_items.rs |
| 3 | src/chest_loot.rs |
| 2 | src/teams_and_commands.rs |
| 1 | src/operator_ux.rs |
| 1 | src/mob_behaviors.rs |
| 1 | src/commands_core.rs |
| 1 | src/combat_and_health.rs |

### `src/nbt.rs`, `src/paletted.rs`, `src/play_protocol.rs`, `src/main.rs`

- **src/nbt.rs: 0 warnings.** **src/paletted.rs: 0 warnings.** (Clean under default lints — but see cast pass below: paletted.rs has 25 cast warnings, nbt.rs 5.)
- **src/play_protocol.rs (8):** `very_complex_type` ×5 (lines 756, 853, 1694, 6144, 9664); `redundant_closure` ×2 (7321, 7324 — `Default::default`); `repeat().take()` → `repeat_n()` (18201, test target only).
- **src/main.rs (21):** `very_complex_type` ×5 (9450, 10513, 11596, 17811, 17812); `too_many_arguments` ×1 (5375, 10/7); immediate-deref ×4 (5801, 15121, 15140, 15476, 15478 — actually 5 sites); `map_or` simplifiable ×2 (9692, 9921); manual `.is_multiple_of()` ×2 (9697→32, 14511→64); `* -1` ×2 (11457, 11458); identical `if` blocks (11778); redundant closure (11635); manual `!Range::contains` (12634); collapsible `if` (14568).

### Focused cast pass: `-W clippy::cast_possible_truncation -W clippy::cast_sign_loss`

`megu (bin "megu" test)` generated **3900 warnings**; `megu (bin "megu")` 2904 (duplicates) → **~3856 unique cast-lint lines** across the crate. This is the candidate protocol-bug surface.

Top files by cast-warning count:

| count | file |
|---|---|
| 511 | src/main.rs |
| 394 | src/worldgen.rs |
| 250 | src/play_protocol.rs |
| 249 | src/menus_and_items.rs |
| 161 | src/combat_and_health.rs |
| 150 | src/feature.rs |
| 107 | src/save_v2.rs |
| 98 | src/registry_tests.rs |
| 98 | src/generation_math.rs |
| 94 | src/tick_tests.rs |
| 89 | src/teams_and_commands.rs |
| 88 | src/commands_core.rs |
| 79 | src/villager_raid.rs |
| 77 | src/tree.rs |
| 77 | src/template_structures.rs |

Targeted files: `src/paletted.rs` **25**, `src/nbt.rs` **5**, `src/play_protocol.rs` **250**.

First ~60 cast warnings (file:line, in clippy output order):

```
src/chunk_ack.rs:85:26:        f32 -> usize truncate + sign-loss
src/connection_limits.rs:140:21: f64 -> i16 truncate
src/connection_limits.rs:260:44: i32 -> u32 sign-loss (window.radius)
src/connection_limits.rs:1333:23: u128 -> u64 truncate
src/connection_limits.rs:1542:26: i32 -> usize sign-loss
src/connection_limits.rs:1869:22: usize -> u32 truncate (64-bit)
src/connection_limits.rs:1956:43: usize -> i32 truncate (64-bit)
src/connection_limits.rs:1959:22: usize -> u8 truncate
src/connection_limits.rs:2083:60: u32 -> u8 truncate
src/connection_limits.rs:2088:22: i32 -> usize sign-loss
src/connection_limits.rs:2092:24: i32 -> usize sign-loss
src/connection_limits.rs:2115:20: i32 -> usize sign-loss
src/connection_limits.rs:2182:30: usize -> u8 truncate
src/connection_limits.rs:2222:64: u32 -> u8 truncate
src/effects.rs:62:22:           usize -> i32 truncate (64-bit)
src/effects.rs:187:34:          i32 -> u32 sign-loss (amplifier)
src/effects.rs:249:23:          i32 -> u32 sign-loss (amplifier)
src/effects.rs:379:26:          u64 -> i32 truncate
src/effects.rs:388:26:          u64 -> i32 truncate
src/effects.rs:395:26:          u64 -> i32 truncate
src/effects.rs:459:68:          u64 -> i32 truncate
src/antixray.rs:63:29:          i32 -> usize sign-loss
src/antixray.rs:83:33:          i32 -> usize sign-loss
src/landing.rs:137:14:           usize -> u8 truncate
src/landing.rs:163:27,46,65:     f64 -> i32 truncate ×3
src/landing.rs:183:21,39,65:     f64 -> i32 truncate ×3
src/landing.rs:195:21:           f64 -> f32 truncate
src/landing.rs:197:22:           usize -> u8 truncate
src/landing.rs:200:27:           f64 -> f32 truncate
src/landing.rs:220:22:           f64 -> f32 truncate
src/landing.rs:277:18:           usize -> u8 truncate
src/landing_types.rs:563:32:     usize -> u32 truncate (64-bit)
src/landing_types.rs:775:21:     usize -> u16 truncate
src/landing_types.rs:803:23:     usize -> u8 truncate
src/landing_types.rs:806:18:     usize -> u8 truncate
src/landing_types.rs:879:28:     usize -> u16 truncate
src/landing_types.rs:894:32:     usize -> u16 truncate
src/landing_types.rs:900:32:     usize -> u32 truncate (64-bit)
src/landing_types.rs:991:22:     i32 -> u32 sign-loss (wire[0])
src/landing_types.rs:1116:13:    usize -> i32 truncate (64-bit)
src/landing_types.rs:1117:13:    usize -> i32 truncate (64-bit)
src/landing_types.rs:1157:18:    usize -> i32 truncate (64-bit)
src/landing_types.rs:1187:27:    usize -> u16 truncate
src/landing_types.rs:1195:28:    i32 -> u16 truncate + sign-loss
src/landing_types.rs:1236:31:    usize -> u32 truncate (64-bit)
src/light_pack_tests.rs:12:35:   usize -> i32 truncate (64-bit) [test]
src/light_pack_tests.rs:34:29:   i32 -> u8 truncate [test]
src/light_pack_tests.rs:43:44:   usize -> i32 truncate (64-bit) [test]
src/light_pack_tests.rs:48:50,74: usize -> i32 truncate ×2 [test]
src/light_pack_tests.rs:80:19:   i32 -> usize sign-loss [test]
src/light_pack_tests.rs:81:18,37: i32 -> usize sign-loss + usize -> i32 truncate [test]
src/light_pack_tests.rs:109:19:  i32 -> usize sign-loss [test]
```

Notable concentrated clusters for protocol review: `landing_types.rs` wire-encode/decode helpers (many `usize`→fixed-width and `i32`→`u16`/`u32` casts on wire paths), `connection_limits.rs` (radius/window arithmetic), `paletted.rs` (palette index `u64`→`u32`/`u8`, `i64`→`usize`), `nbt.rs` (`i32`→`usize` length reads, `i8`→`u8` tag payloads). Most are intentional-width wire encodings, but ~3.9k unchecked casts is far beyond what anyone can eyeball — none are denied by CI (see below).

## 7. `.github/workflows/ci.yml` — standalone sub-crate tests: **now present**

The `remaining-gaps-20261122.md` claim ("ci.yml には独立12クレートのテストが無く wire テストも 9/24 本のみ") is **partially stale**:

- **Standalone crates: FIXED.** The `Standalone crates (12 crates, locked + Clippy -D warnings)` step runs `cargo clippy --all-targets --locked -- -D warnings` **and** `cargo test --locked` for all of `core proto state-store server-files mod-offload mod-adapter-core create-adapter client-kernel client-port client-ui client-app`, plus clippy+check for `client-gpu` (no `cargo test` for client-gpu — check only).
- **Wire tests: still a gap.** `tests/` contains **27** `*wire*.py` files; ci.yml runs **13**:

  `wire_smoke.py`, `dimension_wire.py`, `client_settings_wire.py`, `gameplay_wire.py`, `m2_gameplay_wire.py`, `container_wire.py`, `gp5_gp7_wire.py`, `gp3_gp4_gp5_wire.py`, `gp1_gp6_wire.py`, `gp2_nether_spawn_wire.py`, `command_brigadier_ops_wire.py`, `w1_w3_live_wire.py`, `live_parity_e2e_wire.py`

  **Not in ci.yml:** `client_inventory_pickup_wire.py`, `client_kernel_wire.py`, `client_mesh_wire.py`, `client_oracle_fuzz_wire.py`, `client_prediction_wire.py`, `command_datapack_wire.py`, `creative_parity_wire.py`, `magic_vfx_anim_live_wire.py`, `mod_suite_live_wire.py`, `multi_mod_e2e_wire.py`, `multiplayer_stress_wire.py`, `neoforge_live_server_wire.py`, `network_limits_wire.py`, `protocol_fuzz_wire.py` (14 files).

  `tests/verify.sh` (the long local pre-merge gate) covers most of those — `command_datapack_wire`, `multi_mod_e2e_wire --self-test`, `magic_vfx_anim_live_wire --self-test`, `mod_suite_live_wire --self-test`, `client_prediction_wire`, `client_inventory_pickup_wire`, `client_kernel_wire`, `client_mesh_wire` — but **still omits 6 wire tests entirely**: `client_oracle_fuzz_wire.py`, `protocol_fuzz_wire.py`, `multiplayer_stress_wire.py`, `network_limits_wire.py`, `neoforge_live_server_wire.py`, `creative_parity_wire.py`. Those six run in neither CI nor the canonical local gate.

## Gates that are silently blind

- **`tools/provenance_scan.py`** — exits 1 with usage when run bare; it is a manual pairwise probe, not a gate. Any "provenance scan passed" claim without named file pairs is vacuous. The real gate is `provenance_check.py --strict-zero-gl` (CI only).
- **`tools/check_layers.py` / `tools/layers_check.sh`** — locally FAIL-CLOSED on missing depsgx, so no silent pass; but on any machine where depsgx *is* on PATH from an unpinned build, the layer check runs against an inspector CI never tested (CI builds GXKIT_REV `d6b9ad8d…` in a venv). Local green ≠ CI green here.
- **`tools/dev_check.py`** — three NOT_CHECKED/MISSING items are prerequisites it cannot verify: crate cache (first offline build unproven), vanilla_jar, jdk-vs-21. A `READY_FOR_PYTHON_CHECKS` result says nothing about the Rust build actually compiling offline.
- **`tools/wiring_audit.py`** — exit 0 with **13 production-dead modules** reported as UNWIRED. Unless the allowlist explicitly blesses each, this is a report-only gate: it cannot fail on new dead modules unless the allowlist diff logic does (verify `wiring_audit_allowlist.json` `entries` semantics before trusting the 0).
- **`tools/debt_ratchet.sh`** — reports `ignored tests (parked gates): 1`. One gate is deliberately `#[ignore]`d; the ratchet caps the *count* at 1, so a second parked gate fails, but the existing parked gate itself is never exercised by any run.
- **`cargo clippy` (as run by this survey)** — root-crate clippy is warning-only; nothing denies warnings. The standalone-crate CI step *does* use `-D warnings`, but the root `megu` crate has **no clippy gate at all** in ci.yml — 42 default warnings and ~3.9k cast-lint warnings accumulate unchecked.
- **`client-gpu`** — CI runs clippy + `cargo check` but **no `cargo test`** for this crate; its tests (if any) never run anywhere.
- **`/tmp` disk headroom** — dev_check warns tmp has only 1.0 GiB free; any gate writing temp artifacts (fixture suites, mutation checks) can fail for environmental reasons that look like code failures.

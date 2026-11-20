# Issue #43 — Incremental lighting E3-3 (2026-11-20)

## Implementation and ownership

`src/lighting.rs` provides a clean-room, independently testable light owner for a
loaded-chunk window. `edit` darkens the affected 14-block Manhattan-radius area,
then relights from surviving emitters, direct skylight, and boundary light. Both
light layers cross loaded chunk and section boundaries. Unloaded chunks bound
propagation. Updates report changed sky/block sections, empty-section masks, and
low-nibble-first 2048-byte payloads. Loading or unloading a chunk rebuilds its
connected window. The caller supplies emission and opacity values. No Torch AGPL
or Mojang lighting source was copied.

The checked-in `src/main.rs` has no `mod lighting` declaration. Consequently,
Cargo checks and tests do **not** compile the new module. Independent `rustc`
compilation with `-D warnings` and 12 Rust unit tests verify this implementation.

## Vanilla 1.21.1 comparison

Reference jar `/home/yuu/tmpx/carverfresh/server.jar`, SHA-1
`59353fb40c36d304f2035d51e7d6e6baa98dc05c`. The clean-room Python probe
starts the pinned server with a flat Overworld (`min_y=-64`, height 384),
forceloads a 3×3 chunk area, models the four-layer opaque floor, and places a
supporting stone block at `(8,-41,8)`. Its 5×5 stone roof spans `x,z=6..10` at
`y=-40`, with an optional central opening `(8,-40,8)`.

Ten snapshots were checked in sequence: air, solid roof, roof with opening,
water, ice, persistent oak leaves, oak slab, oak stairs, torch, and lantern.
Registry data pinned to the same jar confirms emission 14 for torch and 15 for
lantern, attenuation 1 for water/leaves/ice, and default scalar opacity 0 for
slab/stairs. All ten initial chunk packet comparisons passed. Their native sky
mask was `0xe`, while block mask was `0x0` except for torch and lantern (`0xe`).
All **30** transmitted skylight section arrays and all **6** transmitted
emitter block-light section arrays matched the local reference byte-for-byte.
All attempted in-world edits delivered a block-update packet to a tracking
client. The fixture checks initial chunk snapshots after each edit.

Native **Update Light** packets for target chunk `(0,0)` were not reliably
observed in the edit-tracking window, so update packet mask parity remains
**UNVERIFIED**. An earlier exploratory emitter-removal snapshot retained the
native `0xe` block mask immediately after clearing a lantern while the local
engine had no nonzero block arrays. This disagreement requires a settled native
removal capture and a comparison of empty-section masks before resolution.
The initial sky packet's omitted implicit upper sections were not compared.

## Incremental unit fixtures

Twelve standalone tests cover torch/lantern spread, generated and placed source
removal with surviving-source relight, exact changed/empty section masks at
`y=15/16`, lateral skylight underhangs, repeated roof placement/removal, loaded
neighbor chunks at positive and negative `x/z` boundaries, opacity 0/1/2/3/15,
repeated emitter toggles and no-op edits, unloaded neighbors, alternate paths,
and underhangs across both chunk and section boundaries. A fixed sequence of
48 mixed edits asserts exact sky/block maps against a full rebuild and exact
section masks and payloads. Slab/stair **directional face occlusion** is not
modeled by this scalar-opacity engine and needs a native directional fixture.

## Verification

| Command | Observed result |
| --- | --- |
| `rustfmt --check src/lighting.rs` | PASS |
| `rustc --edition 2021 --test -D warnings src/lighting.rs -o target/tmp/lighting-engine-tests` and standalone test binary | PASS, 12 passed, 0 failed (14.93 s) |
| `cargo test --offline --locked -- light --test-threads=1` | PASS, 24 passed, 0 failed, 1005 filtered out (3.28 s) |
| `cargo check --offline --locked --tests` | PASS, no warnings (0.02 s) |
| `python3 tests/lighting_parity_test.py` with `TMPDIR=$PWD/target/tmp` | PASS, ten fixtures, 30 sky arrays and six block arrays matched |
| `bash tools/layers_check.sh` | PASS, 44 modules, 111 edges, 0 named errors |
| `bash tools/debt_ratchet.sh` | PASS, 0 over limit |
| `python3 tools/provenance_check.py` | PASS, provenance clean |

## Pending integration in the `src/main.rs` lane

Introduce module ownership and initialization, feed generated emitters and
`BReg` emission/opacity to chunk loading and block edits, handle loaded chunk
life cycles, and publish incremental `Update Light` packets to clients holding
affected chunks. The current owner retains the paths
`set_block_if_inner_dim` (~9820), `block_light_sections_from_dim` (~17718),
`sky_light_sections_world` (~18936), `update_light_packet_dim` (~19907),
`light_dirty_for_dim` (~19949), and the update loop (~20200). Verify dynamic
emitter removal, directional slab/stair behavior, and native update/empty masks
after that wiring. The lane ownership restriction leaves `src/main.rs` untouched.

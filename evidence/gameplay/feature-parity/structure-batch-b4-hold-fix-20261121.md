# Wave 5 B4 structure HOLD investigation (2026-11-21)

All three [Batch B4](structure-batch-b4-20261120.md) fixtures remain **HOLD**. The port was rebuilt from `3aa87c689cc822a9b215be8fb9b253854e33d528` with `cargo build --offline --locked -j 1`. The binary SHA-256 was `60722c3f5e33760c2722f7a56dba9902abf56f3473413e0aa433e186f9a2441b`. The comparisons used the original frozen Java 1.21.1 captures, seed `4242`, Overworld, and all `98,304` typed cells per chunk at Y `-64..319`. The pinned Java server jar SHA-1 was `59353fb40c36d304f2035d51e7d6e6baa98dc05c`.

| Structure | Chunk `(cx, cz)` | B4 before | Current default order | Current recorded Vanilla order |
|---|---|---:|---:|---:|
| `minecraft:buried_treasure` | `(126, -182)` | 12 | 12 | 12 |
| `minecraft:swamp_hut` | `(-332, -339)` | 14 | 14 | 4 |
| `minecraft:jungle_pyramid` | `(137, -117)` | 1 | 1 | 1 |

All three current default port grids matched the prior port grids byte-for-byte. The recorded-order swamp grid SHA-256 was `f1a2d9d5a53385ebcf416a4ecbb6952ff55613ef6a345a45af862776876c3da0`. All six `--fail-on-diff` runs returned exit `1`. The three allowed Rust files were inspected, and the measured divergences did not establish a placement fix there.

## First divergent stages and writers

- **Buried treasure.** A fresh frozen terrain-only Vanilla run with structures, carvers, and placed features disabled reproduced exactly the same 12 `(position, native state, port state)` triples as the original full comparison. Native sand and port gravel already differ at `x=2016..2017`, Y `46..47`, `z=-2904..-2897`. The chest at `(2025,57,-2903)` matches. The terrain-only native and port grid SHA-256 values are `7388c8e56cc9dd272e178328fa7bfa4de84616b41529aec9a3164df0feeab726` and `97a3964549c73c1c390b091973f36207ee6f7d738f4324fe467b9c9070a1adbd`. The divergence is present by the terrain stage. The specific surface-rule input that differs is unverified.
- **Swamp hut.** The captured nine-chunk decoration order removes all ten mineshaft corridor differences without a code change. The three lily pad candidates at `(-5310,63,-5413)`, `(-5312,63,-5418)`, and `(-5308,63,-5418)` reach `patch_waterlily` in both implementations. Vanilla writes all three. The port reports `below=minecraft:water at=minecraft:air ok=false` for each. Pinned `WaterlilyBlock.mayPlaceOn` (`dpc.b`) accepts a water source or ice below with empty fluid at the placement position. [`can_place`](../../../src/generation_providers.rs) has no lily-pad arm and uses the dirt/farmland fallback for these candidates. Both grids contain water source state `80` under all three pads.
- **Swamp mushroom.** Both sides write spruce planks at `(-5310,64,-5420)`. `brown_mushroom_swamp` (`58,9`) consults native brightness **15** at `(-5310,65,-5420)` during feature placement, so `MushroomBlock.canSurvive` rejects the candidate. The port's feature-time predicate accepts it and writes state `2089`. The port marks the mushroom and keeps it at promotion. The traced Vanilla rerun reproduced the original swamp grid and `order.tsv` byte-for-byte, including the missing mushroom. The observed feature-time light discrepancy belongs to the survival predicate and its brightness input.
- **Jungle pyramid.** The differing oak leaf at `(2207,72,-1857)` is outside the pyramid's child box `[2192,65,-1872,2203,74,-1858]`. `trees_jungle` (`7,9`) writes leaf states `264` then `252` in pass `(137,-117)` on both sides. The later `trees_jungle` pass `(138,-116)` writes state `252` (distance 4) in Vanilla and state `248` (distance 3) in the port. The saved grids contain those exact final states. The divergence is in the later tree pass. Its first differing leaf-distance input is unverified.

## Reproduction

From the repository root, use the original capture directories and a new output directory for the probes. Raw logs and grids for this run are under `target/b4-hold-fix/`.

```bash
mkdir -p target/tmp
export TMPDIR=$PWD/target/tmp JAVA_TOOL_OPTIONS="-Djava.io.tmpdir=$PWD/target/tmp -XX:-UsePerfData"
cargo build --offline --locked -j 1
captures=<local>/captures
out="$PWD/target/b4-hold-fix-repro"

triage() {
    local label="$1" capture="$2" x="$3" z="$4"
    shift 4
    mkdir -p "$out/$label"
    python3 tools/port_triage.py "$capture/world" "$x" "$z" \
        --n 1 --preset overworld --height-window noise --samples 20 \
        --binary "$PWD/target/debug/mc-spine" --out-dir "$out/$label" \
        --json-out "$out/$label/triage.json" --fail-on-diff "$@"
}

while read -r label capture x z; do
    capture="$captures/$capture"
    triage "baseline/$label" "$capture" "$x" "$z" --mode full
    triage "recorded/$label" "$capture" "$x" "$z" --mode full --pop-order "$capture/order.tsv"
done <<'FIXTURES'
buried-treasure matrix-v3/nat-4242-b3-buried-treasure 126 -182
swamp-hut matrix-v3/nat-4242-b3-swamp-hut -332 -339
jungle-pyramid matrix-b4/nat-4242-b4-jungle-pyramid 137 -117
FIXTURES

export PATH="/usr/lib/jvm/java-21-openjdk/bin:$PATH"
export CARVER_FIXTURE_BASE=<home>/tmpx/carverfresh
export MC_SPINE_SEED=4242
MC_FIXTURE_STAGE=terrain systemd-run --scope --user -p MemoryMax=1800M -p CPUQuota=100% \
    bash tools/carve-agent/native_fixture.sh "$out/vanilla-terrain-buried" 126 -182 1 frozen
triage terrain-port-buried "$out/vanilla-terrain-buried" 126 -182 --mode terrain

MC_FIXTURE_STAGE=structures MC_FIXTURE_JAVA_EXTRA='-Dmushroomchunk=-332,-339 -Dpatchchunk=-332,-339' \
    systemd-run --scope --user -p MemoryMax=1800M -p CPUQuota=100% \
    bash tools/carve-agent/native_fixture.sh "$out/vanilla-swamp-trace" -332 -339 1 frozen
cmp "$out/vanilla-swamp-trace/native.bin" "$captures/matrix-v3/nat-4242-b3-swamp-hut/native.bin"
cmp "$out/vanilla-swamp-trace/order.tsv" "$captures/matrix-v3/nat-4242-b3-swamp-hut/order.tsv"
```

The port diagnostics run the same binary directly because `port_triage.py` clears `FEATURE_*` variables. These traced dumps compared byte-for-byte to their corresponding recorded-order dumps.

```bash
trace() {
    local label="$1" capture="$2" x="$3" z="$4"
    MC_SPINE_WG_PROFILE=0 MC_SPINE_WG_PRESET=overworld \
        MC_SPINE_WG_DUMP="$out/recorded/$label/trace.bin" MC_SPINE_WG_DUMP_WINDOW=noise \
        MC_SPINE_WG_DUMP_X="$x" MC_SPINE_WG_DUMP_Z="$z" MC_SPINE_WG_DUMP_N=1 \
        MC_SPINE_POP_ORDER="$captures/$capture/order.tsv" \
        FEATURE_ATTEMPT_TRACE=1 FEATURE_WRITE_TRACE="$x,$z" \
        SIMPLE_TRACE=1 PATCH_TRACE="$x,$z" POSTPROC_STATES=1 \
        target/debug/mc-spine > "$out/recorded/$label/trace.log" 2>&1
    cmp "$out/recorded/$label/trace.bin" "$out/recorded/$label/dump.bin"
}
trace swamp-hut matrix-v3/nat-4242-b3-swamp-hut -332 -339
trace jungle-pyramid matrix-b4/nat-4242-b4-jungle-pyramid 137 -117
```

## Validation

| Command | Observed result |
|---|---|
| `cargo test --offline --locked -j 1 -- single_piece structure_pipeline --test-threads=1` | 6 passed. The literal `structure_pipeline` filter matches zero test names. |
| `cargo test --offline --locked -j 1 -- feature::pipeline_tests --test-threads=1` | 17 passed, including the live placement and quart biome tests. |
| `cargo check --offline --locked -j 1 --all-targets` | Exit `0`, zero warnings. |
| `bash tools/layers_check.sh` | PASS, 44 modules and 111 edges with zero named errors. |
| `python3 tools/provenance_check.py` | Clean. |
| `git diff --check` | Clean. |

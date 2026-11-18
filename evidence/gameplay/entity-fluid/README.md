# Entity Fluid Height, Eye Immersion, Fluid Push & Lava Contact（M2 Gameplay Slice 4b）

固定 1.21.1 server jar（SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`）および公式 mappings（SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`）の bytecode から流体高さ・視線水没・水流押し流し・溶岩接触の契約を採取し、単一の共有所有者 [`src/entity_fluid.rs`](../../../src/entity_fluid.rs)（`src/main.rs` から `include!`）として閉塞した。

詳細契約・bytecode offset は [specs/horizon-gameplay/entity-fluid.md](../../../specs/horizon-gameplay/entity-fluid.md) を正本とする。

## 1.21.1 Bytecode の実測補正点

`javap -p -c` による `server-1.21.1.jar` 全探索で確認した 1.21.1 固有契約:

- `Entity.updateFluidOnEyes()`（`bsr.H()`, offsets 0–155）は `getEyeY()` を直接 `BlockPos.containing(x, d, z)` へ渡し、**`0.11111111F`（`0x3de38e39`）を減算しない**（jar 全体で 0 件）。`LivingEntity.baseTick()`（`btn.aw()`, offsets 229–267）が `!Blocks.BUBBLE_COLUMN` を合わせて検査する。
- `Entity.isInLava()`（`bsr.bt()`, offsets 0–29）は `deflate(-0.1, -0.4, -0.1)` ではなく、`Entity.updateFluidHeightAndDoFluidPushing`（`bsr.a(Lawu;D)Z`, `deflate(0.001)`）が更新した `fluidHeight.getDouble(FluidTags.LAVA) > 0.0` を返す。

## 所有者統合

- [`src/entity_collision.rs`](../../../src/entity_collision.rs) の `mob_dimensions` で誤番していた `cave_spider` の entity ID（`13 -> 16`）を修正。
- `player_touch_aabb` を `player_bounding_box(...).inflate(1.0, 0.5, 1.0)` へ集約。
- `Client` tick の水没・溶岩判定を `player_drowning_submerged` / `player_in_lava` へ置換（従来の `!fire_immune && ... || ...` 優先順位バグも解消）。
- `cart_in_water` を `aabb_in_water(EntityAabb::from_dimensions(pos[0], pos[1], pos[2], 0.98, 0.7))` へ委譲。
- `mob_snap_y` に水中ショートサーキット・非遮蔽水柱の降下・水中着地時の `cow_land` および落下ダメージ無効化を統合。
- `mob_tick` に水流押し流し（`mob_water_state` → `mob_collide_move`）・`mob_drowning_submerged`・`!mob_fire_immune && mob_in_lava`（10 tick ごと 4.0 ダメージ、致死時 `burning = true`）・水中での不死系（zombie / skeleton）燃焼解除（creeper `fuse` は保持）を統合。

## 検証結果

- ユニット・結合テスト（`systemd-run --user --scope -p CPUQuota=400% -p MemoryMax=5G`, `-j 4`）:
  - `entity_fluid` **6/6 passed**（`LiquidBlock` 0..=15・`hasSameAbove`・waterlogged slab・`kelp`/`seagrass`/`bubble_column` 高さ、`getFlow` 水平勾配・段差・`IceBlock` 除外、`updateFluidHeightAndDoFluidPushing` `deflate(0.001)` 境界・Player/Mob 正規化差・`0.014`/`0.0045000000000000005`、Player 姿勢別 `eyeHeight`・Mob `eyeHeight`・`bubble_column`、溶岩 `deflate(0.001)` 境界・flowing lava、`Client`/`cart_in_water`/`mob_snap_y`/`mob_tick` 統合）
  - `entity_collision` 6/6 passed、`cart_` 3/3 passed、`spawner_` 8/8 passed
  - 全テスト `cargo test --offline --locked -j 4 -- --test-threads=8`: **692 passed; 0 failed; 1 ignored**（既存の parked gate 1 件から増減なし）
- 故障注入（`MUTATION_FILTER=entity_fluid_ MUTATION_STABILITY=1 CARGO_BUILD_JOBS=4 bash tools/mutation_check.sh --offline`）:
  - **3 proved, 0 not a gate, 0 parked, 0 skipped**:
    - `entity_fluid_has_same_above_returns_full_height`
    - `entity_fluid_is_in_lava_uses_0_001_deflate`
    - `entity_fluid_eye_immersion_respects_pose_and_surface`
- 層・負債ゲート:
  - `tools/layers_check.sh`: `source graph: 44 modules, 111 edges, 0 named errors`, `PASS`
  - `tools/debt_ratchet.sh`: `0 over limit`（`wall-clock reads: 0`, `ignored tests: 1`）

# Entity Collision & Step-Up（M2 Gameplay Slice 4a）

固定 1.21.1 server jar (`59353fb40c36d304f2035d51e7d6e6baa98dc05c`) と公式 mappings (`03f8985492bda0afc0898465341eb0acef35f570`) の bytecode から `Entity.collide` / `collideBoundingBox` / `collideWithShapes` / `collectCandidateStepUpHeights` / `BlockCollisions` / `Shapes.collide` / `Entity.move` の契約を採取し、単一の共有所有者 [`src/entity_collision.rs`](../../../src/entity_collision.rs)（`src/main.rs` から `include!`）として閉塞した。

詳細契約・bytecode offset は [specs/horizon-gameplay/entity-collision.md](../../../specs/horizon-gameplay/entity-collision.md) を正本とする。

## 所有者統合

- `collect_block_collision_boxes` と `collide_axis` の重複を `src/main.rs` から撤去し、`src/entity_collision.rs` に一元化。
- `BlockCollisions` (`dca.<init>`) の走査窓 `[floor(min - 1e-7) - 1 ..= floor(max + 1e-7) + 1]` を 3 軸すべてに適用し、1.5 ブロック高の fence / wall に上端付近（`minY = by + 1.2`）で接触する場合も取り落とさない。
- `CartRuntimeWorld::move_cart` は `entity_collide_world(aabb, delta, false, 0.0, &[])` を呼び出す。
- `mob_tick` の水平移動（creeper / passive wheat / passive wander / skeleton / zombie）は `mob_collide_move`（`Attributes.STEP_HEIGHT` の `0.6` と 1.21.1 `EntityType` 寸法）を経由し、支持面がある場合は `mob_snap_y` による床スナップを抑制する。

## 検証結果

- ユニット・結合テスト（`systemd-run --user --scope -p CPUQuota=400% -p MemoryMax=5G`, `-j 4`）:
  - `entity_collision` 6/6 passed（軸優先度 `|x| < |z|` と同値時の `X` 優先、slab 段差の step-up と `1.0` 壁の拒否、低天井での step-up 抑止、1.5 高 `oak_fence` のブロックと `BlockCollisions` 走査窓、複数 box コーナー滑り・`1e-7` 閾値・`Mth.equal(1e-5f)` 判定、`mob_collide_move` / `mob_snap_y` 統合）
  - `cart_` 3/3 passed、`spawner_` 8/8 passed
  - 全テスト `cargo test --offline --locked -j 4 -- --test-threads=8`: **681 passed; 0 failed; 1 ignored**（既存の parked gate 1 件から増減なし）
- 故障注入（`MUTATION_FILTER=entity_collide_ MUTATION_STABILITY=1 CARGO_BUILD_JOBS=4 bash tools/mutation_check.sh --offline`）:
  - **3 proved, 0 not a gate, 0 parked, 0 skipped**:
    - `entity_collide_axis_tie_prefers_x`
    - `entity_collide_step_up_respects_max_up_step`
    - `entity_collide_block_window_catches_1_5_fence`
- 層・負債ゲート:
  - `tools/layers_check.sh`: `source graph: 44 modules, 111 edges, 0 named errors`, `PASS`
  - `tools/debt_ratchet.sh`: `0 over limit`（`wall-clock reads: 0`, `ignored tests: 1`）

# Entity Push, Cramming & Spatial Grid（M2 Gameplay Slice 4c + M3 P5）

固定 1.21.1 server jar（SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`）および公式 mappings（SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`）の bytecode から `Entity.push`・`LivingEntity.pushEntities`・`EntitySelector.pushableBy`・`maxEntityCramming`・`LivingEntity.hurt` 無敵窓の契約を採取し、単一の共有所有者 [`src/entity_push.rs`](../../../src/entity_push.rs) とチャンク境界対応空間索引 [`src/entity_spatial.rs`](../../../src/entity_spatial.rs)（いずれも [`src/main.rs`](../../../src/main.rs) から `include!`）として閉塞した。

詳細契約・bytecode offset・意図的なモデル境界は [specs/horizon-gameplay/entity-push.md](../../../specs/horizon-gameplay/entity-push.md) を正本とする。

## 1.21.1 Bytecode の実測契約点

- `Entity.push(Entity)`（`bsr.h(Lbsr;)V`, offsets 0–139）: `!same_vehicle && !noPhysics` で `d = Mth.absMax(dx, dz)`、`d >= 0.01F` を処理し、`1 / d` を `min(..., 1.0)` で clamp、`0.05F` 倍して `!isVehicle && isPushable` 側へ `push(dx, 0.0, dz)` を適用。
- `LivingEntity.pushEntities()`（`btn.r()V`, offsets 0–216）: `EntitySelector.pushableBy` 候補に対し、`maxCramming = GameRules.RULE_MAX_ENTITY_CRAMMING`（既定 24）、`maxCramming > 0 && list.size() > maxCramming - 1 && rand_int(4) == 0` かつ非搭乗数 `> maxCramming - 1` で `6.0F` ダメージ。
- `LivingEntity.hurt`（`btn.a(Lbrk;F)`, offsets 228–311）: `invulnerableTime > 10` かつ同額以下を拒否し、受理時は `invulnerableTime = 20`（毎 tick `-1`）で同額ヒットを 10 tick 間隔へ制限。
- `EntitySelector.pushableBy`（`bsw.a(Lbsr;Leyi;Leyi$a;Lbsr;)Z`）: bytecode 上の `PUSH_OTHER_TEAMS` 異チーム拒否／`PUSH_OWN_TEAM` 同チーム拒否の形状をそのまま反映。`ArmorStand`（`0.5 x 1.975`）・`Bat`（`0.5 x 0.9`）は `isPushable = false`。

## 所有者統合

- [`src/entity_spatial.rs`](../../../src/entity_spatial.rs): CSR 圧縮 `(chunk_x, chunk_z)` 空間グリッド [`EntitySpatialGrid`](../../../src/entity_spatial.rs) と stamp 重複排除 [`SpatialScratch`](../../../src/entity_spatial.rs) を実装（`query_aabb_into` / `query_radius_xz_into`）。
- [`src/entity_push.rs`](../../../src/entity_push.rs): `entity_push_delta`、`entity_push_pair`、`entity_selector_pushable_by`、`push_entities_step_indexed`、`hurt_window_accepts`、`apply_entity_push_step` を実装。
- [`src/main.rs`](../../../src/main.rs): `mob_tick` で `EntitySpatialGrid::build_mobs_and_clients` → `apply_entity_push_step`（壁衝突は `mob_collide_move` 経由、`0x70` 座標同期、`Client` へ `0x5a` 速度加算、致死 cramming 時 `mob_env_die`）を実行し、従来の `O(N^2)` 繁殖相手探索を `grid.query_radius_xz_into` へ置換。`/gamerule maxEntityCramming [<int>]` と非既定時のみの `GCR1` 永続化を追加（既定ワールドの保存バイト列は不変）。
- [`src/entity_collision.rs`](../../../src/entity_collision.rs): `mob_dimensions` に `armor_stand`（3）と `bat`（6）を追加。
- [`evidence/time-contract/profile_tick.py`](../../../evidence/time-contract/profile_tick.py): Slice 3c 以降で `mob_tick` 前段に存在する `"chest_viewers"` ステージを `TICK_STAGES` へ補完。

## 検証結果

- ユニット・結合テスト（`entity_push_tests` 9 件 + `entity_spatial_tests` 2 件 = **11/11 passed**）:
  - 定数・符号・clamp・`noPhysics`/spectator ガード、cramming しきい値（`> max - 1` / passenger / gamerule `0`）、10 tick 無敵窓、`EntitySelector` チーム衝突規則、チャンク境界跨ぎ 1,200 体と総当たりの完全一致、`mob_tick` 統合（壁際押し出し・spectator 除外・25 頭パイルの窓制御致死・繁殖探索）、`maxEntityCramming` 永続化（既定時 `GCR1` 非出力・非既定時 round-trip）を確認。
  - Pre-change Red 検証: `mob_tick` の push step 除去および `/gamerule maxEntityCramming` 除去の双方で `entity_push_integration_mob_tick_walls_spectator_cramming_and_breeding` が失敗することを事前確認済み。
- マイクロベンチマーク（`MC_SPINE_PUSH_BENCH=1`, debug 実行 `push-cost-microbench-debug.log`, 全レイアウトで総当たりとの一致検証込み）:
  - `entity_heavy_16b`: `n=600` で `27.6 vs 414.9 ns/mob`（**15.0x**）、`n=2000` で `27.5 vs 1038.8 ns/mob`（**37.7x**）
  - `pile_1b`: `n=600` で `171.8 vs 328.7 ns/mob`（**1.9x**）、`n=2000` で `184.9 vs 1013.7 ns/mob`（**5.5x**）
  - `pile_0.1b`（単一バケット最悪密集ケース 80 体）: `n=600` `2036.8 vs 1598.5 ns/mob`（`0.8x`）、`n=2000` `3654.9 vs 2996.4 ns/mob`（`0.8x`）
- Tick プロファイル前後比較（`evidence/time-contract/profile_tick.py`, `03744b1` before 3 回 vs after 3 回中央値）:
  - 全 10 シナリオで回帰なし（`peak_rss_kib`: `77,932 -> 77,404`、`cpu_user+sys`: `1.519s -> 1.519s`、`entity_heavy` `p50`: `268.97us -> 266.55us`、`mobs` ステージは client 不在ショートサーキットのため前後とも `~0.036ms/1000 ticks`）。
- 故障注入（`MUTATION_FILTER=entity_ MUTATION_STABILITY=1 CARGO_BUILD_JOBS=4 bash tools/mutation_check.sh --offline`）:
  - `entity_` 系 **27 proved, 0 not a gate, 0 parked, 0 skipped**（うち新規 Slice 4c 6 件すべて proved）。
- 層・負債ゲート:
  - `tools/layers_check.sh`: `PASS`（`44 modules, 111 edges, 0 named errors`）
  - `tools/debt_ratchet.sh`: `0 over limit`（`wall-clock reads: 0`, `ignored tests: 1`）

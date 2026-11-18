# `LivingEntity.travel` 流体ブランチ・浮力・溶岩押し流しスケール（M2 Gameplay Slice 4d）

固定 1.21.1 server jar（SHA-1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c`）および公式 mappings
（SHA-1 `03f8985492bda0afc0898465341eb0acef35f570`）の bytecode から
`LivingEntity.travel` の水／溶岩ブランチ、`getFluidFallingAdjustedMovement`、
`Entity.bm()` の次元依存溶岩押し流しスケールを採取し、単一の共有所有者
[`src/entity_fluid.rs`](../../../src/entity_fluid.rs)（`src/main.rs` から `include!`）として閉塞した。

詳細契約・bytecode offset・意図的な逸脱は
[specs/horizon-gameplay/entity-travel.md](../../../specs/horizon-gameplay/entity-travel.md) を正本とする。

## bytecode の採取手順（再現可能）

```
MC_SPINE_VANILLA_JAR=<pinned server.jar> \
MC_SPINE_VANILLA_MAPPINGS=<pinned server.txt> \
  bash tools/bytecode_probe.sh net.minecraft.world.entity.LivingEntity
MC_SPINE_VANILLA_JAR=… MC_SPINE_VANILLA_MAPPINGS=… \
  bash tools/bytecode_probe.sh net.minecraft.world.entity.Entity
```

`tools/bytecode_probe.sh` は jar / mappings の sha1 を検証してから
`javap -p -c` を実行する。以下はすべてその出力の `Code:` offset で確認した。

## 1.21.1 Bytecode の実測点

- `LivingEntity.travel(Vec3)`（`btn.a(Lexc;)V`, offsets 0–1251）
  - 入口 `8–53`: `d = Entity.getGravity()` = `0.08`、下降中（`getDeltaMovement().y <= 0`）に
    `MobEffects.SLOW_FALLING`（`bsb.B`）があれば `0.01`。
  - 水ブランチ `67–351`: `f = isSprinting() ? 0.9F : getWaterSlowDown()`（`btn.fh()` = 定数 `0.8F`）、
    `g = 0.02F`、`h = WATER_MOVEMENT_EFFICIENCY`（`buw.E`）は `!onGround()` で半分、
    `h > 0` のとき `f += (0.54600006F - f) * h` / `g += (speed - g) * h`、
    `DOLPHINS_GRACE`（`bsb.D`）で `f = 0.96F`、`moveRelative(g, input)` → `move(SELF, …)` →
    `multiply(f, 0.8F, f)`（垂直は **widen された `0.800000011920929`**）→
    `getFluidFallingAdjustedMovement(d, bl, …)` → `horizontalCollision && isFree` で `y = 0.3F`
    （`0.30000001192092896`）。climbable 時は `y = 0.2`。
  - 溶岩ブランチ `353–561`: `moveRelative(0.02F, input)`、
    `getFluidHeight(LAVA) <= getFluidJumpThreshold()` なら `multiply(0.5, 0.8F, 0.5)` +
    `getFluidFallingAdjustedMovement`、超えるなら `scale(0.5)`、続いて
    `if (d != 0.0) add(0, -d/4, 0)` = `-0.02`、最後に同じ `0.3F` 持ち上げ。
- `LivingEntity.getFluidFallingAdjustedMovement(double, boolean, Vec3)`（`btn.a(DZLexc;)Lexc;`,
  offsets 0–100）: `d == 0.0` または `isSprinting()` で無変換、それ以外は `y - d/16`。
  `bl && |y - 0.005| >= 0.003 && |y - d/16| < 0.003` で `-0.003`。
  `d = 0.08` では両述語が排他なので `-0.003` 終端は `SECOND` の `SLOW_FALLING`（`d = 0.01`）でのみ到達する。
  この `-d/16` が浮力で、静止したエンティティは水中 `-0.025`/tick、溶岩中 `-0.125`/tick に収束する。
- `Entity.bm()`（`bsr.bm()`, offsets 0–61）: `dimensionType().ultraWarm() ? 0.007 :
  0.0023333333333333335` を溶岩の `updateFluidHeightAndDoFluidPushing` に渡す。
  `DimensionType.ultraWarm()` は `dwy.i()`。したがって **`0.007` が ultra-warm（Nether）**、
  `0.0023333333333333335` が温帯（Overworld）。`Entity.bn()`（offsets 33–42）の水 `0.014` は次元非依存。
- `Entity.getFluidJumpThreshold()`（`bsr.di()`, offsets 0–19）: `eyeHeight < 0.4F ? 0.0 : 0.4`。
- `Entity.getInputVector(Vec3, float, float)`（`bsr.b(Lexc;FF)Lexc;`, offsets 0–104）:
  `1.0E-7` 未満は零、長さ 1 超は正規化、`Mth.sin/cos(yaw * 0.017453292F)` で回転。

### 記述の逆転（重要）

Slice 4b の仕様文と Slice 4d の発注文は `0.007` を "Overworld"、`0.0023333333333333335` を
"Nether (ultraWarm)" と書いていたが、`bsr.bm()` はその逆である。Slice 4b 時点では
溶岩スケールの唯一の呼び出し元（`aabb_in_lava_with`）が `push` ではなく `max_height` しか読まないため
定数は不活性で、名前だけが逆だった。本 slice で `LAVA_ULTRA_WARM_MOTION_SCALE = 0.007` /
`LAVA_TEMPERATE_MOTION_SCALE = 0.0023333333333333335` に改名し、
`travel_contract_4d_lava_push_scale_dimension` で両方向を固定した。
次元を配線した瞬間に、逆ラベルのままだと数値が静かに間違うため。

## 所有者統合

- [`src/entity_fluid.rs`](../../../src/entity_fluid.rs): `fluid_push_scale`、
  `level_ultra_warm` / `set_level_ultra_warm`、`fluid_falling_adjusted_movement`、
  `fluid_jump_threshold`、`living_gravity`、`fluid_input_vector`、`entity_is_free_blocks`、
  `LivingTravelContext`、`FluidTravelOutcome`、`travel_fluid_step`（水／溶岩）、
  `mob_water_state_vel` / `mob_lava_state_vel` / `mob_fluid_travel_tick`。
  `Entity.move` は `src/entity_collision.rs::entity_collide_world` のまま（`STEP_HEIGHT 0.6`）。
- `Mob.vel: Mutex<[f64; 3]>` = `LivingEntity.deltaMovement`（流体ブランチのみが読み書きし、
  流体外では 0 に戻す）。
- `mob_tick`: 旧「水流を変位として適用」ブロックを `mob_fluid_travel_tick` に置換。
  `Entity.baseTick` と同じ順序で push を先に速度へ加算し、移動は `Entity.move` 経由、
  変位があれば `0x70` を送出。不死系の水中燃焼解除は維持。
- `mob_snap_y`: 水中に加えて溶岩中でも short-circuit し、溶岩ポケット内のモブを surface へ
  テレポートさせない。

## 意図的な逸脱（逸脱として明記）

- このサーバーのモブには `movementInput` が無く、AI は `mob_collide_move(m, mp, dx, dz)` の
  **変位**で動く。したがって Slice 4d は「速度（流体 push + 浮力/沈降 + `0.8F`/`0.5` ドラッグ）」を所有し、
  操舵変位はそのまま残す。アクセラレーションを装って変位を `moveRelative` に流し込むことはしない。
  ドラッグ定数・`0.02` インパルス・効率ブレンド・sprint/dolphins・落下補正はいずれも
  vanilla と同じコードで実装し、テストしている。
- `Entity.isFree` はブロックのみ（vanilla の `Level.noCollision` はエンティティ AABB も弾く）。
- `jumpInFluid` / `goDownInWater` の `±0.04` は `jumping` 入力源が無いため未接続。
- `WATER_MOVEMENT_EFFICIENCY` は全 vanilla モブで `0.0` なので `h > 0` ブレンドはテストのみで到達する
  （これは vanilla でも同じ）。

## 検証結果

- ユニット・結合テスト（`systemd-run --user --scope -p CPUQuota=400% -p MemoryMax=6G`, `-j 4`）:
  - `travel_contract_4d_` **8/8 passed**（水 `0.8F`/widen 垂直/sprint `0.9F`/dolphins `0.96F`/
    `0.02` インパルス/`-0.025` 終端、効率 `0.54600006F` と空中半減、`0.4F` ジャンプ閾値と
    `getInputVector`、溶岩 `0.5`/`scale(0.5)`/`-0.02`/零重力/`-0.125` 終端、
    `getFluidFallingAdjustedMovement` の `-0.003` 終端と 2 つの早期 return、`0.3F` 持ち上げと
    climbable `0.2`、溶岩 push スケール極性、`mob_tick` 統合・`mob_snap_y` 委譲・速度リセット）
  - 全テスト `cargo test --offline --locked -j 4 -- --test-threads=8`: **727 passed; 0 failed;
    1 ignored**（既存の parked gate 1 件から増減なし）
- 故障注入（`MUTATION_FILTER='travel_contract_4d_' MUTATION_STABILITY=1 CARGO_BUILD_JOBS=4 bash tools/mutation_check.sh --offline`）:
  **7 proved, 0 not a gate, 0 parked, 0 skipped**
  - `travel_contract_4d_water_vertical_drag_is_0_8f`（`0.800000011920929` → `0.9`）
  - `travel_contract_4d_water_sprint_slow_down_is_0_9f`
  - `travel_contract_4d_lava_push_scale_ultra_warm_is_0_007`（極性入替）
  - `travel_contract_4d_fluid_falling_terminal_frame_is_negative`（`-0.003` → `0.003`）
  - `travel_contract_4d_jump_threshold_reads_eye_height`
  - `travel_contract_4d_mob_tick_applies_the_vertical_travel_delta`
  - `travel_contract_4d_mob_snap_y_defers_lava_to_travel`
    （未接続時は溶岩中のモブが `56.5 → 60.0` へ跳ぶことを実測してから登録した。
    当初のアサーションは `mob_has_support` が先に真になる場面で**落ちない**テストだったため、
    床の無い溶岩ポケットに変更している）
- 8 threads 版（`tests/verify.sh` が 3 回回す条件）: `--test-threads=8` を 2 回実行し、いずれも
  **727 passed; 0 failed; 1 ignored**。
- フィクスチャの座標に関する実測メモ: 統合テストのモブは**原点チャンク内**に置いている。
  `worldgen::Gen` の chunk column キャッシュは 128 件の FIFO で、この test binary は
  generator 有効のまま走る。fixture が独自のチャンクを引くと、後続テスト（実行順は名前順なので
  `travel_contract_4d_*` の直後が `tick_tests::wall_clock_boundaries_*`）が読む列が FIFO から
  押し出され、そのテストの 40 tick が 28ms → 1.93s に膨らんで
  `tick-count boundaries fired immediately: [100]` で落ちる（実測: `random_ticks` が 22ms → 1.84s、
  `mobs()` は空なので mob 側のコストではない）。原点チャンクに置くと 727 passed で戻る。
- 層・負債ゲート:
  - `tools/layers_check.sh`: `source graph: 44 modules, 111 edges, 0 named errors`, `PASS`
  - `tools/debt_ratchet.sh`: `0 over limit`（`wall-clock reads: 0`, `ignored tests: 1`）

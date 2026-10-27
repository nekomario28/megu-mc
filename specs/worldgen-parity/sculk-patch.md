# `sculk_patch` の移植計画（未着手・調査のみ）

2026-10-27。`sculk_patch` は他の feature 型と違い、**1 つのサブシステム**
（`SculkSpreader` と `SculkBehaviour` 実装群）を伴う。ここに、次に着手する人が
知り直さなくて済むよう、実測した構造・分岐・罠・再利用できる部品を固定する。
コードはまだ書いていない（半端な subsystem を緑と記録しないため）。

## 対象クラス（固定 jar、公式 mappings）

| 役割 | クラス | obf |
|---|---|---|
| feature 本体 | `SculkPatchFeature` | `edi` |
| config | `SculkPatchConfiguration` | `eez` |
| 拡散器 | `SculkSpreader` | `dmu` |
| カーソル | `SculkSpreader$ChargeCursor` | `dmu$a` |
| sculk ブロックの振る舞い | `SculkBlock` | `dmq` |
| vein の振る舞い | `SculkVeinBlock`（`MultifaceBlock` 拡張） | `dmv` |
| 振る舞い interface + 既定実装 | `SculkBehaviour` / `SculkBehaviour$1` | `dmp` / `dmp$1` |
| multiface の道具（facing ビット・spreader） | `MultifaceBlock` | `dky` |

参照は `tools/vanilla_src.sh <official class>` で作れる（repo 外・SHA-1 検査つき）。

## 同梱 pack の 2 つの config と、その意味

| config | charge_count | amount_per_charge | spread_attempts | growth_rounds | spread_rounds | catalyst_chance | extra_rare_growths |
|---|---|---|---|---|---|---|---|
| `sculk_patch_deep_dark` | 10 | 32 | 64 | **0** | 1 | 0.5 | uniform 1..3 |
| `sculk_patch_ancient_city` | 10 | 32 | 64 | **0** | 1 | 0.5 | 0 |

**重要**: 両方とも `growth_rounds = 0` なので、pack の config では
`SculkBlock` の **growth 分岐（`spread_only == false`）は一度も実行されない**
（rounds = `spread_rounds + growth_rounds` = 1、`spread_only = i < spread_rounds` = true）。
growth 分岐まで検証するには、cases 用に `growth_rounds = 1` の config を用意する
（config はデータなので可能）。

## feature 本体（`edi.a(ecg)`）

1. `canSpreadFrom(level, origin)`:
   - `state.getBlock() instanceof SculkBehaviour` → true
   - それ以外: `!isAir && (!is(water) || !fluidState.isSource())` → false（つまり air か
     水源だけが許される）。その後 `Direction.stream()` の**6 近傍**のいずれかが
     `isCollisionShapeFullBlock` なら true。
2. `spreader = SculkSpreader.createWorldGenSpreader()`（`dmu.b()` =
   `new dmu(true, tag=bU, 50, 1, 5, 10)`。`d()`=50 charge 分母, `e()`=1, `f()`=5, `g()`=10）
3. `rounds = spread_rounds + growth_rounds` 回:
   - `charge_count` 回 `spreader.addCursor(origin, amount_per_charge)`
   - `spread_only = i < spread_rounds`
   - `spread_attempts` 回 `spreader.updateCursors(level, origin, rng, spread_only)`
   - `spreader.clear()`
4. catalyst: `rng.nextFloat() <= catalyst_chance` かつ `origin.below()` の
   `canSurvive` → `sculk_catalyst` を origin に置く。
5. `extra_rare_growths.sample(rng)` 回: `origin.offset(nextInt(5)-2, 0, nextInt(5)-2)` が
   air かつ below の `isFaceSturdy(UP)` → `sculk_shrieker[CAN_SUMMON=true]`。

罠: 5 の **2 つの `nextInt(5)` は sample の後に**来る（`sample` → ループ → x, z）。
`spreader.updateCursors` の `level.levelEvent(3006, ...)` は**モデル化しない**
（sound/particle は block ではない。end_gateway の block entity と同じ扱いで明記する）。

## 拡散器（`dmu`）

- `addCursor(pos, amount)`: `amount` を 1000 ずつに割って `ChargeCursor(pos, part)` を
  **最大 32 個**まで追加（超えたら捨てる）。
- `updateCursors(level, origin, rng, spread_only)`: 全カーソルを更新し、
  - 生き残ったカーソルを位置で**マージ**する。`!world_gen` なら合計 `<= 1000` のとき
    1 本にまとめる（world gen では常に別カーソルとして残す）。
  - 位置ごとの合計 charge を数え、`levelEvent`（no-op）を出す。
- `ChargeCursor.update`（`dmu$a.a`）:
  - `charge > 0` かつ（`spread_only` か `level instanceof ServerLevel`）のときだけ進む。
  - `update_delay > 0` なら減らして終わり。
  - 現在位置の state から振る舞いを取り、`spread_only` のとき
    `behaviour.attemptSpreadVein(...)` を呼ぶ（成功したら sound = no-op）。
  - `charge = behaviour.spreadOneCharge(...)`、0 以下なら `behaviour.decay(...)`。
  - そうでなければ `findNextSculk`（18 近傍をシャッフルして走査、
    `MultifaceBlock.pack` で facings を、vein の `attemptSpreadVein` の戻りで早期終了）で
    次の位置を決め、`isWorldGeneration` で原点から **15 ブロック**を超えたら charge=0。
  - `facings` は次位置の state が `SculkBehaviour` なら `MultifaceBlock.getFacings`。
  - `decay_delay = behaviour.decayDelay(...)`、`update_delay = behaviour.updateDelay()`。
- 18 近傍の並び: `BlockPos.betweenClosed(-1,-1,-1 .. 1,1,1)` から
  「少なくとも 1 軸が 0」かつ原点でないセル（=18）を `Vec3i` 化したもの。
  **この順序**で作ったリストを `Util.shuffledCopy` するので、順序がそのまま draw になる。

## 振る舞い（`dmp`）

- `SculkBlock`（`dmq`）:
  - `spread_one_charge`: `charge != 0 && rng.nextInt(50) == 0` のとき、原点からの
    距離と `clampedMap` で「育つか」を決め、`charge_count` 未満なら
    `sculk` を **`pos.above()`** に置く（`nextInt(11) == 0` なら shrieker
    `[CAN_SUMMON=world_gen]`、それ以外は sculk。`WATERLOGGED` は
    `isWaterAt` で立てる）。戻りは `max(0, charge - charge_count)`。
  - 育たないときは `rng.nextInt(10)` の値で charge を減らす（`decreaseCharge`）。
  - `decay`: `charge <= 0` のとき `sculk` を `sculk_vein` に置き換える（`decay` 実装）。
  - `isWorldGen` 由来の `isWorldGen()` = false なので、`ChargeCursor` の
    `d()` 分岐で state を取り直す点に注意。
- `SculkVeinBlock`（`dmv`）: `MultifaceBlock` の spreader を使う。**port 側は既存の
  `multiface_state` / `multiface_spread` / `multiface_shuffle` がそのまま使える**
  （`sculk_vein` は 6 面すべてを取れる multiface）。
- 既定（`dmp$1`）: `facings == null` のときは vein の spreader で
  `spreadAll`、`facings` が空でなければ `attemptSpreadVein`、
  非 air かつ非水源なら false。

## port の置き場所

`src/feature.rs` に `struct SculkSpreader` / `struct SculkCursor` と
`fn spc_*` を置く（同じ layer。モジュールを増やすと layering gate の登録が要る）。
必要になる新しい block predicate は 2 つだけ:

- `isCollisionShapeFullBlock`（既存の `is_solid_render` とは別物。`st_side` の全面か
  `boundingBox` から引けるか要確認）
- `isFaceSturdy(level, pos, dir)`

## fixture の設計（次のセッションでそのまま使う）

- cases を 2 つ: `sculk_patch`（pack の `sculk_patch_deep_dark`、growth なし）と
  `sculk_patch_growth`（`growth_rounds: 1`, `spread_rounds: 1` の合成 config）。
- scene: `air`（何も無し → 近傍に collision-full が無いので refuse）、
  `deepslate_floor`（`deepslate` の床 → 拡散の起点）、
  `deepslate_floor_water`（水源を挟む）、
  `sculk_start`（原点が `sculk` → SculkBehaviour 経由で開始）、
  `ceiling`（上に deepslate）。
- box は広めに（spread は 15 ブロックまで届く）: `[[-16,16],[-8,20],[-16,16]]`。
- gate 候補: charge 分母の `nextInt(50)`／18 近傍の順序／マージ上限 1000・カーソル上限 32／
  catalyst の `nextFloat` と `canSurvive`／shrieker の `CAN_SUMMON` と `isFaceSturdy`／
  `growth_rounds` 分岐の有無。

## 未検証として残ること

- 現時点で `sculk_patch` のセルは 1 つも port と照合していない。この文書は調査記録で
  あり、parity の主張ではない。

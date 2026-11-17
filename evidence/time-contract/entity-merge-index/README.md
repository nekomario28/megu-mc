# 実体mergeの索引 — mergeの費用は距離判定ではなくprobeだった

`src/main.rs` の `CellBuckets` / `merge_drops` / `merge_xp_orbs`、
`src/merge_tests.rs` の orb差分ゲート、`src/tick_tests.rs` の payload digest、
`evidence/time-contract/profile_tick.py`。基準は `5c6ccec`（`perf/worldgen-parallel`）。

## 先に訂正すること（このページの前提）

`entity-fixture` と `drop-pile` の各READMEは entity_heavy を
**「667ms/tick」「337,607µs/tick」**と書いていた。**これは1000倍の単位誤読である。**

`results.json` の `total_ns` は**1000 tick分の合計**、`tick.p50_ns` は**1 tickのns**。
`5bc49c1` の実測値を読み直すと **337.6µs/tick**（p50 337,607 ns）であり、
50ms予算の **0.68%** にすぎない。したがって

> 「667ms/tick は20TPSを破綻させる」

は成立しない。**測った数値そのものは正しく、読み方だけが1000倍ずれていた。**
古い記述は履歴として残す（[entity-fixture](../entity-fixture/README.md) /
[drop-pile](../drop-pile/README.md) に訂正とSHAを追記した）。
同じ枝の [random-ticks](../../gameplay/random-ticks/README.md) は最初から
「tick p50 402,932ns = 402µs」と正しく読んでおり、そちらと矛盾しない。

このページの数値はすべて **µs/tick**（`total_ns / 1000 / 1000`、p50は `p50_ns / 1000`）。

## Phase 0 — `5c6ccec` での再測定

runner は既存と同じ `evidence/time-contract/profile_tick.py --runs 3`
（release、100 warmup + 1000 measured tick/tick ごとに stage を計測、sleepなし）。
3プロセスを連続実行し、**p50の中央値**を採る。生の系列は
[`results-5c6ccec.json`](results-5c6ccec.json)（binary `a5513303fd817223`、
`working_diff_sha256=567b4d6b8a23`…=測定用のrunner修正のみ）。

| fixture | p50 | 上位3 stage |
|---|---:|---|
| `empty` | 1.72 | environment 0.281, scheduled_blocks 0.085, drops 0.080 |
| `idle_actor` | 60.03 | random_ticks 49.473, hot_regions 8.884, environment 0.279 |
| `idle_furnaces` | 63.75 | random_ticks 49.519, hot_regions 8.922, furnaces 3.631 |
| `active_furnaces` | 65.40 | random_ticks 49.540, hot_regions 8.935, furnaces 5.747 |
| `crowded_actors` | 66.56 | random_ticks 49.494, hot_regions 10.590, actors 4.924 |
| `dense_actors` | 408.03 | hot_regions 235.564, random_ticks 166.266, actors 4.926 |
| **`entity_heavy`** | **345.18** | **environment 134.888, drops 127.150, random_ticks 72.441** |
| `entity_heavy_furnaces` | 352.91 | environment 135.408, drops 127.849, random_ticks 74.698 |
| `active_furnaces_pack` | 87.75 | random_ticks 71.919, hot_regions 8.943, furnaces 5.845 |
| `drop_pile` | 82.62 | random_ticks 71.892, hot_regions 8.935, drops 0.259 |

**entity_heavy の上位3 stage は environment (134.9µs, 39%)、drops (127.2µs, 37%)、
random_ticks (72.4µs, 21%)。** run間の広がりは p50 で 0.44〜2.23µs、
stage では environment ±0.31µs / drops ±0.55µs。**ノイズ床は約0.5µs**なので、
1µsを超える差は読める。

`drop_pile` の drops が 0.259µs なのは、集中した山が初tickで1スタックに畳まれるため
（[drop-pile](../drop-pile/README.md) の結論どおり）。つまり drops の127µsは
**「600個が散っている」ケースの費用**である。

## 内訳 — sub-split で «どこ» を特定する

`MC_SPINE_TICK_PROFILE=1` で内側の区間を出す。entity_heavy:

| 区間 | µs/tick | 何 |
|---|---:|---|
| environment | 136 | |
| `env/rest` | 111 | bolts / bobbers / pearls / snowballs / **xpのdespawn+merge** / tick関数 |
| `env/xp_merge` | **105** | orbのマージ（全ペア走査） |
| `env/plates` | 16 | mob 600体のpos lock + overlay参照 |
| `env/crops` | 9 | |
| drops | 128 | |
| `drops/merge_scan` | **84** | セル27近傍の走査 |
| `drops/merge_cells` | **25** | HashMap構築 |
| `drops/pickup` / `despawn` / `xp_pickup` | 6 / 5 / <1 | |

**注意（この測定の限界）**：profile有効時は block lookup の多い stage が膨らむ
（`random_ticks` 72.4 → 129µs、tick p50 345 → 403µs）。したがって
**profile有効の絶対値を無効時の絶対値と比べてはいけない**。この表は
「1つのstageの中の比率」を読むためのもので、environment 136 と drops 128 は
無効時の 134.9 / 127.2 とほぼ一致する（±1µs）。

## 試して戻した実験 — orbをdropと同じセルバケットにする

slice 20 と同じ形（セル+26近傍、`i`昇順、候補`j>i`昇順、最初の一致でbreak）を
orbのマージにも入れ、**差分ゲートもpayload digestも緑**のまま測った：

| 区間 | 全ペア走査 | セルバケット(HashMap) |
|---|---:|---:|
| `env/xp_merge` | 105µs | **125.5µs** |
| entity_heavy p50 | 345.18 | 375.22 |

**20µs遅い。戻した。** 理由は測定から明らかで、このfixtureの orb は
16ブロック格子に600個＝**600個が全部別セル**に散っている。すると
- 全ペア走査: 179,700回 × 約0.58ns（sub/mul/add/比較）= 105µs
- セルバケット: 16,200 probe × 約5.7ns（SipHash）+ 600挿入 = 119µs

バケットが得なのは「同じセルに複数いる」ときだけで、**この形は
バケットの最悪ケース**である（[drop-pile](../drop-pile/README.md) が
「近くてマージできない大量のドロップ」を次の課題として挙げていたのと同じ構造）。
dropの側でも同じ数字が出ている（scan 84 + cells 25 = 109µs ≒ 全ペア走査の105µs）。

## 入れた変更 — probeをSipHashから算術に

バケットの**構造**は正しく、高いのは**probeの単価**だった。`CellBuckets` は
セル→メンバー列を「サイズを先に決めた連結ハッシュ」で持つ：
- 容量は要素数から一度だけ決める（再ハッシュなし、`Vec` 3本＋`next` 1本）
- セルごとの `Vec` を作らない（600セル＝600回のmallocを無くす）
- probeは `x*P1 + y*P2 + z*P3` をxorshiftで混ぜてマスク、**衝突は線形探査＋
  完全なキー比較**なので、衝突が変えるのは速度だけ

これを**両方のmerge**が使う（概念の所有者を1つにする）。`merge_drops` と
`merge_xp_orbs` は同じ索引・同じ候補順・同じ「最初の一致でbreak」を共有し、
違うのは距離しきい値とmerge規則だけ、という形にした。

### A/B（3 run交互、p50中央値）

| fixture | `5c6ccec` | 変更後 | Δ | 広がり 前/後 |
|---|---:|---:|---:|---:|
| `entity_heavy` | 345.18 | **242.00** | **−103.19 (−29.9%)** | 0.99 / 0.79 |
| `entity_heavy_furnaces` | 352.91 | **245.03** | **−107.87 (−30.6%)** | 0.44 / 4.97 |
| `drop_pile` | 82.62 | 82.13 | −0.49 | 0.25 / 0.18 |
| `empty` | 1.72 | 1.73 | +0.01 | 0.00 / 0.08 |
| `idle_actor` | 60.03 | 60.04 | +0.01 | 0.05 / 0.14 |
| `idle_furnaces` | 63.75 | 63.74 | −0.01 | 0.11 / 0.25 |
| `active_furnaces` | 65.40 | 65.34 | −0.06 | 0.09 / 0.22 |
| `crowded_actors` | 66.56 | 66.47 | −0.09 | 0.14 / 0.08 |
| `dense_actors` | 408.03 | 405.84 | −2.19 | 2.23 / 1.45 |
| `active_furnaces_pack` | 87.75 | 87.47 | −0.28 | 0.32 / 0.17 |

entity_heavy の stage 別：

| stage | 前 | 後 | Δ |
|---|---:|---:|---:|
| `environment` | 134.888 | 88.783 | **−46.105** |
| `drops` | 127.150 | 69.733 | **−57.418** |
| `random_ticks` | 72.441 | 73.070 | +0.629 |
| `hot_regions` | 9.083 | 9.137 | +0.054 |
| `actors` | 0.431 | 0.430 | −0.001 |

**効いたのは動かした2つのstageだけで、他の stage は ±0.6µs（ノイズ床内）**。
mergeを持たない fixture（`empty` / `idle_*` / `crowded_actors` / `dense_actors` /
`drop_pile` / `active_furnaces_pack`）はすべて ±0.5µs、`dense_actors` の −2.19µs は
hot_regions 235µs の −0.9%（その run の広がり 2.23µs と同程度）。
**置き場所を間違えていないことの対照**としてこの行を残す。

観測された tps 相当：entity_heavy の tick が 242µs なので、tick loop が
空回りする限り 20TPS（50ms/20=2.5ms/loop ではなく 50ms/loop）に対して
**0.48%**。`[tick] observed_tps` はこのfixtureでは常に 20 で飽和する
（50ms周期にsleepで合わせているため）ので、比率の根拠には使っていない。

### 計測の外に置いたもの（手を付けていない）

- **`random_ticks` 72µs**（21%）。hot chunk 121 × 表面2区間 × 3 = 726サンプル/tick の
  機能費用で、`world_rng` を消費する。順序を変えない制約下では触れない
  （内訳の未解明は [random-ticks](../../gameplay/random-ticks/README.md) が持っている）。
- **`env/plates` 16µs**。mob 600体ぶんの `pos` lock と `overlay_block`。
  `world()` を1回だけ取る形にすると、プレートが `set_block` を呼ぶ経路が
  同じロックへ再入してデッドロックする（コードのコメントがその理由を書いている）。
- **`hot_regions` 9µs**。`refresh_hot` の集合構築。dense_actors では 233µs。

### 一度だけ出た外れ値（記録）

「phase 0のmerge + 新しいsub-split」だけを建てた arm は **389.17µs** だった
（同じmergeなのに基準の345.18より44µs遅い）。sub-splitを外した同一ソースを
2回測ると 240.05 / 241.23 で、splitそのものの費用はノイズ内。よって
389.17 は **splitの費用ではなく、全ペア走査を関数へ切り出した形のcodegen差**である。
この arm は帰属の根拠に使わず、**«同一ロジックでも build 間で最大13%動く»**
という事実として残す。だからこの枝の主張は「同じbuildの中で測った差」に限っている。

## 行動が変わっていないことの証明

1. **差分ゲート**（`src/merge_tests.rs`）: 逐語で残した全ペア走査と production の
   バケットを、drop 9シナリオ・orb 9シナリオで比較する。
   orb側は「一致したeatの順序（=destroyパケットの順）」と「生き残ったorbの
   (eid, val, age)」の両方を比べる。座標境界またぎ（x/y/z各軸）、負座標
   （`floor` は `trunc` ではない）、同位置、年齢の同値、`dist == 1.0` の非マージ、
   食べられたorbが後の `i` の候補に残る形（eatの重複）を含む。
   `xp_merge_still_merges_at_all` / `bucketed_merge_still_merges_at_all` が
   「バケットが何も組まない」抜け道を塞ぐ。
2. **保存payloadのdigest**（`tick_tests::the_tick_preserves_the_saved_world_payload_byte_for_byte`）:
   決定的な世界（orb 36・drop 12、天候とRNGを固定）を200 tick動かし、
   `save_world_into` のバイト列をFNV-1a 128で畳んで固定値と比べる。
   **旧実装（全ペア走査）に戻して測った digest と、新しい実装の digest は
   同一**：`54d762909d3321a41cfd670c7ff0baa9`（生き残り orb 13、drop 2）。
   つまり merge順・RNG消費・パケット内容・保存バイトに差は無い。
   この fixture は block overlay を空に保つ（`save_world_into` は overlay を
   `HashMap` 順で書くので、バイト列が hasher の seed に依存してしまう）。
   `world()` が空であること、同じfixtureを2回走らせて digest が一致すること
   （オラクル自体の安定性）を test の中で先に確かめている。
3. **mutation**（`tools/mutation_check.sh`、`MUTATION_FILTER=xp_merge` で2件とも red）:
   近傍を `0..=1` に削る → 差分ゲートが落ちる／`aage <= bage` を `<` にする →
   差分ゲートが落ちる。**最初に書いた近傍mutationは緑のまま生き残った**
   （シナリオ1では手前のペアが先に一致してしまい、`-1` 方向のセルを踏まなかった）。
   シナリオ7（各軸で «下のセルにしか相手がいない» ペア）を足して初めて red になった。

## ついでに直したもの

- `profile_tick.py` の `len(stages) == 17` が腐っていた。tick本体は22 stageに増えており、
  **runnerは全ての測定を拒否する**状態だった（この枝の最初の実行がそれで落ちた）。
  数ではなく stage 名の集合で「どの stage も止まっていない」を主張する形にした。
- `TICKPROF` の表示を上位12行 → 20行。sub-splitを足すと12行では切れる。
- `entity-fixture` / `drop-pile` のREADMEに単位の訂正とSHAを追記（数値は消していない）。

## 観察（この枝では直していない）

同位置のorb 8個（val 1、age 0..7）を1tickマージすると、**生き残りは val 2** になる。
各 `i` が次の `j` を吸収し、その `i` 自身が後の `i` に吸収されるため、
eaten になった orb が持っていた値は消える。vanillaの「近接orbの統合」と
同じ結果かは確認していない（**差分ゲートは «現状の挙動» を固定しているだけ**）。
挙動を変える枝の仕事なので、ここでは記録に留める。

## 再実行

```sh
python evidence/time-contract/profile_tick.py --runs 3 --output /tmp/mc-spine-tick-profile
MC_SPINE_TICK_PROFILE=1 python evidence/time-contract/profile_tick.py --runs 1 --output /tmp/x  # 内訳
cargo test --release -- --exact tick_tests::the_tick_preserves_the_saved_world_payload_byte_for_byte --nocapture
MUTATION_FILTER=xp_merge bash tools/mutation_check.sh --offline
```

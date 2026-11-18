# Track 5C: the jungle dripstone residual was `ClampedNormalInt` rounding instead of `f2i`

Status: **測定で決着。47 → 0。chunk (74,-41) の port dump は native chunk と byte 一致。**
`specs/worldgen-parity/finite-acceptance-set.md` §6 の jungle-temple 行は **MATCH** に移した。

これは索引であり置き換えではない。数値はすべて下記 raw log に紐づく。

## 0. Base state

| | |
|---|---|
| base commit | `6ca45457b25c4f2f2e651379c27d6a9149d837dc` (`perf/worldgen-parallel`, clean) |
| native jar | `captures/matrix-v2/nat-4242-jungle-vine/server.jar` sha1 `59353fb40c36d304f2035d51e7d6e6baa98dc05c` |
| capture | `captures/matrix-v2/nat-4242-jungle-vine`(seed 4242, 1×1 (74,-41), stage structures, frozen, `runtime_writes 0`) |
| reference source | `tools/bytecode_probe.sh`(pinned jar + official mappings `03f8985492bda0afc0898465341eb0acef35f570`) |
| raw logs | `mc-spine-local/audit/jungle-dripstone-47-20261118T110016/` |
| binaries | pre-change `37b2151c92235106…`、post-change `6a7693f5f323037f…` |

前段(2026-11-18、vine lane)は 121 → 47 で樹冠グループを閉じ、残る 47 は dripstone_caves の
地下グループとして残していた(`jungle-east-edge-vine-edge-shape-20261118.md`)。

## 1. 47 セルの列挙と writer の同定(推測ではなく trace)

`FEATURE_WRITE_TRACE=74,-41`(port)と capture の `FCELL`(native)を、セルではなく
**書き込み列**として突き合わせた。47 セルは 3 群に割れる。

| 群 | セル数 | port 側の書き込み | native 側 | 内容 |
|---|---|---|---|---|
| A: native だけが書いた | 19 | **書き込みなし** | `feature=3,7`、すべて `s=4104` | y −39..−28 の帯の `dripstone_block`/`pointed_dripstone` |
| B: port だけが書いた | 28 | `feature=3,7`、pop `74,-42`/`74,-41`/`74,-40` | 書き込みなし | y 6..45 と y −39..−38 |
| | 47 | | | 両者のセル集合は**完全に素** |

`feature=<i>,<step>` の `i,step` は両側とも「step 内の添字, step」。step 7 =
`UNDERGROUND_DECORATION` で、`dripstone_caves` の 7 番は `[dripstone_cluster, pointed_dripstone]`、
port の sorted order では `pointed_dripstone` が添字 3。capture の `s=4104` 窓も
`SPLACE edm`(= `SimpleRandomSelectorFeature`)だけで、`pointed_dripstone` の
`simple_random_selector` と一致する。**writer は `pointed_dripstone` 1 つ**。

patch の形も一致する: どちらの群も「`dripstone_block` の 4〜5 セル + その上の tip」で、
`pointed_dripstone_patch` の base と spread の形。したがって群 B は
「別の場所に patch が立った」であり、material rule でも carver でも aquifer でもない。

## 2. 何が違うのか(draw 列の最初の分岐)

両側の placement RNG は `deco = pop + i + 10000*step` で seed されるので、**同じはず**である。
それを測った:

- port: `FSEED_ARM=74,-41,3,7 FEATURE_DRAW_LIMIT=400000` → `DRAW <n> f=3,7 <kind>=<value>`
- native: capture の `sculk-trace.txt` から `SPWINDOW start s=4104 d=1` の窓を切り出し、
  `SDRAW` の `nextInt(bound)` / `nextDouble()@dyn` を同じ順序で並べる(`@dyz` は
  `LegacyRandomSource.nextGaussian` の戻り値で、生の nextDouble ではないので除外)

結果:

| | 一致した量 |
|---|---|
| `nextInt(bound)` 列 | **352 個一致**、353 個目で分岐 |
| `nextDouble` 列(区間ごと) | 分岐直前まで一致 |
| 分岐点 | ある attempt(`nextInt(321)=116` → y=52、`nextInt(5)=3` → 4 候補)で、native は 12 doubles(=4 候補)を引いてから次の attempt へ。port は 6 doubles(=2 候補)で `nextInt(2)=0`(selector)を引いた |

つまり **「port は候補を採用したが native は採用しなかった」**。この attempt より前は
どちらも 1 度も採用していないので、draw 列は一致し続けていた(採用時の selector + body の
draw が挟まって初めて列が割れる)。

採用判定は `PM::Biome` の 1 点だけ(`biome_filter` は placement の最後)。判定の入力は
「候補セル」と「そのセルの biome」と「その biome の feature 集合」。候補セルは
`in_square`+`height_range`+`count`+`random_offset` で決まるので、**同じ gaussian から違う
セルが出ている**としか説明できない。

`BIOMEF_TRACE=74,-41` の判定 1,354 件を native world の保存 biome と突き合わせると
1,339 件が一致し、**採用された候補の biome は両側とも `dripstone_caves`** だった
(食い違い 3 セルはいずれもこの attempt の候補ではない)。したがって biome sampler ではない。

## 3. 参照実装(bytecode で読んだ `random_offset`)

`tools/bytecode_probe.sh`(jar SHA-1 検証済み)で 3 クラスを読んだ。

`RandomOffsetPlacement`(`eja`)`getPositions(PlacementContext, BlockPos)`:

```
x' = pos.x + xzSpread.sample(random)      // sample #1
y' = pos.y + ySpread.sample(random)       // sample #2
z' = pos.z + xzSpread.sample(random)      // sample #3 — xz を独立にもう一度引く
```

port の `PM::Offset` は `(pos.0 + xz.get(rng), pos.1 + ysp.get(rng), pos.2 + xz.get(rng))` で
**一致していた**(ここは触っていない)。

`ClampedNormalInt`(`bpr`)`sample(RandomSource)`:

```
bpr.a(ayw):          a(random, mean, deviation, (float)minInclusive, (float)maxInclusive)
bpr.a(ayw,FFFF):     ayo.c(random, mean, deviation) -> ayo.a(v, min, max) -> f2i
ayo.c(ayw,FF):       mean + (float)random.nextGaussian() * deviation    // d2f, f32 fmul, f32 fadd
ayo.a(FFF):          v < min ? min : Math.min(v, max)                   // clamp は f32、cast の前
```

`f2i` は **0 方向への切り捨て**。port は `(mean + gauss*dev).round().clamp(lo, hi) as i32` で、
3 点すべてが違っていた:

1. gaussian を f32 に窄めてから掛ける(port は f64 のまま)
2. clamp は f32 で cast の**前**(port は round の後)
3. cast は切り捨て(port は四捨五入)

ずれは「小数部が .5 以上」の候補で ±1 セル。`random_offset` の `xz_spread` は
`deviation` 3.0、`y_spread` は 0.6 なので、**約半分の候補が 1 セルずれる**。1 セルずれれば
biome 判定が変わり、採用/不採用が変わり、selector と body の draw が挟まって以降の draw 列が
全部ずれる — 観測した「352 個一致してから分岐、patch 集合は完全に素」の形と一致する。

`xz_spread` は JSON 上 `"deviation": 3.0` だが **IntProvider**(`bpw`)なので `bpr` が使われる
(`eja` のコンストラクタが `(bpw,bpw)`)。`clamped_normal` を IntProvider として読む箇所は
vanilla datapack 全体で `placed_feature/pointed_dripstone.json` の `random_offset` 1 箇所だけ
(`FloatProvider` 側の `configured_feature/dripstone_cluster.json` の `height_deviation` は別物)。

## 4. 修正

`src/generation_providers.rs` の `IP::Norm`(IntProvider の `clamped_normal`)を vanilla の
式に合わせた: f32 で `mean + (gauss as f32) * dev`、f32 で clamp、`as i32`(切り捨て)。
dripstone の生成構造・vine edge pass・village・population discipline・promotion boundary は
触っていない。`FP::Norm`(FloatProvider 側)も触っていない — §7 参照。

## 5. ゲート

| ゲート | 結果 |
|---|---|
| 新しい contract test(`clamped_normal_int_provider_truncates_toward_zero_like_f2i`) | 6ca4545 の provider 本体で **RED**(`left [-1,1,-1,-2,-1,1,-1,7]` / `right [-1,1,0,-1,-1,0,-1,7]`)、修正後 green |
| jungle (74,-41) 1×1 `all_air_kinds` | **47 → 0** / 98,304 |
| 同 chunk の port dump sha256 | `77964c224c9eeddff4bd0cea6ccbfc559647f44003ee001cccd6f36b33a715bf` = `native.bin` と **byte 一致**(`cmp`) |
| 新規セル | **0**(after の diff セル列挙は空) |
| 64-chunk dump(seed 0, N=8 at (0,0)) | `670fdfe3472812afeeba823a30e08f472ea8eb69965d6ba0c54b30a2e9949def`、25,165,824 B — vine lane の記録値と **byte 一致** |
| `fixture_suite` manifest-promotable | `MATCH 8 / DIFF 0 / INVALID 0 / NOT_RUN 0` exit 0 |
| `fixture_suite` manifest(9 行) | `MATCH 8 / DIFF 1`(village-taiga、この lane の対象外。前段と同じ) |
| serial suite | `662 passed; 0 failed; 1 ignored`(661 + この lane の 1) |
| `--test-threads=8` | `662 passed; 0 failed; 1 ignored` |
| mutation `MUTATION_STABILITY=1`(2 件) | red/red/green。切り捨てを `round()` に戻すと truncate の assert が、clamp の min/max を入れ替えると clamp の assert が落ちる |
| `evidence/foundation/module-layers/provider-baseline.bin` | `UPDATE_BASELINE=1` で再記録。**差分は 6 byte** で、いずれも clamped_normal の値スロット(`+1→0` ×4、`+4→+3`、`−3→−2`)。他の provider/predicate/RNG 消費の byte は 1 つも動いていない |

baseline の再記録について: この fixture は「pre-extraction baseline」で、`UPDATE_BASELINE=1` は
*検証済みの修正*のための逃げ道としてスクリプト自身に書かれている。ここでは §3 の bytecode が
修正の根拠で、差分が clamped_normal の値スロット 6 byte に閉じていることを byte 単位で確認した
(`baseline-old.bin` / `baseline-new.bin` は raw log に保存)。

## 6. この文書が主張しないこと

- 他の seed / 他の座標で `pointed_dripstone` が一致することは主張しない(測ったのは 4242 の
  (74,-41) と、seed 0 の 64 chunk dump が**変わらなかった**ことだけ)。
- dripstone_caves の `dripstone_cluster` / `large_dripstone` の一致は主張しない。この chunk では
  どちらも 1 セルも書いていない(両側)。
- `nextInt` 列が 352 個一致したことは「352 個目まで placement が同一」の意味で、
  それ以前の候補セルが同一という意味ではない(ずれていても採用判定が同じなら draw 列は一致する)。

## 7. 測ったが直していないこと(別 lane 候補)

- **`FP::Norm`(FloatProvider の `clamped_normal`)は f64 で計算している。** vanilla の
  `ClampedNormalFloat.sample` は `Mth.nextGaussian`(f32)を返すので、`dripstone_cluster` の
  `height_deviation` は厳密には f32 経路。今回のセルはこれを名指ししていないので触っていない。
- **`BIOMEF_TRACE` の `feature=<i>,<step>` は入れ子の placed feature では外側のラベルが残る**
  (port / native とも)。判定列を feature で切るときは、この汚染を前提にする必要がある
  (§2 では「採用候補の biome が両側 dripstone_caves」の確認にだけ使い、順序比較には使っていない)。

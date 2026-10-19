# 光値の vanilla 比較（探針は動く。地形が一致しない）

`tools/light_parity_probe.py` は protocol 0x27 を読み、チャンクごとの
sky/block の light-mask と、mask が指す各区間の 2048 バイト配列の digest を出す。
両サーバで同じ seed・同じ view で動かし、同じ (cx,cz) を読めば比較できる。

## 探針が vanilla を読めるようにした修正

`tests/protocol767.py` の `decode_chunk_light` は、チャンクの
**区間配列を厳密に parse** して block state まで検証していた。これは
fixture（mc-spine のパレット）用で、vanilla の実チャンクでは
`non_air=4115 > 4096` のような不整合で `AssertionError` になり、
**光を読む前に落ちていた**（`light_parity_probe` は `STRICT=False` でも
この parse を通っていた）。

`STRICT=False` のときは区間配列を **parse せず読み飛ばし**、
後続の light-mask / 配列だけを読むようにした。光は区間配列の
あとに独立して並んでおり、その長さは varint で与えられるので、
パレット id が fixture と違っても光は読める。

また `Client.raw_chunk` に 0x27 の body を保存し、探針は
`MOTION_BLOCKING` heightmap（地形。光エンジンとは独立）を展開・表示する。

## 比較結果（同一 seed=0、chunk (0,0)）

| | vanilla | mc-spine |
|---|---|---|
| sky_mask | `0x700`（区間 8,9,10 のみ） | `0x3ffffff`（全26区間） |
| block_mask | `0x0`（送らない） | `0x3ffffff`（全26区間） |
| sky 区間8 | `e3e348713da23212` | `d48d514e1f29f9cd` |
| sky 区間9 | `4affdbe77c3695d6` | `e70b2161df86b603` |
| sky 区間10 | `d0ff1b294b5288d1` | `d0ff1b294b5288d1` |

- `e5a00aa9991ac8a5` = **全 nibble 0**、`d0ff1b294b5288d1` = **全 nibble 15**（2048 バイトの sha256 で確認）。
- したがって mc-spine の区間 0..7 は全0、区間 10..25 は全15。vanilla は
  全0/全15 の区間を **mask に載せない**（区間 8,9,10 は表面付近で非自明）。
- 区間 10 は両者一致（全15）。区間 8,9（表面付近）は **digest が違う**。

## 地形は一致する。heightmap の面の取り違えだった（訂正）

最初の比較で heightmap が一致しなかったのは、**mc-spine の `MOTION_BLOCKING` が
density 面（`ChunkCols::surface`、水も葉も含まない）を使っていた**ためである。
vanilla の `MOTION_BLOCKING` は流体を含む面（`heightmaps_nbt` は `wtop` を使うべき）。
`map_chunk` の `tops` を `wtop+1` に直すと、chunk (0,0) の第1行が
**vanilla と 7 列とも完全一致**した:

```
vanilla vals row0: [127,127,127,127,127,127,127,254,254,254,254,254,254,256,4,9]
spine   vals row0: [127,127,127,127,127,127,127,254,254,254,254,254,254,256,4,9]
```

この修正は前の「地形 mismatch」の結論を**撤回する**。地形（density 面と流体）は一致し、
不一致に見えていたのは heightmap の面の選択だけだった。残差は 256 列中 16 列（z=14,15 付近の
末尾 long）で、葉（`MOTION_BLOCKING` は葉を含む）が候補。

## 光の比較（地形一致後）

| 区間 | 一致 |
|---|---|
| 10（y=80..95、全15） | **完全一致**（2048 バイト中 0 差） |
| 9（y=64..79、地表付近） | 39/2048 バイト差 |
| 8（y=48..63、地表とその上） | 476/2048 バイト差。vanilla の y=63 行に暗点（地表の穴）があり、mc-spine はそこも 15 |

つまり mc-spine の sky は区間 8 で **明るすぎる**：床（地表）が y≥63 の列で vanilla は暗いのに
mc-spine は 15。原因は light の床が `surface()`（density）で、実際の地表・流体の高さと
ずれている列があること、および水平伝播の近似が考えられる。**依然として 양자는一致しない**。

## 証拠の境界（主張しないこと）

- これは 1 サーバ・1 seed・数チャンクの比較で、区間 8/9 の差の原因
  （床の高さか水平伝播か）は特定していない。
- 探針は client の復号（protocol 767）で読んだ wire 値であり、
  クライアントの画面表示ではない。
- heightmap の展開は末尾 long 付近で値域外（>384）を出すため、行単位の比較にのみ使った。


## 証拠の境界（主張しないこと）

- これは 1 サーバ・1 seed・数チャンクの比較で、差分の原因（worldgen の実装差、
  seed の解釈、dimension 設定）は特定していない。
- 探針は client の復号（protocol 767）で読んだ wire 値であり、
  クライアントの画面表示ではない。
- 「mc-spine の光が vanilla と違う」とは、地形差があるため**まだ言えない**。
  言えるのは「探針が両者で動き、少なくとも区間8,9で digest が違う」ことだけ。

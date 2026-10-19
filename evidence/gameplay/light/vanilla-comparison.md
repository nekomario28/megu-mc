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

## ただし比較は成立しない：地形が一致しない

同じ seed=0 でも `MOTION_BLOCKING` heightmap が一致しない。
最初の 7 列（9bit 展開）を比較すると:

| chunk | vanilla | mc-spine |
|---|---|---|
| (0,0) | 127,127,127,127,127,127,127 | 120,120,119,119,119,119,120 |
| (0,1) | 133,133,128,128,132,134,134 | 128,128,128,128,127,128,128 |
| (-1,0) | 129,129,131,132,133,131,139 | 129,129,129,129,129,128,128 |

- heightmap 値は `y - MIN_Y`（`MIN_Y=-64`）。vanilla の 127 は表面 y≈127-64-1=62、
  mc-spine の 120 は y≈55。**約 7 の系統差**がある。
- 両者の packed long も先頭から違う。packing の規則（9bit、7値/long、
  37 long、`off=(i%7)*9`）は両者で同じなので、差は **地形そのもの**である。
- mc-spine の worldgen は「M3 で byte-identical」と主張されてきたが、
  **この探針では一致しない**。光値の parity は、まずこの地形差を説明しない限り
  主張できない（光の差がエンジン由来か地形由来か切り分けられない）。

## 証拠の境界（主張しないこと）

- これは 1 サーバ・1 seed・数チャンクの比較で、差分の原因（worldgen の実装差、
  seed の解釈、dimension 設定）は特定していない。
- 探針は client の復号（protocol 767）で読んだ wire 値であり、
  クライアントの画面表示ではない。
- 「mc-spine の光が vanilla と違う」とは、地形差があるため**まだ言えない**。
  言えるのは「探針が両者で動き、少なくとも区間8,9で digest が違う」ことだけ。

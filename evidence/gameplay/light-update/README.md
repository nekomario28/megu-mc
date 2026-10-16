# 境界をまたぐ生成光源を、順序に依存せず点灯する（Q）

生成地形の発光源（lava など）は overlay に無いので、block-light の拡散は編集を走査しても
見つけられない。`chunk_data` が全 state を計算するついでに自チャンクの光源を記録し、
block-light が**自チャンクと隣接8チャンク**の記録をまとめて読むようにした。これで
チャンク境界の生成光源が隣チャンクを照らす。

記録は**接続のチャンク窓が所有**する（`Client.gen_sources`、保持チャンク1件につき1エントリ）。
光が必要とするのは自チャンクと8近傍だけで、それらは全部その接続が保持しているので、
アンロードで消せば有界になる。overlay の編集光源は記録せず、`block_light_sections` の
overlay 走査が生きている状態を読む（壊した松明を再点灯しない）。記録した**生成**光源も、
そのセルが overlay に載ったら（プレイヤーが除去・置換したら）読み飛ばし、overlay の
生きた状態に委ねる（`a_removed_generated_emitter_stops_lighting` と変異で固定）。
生成光源は overlay 編集に**覆われていても記録する**：編集が外れたときに再び点灯できる
ようにするためで、編集が残っている間は上の live-overlay フィルタが抑止する。

## 順序非依存にする仕組み

隣が後から生成されると、先に送ったチャンクの光はその光源を知らない。vanilla と同じく
**Update Light パケット（protocol 767、clientbound 0x2a）**で再計算した光を送る。
`chunk_data` が記録した光源が既存の隣チャンクの探索範囲に入るときだけ、その隣を
dirty として送る。生成順の全順序対（先・後）はどちらか一方の呼び出しで必ず覆われる。

## バイトコードで確認した事項

- `ClientboundLightUpdatePacket`（obf `adj`）は `write` で `x`・`z` を **varint**、
  続いて `ClientboundLightUpdatePacketData`（obf `adk`）を書く。
- `adk.write` の順序は `skyYMask`・`blockYMask`・`emptySkyYMask`・`emptyBlockYMask` の
  4 BitSet、次に sky 配列リスト、block 配列リスト。
- `adk` の構築は section `getMinLightSection()+i`（= `LIGHT_MIN_Y/16+i`）を走査し、
  データがあれば mask に、空なら empty mask に、無ければどちらにも入れない。
- クライアント `ClientPacketListener.applyLightData` は、**どちらの mask にも無い section は
  触らない**。mask にあれば与えられた配列を、empty にあれば零を設定する。
  したがって **sky を送らなくても sky は不変**で、空の block section は配列無しで
  empty mask に置ける。
- `FriendlyByteBuf.writeBitSet` は `BitSet.toLongArray()` を長さ付き long 配列として書く。

`vlmkit`/`gxkit` の座標系ではなく、上記は 1.21.1 の Mojang 公式 mapping と
client/server jar の `javap` から読み取った。生ログは同ディレクトリの
`*.javap.txt` と `mappings.txt`。mapping の取得元は
`https://piston-meta.mojang.com/v1/packages/499680d659b7a0ccb2fa9a809c345c5baf2ace7d/1.21.1.json`。

## 実行時の確認

release ビルドで datapack 地形（`MC_SPINE_WG` 未設定、view 2）を動かし、CLI クライアントで
1チャンク移動した。クライアントは 0x2a を受信し（`tests/protocol767.py` の復号、
mask と配列数の一致を assert）、`(-2,-2)..(3,1)` の 29 チャンク分を記録した。
フラット地形（`MC_SPINE_WG=old`、生成光源なし）では 0 件で、空送信はしない。
生ログの SHA-256 は receipt に記録する。

## 証拠の境界

- これは**受信側が 0x2a を復号できること**と、境界生成光源で**更新が送られること**の
  確認である。実クライアントの描画・vanilla 実機との光値一致は画面検証（ユーザー指示で
  対象外）に残る。
- `light_dirty_for` の到達判定は x/z の探索範囲による**過大近似**で、不要な更新を送る
  ことはあっても必要な更新を落とさない。可視の差は実機照合に残る。
- 記録はチャンクの全 state から取り、隣接の記録が揃う前に送ったチャンクは更新で直す。
  これは vanilla の chunk load ごとの light propagation と同じ役割だが、vanilla の
  `ThreadedLevelLightEngine` の伝播そのものを写したものではない。

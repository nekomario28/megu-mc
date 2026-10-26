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
client/server jar の `javap` から読み取った。当時の生ログと javap 出力・mappings 抜粋はリポジトリに置かない（Mojang のコード・マッピングの再配布を避けるため削除した）ので、
`tools/bytecode_probe.sh`（jarをsha1で固定）と公式mappingsで取り直せる
（mappingsの取得元: `https://piston-meta.mojang.com/v1/packages/499680d659b7a0ccb2fa9a809c345c5baf2ace7d/1.21.1.json`）。

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

## 光値の vanilla 照合を試行（ツール欠陥を記録）

`tools/light_parity_probe.py`（新）はサーバーが送る 0x27 の sky/block light 配列を正準化して
出力する（`tests/protocol767.py` の `decode_chunk_light`、`STRICT=False` で実行）。mc-spine に
対しては動作し、chunk (0,0) で sky/block mask と各セクションの digest を取得できた。

vanilla に対しては、リポジトリの chunk デコーダが vanilla の chunk 形式で desync する
（biome palette を読んだ後、次のセクションの `non_air` が範囲外になる）。厳密デコーダはこれまで
fixture と mc-spine でしか検証されておらず、vanilla では未検証だった。光値の wire 一致を取るには
まずデコーダを vanilla の形式へ対応させる必要がある（独立したツールスライス）。

さらに mc-spine の光エンジンは vanilla `ThreadedLevelLightEngine` の移植ではなく独自伝播
（上記「証拠の境界」）なので、値の一致にはエンジンの移植が要る。したがって **座標基点と生成光源探索は
照合済み**（前者は bytecode、後者は構造）だが、**光値そのものの vanilla 一致は未確立**であり、
GUI を除外した本作業の範囲外として記録する。

### デコーダ desync の具体

chunk (0,0) を比較すると、**mc-spine は全 24 セクションが解析でき**（blob 19005、sec6 pos=12481
`non_air=4096`）、**vanilla は sec6 で desync**（blob 19002、sec6 pos=12486 `non_air=4115` > 4096）。
sec0-5 は両者とも整合する。デコーダは fixture と mc-spine に合わせて書かれており、vanilla の
chunk 形式の何か（セクション内の追加/差異）を扱えていない。実クライアントは mc-spine の chunk を
正しく描画している（監査の実クライアント画像）ので、これは**ツール側の未対応**であって製品不具合ではない。

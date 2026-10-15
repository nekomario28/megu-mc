# 密な照会をせず、同じsky-lightバイトを作る

最大視野のCLI試験が、commit `5ed649b` の hosted run `38625751779` で10秒の期限を超えた。
同じ実行ファイルへの生フレーム受信と、デコードを後に回す切り分けでも受信時間はほぼ変わらず、
サーバーの計算が支配的だった。地形高さのキャッシュ候補は効果が小さく、撤去した。
チャンク生成の区間計測はsky-lightのパックを指した。**期限・assertion・受信契約は緩めていない。**

## 省略してよい仕事

横から入った光のmapには、暗いセルを明るくしない場合にも明るいseedが入る。
「mapが空なら速いpackを使う」だけでは、実際の送信経路は常に全セルのhash照会へ落ちていた。
列の光を既存のpackerで作り、map内の該当セルだけを `max` で合成すれば、出力は同じになる。
世界・光伝播・source探索・RNG・イベントの回数は変えず、永続cacheも増やさない。

`src/light_pack_tests.rs` は旧来の密な式から全バイトの期待値を求める。
負座標、隣接チャンク、上下端、両nibble、空mapとmask順序を含めて照合する。
意図的なnibble入れ替えはこの照合で失敗し、復元・再ビルドした対照は成功した。
実CLIでも25→49→25→289→25の保持集合を受信し、既存のlight-mask変異を引き続き拒否する。

## 証拠の境界

receiptの3組の直後比較は、同じ2 CPUへ制限した**debugビルドのCI診断**。
実際の最大視野受信を通すが、負荷下の実20TPSを測定しておらず、
releaseの代表負荷性能やvanillaとの軽量性比較には使わない。
全verifyと独立レビューの記録もreceiptへ結ぶ。最終revisionのhosted CIはQで別途確認する。

この変更は従来のsection座標と光値を保存する。vanillaのlight-section座標や
生成地形の全発光源の正しさを証明するものではない。それらの照合は全体完了条件に残る。

## nibble/byte の基点を vanilla バイトコードで確認（Qの一部）

vanilla `DataLayer`（obf `dvd`）の `javap`（`DataLayer.javap.txt`）から：

- `getIndex(x,y,z) = (y << 8) | (z << 4) | x`
- バイト位置 = `index >> 1`、nibble shift = `4 * (index & 1)`
- `get` は `data[index >> 1] >> (4*(index&1)) & 15`

mc-spine の `sky_light_sections_with_spill` は
`index = (relative_y&15)*128 + lz*8 + lx/2`、`shift = (lx&1)*4` で、
`(relative_y&15)*128 + lz*8 + lx/2 == ((y<<8)|(z<<4)|x) >> 1`（y=relative_y）。
**nibble/byte の基点は vanilla と一致する**（静的導出）。

これは光値の基点だけである。**world y → light section の対応（26 section の採り方）と、
生成地形の全発光源の網羅は未照合**であり、Qの残件として変わらない。

## light section の基点を修正（Q）

vanilla `LevelLightEngine`（obf `eot`）のバイトコード：

- `getMinLightSection() = getMinSection() - 1`（`d()`）
- `getLightSectionCount() = getSectionsCount() + 2`（`c()`、overworld で 26）

つまり light の26配列は **`minSection-1`（= `MIN_Y - 16`、y=-80）から**始まる。
従来の mc-spine は `sec*16 + MIN_Y` で **1セクション高く**配置しており、クライアントは
index i を `-4+i` ではなく `-5+i` として読むため、全配列が1つずれていた。

`LIGHT_MIN_Y = MIN_Y - 16` を導入し、`sky_light_sections_from`・`block_light_sections_from`・
`sky_light_sections_with_spill` の基点とクリップを修正。`light_sections_start_one_below_the_world`
が `LIGHT_MIN_Y == MIN_Y - 16` と `MIN_Y` セクションの独立性を固定し、
`tools/mutation_check.sh` が `LIGHT_MIN_Y` を `MIN_Y` に戻す変異を赤にする。

残りは**生成地形の全発光源の網羅**（現状は overlay の編集と溶岩のみを探索する）で、これは Q に残る。

## 生成地形の光源（実装済み、境界も対応）

`chunk_data` は各チャンクの全ブロック状態を計算するので、その非fast-path分岐で
生成地形由来の `luminance_of(st) > 0` を接続のチャンク窓（`Client.gen_sources`）へ記録する。
overlay の編集光源は記録せず、`block_light_sections` の overlay 走査に任せる（壊した松明を
再点灯しない）。`block_light_sections` は**自チャンクと隣接8チャンク**の記録をまとめて
overlay/溶岩の光源へマージする。これで生成された溶岩湖や glowstone が（境界をまたいでも）
光る。

境界の順序依存は、vanilla と同じ **Update Light パケット（0x2a）** で解消した。
あるチャンクの生成光源が既存の隣の探索範囲に入るとき、その隣の光を再計算して送る。
クライアントの `applyLightData` が「どちらの mask にも無い section は不変」と扱うことを
bytecode で確認し、sky を送らず空 block section を empty mask に置く。詳細と証拠は
[境界生成光源](../../../evidence/gameplay/light-update/README.md)。

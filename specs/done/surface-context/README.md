# Surface context — steep heightmap

固定1.21.1のsurface steep条件が読むheightmapの種類を、既存surface scanへ合わせる
限定契約です。`SurfaceSystem.buildSurface`全体の移植ではありません。

## なぜ必要か

`SurfaceRules.Context$SteepMaterialCondition` は `WORLD_SURFACE_WG` の隣接4列を
比較します。既存の `cc.surf` はdensityが正の石の上面で、海面下では水を除いた高さ
です。そのまま渡すと、水中の斜面判定が陸の石面だけを見てしまいます。既存の
`cc.wtop` はこのserverのsurface段階での「固体または流体の上面」なので、これを
steepのheightmapとして渡します。

## 維持する不変条件

- steepは`z+1 >= z-1 + 4`または`x-1 >= x+1 + 4`。3ブロック差はsteepではない。
- 石のdensity上面と、水を含むsurface heightmapを混同しない。
- 変更は既存`Gen`の3つのsurface呼出し点だけに閉じ、別のsurface engineを追加しない。
- コンパイル済みの展開や実worldのsurface一致を、bytecode由来の限定修正の代用にしない。

実装は [`worldgen.rs`](../../../src/worldgen.rs) の `test_cond` / `eval_rule` と呼出し点、
テストは同ファイルの `surface_steep_reads_the_world_surface_heightmap` が持ちます。

## 閉じていないもの

`SurfaceSystem.buildSurface`のwater height・stone depth・ocean floor開始位置・
deep stateとの相互作用、biome lookupの実world比較、whole-world surface差分、
carver `top_material`の完全一致は別です。

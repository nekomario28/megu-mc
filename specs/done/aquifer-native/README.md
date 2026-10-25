# Aquifer native seam — 固定1.21.1のpreliminary surfaceと流体状態

既存の `Gen` / `DF` / `Normal` / aquifer owner を修復した限定契約です。全worldgenや全aquiferの受け入れではありません。数値・fixture・実行unitは[検証記録](../../../evidence/foundation/aquifer/README.md)を正本とします。

## なぜこの分け方か

donorのaquifer fixtureは、実chunkの外側の座標やdonorの乱数設定を含み、固定jarの `NoiseBasedAquifer` にそのまま問い合わせられません。そこで、jar自身のdatapackを `RegistryDataLoader` に読ませ、普通の `ProtoChunk` / `NoiseChunk` / `Aquifer` を作り、公開APIの `preliminarySurfaceLevel` と `computeSubstance` を正解にしました。donorの内部メソッド列は回帰pinとして残し、native証拠とは呼びません。

Preliminary surfaceの列は **QuartPos** です。`SectionPos` と取り違えると、隣の4ブロック列の密度を読み、しきい値の直前で8ブロック違う面を返します。`initial_density_without_jaggedness` を `minY+height` からcell height刻みで下ろし、`0.390625` を超える最初の値を採る、という本体のscanをownerに置きました。

CubicSplineはf32の世界です。location・derivative・定数・coordinateをf32にし、`Mth.lerp` の順序で評価しないと、densityの最下位ビットがずれてsurface判定が変わります。この変更はaquifer専用ではなく、converted/referenceのdensity経路にも影響します。

Deep aquiferは1.21.1では `erosion < -0.225F && depth > 0.9F`（`OverworldBiomeBuilder.isDeepDark`。`NoiseRouter.h()/i()` は erosion/depth accessor）です。旧`exclusion`はこの版の意味ではないため使いません。donorのskip-above最適化もvanillaに無いので外しました。初回の独立reviewはfield名から`continents/erosion`と読む誤りを検出し、`erosion/depth`へ修正しました。

## 維持する不変条件

- 実クラスのdatapack・codec・constructorを正解とし、donorの版・乱数設定・定数を仕様にしない。
- Preliminary surfaceはQuartPos、`initial_density_without_jaggedness`、`minY+height` 開始、cell height刻み、`0.390625` 超、`Integer.MAX_VALUE` sentinelを保つ。
- Splineはf32の値・演算順を保つ。f64へ昇格して「ほぼ同じ」式にしない。
- Deep判定はf32由来の`-0.225`/`0.9`と短絡順を保つ。旧`exclusion`やskipを成功した代替にしない。
- native corpusは入力・生density・prelim・最終fluidを別々に照合し、余分・欠落・bool/int・出自違いを拒否する。donor fixtureをnative parityの代用にしない。

評価は [`worldgen.rs`](../../../src/worldgen.rs) の `prelim_est_raw` / `aq_fluid_y` / `eval_spline_c`、検証は [`aquifer_native_tests.rs`](../../../src/aquifer_native_tests.rs) と [`aquifer_oracle.py`](../../../tools/aquifer_oracle.py) が持ちます。

## 閉じていないもの

1 seed・1 chunk・限られた密度のcorpus、native deep-dark chunk、全surface context、biome carver/spawner方言、任意seed/chunk、whole-worldの再受け入れ、性能は別です。旧feature captureや全体の赤をこのsliceで消したとは扱いません。

## 捨てた道

- `SectionPos` で揃える案: previewの`(64,47)`で8ブロックずれ、QuartPosが正しいと判明。
- donorの`exclusion`を0で埋める案: 1.21.1のdeep判定はerosion/depth（field名とaccessor名を取り違えない）。
- donorのskip-above: 実クラスに存在しない最適化。
- 代数変形だけのspline mutation: IEEEのnegationでビット同一になり、gateにならなかったため、`Mth.lerp`の順序を変えるmutationへ差し替えた。

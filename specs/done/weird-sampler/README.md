# Weird scaled sampler — ゼロ代替を評価契約へ戻す

既存の `Gen` / `DF` / `Normal` で、固定1.21.1のsamplerとそのnative noise経路を修復した限定契約です。全worldgenの受け入れとは別で、実行結果と制約は[検証記録](../../../evidence/foundation/weird-sampler/README.md)を正本とします。

## なぜこの分け方か

未知のdensity typeをゼロへ代替すると、ロード成功が未実装を隠します。一方、samplerから正の値が出ても、その下のnoiseが正しいとは限りません。実際に下位の振幅係数と浮動小数点の演算順にも差があったため、mapper・raw noise・sampler結果を独立した実クラス対照にしました。

係数の全体調整は選びません。nativeとconverted encodingは構築規則が異なり、同じ最終値を意図した式でもf64の括り方を変えるとビットが変わります。既存ownerを使い、native構築だけが持つ意味を明示することで、無関係なconverted/legacy経路の構築計算を維持します。このnative constructorは共有されるため、cave以外の気候・地形noiseの入力も変わります。それらの利用先全体の受け入れは、この限定契約に含めません。

## 維持する不変条件

- samplerはinputの評価contextを引き継ぎ、補間依存探索からもinputを落としません。必要なsamplerフィールド・mapper・名前参照の失敗を、このvariantの成功したゼロへ変えません。
- mapper境界とnoise評価は固定jarの数値幅・演算順に従います。中間値と最終値を分けて検査し、片方の誤りがもう片方を隠す比較を避けます。
- 正常系だけでなく、意図した故障でテストが落ちることと、source復元後の正常系を要求します。古いworldgen captureの再記録を修正の代用にしません。

評価と依存探索のownerは [`worldgen.rs`](../../../src/worldgen.rs) の `Gen::node` / `eval_c` / `eval0_c` / `push_kids`、noise構築は [`generation_math.rs`](../../../src/generation_math.rs) の `normal_new_amps` です。これらの境界を固定するテストは [`weird_sampler_tests.rs`](../../../src/weird_sampler_tests.rs) と[実クラスoracle](../../../tools/weird-sampler-oracle/)にあります。

一般のcodec、循環参照、cacheの生成器寿命、aquiferやsurface全評価はこの契約で閉じていません。残る順序と出口条件は[ロードマップ](../../ROADMAP.md)が持ちます。

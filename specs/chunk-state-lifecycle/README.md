# chunk stage の所有者を 1 つにする（carve stage）

Status: **実装済み**。これは [ロードマップ](../ROADMAP.md) が「段階1: chunk状態の整合性」に
置く最初の実装単位で、1 回の変更で閉じる carve stage の所有権だけを扱う。
根拠はこの木のテスト（`worldgen::wg_tests`）と `tools/mutation_check.sh` に残す。

## 直した欠陥（測定）

`src/worldgen.rs` では 1 chunk の carve stage が 3 つの容器に分かれていた:
`carved`（latch、上限なし）、`wg_after_carve`（carve 後の WG 高さ、4096 chunk）、
`feat_c`（block 出力、feature 層と同居する 384 chunk の LRU）。bound が違うので
「claim だけ残る」経路が 2 つあり、どちらも seed 4242 で再現した:

- `feat_c` の 384 が chunk (6,7) の carver 書き込みを落としても `carved` が残るため、
  `apply_carvers` は `begin_carve` で return する。同じ入力・同じ順序で FEATURES を
  やり直すと block 出力は 7,782 セル → 5,893 セル（24% が変わる）。
- `wg_after_carve` の 4096 が値を落としても `carved` が残るため、値は二度と再導出されず、
  読み手（`FC::heightmap`）は carve 前の scan を返す — 別 stage の答え。

## 所有者

`Gen::carve_c: HashMap<(i32, i32), CarveStage>` が唯一の所有者で、**record の存在が latch**。

```rust
pub(crate) struct CarveStage {
    wg: Option<Box<[i32; 512]>>, // carve 後の WG 高さ（4096 chunk 保持）
    blocks_live: bool,           // その pass の block 出力が feat_c に実在するか
}
```

遷移は 4 つだけで、この 2 フィールドを書く他の経路は無い:

- `begin_carve` — record が無い、または出力が落ちている → claim して `true`（pass を実行）。
  claim が実在する出力を指している → `false`。
- `set_wg_after_carve` — 値を書き、`carve_ord` で 4096 を回す（bound はここ 1 箇所）。
- `forget_feature_bucket` — `feat_c` の 384 bound が bucket を落とす唯一の点。
  bucket と `pop_done` と `feat_ord` の slot を落とし、`blocks_live = false` にする。
- `evict_carve_record` — 4096 bound が record を落とす唯一の点。record と
  （`forget_feature_bucket` 経由で）block 出力を同時に落とす。

`feat_ord` は「`feat_c` が持つ key の一覧」として両方の bound で同じ関数が更新するので、
queue と map がずれず、slot の重複も起きない。

## 寿命と、承認された容量

- WG 側は **4096 のまま**。384 へ縮めない。WG 値は carve 後の地形の scan なので pass を
  やり直せば同じ値が出るが、落とした block bucket からは読み直せない。だから feature 側の
  384 が WG の保持を切り詰めてはいけない。
- feature 側は **384 のまま**（容量変更なし）。ただし bucket を落とすときは claim も落とす。
  落とさないと次の FEATURES pass が carve 前の石を読み、出力が変わる。
- 4096 を超えた chunk は「まだ carve していない chunk」に戻る（値も claim も無い）。
  値が無い読み手は carve 前の scan を見るが、それは vanilla が CARVERS 未到達の chunk に
  答える値と同じで、claim が無いので次の生成で必ず pass が走り両方を作り直す。
  「claim は生きているのに値だけ永久に無い」が直した状態。

## 変えないもの

vanilla のアルゴリズム・RNG・順序・出力（キャッシュに圧力が無い限り）は不変。`Gen`/`FC` の
構造、`feature.rs`・`generation_context.rs` の呼び手、`feat_hm`、`shape_marks`、payload staging も
不変（bucket を落としても runtime へ渡す pending payload は落とさない）。並列化・
snapshot/merge・tick・キャッシュ容量には触れない。

## 検証

`worldgen::wg_tests` の 5 本が旧 tree で red、新 tree で green（
`carve_output_and_its_latch_share_one_lifetime` は実際の carve→FEATURES 経路）。
`tools/mutation_check.sh` に 4 つの mutant を置き、それぞれが対応するテストを赤にすることを
確認した（旧 `begin_carve` の mutant は新しい本体に合わせて置き換え）。native fixture
（`nether_carvers_1211`, `end_*_features_1211`）は入力を再生成せず、差分が出ないことを確認する。

# 世界乱数（N）

時計依存の `rand_u64()`（wall-clock の `SystemTime`/`subsec_nanos` を混ぜる）を全廃し、
`world_rng`（xorshift64*、seed 固定・状態保存）へ一本化した。目的は「clock を読む乱数が無い」
「保存・再読込後も列が続く」「消費順序と分布を検証できる」こと。
vanilla の乱数列そのものとの一致は対象外で、Q の残件に残す。

## 変更

- `fn rand_u64()` を削除。19 呼出し（`src/main.rs` 17、`src/landing.rs` 2）を
  `world_rng::next_u64()` に置換。各サイトの使い方（`% n`、`>> 40` で float 化）は変えない。
- `world_rng::saved_state()`/`restore()` を `pub(crate)` にし、保存形式へ `RNG1`（8バイト）
  セクションを追加。`FAL1` の後に追記し、無い旧セーブは seed 起点のまま読める。
- `tools/debt_ratchet.sh` の時計RNGラチェットを `SystemTime|subsec_nanos` の 0 上限へ変更。
  再導入は `rand_u64()` の再追加でなく **wall-clock 読み出し**として赤くなる。

## 検証

- `cargo test world_rng::`：固定ベクトル（seed 12345 の先頭4値）でアルゴリズムと消費順を固定。
  同 seed 再現、別 seed 相違、0 seed 非固定点、`next_below` の範囲全覆、状態ラウンドトリップ。
- `persistence_tests::the_world_rng_stream_continues_across_a_save_and_load_round_trip`：
  子プロセス隔離で save→別状態へ移動→load→続きが保存値から一致。`RNG1` の存在も検査。
- `round_trip` フィクスチャは RNG 状態も保存・復元し、非隔離の旧保存テストへ漏らさない。
- `tools/mutation_check.sh`：`RNG1` を無視する変異で上記テストが赤（`28 proved / 0 skipped`）。
- `tests/verify.sh` PASS：各 Rust 実行 121 passed / 1 ignored、ソース制御17、変異28、gragh 40辺0指摘。
- 層ゲートに `landing → world_rng`（下向き）の辺を追加。

## 限界

- 乱数源が clock から共有 seeded stream へ変わるため、各サイトの**分布は変わる**。
  これは計画が明記した意図的な変更で、vanilla の各 `RandomSource` 消費との一致は未検証。
- `RNG1` は後方互換（旧セーブに無ければ seed 起点）。vanilla が同一の状態を保存するかは未確認。
- 代表負荷性能、F（落下）、Q（全体レビュー・光座標・発光源探索）は未完了。

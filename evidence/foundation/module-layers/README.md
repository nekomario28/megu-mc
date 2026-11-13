# ソースに結びつけた依存ゲート

`deps.py` は手書きの辺ではなく、`tools/rust_module_graph.py` が得たRustの展開結果を読む。
`policy.json` は許可する辺・層・既知の指摘の所有者であり、観測グラフの代わりではない。
追加だけでなく削除でも方針との差を失敗にし、直した依存を再び許可し続けない。
`deps_target.py` は別の設計案。設計図が無循環でも、実装がその構造であるとは限らない。

## コンパイラが条件とmacroを解釈する

手書きのcfg/macro解釈では、入れ子import、test限定の定数、標準名を隠すmacro、
`stringify!` のデータを取り違えた。これらの解釈は撤去し、Cargo/rustcによる
**all-features・非testの展開**を使う。使っていないmacro定義のトークン木は参照に数えない。
実行時の結果や最適化後の呼び出しグラフを推定する検査ではない。

展開だけの子プロセスに `RUSTC_BOOTSTRAP=mc_spine` と診断用のprivate flagを与える。
[Rustの公式説明](https://doc.rust-lang.org/unstable-book/compiler-environment-variables/RUSTC_BOOTSTRAP.html)
どおり、この機能にstableの互換保証はない。Rust 1.98.1で実行検証し、利用不能・失敗・空出力は
非0にする。手書きモデルや古い展開結果へのフォールバックはない。
通常のサーバービルド、依存crate、呼出元の環境にはbootstrapを適用しない。
展開専用キャッシュから実行ファイルを選ぶこともない。

これは**フラットなbinモジュールの明示的なパスの射影**であり、型・名前解決の完全な検査ではない。
ワールド生成モジュールの親facade経由、間接的なself-crate alias、予約所有者名の衝突、
未分類のモジュールや入れ子は黙って省略せず失敗する。
生成以外の `use super::*` が持ち込む個別シンボルの依存は範囲外。
この境界を、Rust全体の依存や実行時の無循環の証明へ読み替えない。

## 検査器自身を赤にできること

`tools/layers_check.sh` が通常の入口。CIが所有する固定revisionの専用venvをPATHへ置く。
固定導入と事前のlocked Cargo fetchは `.github/workflows/ci.yml` にある。
ツール不在はSKIPでなく失敗。検査器には**独立した2つの循環**・層違反・禁止依存を与え、
コードと件数と終了値を検査する。旧検査器の「最初の循環しか報告しない」挙動も通さない。
この検査器修正は [gxkit PR #4](https://github.com/nekomario28/gxkit/pull/4) に分離してある。
PR公開とマージは別で、CIの参照は公開した正確なコミットに固定する。

`tests/layers_gate_test.py` は使い捨ての実ソースで、禁止importの追加・macro展開・
cfgと文字列の非依存・モジュール追加を確かめる。失敗は検査対象の指摘コードまで照合する。
コンパイラの空出力、古い/盲目な検査器、終了値のマスクも自己試験で拒否する。
必要なRust依存は先にfetchし、展開はlocked/offlineで行う。

## 現在の結果と残件

現行の所有者・辺・指摘の正本は `policy.json`、チェックポイントの実行結果は各receiptが持つ。
`java_compat ↔ landing` は、相互に使うメッセージ型の所有者を共有の下位へ移して解消した。
型の再exportや互換wrapperは残さず、コールバックや世界状態の所有者も増やしていない。
実ソースに逆向きの参照を戻す負例が、循環の指摘で失敗することを検査する。
共有JSONとseeded数学も、地形生成から独立した下位の所有者を持つ。
これらは既存実装の移動であり、解析結果を使ってパース規則や乱数列を作り直す変更ではない。
数学のシード記述値には既存消費者が必要とするcrate内可視性だけを与え、
RNGインスタンスやキャッシュの所有者・消費順を増やさない。
両下位モジュールへ地形生成の参照を戻す負例は、正常系→対象の層違反→復元正常系で検査する。
これは時計依存RNGの移行や保存Nを閉じるものでもない。

配置実装の選択はアプリケーションの呼出境界が持ち、地形のlookupはその処理を引数で受け取る。
保持callback、trait object、追加の世界状態や選択モードは作らない。
近傍を巡回する順序、完了印、キャッシュ更新の時点はlookupの同じ経路に残す。
実配置と直接配置の全上書き値を比較し、負座標・重複照会・境界越えの順序と再利用も返却値で検査する。
この静的依存の逆転を、実行時の呼出グラフが無循環であるという主張にはしない。

feature/treeの循環とtreeの上向き依存も、配置と樹木が共有する契約を下位へ移して解消した。
registry（状態・metadata・タグ）は `block_registry.rs`、値/高さ/状態providerとpredicateは
`generation_providers.rs`、1本のGen借用と上書き・heightmap・biomeキャッシュは
`generation_context.rs` が所有する。既存定義の移動であり、式・分岐・反復順・RNG消費・
保存形式を変えず、旧feature側の再exportや互換wrapperは残さない。
現行のモジュール集合・許可辺は `policy.json` を正本とし、件数はsource-bound gateの実行結果で確認する。accepted findingsは空である。
移動前後を独立プロセスで2回比較した全上書き値と、provider/predicateの値とRNG消費を固定した
fixture、実population出力のMD5対照が保存を検査する。これはvanilla準拠や代表負荷の証明ではなく、
T/F/N/Qと既存ignoredテストは別の未完了契約として残る。

# 復元と変更の境界

保存元のmc-spineサブツリーはv2.37、元の履歴識別子は `860142fa21e5c77ca3818d9e331756fa0df72b13`。独立したGitリポジトリとして再開するため、復元ソースを最初のコミットとした。元の大きなリポジトリ全体の履歴や、稼働ワールド・旧バイナリは取り込んでいない。

元の保存場所のローカル補助資産 `assets/properties.json` と `resourcepack.zip` を含む(当時の記述。`assets/properties.json` は後に削除され、状態データは `assets/block_states_1211.json` に置き換わった。THIRD_PARTY.md 参照)。`resourcepack.zip`、`config_packets.bin`、`datapacks/vanilla/` はその後コミットから外され、jar から生成する形になった(THIRD_PARTY.md §9)。ソース復元時のビルドと9個の既存テストは合格した。過去のクライアントfixture結果には対応するソースSHAがないため、現在の版の互換性証拠として使わない。

今回の追加は、依存値に対する単一エントリの共通キャッシュと、かまどの固定精錬・燃料判定への適用。動的datapack判定を省略しない。新規かまどと保存から復元したかまどは、どちらも空のキャッシュから始まる。

検証ではv2.37のかまどtick本体を参照実装として固定し、通常投入、出力満杯・入力変更、datapack追加・削除とtag変更の3シナリオを各160tick比較する。入力・燃料・出力の実値、burn_left、progress、XPを各tick照合し、通常の出力生成tickが49・89・129であることも確認する。固定検索失敗の1,200回の繰り返しでは計算が1回になり、キー変更で即時再計算することを独立して確認する。

これはmc-spine内部の既存動作維持の確認であり、Vanillaとの精錬時間の一致やクライアント通信の証明ではない。実サーバーへの接続、保存・再読込の統合試験、ワールド生成全体のVanilla比較、mc-spineのCPUベンチマークは未実施。別実装であるPumpkinの性能測定値は、この実装の性能値として使用しない。

## 2026-10-05のチェック

`cargo test --offline --locked -j 1 -- --test-threads=1`: 13 passed、0 failed（既存9、新規4）。チェック時はMemoryMax=512MiB、MemorySwapMax=0、CPUQuota=50%に制限した。新規2モジュールのrustfmtチェックと差分の空白チェックは合格。

`cargo build --offline --locked -j 1` も合格。依存キー比較を無効にする負の対照は新規キャッシュテスト2件で失敗し、退行を検出した。[実行ログとソースハッシュ](evidence/validation.json)を同梱する。Clippyと既存ソース全体のrustfmtは未実施。

独立した読み取り専用レビューでも、修正を要する退行は指摘されなかった。静的レビューの判断と実行テストの結果は区別して記録した。

# CLIで接続とゲーム操作を検査する

実画面とKDE経由の試験は行わない。使い捨て世界のサーバーに、資格情報を持たない
protocol 767のCLIクライアントを接続する。入口は `tests/gameplay_wire.py`、
通信・復号の共有実装は `tests/protocol767.py`。

設定レジストリを読み、チャンクのパレット・ブロック・バイオーム・光マスクと配列を復号する。
座標を送信した結果は、実際の送信・アンロード集合で判定する。
破壊は更新パケットを受け取るだけで閉じない。新しい接続でチャンクを再取得し、
予測や更新キャッシュを引き継がずに、サーバー側でセルが消えたことを確認する。
チャットは送信者と本文が返ることを確認する。

## 緑の条件

`tests/verify.sh` は復号器の既知解・負例、実通信、変異をまとめて実行する。
変異は **正常系が成功 → 狙った契約で失敗 → ソースを戻して再ビルドした正常系が成功**
したときだけ検出成功に数える。ワイヤ試験の失敗には契約識別子を要求し、
無関係なassert・起動失敗・古い実行ファイル・失敗した復元対照を検出成功に数えない。
この終了契約自体も `tests/gate_scripts.py` の隔離した偽バックエンドで試す。

`receipt.json` は今回の入力ソース識別子と結果。これはCLI経由の通信と操作の証拠であり、
画面の描画検証や、全ゲーム要素・vanillaとの全挙動一致・代表負荷での性能優位は主張しない。

## M2 gameplay live wire roundtrips

`tests/m2_gameplay_wire.py` は通常の `target/debug/mc-spine` を起動し、
使い捨てワールドにprotocol 767のCLIクライアントで接続する。
`tests/verify.sh` からも同じテストを実行する。

実行コマンド（リポジトリ直下）

```bash
mkdir -p target/tmp
export TMPDIR="$PWD/target/tmp"
cargo build --offline --locked -j 2
PYTHONDONTWRITEBYTECODE=1 python3 tests/m2_gameplay_wire.py
```

- **時刻・天候** `/gamerule doDaylightCycle false` と `/gamerule doWeatherCycle false`
  の後に `/time set 12345` を送り、`0x64` Update Timeを確認する。
  `/weather rain`、`/weather thunder`、`/weather clear`、再度`thunder`を実行し、
  `0x22` Game Eventで雨の開始・終了と雷の強度を検証する。
  別クライアントのログインとサーバー再起動後に、時刻12345・雨・雷が復元されることを確認する。
- **クラフト・製錬** クリエイティブ在庫へoak logを入れ、2×2クラフトのスロットを
  クリックしてoak planks×4を生成、出力を取り、原料が消費されることを確認する。
  `/setblock` でかまどを置き、raw ironとcoalをスロットに入れ、
  燃焼・製錬後のiron ingotを `0x13` Window Itemsで確認する。
  保存・サーバー再起動後に同じかまどを開き、iron ingotの永続化と取り出しを検証する。
- **戦闘・死亡・リスポーン** 別々のEntity IDを持つ2クライアントを接続し、
  対象をsurvivalへ変更して `0x16` Interact（attack）を送る。
  `0x24` Hurt Animationと `0x5d` Update Healthで体力減少を確認する。
  `/kill` によりHealth≦0となることを確認後、`0x09` Client Status
  （perform respawn）を送信し、`0x47` Respawn、新しい `0x40` Position、
  正の体力の復帰を確認する。

2026-11-20 の観測結果（ローカル、使い捨てワールド）

```text
{"crafting": "oak_log -> 4 oak_planks", "death": true, "furnace_saved": true, "protocol": 767, "pvp_damage": true, "respawn": true, "smelting": "raw_iron -> iron_ingot", "time": 12345, "transitions": ["rain", "thunder", "clear"], "weather": "thunder"}
m2 gameplay wire: PASS
```

実通信で発見した不具合として、log→planksのレシピが古いアイテムIDを返す問題と、
Join Gameが全プレイヤーにEntity ID=1を通知する問題を修正した。
この結果は記載のライブ操作・再接続・再起動永続化の確認範囲に限る。

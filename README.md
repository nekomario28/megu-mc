# mc-spine

Minecraft Java 1.21.1（protocol 767）向けの実験的なRustサーバー。保存されていたv2.37のソース、datapack、プロトコル設定を復元した。Minecraft全体との互換性が完成した実装ではない。

目標は、**小規模・低負荷の世界では計算量を大きく減らし、必要な既存Modも使えるサーバー**。処理を省略できる条件・状態の所有・Modの呼出しを一貫して扱う。[目的と設計原則](DESIGN.md)に方針、構造の方向、実装済みの範囲と評価条件を記載する。

Cargoのpackage version `0.0.0` は保存時の値を維持している。`AUDIT_WIRE.md` と `SECURITY_NOTES.md` は当時の監査記録であり、現在の安全性や完成度を保証する資料ではない。

## 現在地（2026-10-20）

目標は vanilla と同一の world 生成。到達点は測った範囲だけを書く。

| 項目 | 状態 |
|---|---|
| 地形（feature/carver なし） | vanilla と **0 セル差**（seed 0、chunk (0,0)、(0,1)、(-1,-1)、(-2,0)）。density・surface rule・bedrock・光の非一様区間 digest が一致 |
| feature 層 | 10302 / 98304 セル差（10.48%、chunk (0,0) の offline 比較）。placement modifier を vanilla の stream 順に直して 11017 → 10302。biome リストと block tag は 1.21.1 の jar に一致済み。残差は石系 ore の実行側 |
| 軽量性（M3） | **未達**。代表負荷で CPU x1.10（8 回中央値、RSS は約 1/8）、spawn burst を除くと x1.79 |
| 必須ゲート | `tests/verify.sh` がローカルで PASS（148 passed / 1 ignored、mutation 53 proved） |
| hosted CI | 支払い/上限ブロックで job が起動しない（コード失敗ではない）。ローカル verify で代替中 |

順序と出口条件は [ロードマップ](specs/ROADMAP.md)、欠陥の状態は
[close-defects](specs/close-defects/README.md) が持つ。数値の根拠は
[地形](evidence/gameplay/terrain-parity/README.md)・
[feature](evidence/gameplay/feature-parity/README.md)・
[費用](evidence/gameplay/worldgen-cost/README.md) の記録にある。

## ビルドと確認

```sh
cargo build --locked
cargo test --locked -- --test-threads=1
```

必須ゲートは `bash tests/verify.sh`。出力を整形しても、呼び出すシェルが失敗の終了値を保持する：

```sh
set -euo pipefail
bash tests/verify.sh > verify.log 2>&1
# この行以降へ進めるのは全必須ゲート成功時だけ。
```

`verify.sh | tail` を終了判定に使わない。スキップ・未実行の必須変異も検証失敗として扱う。
[残る欠陥の完了契約](specs/close-defects/README.md)を参照。

設定とdatapackは相対パスで読むため、リポジトリ直下から実行する。既存ワールドは復元ソースに含めていない。

```sh
MC_SPINE_PORT=25699 cargo run --locked
```

ゲームlistenerは安全のため既定で `127.0.0.1` にbindする。resource pack HTTPはポート25598を利用する。起動前にポートの使用状況を確認する。`MC_SPINE_WG=old` で旧world generatorを選択できる。

## 依存値が安定した処理の共通化

`CachedLookup<K, V>` は直近の完全な依存キーと純粋な結果を一組だけ保持する。検索失敗も保持し、キーが変わればその呼び出しで再計算する。同期は呼び出し側が担う。

かまどの固定精錬結果と燃料時間に適用した。datapackレシピとingredient tagは変更可能なので、精錬候補の判定時に引き続き確認する。tick、燃焼時間、進捗、インベントリ更新は既存の処理で実行する。キャッシュは保存せず、ワールド読込時に空で作成する。

他の処理へ適用する際は、計算が純粋であること、キーがすべての依存を含むこと、変更と同じ呼び出しで再計算されることを確認する。変更可能なテーブルの参照を含める場合は、そのスナップショットと整合するrevisionも必要になる。

[復元と検証の記録](RESTORATION.md)を参照。

## ライセンスと第三者データ

このリポジトリのコードは MIT（[LICENSE](LICENSE)）。ただし `datapacks/vanilla/` の
worldgen データ、`assets/` と `resourcepack.zip` の Mojang 由来資産、
`config_packets.bin` は MIT の対象ではない。1.21.1 の互換検証に使う派生物であり、
再配布・利用条件は Mojang の EULA と同データの出所に従う。Pumpkin など他実装の
GPL コードは取り込んでいない（[#3 の由来メモ](https://github.com/nekomario28/mc-spine/issues/3)）。

## ゲーム時間と処理省略

[時間契約の実装計画](specs/time-contract/README.md)に従い、vanillaの結果を保つ時間基準と、
結果を変えない処理省略を分けて検証する。20TPS移行・全体互換・性能改善はまだ未完了。

## Java mod互換層

[段階設計](specs/mod-compatibility/README.md)と[Fabric接続の診断](compat/fabric-probe/README.md)を参照。世界の正本はRustに置き、Java互換層からJNIで操作する。通常ビルドではJava実行機能を有効にしない。現在対応する契約と未検証の境界は以下に記載する。

## Selected Java mod contract

Java compatibility is opt-in. The pinned Seed Protect Fabric artifact and its NeoForge event counterpart now reach the normal player/adult-cow farmland landing path. Rust keeps the only authoritative world; one synchronous JVM owner executes callbacks, and a native commit replaces farmland only if its original state still matches. Cancellation preserves farmland while existing fall damage continues.

Build and validation instructions are in [Fabric contract](compat/fabric-probe/README.md) and [NeoForge event adapter](compat/neoforge-events/README.md). Set exactly one of `MC_SPINE_FABRIC_CLASSPATH` or `MC_SPINE_NEOFORGE_CLASSPATH` to its built absolute classpath-file path, and use the selected original JAR alone in that server working directory's `mods/`. Unknown mod bytes and required transformation failures reject startup before listeners open. Without those settings, the default Rust server loads no JVM.

This supports one pinned feature contract, not arbitrary Minecraft mods. Full FML/NeoForge startup, all-entity collision physics, crop/loot behavior and general Minecraft API object identity remain outside the verified scope. See [compatibility specification](specs/mod-compatibility/README.md) for the open gates.

## ネットワーク・権限・保存上の注意（Issue #1）

この実装は**オフラインモードの開発用サーバー**であり、Mojang/Microsoftによるプレイヤー認証は**行っていない**。インターネットへ直接公開しない。LAN等でゲームポートを明示的に開く場合は、ファイアウォールと信頼できる接続元の制限を用意する。

- `MC_SPINE_BIND_ADDR`（既定 `127.0.0.1`）: ゲーム接続の待受アドレス。LAN向けに `0.0.0.0` とする場合もオンライン認証が追加されるわけではない。
- 一般プレイヤーがゲーム内で実行できるコマンドは `/help`、`/list`、`/time query`（または `get`）に限定。管理コマンドはゲーム内から許可しない。**ローカルの実験専用**に限り `MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS=1` で旧動作へ戻せる（公開・共有サーバーで使用しない）。
- `MC_SPINE_RCON_PORT` を設定した場合、非空の `MC_SPINE_RCON_PASS` が必須。RCONのbind先は `MC_SPINE_RCON_BIND_ADDR`（既定 `127.0.0.1`）、同時8接続上限、読み書き各10秒のtimeout。RCONのコマンドは管理権限扱いであり、プロトコルは平文なので外部公開しない。
- ログイン前の通信は接続開始から合計30秒で期限切れとし、pre-playフレームは64KiBに制限する。
- `MC_SPINE_RESOURCE_PACK_URL` には、クライアントがアクセスできるHTTP(S) URLを指定できる。ゲームlistenerがloopback以外の場合、未設定ならリソースパックを案内しない。リソースパックHTTPのbind先は `MC_SPINE_RESOURCE_PACK_BIND_ADDR`、未設定なら `MC_SPINE_BIND_ADDR` を利用する。
- ゲームロジックの呼び出し周期は50ms（**20TPS目標**）とし、ワールド時刻も1 tickに1進める。負荷時に20TPSを保証するものではない。Vanillaとの差分・負荷を実機で比較する必要がある。
- 新規保存は `SPN4` 形式のサイズ・SHA-1チェック付き外側フレームを使用する。既存の `world_overlay.bin` は旧形式として読める。保存更新時に `world_overlay.bin.bak` へ旧ファイルをコピーする。新形式に破損が見つかったときはロードを拒否し、元ファイルを保持する。**旧形式の部分破損の検出とトランザクショナル復旧は未完成**。運用前に別途バックアップを取る。

CIは `.github/workflows/ci.yml` にて `cargo build --locked`、`cargo test --locked -- --test-threads=1`、Java互換機能の`cargo check`を実行する。CI成功はVanillaとの互換性・ネットワーク負荷耐性・20TPS実測を保証しない。

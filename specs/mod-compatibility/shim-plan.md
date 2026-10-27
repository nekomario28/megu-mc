# Mod互換shim計画（S0〜S5）

状態: **計画のみ。未着手。** 根拠は2026-10-25の実現性調査（Seed Protect実測、modの表面積の概算、先行事例）。数値は調査時点のもので、各マイルストーンで再測定する。出口条件は[ROADMAP R5](../ROADMAP.md)と、このディレクトリの[README](README.md)の契約に従う。PLANのM2/M3と区別するため、ここではS番号を使う。

## 方針

- 目標は「どんなModでも動く」ではなく、**版固定・明示的・動かないものは起動時に拒否する互換プロファイル**。
- shimは `net.minecraft` の必要最小限の面だけを持つ。vanillaのクラスは読み込まない（Rustがワールドを持つ）。
- shimの骨組み（クラス名・メソッド署名・フィールド）は、**ユーザー自身のserver jarと公式マッピングから手元で自動生成**する。Mojang由来なので生成物はリポジトリに置かない。FabricはintermediaryとNeoForgeはMojang名の、2種類の投影が必要。
- 骨組みで解決できるのは「対象が存在しない」エラーまで。メソッド本体の途中を狙う注入（`@At("INVOKE")`、`@Redirect`、`@ModifyVariable` など）は、本物のバイトコードの形が要るため別問題として残す。
- NeoForgeを先、Fabric（M4相当）を後にする。NeoForgeのアダプタはFabricより約35 MiB・約300 ms軽い（調査時の実測）。

## 計測プロトコル（全S共通）

seed 0、view/simulation distance 4、プレイヤー0人とN人、datapackのworldgenが有効であることをログで確認（平らなワールドで測らない）。RSSは `/proc/<pid>/status` の `VmRSS`・`VmHWM`・`RssAnon`・`RssFile` を12秒後と負荷後に取る。起動はプロセス開始から RCON `list` が返るまで。tick時間はp50/p95/p99。基準は (a) mc-spineのみ、(b) JVMのみ・Modなし、(c) **NeoForge 1.21.1 + Lithium + FerriteCore（ユーザー側で実測）**、(d) vanilla 1.21.1。JVMフラグ・JDK・ロード済みクラス数も記録する。

## マイルストーン

| S | 内容 | 合格条件 |
|---|---|---|
| S0 | `shimcheck`: mod jarをASMで読み、必要な `net/minecraft` のクラス/メンバー/署名、`@Mixin` 対象と注入の種類、AT/AW、レジストリ/イベント利用を一覧にし、shimの宣言面と差分を出す。実行時の変更なし（1〜2日） | Seed Protect（Fabric/Neo）で100%対応と判定。Lithiumなどでは、動かない理由を明示した拒否一覧が出る |
| S1 | NeoForgeで、ゲームバスに2〜3個のリスナ（参加・ブロック破壊・チャット）と、shimメソッドへのHEAD/RETURN注入を持つ単純なModを動かす（約1週間） | 無改造jarが実クライアントのセッションで正しく動く。署名不一致は起動時に拒否。調整したJVMで、RSSがネイティブ+60 MiB以内、起動は+250 ms以内（NeoForge） |
| S2 | ブロックとアイテムを登録して保存するコンテンツMod（3〜5週）。レジストリ、`Block`/`Item`/`Properties`、`BlockState`、パレット/ネットワークID、データコンポーネントとコーデック、`DeferredRegister` の最小限のライフサイクル | 置いたブロックが再起動後も残る。NBT/パレットがNeoForgeの書き込みと一致（実NeoForgeでの読み込みで確認）。登録1ブロックあたりのクラス数とRSS増加を測る |
| S3 | ブロックエンティティ、インベントリ、tick（5〜8週）。NBT読み書き、コンテナ、`scheduleTick`/ランダムtick、カスタムペイロード | 再入とハンドル寿命の規則を定義して検証。JNI呼び出し頻度のストレステストを行う |
| S4 | Fabric API部分集合（6〜10週）。選んだモジュール（ライフサイクルイベント、コマンドAPI、ネットワーク、レジストリ同期）を載せるか、ネイティブ実装で置き換える | 人気のあるサーバー側Fabric mod 5個が、無改造で動く |
| S5 | ワールド生成フック／エンティティ（未定） | S2〜S4の実測で、範囲に入れるかを判断する。おそらく範囲外 |

## 先にやること

1. このページと[ROADMAP R5](../ROADMAP.md)の更新（このPR）。
2. S0 `shimcheck`。
3. ユーザー側でのNeoForge 1.21.1 + Lithium + FerriteCoreの基準測定。

## 範囲外・投資しないもの

Lithium系の性能Mod、Create規模のMod、GraalVM native-image（実行時にMixinがバイトコードを生成するため）、vanillaクラスの遅延読み込み（「vanillaからワールドを引いたもの」になり、節約効果が不明。やるなら、ユーザー提供のjarを使う測定実験として）。

## 主なリスク

署名と継承階層の再現度（Modごとに面が広がる）、FabricとNeoForgeの名前の二重化、FML/ModLauncher/JPMSが動かないこと、レジストリ・パレット・ネットワークIDのRustとの一致、データコンポーネント/コーデック/DFUによるJVM側の肥大化、スレッドと再入、JNIの呼び出しコスト（未測定）、実コンテンツを載せたときのメモリ増加（現在の82〜115 MiBはModが1個・レジストリなしの値で、実コンテンツでは150〜300 MiBもあり得る）、版ごとの保守。ライセンスは、Mojangのデータとマッピング、Pumpkin/PatchBukkit（GPL-3.0）は読むだけでコピーしない、Lithium（LGPL-3.0）は試験入力としてのみ使用。

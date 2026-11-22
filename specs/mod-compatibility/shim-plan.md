# Mod互換shim計画（S0〜S5）

状態: **S0〜S3+ 設計確定済み（[NeoForge 1.21.1 互換層アーキテクチャ設計](neoforge-architecture.md)）。NF-0 (`shimcheck`) 着手待機。** 根拠は2026-10-25の実現性調査（Seed Protect実測、modの表面積の概算、先行事例）。数値は調査時点のもので、各マイルストーンで再測定する。出口条件は[ROADMAP R5](../ROADMAP.md)と、このディレクトリの[README](README.md)の契約に従う。PLANのM2/M3と区別するため、ここではS番号を使う。

## 方針

- 目標は「どんなModでも動く」ではなく、**版固定・明示的・動かないものは起動時に拒否する互換プロファイル**。
- shimは `net.minecraft` の必要最小限の面だけを持つ。vanillaのクラスは読み込まない（Rustがワールドを持つ）。
- shimの骨組み（クラス名・メソッド署名・フィールド）は、**ユーザー自身のserver jarと公式マッピングから手元で自動生成**する。Mojang由来なので生成物はリポジトリに置かない。FabricはintermediaryとNeoForgeはMojang名の、2種類の投影が必要。
- 骨組みで解決できるのは「対象が存在しない」エラーまで。メソッド本体の途中を狙う注入（`@At("INVOKE")`、`@Redirect`、`@ModifyVariable` など）は、本物のバイトコードの形が要るため別問題として残す。
- NeoForgeを先、Fabric（M4相当）を後にする。NeoForgeのアダプタはFabricより約35 MiB・約300 ms軽い（調査時の実測）。

## 計測プロトコル（全S共通）

seed 0、view/simulation distance 4、プレイヤー0人とN人、datapackのworldgenが有効であることをログで確認（平らなワールドで測らない）。RSSは `/proc/<pid>/status` の `VmRSS`・`VmHWM`・`RssAnon`・`RssFile` を12秒後と負荷後に取る。起動はプロセス開始から RCON `list` が返るまで。tick時間はp50/p95/p99。基準は (a) meguのみ、(b) JVMのみ・Modなし、(c) **NeoForge 1.21.1 + Lithium + FerriteCore（ユーザー側で実測）**、(d) vanilla 1.21.1。JVMフラグ・JDK・ロード済みクラス数も記録する。

## マイルストーン

| S | 内容 | 合格条件 |
|---|---|---|
| S0 | `shimcheck`: mod jarをASMで読み、必要な `net/minecraft` のクラス/メンバー/署名、`@Mixin` 対象と注入の種類、AT/AW、レジストリ/イベント利用を一覧にし、shimの宣言面と差分を出す。実行時の変更なし（1〜2日） | Seed Protect（Fabric/Neo）で100%対応と判定。Lithiumなどでは、動かない理由を明示した拒否一覧が出る |
| S1 | NeoForgeで、ゲームバスに2〜3個のリスナ（参加・ブロック破壊・チャット）と、shimメソッドへのHEAD/RETURN注入を持つ単純なModを動かす（約1週間） | 無改造jarが実クライアントのセッションで正しく動く。署名不一致は起動時に拒否。調整したJVMで、RSSがネイティブ+60 MiB以内、起動は+250 ms以内（NeoForge） |
| S2 | ブロックとアイテムを登録して保存するコンテンツMod（3〜5週）。レジストリ、`Block`/`Item`/`Properties`、`BlockState`、パレット/ネットワークID、データコンポーネントとコーデック、`DeferredRegister` の最小限のライフサイクル | 置いたブロックが再起動後も残る。NBT/パレットがNeoForgeの書き込みと一致（実NeoForgeでの読み込みで確認）。登録1ブロックあたりのクラス数とRSS増加を測る |
| S3 | ブロックエンティティ、インベントリ、tick（5〜8週）。NBT読み書き、コンテナ、`scheduleTick`/ランダムtick、カスタムペイロード、**バッチ／スナップショット JNI 境界** | 再入とハンドル寿命の規則を定義して検証。1セル毎のJNI往復を禁止し、セクション/差分バッチ境界とJNI呼び出し頻度のストレステストを行う |
| S4 | Fabric API部分集合（6〜10週）。選んだモジュール（ライフサイクルイベント、コマンドAPI、ネットワーク、レジストリ同期）を載せるか、ネイティブ実装で置き換える | 人気のあるサーバー側Fabric mod 5個が、無改造で動く |
| S5 | ワールド生成フック／エンティティ／重量Mod向け汎用オフロード拡張（未定） | S2〜S4の実測で、範囲に入れるかを判断する |

## 先にやること

1. このページと[ROADMAP R5](../ROADMAP.md)の更新（このPR）。
2. S0 `shimcheck`。
3. ユーザー側でのNeoForge 1.21.1 + Lithium + FerriteCoreの基準測定。

## 将来拡張：汎用オフロードカーネル＋重量 Mod 向けネイティブ加速（S3 以降）

Create・Mekanism・AE2 等の重量 Mod は、別プロジェクトへ切り出さず **`megu` 本体（[M3 基盤](../chunk-parallelism/README.md)の `PalettedSection`・空間索引・[`tick_queue`](../../src/tick_queue.rs)・差分配信）の上に「汎用オフロードカーネル（7〜8 割）＋ Mod 固有薄型アダプタ（2〜3 割）」として統合**する方針を採る（純 Vanilla コア層との境界は [`tools/layers_check.sh`](../../tools/layers_check.sh) で隔離する）。

1. **全 Mod 向けに汎化する 5 つの共通カーネル**
   - **データ駆動コンテンツ層**: Mod JAR 内のブロック/アイテム/レシピ/タグ/DataComponent を Java を動かさず Rust 側へ直接登録・配信。
   - **汎用「接続グラフ／パイプ網」カーネル**: 毎 tick 走査を廃し、隣接変更イベント時のみ連結成分・応力/帯域/電力配分を解く（Create Kinetic、AE2 チャンネル、Mekanism/Thermal ケーブル・パイプ等に共通）。
   - **汎用「イベント駆動・スロット輸送／レシピ加工」カーネル**: 入力空・出力満杯の機械/ベルト/パイプを自動スリープさせ、隣接または在庫変更イベント時のみ起床。
   - **汎用「Contraption / 移動複合構造」フレームワーク**: 列車・採掘機等の移動ブロック群を軽量パレット塊＋統合 AABB として Rust 側の衝突・同期に載せる共通枠組み。
   - **バッチ JNI 境界＋リージョン負荷隔離**: チャンク／セクション単位のスナップショット受け渡しと BlockEntity 差分バッチ同期により、特定領域の巨大工場が他領域の tick・通信を巻き添えにするのを防ぐ。
2. **Mod 固有アダプタ（S3 以降の段階適用）**
   - 任意の `@Mixin` や独自バイトコードまで自動解析・自動変換はしない。軽量〜中量 Mod は S0〜S4＋バッチ JNI で動かし、Create 等の超重量級 Mod については S3 基盤確立後に「上記汎用カーネル群＋Create 固有ストレス/レシピ規則のみを受け持つ薄型ネイティブ加速アダプタ」として段階導入を判断する（版追従コストとの兼ね合いを実測で評価）。

## 範囲外・投資しないもの

Lithium系の性能Mod（M3 P1〜P5 で Rust 本体が代替）、**S0〜S3 段階における Create 規模の Mod（S3 のバッチ JNI・汎用オフロードカーネル確立後に段階対象化）**、GraalVM native-image（実行時にMixinがバイトコードを生成するため）、vanillaクラスの遅延読み込み（「vanillaからワールドを引いたもの」になり、節約効果が不明。やるなら、ユーザー提供のjarを使う測定実験として）。

## 主なリスク

署名と継承階層の再現度（Modごとに面が広がる）、FabricとNeoForgeの名前の二重化、FML/ModLauncher/JPMSが動かないこと、レジストリ・パレット・ネットワークIDのRustとの一致、データコンポーネント/コーデック/DFUによるJVM側の肥大化、スレッドと再入、JNIの呼び出しコスト（未測定）、実コンテンツを載せたときのメモリ増加（現在の82〜115 MiBはModが1個・レジストリなしの値で、実コンテンツでは150〜300 MiBもあり得る）、版ごとの保守（重量 Mod アダプタを含む）。ライセンスは、Mojangのデータとマッピング、Pumpkin/PatchBukkit（GPL-3.0）は読むだけでコピーしない、Lithium（LGPL-3.0）は試験入力としてのみ使用。


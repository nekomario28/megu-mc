# Worldgen parity: 最小の因果差分から進める手順

状態: 2026-11-13 の手順整理。実装・native実行・全体合格の宣言ではない。
順序の正本は [ROADMAP](../ROADMAP.md)、項目の状態は
[実行台帳](../implementation-checklist.md)、測定は該当する evidence を参照する。

## 1. 最初に固定する入力と証拠

- 対象source SHA、実際に使ったbinaryのpath/hash、資産のdigest、固定1.21.1 jarのhash。
  CargoのJSON出力からartifactを取得し、推測した古いtargetを使わない。
- seed、dimension、datapack、structures設定、対象chunk/window、source順、halo/margin、
  chunk status、時計と保存時点。未取得の値は「不明」と書き、同一条件とみなさない。
- artifactごとにstart/finish、生成stage、writer、座標系、min_y/height、件数、SHA-256、
  skipしたchunk/config/modifier/referenceの名前を保存する。
- 保存されたFULL chunkも「runtime tick前」を自動的には意味しない。
  terrain、carver後、structure/feature後、runtime後の比較を混ぜない。
- nativeの独立した複数runを別artifactとして保持する。観測したrun間差は
  許容マスクでも普遍的なノイズ床でもない。片側だけ都合のよい参照へ取り替えない。

## 2. 既存captureから、最初に違うstageを探す

1. まず既存world/regionとdumpを読む。新規server起動や全chunk再生成は、
   既存artifactだけでは必要なstage/情報を得られないと確認した後に限る。
2. 名前だけでなく全プロパティを含むcanonical stateを、同じ絶対座標・高さ範囲で比較する。
   airの省略表現とair種別の扱いも明記し、全volumeと非air行数を混同しない
   （[chunk_triage.py](../../tools/chunk_triage.py) で `cave_air` 区別・structure BB 帰属・`--world-b` ゆらぎ分離を1コマンドで確認できる）。
3. 座標集合と置換方向で差を分ける。最終stateがairというだけでcarver、
   planksというだけで特定構造物、同じ形というだけでRNG原因と断定しない。
4. 対象セルを作り得るwriterを列挙し、両側で最初に分岐するstageの前後を比較する。
   中間stageが無ければ、そのstageだけを観測する限定probeを計画する。
5. 対象のownerにcounter/traceを付けて本番経路の到達を確認する。
   bit-identicalな結果だけを「この仮説は無効」の証拠にしない。

### biomeを比べるときの入力契約

[Gen::biome_name_at](../../src/worldgen.rs) はquart-sampled biome、
同ファイルの `Gen::biome_blended` はblock位置から周囲8点を順位付けする
BiomeManager相当の問い合わせ。保存されたquart paletteを、そのままblockのblended biomeと
比較しない。呼び出し位置（下の草か上の空間か）、高さ、lookup方法、温度の参照、
判定stageとwriterを両側で記録する。

現在のsnowy配置は、[feature owner](../../src/feature.rs) の `Cf::FreezeTop`（`snow_can_survive` と `HeightmapKind::MotionBlocking`）が担当する。surface列を後から書き換える旧 `Gen::apply_snowy` は植物が上にある 18 セルを誤って `snowy=true` にしていたため撤去した。port内の独自温度テストだけで、nativeのsnowy規則やbiome自体の正しさを認定しない。

## 3. 仮説を実装する前の反証チェック

各仮説は「観測事実 / 仮説 / 反証条件 / 次の最小probe」に分けて記録する。
他のtreeに同名のsymbolがあっても、両body・call site・入力・出力・書込みkeyを確認する。

**今回測定した訂正:** seed 4242の残差82セルのうち32 air/stoneと18 snowyを、
位置の近さから「50セルはbiomeが選ぶcarverリストの違い」と推測した。
しかし固定native一覧と同梱データでは、Overworldの53 biomeすべてが同じ順序の
`cave, cave_extra_underground, canyon` を持つ。通常のOverworld内でのbiome取り違え
だけではそのリストは変わらない。近傍seedの17×17走査も既存実装にあった。
この事実はcarver全体の正しさや、他の共通原因の不存在を証明しない。

検出元は `evidence/foundation/biome-carvers/vanilla.json` と同梱biome JSONのgroup比較。
最も早く確認できた場所は、原因を記す前の**消費者のlookup/listと両bodyの確認**だった。
以後、同じ帯にあるだけで原因を統合せず、リストが変わると主張するなら実際の両リストを
先に出す。[残差記録](../template-finalization/slices/04-chunks.md)もこの区別を採用する。

## 4. 小さい対照から、本番の故障検出へ

1. 同じ入力でnative実クラスとRustの独立比較を作る。unsupportedをfalse/空に読み替えない。
2. 本番のloader/dispatch/ownerを通し、正の書込み・イベント・実行回数を確認する。
3. 最初の因果差分だけを修正し、元のcapture/期待値と失敗runを保持する。
4. 新テストをpre-change moduleへ再生する。そこで通るなら、そのテストだけでは修正を証明しない。
5. compiling mutantで同じ不具合を注入し、テストの実失敗を確認する。
   compile error、0件選択、ignored、アサーション自体を弱める変異は成功に数えない。
6. 正常系→変異→復元後正常系を確認する。必要な反復は `MUTATION_STABILITY=1` を使う。
   `MUTATION_FILTER` は対象testのfilter部分文字列で選択する（説明文ではない）。
7. state、RNG消費、順序、予約、実配送、実効果、保存/再読込、wire/lightは別の出口を持つ。
   helper/proxyの緑で実Levelでのentity生成・流体物理・全chunk受け入れを閉じない。

## 5. 計算予算と広い回帰

- 調査中は対象testとfocused mutationを使う。独立した軽いファイル調査はbatch化するが、
  大量の短命subagentや全native matrixの反復起動をしない。
- 2026-11-12の全gateは約48分かかった
  （[対象SHA・ログdigest・結果](../../evidence/gameplay/feature-parity/verification-20261112.md)）。
  原因の変わらない全gateを調査ループで繰り返さない。
- 変更が固まった時点で、対象SHAの `tests/verify.sh` を省略なく実行する。
  並列round、wire、adapter、layers/debt、mutationsと終了値を確認し、失敗・中断も保存する。
  文書のみの変更ではdiff・リンク・記述と実装の整合性を検査し、全gate未実行を明記する。
- native起動・build・全gateの前に時間/メモリ予算を宣言し、そのhostのresource gateを使う。
  lightはnamed scopeとMemoryMax、heavyはlease取得・bind・releaseを行う。
  この手順はresource制約や外部公開の同意を上書きしない。

## 6. ローカル配置と公開境界

このhostの正本は `~/mc-spine`、支援データは `~/mc-spine-local`。
新しいcaptureは `captures/`、実行記録は `audit/`、作業用実験は `scratch/current/` に置く。
別hostではrootを選び直す。廃止した一時pathや旧cache aliasに依存させない。
jar、decompile、native worldはローカルに保持し、既存fixtureも権利判断なしに追加公開しない。

履歴のprovenanceにある旧pathは書き換えず、ローカルの `inventory/` の移動台帳で解決する。
P0-3の資産/出所監査、独立レビュー、hosted CI、M2/M3は別々の未完了境界であり、
文書のpushやローカルsuiteの緑をリリース承認と扱わない。

## 7. 汎用理論とフィードバック階層（L0〜L5）

生存者バイアス・ポパーの反証主義・パールの因果推論（ステージ介入）・制約理論（TOC）・情報利得（Writer単位のパレート圧縮）・スイスチーズモデル（ポカヨケ）を統合した汎用方法論と、5ステップ高速収束サイクルは [high-speed-convergence.md](../methodology/high-speed-convergence.md) を参照する。

## 8. 報告と検証の契約（2026-11-16 承認）

短い報告は**索引**であり、証拠の置き換えではない。報告だけを読んで「測った」「合格した」と
扱える状態にしてはならない。1件の変更について、報告は最低限これを載せる。

- base/result SHAと `git status` のdirty状態、対象chunk/window・seed・jarとmappingのhash、
  実行したbinaryのpath/SHAとbuild profile。再実行していない既存logは、source/inputの同一性を
  確認できるまで再利用せず、確認できないものは「未拘束」と書く。
- 最初に分岐したstage/writerと、**実際に実行された経路**（counter・trace・到達した関数）。
  最終残差の件数を最初の差分の代わりにしない。
- 設計上の選択、既定値、近似、owner、互換性の変更（例: 未初期化lightを15/0と読む）。
  観測と推論を同じ文に混ぜず、推論は「仮説」と明記し、必要な次の計測を名指しする。
- PASS/FAIL/SKIP/NOT RUNと、無効だった試行（OOM、input取り違え、0件選択、stale artifact）。
  無効試行は削除せず残し、同じ条件のまま再試行しない。
- 対照: compileするmutantを入れて対象testが実際に落ちたこと、復元後の再緑、
  pre-change moduleへの再生結果（そこで通るtestは修正を証明しない）。
- 一次証拠の絶対path（log、`*.bin`、`*.tsv`、run.json、world）とartifactごとのstart/finish・
  SHA-256。短い報告から一次証拠へ辿れること。
- 未解決項目と、**受け入れる範囲**。何をどこまで主張しないかを書く。

運用上の帰結:

- 共有logic、永続化、RNG消費、stage境界を変える変更は、canonicalへ入れる前に**実装者とは別セッション・
  別著者の独立レビュー**を通す。独立性は著者とセッションで定義し、特定のmodel名では定義しない
  （同じmodelでも別セッションなら独立、実行主体が同じなら独立ではない）。実装者自身の承認で閉じない。
  現在の運用上の選択は user 指示による DeepSeek の別セッションであり、これは brand 要件ではない。
- focused checkの緑から一般的なparity gate・promotion gateを主張しない。
- 最終的な集約検証は**1つのintegration candidate SHA**に拘束する。
  実装summaryの寄せ集めは承認にならない。
- 失敗した証拠は保持する。書き換えるのは記述の誤りだけで、観測値を遡って消さない。

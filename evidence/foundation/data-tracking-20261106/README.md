# worldgen-parallel の追跡データ監査（2026-11-06）

対象: `nekomario28/mc-spine`, `perf/worldgen-parallel` @
`48dfe73b4e1db64d5ce295f0e533d30780a8fc37`。
取得根拠: GitHub Git Tree API の `git/trees/perf/worldgen-parallel?recursive=1`
（`truncated=false`、全3,860 tree entries）。
**この監査はGitの追跡状況の静的確認**であり、由来や再配布可能性の法的判断、
実際の資産のバイト照合、server JARとの整合性の実行証明ではない。

## 確認した事実

| 追跡パス | 実在する内容 | 整理が必要な理由 |
|---|---|---|
| `datapacks/vanilla/` | 2,346 blob（そのうちJSON 1,165、構造物NBT 1,180） | `main` 側の利用者jarから生成・非コミット方針との不整合。構造物テンプレートの利用者権限・生成手順は別途確認 |
| `config_packets.bin` | 105,095 bytes | READMEの「ローカル生成・非コミット」と矛盾 |
| `datapacks/vanilla/multi_noise_biome_tree.json` | 2,059,828 bytes | 自前データへの移行と古い帰属記録の照合が必要 |
| `assets/pblocks.json` | 7,526,001 bytes | `THIRD_PARTY.md` の旧監査で Pumpkin のassetとのバイト一致が報告されている。現在のblobも元データと再照合するまで安全な由来を仮定しない |
| `assets/properties.json` | 14,122 bytes | README/ゲーム実装計画では削除済みとしているがGit上では追跡されている |
| `resourcepack.zip` | 345 bytes | README/.gitignoreは利用者の任意指定、THIRD_PARTYは削除済みと述べるがGit上に存在 |
| `assets/blocks_1211.json` | 765,435 bytes | PrismarineJS/minecraft-dataからの取り込みと著作権表示・外部データ境界の確認を要する |
| `assets/*_1211.bin` 等 | block predicates, full collision shape, multiface support, structure transforms, template solid | 実際の生成元・入力jar・digestを追跡し、生成物の責任境界を定める必要がある |

`datapacks/vanilla/biome_climate_tree.json` と `tests/data/vanilla_vectors_1211.tsv` はこのHEADでは追跡されていない。
`resourcepack.zip` が `.gitignore` にあることは、既にtrackedなblobを除去しない。

## 文書との具体的な矛盾

- 作業ブランチの `README.md` は Mojang由来データ・registry/tag packetを
  リポジトリに含まないとしているが、Gitツリーでは該当する生成/コピー成果物を含む。
- `THIRD_PARTY.md` §1, §9 は `datapacks/vanilla/`・`resourcepack.zip` 等を
  「コミットから削除済み」としている。一方この枝の `.gitignore` は
  **明示的にデータをin-treeで追跡する方針**に変更されている。
- バイト一致と派生物の判定、Mojangの配布条件、Pumpkinとのライセンス関係は
  Gitツリー一覧だけでは確定できない。

## 安全な解消順（P0-3）

- [ ] **D1 inventoryの固定**: このHEADの各tracked blob SHA、ファイル区分、
  実際の消費者（build・runtime・native fixture）を記録。元のファイルを先に削除しない。
- [ ] **D2再生成の検証**: 公式1.21.1 JARのSHA-1を検証し、
  `tools/vanilla_data/gen_vanilla_data.sh` の生成先を含むコードを確認する。
  既存trackedパスを直接上書きしない隔離worktree等で再生成し、
  必須資産の生成率・digest・1.21.1の実クラス挙動を照合。
  特に1,180個のtemplate NBT、processor/structure tags、packet bytesを忘れない。
- [ ] **D3 出所不明の分離**: `pblocks`・`properties`・resourcepack・各テーブルの
  生成元と権利を追跡し、必要なら自前jarからの再生成や、検証専用の外部取得へ移す。
  `assets/blocks_1211.json` と古い履歴の扱いも `LICENSING.md` と突き合わせる。
- [ ] **D4 消費者の切替**: 入力が全て再生可能になり、実行fixtureと全体テストが維持できてから
  `.gitignore` / 配布物 / 文書を一体で更新し、追跡停止（`git rm --cached`）を検討。
  ローカルfixtureの消失・再現不能化を作らない。
- [ ] **D5 統合判定**: Git履歴に残る旧データとライセンス表記、NOTICE、
  第三者依存、公開範囲を別途審査。**D1〜D4が未了なら `main` / releaseは不可**。
  将来の統合候補に、追跡禁止パスと成果物を検出するfail-closed gateを追加する。

現時点ではデータを削除・置換していない。必要な実装は作業ブランチ上で
継続できるが、データ追跡の現状を「非コミットだから配布上問題ない」と説明しない。
参照: [全体チェックリスト](../../../specs/implementation-checklist.md) P0-3、
[権利確認](../../../LICENSING.md)、[第三者由来一覧](../../../THIRD_PARTY.md)。

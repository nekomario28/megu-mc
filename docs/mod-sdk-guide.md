# Mod SDK ガイド / Megu Mod SDK Guide

対象: Minecraft 1.21.1 / 26.3 向けの **ネイティブ Megu Mod** を作る人と、
AI コーディングエージェント。Gradle・JDK・Mixin なしで、JSON と標準データパックだけで
Mod を組み立て、`.mmod` として配布・ホットリロードするまでの一連の流れを扱います。

Audience: human and AI authors of **native Megu mods** for Minecraft 1.21.1 / 26.3.
No Gradle, no JDK, no Mixin: a package manifest, one declarative bundle, standard
datapack and resource-pack trees, packed into a deterministic `.mmod`.

- 実装の正 / runtime authority: [mod-adapter-core/src/unified_bundle.rs](../mod-adapter-core/src/unified_bundle.rs)
- スキーマ / annotated schema: [sdk/schemas/megu-mod-bundle-v1.schema.json](../sdk/schemas/megu-mod-bundle-v1.schema.json)
- AI エージェント契約 / agent contract: [sdk/AGENTS.md](../sdk/AGENTS.md)
- 完成例 / complete example: [sdk/examples/ruby-starter/README.md](../sdk/examples/ruby-starter/README.md)
- 既存 JAR・シェーダー作者向け / existing JARs & shader packs: [mod-author-guide.md](mod-author-guide.md)

## 1. 60秒クイックスタート / 60-second quickstart

```sh
# 1) 雛形を生成 (starter | machine | creature | shader)
python3 tools/megu_mod_dev.py init ruby_starter --out ruby-starter --template starter

# 2) 検証 — 0 errors で exit 0。--json はエージェント向けの契約出力
python3 tools/megu_mod_dev.py validate ruby-starter --json

# 3) 決定論的パッケージ — sha256 / sha512 を表示 (Modrinth / ModSync 用)
python3 tools/megu_mod_dev.py pack ruby-starter --deterministic

# 4) 開発サーバーの mods/ へ即時反映 (--once はCI用に1周だけ)
python3 tools/megu_mod_dev.py watch ruby-starter --mods-dir ./mods --once
```

`init` は `megu-mod.toml` / `megu-mod-bundle.json` / `pack.mcmeta` / `README.md` /
`data/` / `assets/` を生成します。`--compat-jar` を付けたときだけ
`META-INF/neoforge.mods.toml` と `fabric.mod.json` も生成されます（既定は
ネイティブ専用 = Java ボイラープレートなし）。

`init` generates the native tree above. Java-loader manifests are emitted only
with an explicit `--compat-jar`, so the default project stays pure data.

> **バージョン非依存（Versionless `.mmod`）方針:** ネイティブ `.mmod` / `.mplugin` は将来にわたり特定の Minecraft バージョン番号の固定を必須にしない方向を採ります（`megu_api` 世代と必要ケイパビリティ／バニラ機能要求のみを宣言し、ID・コンポーネント形式差はランタイムの `VersionPack` / `IdRemapper` が吸収する。`--mc-version` は `--compat-jar` 出力や明示制限が必要な場合のみ指定）。

| コマンド / command | 用途 / purpose |
| --- | --- |
| `init <mod_id> --out <dir> [--template ...] [--mc-version 1.21.1] [--compat-jar]` | 雛形生成。不正 ID・`..` は exit 2 で拒否 / fail-closed scaffold |
| `validate <dir> [--json]` | bundle・manifest・data/assets を検証。0=OK, 1=errors, 2=usage |
| `pack <dir> [--out ...] [--deterministic] [--compat-jar]` | 検証後に `.mmod`（`--compat-jar` は `.jar`）を生成 |
| `watch <dir> --mods-dir <mods> [--interval 0.5] [--once]` | `<mod_id>-dev.mmod` を `.part` + rename で原子的に配置 |
| `doctor [--json]` | Python・`megu`・`megu-mod-check`・スキーマの有無を点検 |

`validate --json` の出力は
`{"ok": bool, "mod_id": str|null, "diagnostics": [{"code", "path", "message", "fix_hint", "severity"}]}`
です。`severity == "error"` の `path` だけを直して再実行すれば収束します。

## 2. パッケージ形式 `.mmod` / The `.mmod` package

`.mmod` は決定的に並べた ZIP です。中身は
`megu-mod.toml` + `megu-mod-bundle.json` + `pack.mcmeta` + `README.md` +
`data/**` + `assets/**` + `shaderpacks/**`（+ `LICENSE*`）。`--compat-jar` のときだけ
`META-INF/neoforge.mods.toml` と `fabric.mod.json` が加わります。

```text
ruby-starter/
├── megu-mod.toml                 # id / version / mc_version_range / license
├── megu-mod-bundle.json          # megu-mod-bundle/1 (JVM 不要の宣言)
├── pack.mcmeta                   # pack_format 48 (1.21.1) / 121 (26.3)
├── data/ruby_starter/recipe/     # shaped / shapeless / smelting
├── data/c/tags/item/gems/ruby.json
├── assets/ruby_starter/blockstates/
├── assets/ruby_starter/models/
├── assets/ruby_starter/lang/{en_us,ja_jp}.json
└── README.md
```

`pack --deterministic` は ZIP エントリをソートし、タイムスタンプを 1980-01-01 に
固定するため、同じ入力からはバイト一致の `.mmod` が得られます。出力される
SHA-256 / SHA-512 はそのまま Modrinth のバージョン作成や ModSync の照合に使えます。

`--deterministic` sorts entries and pins the ZIP epoch, so the same tree yields
byte-identical output; the printed SHA-256/SHA-512 are ready for Modrinth and
ModSync metadata.

## 3. IDE 補完 / IDE autocomplete

`sdk/schemas/megu-mod-bundle-v1.schema.json` は JSON Schema Draft 2020-12 で、
`megu-mod-bundle/1` の全キー・全 `kind` 値・数値上限・既定値・`pattern` を持ちます。
VS Code ではワークスペース設定に次を追加します。

```json
{
  "json.schemas": [
    {
      "fileMatch": ["**/megu-mod-bundle.json"],
      "url": "./sdk/schemas/megu-mod-bundle-v1.schema.json"
    }
  ]
}
```

IntelliJ は *Settings → Languages & Frameworks → Schemas and DTDs → JSON Schema
Mappings*、Zed は `settings.json` の `lsp.json-language-server.settings.schemas`
で同じファイルを割り当てます。

> **注意 / note:** `megu-mod-bundle.json` の中に `"$schema"` キーを書かないでください。
> `DeclarativeModBundle::from_json` は未知のトップレベルキーを拒否するため、
> `$schema` を書いた bundle は**実行時に無効**になります。補完はエディタ設定側で
> 割り当てるのが正しい方法です。`tools/megu_mod_dev.py validate` も同じ理由で
> `bundle.unknown_key` を返します。
>
> Do **not** put a `$schema` key inside the bundle: the runtime rejects unknown
> top-level keys. Map the schema in editor settings instead.

## 4. Rust SDK / 型付きビルダーとテストハーネス

Rust 側の型付き API は `mod-adapter-core` が提供します。`DeclarativeModBundle`
（`portals` / `progression_barriers` / `biome_regions` / `creatures` /
`custom_channels` / `spells` / `custom_statuses` / `rituals`）を構築し、
`to_json()` で決定的な JSON を出力、`validate()` で同じ fail-closed 検証を
再実行できます。

```rust
use mod_adapter_core::unified_bundle::DeclarativeModBundle;

let bundle = DeclarativeModBundle::from_json(include_str!("../megu-mod-bundle.json"))?;
bundle.validate()?;                     // 同じ制約を再確認
let wire = bundle.to_json();            // BTreeMap 順 = 決定的な wire 表現
```

```rust
use mod_adapter_core::unified_bundle::UnifiedModRuntime;
use mod_adapter_core::version_compat::McVersion;

let mut runtime = UnifiedModRuntime::default();
let receipt = runtime.register_bundle(&bundle, McVersion::new(1, 21, 1))?;
assert_eq!(receipt.mod_id, "ruby_starter");
```

テストハーネスの実例は
[mod-adapter-core/tests/cross_mod_interop_tests.rs](../mod-adapter-core/tests/cross_mod_interop_tests.rs)
（`BlockCapabilityBus` と `UnifiedTagIndex` を跨ぐ end-to-end）と
`unified_bundle.rs` 内の `magic_bundle_tests` です。どちらも
「bundle を作る → `to_json()` → `from_json()` で往復 → runtime に登録」
という同じ形で、ラウンドトリップと登録結果を同時に固定します。

> **状態 / status:** 独立クレート `sdk/megu-mod-sdk`（`ModTestHarness` を持つ
> 予定のファサード）は**このツリーにはまだ存在しません**。現時点の型付き
> ビルダーは `mod-adapter-core` の公開型で、ハーネスは上記の統合テストです。
> このガイドは実在する API だけを案内します。
>
> The standalone `sdk/megu-mod-sdk` facade crate (with `ModTestHarness`) is
> **not in this tree yet**; today the typed builders are `mod-adapter-core`
> public types and the harness is the integration-test pattern above.

## 5. クロス Mod 相互運用 / Cross-mod interop

### 5.1 統一タグ `#c:` / `#forge:` / `#neoforge:`

[mod-adapter-core/src/unified_tags.rs](../mod-adapter-core/src/unified_tags.rs) の
`UnifiedTagIndex` は `data/<ns>/tags/{item,block,fluid}/**` を1つの表に統合し、
入れ子タグを決定的に展開します。NeoForge 1.21.1 の `#c:gems/ruby` と旧綴りの
`#forge:gems/ruby` / `#neoforge:gems/ruby` は**双方向に別名**として解決されるため、
どちらか一方しか登録していない Mod 同士でも同じ問い合わせに答えます。

```json
{"replace": false, "values": ["ruby_starter:ruby"]}
```

`data/c/tags/item/gems/ruby.json` に置けば `#c:gems/ruby` になります。循環参照は
`MAX_UNIFIED_TAG_DEPTH` で打ち切られ、`matches` は `false` を返します。

### 5.2 ブロック間搬送 `BlockCapabilityBus`

[mod-adapter-core/src/capability_bus.rs](../mod-adapter-core/src/capability_bus.rs)
はアイテム・流体・FE の搬送を面（`FaceDirection`）単位で扱います。
`register_item_handler` / `register_fluid_handler` / `register_energy_handler` で
エンドポイントを登録し、`transfer_item_between` は失敗時に原子的にロールバック
します。`SimpleSlotInventory` / `SimpleFluidTank` / `SimpleEnergyBuffer` は
そのまま使える実装、`CookingPotItemAdapter` は料理鍋を搬送網に繋ぐ実例です。

### 5.3 Create / Culinary / 大型クリーチャー

- **Create 系**: [create-adapter/src/factory.rs](../create-adapter/src/factory.rs) の
  `CreateFactory`（kinetic ネットワーク、`wrench_rotate`、回転伝播）と
  [create-adapter/src/addon_profile.rs](../create-adapter/src/addon_profile.rs) の
  アドオンプロファイル。ベルト・シャフト・動力源/消費側を宣言で表現します。
- **料理系**: [mod-adapter-core/src/culinary_adapter.rs](../mod-adapter-core/src/culinary_adapter.rs)
  が `*:cooking` / `*:fermenting` / `*:cutting` レシピとタグを JAR から自動発見します。
- **大型（コロッサル）クリーチャー**: サーバー側のフェーズ制御と多部位ヒットボックスは
  [mod-adapter-core/src/world_extensions.rs](../mod-adapter-core/src/world_extensions.rs) の
  `BossEncounterState` + `SubPartHitbox`、クライアント側の関節リグは
  [client-port/src/entity_rig.rs](../client-port/src/entity_rig.rs) の
  `ColossalRigKind`（`WingedDragon` / `SeaSerpent`）と `build_colossal_rig_mesh` が
  提供します。`creatures` セクションは通常の飼育・騎乗・移動を担い、巨大ボスの
  部位制御・描画リグはこの Rust プリミティブ側です（専用の "colossal" バンドル
  キーは存在しません）。

## 6. シェーダーパック / Shader pack authoring

ネイティブ WGSL パックは `shaderpacks/<name>/megu-shader.toml` と `.wgsl` で構成します。

```toml
[pack]
name = "Ruby Shader"
author = "Example Author"
version = "0.1.0"
api_version = 1
requires_compute = false
requires_ssbo = false
supports_lod = false

[pass.final]
stage = "final"
kind = "graphics"
file = "final.wgsl"
```

`python3 tools/megu_mod_dev.py init ruby_shader --template shader --out ruby-shader`
が最小の `final.wgsl`（フルスクリーン三角形）を生成します。OptiFine / Iris の
既存パックは `client-port/src/shaderpack.rs` が読み、必要な GPU 機能を満たさない
場合は内蔵バニラ描画へ安全にフォールバックします。詳細な対応表と
`shaders.properties` / `lang/*.lang` の扱いは
[mod-author-guide.md](mod-author-guide.md) の第4節を参照してください。

## 7. 公開チェックリスト / Publishing checklist

1. **検証**: `python3 tools/megu_mod_dev.py validate <dir> --json` が `"ok": true`。
2. **決定論的ビルド**: `python3 tools/megu_mod_dev.py pack <dir> --deterministic`。
   2回実行してバイト一致を確認（CI では `tests/megu_mod_dev_test.py` が同じ検査をします）。
3. **ダイジェスト記録**: 出力の `sha256` / `sha512` をリリースノートと
   Modrinth バージョンに記載。
4. **バージョン整合**: `megu-mod.toml` の `version` と `mc_version_range` を
   bundle と一致させる（`validate` が `manifest.mismatch` で検出）。
5. **ライセンス**: `license` を明記し、同梱物の来歴を確認。Megu 本体は
   `LGPL-3.0-or-later`、外部 Mod / シェーダーは作者のライセンスのままです
   （[LICENSING.md](../LICENSING.md)、[LICENSE](../LICENSE)）。
6. **アセット**: Mojang・第三者のテクスチャ/モデル/音声を同梱しない。
7. **Modrinth**: `.mmod`（または互換が必要なら `--compat-jar` の `.jar`）を
   アップロードし、ゲームバージョン・ローダー・ハッシュを登録。ダウンロード
   リンクは `/releases` または具体タグを使う（`/releases/latest` は
   プレリリースのみのリポジトリで 404 になります）。
8. **動作確認**: `watch --once` で開発サーバーの `mods/` に配置し、
   起動・登録・保存・再起動を実際に確認。検証はスキーマ適合であり、
   ゲーム内挙動の保証ではありません。

## 8. AI エージェント向け / For AI coding agents

エージェントは [sdk/AGENTS.md](../sdk/AGENTS.md) の契約に従ってください。要点:

- `validate --json` の `diagnostics` だけを読み、`path` のファイルと
  JSON ポインタを最小編集して再実行する（自己修復ループ）。
- `$schema` を bundle に書かない、Java マニフェストを勝手に足さない、
  `.exe` / `.so` / `.dll` / `.class` / `.jar` を同梱しない、
  Mojang アセットを持ち込まない。
- データで表現できない要求（新規ブロック/アイテム登録、独自レンダーパス、
  Java API 依存）は、bundle に押し込まずに明示して停止する。

## 9. 検証コマンド / Verification

```sh
python3 tests/megu_mod_dev_test.py                                  # 30 tests
python3 tools/megu_mod_dev.py validate sdk/examples/ruby-starter    # 0 errors
python3 tools/docs_link_audit.py                                    # 0 broken links
python3 -m py_compile tools/megu_mod_dev.py tests/megu_mod_dev_test.py
```

## 10. English quick summary

- A native mod is `megu-mod.toml` + `megu-mod-bundle.json` + `data/` + `assets/`;
  no Gradle, JDK, Mixin, or Java manifest unless `--compat-jar` is requested.
- `python3 tools/megu_mod_dev.py init|validate|pack|watch|doctor` is the whole
  toolchain; `validate --json` returns structured, self-healing diagnostics.
- `pack --deterministic` yields byte-identical `.mmod` archives and prints
  SHA-256/SHA-512 for Modrinth and ModSync.
- The bundle schema is fail-closed: exact `megu-mod-bundle/1` tag, resource-
  location ids, bounded sections, enumerated trigger/movement/cast/form/effect
  kinds, and no unknown keys (including `$schema`).
- Autocomplete comes from `sdk/schemas/megu-mod-bundle-v1.schema.json`, mapped
  in editor settings rather than embedded in the bundle.
- Rust typed builders live in `mod-adapter-core` (`DeclarativeModBundle`,
  `UnifiedModRuntime`); the standalone `sdk/megu-mod-sdk` facade crate is not in
  this tree yet.
- Interop primitives: `UnifiedTagIndex` (`#c:` ↔ `#forge:` ↔ `#neoforge:`),
  `BlockCapabilityBus` (item/fluid/energy), `CulinaryRegistry`, `CreateFactory`,
  and `BossEncounterState` for multi-part bosses.
- Shader packs are `megu-shader.toml` + `.wgsl`; existing OptiFine/Iris packs
  are parsed and safely fall back when GPU features are missing.
- AI agents follow [sdk/AGENTS.md](../sdk/AGENTS.md): fix only the paths named
  by `validate --json`, never widen the schema, never ship executables or
  borrowed assets.

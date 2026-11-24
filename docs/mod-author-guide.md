# Mod・シェーダー開発者ガイド / Mod & Shader Author Guide

対象は Minecraft 1.21.1 の NeoForge / Fabric Mod 作者、および Megu クライアント用シェーダー作者です。現行の CLI・アダプター・Bundle API の使い方を示します。事前解析で互換候補と判定されても、実ワールドでの動作保証にはなりません。

ネイティブ Mod を JSON だけで作りたい場合は [Mod SDK ガイド](mod-sdk-guide.md) と [tools/megu_mod_dev.py](../tools/megu_mod_dev.py)（`init` / `validate` / `pack` / `watch` / `doctor`）を先に読んでください。AI エージェント向けの契約は [sdk/AGENTS.md](../sdk/AGENTS.md) にあります。

## 1. 既存 Mod（NeoForge 1.21.1 / Fabric 1.21.1）をそのまま動かす仕組み（自動アダプト）

既存の `.jar` は `mods/` に置く運用を想定しています。作者が JAR を手作業で Rust に変換する必要はありません。サーバー側の `mod-adapter-core` がメタデータ、JSON データ、Mixin と JVM バイトコードを調べ、利用できるネイティブ機能・互換経路・フォールバックを振り分けます。**自動判定とすべての Mod の完全動作は別**で、実行には Mod ごとの互換性が必要です。

- **組み込み最適化の重複排除** — `BUILTIN_SUPERSEDED_PERF_MODS` に登録された Sodium、Embeddium、Lithium、Starlight、FerriteCore、ModernFix、Distant Horizons などは、対応する Mod ID を認識してスキップ対象とします。megu 側の独立した最適化実装を使います。第三者の JAR は実行・取り込みません。
- **データのネイティブ処理** — データパック由来のレシピ、アイテム・ブロックタグ、loot modifier、調理・切断・発酵データ、`dimension_type`・`dimension`・worldgen といった宣言データを解析する仕組みがあります。生物の飼育・繁殖・移動やポータルなどは汎用プリミティブで表現できます。個別 Mod の全挙動を自動再現できるという意味ではありません。
- **`.class` / Mixin の解析** — `*.mixins.json` に登録されたクラスの `.class` を JVM を起動せず静的走査します。対応する `@Inject` の `HEAD`・`TAIL`・`RETURN` などは標準イベントへ lift（対応付け）を試みます。`@Redirect`・`@Overwrite`、locals capture、不明な対象などは安全側に倒して未対応と報告します。

| 区分 | 実行経路 | JVM |
| --- | --- | --- |
| Tier 0 — Native | 専用 Rust アダプター | 不要 |
| Tier 1 — Data + Bundle | データパック、宣言的プロファイルと `megu-mod-bundle/1` | 不要 |
| Tier 2 — Lifted Shim | 対応する Mixin をイベントへ lift し、制限付き NeoForge/Fabric JVM shim で実行 | shim で使用 |
| Tier 3 — Fallback | 未対応の Mixin / API / AT。ポリシーに従い起動拒否、隔離、上流 NeoForge への委譲を判断 | 委譲時は使用 |

この区分とは別に、組み込み済み最適化 Mod は `BuiltInSuperseded` としてスキップします。**Tier 3 の Mod がネイティブの小型 JRE で必ず動くわけではありません。**

事前検査 CLI は `mod-adapter-core/src/bin/mod_check.rs` の `megu-mod-check` です。現在実装されている呼び出しは次のとおりです。

```sh
megu-mod-check --modpack example.mrpack --mc-version 1.21.1 --json
megu-mod-check --scan-jar mods/example.jar --json
megu-mod-check --scan-mods-dir mods/ --json
```

- `--modpack` は Modrinth `.mrpack` / `modrinth.index.json`、CurseForge `manifest.json` / パック ZIP の **ダウンロード・起動前**の対応 Minecraft バージョン、軽量化 Mod のスキップ、ネイティブ候補、追加検査が必要な JAR を報告します。`--flagship <id,...>`・`--profiles <id,...>` もこのモード専用です。終了コード `0` でも未検査 JAR の実行成功を証明しません。
- `--scan-jar` は単一 JAR 内の Mixin バイトコードとイベントへの lift 可否を調べます。デフォルトで専用サーバー用の Mixin が対象で、`--include-client-mixins` でクライアント側も含められます。
- `--scan-mods-dir` はディレクトリ内の JAR をまとめて検査し、重複軽量化 Mod のスキップ、調理系のレシピ・タグ・loot modifier の検出、Tier 2 / Tier 3 の判定を報告します。
- 終了コードは `0`（検査上の受理）、`2`（不一致・未 lift・引数不正など）、`1`（読めない・危険な入力など）です。各モードで `0` の意味は異なるため、診断内容も確認してください。

### 自動アダプト CLI（実装済み）

`megu-mod-check` の `--adapt-jar` と `--adapt-mods-dir` は利用できます。JAR 内の loader manifest、宣言データ、Mixin バイトコードを読み、`auto_skipped`、`native_bundle`、`tier2_lifted_shim`、`tier3_fallback_required` のいずれかに分類します。実行は JVM 不要です。

```sh
# 単一 JAR の自動分類。生成されたネイティブ Bundle と診断 JSON を保存
megu-mod-check --adapt-jar mods/example.jar --out mods/adapted --json

# mods/ の全 JAR を分類。結果を mods/adapted/ に保存
megu-mod-check --adapt-mods-dir mods/ --out mods/adapted/ --json

# サーバー用途では除外される client-only Mixin も解析する場合
megu-mod-check --adapt-jar mods/example.jar --include-client-mixins --json
```

`--json` の標準出力は単一 JAR では `megu-jar-adaptation/1`、ディレクトリでは `megu-mods-dir-adaptation/1` です。`--out <dir>` を指定すると `adaptation-report.json` と、ネイティブ化できた Mod ごとの `<mod_id>.bundle.json` を書き出します。後者の JSON は厳格な `megu-mod-bundle/1` スキーマで検証されます。`--out` を省略するとファイルを生成せず、診断のみ表示します。

終了コード `0` は全 JAR がスキップ・Native・Tier 2 のいずれかと判定されたことを示し、`2` は Tier 3 が含まれるか引数が不正、`1` はファイルの読み書き不可や危険・破損した入力を示します。`2` の場合でも、`--out` を指定していれば成功した Native Bundle と診断レポートは保存されます。

サーバーの `src/live_mod_bridge.rs` には起動時の JAR 分類・宣言データ読込・対応する Mixin の lift と、`mods/` および `mods/adapted/` の検証済み `*.bundle.json` 登録が実装されています。ランチャーの `tools/mod_launcher.py --auto-adapt` も実装済みで、起動計画時に既存 Bundle を事前確認し、利用可能な `megu-mod-check --scan-mods-dir` の結果と ShimCheck を組み合わせて判定します。次のコマンドで起動計画を確認できます。

```sh
python3 tools/mod_launcher.py --mods-dir mods/ --auto-adapt --dry-run --json
```

ランチャーの `--auto-adapt` は `--adapt-mods-dir --out` による Bundle ファイルの書き出しとは別の入口です。JAR から生成ファイルが必要な場合は上記の `megu-mod-check --adapt-* --out` を実行してください。

## 2. 既存 Mod を megu で最大限速く・互換性高く動かすための設計ベストプラクティス

1. **NeoForge / Fabric の標準 registry とデータパックを使う。** レシピ、タグ、loot table、dimension、worldgen をできる限り `data/<namespace>/...` の JSON で記述します。一般的なデータ形式ほど JVM を使わない Tier 1 への対応付けが容易です。
2. **イベントを標準 API の範囲に収める。** ライフサイクルイベントや既知の `@Inject` 対象は、独自イベントに変換しやすくなります。`@Redirect` / `@Overwrite`、ローカル変数の捕捉、深い Minecraft 内部 API の差し替えに依存する設計は互換性を下げます。
3. **軽量化機能はオプションとして切り離す。** サーバー tick、照明、メモリ管理、描画・LOD に関する独自最適化は megu に実装済みの分野と重複し得ます。既存の最適化 Mod と機能・性能が同等とは未検証です。
4. **Mod ID と対応バージョンを正確に宣言する。** `fabric.mod.json` と `META-INF/neoforge.mods.toml` の依存関係、サイド区分、Minecraft バージョン制約が自動振り分けの入力になります。
5. **実行検証する。** `--scan-*` の結果を開発中の診断に使い、実際のサーバーで登録、データ保存、イベント、ワールド生成、クライアント接続を別途試験してください。

## 3. JVM 不要・最速ネイティブ Mod フォーマット（`megu-mod-bundle/1`）の作り方

**ネイティブ開発の利点** — 対応する宣言的プリミティブだけで完結する Mod は、**Gradle・JDK・Minecraft mappings・Mixin の設定やボイラープレートが不要**です。JSON を記述して Rust の汎用ランタイムに渡せるため、個別の Java クラスをコンパイルする工程と JVM 実行コストを省けます。対応範囲では変更もデータ中心で、`dep_resolver::assess_hot_reload_after_add` は Bundle を含む安全な JAR を **hot-reload 可能と判定**します。再起動不要の適用可否は JAR 内の Mixin・ネイティブライブラリ・バイトコードに依存します。

**スターターの自動生成は実装済み**です。`--scaffold-native` は `megu-mod-bundle/1` の検証可能な宣言 JSON を標準出力または指定ファイルに生成します。プロジェクト全体（`megu-mod.toml`、bundle、`data/`、`assets/`、`README.md`）を雛形から作る場合は [tools/megu_mod_dev.py](../tools/megu_mod_dev.py) の `init`、検証・`.mmod` 生成・ホットリロードは `validate` / `pack` / `watch` を使います。手順の全体は [Mod SDK ガイド](mod-sdk-guide.md) を参照してください。`--version` は Bundle の `mc_version_range`（バージョン指定または Maven / SemVer 範囲）、`--name` は表示名の検証用引数です。現行のスキーマには表示名のフィールドがないため、Bundle に保存されるのは `mod_id` とバージョン範囲などの定義済みフィールドです。データパックの JSON は別途作成してください。

```sh
# ファイルに保存する（親ディレクトリは事前に作成）
megu-mod-check --scaffold-native my_sample_mod --version '[1.21,)' --out my_sample_mod.bundle.json

# ファイル指定がない場合は Bundle JSON を標準出力する
megu-mod-check --scaffold-native my_sample_mod
```

Mod ID は安全な小文字の英数字・`_`・`.`・`-` のみで構成された単独 ID に限られます。`../escape` のようなパスを含む ID は終了コード `2` で拒否されます。ファイル書き込み失敗時は `1` です。

### Bundle 本体の最小例

`bundle.json` に以下を保存します。この例は `mod-adapter-core/src/unified_bundle.rs` の `DeclarativeModBundle::from_json` が受理するキーだけで構成しています。`mod_id` は名前空間、`mc_version_range` は Maven / SemVer 形式のバージョン範囲です。

```json
{
  "schema": "megu-mod-bundle/1",
  "mod_id": "starling",
  "mc_version_range": "[1.21,)",
  "portals": [
    {
      "name": "starling:moon_gate",
      "target_dim": "starling:moon",
      "teleport_grace_ticks": 40,
      "trigger": {
        "kind": "direct_block_interact",
        "block_id": "starling:moon_gate",
        "required_item": "starling:moon_key"
      }
    }
  ],
  "progression_barriers": [],
  "biome_regions": [
    {
      "mod_id": "starling",
      "dimension": "starling:moon",
      "weight": 100,
      "biome_overrides": { "minecraft:plains": "starling:moon_plains" }
    }
  ],
  "creatures": [
    {
      "entity_type": "starling:moon_moth",
      "max_health": 12.0,
      "tame_items": ["starling:moon_berry"],
      "tame_chance": 0.4,
      "breed_items": ["#starling:moon_moth_food"],
      "movement": { "kind": "land", "ground_speed": 0.2 }
    }
  ],
  "custom_channels": ["starling:sync_v1"]
}
```

`portals` は対象ディメンションと起動条件、`biome_regions` はディメンション内のバイオーム置換、`creatures` は種別と飼育・移動の振る舞いを宣言します。`progression_barriers` には実績による進行制限を追加できます。`custom_channels` は **ネットワーク payload チャネル ID** であり、任意の実行可能イベントハンドラを宣言するフィールドではありません。イベントの lift は既存 JAR の Mixin 解析経路が担当します。

### 魔法・カスタム状態異常・儀式（任意）

`spells`、`custom_statuses`、`rituals` は Bundle のトップレベルに追加できる任意フィールドです。従来の Bundle は変更不要です。次の JSON は既存の `bundle.json` に追加する3フィールドの抜粋です。

```json
{
  "spells": [
    {
      "id": "starling:ember_burst",
      "mana_cost": 8.0,
      "base_cooldown_ticks": 40,
      "cast": { "kind": "cast_time", "total_ticks": 10 },
      "interrupt_on_damage": true,
      "steps": [{
        "form": { "kind": "aoe_zone", "radius": 4, "duration_ticks": 60, "pulse_interval": 10 },
        "effects": [
          { "kind": "damage", "school": "starling:fire", "base_amount": 3 },
          { "kind": "apply_status", "status_id": "starling:scorch", "amplifier": 1, "duration_ticks": 80 },
          { "kind": "emit_vfx", "vfx_id": "starling:ember_cloud" }
        ]
      }]
    }
  ],
  "custom_statuses": [{
    "id": "starling:scorch",
    "max_amplifier": 3,
    "pulse_interval_ticks": 20,
    "damage_per_pulse": 1,
    "heal_per_pulse": 0,
    "absorption_per_level": 2,
    "silenced": true,
    "attribute_modifiers": { "starling:fire_resistance": -0.25 },
    "vfx_aura": "starling:scorch_aura"
  }],
  "rituals": [{
    "id": "starling:moon_altar",
    "pattern": [
      { "dx": 0, "dy": 0, "dz": 0, "block": "starling:altar_core" },
      { "dx": 1, "dy": 0, "dz": 0, "block": "minecraft:amethyst_block" }
    ],
    "reagents": { "minecraft:amethyst_shard": 4 },
    "duration_ticks": 100,
    "mana_per_tick": 0.5
  }]
}
```

`spells[].cast.kind` は `instant` / `cast_time` / `channeled`、`steps[].form.kind` は `self_cast` / `touch` / `projectile` / `raycast_beam` / `aoe_zone` / `chain` / `orbit` / `summon`、`effects[].kind` は `damage` / `heal` / `shield` / `apply_status` / `knockback` / `blink` / `conjure_light` / `break_block` / `emit_vfx` に対応します。`custom_statuses` は DoT、HoT、吸収シールド、行動制限（`frozen` / `rooted` / `silenced` / `disarmed`）、属性補正、VFX オーラを定義します。儀式の `pattern` は祭壇原点からの相対座標で、`reagents` はアイテム ID ごとの必要数です。

宣言は `SpellDefinition::validate`、`CustomStatusRegistry`、`RitualRecipe::validate` によって制限され、未知のキー・種類、重複 ID、上限を超えるパラメータは拒否されます。Bundle のパースとランタイム登録により魔法定義を参照できます。ゲーム内の発動操作・描画・ワールドへの効果適用には対応する実行経路との接続が必要です。魔法同期用の `MAG1` コーデックは `megu:magic_v1` チャネルを使用します。

### ブロック・アイテム・レシピ・タグとディメンション

現行 Bundle のトップレベルキーは `schema` / `mod_id` / `mc_version_range` / `portals` / `progression_barriers` / `biome_regions` / `creatures` / `custom_channels` / `spells` / `custom_statuses` / `rituals` の11種です。**`blocks`、`items`、`recipes`、`tags`、`events`、`dimensions` のキーを足すと検証に失敗します。** カスタムブロック・アイテム・生物種別やディメンションの **ID を参照すること**と、その実体を新規登録することは別です。該当 ID は互換レイヤ・レジストリなどで別途提供してください。

レシピやタグ、dimension/worldgen の JSON は標準データパックとして配置します。Minecraft 1.21.1 の例です。

```text
pack.mcmeta
bundle.json
data/starling/recipe/stone_button.json
data/starling/tags/item/moon_moth_food.json
data/starling/dimension/moon.json           # 別途、実際の次元定義が必要
data/starling/dimension_type/moon.json      # 別途、実際の次元型定義が必要
data/starling/worldgen/biome/moon_plains.json  # 別途、実際のバイオーム定義が必要
```

`pack.mcmeta` の例（1.21.1 の `pack_format` は `48`）:

```json
{"pack": {"pack_format": 48, "description": "Starling native data"}}
```

`data/starling/recipe/stone_button.json` の例:

```json
{
  "type": "minecraft:crafting_shapeless",
  "ingredients": [{"item": "minecraft:stone"}],
  "result": {"id": "minecraft:stone_button", "count": 1}
}
```

`data/starling/tags/item/moon_moth_food.json` の例:

```json
{"replace": false, "values": ["minecraft:apple"]}
```

カスタムアイテム `starling:moon_berry` をこのタグに追加する場合、そのアイテムの登録も必要です。上記の ID は **宣言形の説明用**です。現行スキーマが自由なブロック・アイテム・イベントを JSON からすべて登録できるとは主張しません。現行の `--scaffold-native` は Bundle の初期 JSON を生成しますが、標準データパックのレシピ・タグやカスタム登録定義は自動生成しません。

## 4. 外部シェーダーパック対応とネイティブ WGSL パック（`megu-shader.toml`）

Megu の `client-port/src/shaderpack.rs` はユーザーが配置した `shaderpacks/<name>/` と `shaderpacks/<name>.zip` を読み、`client-app/src/shaderpack_runtime.rs` が GPU 機能と照合して描画パイプラインを選びます。

| パック | 認識・解析対象 |
| --- | --- |
| Classic OptiFine | GLSL `120`–`330`、`gbuffers_*` / `composite*` / `final`、`DRAWBUFFERS`、従来のテクスチャ群 |
| Iris 1.7–1.11+ 系 | GLSL `430`–`460`、`.csh` compute、`bufferObject.*` SSBO、`image.*`、`RENDERTARGETS`、dual-hand lighting uniforms、`block.properties` / `item.properties` / `entity.properties` |
| DistantHorizons 連携 | `dh_terrain` / `dh_water` / `dh_shadow` の LOD パス、未提供なら Megu のネイティブ LOD 描画経路 |
| Megu ネイティブ | `megu-shader.toml` + `.wgsl`、GLSL 変換を経由しないパス宣言 |

`shaders.properties` の設定・プロファイルや `lang/*.lang`（例: `lang/ja_JP.lang`、未翻訳キーは `en_US` にフォールバック）も解析対象です。Compute / SSBO / custom images や描画ターゲット数など、GPU が必要機能を満たさないパックは **内蔵バニラ描画へ安全にフォールバック**します。対応表はパーサー・パイプライン計画機能の範囲であり、すべての市販シェーダーの画面一致を保証するものではありません。

### WGSL パックの作例

`shaderpacks/starling/megu-shader.toml`:

```toml
[pack]
name = "Starling Simple"
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

`shaderpacks/starling/final.wgsl`:

```wgsl
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let xy = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0)
    );
    return vec4<f32>(xy[vertex_index], 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return vec4<f32>(0.15, 0.25, 0.45, 1.0);
}
```

`[pass.<name>]` は `stage`（`begin` / `shadow` / `prepare` / `gbuffers` / `terrain` / `opaque` / `translucent` / `lod` / `distant_horizons` / `dh` / `deferred` / `composite` / `final`）、`kind`（`graphics` / `compute`）、必須の `file` を受け付けます。`targets = ["..."]` と compute 用 `workgroups = [x, y, z]` も指定できます。例はシェーダーモジュールの最小構造です。ゲーム画面を描く実用パックではレンダーグラフに合わせた入力・出力と実機試験が必要です。

## 5. ライセンスとクリーンルーム境界

megu 本体の配布ライセンスは **`GPL-3.0-or-later`** です。寄稿・ファーストパーティのコードについては `LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later` のトリプルライセンスの方針があります。詳細は [LICENSING.md](../LICENSING.md)、[LICENSE](../LICENSE)、[NOTICE](../NOTICE) を参照してください。

Fabric / NeoForge 互換レイヤ経由で動的ロードする **独立した外部 Mod JAR** と、利用者が自らインストールする **外部シェーダーパック**はそれぞれの作者のライセンスのままです。利用するだけで megu 本体の GPL がそれらの独立した配布物に自動波及することはありません。再配布・コードの組み込み・派生物の扱いは、各ライセンスと実際の結合形態を確認してください。

本体には **第三者のシェーダーや Mojang のアセットを同梱しません**。互換機能は仕様・観測可能な挙動に基づくクリーンルーム実装で、第三者の ARR / PolyForm Shield などのソースやアセットを取り込みません。公開用成果物の来歴チェックは [LICENSING.md](../LICENSING.md) の配布境界に従ってください。

## 6. English Quick Summary

- Target: Minecraft 1.21.1 NeoForge and Fabric mod authors, plus Megu shaderpack authors.
- Drop existing compatible JARs into `mods/`; manual Rust conversion is not required.
- The server classifies JAR metadata, data, Mixin sites and execution tiers automatically.
- Known redundant performance mod IDs are skipped in favor of built-in optimizations.
- Recipes, tags, loot modifiers and dimension/worldgen data have native parsing paths.
- Tier 0 uses a dedicated Rust adapter with no JVM.
- Tier 1 uses declarative data and `megu-mod-bundle/1` with no JVM.
- Tier 2 lifts supported Mixin injection sites into bounded JVM-shim events.
- Tier 3 needs a safe fallback policy, quarantine or delegation to NeoForge.
- Inspect packs with `megu-mod-check --modpack <pack.mrpack>`.
- Inspect Mixin bytecode with `--scan-jar <mod.jar>`.
- Inspect a complete `mods/` folder with `--scan-mods-dir mods/`.
- Auto-adapt an existing JAR with `--adapt-jar mods/example.jar --out mods/adapted --json` (schema `megu-jar-adaptation/1`).
- Auto-adapt a full folder with `--adapt-mods-dir mods/ --out mods/adapted --json` (schema `megu-mods-dir-adaptation/1`).
- Generate a valid `megu-mod-bundle/1` starter with `--scaffold-native my_sample_mod --out my_sample_mod.bundle.json`.
- Build a whole native project with `python3 tools/megu_mod_dev.py init|validate|pack|watch`; see [the Mod SDK guide](mod-sdk-guide.md) and the [AI agent contract](../sdk/AGENTS.md).
- `--adapt-*` exits 0 for accepted/skipped/Tier 2 mods, 2 for Tier 3 or invalid arguments, and 1 for unreadable or unsafe input.
- `live_mod_bridge` auto-classifies JARs and registers validated bundles during boot. `tools/mod_launcher.py --auto-adapt` enables launcher preflight classification.
- Native bundle authors can avoid Gradle, JDK, mappings and Mixin boilerplate.
- The current bundle schema defines portals, biome regions, creatures, custom channels, spells, custom statuses, and rituals.
- Recipes, tags and dimensions use separate standard datapack JSON assets.
- Custom block/item registration and arbitrary event handlers need separate support.
- Eligible pure-data bundles can be classified as hot-reloadable.
- External shaders support OptiFine, Iris, DistantHorizons LOD and native WGSL planning.
- Native WGSL uses `megu-shader.toml` with `[pack]` and `[pass.<name>]` sections.
- Unsupported GPU features trigger a safe built-in-renderer fallback.
- Independent external JARs and user-installed shaders retain their own licenses.
- The auto-adapter and native Bundle scaffold commands are available now, with compatibility still determined per mod.

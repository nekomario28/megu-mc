# Ruby Starter — a complete native Megu mod (`ruby_starter`)

A minimal but complete `.mmod` project: no Gradle, no JDK, no Mixin, no Java
manifest. `megu-mod-bundle.json` is the declarative mod document read by
`mod-adapter-core/src/unified_bundle.rs`, and `data/` + `assets/` are the
standard datapack and resource-pack trees.

```
megu-mod.toml                        package manifest (id, version, range, license)
megu-mod-bundle.json                 megu-mod-bundle/1 mod document
pack.mcmeta                          datapack metadata (pack_format 48 = 1.21.1)
data/ruby_starter/recipe/*.json      shaped, shapeless, and smelting recipes
data/c/tags/item/gems/ruby.json      unified #c:gems/ruby common tag
assets/ruby_starter/blockstates/     blockstate variants
assets/ruby_starter/models/          block + item models (no borrowed textures)
assets/ruby_starter/lang/            en_us.json + ja_jp.json
```

## 60-second quickstart / 60秒クイックスタート

From the repository root:

```sh
python3 tools/megu_mod_dev.py validate sdk/examples/ruby-starter
python3 tools/megu_mod_dev.py pack sdk/examples/ruby-starter --deterministic
python3 tools/megu_mod_dev.py watch sdk/examples/ruby-starter --mods-dir ./mods --once
```

`validate` exits `0` with no errors. `pack` prints the SHA-256 and SHA-512 of
the archive for Modrinth / ModSync metadata. `watch --once` installs
`mods/ruby_starter-dev.mmod` atomically; drop `--once` to repack on every edit.

## IDE autocomplete / IDE補完

Point the editor at the shipped schema (no `$schema` key is written into the
bundle, because the runtime parser rejects unknown top-level keys):

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

Save that as `.vscode/settings.json` (workspace root) or add the equivalent
Jean/IntelliJ JSON Schema mapping. The same schema powers inline validation in
VS Code, IntelliJ, and Zed.

## What is declared / 何を宣言しているか

- `ruby_block` shaped recipe (9 × `ruby_starter:ruby`), reverse shapeless
  recipe, and a smelting recipe producing rubies.
- `#c:gems/ruby` in `data/c/tags/item/gems/ruby.json` — the NeoForge 1.21.1
  unified common tag. `UnifiedTagIndex` also answers `#forge:gems/ruby` and
  `#neoforge:gems/ruby` queries for the same members.
- `en_us.json` and `ja_jp.json` locales; the Japanese file falls back to
  English only for missing keys by Minecraft convention, so both are complete.
- Empty `portals`, `progression_barriers`, `biome_regions`, `creatures`, and
  `custom_channels` arrays. They are required keys (the Rust parser rejects a
  document that omits them) but may be empty.

## Textures / テクスチャ

`assets/ruby_starter/models/` references `ruby_starter:block/ruby_block` and
`ruby_starter:item/ruby`. Add your own PNGs under
`assets/ruby_starter/textures/{block,item}/`; this example deliberately ships no
third-party or Mojang assets.

## Next steps

- Rust-side typed builders: see [the Mod SDK guide](../../../docs/mod-sdk-guide.md).
- AI-agent authoring contract: [sdk/AGENTS.md](../../AGENTS.md).
- Existing NeoForge/Fabric JARs and shader packs: [mod author guide](../../../docs/mod-author-guide.md).

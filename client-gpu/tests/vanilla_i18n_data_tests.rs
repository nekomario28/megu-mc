// SPDX-License-Identifier: LGPL-3.0-or-later
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use megu_client_gpu::vanilla_i18n_data::{
    VanillaJarDataCatalog, VanillaLanguageTable, VanillaRecipeKind, VanillaTextsAndFonts,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "megu-vanilla-i18n-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let off = out.len() as u32;
        let crc = megu_core::hash::crc32_ieee_update(0, data);
        out.extend_from_slice(&0x04034b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(data);
        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&off.to_le_bytes());
        central.extend_from_slice(name);
    }
    let cd_off = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x06054b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn write_asset(root: &Path, bytes: &[u8]) -> String {
    let hash = megu_core::crypto::sha1_hex(bytes);
    let object = root.join("objects").join(&hash[..2]).join(&hash);
    fs::create_dir_all(object.parent().unwrap()).unwrap();
    fs::write(object, bytes).unwrap();
    hash
}

#[test]
fn loads_localization_texts_fonts_recipes_and_recursive_tags_fail_closed() {
    let temp = TempDir::new();
    let root = temp.path();
    let jar = root.join("1.21.1.jar");
    let end = b"The End";
    let credits = b"Post credits";
    let default_font = br#"{"providers":[{"type":"bitmap"},{"type":"space"}]}"#;
    let jar_bytes=stored_zip(&[
        ("assets/minecraft/lang/en_us.json",br#"{"hello":"Hello","fmt":"%s %1$s %2$s %%","item.minecraft.apple":"Apple","missing.fallback":"English"}"#),
        ("assets/minecraft/texts/end.txt",end),("assets/minecraft/texts/postcredits.txt",credits),
        ("assets/minecraft/font/default.json",default_font),("assets/minecraft/textures/font/nonlatin_european.png",b"nonlatin-png"),
        ("assets/minecraft/textures/font/accented.png",b"accented-png"),
        ("data/minecraft/recipe/shaped.json",br#"{"type":"minecraft:crafting_shaped","group":"g","category":"building","key":{"A":{"item":"minecraft:oak_planks"}},"result":{"id":"minecraft:stick","count":4}}"#),
        ("data/minecraft/recipe/shapeless.json",br#"{"type":"minecraft:crafting_shapeless","ingredients":[{"item":"minecraft:oak_planks"}],"result":{"id":"minecraft:button"}}"#),
        ("data/minecraft/recipe/smelting.json",br#"{"type":"minecraft:smelting","ingredient":{"tag":"minecraft:logs"},"result":{"id":"minecraft:charcoal"}}"#),
        ("data/minecraft/recipe/stonecutting.json",br#"{"type":"minecraft:stonecutting","ingredient":{"item":"minecraft:stone"},"result":{"id":"minecraft:stone_bricks"}}"#),
        ("data/minecraft/recipe/smithing.json",br#"{"type":"minecraft:smithing_transform","template":{"item":"minecraft:template"},"base":{"item":"minecraft:diamond_sword"},"addition":{"item":"minecraft:netherite_ingot"},"result":{"id":"minecraft:netherite_sword"}}"#),
        ("data/minecraft/tags/item/logs.json",br##"{"values":["minecraft:oak_log","#minecraft:wood"]}"##),
        ("data/minecraft/tags/item/wood.json",br##"{"values":["minecraft:oak_planks","#minecraft:logs"]}"##),
        ("data/minecraft/tags/item/cycle.json",br##"{"values":["#minecraft:cycle","minecraft:stick"]}"##),
    ]);
    fs::write(&jar, jar_bytes).unwrap();
    let assets = root.join("assets");
    let ja = r#"{"hello":"こんにちは","ja.only":"日本語"}"#.as_bytes();
    let h = write_asset(&assets, ja);
    let unihex = b"not-read-font-payload";
    let h2 = write_asset(&assets, unihex);
    let indexes = assets.join("indexes/17.json");
    fs::create_dir_all(indexes.parent().unwrap()).unwrap();
    fs::write(indexes,format!("{{\"objects\":{{\"minecraft/lang/ja_jp.json\":{{\"hash\":\"{h}\",\"size\":{}}},\"minecraft/font/unihex/unifont.zip\":{{\"hash\":\"{h2}\",\"size\":{}}}}}}}",ja.len(),unihex.len())).unwrap();
    let pack = root.join("pack");
    let lang = pack.join("assets/minecraft/lang/ja_jp.json");
    fs::create_dir_all(lang.parent().unwrap()).unwrap();
    fs::write(
        lang,
        br#"{"hello":"Pack Hello","item.minecraft.apple":"Pack Apple"}"#,
    )
    .unwrap();
    let pack_end = pack.join("assets/minecraft/texts/end.txt");
    fs::create_dir_all(pack_end.parent().unwrap()).unwrap();
    fs::write(&pack_end, "Pack credits").unwrap();
    let pack_font = pack.join("assets/minecraft/font/default.json");
    fs::create_dir_all(pack_font.parent().unwrap()).unwrap();
    fs::write(&pack_font, br#"{"providers":[{"type":"space"}]}"#).unwrap();
    let pack_accented = pack.join("assets/minecraft/textures/font/accented.png");
    fs::create_dir_all(pack_accented.parent().unwrap()).unwrap();
    fs::write(pack_accented, b"pack-accented").unwrap();

    let table = VanillaLanguageTable::load(
        &jar,
        Some(&assets),
        Some("17"),
        "ja_jp",
        &[],
        std::slice::from_ref(&pack),
    )
    .unwrap();
    assert_eq!(table.translate("hello"), Some("Pack Hello"));
    assert_eq!(table.translate("missing.fallback"), Some("English"));
    assert_eq!(table.translate("ja.only"), Some("日本語"));
    assert_eq!(table.item_display_name("minecraft:apple"), "Pack Apple");
    assert_eq!(table.block_display_name("minecraft:oak_log"), "Oak Log");
    assert_eq!(table.menu_title("menu.title"), "Menu Title");
    let en = VanillaLanguageTable::load(&jar, None, None, "en_us", &[], &[]).unwrap();
    assert_eq!(en.format("fmt", &["A", "B"]), "A A B %");

    let fonts =
        VanillaTextsAndFonts::load(&jar, Some(&assets), Some("17"), std::slice::from_ref(&pack))
            .unwrap();
    assert_eq!(fonts.end_text.as_deref(), Some("Pack credits"));
    assert_eq!(fonts.postcredits_text.as_deref(), Some("Post credits"));
    assert_eq!(fonts.font_provider_count, 1);
    assert!(fonts.has_unihex_in_assets_index);
    assert_eq!(
        fonts.nonlatin_european_png.as_deref(),
        Some(&b"nonlatin-png"[..])
    );
    assert_eq!(fonts.accented_png.as_deref(), Some(&b"pack-accented"[..]));

    let overlay = root.join("datapack");
    let plural = overlay.join("data/extra/recipes/plural.json");
    fs::create_dir_all(plural.parent().unwrap()).unwrap();
    fs::write(&plural, br#"{"type":"minecraft:smoking","ingredient":{"item":"minecraft:oak_log"},"result":{"id":"extra:charcoal"}}"#).unwrap();
    let block_tag = overlay.join("data/minecraft/tags/blocks/natural.json");
    fs::create_dir_all(block_tag.parent().unwrap()).unwrap();
    fs::write(block_tag, br#"{"values":["minecraft:stone"]}"#).unwrap();
    let catalog = VanillaJarDataCatalog::load(&jar, std::slice::from_ref(&overlay)).unwrap();
    assert_eq!(
        catalog.recipes["minecraft:shaped"].kind,
        VanillaRecipeKind::CraftingShaped
    );
    assert_eq!(
        catalog.recipes["minecraft:shapeless"].kind,
        VanillaRecipeKind::CraftingShapeless
    );
    assert_eq!(
        catalog.recipes["minecraft:smelting"].kind,
        VanillaRecipeKind::Smelting
    );
    assert_eq!(
        catalog.recipes["minecraft:stonecutting"].kind,
        VanillaRecipeKind::Stonecutting
    );
    assert_eq!(
        catalog.recipes["minecraft:smithing"].kind,
        VanillaRecipeKind::SmithingTransform
    );
    assert_eq!(
        catalog.recipes["extra:plural"].kind,
        VanillaRecipeKind::Smoking
    );
    assert_eq!(
        catalog.recipes["minecraft:shaped"]
            .result
            .as_ref()
            .unwrap()
            .count,
        4
    );
    assert_eq!(
        catalog.resolve_item_tag("minecraft:logs"),
        vec!["minecraft:oak_log", "minecraft:oak_planks"]
    );
    assert_eq!(
        catalog.resolve_item_tag("minecraft:cycle"),
        vec!["minecraft:stick"]
    );
    assert_eq!(
        catalog.resolve_block_tag("minecraft:natural"),
        vec!["minecraft:stone"]
    );
    assert_eq!(catalog.recipes_for_result("minecraft:stick").len(), 1);
    assert_eq!(catalog.recipes_using_item("minecraft:oak_log").len(), 2);

    // A corrupt/traversal hash is ignored; English remains available.
    let bad = root.join("bad-assets");
    let idx = bad.join("indexes/17.json");
    fs::create_dir_all(idx.parent().unwrap()).unwrap();
    fs::write(
        idx,
        r#"{"objects":{"minecraft/lang/ja_jp.json":{"hash":"../../escape","size":4}}}"#,
    )
    .unwrap();
    let safe = VanillaLanguageTable::load(&jar, Some(&bad), Some("17"), "ja_jp", &[], &[]).unwrap();
    assert_eq!(safe.translate("hello"), Some("Hello"));
    let malformed = root.join("malformed.jar");
    fs::write(
        &malformed,
        stored_zip(&[("assets/minecraft/lang/en_us.json", b"{broken")]),
    )
    .unwrap();
    let malformed_table =
        VanillaLanguageTable::load(&malformed, None, None, "en_us", &[], &[]).unwrap();
    assert_eq!(malformed_table.translate("hello"), None);
}

// SPDX-License-Identifier: LGPL-3.0-or-later
//! Runtime entity appearance resolver backed by one uploadable texture atlas.
//!
//! The public [`VanillaEntityTextureAtlas`] loader is intentionally reused as
//! the construction boundary: this module encodes the base atlas textures plus
//! the extended official entity catalog into a bounded in-memory archive, then
//! tracks the finer-grained UV rectangles inside the resulting GPU-ready
//! atlas. No Mojang asset bytes are stored in this crate.

use std::collections::{BTreeMap, BTreeSet};

use megu_client_mesh::entity_rig::classify_entity_rig;
use megu_core::hash::crc32_ieee;

use crate::player_skins::{ResolvedPlayerSkin, normalize_skin_to_64x64};
use crate::vanilla_jar::{
    VanillaEntityTextureAtlas, ber_texture_path_for_name, entity_texture_path_for_id,
    load_vanilla_entity_atlas_from_jar_bytes,
};
use crate::vanilla_official_extras::{VanillaOfficialExtras, default_skin_for_uuid};

const TEXTURE_PREFIX: &str = "assets/minecraft/textures/";
const COMPOSITE_NOMINAL_WIDTH: u32 = 1024;
const COMPOSITE_MAX_ENTRY_BYTES: usize = 7 * 1024 * 1024;
const COMPOSITE_TARGETS: &[usize] = &[1_000_000, 650_000, 400_000, 220_000];
const MAX_DYNAMIC_PLAYER_SKINS: usize = 256;
const MAX_CAPE_PIXELS: usize = 128 * 64;

const SKIN_NAMES: [&str; 9] = [
    "alex", "ari", "efe", "kai", "makena", "noor", "steve", "sunny", "zuri",
];
const ARMOR_MATERIALS: [&str; 7] = [
    "leather",
    "chainmail",
    "iron",
    "gold",
    "diamond",
    "netherite",
    "turtle",
];
const COLORS: [&str; 16] = [
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];
const WOODS: [&str; 11] = [
    "oak", "spruce", "birch", "jungle", "acacia", "dark_oak", "mangrove", "cherry", "bamboo",
    "crimson", "warped",
];
const BER_NAMES: [&str; 13] = [
    "chest",
    "trapped_chest",
    "ender_chest",
    "shulker",
    "bed",
    "sign",
    "hanging_sign",
    "banner",
    "bell",
    "enchanting_table",
    "beacon",
    "conduit",
    "decorated_pot",
];

/// Visual overlay category consumed by the 3D entity rig renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityOverlayKind {
    ArmorLayer1,
    ArmorLayer2,
    SheepWool,
    CreeperCharge,
    EmissiveEyes,
    VillagerBiome,
    VillagerProfession,
    VillagerLevel,
    WolfCollar,
    HorseMarking,
    HorseArmor,
    Cape,
    EnchantedGlint,
}

/// One additional render layer over an entity base texture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityOverlayLayer {
    pub uv: [f32; 4],
    pub tint: [f32; 3],
    pub emissive: bool,
    pub inflated: f32,
    pub kind: EntityOverlayKind,
}

/// Resolved base texture and overlay stack for one entity frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntityVisualLayers {
    pub base_uv: Option<[f32; 4]>,
    pub overlays: Vec<EntityOverlayLayer>,
    pub slim_arms: bool,
}

/// One worn armor item. `item_name` accepts either `minecraft:diamond_helmet`
/// or `diamond_helmet` spellings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EquipmentItem<'a> {
    pub item_name: &'a str,
    pub enchanted: bool,
}

impl<'a> EquipmentItem<'a> {
    pub fn new(item_name: &'a str, enchanted: bool) -> Self {
        Self {
            item_name,
            enchanted,
        }
    }
}

/// Vanilla equipment slots relevant to armor-layer rendering.
#[derive(Clone, Copy, Debug, Default)]
pub struct EntityEquipmentSlots<'a> {
    pub head: Option<EquipmentItem<'a>>,
    pub chest: Option<EquipmentItem<'a>>,
    pub legs: Option<EquipmentItem<'a>>,
    pub feet: Option<EquipmentItem<'a>>,
}

/// Appearance query for a player or mob. `variant` is the primary mob variant
/// (cat type, rabbit type, fox color, frog temperature, axolotl color, parrot
/// color, panda personality, horse color, wolf biome, shulker color or boat
/// wood); specialized state uses the named fields.
#[derive(Clone, Debug, Default)]
pub struct EntityAppearanceQuery<'a> {
    pub entity_type_id: Option<u16>,
    pub entity_name: Option<&'a str>,
    pub uuid: Option<[u8; 16]>,
    pub variant: Option<&'a str>,
    pub color: Option<&'a str>,
    pub sheared: bool,
    pub powered: bool,
    pub tame: bool,
    pub angry: bool,
    pub marking: Option<&'a str>,
    pub horse_armor: Option<&'a str>,
    pub collar_color: Option<&'a str>,
    pub villager_biome: Option<&'a str>,
    pub villager_profession: Option<&'a str>,
    /// Vanilla level 1..=5 (`stone`..`emerald` badges); zero means novice.
    pub villager_level: u8,
    pub equipment: EntityEquipmentSlots<'a>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TexturePixels {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl TexturePixels {
    fn new(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let expected = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        (width != 0 && height != 0 && rgba.len() == expected).then_some(Self {
            width,
            height,
            rgba,
        })
    }

    fn area(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DynamicPlayerAppearance {
    skin: TexturePixels,
    slim: bool,
    cape: Option<TexturePixels>,
}

#[derive(Clone, Copy, Debug)]
struct CarrierSlot {
    entity_type_id: u16,
    path: &'static str,
}

#[derive(Clone, Debug)]
struct CompositePlacement {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug)]
struct PackedComposite {
    width: u32,
    height: u32,
    placements: BTreeMap<String, CompositePlacement>,
    png: Vec<u8>,
}

/// Extended entity/player appearance atlas with explicit upload revisioning.
///
/// `revision` starts at zero for the initially constructed atlas and increments
/// only when a dynamic player skin or cape changes the pixels. `dirty` is true
/// initially and after every revision bump; `take_dirty`/`mark_uploaded` clear
/// it after `renderer.upload_entity_texture_atlas(atlas.atlas())`.
#[derive(Clone, Debug)]
pub struct RuntimeEntityAppearanceAtlas {
    atlas: VanillaEntityTextureAtlas,
    revision: u64,
    dirty: bool,
    direct_png_entries: BTreeMap<String, Vec<u8>>,
    composite_sources: BTreeMap<String, TexturePixels>,
    dynamic_players: BTreeMap<[u8; 16], DynamicPlayerAppearance>,
    texture_uvs: BTreeMap<String, [f32; 4]>,
}

impl RuntimeEntityAppearanceAtlas {
    /// Build a unified atlas from the existing representative entity atlas and
    /// the complete official extras catalog.
    pub fn new(base_atlas: &VanillaEntityTextureAtlas, extras: &VanillaOfficialExtras) -> Self {
        let direct_images = extract_base_images(base_atlas);
        let direct_png_entries = direct_images
            .iter()
            .filter_map(|(path, image)| {
                encode_png_rgba(image.width, image.height, &image.rgba)
                    .map(|png| (path.clone(), png))
            })
            .collect();
        let mut composite_sources = official_composite_catalog(extras);
        // Carrier slots are chosen among entity paths already represented in
        // the extended catalog wherever possible. If a rarer base carrier is
        // selected for a very large dynamic-skin workload, keep its original
        // texture available inside that composite too.
        for slot in carrier_slots() {
            let key = canonical_key(slot.path);
            if let std::collections::btree_map::Entry::Vacant(entry) = composite_sources.entry(key)
            {
                if let Some(image) = direct_images.get(slot.path) {
                    entry.insert(image.clone());
                }
            }
        }
        let mut out = Self {
            atlas: base_atlas.clone(),
            revision: 0,
            dirty: true,
            direct_png_entries,
            composite_sources,
            dynamic_players: BTreeMap::new(),
            texture_uvs: BTreeMap::new(),
        };
        out.rebuild_atlas();
        out
    }

    /// GPU-uploadable atlas pixels. UVs for the extended appearance resolver
    /// are exposed through [`Self::texture_uv`], [`Self::resolve_entity`] and
    /// [`Self::resolve_ber`].
    pub fn atlas(&self) -> &VanillaEntityTextureAtlas {
        &self.atlas
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn dirty(&self) -> bool {
        self.is_dirty()
    }

    /// Return and clear the upload-needed flag in one call.
    pub fn take_dirty(&mut self) -> bool {
        let dirty = self.dirty;
        self.dirty = false;
        dirty
    }

    /// Mark the current revision as uploaded to the GPU.
    pub fn mark_uploaded(&mut self) {
        self.dirty = false;
    }

    /// Insert or replace a downloaded Mojang skin/cape. Returns `true` only
    /// when the visual pixels or arm model changed and the atlas revision was
    /// bumped. Empty/default skin payloads remove a previous custom override.
    pub fn upsert_player_skin(&mut self, uuid: [u8; 16], resolved: &ResolvedPlayerSkin) -> bool {
        if uuid != resolved.uuid {
            return false;
        }
        let skin = if resolved.skin_rgba.is_empty() {
            None
        } else {
            normalize_skin_to_64x64(resolved.width, resolved.height, &resolved.skin_rgba)
                .ok()
                .and_then(|(width, height, rgba)| {
                    ((width, height) == (64, 64))
                        .then(|| TexturePixels::new(width, height, rgba))
                        .flatten()
                })
        };
        let cape = resolved
            .cape_rgba
            .as_ref()
            .and_then(|(width, height, rgba)| {
                let pixels = (*width as usize).checked_mul(*height as usize)?;
                (*width <= 512 && *height <= 512 && pixels <= MAX_CAPE_PIXELS)
                    .then(|| TexturePixels::new(*width, *height, rgba.clone()))
                    .flatten()
            });
        if resolved.skin_rgba.is_empty() {
            if self.dynamic_players.remove(&uuid).is_none() {
                return false;
            }
        } else {
            let Some(skin) = skin else {
                return false;
            };
            let next = DynamicPlayerAppearance {
                skin,
                slim: resolved.slim,
                cape,
            };
            if self.dynamic_players.get(&uuid) == Some(&next) {
                return false;
            }
            if !self.dynamic_players.contains_key(&uuid)
                && self.dynamic_players.len() >= MAX_DYNAMIC_PLAYER_SKINS
            {
                return false;
            }
            self.dynamic_players.insert(uuid, next);
        }

        let skin_key = dynamic_skin_key(uuid);
        let cape_key = dynamic_cape_key(uuid);
        self.composite_sources.remove(&skin_key);
        self.composite_sources.remove(&cape_key);
        if let Some(player) = self.dynamic_players.get(&uuid) {
            self.composite_sources.insert(skin_key, player.skin.clone());
            if let Some(cape) = &player.cape {
                self.composite_sources.insert(cape_key, cape.clone());
            }
        }
        self.rebuild_atlas();
        self.revision = self.revision.saturating_add(1);
        self.dirty = true;
        true
    }

    /// Look up any canonical texture rectangle. Keys may be full asset paths,
    /// paths below `textures/`, entity-relative paths, or the resolver's
    /// canonical `entity/...` / `models/armor/...` keys.
    pub fn texture_uv(&self, key: &str) -> Option<[f32; 4]> {
        self.texture_uvs.get(&lookup_key(key)).copied()
    }

    /// Current skin UV for a player UUID: a downloaded custom skin when one is
    /// present, otherwise the exact UUID-derived official default.
    pub fn player_skin_uv(&self, uuid: [u8; 16]) -> Option<[f32; 4]> {
        if self.dynamic_players.contains_key(&uuid) {
            return self.texture_uv(&dynamic_skin_key(uuid));
        }
        let (name, slim) = default_skin_for_uuid(uuid);
        self.texture_uv(&format!(
            "entity/player/{}/{name}.png",
            if slim { "slim" } else { "wide" }
        ))
    }

    /// Downloaded cape UV for a player UUID, if a cape was supplied.
    pub fn player_cape_uv(&self, uuid: [u8; 16]) -> Option<[f32; 4]> {
        self.dynamic_players
            .get(&uuid)
            .filter(|player| player.cape.is_some())
            .and_then(|_| self.texture_uv(&dynamic_cape_key(uuid)))
    }

    /// Resolve a complete entity appearance, including variant selection,
    /// state overlays, armor layers and player capes.
    pub fn resolve_entity(&self, query: &EntityAppearanceQuery<'_>) -> EntityVisualLayers {
        let name = query_entity_name(query);
        let mut out = EntityVisualLayers::default();
        if name == "player" || query.uuid.is_some() {
            self.resolve_player(query, &mut out);
        } else {
            out.base_uv = self.resolve_mob_base_uv(query, &name);
        }
        self.push_mob_state_overlays(query, &name, &mut out);
        self.push_equipment_overlays(query, &mut out);
        out
    }

    /// Resolve a colored/wood/variant block-entity texture rectangle.
    pub fn resolve_ber(&self, kind: &str, variant: &str) -> Option<[f32; 4]> {
        let kind = normalize_token(kind);
        let variant = normalize_token(variant);
        let key = match kind.as_str() {
            "bed" => format!(
                "entity/bed/{}.png",
                if variant.is_empty() { "red" } else { &variant }
            ),
            "shulker" | "shulker_box" | "shulkerbox" => {
                if variant.is_empty() || variant == "undyed" || variant == "default" {
                    "entity/shulker/shulker.png".to_owned()
                } else {
                    format!("entity/shulker/shulker_{variant}.png")
                }
            }
            "chest" | "trapped_chest" | "ender_chest" => {
                let base = match kind.as_str() {
                    "trapped_chest" => "trapped",
                    "ender_chest" => "ender",
                    _ => "normal",
                };
                let file = match variant.as_str() {
                    "" => base.to_owned(),
                    "left" => format!("{base}_left"),
                    "right" => format!("{base}_right"),
                    "christmas" => "christmas".to_owned(),
                    "christmas_left" => "christmas_left".to_owned(),
                    "christmas_right" => "christmas_right".to_owned(),
                    other => other.to_owned(),
                };
                format!("entity/chest/{file}.png")
            }
            "sign" | "standing_sign" => format!(
                "entity/signs/{}.png",
                if variant.is_empty() { "oak" } else { &variant }
            ),
            "hanging_sign" => format!(
                "entity/signs/hanging/{}.png",
                if variant.is_empty() { "oak" } else { &variant }
            ),
            "banner" | "standing_banner" | "wall_banner" => "entity/banner_base.png".to_owned(),
            "shield" => {
                if variant == "nopattern" || variant == "no_pattern" {
                    "entity/shield_base_nopattern.png".to_owned()
                } else {
                    "entity/shield_base.png".to_owned()
                }
            }
            other => {
                let fixed = ber_texture_path_for_name(other)?;
                canonical_key(fixed)
            }
        };
        self.texture_uv(&key)
    }

    fn rebuild_atlas(&mut self) {
        let packed = pack_composite_entries(&self.composite_sources);
        let slots = carrier_slots();
        let used = packed.len().min(slots.len());
        let selected_paths: BTreeSet<&str> =
            slots.iter().take(used).map(|slot| slot.path).collect();
        let mut zip_inputs: Vec<(&str, &[u8])> = Vec::new();
        for (path, png) in &self.direct_png_entries {
            if !selected_paths.contains(path.as_str()) {
                zip_inputs.push((path, png));
            }
        }
        for (slot, composite) in slots.iter().zip(packed.iter()) {
            zip_inputs.push((slot.path, &composite.png));
        }
        let jar = build_stored_zip(&zip_inputs);
        self.atlas = load_vanilla_entity_atlas_from_jar_bytes(&jar, &[]);

        let mut uvs = BTreeMap::new();
        for entity_type_id in 0..130u16 {
            let Some(path) = entity_texture_path_for_id(entity_type_id) else {
                continue;
            };
            if selected_paths.contains(path) {
                continue;
            }
            if let Some(uv) = self.atlas.entity_uv(entity_type_id) {
                uvs.entry(canonical_key(path)).or_insert(uv);
            }
        }
        for name in BER_NAMES {
            if let (Some(path), Some(uv)) =
                (ber_texture_path_for_name(name), self.atlas.ber_uv(name))
            {
                uvs.entry(canonical_key(path)).or_insert(uv);
            }
        }
        let selected_keys: BTreeSet<String> = selected_paths
            .iter()
            .map(|path| canonical_key(path))
            .collect();
        for (slot, composite) in slots.iter().zip(packed.iter()) {
            let Some(outer) = self.atlas.entity_uv(slot.entity_type_id) else {
                continue;
            };
            for (key, place) in &composite.placements {
                let uv = subrect_uv(outer, composite.width, composite.height, place);
                if selected_keys.contains(key) || !uvs.contains_key(key) {
                    uvs.insert(key.clone(), uv);
                }
            }
        }
        self.texture_uvs = uvs;
    }

    fn resolve_player(&self, query: &EntityAppearanceQuery<'_>, out: &mut EntityVisualLayers) {
        let uuid = query.uuid.unwrap_or([0; 16]);
        if let Some(player) = self.dynamic_players.get(&uuid) {
            out.base_uv = self.texture_uv(&dynamic_skin_key(uuid));
            out.slim_arms = player.slim;
            if player.cape.is_some() {
                if let Some(uv) = self.texture_uv(&dynamic_cape_key(uuid)) {
                    out.overlays.push(EntityOverlayLayer {
                        uv,
                        tint: [1.0, 1.0, 1.0],
                        emissive: false,
                        inflated: 0.0,
                        kind: EntityOverlayKind::Cape,
                    });
                }
            }
        } else {
            let (skin, slim) = default_skin_for_uuid(uuid);
            out.base_uv = self.texture_uv(&format!(
                "entity/player/{}/{skin}.png",
                if slim { "slim" } else { "wide" }
            ));
            out.slim_arms = slim;
        }
    }

    fn resolve_mob_base_uv(
        &self,
        query: &EntityAppearanceQuery<'_>,
        name: &str,
    ) -> Option<[f32; 4]> {
        if let Some(key) = variant_base_key(query, name) {
            if let Some(uv) = self.texture_uv(&key) {
                return Some(uv);
            }
        }
        let id = query.entity_type_id.or_else(|| entity_id_for_name(name))?;
        let path = entity_texture_path_for_id(id)?;
        self.texture_uv(&canonical_key(path))
    }

    fn push_mob_state_overlays(
        &self,
        query: &EntityAppearanceQuery<'_>,
        name: &str,
        out: &mut EntityVisualLayers,
    ) {
        match name {
            "sheep" if !query.sheared => {
                let color = query
                    .color
                    .or(query.variant)
                    .map(normalize_token)
                    .filter(|color| !color.is_empty())
                    .unwrap_or_else(|| "white".to_owned());
                self.push_overlay(
                    out,
                    "entity/sheep/sheep_fur.png",
                    EntityOverlayKind::SheepWool,
                    dye_color(&color),
                    false,
                    0.05,
                );
            }
            "creeper" if query.powered => {
                self.push_overlay(
                    out,
                    "entity/creeper/creeper_armor.png",
                    EntityOverlayKind::CreeperCharge,
                    [1.0, 1.0, 1.0],
                    false,
                    0.2,
                );
            }
            "spider" | "cave_spider" => {
                self.push_overlay(
                    out,
                    "entity/spider_eyes.png",
                    EntityOverlayKind::EmissiveEyes,
                    [1.0, 1.0, 1.0],
                    true,
                    0.0,
                );
            }
            "enderman" => {
                self.push_overlay(
                    out,
                    "entity/enderman/enderman_eyes.png",
                    EntityOverlayKind::EmissiveEyes,
                    [1.0, 1.0, 1.0],
                    true,
                    0.0,
                );
            }
            "phantom" => {
                self.push_overlay(
                    out,
                    "entity/phantom_eyes.png",
                    EntityOverlayKind::EmissiveEyes,
                    [1.0, 1.0, 1.0],
                    true,
                    0.0,
                );
            }
            "breeze" => {
                self.push_overlay(
                    out,
                    "entity/breeze/breeze_eyes.png",
                    EntityOverlayKind::EmissiveEyes,
                    [1.0, 1.0, 1.0],
                    true,
                    0.0,
                );
            }
            "warden" => {
                self.push_overlay(
                    out,
                    "entity/warden/warden_bioluminescent_layer.png",
                    EntityOverlayKind::EmissiveEyes,
                    [1.0, 1.0, 1.0],
                    true,
                    0.0,
                );
            }
            "wolf" if query.tame => {
                let color = query
                    .collar_color
                    .map(normalize_token)
                    .filter(|color| !color.is_empty())
                    .unwrap_or_else(|| "red".to_owned());
                self.push_overlay(
                    out,
                    "entity/wolf/wolf_collar.png",
                    EntityOverlayKind::WolfCollar,
                    dye_color(&color),
                    false,
                    0.02,
                );
            }
            "horse" => {
                if let Some(marking) = query.marking.map(normalize_token).filter(|m| !m.is_empty())
                {
                    self.push_overlay(
                        out,
                        &format!("entity/horse/horse_markings_{marking}.png"),
                        EntityOverlayKind::HorseMarking,
                        [1.0, 1.0, 1.0],
                        false,
                        0.01,
                    );
                }
                if let Some(armor) = query
                    .horse_armor
                    .map(normalize_token)
                    .filter(|armor| !armor.is_empty())
                {
                    self.push_overlay(
                        out,
                        &format!("entity/horse/armor/horse_armor_{armor}.png"),
                        EntityOverlayKind::HorseArmor,
                        [1.0, 1.0, 1.0],
                        false,
                        0.03,
                    );
                }
            }
            "villager" => {
                let biome = query
                    .villager_biome
                    .map(normalize_token)
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "plains".to_owned());
                self.push_overlay(
                    out,
                    &format!("entity/villager/type/{biome}.png"),
                    EntityOverlayKind::VillagerBiome,
                    [1.0, 1.0, 1.0],
                    false,
                    0.01,
                );
                if let Some(profession) = query
                    .villager_profession
                    .map(normalize_token)
                    .filter(|v| !v.is_empty() && v != "none")
                {
                    self.push_overlay(
                        out,
                        &format!("entity/villager/profession/{profession}.png"),
                        EntityOverlayKind::VillagerProfession,
                        [1.0, 1.0, 1.0],
                        false,
                        0.015,
                    );
                }
                let level = match query.villager_level.clamp(1, 5) {
                    1 => "stone",
                    2 => "iron",
                    3 => "gold",
                    4 => "diamond",
                    _ => "emerald",
                };
                self.push_overlay(
                    out,
                    &format!("entity/villager/profession_level/{level}.png"),
                    EntityOverlayKind::VillagerLevel,
                    [1.0, 1.0, 1.0],
                    false,
                    0.02,
                );
            }
            _ => {}
        }
    }

    fn push_equipment_overlays(
        &self,
        query: &EntityAppearanceQuery<'_>,
        out: &mut EntityVisualLayers,
    ) {
        let equipment = query.equipment;
        for (item, layer, kind) in [
            (equipment.head, 1, EntityOverlayKind::ArmorLayer1),
            (equipment.chest, 1, EntityOverlayKind::ArmorLayer1),
            (equipment.legs, 2, EntityOverlayKind::ArmorLayer2),
            (equipment.feet, 1, EntityOverlayKind::ArmorLayer1),
        ] {
            let Some(item) = item else { continue };
            let Some(material) = armor_material(item.item_name) else {
                continue;
            };
            let key = format!("models/armor/{material}_layer_{layer}.png");
            let tint = if material == "leather" {
                [0.627, 0.392, 0.216]
            } else {
                [1.0, 1.0, 1.0]
            };
            self.push_overlay(out, &key, kind, tint, false, armor_inflation(layer));
            if material == "leather" {
                self.push_overlay(
                    out,
                    &format!("models/armor/leather_layer_{layer}_overlay.png"),
                    kind,
                    [1.0, 1.0, 1.0],
                    false,
                    armor_inflation(layer) + 0.005,
                );
            }
            if item.enchanted {
                self.push_overlay(
                    out,
                    "misc/enchanted_glint_entity.png",
                    EntityOverlayKind::EnchantedGlint,
                    [1.0, 1.0, 1.0],
                    true,
                    armor_inflation(layer) + 0.01,
                );
            }
        }
    }

    fn push_overlay(
        &self,
        out: &mut EntityVisualLayers,
        key: &str,
        kind: EntityOverlayKind,
        tint: [f32; 3],
        emissive: bool,
        inflated: f32,
    ) {
        if let Some(uv) = self.texture_uv(key) {
            out.overlays.push(EntityOverlayLayer {
                uv,
                tint,
                emissive,
                inflated,
                kind,
            });
        }
    }
}

fn query_entity_name(query: &EntityAppearanceQuery<'_>) -> String {
    if let Some(name) = query.entity_name {
        return normalize_token(name);
    }
    query
        .entity_type_id
        .and_then(classify_entity_rig)
        .map(|spec| normalize_token(spec.name))
        .unwrap_or_default()
}

fn entity_id_for_name(name: &str) -> Option<u16> {
    let wanted = normalize_token(name);
    (0..130u16).find(|id| {
        classify_entity_rig(*id).is_some_and(|spec| normalize_token(spec.name) == wanted)
    })
}

fn variant_base_key(query: &EntityAppearanceQuery<'_>, name: &str) -> Option<String> {
    let variant = query.variant.map(normalize_token).unwrap_or_default();
    let color = query.color.map(normalize_token).unwrap_or_default();
    let key = match name {
        "horse" => {
            let color = match variant.as_str() {
                "" => "chestnut".to_owned(),
                "dark_brown" | "dark-brown" => "darkbrown".to_owned(),
                other => other.replace('-', "_"),
            };
            format!("entity/horse/horse_{}.png", color.replace('_', ""))
        }
        "wolf" => {
            let variant = match variant.as_str() {
                "" | "pale" | "wolf" => "pale".to_owned(),
                other => other.to_owned(),
            };
            let stem = if variant == "pale" {
                "wolf".to_owned()
            } else {
                format!("wolf_{variant}")
            };
            let stem = if query.angry {
                format!("{stem}_angry")
            } else if query.tame {
                format!("{stem}_tame")
            } else {
                stem
            };
            format!("entity/wolf/{stem}.png")
        }
        "cat" => format!(
            "entity/cat/{}.png",
            if variant.is_empty() {
                "tabby"
            } else {
                &variant
            }
        ),
        "rabbit" => format!(
            "entity/rabbit/{}.png",
            if variant.is_empty() {
                "black"
            } else {
                &variant
            }
        ),
        "fox" => match variant.as_str() {
            "" | "red" => "entity/fox/fox.png".to_owned(),
            "snow" | "snow_fox" | "white" => "entity/fox/snow_fox.png".to_owned(),
            "sleep" | "sleeping" | "fox_sleep" => "entity/fox/fox_sleep.png".to_owned(),
            "snow_sleep" | "snow_sleeping" | "snow_fox_sleep" => {
                "entity/fox/snow_fox_sleep.png".to_owned()
            }
            other => format!("entity/fox/{other}.png"),
        },
        "frog" => {
            let variant = if variant.is_empty() {
                "temperate"
            } else {
                variant.as_str()
            };
            if variant.ends_with("_frog") {
                format!("entity/frog/{variant}.png")
            } else {
                format!("entity/frog/{variant}_frog.png")
            }
        }
        "axolotl" => {
            let variant = if variant.is_empty() {
                "lucy"
            } else {
                variant.as_str()
            };
            if variant.starts_with("axolotl_") {
                format!("entity/axolotl/{variant}.png")
            } else {
                format!("entity/axolotl/axolotl_{variant}.png")
            }
        }
        "parrot" => {
            let variant = if variant.is_empty() {
                "red_blue"
            } else if variant == "gray" {
                "grey"
            } else {
                variant.as_str()
            };
            if variant.starts_with("parrot_") {
                format!("entity/parrot/{variant}.png")
            } else {
                format!("entity/parrot/parrot_{variant}.png")
            }
        }
        "panda" => match variant.as_str() {
            "" | "normal" | "panda" => "entity/panda/panda.png".to_owned(),
            other if other.ends_with("_panda") => format!("entity/panda/{other}.png"),
            other => format!("entity/panda/{other}_panda.png"),
        },
        "shulker" => {
            let color = if color.is_empty() { variant } else { color };
            if color.is_empty() || color == "undyed" || color == "default" {
                "entity/shulker/shulker.png".to_owned()
            } else {
                format!("entity/shulker/shulker_{color}.png")
            }
        }
        "boat" | "chest_boat" => {
            let wood = if variant.is_empty() {
                if color.is_empty() {
                    "oak".to_owned()
                } else {
                    color
                }
            } else {
                variant
            };
            let directory = if name == "chest_boat" {
                "chest_boat"
            } else {
                "boat"
            };
            format!("entity/{directory}/{wood}.png")
        }
        _ => return None,
    };
    Some(key)
}

fn armor_material(item_name: &str) -> Option<String> {
    let item = normalize_token(item_name);
    if item == "turtle_shell" {
        return Some("turtle".to_owned());
    }
    for material in ARMOR_MATERIALS {
        for suffix in ["helmet", "chestplate", "leggings", "boots"] {
            if item == format!("{material}_{suffix}") {
                return Some(material.to_owned());
            }
        }
    }
    None
}

fn armor_inflation(layer: u8) -> f32 {
    if layer == 2 { 0.08 } else { 0.06 }
}

fn dye_color(color: &str) -> [f32; 3] {
    match normalize_token(color).as_str() {
        "orange" => [0.976, 0.498, 0.114],
        "magenta" => [0.741, 0.259, 0.702],
        "light_blue" => [0.204, 0.663, 0.851],
        "yellow" => [1.0, 0.820, 0.122],
        "lime" => [0.420, 0.750, 0.090],
        "pink" => [0.950, 0.470, 0.650],
        "gray" => [0.270, 0.290, 0.310],
        "light_gray" => [0.620, 0.620, 0.590],
        "cyan" => [0.050, 0.550, 0.610],
        "purple" => [0.560, 0.240, 0.750],
        "blue" => [0.150, 0.250, 0.750],
        "brown" => [0.450, 0.270, 0.150],
        "green" => [0.250, 0.430, 0.100],
        "red" => [0.720, 0.120, 0.100],
        "black" => [0.080, 0.080, 0.100],
        _ => [1.0, 1.0, 1.0],
    }
}

fn extract_base_images(atlas: &VanillaEntityTextureAtlas) -> BTreeMap<String, TexturePixels> {
    let mut paths = BTreeSet::new();
    let mut uv_by_path = BTreeMap::new();
    for entity_type_id in 0..130u16 {
        if let (Some(path), Some(uv)) = (
            entity_texture_path_for_id(entity_type_id),
            atlas.entity_uv(entity_type_id),
        ) {
            paths.insert(path);
            uv_by_path.entry(path).or_insert(uv);
        }
    }
    for name in BER_NAMES {
        if let (Some(path), Some(uv)) = (ber_texture_path_for_name(name), atlas.ber_uv(name)) {
            paths.insert(path);
            uv_by_path.entry(path).or_insert(uv);
        }
    }
    paths
        .into_iter()
        .filter_map(|path| {
            uv_by_path
                .get(path)
                .and_then(|uv| extract_rect(atlas, *uv))
                .map(|image| (path.to_owned(), image))
        })
        .collect()
}

fn extract_rect(atlas: &VanillaEntityTextureAtlas, uv: [f32; 4]) -> Option<TexturePixels> {
    let width = atlas.width as usize;
    let height = atlas.height as usize;
    if width == 0 || height == 0 || atlas.rgba.len() != width * height * 4 {
        return None;
    }
    let x0 = (uv[0] * atlas.width as f32).round() as usize;
    let y0 = (uv[1] * atlas.height as f32).round() as usize;
    let x1 = (uv[2] * atlas.width as f32).round() as usize;
    let y1 = (uv[3] * atlas.height as f32).round() as usize;
    if x1 <= x0 || y1 <= y0 || x1 > width || y1 > height {
        return None;
    }
    let w = x1 - x0;
    let h = y1 - y0;
    let row_bytes = w * 4;
    let mut rgba = Vec::with_capacity(row_bytes * h);
    for y in y0..y1 {
        let start = (y * width + x0) * 4;
        rgba.extend_from_slice(&atlas.rgba[start..start + row_bytes]);
    }
    TexturePixels::new(w as u32, h as u32, rgba)
}

fn official_composite_catalog(extras: &VanillaOfficialExtras) -> BTreeMap<String, TexturePixels> {
    let mut out = BTreeMap::new();
    for name in SKIN_NAMES {
        for slim in [false, true] {
            let key = format!(
                "entity/player/{}/{name}.png",
                if slim { "slim" } else { "wide" }
            );
            let image = extras
                .skin_rgba(name, slim)
                .and_then(|image| TexturePixels::new(image.width, image.height, image.rgba.clone()))
                .unwrap_or_else(|| procedural_texture(&key));
            out.insert(key, image);
        }
    }
    for material in ARMOR_MATERIALS {
        for layer in [1u8, 2] {
            let key = format!("models/armor/{material}_layer_{layer}.png");
            let image = extras
                .armor_layer_rgba(material, layer, false)
                .and_then(|image| TexturePixels::new(image.width, image.height, image.rgba.clone()))
                .unwrap_or_else(|| procedural_texture(&key));
            out.insert(key, image);
        }
    }
    for layer in [1u8, 2] {
        let key = format!("models/armor/leather_layer_{layer}_overlay.png");
        let image = extras
            .armor_layer_rgba("leather", layer, true)
            .and_then(|image| TexturePixels::new(image.width, image.height, image.rgba.clone()))
            .unwrap_or_else(|| procedural_texture(&key));
        out.insert(key, image);
    }
    for key in entity_catalog_keys() {
        let canonical = format!("entity/{key}");
        let image = extras
            .entity_variant_rgba(&key)
            .and_then(|image| TexturePixels::new(image.width, image.height, image.rgba.clone()))
            .unwrap_or_else(|| procedural_texture(&canonical));
        out.insert(canonical, image);
    }
    for key in [
        "misc/enchanted_glint_entity.png",
        "misc/enchanted_glint_item.png",
    ] {
        out.entry(key.to_owned())
            .or_insert_with(|| procedural_texture(key));
    }
    out
}

fn entity_catalog_keys() -> Vec<String> {
    let mut keys = vec![
        "sheep/sheep.png".to_owned(),
        "sheep/sheep_fur.png".to_owned(),
        "creeper/creeper.png".to_owned(),
        "creeper/creeper_armor.png".to_owned(),
        "spider_eyes.png".to_owned(),
        "enderman/enderman_eyes.png".to_owned(),
        "phantom_eyes.png".to_owned(),
        "breeze/breeze_eyes.png".to_owned(),
        "warden/warden_bioluminescent_layer.png".to_owned(),
        "shield_base.png".to_owned(),
        "shield_base_nopattern.png".to_owned(),
        "elytra.png".to_owned(),
        "banner_base.png".to_owned(),
        "shulker/shulker.png".to_owned(),
        "shulker/spark.png".to_owned(),
    ];
    for color in [
        "white",
        "creamy",
        "chestnut",
        "brown",
        "black",
        "gray",
        "darkbrown",
    ] {
        keys.push(format!("horse/horse_{color}.png"));
    }
    for marking in ["white", "whitefield", "blackdots", "whitedots"] {
        keys.push(format!("horse/horse_markings_{marking}.png"));
    }
    for material in ["leather", "iron", "gold", "diamond"] {
        keys.push(format!("horse/armor/horse_armor_{material}.png"));
    }
    for key in [
        "donkey.png",
        "mule.png",
        "horse_skeleton.png",
        "horse_zombie.png",
    ] {
        keys.push(format!("horse/{key}"));
    }
    let wolf_variants = [
        "", "woods", "ashen", "black", "chestnut", "rusty", "snowy", "spotted", "striped",
    ];
    for variant in wolf_variants {
        let stem = if variant.is_empty() {
            "wolf".to_owned()
        } else {
            format!("wolf_{variant}")
        };
        keys.push(format!("wolf/{stem}.png"));
        keys.push(format!("wolf/{stem}_tame.png"));
        keys.push(format!("wolf/{stem}_angry.png"));
    }
    for key in [
        "wolf/wolf_collar.png",
        "wolf/wolf_armor.png",
        "wolf/wolf_armor_overlay.png",
        "wolf/wolf_armor_crackiness_low.png",
        "wolf/wolf_armor_crackiness_medium.png",
        "wolf/wolf_armor_crackiness_high.png",
    ] {
        keys.push(key.to_owned());
    }
    for key in [
        "all_black",
        "black",
        "british_shorthair",
        "calico",
        "jellie",
        "ocelot",
        "persian",
        "ragdoll",
        "red",
        "siamese",
        "tabby",
        "white",
    ] {
        keys.push(format!("cat/{key}.png"));
    }
    keys.push("cat/cat_collar.png".to_owned());
    for key in [
        "black",
        "brown",
        "caerbannog",
        "gold",
        "salt",
        "toast",
        "white",
        "white_splotched",
    ] {
        keys.push(format!("rabbit/{key}.png"));
    }
    for key in [
        "fox.png",
        "fox_sleep.png",
        "snow_fox.png",
        "snow_fox_sleep.png",
    ] {
        keys.push(format!("fox/{key}"));
    }
    for key in ["cold_frog.png", "temperate_frog.png", "warm_frog.png"] {
        keys.push(format!("frog/{key}"));
    }
    for key in ["blue", "cyan", "gold", "lucy", "wild"] {
        keys.push(format!("axolotl/axolotl_{key}.png"));
    }
    for key in ["blue", "green", "grey", "red_blue", "yellow_blue"] {
        keys.push(format!("parrot/parrot_{key}.png"));
    }
    for key in [
        "panda.png",
        "aggressive_panda.png",
        "brown_panda.png",
        "lazy_panda.png",
        "playful_panda.png",
        "weak_panda.png",
        "worried_panda.png",
    ] {
        keys.push(format!("panda/{key}"));
    }
    for biome in [
        "desert", "jungle", "plains", "savanna", "snow", "swamp", "taiga",
    ] {
        keys.push(format!("villager/type/{biome}.png"));
    }
    for profession in [
        "armorer",
        "butcher",
        "cartographer",
        "cleric",
        "farmer",
        "fisherman",
        "fletcher",
        "leatherworker",
        "librarian",
        "mason",
        "nitwit",
        "shepherd",
        "toolsmith",
        "weaponsmith",
    ] {
        keys.push(format!("villager/profession/{profession}.png"));
    }
    for level in ["stone", "iron", "gold", "diamond", "emerald"] {
        keys.push(format!("villager/profession_level/{level}.png"));
    }
    keys.push("villager/villager.png".to_owned());
    for color in COLORS {
        keys.push(format!("bed/{color}.png"));
        keys.push(format!("shulker/shulker_{color}.png"));
    }
    for key in [
        "normal.png",
        "normal_left.png",
        "normal_right.png",
        "trapped.png",
        "trapped_left.png",
        "trapped_right.png",
        "ender.png",
        "christmas.png",
        "christmas_left.png",
        "christmas_right.png",
    ] {
        keys.push(format!("chest/{key}"));
    }
    for wood in WOODS {
        keys.push(format!("boat/{wood}.png"));
        keys.push(format!("chest_boat/{wood}.png"));
        keys.push(format!("signs/{wood}.png"));
        keys.push(format!("signs/hanging/{wood}.png"));
    }
    keys.push("boat/pale_oak.png".to_owned());
    keys.push("chest_boat/pale_oak.png".to_owned());
    keys.sort();
    keys.dedup();
    keys
}

fn carrier_slots() -> Vec<CarrierSlot> {
    const DESIRED: &[&str] = &[
        "assets/minecraft/textures/entity/player/wide/steve.png",
        "assets/minecraft/textures/entity/cat/tabby.png",
        "assets/minecraft/textures/entity/horse/horse_chestnut.png",
        "assets/minecraft/textures/entity/boat/oak.png",
        "assets/minecraft/textures/entity/villager/villager.png",
        "assets/minecraft/textures/entity/wolf/wolf.png",
        "assets/minecraft/textures/entity/sheep/sheep.png",
        "assets/minecraft/textures/entity/shulker/shulker.png",
        "assets/minecraft/textures/painting/kebab.png",
        "assets/minecraft/textures/entity/armorstand/wood.png",
        "assets/minecraft/textures/entity/experience_orb.png",
        "assets/minecraft/textures/entity/phantom.png",
        "assets/minecraft/textures/entity/fox/fox.png",
        "assets/minecraft/textures/entity/frog/temperate_frog.png",
        "assets/minecraft/textures/entity/parrot/parrot_red_blue.png",
        "assets/minecraft/textures/entity/panda/panda.png",
    ];
    DESIRED
        .iter()
        .filter_map(|&path| {
            (0..130u16)
                .find(|id| entity_texture_path_for_id(*id) == Some(path))
                .map(|entity_type_id| CarrierSlot {
                    entity_type_id,
                    path,
                })
        })
        .collect()
}

fn pack_composite_entries(entries: &BTreeMap<String, TexturePixels>) -> Vec<PackedComposite> {
    if entries.is_empty() {
        return Vec::new();
    }
    for &target in COMPOSITE_TARGETS {
        let bins = split_composite_entries(entries, target);
        if bins.len() > carrier_slots().len() {
            continue;
        }
        let packed: Vec<_> = bins
            .iter()
            .filter_map(|bin| pack_one_composite(bin))
            .collect();
        if packed.len() == bins.len()
            && packed
                .iter()
                .all(|image| image.png.len() <= COMPOSITE_MAX_ENTRY_BYTES)
        {
            return packed;
        }
    }
    split_composite_entries(entries, *COMPOSITE_TARGETS.last().unwrap_or(&400_000))
        .iter()
        .take(carrier_slots().len())
        .filter_map(|bin| pack_one_composite(bin))
        .collect()
}

#[derive(Clone)]
struct CompositeInput {
    key: String,
    image: TexturePixels,
}

fn split_composite_entries(
    entries: &BTreeMap<String, TexturePixels>,
    target_pixels: usize,
) -> Vec<Vec<CompositeInput>> {
    let mut sorted: Vec<_> = entries
        .iter()
        .map(|(key, image)| CompositeInput {
            key: key.clone(),
            image: image.clone(),
        })
        .collect();
    sorted.sort_by(|a, b| {
        b.image
            .area()
            .cmp(&a.image.area())
            .then_with(|| a.key.cmp(&b.key))
    });
    let mut bins: Vec<Vec<CompositeInput>> = Vec::new();
    let mut current = Vec::new();
    let mut current_pixels = 0usize;
    for entry in sorted {
        let area = entry.image.area().max(1);
        if !current.is_empty() && current_pixels.saturating_add(area) > target_pixels {
            bins.push(current);
            current = Vec::new();
            current_pixels = 0;
        }
        current_pixels = current_pixels.saturating_add(area);
        current.push(entry);
    }
    if !current.is_empty() {
        bins.push(current);
    }
    bins
}

fn pack_one_composite(entries: &[CompositeInput]) -> Option<PackedComposite> {
    if entries.is_empty() {
        return None;
    }
    let max_width = entries
        .iter()
        .map(|entry| entry.image.width.saturating_add(2))
        .max()?;
    let width_limit = COMPOSITE_NOMINAL_WIDTH.max(max_width).min(4096);
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| {
        b.image
            .height
            .cmp(&a.image.height)
            .then_with(|| a.key.cmp(&b.key))
    });
    let mut placements = BTreeMap::new();
    let mut positioned = Vec::new();
    let (mut x, mut y, mut shelf, mut used_width) = (0u32, 0u32, 0u32, 0u32);
    for entry in sorted {
        let (w, h) = (entry.image.width, entry.image.height);
        let (cw, ch) = (w.saturating_add(2), h.saturating_add(2));
        if cw > width_limit || h == 0 || w == 0 {
            continue;
        }
        if x + cw > width_limit {
            x = 0;
            y = y.saturating_add(shelf);
            shelf = 0;
        }
        let px = x + 1;
        let py = y + 1;
        positioned.push((entry.key.clone(), entry.image.clone(), px, py));
        placements.insert(
            entry.key,
            CompositePlacement {
                x: px,
                y: py,
                width: w,
                height: h,
            },
        );
        x = x.saturating_add(cw);
        shelf = shelf.max(ch);
        used_width = used_width.max(x);
    }
    if positioned.is_empty() {
        return None;
    }
    let width = used_width.clamp(1, 4096);
    let height = y.saturating_add(shelf).clamp(1, 4096);
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for (key, image, px, py) in positioned {
        if py.saturating_add(image.height + 1) > height
            || px.saturating_add(image.width + 1) > width
        {
            placements.remove(&key);
            continue;
        }
        blit_with_gutter(&mut rgba, width, &image, px, py);
    }
    if placements.is_empty() {
        return None;
    }
    let png = encode_png_rgba(width, height, &rgba)?;
    Some(PackedComposite {
        width,
        height,
        placements,
        png,
    })
}

fn blit_with_gutter(dst: &mut [u8], atlas_width: u32, image: &TexturePixels, x: u32, y: u32) {
    for gy in 0..image.height + 2 {
        let sy = gy.saturating_sub(1).min(image.height - 1);
        let dy = y + gy - 1;
        for gx in 0..image.width + 2 {
            let sx = gx.saturating_sub(1).min(image.width - 1);
            let dx = x + gx - 1;
            let src = (sy * image.width + sx) as usize * 4;
            let dst_index = (dy * atlas_width + dx) as usize * 4;
            dst[dst_index..dst_index + 4].copy_from_slice(&image.rgba[src..src + 4]);
        }
    }
}

fn subrect_uv(outer: [f32; 4], width: u32, height: u32, place: &CompositePlacement) -> [f32; 4] {
    let du = outer[2] - outer[0];
    let dv = outer[3] - outer[1];
    let w = width.max(1) as f32;
    let h = height.max(1) as f32;
    [
        outer[0] + (place.x as f32 / w) * du,
        outer[1] + (place.y as f32 / h) * dv,
        outer[0] + ((place.x + place.width) as f32 / w) * du,
        outer[1] + ((place.y + place.height) as f32 / h) * dv,
    ]
}

fn canonical_key(path: &str) -> String {
    path.trim_start_matches(TEXTURE_PREFIX).to_owned()
}

fn lookup_key(key: &str) -> String {
    let mut key = key
        .trim()
        .trim_start_matches('/')
        .replace('\\', "/")
        .trim_start_matches(TEXTURE_PREFIX)
        .trim_start_matches("textures/")
        .to_owned();
    if !key.ends_with(".png") {
        key.push_str(".png");
    }
    if key.starts_with("dynamic/")
        || key.starts_with("entity/")
        || key.starts_with("models/")
        || key.starts_with("misc/")
        || key.starts_with("block/")
        || key.starts_with("item/")
        || key.starts_with("painting/")
    {
        key
    } else {
        format!("entity/{key}")
    }
}

fn dynamic_skin_key(uuid: [u8; 16]) -> String {
    format!("dynamic/player/{}/skin.png", uuid_hex(uuid))
}

fn dynamic_cape_key(uuid: [u8; 16]) -> String {
    format!("dynamic/player/{}/cape.png", uuid_hex(uuid))
}

fn uuid_hex(uuid: [u8; 16]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(32);
    for byte in uuid {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn normalize_token(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("minecraft:")
        .trim_end_matches(".png")
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
}

fn fallback_dimensions(key: &str) -> (u32, u32) {
    if key.starts_with("models/armor/")
        || key.contains("/signs/")
        || key == "entity/elytra.png"
        || key.contains("/cape.png")
    {
        (64, 32)
    } else if key.starts_with("entity/boat/") {
        (128, 64)
    } else if key.starts_with("misc/enchanted_glint") {
        (128, 128)
    } else {
        (64, 64)
    }
}

fn procedural_texture(key: &str) -> TexturePixels {
    let (width, height) = fallback_dimensions(key);
    let mut hash = 0x811c9dc5u32;
    for byte in key.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    let a = [
        64 + (hash & 0x7f) as u8,
        64 + ((hash >> 8) & 0x7f) as u8,
        64 + ((hash >> 16) & 0x7f) as u8,
        255,
    ];
    let b = [a[0] / 2, a[1] / 2, a[2] / 2, 255];
    let overlayish = key.contains("overlay")
        || key.contains("eyes")
        || key.contains("fur")
        || key.contains("collar")
        || key.contains("markings")
        || key.contains("glint")
        || key.contains("armor");
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let mut pixel = if ((x / 8 + y / 8) & 1) == 0 { a } else { b };
            if overlayish && (x + y) % 3 == 0 {
                pixel[3] = 0;
            }
            rgba.extend_from_slice(&pixel);
        }
    }
    TexturePixels {
        width,
        height,
        rgba,
    }
}

fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if width == 0
        || height == 0
        || width > 4096
        || height > 4096
        || rgba.len() != width as usize * height as usize * 4
    {
        return None;
    }
    let row_bytes = width as usize * 4;
    let mut filtered = Vec::with_capacity((row_bytes + 1) * height as usize);
    for row in 0..height as usize {
        filtered.push(0);
        let start = row * row_bytes;
        filtered.extend_from_slice(&rgba[start..start + row_bytes]);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    write_png_chunk(&mut out, b"IHDR", &ihdr);
    write_png_chunk(&mut out, b"IDAT", &zlib_stored(&filtered));
    write_png_chunk(&mut out, b"IEND", &[]);
    Some(out)
}

fn write_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(kind.len() + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32_ieee(&crc_input).to_be_bytes());
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65_535 * 5 + 11);
    out.extend_from_slice(&[0x78, 0x01]);
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0, 0, 0xff, 0xff]);
    } else {
        let count = data.len().div_ceil(65_535);
        for (index, chunk) in data.chunks(65_535).enumerate() {
            out.push(u8::from(index + 1 == count));
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut a = 1u32;
    let mut b = 0u32;
    for chunk in data.chunks(5_552) {
        for byte in chunk {
            a = (a + u32::from(*byte)) % MOD;
            b = (b + a) % MOD;
        }
    }
    (b << 16) | a
}

fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn w16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn w32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for &(name, payload) in entries {
        let local_offset = out.len() as u32;
        let crc = crc32_ieee(payload);
        let len = payload.len() as u32;
        let name_len = name.len() as u16;
        w32(&mut out, 0x0403_4b50);
        w16(&mut out, 20);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w16(&mut out, 0);
        w32(&mut out, crc);
        w32(&mut out, len);
        w32(&mut out, len);
        w16(&mut out, name_len);
        w16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);

        w32(&mut central, 0x0201_4b50);
        w16(&mut central, 20);
        w16(&mut central, 20);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, crc);
        w32(&mut central, len);
        w32(&mut central, len);
        w16(&mut central, name_len);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w16(&mut central, 0);
        w32(&mut central, 0);
        w32(&mut central, local_offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    w32(&mut out, 0x0605_4b50);
    w16(&mut out, 0);
    w16(&mut out, 0);
    w16(&mut out, entries.len() as u16);
    w16(&mut out, entries.len() as u16);
    w32(&mut out, central_size);
    w32(&mut out, central_offset);
    w16(&mut out, 0);
    out
}

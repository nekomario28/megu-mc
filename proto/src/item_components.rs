// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded, typed protocol-767 item-component values for untrusted creative stacks.
//!
//! Component values have no length prefix in a vanilla slot. This API accepts
//! already-separated `RawComponent` values and validates every supported shape.
//! A peer's *whole* slot still needs structural decoding before these values
//! can be passed to the sanitizer.

use std::collections::HashSet;

use crate::codec::{ItemStack, RawComponent};
use crate::data_v767::{V767_COMPONENTS, V767_ITEMS};
use crate::wire::{BlockPos, ProtoError, WireReader, WireWriter};

pub const MAX_COMPONENT_BYTES: usize = 4_096;
pub const MAX_STACK_COMPONENTS_BYTES: usize = 8_192;
pub const MAX_LIST_ENTRIES: usize = 64;
pub const MAX_CONTAINER_SLOTS: usize = 27;
pub const MAX_CONTAINER_DEPTH: usize = 1;
const MAX_NBT_DEPTH: usize = 32;
const MAX_BOOK_PAGES: usize = 100;
const MAX_CHARGED_PROJECTILES: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enchantment {
    pub id: u32,
    pub level: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PotionEffect {
    pub id: u32,
    pub amplifier: u32,
    pub duration: u32,
    pub ambient: bool,
    pub show_particles: bool,
    pub show_icon: bool,
    pub hidden: Option<Box<PotionEffectDetails>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PotionEffectDetails {
    pub amplifier: u32,
    pub duration: u32,
    pub ambient: bool,
    pub show_particles: bool,
    pub show_icon: bool,
    pub hidden: Option<Box<PotionEffectDetails>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PotionContents {
    pub potion: Option<u32>,
    pub custom_color: Option<u32>,
    pub effects: Vec<PotionEffect>,
    pub custom_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FireworkExplosion {
    pub shape: u8,
    pub colors: Vec<u32>,
    pub fade_colors: Vec<u32>,
    pub trail: bool,
    pub twinkle: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BannerPatternKind {
    Registry(u32),
    Inline {
        asset_id: String,
        translation_key: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BannerPattern {
    pub pattern: BannerPatternKind,
    pub color: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileProperty {
    pub name: String,
    pub value: String,
    pub signature: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: Option<String>,
    pub uuid: Option<[u8; 16]>,
    pub properties: Vec<ProfileProperty>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Instrument {
    Registry(u32),
    Inline {
        sound: String,
        fixed_range: Option<f32>,
        use_duration: f32,
        range: f32,
        description: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdSet {
    Tag(String),
    Ids(Vec<u32>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockPropertyValue {
    Exact(String),
    Range { min: String, max: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockProperty {
    pub name: String,
    pub value: BlockPropertyValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockPredicate {
    pub blocks: Option<IdSet>,
    pub properties: Option<Vec<BlockProperty>>,
    pub nbt: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttributeModifier {
    pub attribute: u32,
    pub id: String,
    pub amount: f64,
    pub operation: u8,
    pub slot: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FoodEffect {
    pub effect: u32,
    pub probability: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Food {
    pub nutrition: u32,
    pub saturation: f32,
    pub can_always_eat: bool,
    pub seconds_to_eat: f32,
    pub converts_to: ItemStack,
    pub effects: Vec<FoodEffect>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolRule {
    pub blocks: IdSet,
    pub speed: Option<f32>,
    pub correct_drop_for_blocks: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub rules: Vec<ToolRule>,
    pub default_mining_speed: f32,
    pub damage_per_block: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookPage {
    pub text: String,
    pub filtered: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenPage {
    pub text: Vec<u8>,
    pub filtered: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenBook {
    pub title: String,
    pub filtered_title: Option<String>,
    pub author: String,
    pub generation: u8,
    pub pages: Vec<WrittenPage>,
    pub resolved: bool,
}

/// The protocol's `ID or inline value` holder uses zero for inline data.
#[derive(Clone, Debug, PartialEq)]
pub enum Holder<T> {
    Registry(u32),
    Inline(T),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrimMaterial {
    pub asset_name: String,
    pub ingredient: u32,
    pub overrides: Vec<(String, String)>,
    pub description: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrimPattern {
    pub asset_id: String,
    pub template: u32,
    pub description: Vec<u8>,
    pub decal: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trim {
    pub material: Holder<TrimMaterial>,
    pub pattern: Holder<TrimPattern>,
    pub show_in_tooltip: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SoundEvent {
    pub name: String,
    pub fixed_range: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JukeboxSong {
    pub sound: Holder<SoundEvent>,
    pub description: Vec<u8>,
    pub duration: f32,
    pub comparator_output: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JukeboxSongRef {
    Name(String),
    Holder(Holder<JukeboxSong>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct JukeboxPlayable {
    pub song: JukeboxSongRef,
    pub show_in_tooltip: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LodestoneTracker {
    pub position: Option<(String, BlockPos)>,
    pub tracked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeeData {
    pub nbt: Vec<u8>,
    pub ticks_in_hive: u32,
    pub min_ticks_in_hive: u32,
}

/// All 57 Vanilla 1.21.1 `minecraft:data_component_type` entries.
/// Text is sanitized anonymous NBT; other NBT spans are structurally bounded.
#[derive(Clone, Debug, PartialEq)]
pub enum VerifiedComponent {
    CustomData(Vec<u8>),
    MaxStackSize(u32),
    MaxDamage(u32),
    CustomName(Vec<u8>),
    ItemName(Vec<u8>),
    Lore(Vec<Vec<u8>>),
    Rarity(u8),
    Enchantments {
        entries: Vec<Enchantment>,
        show_in_tooltip: bool,
    },
    StoredEnchantments {
        entries: Vec<Enchantment>,
        show_in_tooltip: bool,
    },
    CanPlaceOn {
        predicates: Vec<BlockPredicate>,
        show_in_tooltip: bool,
    },
    CanBreak {
        predicates: Vec<BlockPredicate>,
        show_in_tooltip: bool,
    },
    AttributeModifiers {
        entries: Vec<AttributeModifier>,
        show_in_tooltip: bool,
    },
    CustomModelData(u32),
    EnchantmentGlintOverride(bool),
    Unbreakable(bool),
    Damage(u32),
    RepairCost(u32),
    DyedColor {
        rgb: u32,
        show_in_tooltip: bool,
    },
    MapColor(u32),
    MapId(u32),
    MapDecorations(Vec<u8>),
    MapPostProcessing(u8),
    ChargedProjectiles(Vec<ItemStack>),
    BundleContents(Vec<ItemStack>),
    BaseColor(u8),
    Food(Food),
    Tool(Tool),
    PotionContents(PotionContents),
    SuspiciousStewEffects(Vec<(u32, u32)>),
    OminousBottleAmplifier(u32),
    Fireworks {
        flight_duration: u32,
        explosions: Vec<FireworkExplosion>,
    },
    FireworkExplosion(FireworkExplosion),
    BannerPatterns(Vec<BannerPattern>),
    HideAdditionalTooltip,
    HideTooltip,
    CreativeSlotLock,
    IntangibleProjectile(Vec<u8>),
    FireResistant,
    Instrument(Instrument),
    BlockState(Vec<(String, String)>),
    Profile(Profile),
    Container(Vec<ItemStack>),
    WritableBookContent(Vec<BookPage>),
    WrittenBookContent(WrittenBook),
    Trim(Trim),
    DebugStickState(Vec<u8>),
    EntityData(Vec<u8>),
    BucketEntityData(Vec<u8>),
    BlockEntityData(Vec<u8>),
    JukeboxPlayable(JukeboxPlayable),
    Recipes(Vec<u8>),
    LodestoneTracker(LodestoneTracker),
    NoteBlockSound(String),
    PotDecorations(Vec<u32>),
    Bees(Vec<BeeData>),
    Lock(Vec<u8>),
    ContainerLoot(Vec<u8>),
}

/// Sanitized, byte-exact encodable component patch with explicit drop counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SanitizedComponents {
    pub components: Vec<RawComponent>,
    pub kept: usize,
    pub dropped_unknown: usize,
    pub dropped_oversized: usize,
    pub dropped_invalid: usize,
}

fn invalid(what: &'static str) -> ProtoError {
    ProtoError::Malformed(what)
}

fn bound(value: usize, max: usize) -> Result<(), ProtoError> {
    if value > max {
        Err(ProtoError::LengthOutOfBounds { length: value, max })
    } else {
        Ok(())
    }
}

fn positive_id(r: &mut WireReader<'_>) -> Result<u32, ProtoError> {
    let n = r.read_varint()?;
    u32::try_from(n).map_err(|_| invalid("negative component integer"))
}

fn read_string(r: &mut WireReader<'_>) -> Result<String, ProtoError> {
    r.read_string(MAX_COMPONENT_BYTES)
}

fn read_list<T>(
    r: &mut WireReader<'_>,
    mut read: impl FnMut(&mut WireReader<'_>) -> Result<T, ProtoError>,
) -> Result<Vec<T>, ProtoError> {
    let count = r.read_count(MAX_LIST_ENTRIES)?;
    (0..count).map(|_| read(r)).collect()
}

fn put_list<T>(w: &mut WireWriter, list: &[T], mut write: impl FnMut(&mut WireWriter, &T)) {
    w.write_varint(list.len() as i32);
    for entry in list {
        write(w, entry);
    }
}

fn read_enchantments(r: &mut WireReader<'_>) -> Result<(Vec<Enchantment>, bool), ProtoError> {
    let entries = read_list(r, |r| {
        let id = positive_id(r)?;
        let level = positive_id(r)?;
        if level == 0 || level > 255 {
            return Err(invalid("enchantment level outside 1..=255"));
        }
        Ok(Enchantment { id, level })
    })?;
    Ok((entries, r.read_bool()?))
}

fn write_enchantments(w: &mut WireWriter, entries: &[Enchantment], tooltip: bool) {
    put_list(w, entries, |w, e| {
        w.write_varint(e.id as i32);
        w.write_varint(e.level as i32);
    });
    w.write_bool(tooltip);
}

fn read_effect_details(
    r: &mut WireReader<'_>,
    depth: usize,
) -> Result<PotionEffectDetails, ProtoError> {
    if depth >= 16 {
        return Err(invalid("potion effect nesting exceeds 16"));
    }
    let amplifier = positive_id(r)?;
    let duration = positive_id(r)?;
    let ambient = r.read_bool()?;
    let show_particles = r.read_bool()?;
    let show_icon = r.read_bool()?;
    let hidden = if r.read_bool()? {
        Some(Box::new(read_effect_details(r, depth + 1)?))
    } else {
        None
    };
    Ok(PotionEffectDetails {
        amplifier,
        duration,
        ambient,
        show_particles,
        show_icon,
        hidden,
    })
}

fn write_effect_details(w: &mut WireWriter, details: &PotionEffectDetails) {
    w.write_varint(details.amplifier as i32);
    w.write_varint(details.duration as i32);
    w.write_bool(details.ambient);
    w.write_bool(details.show_particles);
    w.write_bool(details.show_icon);
    w.write_bool(details.hidden.is_some());
    if let Some(hidden) = &details.hidden {
        write_effect_details(w, hidden);
    }
}

fn read_explosion(r: &mut WireReader<'_>) -> Result<FireworkExplosion, ProtoError> {
    let shape = positive_id(r)?;
    if shape > 4 {
        return Err(invalid("invalid firework explosion shape"));
    }
    let colors = read_list(r, |r| r.read_u32())?;
    let fade_colors = read_list(r, |r| r.read_u32())?;
    let trail = r.read_bool()?;
    let twinkle = r.read_bool()?;
    Ok(FireworkExplosion {
        shape: shape as u8,
        colors,
        fade_colors,
        trail,
        twinkle,
    })
}

fn write_explosion(w: &mut WireWriter, explosion: &FireworkExplosion) {
    w.write_varint(i32::from(explosion.shape));
    put_list(w, &explosion.colors, |w, color| w.write_u32(*color));
    put_list(w, &explosion.fade_colors, |w, color| w.write_u32(*color));
    w.write_bool(explosion.trail);
    w.write_bool(explosion.twinkle);
}

fn read_instrument(r: &mut WireReader<'_>) -> Result<Instrument, ProtoError> {
    let holder = positive_id(r)?;
    if holder != 0 {
        return Ok(Instrument::Registry(holder));
    }
    let sound_holder = positive_id(r)?;
    if sound_holder != 0 {
        return Err(invalid("unresolved inline instrument sound holder"));
    }
    let sound = read_string(r)?;
    let fixed_range = if r.read_bool()? {
        let range = r.read_f32()?;
        if !range.is_finite() || range < 0.0 {
            return Err(invalid("invalid instrument sound range"));
        }
        Some(range)
    } else {
        None
    };
    let use_duration = r.read_f32()?;
    let range = r.read_f32()?;
    if !use_duration.is_finite() || use_duration < 0.0 || !range.is_finite() || range < 0.0 {
        return Err(invalid("invalid instrument duration or range"));
    }
    let description = read_text(r)?;
    Ok(Instrument::Inline {
        sound,
        fixed_range,
        use_duration,
        range,
        description,
    })
}

fn write_instrument(w: &mut WireWriter, instrument: &Instrument) {
    match instrument {
        Instrument::Registry(id) => w.write_varint(*id as i32),
        Instrument::Inline {
            sound,
            fixed_range,
            use_duration,
            range,
            description,
        } => {
            w.write_varint(0);
            w.write_varint(0);
            w.write_string(sound);
            w.write_bool(fixed_range.is_some());
            if let Some(value) = fixed_range {
                w.write_f32(*value);
            }
            w.write_f32(*use_duration);
            w.write_f32(*range);
            w.raw(description);
        }
    }
}

fn read_enum(r: &mut WireReader<'_>, max: u32) -> Result<u8, ProtoError> {
    let value = positive_id(r)?;
    if value > max {
        return Err(invalid("component enum out of range"));
    }
    Ok(value as u8)
}

fn finite_nonnegative(value: f32) -> Result<f32, ProtoError> {
    if !value.is_finite() || value < 0.0 {
        return Err(invalid("invalid component float"));
    }
    Ok(value)
}

fn probability(value: f32) -> Result<f32, ProtoError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(invalid("invalid effect probability"));
    }
    Ok(value)
}

fn valid_identifier(value: &str) -> bool {
    let mut parts = value.split(':');
    let first = parts.next().unwrap_or("");
    let second = parts.next();
    if parts.next().is_some() || first.is_empty() {
        return false;
    }
    let (namespace, path) = if let Some(path) = second {
        (Some(first), path)
    } else {
        (None, first)
    };
    !path.is_empty()
        && namespace.is_none_or(|name| {
            name.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
        })
        && path
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-/".contains(&c))
}

fn read_identifier(r: &mut WireReader<'_>) -> Result<String, ProtoError> {
    let id = read_string(r)?;
    if !valid_identifier(&id) {
        return Err(invalid("invalid component identifier"));
    }
    Ok(id)
}

fn read_book_string(r: &mut WireReader<'_>, max: usize) -> Result<String, ProtoError> {
    let value = r.read_string(max)?;
    validate_book_string(&value, max)?;
    Ok(value)
}

fn validate_book_string(value: &str, max: usize) -> Result<(), ProtoError> {
    bound(value.len(), MAX_COMPONENT_BYTES)?;
    bound(value.encode_utf16().count(), max)?;
    if value
        .chars()
        .any(|c| (c.is_control() && c != '\n' && c != '\t') || c == '\u{7f}')
    {
        return Err(invalid("disallowed control in book text"));
    }
    Ok(())
}

// Validate non-text NBT without allocating a recursive value tree. The 32-level
// limit and the 64-element cap apply at every nested list and compound.
fn validate_nbt_payload(r: &mut WireReader<'_>, tag: u8, depth: usize) -> Result<(), ProtoError> {
    if depth > MAX_NBT_DEPTH {
        return Err(invalid("component NBT depth exceeds 32"));
    }
    match tag {
        1 => {
            r.take(1)?;
        }
        2 => {
            r.take(2)?;
        }
        3 | 5 => {
            r.take(4)?;
        }
        4 | 6 => {
            r.take(8)?;
        }
        7 | 11 | 12 => {
            let len = r.read_i32()?;
            if len < 0 {
                return Err(invalid("negative NBT array length"));
            }
            let width = if tag == 7 {
                1
            } else if tag == 11 {
                4
            } else {
                8
            };
            bound(len as usize, MAX_LIST_ENTRIES)?;
            r.take(len as usize * width)?;
        }
        8 => {
            nbt_string(r)?;
        }
        9 => {
            let child_tag = r.read_u8()?;
            let len = r.read_i32()?;
            if len < 0 || (child_tag == 0 && len != 0) {
                return Err(invalid("invalid NBT list"));
            }
            bound(len as usize, MAX_LIST_ENTRIES)?;
            if child_tag > 12 {
                return Err(invalid("invalid NBT element tag"));
            }
            for _ in 0..len {
                validate_nbt_payload(r, child_tag, depth + 1)?;
            }
        }
        10 => {
            let mut fields = HashSet::new();
            loop {
                let child_tag = r.read_u8()?;
                if child_tag == 0 {
                    break;
                }
                bound(fields.len() + 1, MAX_LIST_ENTRIES)?;
                let name = nbt_string(r)?;
                if !fields.insert(name) {
                    return Err(invalid("duplicate NBT key"));
                }
                validate_nbt_payload(r, child_tag, depth + 1)?;
            }
        }
        _ => return Err(invalid("invalid NBT component tag")),
    }
    Ok(())
}

fn read_opaque_nbt(r: &mut WireReader<'_>, root: u8) -> Result<Vec<u8>, ProtoError> {
    // Restrict the reader itself so a single component cannot consume an
    // oversized NBT payload before the final length check.
    let remaining = r.rest();
    let mut bounded = WireReader::new(&remaining[..remaining.len().min(MAX_COMPONENT_BYTES)]);
    let tag = bounded.read_u8()?;
    if tag != root {
        return Err(invalid("unexpected NBT component root"));
    }
    validate_nbt_payload(&mut bounded, tag, 0)?;
    let bytes = bounded.position();
    r.take(bytes)?;
    Ok(remaining[..bytes].to_vec())
}

fn read_optional_opaque_nbt(r: &mut WireReader<'_>) -> Result<Option<Vec<u8>>, ProtoError> {
    if r.rest().first() == Some(&0) {
        r.read_u8()?;
        Ok(None)
    } else {
        read_opaque_nbt(r, 10).map(Some)
    }
}

fn validate_opaque_nbt(bytes: &[u8], root: u8) -> Result<usize, ProtoError> {
    bound(bytes.len(), MAX_COMPONENT_BYTES)?;
    let mut r = WireReader::new(bytes);
    read_opaque_nbt(&mut r, root)?;
    r.finish()?;
    Ok(bytes.len())
}

fn read_id_set(r: &mut WireReader<'_>) -> Result<IdSet, ProtoError> {
    let count = positive_id(r)?;
    if count == 0 {
        Ok(IdSet::Tag(read_identifier(r)?))
    } else {
        bound(count as usize - 1, MAX_LIST_ENTRIES)?;
        Ok(IdSet::Ids(
            (1..count)
                .map(|_| positive_id(r))
                .collect::<Result<_, _>>()?,
        ))
    }
}

fn write_id_set(w: &mut WireWriter, blocks: &IdSet) {
    match blocks {
        IdSet::Tag(name) => {
            w.write_varint(0);
            w.write_string(name);
        }
        IdSet::Ids(ids) => {
            w.write_varint(ids.len() as i32 + 1);
            for id in ids {
                w.write_varint(*id as i32);
            }
        }
    }
}

fn read_block_predicate(r: &mut WireReader<'_>) -> Result<BlockPredicate, ProtoError> {
    let blocks = if r.read_bool()? {
        Some(read_id_set(r)?)
    } else {
        None
    };
    let properties = if r.read_bool()? {
        Some(read_list(r, |r| {
            let name = read_string(r)?;
            let value = if r.read_bool()? {
                BlockPropertyValue::Exact(read_string(r)?)
            } else {
                BlockPropertyValue::Range {
                    min: read_string(r)?,
                    max: read_string(r)?,
                }
            };
            Ok(BlockProperty { name, value })
        })?)
    } else {
        None
    };
    let nbt = read_optional_opaque_nbt(r)?;
    Ok(BlockPredicate {
        blocks,
        properties,
        nbt,
    })
}

fn write_block_predicate(w: &mut WireWriter, value: &BlockPredicate) {
    w.write_bool(value.blocks.is_some());
    if let Some(blocks) = &value.blocks {
        write_id_set(w, blocks);
    }
    w.write_bool(value.properties.is_some());
    if let Some(properties) = &value.properties {
        put_list(w, properties, |w, property| {
            w.write_string(&property.name);
            match &property.value {
                BlockPropertyValue::Exact(value) => {
                    w.write_bool(true);
                    w.write_string(value);
                }
                BlockPropertyValue::Range { min, max } => {
                    w.write_bool(false);
                    w.write_string(min);
                    w.write_string(max);
                }
            }
        });
    }
    if let Some(nbt) = &value.nbt {
        w.raw(nbt);
    } else {
        w.write_u8(0);
    }
}

fn read_holder<T>(
    r: &mut WireReader<'_>,
    read_inline: impl FnOnce(&mut WireReader<'_>) -> Result<T, ProtoError>,
) -> Result<Holder<T>, ProtoError> {
    let id = positive_id(r)?;
    if id == 0 {
        Ok(Holder::Inline(read_inline(r)?))
    } else {
        Ok(Holder::Registry(id))
    }
}

fn write_holder<T>(
    w: &mut WireWriter,
    value: &Holder<T>,
    write_inline: impl FnOnce(&mut WireWriter, &T),
) {
    match value {
        Holder::Registry(id) => w.write_varint(*id as i32),
        Holder::Inline(value) => {
            w.write_varint(0);
            write_inline(w, value);
        }
    }
}

fn read_trim_material(r: &mut WireReader<'_>) -> Result<TrimMaterial, ProtoError> {
    Ok(TrimMaterial {
        asset_name: read_identifier(r)?,
        ingredient: positive_id(r)?,
        overrides: read_list(r, |r| Ok((read_string(r)?, read_identifier(r)?)))?,
        description: read_text(r)?,
    })
}

fn write_trim_material(w: &mut WireWriter, material: &TrimMaterial) {
    w.write_string(&material.asset_name);
    w.write_varint(material.ingredient as i32);
    put_list(w, &material.overrides, |w, (name, asset)| {
        w.write_string(name);
        w.write_string(asset);
    });
    w.raw(&material.description);
}

fn read_trim_pattern(r: &mut WireReader<'_>) -> Result<TrimPattern, ProtoError> {
    Ok(TrimPattern {
        asset_id: read_identifier(r)?,
        template: positive_id(r)?,
        description: read_text(r)?,
        decal: r.read_bool()?,
    })
}

fn write_trim_pattern(w: &mut WireWriter, pattern: &TrimPattern) {
    w.write_string(&pattern.asset_id);
    w.write_varint(pattern.template as i32);
    w.raw(&pattern.description);
    w.write_bool(pattern.decal);
}

fn read_sound_event(r: &mut WireReader<'_>) -> Result<SoundEvent, ProtoError> {
    let name = read_identifier(r)?;
    let fixed_range = if r.read_bool()? {
        Some(finite_nonnegative(r.read_f32()?)?)
    } else {
        None
    };
    Ok(SoundEvent { name, fixed_range })
}

fn write_sound_event(w: &mut WireWriter, sound: &SoundEvent) {
    w.write_string(&sound.name);
    w.write_bool(sound.fixed_range.is_some());
    if let Some(value) = sound.fixed_range {
        w.write_f32(value);
    }
}

fn read_jukebox_song(r: &mut WireReader<'_>) -> Result<JukeboxSong, ProtoError> {
    let sound = read_holder(r, read_sound_event)?;
    let description = read_text(r)?;
    let duration = finite_nonnegative(r.read_f32()?)?;
    let comparator_output = read_enum(r, 15)?;
    Ok(JukeboxSong {
        sound,
        description,
        duration,
        comparator_output,
    })
}

fn write_jukebox_song(w: &mut WireWriter, song: &JukeboxSong) {
    write_holder(w, &song.sound, write_sound_event);
    w.raw(&song.description);
    w.write_f32(song.duration);
    w.write_varint(song.comparator_output as i32);
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Nbt {
    Text(String),
    Byte(i8),
    List(u8, Vec<Nbt>),
    Compound(Vec<(String, Nbt)>),
    Ignored,
}

fn nbt_string(r: &mut WireReader<'_>) -> Result<String, ProtoError> {
    let count = usize::from(r.read_u16()?);
    let bytes = r.take(count)?;
    Ok(std::str::from_utf8(bytes)
        .map_err(|_| ProtoError::InvalidUtf8)?
        .to_owned())
}

fn parse_nbt(r: &mut WireReader<'_>, tag: u8, depth: usize) -> Result<Nbt, ProtoError> {
    if depth > 16 {
        return Err(invalid("text NBT depth exceeds 16"));
    }
    match tag {
        1 => Ok(Nbt::Byte(r.read_i8()?)),
        2 => {
            r.take(2)?;
            Ok(Nbt::Ignored)
        }
        3 | 5 => {
            r.take(4)?;
            Ok(Nbt::Ignored)
        }
        4 | 6 => {
            r.take(8)?;
            Ok(Nbt::Ignored)
        }
        7 | 11 | 12 => {
            let count = r.read_i32()?;
            if count < 0 {
                return Err(invalid("negative NBT array length"));
            }
            let width = match tag {
                11 => 4,
                12 => 8,
                _ => 1,
            };
            bound(count as usize, MAX_COMPONENT_BYTES / width)?;
            r.take(count as usize * width)?;
            Ok(Nbt::Ignored)
        }
        8 => Ok(Nbt::Text(nbt_string(r)?)),
        9 => {
            let elem_tag = r.read_u8()?;
            let count = r.read_i32()?;
            if count < 0 {
                return Err(invalid("negative NBT list count"));
            }
            bound(count as usize, MAX_LIST_ENTRIES)?;
            if elem_tag == 0 && count != 0 {
                return Err(invalid("invalid NBT list type"));
            }
            let items = (0..count)
                .map(|_| parse_nbt(r, elem_tag, depth + 1))
                .collect::<Result<_, _>>()?;
            Ok(Nbt::List(elem_tag, items))
        }
        10 => {
            let mut entries = Vec::new();
            loop {
                let child = r.read_u8()?;
                if child == 0 {
                    break;
                }
                bound(entries.len() + 1, MAX_LIST_ENTRIES)?;
                let name = nbt_string(r)?;
                let value = parse_nbt(r, child, depth + 1)?;
                entries.push((name, value));
            }
            Ok(Nbt::Compound(entries))
        }
        _ => Err(invalid("invalid NBT tag")),
    }
}

fn tag_id(value: &Nbt) -> u8 {
    match value {
        Nbt::Text(_) => 8,
        Nbt::Byte(_) => 1,
        Nbt::List(_, _) => 9,
        Nbt::Compound(_) => 10,
        Nbt::Ignored => unreachable!("ignored NBT is never serialized"),
    }
}

fn put_nbt_string(w: &mut WireWriter, value: &str) {
    w.write_u16(value.len() as u16);
    w.raw(value.as_bytes());
}

fn put_nbt(w: &mut WireWriter, value: &Nbt) {
    match value {
        Nbt::Text(text) => put_nbt_string(w, text),
        Nbt::Byte(flag) => w.write_i8(*flag),
        Nbt::List(elem, items) => {
            w.write_u8(*elem);
            w.write_i32(items.len() as i32);
            for item in items {
                put_nbt(w, item);
            }
        }
        Nbt::Compound(fields) => {
            for (name, child) in fields {
                w.write_u8(tag_id(child));
                put_nbt_string(w, name);
                put_nbt(w, child);
            }
            w.write_u8(0);
        }
        Nbt::Ignored => unreachable!("ignored NBT is never serialized"),
    }
}

fn get_text<'a>(fields: &'a [(String, Nbt)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .and_then(|(_, val)| {
            if let Nbt::Text(text) = val {
                Some(text.as_str())
            } else {
                None
            }
        })
}

fn safe_color(color: &str) -> bool {
    matches!(
        color,
        "black"
            | "dark_blue"
            | "dark_green"
            | "dark_aqua"
            | "dark_red"
            | "dark_purple"
            | "gold"
            | "gray"
            | "dark_gray"
            | "blue"
            | "green"
            | "aqua"
            | "red"
            | "light_purple"
            | "yellow"
            | "white"
            | "reset"
    ) || (color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|c| c.is_ascii_hexdigit()))
}

fn sanitize_nbt(value: Nbt, depth: usize) -> Result<Nbt, ProtoError> {
    if depth > 16 {
        return Err(invalid("text nesting exceeds 16"));
    }
    match value {
        Nbt::Text(s) => Ok(Nbt::Text(s)),
        Nbt::List(_, list) => {
            let mut children = Vec::new();
            for value in list {
                let child = sanitize_nbt(value, depth + 1)?;
                match child {
                    Nbt::Compound(_) => children.push(child),
                    Nbt::Text(text) => {
                        children.push(Nbt::Compound(vec![("text".to_owned(), Nbt::Text(text))]))
                    }
                    _ => {}
                }
            }
            Ok(Nbt::List(10, children))
        }
        Nbt::Compound(fields) => {
            let mut safe = Vec::new();
            let content = ["text", "translate", "keybind"]
                .into_iter()
                .find_map(|key| get_text(&fields, key).map(|value| (key, value.to_owned())));
            if let Some((key, value)) = content {
                safe.push((key.to_owned(), Nbt::Text(value)));
            } else {
                safe.push(("text".to_owned(), Nbt::Text(String::new())));
            }
            if get_text(&fields, "translate").is_some() {
                if let Some(fallback) = get_text(&fields, "fallback") {
                    safe.push(("fallback".to_owned(), Nbt::Text(fallback.to_owned())));
                }
                if let Some((_, Nbt::List(_, list))) = fields.iter().find(|(k, _)| k == "with") {
                    let mut args = Vec::new();
                    for val in list.iter().cloned() {
                        let child = sanitize_nbt(val, depth + 1)?;
                        if matches!(child, Nbt::Text(_) | Nbt::Compound(_)) {
                            args.push(child);
                        }
                    }
                    // Mixed NBT list tags cannot be emitted. Network text 'with'
                    // uses list-of-compounds when any argument is structured.
                    let args = args
                        .into_iter()
                        .map(|entry| match entry {
                            Nbt::Text(s) => Nbt::Compound(vec![("text".to_owned(), Nbt::Text(s))]),
                            other => other,
                        })
                        .collect();
                    safe.push(("with".to_owned(), Nbt::List(10, args)));
                }
            }
            if let Some(color) = get_text(&fields, "color") {
                if safe_color(color) {
                    safe.push(("color".to_owned(), Nbt::Text(color.to_owned())));
                }
            }
            for flag in [
                "bold",
                "italic",
                "underlined",
                "strikethrough",
                "obfuscated",
            ] {
                if let Some((_, Nbt::Byte(value @ (0 | 1)))) =
                    fields.iter().find(|(name, _)| name == flag)
                {
                    safe.push((flag.to_owned(), Nbt::Byte(*value)));
                }
            }
            if let Some((_, Nbt::List(_, list))) = fields.iter().find(|(name, _)| name == "extra") {
                let mut children = Vec::new();
                for value in list.iter().cloned() {
                    let child = sanitize_nbt(value, depth + 1)?;
                    children.push(match child {
                        Nbt::Text(s) => Nbt::Compound(vec![("text".to_owned(), Nbt::Text(s))]),
                        Nbt::Compound(_) => child,
                        _ => continue,
                    });
                }
                if !children.is_empty() {
                    safe.push(("extra".to_owned(), Nbt::List(10, children)));
                }
            }
            Ok(Nbt::Compound(safe))
        }
        _ => Ok(Nbt::Compound(vec![(
            "text".to_owned(),
            Nbt::Text(String::new()),
        )])),
    }
}

/// Preserve safe text/translate/keybind content and formatting while dropping
/// click, hover, selector, score, NBT lookup, and custom font fields.
pub fn sanitize_text_component(raw_nbt: &[u8]) -> Result<Vec<u8>, ProtoError> {
    bound(raw_nbt.len(), MAX_COMPONENT_BYTES)?;
    let mut r = WireReader::new(raw_nbt);
    let tag = r.read_u8()?;
    // A zero root is the network's nullable, empty text component. It is
    // present in Vanilla lore patches and cannot carry executable fields.
    if tag == 0 {
        r.finish()?;
        return Ok(vec![0]);
    }
    if !matches!(tag, 8..=10) {
        return Err(invalid("invalid chat component root tag"));
    }
    let parsed = parse_nbt(&mut r, tag, 0)?;
    r.finish()?;
    let safe = sanitize_nbt(parsed, 0)?;
    let mut w = WireWriter::new();
    w.write_u8(tag_id(&safe));
    put_nbt(&mut w, &safe);
    let result = w.into_vec();
    bound(result.len(), MAX_COMPONENT_BYTES)?;
    Ok(result)
}

/// Construct a safe anonymous-NBT plain-text component.
pub fn plain_text_component(text: &str) -> Result<Vec<u8>, ProtoError> {
    bound(text.len(), MAX_COMPONENT_BYTES.saturating_sub(3))?;
    let mut w = WireWriter::new();
    w.write_u8(8);
    put_nbt_string(&mut w, text);
    Ok(w.into_vec())
}

fn read_text(r: &mut WireReader<'_>) -> Result<Vec<u8>, ProtoError> {
    let remaining = r.rest();
    let mut bounded = WireReader::new(&remaining[..remaining.len().min(MAX_COMPONENT_BYTES)]);
    let raw = bounded.read_anonymous_nbt()?;
    r.take(bounded.position())?;
    sanitize_text_component(raw)
}

fn read_stack(r: &mut WireReader<'_>, depth: usize) -> Result<ItemStack, ProtoError> {
    let start = r.position();
    let count = positive_id(r)?;
    if count > 127 {
        return Err(invalid("invalid nested stack count"));
    }
    if count == 0 {
        return Ok(ItemStack::empty());
    }
    let item_id = positive_id(r)?;
    if item_id as usize >= V767_ITEMS.len() {
        return Err(invalid("invalid nested item id"));
    }
    let add = r.read_count(MAX_LIST_ENTRIES)?;
    let remove = r.read_count(MAX_LIST_ENTRIES)?;
    let mut added = Vec::with_capacity(add);
    let mut seen = HashSet::new();
    for _ in 0..add {
        let type_id = positive_id(r)?;
        if !seen.insert(type_id) {
            return Err(invalid("duplicate nested component"));
        }
        let value = decode_from_reader(type_id, r, depth)?;
        let raw = value.to_raw()?;
        added.push(raw);
        bound(r.position() - start, MAX_STACK_COMPONENTS_BYTES)?;
    }
    let mut removed = Vec::with_capacity(remove);
    for _ in 0..remove {
        let id = positive_id(r)?;
        if id as usize >= V767_COMPONENTS.len() || !seen.insert(id) {
            return Err(invalid("invalid or duplicate removed component"));
        }
        removed.push(id);
    }
    bound(r.position() - start, MAX_STACK_COMPONENTS_BYTES)?;
    Ok(ItemStack {
        item_id,
        count: count as u8,
        added,
        removed,
    })
}

fn write_stack(w: &mut WireWriter, stack: &ItemStack) {
    w.write_varint(i32::from(stack.count));
    if stack.count == 0 {
        return;
    }
    w.write_varint(stack.item_id as i32);
    w.write_varint(stack.added.len() as i32);
    w.write_varint(stack.removed.len() as i32);
    for component in &stack.added {
        w.write_varint(component.type_id as i32);
        w.raw(&component.data);
    }
    for &id in &stack.removed {
        w.write_varint(id as i32);
    }
}

// Check an externally constructed typed value before allocating its wire
// representation. All size estimates are exact for the corresponding writer.
// In particular this bounds strings, collection lengths, and recursion *prior*
// to attempting serialization, even when the caller did not decode from wire.
fn add_size(total: &mut usize, bytes: usize, limit: usize) -> Result<(), ProtoError> {
    *total = total.saturating_add(bytes);
    bound(*total, limit)
}

fn integer_size(n: u32) -> Result<usize, ProtoError> {
    if n > i32::MAX as u32 {
        return Err(invalid("component integer exceeds VarInt range"));
    }
    Ok(varint_length(n))
}

fn string_size(text: &str) -> Result<usize, ProtoError> {
    bound(text.len(), MAX_COMPONENT_BYTES)?;
    bound(text.encode_utf16().count(), MAX_COMPONENT_BYTES)?;
    Ok(varint_length(text.len() as u32) + text.len())
}

fn list_prefix(count: usize, limit: usize) -> Result<usize, ProtoError> {
    bound(count, limit)?;
    Ok(varint_length(count as u32))
}

fn effect_details_size(mut effect: &PotionEffectDetails) -> Result<usize, ProtoError> {
    let mut size = 0;
    for _ in 0..16 {
        add_size(
            &mut size,
            integer_size(effect.amplifier)? + integer_size(effect.duration)? + 4,
            MAX_COMPONENT_BYTES,
        )?;
        if let Some(hidden) = &effect.hidden {
            effect = hidden;
        } else {
            return Ok(size);
        }
    }
    Err(invalid("potion effect nesting exceeds 16"))
}

fn explosion_size(explosion: &FireworkExplosion) -> Result<usize, ProtoError> {
    if explosion.shape > 4 {
        return Err(invalid("invalid firework explosion shape"));
    }
    let mut size = integer_size(u32::from(explosion.shape))? + 2;
    add_size(
        &mut size,
        list_prefix(explosion.colors.len(), MAX_LIST_ENTRIES)? + explosion.colors.len() * 4,
        MAX_COMPONENT_BYTES,
    )?;
    add_size(
        &mut size,
        list_prefix(explosion.fade_colors.len(), MAX_LIST_ENTRIES)?
            + explosion.fade_colors.len() * 4,
        MAX_COMPONENT_BYTES,
    )?;
    Ok(size)
}

fn nested_stack_size(stack: &ItemStack) -> Result<usize, ProtoError> {
    if stack.count > 127 {
        return Err(invalid("invalid nested stack count"));
    }
    let mut size = integer_size(u32::from(stack.count))?;
    if stack.count == 0 {
        if stack.item_id != 0 || !stack.added.is_empty() || !stack.removed.is_empty() {
            return Err(invalid("noncanonical empty nested stack"));
        }
        return Ok(size);
    }
    if stack.item_id as usize >= V767_ITEMS.len() {
        return Err(invalid("invalid nested item id"));
    }
    add_size(
        &mut size,
        integer_size(stack.item_id)?,
        MAX_STACK_COMPONENTS_BYTES,
    )?;
    add_size(
        &mut size,
        list_prefix(stack.added.len(), MAX_LIST_ENTRIES)?,
        MAX_STACK_COMPONENTS_BYTES,
    )?;
    add_size(
        &mut size,
        list_prefix(stack.removed.len(), MAX_LIST_ENTRIES)?,
        MAX_STACK_COMPONENTS_BYTES,
    )?;
    let mut seen = HashSet::new();
    for raw in &stack.added {
        if !seen.insert(raw.type_id) {
            return Err(invalid("duplicate nested component"));
        }
        bound(raw.data.len(), MAX_COMPONENT_BYTES)?;
        let verified = VerifiedComponent::decode(raw)?;
        if matches!(
            verified,
            VerifiedComponent::Container(_)
                | VerifiedComponent::BundleContents(_)
                | VerifiedComponent::ChargedProjectiles(_)
                | VerifiedComponent::Food(_)
        ) {
            return Err(invalid("nested stack-bearing component forbidden"));
        }
        add_size(
            &mut size,
            integer_size(raw.type_id)? + raw.data.len(),
            MAX_STACK_COMPONENTS_BYTES,
        )?;
    }
    for &id in &stack.removed {
        if id as usize >= V767_COMPONENTS.len() || !seen.insert(id) {
            return Err(invalid("invalid or duplicate removed component"));
        }
        add_size(&mut size, integer_size(id)?, MAX_STACK_COMPONENTS_BYTES)?;
    }
    Ok(size)
}

fn identifier_size(value: &str) -> Result<usize, ProtoError> {
    if !valid_identifier(value) {
        return Err(invalid("invalid component identifier"));
    }
    string_size(value)
}

fn id_set_size(set: &IdSet) -> Result<usize, ProtoError> {
    match set {
        IdSet::Tag(name) => Ok(1 + identifier_size(name)?),
        IdSet::Ids(ids) => {
            bound(ids.len(), MAX_LIST_ENTRIES)?;
            // The encoded count is number of IDs + 1.
            let mut size = varint_length(ids.len() as u32 + 1);
            for id in ids {
                add_size(&mut size, integer_size(*id)?, MAX_COMPONENT_BYTES)?;
            }
            Ok(size)
        }
    }
}

fn block_predicate_size(value: &BlockPredicate) -> Result<usize, ProtoError> {
    let mut size = 2; // blocks and properties option booleans
    if let Some(blocks) = &value.blocks {
        add_size(&mut size, id_set_size(blocks)?, MAX_COMPONENT_BYTES)?;
    }
    if let Some(properties) = &value.properties {
        add_size(
            &mut size,
            list_prefix(properties.len(), MAX_LIST_ENTRIES)?,
            MAX_COMPONENT_BYTES,
        )?;
        for property in properties {
            add_size(
                &mut size,
                string_size(&property.name)? + 1,
                MAX_COMPONENT_BYTES,
            )?;
            match &property.value {
                BlockPropertyValue::Exact(exact) => {
                    add_size(&mut size, string_size(exact)?, MAX_COMPONENT_BYTES)?;
                }
                BlockPropertyValue::Range { min, max } => {
                    add_size(
                        &mut size,
                        string_size(min)? + string_size(max)?,
                        MAX_COMPONENT_BYTES,
                    )?;
                }
            }
        }
    }
    add_size(
        &mut size,
        if let Some(nbt) = &value.nbt {
            validate_opaque_nbt(nbt, 10)?
        } else {
            1
        },
        MAX_COMPONENT_BYTES,
    )?;
    Ok(size)
}

fn text_size(text: &[u8]) -> Result<usize, ProtoError> {
    if sanitize_text_component(text)? != text {
        return Err(invalid("noncanonical text component"));
    }
    Ok(text.len())
}

fn sound_size(sound: &SoundEvent) -> Result<usize, ProtoError> {
    let mut size = identifier_size(&sound.name)? + 1;
    if let Some(range) = sound.fixed_range {
        finite_nonnegative(range)?;
        add_size(&mut size, 4, MAX_COMPONENT_BYTES)?;
    }
    Ok(size)
}

fn holder_size<T>(
    holder: &Holder<T>,
    inline_size: impl FnOnce(&T) -> Result<usize, ProtoError>,
) -> Result<usize, ProtoError> {
    match holder {
        Holder::Registry(id) if *id > 0 => integer_size(*id),
        Holder::Registry(_) => Err(invalid("zero registry holder")),
        Holder::Inline(value) => Ok(1 + inline_size(value)?),
    }
}

fn trim_material_size(material: &TrimMaterial) -> Result<usize, ProtoError> {
    let mut size = identifier_size(&material.asset_name)? + integer_size(material.ingredient)?;
    add_size(
        &mut size,
        list_prefix(material.overrides.len(), MAX_LIST_ENTRIES)?,
        MAX_COMPONENT_BYTES,
    )?;
    for (name, asset) in &material.overrides {
        add_size(
            &mut size,
            string_size(name)? + identifier_size(asset)?,
            MAX_COMPONENT_BYTES,
        )?;
    }
    add_size(
        &mut size,
        text_size(&material.description)?,
        MAX_COMPONENT_BYTES,
    )?;
    Ok(size)
}

fn trim_pattern_size(pattern: &TrimPattern) -> Result<usize, ProtoError> {
    Ok(identifier_size(&pattern.asset_id)?
        + integer_size(pattern.template)?
        + text_size(&pattern.description)?
        + 1)
}

fn jukebox_song_size(song: &JukeboxSong) -> Result<usize, ProtoError> {
    finite_nonnegative(song.duration)?;
    if song.comparator_output > 15 {
        return Err(invalid("jukebox comparator output exceeds 15"));
    }
    Ok(holder_size(&song.sound, sound_size)?
        + text_size(&song.description)?
        + 4
        + integer_size(song.comparator_output.into())?)
}

fn list_of_stacks_size(stacks: &[ItemStack], limit: usize) -> Result<usize, ProtoError> {
    let mut size = list_prefix(stacks.len(), limit)?;
    for stack in stacks {
        add_size(&mut size, nested_stack_size(stack)?, MAX_COMPONENT_BYTES)?;
    }
    Ok(size)
}

fn book_page_count(pages: usize) -> Result<usize, ProtoError> {
    bound(pages, MAX_BOOK_PAGES)?;
    list_prefix(pages, MAX_LIST_ENTRIES)
}

fn book_string_size(value: &str, limit: usize) -> Result<usize, ProtoError> {
    validate_book_string(value, limit)?;
    string_size(value)
}

fn verified_size(value: &VerifiedComponent) -> Result<usize, ProtoError> {
    let mut size: usize;
    match value {
        VerifiedComponent::CustomData(data)
        | VerifiedComponent::IntangibleProjectile(data)
        | VerifiedComponent::MapDecorations(data)
        | VerifiedComponent::DebugStickState(data)
        | VerifiedComponent::EntityData(data)
        | VerifiedComponent::BucketEntityData(data)
        | VerifiedComponent::BlockEntityData(data)
        | VerifiedComponent::Recipes(data)
        | VerifiedComponent::Lock(data)
        | VerifiedComponent::ContainerLoot(data) => size = validate_opaque_nbt(data, 10)?,
        VerifiedComponent::MaxStackSize(n) => {
            if !(1..=99).contains(n) {
                return Err(invalid("stack size outside 1..=99"));
            }
            size = integer_size(*n)?;
        }
        VerifiedComponent::MaxDamage(n)
        | VerifiedComponent::CustomModelData(n)
        | VerifiedComponent::MapId(n) => size = integer_size(*n)?,
        VerifiedComponent::CustomName(text) | VerifiedComponent::ItemName(text) => {
            size = text_size(text)?;
        }
        VerifiedComponent::Lore(lines) => {
            size = list_prefix(lines.len(), MAX_LIST_ENTRIES)?;
            for line in lines {
                add_size(&mut size, text_size(line)?, MAX_COMPONENT_BYTES)?;
            }
        }
        VerifiedComponent::CanPlaceOn { predicates, .. }
        | VerifiedComponent::CanBreak { predicates, .. } => {
            size = list_prefix(predicates.len(), MAX_LIST_ENTRIES)? + 1;
            for predicate in predicates {
                add_size(
                    &mut size,
                    block_predicate_size(predicate)?,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::AttributeModifiers { entries, .. } => {
            size = list_prefix(entries.len(), MAX_LIST_ENTRIES)? + 1;
            for modifier in entries {
                if !modifier.amount.is_finite() || modifier.operation > 2 || modifier.slot > 9 {
                    return Err(invalid("invalid attribute modifier"));
                }
                add_size(
                    &mut size,
                    integer_size(modifier.attribute)?
                        + string_size(&modifier.id)?
                        + 8
                        + integer_size(modifier.operation.into())?
                        + integer_size(modifier.slot.into())?,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::Rarity(n) => {
            if *n > 3 {
                return Err(invalid("invalid rarity"));
            }
            size = integer_size(u32::from(*n))?;
        }
        VerifiedComponent::BaseColor(n) => {
            if *n > 15 {
                return Err(invalid("invalid dye color"));
            }
            size = integer_size(u32::from(*n))?;
        }
        VerifiedComponent::Enchantments { entries, .. }
        | VerifiedComponent::StoredEnchantments { entries, .. } => {
            size = list_prefix(entries.len(), MAX_LIST_ENTRIES)? + 1;
            for entry in entries {
                if !(1..=255).contains(&entry.level) {
                    return Err(invalid("enchantment level outside 1..=255"));
                }
                add_size(
                    &mut size,
                    integer_size(entry.id)? + integer_size(entry.level)?,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::EnchantmentGlintOverride(_) | VerifiedComponent::Unbreakable(_) => {
            size = 1
        }
        VerifiedComponent::HideAdditionalTooltip
        | VerifiedComponent::HideTooltip
        | VerifiedComponent::CreativeSlotLock
        | VerifiedComponent::FireResistant => size = 0,
        VerifiedComponent::Damage(n) | VerifiedComponent::RepairCost(n) => size = integer_size(*n)?,
        VerifiedComponent::MapColor(_) => size = 4,
        VerifiedComponent::MapPostProcessing(value) => {
            if *value > 1 {
                return Err(invalid("invalid map post-processing mode"));
            }
            size = integer_size((*value).into())?;
        }
        VerifiedComponent::ChargedProjectiles(stacks) => {
            size = list_of_stacks_size(stacks, MAX_CHARGED_PROJECTILES)?;
        }
        VerifiedComponent::BundleContents(stacks) => {
            size = list_of_stacks_size(stacks, MAX_LIST_ENTRIES)?;
        }
        VerifiedComponent::OminousBottleAmplifier(n) => {
            if *n > 4 {
                return Err(invalid("invalid ominous bottle amplifier"));
            }
            size = integer_size(*n)?;
        }
        VerifiedComponent::DyedColor { .. } => size = 5,
        VerifiedComponent::Food(food) => {
            finite_nonnegative(food.saturation)?;
            finite_nonnegative(food.seconds_to_eat)?;
            size =
                integer_size(food.nutrition)? + 4 + 1 + 4 + nested_stack_size(&food.converts_to)?;
            add_size(
                &mut size,
                list_prefix(food.effects.len(), MAX_LIST_ENTRIES)?,
                MAX_COMPONENT_BYTES,
            )?;
            for effect in &food.effects {
                probability(effect.probability)?;
                add_size(
                    &mut size,
                    integer_size(effect.effect)? + 4,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::Tool(tool) => {
            finite_nonnegative(tool.default_mining_speed)?;
            size = list_prefix(tool.rules.len(), MAX_LIST_ENTRIES)?
                + 4
                + integer_size(tool.damage_per_block)?;
            for rule in &tool.rules {
                let mut bytes = id_set_size(&rule.blocks)? + 2;
                if let Some(speed) = rule.speed {
                    finite_nonnegative(speed)?;
                    bytes += 4;
                }
                add_size(&mut size, bytes, MAX_COMPONENT_BYTES)?;
            }
        }
        VerifiedComponent::PotionContents(potion) => {
            size = 3; // optional potion, color and name flags
            if let Some(id) = potion.potion {
                add_size(&mut size, integer_size(id)?, MAX_COMPONENT_BYTES)?;
            }
            if potion.custom_color.is_some() {
                add_size(&mut size, 4, MAX_COMPONENT_BYTES)?;
            }
            add_size(
                &mut size,
                list_prefix(potion.effects.len(), MAX_LIST_ENTRIES)?,
                MAX_COMPONENT_BYTES,
            )?;
            for effect in &potion.effects {
                let mut bytes = integer_size(effect.id)?
                    + integer_size(effect.amplifier)?
                    + integer_size(effect.duration)?
                    + 4;
                if let Some(hidden) = &effect.hidden {
                    bytes += effect_details_size(hidden)?;
                }
                add_size(&mut size, bytes, MAX_COMPONENT_BYTES)?;
            }
            if let Some(name) = &potion.custom_name {
                add_size(&mut size, string_size(name)?, MAX_COMPONENT_BYTES)?;
            }
        }
        VerifiedComponent::SuspiciousStewEffects(effects) => {
            size = list_prefix(effects.len(), MAX_LIST_ENTRIES)?;
            for &(id, duration) in effects {
                add_size(
                    &mut size,
                    integer_size(id)? + integer_size(duration)?,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::Fireworks {
            flight_duration,
            explosions,
        } => {
            if *flight_duration > 255 {
                return Err(invalid("invalid firework flight duration"));
            }
            size =
                integer_size(*flight_duration)? + list_prefix(explosions.len(), MAX_LIST_ENTRIES)?;
            for explosion in explosions {
                add_size(&mut size, explosion_size(explosion)?, MAX_COMPONENT_BYTES)?;
            }
        }
        VerifiedComponent::FireworkExplosion(explosion) => size = explosion_size(explosion)?,
        VerifiedComponent::BannerPatterns(patterns) => {
            size = list_prefix(patterns.len(), MAX_LIST_ENTRIES)?;
            for pattern in patterns {
                if pattern.color > 15 {
                    return Err(invalid("invalid banner dye"));
                }
                match &pattern.pattern {
                    BannerPatternKind::Registry(id) => {
                        if *id == 0 {
                            return Err(invalid("invalid banner pattern registry id"));
                        }
                        add_size(&mut size, integer_size(*id)?, MAX_COMPONENT_BYTES)?;
                    }
                    BannerPatternKind::Inline {
                        asset_id,
                        translation_key,
                    } => {
                        add_size(
                            &mut size,
                            1 + string_size(asset_id)? + string_size(translation_key)?,
                            MAX_COMPONENT_BYTES,
                        )?;
                    }
                }
                add_size(
                    &mut size,
                    integer_size(u32::from(pattern.color))?,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::WritableBookContent(pages) => {
            size = book_page_count(pages.len())?;
            for page in pages {
                add_size(
                    &mut size,
                    book_string_size(&page.text, 1024)? + 1,
                    MAX_COMPONENT_BYTES,
                )?;
                if let Some(filtered) = &page.filtered {
                    add_size(
                        &mut size,
                        book_string_size(filtered, 1024)?,
                        MAX_COMPONENT_BYTES,
                    )?;
                }
            }
        }
        VerifiedComponent::WrittenBookContent(book) => {
            if book.generation > 3 {
                return Err(invalid("invalid written book generation"));
            }
            size = book_string_size(&book.title, 128)?
                + 1
                + book_string_size(&book.author, 128)?
                + integer_size(book.generation.into())?
                + book_page_count(book.pages.len())?
                + 1;
            if let Some(filtered) = &book.filtered_title {
                add_size(
                    &mut size,
                    book_string_size(filtered, 128)?,
                    MAX_COMPONENT_BYTES,
                )?;
            }
            for page in &book.pages {
                add_size(&mut size, text_size(&page.text)? + 1, MAX_COMPONENT_BYTES)?;
                if let Some(filtered) = &page.filtered {
                    add_size(&mut size, text_size(filtered)?, MAX_COMPONENT_BYTES)?;
                }
            }
        }
        VerifiedComponent::Trim(trim) => {
            size = holder_size(&trim.material, trim_material_size)?
                + holder_size(&trim.pattern, trim_pattern_size)?
                + 1;
        }
        VerifiedComponent::JukeboxPlayable(playable) => {
            size = 2;
            add_size(
                &mut size,
                match &playable.song {
                    JukeboxSongRef::Name(name) => identifier_size(name)?,
                    JukeboxSongRef::Holder(song) => holder_size(song, jukebox_song_size)?,
                },
                MAX_COMPONENT_BYTES,
            )?;
        }
        VerifiedComponent::LodestoneTracker(tracker) => {
            size = 2;
            if let Some((dimension, pos)) = &tracker.position {
                if BlockPos::unpack(pos.pack()) != *pos {
                    return Err(invalid("lodestone position outside packed bounds"));
                }
                add_size(
                    &mut size,
                    identifier_size(dimension)? + 8,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::NoteBlockSound(name) => size = identifier_size(name)?,
        VerifiedComponent::PotDecorations(ids) => {
            size = list_prefix(ids.len(), 4)?;
            for id in ids {
                add_size(&mut size, integer_size(*id)?, MAX_COMPONENT_BYTES)?;
            }
        }
        VerifiedComponent::Bees(bees) => {
            size = list_prefix(bees.len(), MAX_LIST_ENTRIES)?;
            for bee in bees {
                add_size(
                    &mut size,
                    validate_opaque_nbt(&bee.nbt, 10)?
                        + integer_size(bee.ticks_in_hive)?
                        + integer_size(bee.min_ticks_in_hive)?,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::Instrument(instrument) => match instrument {
            Instrument::Registry(id) => {
                if *id == 0 {
                    return Err(invalid("invalid instrument registry id"));
                }
                size = integer_size(*id)?;
            }
            Instrument::Inline {
                sound,
                fixed_range,
                use_duration,
                range,
                description,
            } => {
                if fixed_range.is_some_and(|v| !v.is_finite() || v < 0.0)
                    || !use_duration.is_finite()
                    || *use_duration < 0.0
                    || !range.is_finite()
                    || *range < 0.0
                {
                    return Err(invalid("invalid instrument duration or range"));
                }
                size = 2 + string_size(sound)? + 1 + 8 + description.len();
                if fixed_range.is_some() {
                    add_size(&mut size, 4, MAX_COMPONENT_BYTES)?;
                }
            }
        },
        VerifiedComponent::BlockState(properties) => {
            size = list_prefix(properties.len(), MAX_LIST_ENTRIES)?;
            for (key, val) in properties {
                add_size(
                    &mut size,
                    string_size(key)? + string_size(val)?,
                    MAX_COMPONENT_BYTES,
                )?;
            }
        }
        VerifiedComponent::Profile(profile) => {
            size = 2;
            if let Some(name) = &profile.name {
                add_size(&mut size, string_size(name)?, MAX_COMPONENT_BYTES)?;
            }
            if profile.uuid.is_some() {
                add_size(&mut size, 16, MAX_COMPONENT_BYTES)?;
            }
            add_size(
                &mut size,
                list_prefix(profile.properties.len(), MAX_LIST_ENTRIES)?,
                MAX_COMPONENT_BYTES,
            )?;
            for prop in &profile.properties {
                add_size(
                    &mut size,
                    string_size(&prop.name)? + string_size(&prop.value)? + 1,
                    MAX_COMPONENT_BYTES,
                )?;
                if let Some(sig) = &prop.signature {
                    add_size(&mut size, string_size(sig)?, MAX_COMPONENT_BYTES)?;
                }
            }
        }
        VerifiedComponent::Container(stacks) => {
            size = list_prefix(stacks.len(), MAX_CONTAINER_SLOTS)?;
            for stack in stacks {
                add_size(&mut size, nested_stack_size(stack)?, MAX_COMPONENT_BYTES)?;
            }
        }
    }
    bound(size, MAX_COMPONENT_BYTES)?;
    Ok(size)
}

impl VerifiedComponent {
    pub fn decode(raw: &RawComponent) -> Result<Self, ProtoError> {
        bound(raw.data.len(), MAX_COMPONENT_BYTES)?;
        let mut r = WireReader::new(&raw.data);
        let component = decode_from_reader(raw.type_id, &mut r, 0)?;
        r.finish()?;
        Ok(component)
    }

    fn registry_name(&self) -> &'static str {
        match self {
            Self::CustomData(_) => "minecraft:custom_data",
            Self::MaxStackSize(_) => "minecraft:max_stack_size",
            Self::MaxDamage(_) => "minecraft:max_damage",
            Self::CanPlaceOn { .. } => "minecraft:can_place_on",
            Self::CanBreak { .. } => "minecraft:can_break",
            Self::AttributeModifiers { .. } => "minecraft:attribute_modifiers",
            Self::CustomModelData(_) => "minecraft:custom_model_data",
            Self::CreativeSlotLock => "minecraft:creative_slot_lock",
            Self::IntangibleProjectile(_) => "minecraft:intangible_projectile",
            Self::Food(_) => "minecraft:food",
            Self::FireResistant => "minecraft:fire_resistant",
            Self::Tool(_) => "minecraft:tool",
            Self::MapColor(_) => "minecraft:map_color",
            Self::MapId(_) => "minecraft:map_id",
            Self::MapDecorations(_) => "minecraft:map_decorations",
            Self::MapPostProcessing(_) => "minecraft:map_post_processing",
            Self::ChargedProjectiles(_) => "minecraft:charged_projectiles",
            Self::BundleContents(_) => "minecraft:bundle_contents",
            Self::WritableBookContent(_) => "minecraft:writable_book_content",
            Self::WrittenBookContent(_) => "minecraft:written_book_content",
            Self::Trim(_) => "minecraft:trim",
            Self::DebugStickState(_) => "minecraft:debug_stick_state",
            Self::EntityData(_) => "minecraft:entity_data",
            Self::BucketEntityData(_) => "minecraft:bucket_entity_data",
            Self::BlockEntityData(_) => "minecraft:block_entity_data",
            Self::JukeboxPlayable(_) => "minecraft:jukebox_playable",
            Self::Recipes(_) => "minecraft:recipes",
            Self::LodestoneTracker(_) => "minecraft:lodestone_tracker",
            Self::NoteBlockSound(_) => "minecraft:note_block_sound",
            Self::PotDecorations(_) => "minecraft:pot_decorations",
            Self::Bees(_) => "minecraft:bees",
            Self::Lock(_) => "minecraft:lock",
            Self::ContainerLoot(_) => "minecraft:container_loot",
            Self::CustomName(_) => "minecraft:custom_name",
            Self::ItemName(_) => "minecraft:item_name",
            Self::Lore(_) => "minecraft:lore",
            Self::Rarity(_) => "minecraft:rarity",
            Self::Enchantments { .. } => "minecraft:enchantments",
            Self::StoredEnchantments { .. } => "minecraft:stored_enchantments",
            Self::EnchantmentGlintOverride(_) => "minecraft:enchantment_glint_override",
            Self::Unbreakable(_) => "minecraft:unbreakable",
            Self::Damage(_) => "minecraft:damage",
            Self::RepairCost(_) => "minecraft:repair_cost",
            Self::DyedColor { .. } => "minecraft:dyed_color",
            Self::BaseColor(_) => "minecraft:base_color",
            Self::PotionContents(_) => "minecraft:potion_contents",
            Self::SuspiciousStewEffects(_) => "minecraft:suspicious_stew_effects",
            Self::OminousBottleAmplifier(_) => "minecraft:ominous_bottle_amplifier",
            Self::Fireworks { .. } => "minecraft:fireworks",
            Self::FireworkExplosion(_) => "minecraft:firework_explosion",
            Self::BannerPatterns(_) => "minecraft:banner_patterns",
            Self::HideAdditionalTooltip => "minecraft:hide_additional_tooltip",
            Self::HideTooltip => "minecraft:hide_tooltip",
            Self::Instrument(_) => "minecraft:instrument",
            Self::BlockState(_) => "minecraft:block_state",
            Self::Profile(_) => "minecraft:profile",
            Self::Container(_) => "minecraft:container",
        }
    }

    /// Encode the canonical Vanilla 767 payload and verify its shape and bounds.
    pub fn to_raw(&self) -> Result<RawComponent, ProtoError> {
        verified_size(self)?;
        let type_id = V767_COMPONENTS
            .iter()
            .position(|name| *name == self.registry_name())
            .ok_or_else(|| invalid("component registry entry absent"))?
            as u32;
        let mut w = WireWriter::new();
        match self {
            Self::CustomData(nbt)
            | Self::IntangibleProjectile(nbt)
            | Self::MapDecorations(nbt)
            | Self::DebugStickState(nbt)
            | Self::EntityData(nbt)
            | Self::BucketEntityData(nbt)
            | Self::BlockEntityData(nbt)
            | Self::Recipes(nbt)
            | Self::Lock(nbt)
            | Self::ContainerLoot(nbt) => w.raw(nbt),
            Self::MaxStackSize(value)
            | Self::MaxDamage(value)
            | Self::CustomModelData(value)
            | Self::MapId(value) => w.write_varint(*value as i32),
            Self::CustomName(raw) | Self::ItemName(raw) => w.raw(raw),
            Self::Lore(lines) => put_list(&mut w, lines, |w, line| w.raw(line)),
            Self::CanPlaceOn {
                predicates,
                show_in_tooltip,
            }
            | Self::CanBreak {
                predicates,
                show_in_tooltip,
            } => {
                put_list(&mut w, predicates, write_block_predicate);
                w.write_bool(*show_in_tooltip);
            }
            Self::AttributeModifiers {
                entries,
                show_in_tooltip,
            } => {
                put_list(&mut w, entries, |w, modifier| {
                    w.write_varint(modifier.attribute as i32);
                    w.write_string(&modifier.id);
                    w.write_f64(modifier.amount);
                    w.write_varint(i32::from(modifier.operation));
                    w.write_varint(i32::from(modifier.slot));
                });
                w.write_bool(*show_in_tooltip);
            }
            Self::Rarity(rarity) | Self::BaseColor(rarity) => w.write_varint(i32::from(*rarity)),
            Self::Enchantments {
                entries,
                show_in_tooltip,
            }
            | Self::StoredEnchantments {
                entries,
                show_in_tooltip,
            } => write_enchantments(&mut w, entries, *show_in_tooltip),
            Self::EnchantmentGlintOverride(value) | Self::Unbreakable(value) => {
                w.write_bool(*value)
            }
            Self::CreativeSlotLock | Self::FireResistant => {}
            Self::Damage(value) | Self::RepairCost(value) | Self::OminousBottleAmplifier(value) => {
                w.write_varint(*value as i32)
            }
            Self::MapColor(color) => w.write_u32(*color),
            Self::MapPostProcessing(mode) => w.write_varint(i32::from(*mode)),
            Self::ChargedProjectiles(stacks) | Self::BundleContents(stacks) => {
                put_list(&mut w, stacks, write_stack);
            }
            Self::DyedColor {
                rgb,
                show_in_tooltip,
            } => {
                w.write_u32(*rgb);
                w.write_bool(*show_in_tooltip);
            }
            Self::Food(food) => {
                w.write_varint(food.nutrition as i32);
                w.write_f32(food.saturation);
                w.write_bool(food.can_always_eat);
                w.write_f32(food.seconds_to_eat);
                write_stack(&mut w, &food.converts_to);
                put_list(&mut w, &food.effects, |w, effect| {
                    w.write_varint(effect.effect as i32);
                    w.write_f32(effect.probability);
                });
            }
            Self::Tool(tool) => {
                put_list(&mut w, &tool.rules, |w, rule| {
                    write_id_set(w, &rule.blocks);
                    w.write_bool(rule.speed.is_some());
                    if let Some(speed) = rule.speed {
                        w.write_f32(speed);
                    }
                    w.write_bool(rule.correct_drop_for_blocks.is_some());
                    if let Some(yes) = rule.correct_drop_for_blocks {
                        w.write_bool(yes);
                    }
                });
                w.write_f32(tool.default_mining_speed);
                w.write_varint(tool.damage_per_block as i32);
            }
            Self::PotionContents(potion) => {
                w.write_bool(potion.potion.is_some());
                if let Some(id) = potion.potion {
                    w.write_varint(id as i32);
                }
                w.write_bool(potion.custom_color.is_some());
                if let Some(color) = potion.custom_color {
                    w.write_u32(color);
                }
                put_list(&mut w, &potion.effects, |w, e| {
                    w.write_varint(e.id as i32);
                    write_effect_details(
                        w,
                        &PotionEffectDetails {
                            amplifier: e.amplifier,
                            duration: e.duration,
                            ambient: e.ambient,
                            show_particles: e.show_particles,
                            show_icon: e.show_icon,
                            hidden: e.hidden.clone(),
                        },
                    );
                });
                w.write_bool(potion.custom_name.is_some());
                if let Some(name) = &potion.custom_name {
                    w.write_string(name);
                }
            }
            Self::SuspiciousStewEffects(effects) => {
                put_list(&mut w, effects, |w, (id, duration)| {
                    w.write_varint(*id as i32);
                    w.write_varint(*duration as i32);
                })
            }
            Self::Fireworks {
                flight_duration,
                explosions,
            } => {
                w.write_varint(*flight_duration as i32);
                put_list(&mut w, explosions, write_explosion);
            }
            Self::FireworkExplosion(explosion) => write_explosion(&mut w, explosion),
            Self::BannerPatterns(patterns) => put_list(&mut w, patterns, |w, p| {
                match &p.pattern {
                    BannerPatternKind::Registry(id) => w.write_varint(*id as i32),
                    BannerPatternKind::Inline {
                        asset_id,
                        translation_key,
                    } => {
                        w.write_varint(0);
                        w.write_string(asset_id);
                        w.write_string(translation_key);
                    }
                }
                w.write_varint(i32::from(p.color));
            }),
            Self::HideAdditionalTooltip | Self::HideTooltip => {}
            Self::WritableBookContent(pages) => {
                put_list(&mut w, pages, |w, page| {
                    w.write_string(&page.text);
                    w.write_bool(page.filtered.is_some());
                    if let Some(filtered) = &page.filtered {
                        w.write_string(filtered);
                    }
                });
            }
            Self::WrittenBookContent(book) => {
                w.write_string(&book.title);
                w.write_bool(book.filtered_title.is_some());
                if let Some(filtered) = &book.filtered_title {
                    w.write_string(filtered);
                }
                w.write_string(&book.author);
                w.write_varint(i32::from(book.generation));
                put_list(&mut w, &book.pages, |w, page| {
                    w.raw(&page.text);
                    w.write_bool(page.filtered.is_some());
                    if let Some(filtered) = &page.filtered {
                        w.raw(filtered);
                    }
                });
                w.write_bool(book.resolved);
            }
            Self::Trim(trim) => {
                write_holder(&mut w, &trim.material, write_trim_material);
                write_holder(&mut w, &trim.pattern, write_trim_pattern);
                w.write_bool(trim.show_in_tooltip);
            }
            Self::JukeboxPlayable(playable) => {
                match &playable.song {
                    JukeboxSongRef::Name(name) => {
                        w.write_bool(false);
                        w.write_string(name);
                    }
                    JukeboxSongRef::Holder(holder) => {
                        w.write_bool(true);
                        write_holder(&mut w, holder, write_jukebox_song);
                    }
                }
                w.write_bool(playable.show_in_tooltip);
            }
            Self::LodestoneTracker(tracker) => {
                w.write_bool(tracker.position.is_some());
                if let Some((dimension, pos)) = &tracker.position {
                    w.write_string(dimension);
                    w.write_position(*pos);
                }
                w.write_bool(tracker.tracked);
            }
            Self::NoteBlockSound(name) => w.write_string(name),
            Self::PotDecorations(ids) => put_list(&mut w, ids, |w, id| w.write_varint(*id as i32)),
            Self::Bees(bees) => put_list(&mut w, bees, |w, bee| {
                w.raw(&bee.nbt);
                w.write_varint(bee.ticks_in_hive as i32);
                w.write_varint(bee.min_ticks_in_hive as i32);
            }),
            Self::Instrument(instrument) => write_instrument(&mut w, instrument),
            Self::BlockState(properties) => put_list(&mut w, properties, |w, (key, val)| {
                w.write_string(key);
                w.write_string(val);
            }),
            Self::Profile(profile) => {
                w.write_bool(profile.name.is_some());
                if let Some(name) = &profile.name {
                    w.write_string(name);
                }
                w.write_bool(profile.uuid.is_some());
                if let Some(uuid) = &profile.uuid {
                    w.write_uuid(uuid);
                }
                put_list(&mut w, &profile.properties, |w, prop| {
                    w.write_string(&prop.name);
                    w.write_string(&prop.value);
                    w.write_bool(prop.signature.is_some());
                    if let Some(sig) = &prop.signature {
                        w.write_string(sig);
                    }
                });
            }
            Self::Container(stacks) => put_list(&mut w, stacks, write_stack),
        }
        let raw = RawComponent {
            type_id,
            data: w.into_vec(),
        };
        bound(raw.data.len(), MAX_COMPONENT_BYTES)?;
        let decoded = Self::decode(&raw)?;
        if &decoded != self {
            return Err(invalid("component is not canonical"));
        }
        Ok(raw)
    }
}

/// Consume one of the supported protocol-767 components from a larger slot
/// stream, retaining exact framing while validating its typed shape.
pub(crate) fn read_component_from_wire(
    type_id: u32,
    reader: &mut WireReader<'_>,
) -> Result<RawComponent, ProtoError> {
    let start = reader.position();
    let value = decode_from_reader(type_id, reader, 0)?;
    bound(reader.position() - start, MAX_COMPONENT_BYTES)?;
    value.to_raw()
}

pub(crate) fn is_verified_component_type(type_id: u32) -> bool {
    V767_COMPONENTS.get(type_id as usize).is_some()
}

fn decode_from_reader(
    type_id: u32,
    r: &mut WireReader<'_>,
    depth: usize,
) -> Result<VerifiedComponent, ProtoError> {
    let name = V767_COMPONENTS
        .get(type_id as usize)
        .ok_or_else(|| invalid("unknown data component type"))?;
    Ok(match *name {
        "minecraft:custom_data" => VerifiedComponent::CustomData(read_opaque_nbt(r, 10)?),
        "minecraft:max_stack_size" => {
            let count = positive_id(r)?;
            if !(1..=99).contains(&count) {
                return Err(invalid("stack size outside 1..=99"));
            }
            VerifiedComponent::MaxStackSize(count)
        }
        "minecraft:max_damage" => VerifiedComponent::MaxDamage(positive_id(r)?),
        "minecraft:can_place_on" => VerifiedComponent::CanPlaceOn {
            predicates: read_list(r, read_block_predicate)?,
            show_in_tooltip: r.read_bool()?,
        },
        "minecraft:can_break" => VerifiedComponent::CanBreak {
            predicates: read_list(r, read_block_predicate)?,
            show_in_tooltip: r.read_bool()?,
        },
        "minecraft:attribute_modifiers" => VerifiedComponent::AttributeModifiers {
            entries: read_list(r, |r| {
                let attribute = positive_id(r)?;
                let id = read_string(r)?;
                let amount = r.read_f64()?;
                if !amount.is_finite() {
                    return Err(invalid("invalid attribute modifier amount"));
                }
                let operation = read_enum(r, 2)?;
                let slot = read_enum(r, 9)?;
                Ok(AttributeModifier {
                    attribute,
                    id,
                    amount,
                    operation,
                    slot,
                })
            })?,
            show_in_tooltip: r.read_bool()?,
        },
        "minecraft:custom_model_data" => VerifiedComponent::CustomModelData(positive_id(r)?),
        "minecraft:creative_slot_lock" => VerifiedComponent::CreativeSlotLock,
        "minecraft:intangible_projectile" => {
            VerifiedComponent::IntangibleProjectile(read_opaque_nbt(r, 10)?)
        }
        "minecraft:food" => {
            if depth >= MAX_CONTAINER_DEPTH {
                return Err(invalid("nested food component forbidden"));
            }
            let nutrition = positive_id(r)?;
            let saturation = finite_nonnegative(r.read_f32()?)?;
            let can_always_eat = r.read_bool()?;
            let seconds_to_eat = finite_nonnegative(r.read_f32()?)?;
            let converts_to = read_stack(r, depth + 1)?;
            let effects = read_list(r, |r| {
                Ok(FoodEffect {
                    effect: positive_id(r)?,
                    probability: probability(r.read_f32()?)?,
                })
            })?;
            VerifiedComponent::Food(Food {
                nutrition,
                saturation,
                can_always_eat,
                seconds_to_eat,
                converts_to,
                effects,
            })
        }
        "minecraft:fire_resistant" => VerifiedComponent::FireResistant,
        "minecraft:tool" => VerifiedComponent::Tool(Tool {
            rules: read_list(r, |r| {
                let blocks = read_id_set(r)?;
                let speed = if r.read_bool()? {
                    Some(finite_nonnegative(r.read_f32()?)?)
                } else {
                    None
                };
                let correct_drop_for_blocks = if r.read_bool()? {
                    Some(r.read_bool()?)
                } else {
                    None
                };
                Ok(ToolRule {
                    blocks,
                    speed,
                    correct_drop_for_blocks,
                })
            })?,
            default_mining_speed: finite_nonnegative(r.read_f32()?)?,
            damage_per_block: positive_id(r)?,
        }),
        "minecraft:map_color" => VerifiedComponent::MapColor(r.read_u32()?),
        "minecraft:map_id" => VerifiedComponent::MapId(positive_id(r)?),
        "minecraft:map_decorations" => VerifiedComponent::MapDecorations(read_opaque_nbt(r, 10)?),
        "minecraft:map_post_processing" => VerifiedComponent::MapPostProcessing(read_enum(r, 1)?),
        "minecraft:charged_projectiles" => {
            if depth >= MAX_CONTAINER_DEPTH {
                return Err(invalid("nested charged projectiles forbidden"));
            }
            let count = r.read_count(MAX_CHARGED_PROJECTILES)?;
            let mut stacks = Vec::with_capacity(count);
            for _ in 0..count {
                stacks.push(read_stack(r, depth + 1)?);
            }
            VerifiedComponent::ChargedProjectiles(stacks)
        }
        "minecraft:bundle_contents" => {
            if depth >= MAX_CONTAINER_DEPTH {
                return Err(invalid("nested bundle contents forbidden"));
            }
            let count = r.read_count(MAX_LIST_ENTRIES)?;
            let mut stacks = Vec::with_capacity(count);
            for _ in 0..count {
                stacks.push(read_stack(r, depth + 1)?);
            }
            VerifiedComponent::BundleContents(stacks)
        }
        "minecraft:writable_book_content" => {
            let count = r.read_count(MAX_LIST_ENTRIES)?;
            bound(count, MAX_BOOK_PAGES)?;
            let mut pages = Vec::with_capacity(count);
            for _ in 0..count {
                let text = read_book_string(r, 1024)?;
                let filtered = if r.read_bool()? {
                    Some(read_book_string(r, 1024)?)
                } else {
                    None
                };
                pages.push(BookPage { text, filtered });
            }
            VerifiedComponent::WritableBookContent(pages)
        }
        "minecraft:written_book_content" => {
            let title = read_book_string(r, 128)?;
            let filtered_title = if r.read_bool()? {
                Some(read_book_string(r, 128)?)
            } else {
                None
            };
            let author = read_book_string(r, 128)?;
            let generation = read_enum(r, 3)?;
            let count = r.read_count(MAX_LIST_ENTRIES)?;
            bound(count, MAX_BOOK_PAGES)?;
            let mut pages = Vec::with_capacity(count);
            for _ in 0..count {
                let text = read_text(r)?;
                let filtered = if r.read_bool()? {
                    Some(read_text(r)?)
                } else {
                    None
                };
                pages.push(WrittenPage { text, filtered });
            }
            VerifiedComponent::WrittenBookContent(WrittenBook {
                title,
                filtered_title,
                author,
                generation,
                pages,
                resolved: r.read_bool()?,
            })
        }
        "minecraft:trim" => VerifiedComponent::Trim(Trim {
            material: read_holder(r, read_trim_material)?,
            pattern: read_holder(r, read_trim_pattern)?,
            show_in_tooltip: r.read_bool()?,
        }),
        "minecraft:debug_stick_state" => {
            VerifiedComponent::DebugStickState(read_opaque_nbt(r, 10)?)
        }
        "minecraft:entity_data" => VerifiedComponent::EntityData(read_opaque_nbt(r, 10)?),
        "minecraft:bucket_entity_data" => {
            VerifiedComponent::BucketEntityData(read_opaque_nbt(r, 10)?)
        }
        "minecraft:block_entity_data" => {
            VerifiedComponent::BlockEntityData(read_opaque_nbt(r, 10)?)
        }
        "minecraft:jukebox_playable" => {
            let song = if r.read_bool()? {
                JukeboxSongRef::Holder(read_holder(r, read_jukebox_song)?)
            } else {
                JukeboxSongRef::Name(read_identifier(r)?)
            };
            VerifiedComponent::JukeboxPlayable(JukeboxPlayable {
                song,
                show_in_tooltip: r.read_bool()?,
            })
        }
        "minecraft:recipes" => VerifiedComponent::Recipes(read_opaque_nbt(r, 10)?),
        "minecraft:lodestone_tracker" => {
            let position = if r.read_bool()? {
                Some((read_identifier(r)?, r.read_position()?))
            } else {
                None
            };
            VerifiedComponent::LodestoneTracker(LodestoneTracker {
                position,
                tracked: r.read_bool()?,
            })
        }
        "minecraft:note_block_sound" => VerifiedComponent::NoteBlockSound(read_identifier(r)?),
        "minecraft:pot_decorations" => {
            let count = r.read_count(4)?;
            let ids = (0..count)
                .map(|_| positive_id(r))
                .collect::<Result<Vec<_>, _>>()?;
            VerifiedComponent::PotDecorations(ids)
        }
        "minecraft:bees" => VerifiedComponent::Bees(read_list(r, |r| {
            Ok(BeeData {
                nbt: read_opaque_nbt(r, 10)?,
                ticks_in_hive: positive_id(r)?,
                min_ticks_in_hive: positive_id(r)?,
            })
        })?),
        "minecraft:lock" => VerifiedComponent::Lock(read_opaque_nbt(r, 10)?),
        "minecraft:container_loot" => VerifiedComponent::ContainerLoot(read_opaque_nbt(r, 10)?),
        "minecraft:custom_name" => VerifiedComponent::CustomName(read_text(r)?),
        "minecraft:item_name" => VerifiedComponent::ItemName(read_text(r)?),
        "minecraft:lore" => VerifiedComponent::Lore(read_list(r, read_text)?),
        "minecraft:rarity" => {
            let rarity = positive_id(r)?;
            if rarity > 3 {
                return Err(invalid("invalid rarity"));
            }
            VerifiedComponent::Rarity(rarity as u8)
        }
        "minecraft:enchantments" => {
            let (entries, show_in_tooltip) = read_enchantments(r)?;
            VerifiedComponent::Enchantments {
                entries,
                show_in_tooltip,
            }
        }
        "minecraft:stored_enchantments" => {
            let (entries, show_in_tooltip) = read_enchantments(r)?;
            VerifiedComponent::StoredEnchantments {
                entries,
                show_in_tooltip,
            }
        }
        "minecraft:enchantment_glint_override" => {
            VerifiedComponent::EnchantmentGlintOverride(r.read_bool()?)
        }
        "minecraft:unbreakable" => VerifiedComponent::Unbreakable(r.read_bool()?),
        "minecraft:damage" => VerifiedComponent::Damage(positive_id(r)?),
        "minecraft:repair_cost" => VerifiedComponent::RepairCost(positive_id(r)?),
        "minecraft:dyed_color" => VerifiedComponent::DyedColor {
            rgb: r.read_u32()?,
            show_in_tooltip: r.read_bool()?,
        },
        "minecraft:base_color" => {
            let color = positive_id(r)?;
            if color > 15 {
                return Err(invalid("invalid dye color"));
            }
            VerifiedComponent::BaseColor(color as u8)
        }
        "minecraft:potion_contents" => {
            let potion = if r.read_bool()? {
                Some(positive_id(r)?)
            } else {
                None
            };
            let custom_color = if r.read_bool()? {
                Some(r.read_u32()?)
            } else {
                None
            };
            let effects = read_list(r, |r| {
                let id = positive_id(r)?;
                let details = read_effect_details(r, 0)?;
                Ok(PotionEffect {
                    id,
                    amplifier: details.amplifier,
                    duration: details.duration,
                    ambient: details.ambient,
                    show_particles: details.show_particles,
                    show_icon: details.show_icon,
                    hidden: details.hidden,
                })
            })?;
            let custom_name = if r.read_bool()? {
                Some(read_string(r)?)
            } else {
                None
            };
            VerifiedComponent::PotionContents(PotionContents {
                potion,
                custom_color,
                effects,
                custom_name,
            })
        }
        "minecraft:suspicious_stew_effects" => {
            VerifiedComponent::SuspiciousStewEffects(read_list(r, |r| {
                Ok((positive_id(r)?, positive_id(r)?))
            })?)
        }
        "minecraft:ominous_bottle_amplifier" => {
            let amplifier = positive_id(r)?;
            if amplifier > 4 {
                return Err(invalid("invalid ominous bottle amplifier"));
            }
            VerifiedComponent::OminousBottleAmplifier(amplifier)
        }
        "minecraft:fireworks" => {
            let flight_duration = positive_id(r)?;
            if flight_duration > 255 {
                return Err(invalid("invalid firework flight duration"));
            }
            let explosions = read_list(r, read_explosion)?;
            VerifiedComponent::Fireworks {
                flight_duration,
                explosions,
            }
        }
        "minecraft:firework_explosion" => VerifiedComponent::FireworkExplosion(read_explosion(r)?),
        "minecraft:banner_patterns" => VerifiedComponent::BannerPatterns(read_list(r, |r| {
            let holder = positive_id(r)?;
            let pattern = if holder == 0 {
                BannerPatternKind::Inline {
                    asset_id: read_string(r)?,
                    translation_key: read_string(r)?,
                }
            } else {
                BannerPatternKind::Registry(holder)
            };
            let color = positive_id(r)?;
            if color > 15 {
                return Err(invalid("invalid banner dye"));
            }
            Ok(BannerPattern {
                pattern,
                color: color as u8,
            })
        })?),
        "minecraft:hide_additional_tooltip" => VerifiedComponent::HideAdditionalTooltip,
        "minecraft:hide_tooltip" => VerifiedComponent::HideTooltip,
        "minecraft:instrument" => VerifiedComponent::Instrument(read_instrument(r)?),
        "minecraft:block_state" => {
            VerifiedComponent::BlockState(read_list(r, |r| Ok((read_string(r)?, read_string(r)?)))?)
        }
        "minecraft:profile" => {
            let name = if r.read_bool()? {
                Some(read_string(r)?)
            } else {
                None
            };
            let uuid = if r.read_bool()? {
                Some(r.read_uuid()?)
            } else {
                None
            };
            let properties = read_list(r, |r| {
                let name = read_string(r)?;
                let value = read_string(r)?;
                let signature = if r.read_bool()? {
                    Some(read_string(r)?)
                } else {
                    None
                };
                Ok(ProfileProperty {
                    name,
                    value,
                    signature,
                })
            })?;
            VerifiedComponent::Profile(Profile {
                name,
                uuid,
                properties,
            })
        }
        "minecraft:container" => {
            if depth >= MAX_CONTAINER_DEPTH {
                return Err(invalid("nested container forbidden"));
            }
            let count = r.read_count(MAX_CONTAINER_SLOTS)?;
            let mut stacks = Vec::with_capacity(count);
            for _ in 0..count {
                stacks.push(read_stack(r, depth + 1)?);
            }
            VerifiedComponent::Container(stacks)
        }
        _ => return Err(invalid("unsupported component type")),
    })
}

/// Decode, sanitize, encode canonically, and retain components under both
/// per-component and aggregate budgets. Each rejected value is counted once.
pub fn sanitize_and_roundtrip_components(raw: &[RawComponent]) -> SanitizedComponents {
    let mut out = SanitizedComponents::default();
    let mut total = 0;
    let mut seen = HashSet::new();
    for component in raw {
        if !is_verified_component_type(component.type_id) {
            out.dropped_unknown += 1;
            continue;
        }
        if !seen.insert(component.type_id) {
            out.dropped_invalid += 1;
            continue;
        }
        if component.data.len() > MAX_COMPONENT_BYTES {
            out.dropped_oversized += 1;
            continue;
        }
        let encoded = VerifiedComponent::decode(component).and_then(|c| c.to_raw());
        match encoded {
            Ok(encoded) => {
                let size = encoded.data.len() + varint_length(encoded.type_id);
                if encoded.data.len() > MAX_COMPONENT_BYTES
                    || total + size > MAX_STACK_COMPONENTS_BYTES
                    || out.kept >= MAX_LIST_ENTRIES
                {
                    out.dropped_oversized += 1;
                } else {
                    total += size;
                    out.kept += 1;
                    out.components.push(encoded);
                }
            }
            Err(ProtoError::LengthOutOfBounds { .. }) => out.dropped_oversized += 1,
            Err(_) => out.dropped_invalid += 1,
        }
    }
    out
}

fn varint_length(mut id: u32) -> usize {
    let mut count = 1;
    while id >= 128 {
        id >>= 7;
        count += 1;
    }
    count
}

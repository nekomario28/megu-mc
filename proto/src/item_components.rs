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
use crate::wire::{ProtoError, WireReader, WireWriter};

pub const MAX_COMPONENT_BYTES: usize = 4_096;
pub const MAX_STACK_COMPONENTS_BYTES: usize = 8_192;
pub const MAX_LIST_ENTRIES: usize = 64;
pub const MAX_CONTAINER_SLOTS: usize = 27;
pub const MAX_CONTAINER_DEPTH: usize = 1;

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

/// All 24 distinct types explicitly named in the requested 1.21.1 subset.
/// Compound text values are sanitized anonymous-NBT spans.
#[derive(Clone, Debug, PartialEq)]
pub enum VerifiedComponent {
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
    EnchantmentGlintOverride(bool),
    Unbreakable(bool),
    Damage(u32),
    RepairCost(u32),
    DyedColor {
        rgb: u32,
        show_in_tooltip: bool,
    },
    BaseColor(u8),
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
    Instrument(Instrument),
    BlockState(Vec<(String, String)>),
    Profile(Profile),
    Container(Vec<ItemStack>),
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
    let raw = r.read_anonymous_nbt()?;
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
        if matches!(verified, VerifiedComponent::Container(_)) {
            return Err(invalid("nested container forbidden"));
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

fn verified_size(value: &VerifiedComponent) -> Result<usize, ProtoError> {
    let mut size = 0;
    match value {
        VerifiedComponent::CustomName(text) | VerifiedComponent::ItemName(text) => {
            add_size(&mut size, text.len(), MAX_COMPONENT_BYTES)?;
        }
        VerifiedComponent::Lore(lines) => {
            size = list_prefix(lines.len(), MAX_LIST_ENTRIES)?;
            for line in lines {
                add_size(&mut size, line.len(), MAX_COMPONENT_BYTES)?;
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
        VerifiedComponent::Damage(n) | VerifiedComponent::RepairCost(n) => size = integer_size(*n)?,
        VerifiedComponent::OminousBottleAmplifier(n) => {
            if *n > 4 {
                return Err(invalid("invalid ominous bottle amplifier"));
            }
            size = integer_size(*n)?;
        }
        VerifiedComponent::DyedColor { .. } => size = 5,
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
        VerifiedComponent::HideAdditionalTooltip | VerifiedComponent::HideTooltip => size = 0,
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
            Self::CustomName(raw) | Self::ItemName(raw) => w.raw(raw),
            Self::Lore(lines) => put_list(&mut w, lines, |w, line| w.raw(line)),
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
            Self::Damage(value) | Self::RepairCost(value) | Self::OminousBottleAmplifier(value) => {
                w.write_varint(*value as i32)
            }
            Self::DyedColor {
                rgb,
                show_in_tooltip,
            } => {
                w.write_u32(*rgb);
                w.write_bool(*show_in_tooltip);
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
    V767_COMPONENTS.get(type_id as usize).is_some_and(|name| {
        matches!(
            *name,
            "minecraft:custom_name"
                | "minecraft:item_name"
                | "minecraft:lore"
                | "minecraft:rarity"
                | "minecraft:enchantments"
                | "minecraft:stored_enchantments"
                | "minecraft:enchantment_glint_override"
                | "minecraft:unbreakable"
                | "minecraft:damage"
                | "minecraft:repair_cost"
                | "minecraft:dyed_color"
                | "minecraft:base_color"
                | "minecraft:potion_contents"
                | "minecraft:suspicious_stew_effects"
                | "minecraft:ominous_bottle_amplifier"
                | "minecraft:fireworks"
                | "minecraft:firework_explosion"
                | "minecraft:banner_patterns"
                | "minecraft:hide_additional_tooltip"
                | "minecraft:hide_tooltip"
                | "minecraft:instrument"
                | "minecraft:block_state"
                | "minecraft:profile"
                | "minecraft:container"
        )
    })
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

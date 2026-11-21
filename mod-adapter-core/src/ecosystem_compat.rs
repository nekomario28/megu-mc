// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//
//! Generic top-100 ecosystem primitives shared by every modded ecosystem:
//!
//! 1. [`MapFairPlayConfig`] / [`FairPlayEnforcer`] — map-mod fair-play mode.
//!    One config produces *client-side* control signals (the official Xaero
//!    `§f§a§i§r§x§a§e§r§o` / `§n§o§m§i§n§i§m§a§p` / `§r§e§s§e§t§x§a§e§r§o`
//!    chat codes plus the `xaerominimap:main` / `xaeroworldmap:main`
//!    level-ID plugin-channel payloads) and *server-side* radar suppression
//!    ([`should_transmit_entity`]) so a hacked map client that ignores the
//!    codes still cannot see occluded underground or far-away entities. The
//!    same code path runs on a dedicated server and on a client-hosted
//!    ("friend host") world — the enforcer is pure logic.
//! 2. [`AccessoryInventory`] — a unified Curios/Trinkets-style accessory slot
//!    model: standard slot groups (`head`, `necklace`, `back`, `body`,
//!    `belt`, `hands`, `ring` x2, `charm`, `feet`) plus arbitrary custom
//!    slot types, tag-based equip validation, vanilla three-stage attribute
//!    modifier aggregation, and deterministic `ACC1` persistence.
//! 3. [`scan_mod_jar`] / [`plan_dual_loader_mods_dir`] — a native
//!    dual-loader JAR scanner reading `fabric.mod.json` and
//!    `META-INF/neoforge.mods.toml` / `META-INF/mods.toml` out of one
//!    archive, then merging Fabric and NeoForge mods inside the *same*
//!    `mods/` directory into a single unified native/Tier-2 mod table.
//!
//! Everything here is pure `std` over [`crate::data_loader`]'s JSON parser,
//! [`crate::modpack`]'s fail-closed ZIP reader, and
//! [`crate::version_compat`]'s `VersionRange` — zero new crates.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::data_loader::{parse_json, JsonValue};
use crate::fallback::is_builtin_superseded_mod;
use crate::modpack;
use crate::version_compat::VersionRange;

// ===========================================================================
// 1. Map-mod fair-play mode
// ===========================================================================

/// Official Xaero's-Map fair-play control code. Sent inside a chat/system
/// message; honest Xaero Minimap/World Map clients disable cave mapping and
/// the entity radar for the rest of the connection.
pub const XAERO_FAIR_PLAY_CODE: &str = "§f§a§i§r§x§a§e§r§o";

/// Control code telling every map mod the minimap must stay fully off for
/// this connection (the strictest state: nothing allowed at all).
pub const NO_MINIMAP_CODE: &str = "§n§o§m§i§n§i§m§a§p";

/// Control code lifting every previously broadcast map restriction (sent
/// when fair-play is off or the policy relaxed so stale restrictions do not
/// persist across a config change).
pub const XAERO_RESET_CODE: &str = "§r§e§s§e§t§x§a§e§r§o";

/// Xaero's Minimap plugin channel carrying the per-world level id.
pub const XAERO_MINIMAP_CHANNEL: &str = "xaerominimap:main";

/// Xaero's World Map plugin channel carrying the per-world level id.
pub const XAERO_WORLDMAP_CHANNEL: &str = "xaeroworldmap:main";

/// Radius within which an entity is always transmitted under fair-play
/// radar suppression — close combat stays 100% vanilla-smooth.
pub const FAIR_PLAY_NEARBY_RADIUS: f64 = 24.0;

/// Beyond this radius an occluded entity is always suppressed under
/// fair-play radar enforcement (the "far occluded" wallhack case).
pub const FAIR_PLAY_TRACK_RADIUS: f64 = 64.0;

/// Y level below which an entity counts as underground for radar
/// suppression (one layer under vanilla's Y=63 sea level).
pub const CAVE_LAYER_Y: f64 = 60.0;

/// Map-mod fair-play configuration shared by dedicated servers and client
/// friend-hosts. The *same* config drives the client-side control codes and
/// the server-side radar enforcement, so both hosts behave identically.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MapFairPlayConfig {
    /// Master switch. `false` = no restrictions (a `§r§e§s§e§t§x§a§e§r§o`
    /// reset code is broadcast so previously applied restrictions clear).
    pub fair_play: bool,
    /// Permit cave-mode / underground-entity mapping on honest clients.
    pub allow_cave_map: bool,
    /// Permit the entity radar on honest clients (and disable server-side
    /// radar suppression entirely).
    pub allow_entity_radar: bool,
    /// Permit waypoint teleports through map mods.
    pub allow_waypoint_teleport: bool,
    /// Deterministic non-zero per-world id broadcast on the Xaero plugin
    /// channels so a client never merges maps across servers or worlds.
    /// `0` means "unassigned" — no level-id payload is emitted. Derive one
    /// with [`derive_world_level_id`].
    pub world_level_id: i32,
}

/// Error produced by [`MapFairPlayConfig::from_kv_pairs`] and
/// [`MapFairPlayConfig::from_properties_text`]. Fail-closed: a malformed
/// known key is an error, never a silent default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FairPlayConfigError(String);

impl FairPlayConfigError {
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FairPlayConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FairPlayConfigError {}

fn fp_err(msg: impl Into<String>) -> FairPlayConfigError {
    FairPlayConfigError(msg.into())
}

fn parse_bool_value(raw: &str, key: &str) -> Result<bool, FairPlayConfigError> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(fp_err(format!(
            "{key}: '{other}' is not a boolean (expected true/false)"
        ))),
    }
}

/// Normalize a config key: lowercase, `-` folded to `_`, `xaero.`/`mc-spine.`
/// style prefixes flattened so `mc_spine.fair-play`, `fair-play`, and
/// `fair_play` all land on the same field.
fn normalize_fair_play_key(key: &str) -> String {
    let mut k = key.trim().to_ascii_lowercase().replace(['-', '.'], "_");
    for prefix in ["mc_spine_", "mcspine_", "xaero_", "map_", "minimap_"] {
        if let Some(rest) = k.strip_prefix(prefix) {
            k = rest.to_string();
            break;
        }
    }
    k
}

/// Strip one matching pair of quotes around a TOML-style value.
fn unquote_value(value: &str) -> &str {
    let v = value.trim();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

impl MapFairPlayConfig {
    /// Build a config from `(key, value)` pairs — the shape a
    /// `server.properties` line or a TOML `key = value` entry reduces to.
    /// Unknown keys are ignored (a real `server.properties` carries dozens
    /// of unrelated settings); a *known* key with a malformed value is a
    /// fail-closed error, as is `world_level_id = 0` (the id must be
    /// non-zero so maps never mix across worlds).
    pub fn from_kv_pairs<'a, I>(pairs: I) -> Result<Self, FairPlayConfigError>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut cfg = Self::default();
        for (raw_key, raw_value) in pairs {
            let key = normalize_fair_play_key(raw_key);
            let value = unquote_value(raw_value);
            match key.as_str() {
                "fair_play" => cfg.fair_play = parse_bool_value(value, "fair_play")?,
                "allow_cave_map" => {
                    cfg.allow_cave_map = parse_bool_value(value, "allow_cave_map")?
                }
                "allow_entity_radar" => {
                    cfg.allow_entity_radar = parse_bool_value(value, "allow_entity_radar")?
                }
                "allow_waypoint_teleport" => {
                    cfg.allow_waypoint_teleport =
                        parse_bool_value(value, "allow_waypoint_teleport")?
                }
                "world_level_id" | "level_id" => {
                    let id: i32 = value.parse().map_err(|_| {
                        fp_err(format!("world_level_id: '{value}' is not an integer"))
                    })?;
                    if id == 0 {
                        return Err(fp_err(
                            "world_level_id must be non-zero so maps never mix across worlds",
                        ));
                    }
                    cfg.world_level_id = id;
                }
                _ => {}
            }
        }
        Ok(cfg)
    }

    /// Parse a `server.properties`/TOML-style text: `#`/`!` comment lines,
    /// blank lines, `[section]` headers, and `key = value` / `key: value`
    /// lines. A line with neither separator is a fail-closed error.
    pub fn from_properties_text(text: &str) -> Result<Self, FairPlayConfigError> {
        let mut pairs = Vec::new();
        for (lineno, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty()
                || line.starts_with('#')
                || line.starts_with('!')
                || (line.starts_with('[') && line.ends_with(']'))
            {
                continue;
            }
            let Some((key, value)) = line
                .split_once('=')
                .or_else(|| line.split_once(':'))
            else {
                return Err(fp_err(format!(
                    "line {}: '{line}' has no '=' or ':' separator",
                    lineno + 1
                )));
            };
            pairs.push((key.trim().to_string(), value.trim().to_string()));
        }
        Self::from_kv_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    }
}

/// Deterministic, non-zero world level id derived from a stable world key
/// (world name, dimension path, or seed text). FNV-1a 64 folded into the
/// positive `i32` range; the `0` outcome is remapped to `1`.
pub fn derive_world_level_id(world_key: &str) -> i32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in world_key.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    match (h & 0x7fff_ffff) as i32 {
        0 => 1,
        v => v,
    }
}

/// One plugin-channel payload to send to a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelPayload {
    /// Channel resource location (`xaerominimap:main`, `xaeroworldmap:main`).
    pub channel: String,
    /// Raw payload bytes.
    pub payload: Vec<u8>,
}

/// Everything a client needs on join to enforce the map policy honestly:
/// chat-encoded control codes plus the per-world level-id plugin payloads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FairPlaySignals {
    /// Control strings to broadcast in a chat/system message, in order.
    pub control_codes: Vec<String>,
    /// `[0x00, i32_be(world_level_id)]` payloads per channel.
    pub level_id_payloads: Vec<ChannelPayload>,
}

/// Emit the client-side signals for `cfg`. Identical on a dedicated server
/// and a client friend-host — the caller is whichever host broadcasts the
/// join-time system message and plugin payloads.
///
/// Control-code semantics:
/// - `fair_play = false` → `§r§e§s§e§t§x§a§e§r§o` (lift all restrictions).
/// - `fair_play` with cave map *and* entity radar both denied →
///   `§f§a§i§r§x§a§e§r§o`; when waypoint teleports are denied too the map is
///   fully disabled, so `§n§o§m§i§n§i§m§a§p` follows.
/// - `fair_play` with a partial allowance (no control code exists for
///   per-feature grants) → `§r§e§s§e§t§x§a§e§r§o`; the remaining restriction
///   is enforced server-side by [`should_transmit_entity`] where a hacked
///   client cannot bypass it.
///
/// Level-id payloads are emitted whenever `world_level_id` is non-zero so
/// Xaero never mixes maps across servers/worlds — even on unrestricted
/// servers.
pub fn fair_play_control_signals(cfg: &MapFairPlayConfig) -> FairPlaySignals {
    let mut control_codes = Vec::new();
    if !cfg.fair_play {
        control_codes.push(XAERO_RESET_CODE.to_string());
    } else if !cfg.allow_cave_map && !cfg.allow_entity_radar {
        control_codes.push(XAERO_FAIR_PLAY_CODE.to_string());
        if !cfg.allow_waypoint_teleport {
            control_codes.push(NO_MINIMAP_CODE.to_string());
        }
    } else {
        control_codes.push(XAERO_RESET_CODE.to_string());
    }

    let mut level_id_payloads = Vec::new();
    if cfg.world_level_id != 0 {
        let mut payload = Vec::with_capacity(5);
        payload.push(0x00);
        payload.extend_from_slice(&cfg.world_level_id.to_be_bytes());
        for channel in [XAERO_MINIMAP_CHANNEL, XAERO_WORLDMAP_CHANNEL] {
            level_id_payloads.push(ChannelPayload {
                channel: channel.to_string(),
                payload: payload.clone(),
            });
        }
    }
    FairPlaySignals {
        control_codes,
        level_id_payloads,
    }
}

/// Server-side entity-tracking gate enforcing the radar restriction a
/// modified map client would otherwise ignore.
///
/// Under `fair_play && !allow_entity_radar` a non-operator viewer receives
/// an entity only when at least one vanilla-legitimate channel exists:
/// the target is not radar-relevant (`is_hostile_or_player = false`), it is
/// within [`FAIR_PLAY_NEARBY_RADIUS`], the viewer has line of sight, or it
/// is neither underground nor beyond [`FAIR_PLAY_TRACK_RADIUS`]. Occluded
/// underground entities and far-away occluded entities — exactly what a
/// cheat radar reveals — are suppressed while nearby/visible entities keep
/// combat and gameplay 100% vanilla-smooth. Operators always receive every
/// entity.
pub fn should_transmit_entity(
    cfg: &MapFairPlayConfig,
    viewer_pos: [f64; 3],
    target_pos: [f64; 3],
    is_hostile_or_player: bool,
    has_line_of_sight: bool,
    viewer_is_op: bool,
) -> bool {
    if viewer_is_op || !cfg.fair_play || cfg.allow_entity_radar || !is_hostile_or_player {
        return true;
    }
    let dx = viewer_pos[0] - target_pos[0];
    let dy = viewer_pos[1] - target_pos[1];
    let dz = viewer_pos[2] - target_pos[2];
    let dist2 = dx * dx + dy * dy + dz * dz;
    if dist2 <= FAIR_PLAY_NEARBY_RADIUS * FAIR_PLAY_NEARBY_RADIUS {
        return true;
    }
    if has_line_of_sight {
        return true;
    }
    if target_pos[1] < CAVE_LAYER_Y {
        return false;
    }
    dist2 <= FAIR_PLAY_TRACK_RADIUS * FAIR_PLAY_TRACK_RADIUS
}

/// Owning wrapper around [`MapFairPlayConfig`] for hosts that keep one
/// enforcer per world.
#[derive(Clone, Debug)]
pub struct FairPlayEnforcer {
    cfg: MapFairPlayConfig,
}

impl FairPlayEnforcer {
    pub fn new(cfg: MapFairPlayConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &MapFairPlayConfig {
        &self.cfg
    }

    /// [`fair_play_control_signals`] over this enforcer's config.
    pub fn signals(&self) -> FairPlaySignals {
        fair_play_control_signals(&self.cfg)
    }

    /// [`should_transmit_entity`] over this enforcer's config.
    pub fn should_transmit_entity(
        &self,
        viewer_pos: [f64; 3],
        target_pos: [f64; 3],
        is_hostile_or_player: bool,
        has_line_of_sight: bool,
        viewer_is_op: bool,
    ) -> bool {
        should_transmit_entity(
            &self.cfg,
            viewer_pos,
            target_pos,
            is_hostile_or_player,
            has_line_of_sight,
            viewer_is_op,
        )
    }
}

// ===========================================================================
// 2. Unified accessory slots (Curios / Trinkets compatible)
// ===========================================================================

/// Maximum slots one slot type may declare (`ring` = 2, custom types up to
/// 16).
pub const MAX_SLOTS_PER_TYPE: usize = 16;

/// Vanilla `AttributeModifier.Operation`: the three evaluation stages an
/// attribute value passes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifierOperation {
    /// Flat addition applied to the base first.
    AddValue,
    /// Fraction of the *original* base added after [`Self::AddValue`].
    AddMultipliedBase,
    /// Fraction of the running total added last.
    AddMultipliedTotal,
}

impl ModifierOperation {
    fn to_wire(self) -> u8 {
        match self {
            Self::AddValue => 0,
            Self::AddMultipliedBase => 1,
            Self::AddMultipliedTotal => 2,
        }
    }

    fn from_wire(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::AddValue,
            1 => Self::AddMultipliedBase,
            2 => Self::AddMultipliedTotal,
            _ => return None,
        })
    }
}

/// One attribute modifier granted by an equipped accessory.
#[derive(Clone, Debug, PartialEq)]
pub struct AttributeModifier {
    /// Attribute id (`minecraft:generic.armor`, `minecraft:generic.luck`, …).
    pub attribute: String,
    pub operation: ModifierOperation,
    pub amount: f64,
}

/// Per-attribute stage sums from all equipped accessories.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AttributeTotals {
    pub add_value: f64,
    pub add_multiplied_base: f64,
    pub add_multiplied_total: f64,
}

impl AttributeTotals {
    /// Fold the stage sums into a final value the way vanilla
    /// `Attribute.getValue` does: `v = base + Σadd + base·Σmult_base`,
    /// then `v·(1 + Σmult_total)`.
    pub fn apply(&self, base: f64) -> f64 {
        let running = base + self.add_value + base * self.add_multiplied_base;
        running * (1.0 + self.add_multiplied_total)
    }

    /// Whether every stage sum is exactly zero.
    pub fn is_zero(&self) -> bool {
        self.add_value == 0.0 && self.add_multiplied_base == 0.0 && self.add_multiplied_total == 0.0
    }
}

/// One equippable accessory: an item id, the slot tags it belongs to, and
/// the attribute modifiers it grants while equipped.
#[derive(Clone, Debug, PartialEq)]
pub struct AccessoryItem {
    /// Item id (`aether:iron_ring`, `sophisticatedbackpacks:backpack`, …).
    pub item_id: String,
    /// Slot tags the item carries (`ring`, `pendant`, `goggles`, …). An
    /// item equips into a slot only when the tag sets intersect — or when
    /// the slot declares an empty allowlist (unrestricted slot).
    pub tags: BTreeSet<String>,
    /// Attribute modifiers applied while the item is equipped.
    pub modifiers: Vec<AttributeModifier>,
}

impl AccessoryItem {
    pub fn new<I, S>(item_id: &str, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            item_id: item_id.to_string(),
            tags: tags.into_iter().map(Into::into).collect(),
            modifiers: Vec::new(),
        }
    }

    pub fn with_modifiers(mut self, modifiers: Vec<AttributeModifier>) -> Self {
        self.modifiers = modifiers;
        self
    }
}

/// One accessory slot type: how many indices it offers and which item tags
/// may equip into it. An empty `allowed_tags` set means *unrestricted*.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotTypeDef {
    pub slot_type: String,
    /// `1..=MAX_SLOTS_PER_TYPE`.
    pub slot_count: u8,
    pub allowed_tags: BTreeSet<String>,
}

/// Error produced by [`AccessoryInventory`] operations and `ACC1`
/// decoding. Fail-closed throughout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessoryError {
    UnknownSlotType(String),
    SlotIndexOutOfRange {
        slot_type: String,
        index: usize,
        count: usize,
    },
    ItemNotAllowed {
        slot_type: String,
        item_id: String,
    },
    SlotOccupied {
        slot_type: String,
        index: usize,
    },
    SlotEmpty {
        slot_type: String,
        index: usize,
    },
    InvalidSlotCount {
        slot_type: String,
        count: usize,
    },
    InvalidSlotTypeName(String),
    DuplicateSlotType(String),
    Persistence(String),
}

impl fmt::Display for AccessoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSlotType(t) => write!(f, "unknown accessory slot type '{t}'"),
            Self::SlotIndexOutOfRange {
                slot_type,
                index,
                count,
            } => write!(
                f,
                "slot index {index} out of range for '{slot_type}' ({count} slots)"
            ),
            Self::ItemNotAllowed { slot_type, item_id } => {
                write!(f, "item '{item_id}' is not allowed in slot '{slot_type}'")
            }
            Self::SlotOccupied { slot_type, index } => {
                write!(f, "slot '{slot_type}[{index}]' is already occupied")
            }
            Self::SlotEmpty { slot_type, index } => {
                write!(f, "slot '{slot_type}[{index}]' is empty")
            }
            Self::InvalidSlotCount { slot_type, count } => write!(
                f,
                "slot type '{slot_type}' count {count} outside 1..={MAX_SLOTS_PER_TYPE}"
            ),
            Self::InvalidSlotTypeName(name) => {
                write!(f, "invalid accessory slot type name '{name}'")
            }
            Self::DuplicateSlotType(t) => write!(f, "duplicate accessory slot type '{t}'"),
            Self::Persistence(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for AccessoryError {}

/// Standard slot groups covering the generic accessory surface (Aether
/// rings/pendant/cape/gloves/shield-style items, Artifacts, backpack mods,
/// goggle mods, pet/accessory mods) with zero mod-specific entries: the
/// aliases are slot *semantics*, not mod namespaces.
///
/// `(slot_type, slot_count, allowed_tags)`.
pub const STANDARD_SLOT_TYPES: &[(&str, u8, &[&str])] = &[
    ("head", 1, &["head", "hat", "goggles"]),
    ("necklace", 1, &["necklace", "pendant"]),
    ("back", 1, &["back", "cape", "backpack"]),
    ("body", 1, &["body", "chest"]),
    ("belt", 1, &["belt"]),
    ("hands", 1, &["hands", "gloves"]),
    ("ring", 2, &["ring"]),
    ("charm", 1, &["charm", "artifact"]),
    ("feet", 1, &["feet", "boots"]),
];

fn valid_slot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b':' | b'/'))
}

fn valid_tag_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b':' | b'/'))
}

/// A complete accessory inventory: slot type definitions plus the equipped
/// item (or empty slot) under each index. Curios and Trinkets both reduce
/// to this shape.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccessoryInventory {
    defs: BTreeMap<String, SlotTypeDef>,
    slots: BTreeMap<String, Vec<Option<AccessoryItem>>>,
}

impl AccessoryInventory {
    /// The nine standard slot groups from [`STANDARD_SLOT_TYPES`].
    pub fn standard() -> Self {
        let mut inv = Self::default();
        for (slot_type, count, tags) in STANDARD_SLOT_TYPES {
            inv.defs.insert(
                (*slot_type).to_string(),
                SlotTypeDef {
                    slot_type: (*slot_type).to_string(),
                    slot_count: *count,
                    allowed_tags: tags.iter().map(|t| (*t).to_string()).collect(),
                },
            );
            inv.slots
                .insert((*slot_type).to_string(), vec![None; *count as usize]);
        }
        inv
    }

    /// Register an additional slot type (`aether:shield`, `spell:focus`,
    /// …). `slot_count` must be in `1..=MAX_SLOTS_PER_TYPE`, the name must
    /// be a valid slot id, and it must not collide with an existing type.
    pub fn add_slot_type<I, S>(
        &mut self,
        slot_type: &str,
        slot_count: usize,
        allowed_tags: I,
    ) -> Result<(), AccessoryError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if !valid_slot_name(slot_type) {
            return Err(AccessoryError::InvalidSlotTypeName(slot_type.to_string()));
        }
        if !(1..=MAX_SLOTS_PER_TYPE).contains(&slot_count) {
            return Err(AccessoryError::InvalidSlotCount {
                slot_type: slot_type.to_string(),
                count: slot_count,
            });
        }
        if self.defs.contains_key(slot_type) {
            return Err(AccessoryError::DuplicateSlotType(slot_type.to_string()));
        }
        let tags: BTreeSet<String> = allowed_tags.into_iter().map(Into::into).collect();
        for tag in &tags {
            if !valid_tag_name(tag) {
                return Err(AccessoryError::InvalidSlotTypeName(tag.clone()));
            }
        }
        self.defs.insert(
            slot_type.to_string(),
            SlotTypeDef {
                slot_type: slot_type.to_string(),
                slot_count: slot_count as u8,
                allowed_tags: tags,
            },
        );
        self.slots
            .insert(slot_type.to_string(), vec![None; slot_count]);
        Ok(())
    }

    /// Definition for `slot_type`, when registered.
    pub fn slot_def(&self, slot_type: &str) -> Option<&SlotTypeDef> {
        self.defs.get(slot_type)
    }

    /// All registered slot types in deterministic (sorted) order.
    pub fn slot_types(&self) -> Vec<&str> {
        self.defs.keys().map(String::as_str).collect()
    }

    /// Item currently equipped at `slot_type[index]`, if any.
    pub fn item_at(&self, slot_type: &str, index: usize) -> Option<&AccessoryItem> {
        self.slots
            .get(slot_type)
            .and_then(|v| v.get(index))
            .and_then(Option::as_ref)
    }

    fn slot_mut(
        &mut self,
        slot_type: &str,
        index: usize,
    ) -> Result<&mut Option<AccessoryItem>, AccessoryError> {
        let count = self
            .defs
            .get(slot_type)
            .ok_or_else(|| AccessoryError::UnknownSlotType(slot_type.to_string()))?
            .slot_count as usize;
        self.slots
            .get_mut(slot_type)
            .and_then(|v| v.get_mut(index))
            .ok_or(AccessoryError::SlotIndexOutOfRange {
                slot_type: slot_type.to_string(),
                index,
                count,
            })
    }

    /// Equip `item` at `slot_type[index]`, validating it against the slot's
    /// tag allowlist. An occupied slot is a fail-closed error — call
    /// [`Self::unequip`] first.
    pub fn equip(
        &mut self,
        slot_type: &str,
        index: usize,
        item: AccessoryItem,
    ) -> Result<(), AccessoryError> {
        let def = self
            .defs
            .get(slot_type)
            .ok_or_else(|| AccessoryError::UnknownSlotType(slot_type.to_string()))?;
        if index >= def.slot_count as usize {
            return Err(AccessoryError::SlotIndexOutOfRange {
                slot_type: slot_type.to_string(),
                index,
                count: def.slot_count as usize,
            });
        }
        if !def.allowed_tags.is_empty() && def.allowed_tags.is_disjoint(&item.tags) {
            return Err(AccessoryError::ItemNotAllowed {
                slot_type: slot_type.to_string(),
                item_id: item.item_id.clone(),
            });
        }
        let slot = self.slot_mut(slot_type, index)?;
        if slot.is_some() {
            return Err(AccessoryError::SlotOccupied {
                slot_type: slot_type.to_string(),
                index,
            });
        }
        *slot = Some(item);
        Ok(())
    }

    /// Remove and return the item at `slot_type[index]`.
    pub fn unequip(
        &mut self,
        slot_type: &str,
        index: usize,
    ) -> Result<AccessoryItem, AccessoryError> {
        let slot = self.slot_mut(slot_type, index)?;
        slot.take().ok_or_else(|| AccessoryError::SlotEmpty {
            slot_type: slot_type.to_string(),
            index,
        })
    }

    /// Sum every equipped accessory's modifiers per attribute, keeping the
    /// three vanilla stages separate so [`AttributeTotals::apply`]
    /// reproduces `Attribute.getValue` exactly.
    pub fn aggregate_attribute_modifiers(&self) -> BTreeMap<String, AttributeTotals> {
        let mut out: BTreeMap<String, AttributeTotals> = BTreeMap::new();
        for items in self.slots.values() {
            for item in items.iter().flatten() {
                for m in &item.modifiers {
                    let t = out.entry(m.attribute.clone()).or_default();
                    match m.operation {
                        ModifierOperation::AddValue => t.add_value += m.amount,
                        ModifierOperation::AddMultipliedBase => {
                            t.add_multiplied_base += m.amount
                        }
                        ModifierOperation::AddMultipliedTotal => {
                            t.add_multiplied_total += m.amount
                        }
                    }
                }
            }
        }
        out
    }

    /// Encode as a deterministic `ACC1` frame: `ACC1` magic, a big-endian
    /// CRC-32 of the body, then the body. `BTreeMap`/`BTreeSet` ordering
    /// plus ascending slot indices make the bytes a canonical form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_u16(&mut body, self.defs.len());
        for (slot_type, def) in &self.defs {
            put_str(&mut body, slot_type);
            body.push(def.slot_count);
            put_u16(&mut body, def.allowed_tags.len());
            for tag in &def.allowed_tags {
                put_str(&mut body, tag);
            }
            let items = &self.slots[slot_type];
            put_u16(&mut body, items.iter().flatten().count());
            for (index, item) in items.iter().enumerate() {
                let Some(item) = item else { continue };
                body.push(index as u8);
                put_str(&mut body, &item.item_id);
                put_u16(&mut body, item.tags.len());
                for tag in &item.tags {
                    put_str(&mut body, tag);
                }
                put_u16(&mut body, item.modifiers.len());
                for m in &item.modifiers {
                    put_str(&mut body, &m.attribute);
                    body.push(m.operation.to_wire());
                    body.extend_from_slice(&m.amount.to_bits().to_be_bytes());
                }
            }
        }
        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(ACC1_MAGIC);
        out.extend_from_slice(&acc_crc32(&body).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Decode an `ACC1` frame produced by [`Self::to_bytes`]. Magic,
    /// CRC-32, structure, slot-count bounds, ascending-index order, and
    /// trailing-byte checks all fail closed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, AccessoryError> {
        if bytes.len() < 8 || &bytes[..4] != ACC1_MAGIC {
            return Err(AccessoryError::Persistence(
                "ACC1: bad magic or truncated frame".to_string(),
            ));
        }
        let declared_crc = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
        let body = &bytes[8..];
        if acc_crc32(body) != declared_crc {
            return Err(AccessoryError::Persistence(
                "ACC1: CRC-32 mismatch".to_string(),
            ));
        }
        let mut cur = 0usize;
        let type_count = get_u16(body, &mut cur)? as usize;
        if type_count > 256 {
            return Err(AccessoryError::Persistence(
                "ACC1: unreasonable slot type count".to_string(),
            ));
        }
        let mut inv = AccessoryInventory::default();
        for _ in 0..type_count {
            let slot_type = get_str(body, &mut cur)?;
            if !valid_slot_name(&slot_type) {
                return Err(AccessoryError::Persistence(format!(
                    "ACC1: invalid slot type name '{slot_type}'"
                )));
            }
            if inv.defs.contains_key(&slot_type) {
                return Err(AccessoryError::Persistence(format!(
                    "ACC1: duplicate slot type '{slot_type}'"
                )));
            }
            let count = get_u8(body, &mut cur)? as usize;
            if !(1..=MAX_SLOTS_PER_TYPE).contains(&count) {
                return Err(AccessoryError::Persistence(format!(
                    "ACC1: slot type '{slot_type}' count {count} outside 1..={MAX_SLOTS_PER_TYPE}"
                )));
            }
            let tag_count = get_u16(body, &mut cur)? as usize;
            let mut allowed_tags = BTreeSet::new();
            for _ in 0..tag_count {
                let tag = get_str(body, &mut cur)?;
                if !valid_tag_name(&tag) {
                    return Err(AccessoryError::Persistence(format!(
                        "ACC1: invalid tag '{tag}'"
                    )));
                }
                allowed_tags.insert(tag);
            }
            let equipped_count = get_u16(body, &mut cur)? as usize;
            if equipped_count > count {
                return Err(AccessoryError::Persistence(format!(
                    "ACC1: '{slot_type}' equips {equipped_count} items into {count} slots"
                )));
            }
            let mut items = vec![None; count];
            let mut prev_index = None;
            for _ in 0..equipped_count {
                let index = get_u8(body, &mut cur)? as usize;
                if index >= count {
                    return Err(AccessoryError::Persistence(format!(
                        "ACC1: '{slot_type}' index {index} out of range"
                    )));
                }
                if prev_index.is_some_and(|p| index <= p) {
                    return Err(AccessoryError::Persistence(format!(
                        "ACC1: '{slot_type}' indices not strictly ascending"
                    )));
                }
                prev_index = Some(index);
                let item_id = get_str(body, &mut cur)?;
                let n_tags = get_u16(body, &mut cur)? as usize;
                let mut tags = BTreeSet::new();
                for _ in 0..n_tags {
                    tags.insert(get_str(body, &mut cur)?);
                }
                let n_mods = get_u16(body, &mut cur)? as usize;
                let mut modifiers = Vec::with_capacity(n_mods);
                for _ in 0..n_mods {
                    let attribute = get_str(body, &mut cur)?;
                    let op = ModifierOperation::from_wire(get_u8(body, &mut cur)?).ok_or_else(|| {
                        AccessoryError::Persistence("ACC1: invalid modifier operation".to_string())
                    })?;
                    let amount = get_f64(body, &mut cur)?;
                    modifiers.push(AttributeModifier {
                        attribute,
                        operation: op,
                        amount,
                    });
                }
                items[index] = Some(AccessoryItem {
                    item_id,
                    tags,
                    modifiers,
                });
            }
            inv.defs.insert(
                slot_type.clone(),
                SlotTypeDef {
                    slot_type: slot_type.clone(),
                    slot_count: count as u8,
                    allowed_tags,
                },
            );
            inv.slots.insert(slot_type, items);
        }
        if cur != body.len() {
            return Err(AccessoryError::Persistence(
                "ACC1: trailing bytes after final slot type".to_string(),
            ));
        }
        Ok(inv)
    }
}

// ---------------------------------------------------------------------------
// ACC1 codec primitives
// ---------------------------------------------------------------------------

const ACC1_MAGIC: &[u8; 4] = b"ACC1";
/// Longest accepted string inside an `ACC1` frame.
const ACC1_MAX_STRING: usize = 1024;

const ACC_CRC_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn acc_crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0, |crc, byte| {
        (crc >> 8) ^ ACC_CRC_TABLE[((crc as u8) ^ byte) as usize]
    })
}

fn put_u16(out: &mut Vec<u8>, v: usize) {
    out.extend_from_slice(&(v as u16).to_be_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    debug_assert!(s.len() <= u16::MAX as usize);
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn get_u8(body: &[u8], cur: &mut usize) -> Result<u8, AccessoryError> {
    let v = *body.get(*cur).ok_or_else(|| {
        AccessoryError::Persistence("ACC1: truncated body".to_string())
    })?;
    *cur += 1;
    Ok(v)
}

fn get_u16(body: &[u8], cur: &mut usize) -> Result<u16, AccessoryError> {
    let end = cur.checked_add(2).ok_or_else(|| {
        AccessoryError::Persistence("ACC1: truncated body".to_string())
    })?;
    let v = u16::from_be_bytes(
        body.get(*cur..end)
            .ok_or_else(|| AccessoryError::Persistence("ACC1: truncated body".to_string()))?
            .try_into()
            .unwrap(),
    );
    *cur = end;
    Ok(v)
}

fn get_f64(body: &[u8], cur: &mut usize) -> Result<f64, AccessoryError> {
    let end = cur.checked_add(8).ok_or_else(|| {
        AccessoryError::Persistence("ACC1: truncated body".to_string())
    })?;
    let v = f64::from_bits(u64::from_be_bytes(
        body.get(*cur..end)
            .ok_or_else(|| AccessoryError::Persistence("ACC1: truncated body".to_string()))?
            .try_into()
            .unwrap(),
    ));
    *cur = end;
    Ok(v)
}

fn get_str(body: &[u8], cur: &mut usize) -> Result<String, AccessoryError> {
    let len = get_u16(body, cur)? as usize;
    if len > ACC1_MAX_STRING {
        return Err(AccessoryError::Persistence(
            "ACC1: string exceeds length cap".to_string(),
        ));
    }
    let end = cur.checked_add(len).ok_or_else(|| {
        AccessoryError::Persistence("ACC1: truncated string".to_string())
    })?;
    let slice = body
        .get(*cur..end)
        .ok_or_else(|| AccessoryError::Persistence("ACC1: truncated string".to_string()))?;
    *cur = end;
    std::str::from_utf8(slice)
        .map(str::to_string)
        .map_err(|_| AccessoryError::Persistence("ACC1: non-UTF-8 string".to_string()))
}

// ===========================================================================
// 3. Dual-loader (`fabric.mod.json` + `META-INF/neoforge.mods.toml`) JAR scan
// ===========================================================================

/// Which loader ecosystem a mod JAR declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModLoaderKind {
    /// `fabric.mod.json` only.
    Fabric,
    /// `META-INF/neoforge.mods.toml` only.
    NeoForge,
    /// `META-INF/mods.toml` only (legacy Forge layout).
    ForgeLegacy,
    /// Both `fabric.mod.json` and a Forge-family manifest coexist in the
    /// same JAR (real "unified"/multi-loader releases).
    DualFabricNeoForge,
    /// Neither loader manifest — a pure datapack/resource archive.
    DatapackOnly,
}

impl ModLoaderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fabric => "fabric",
            Self::NeoForge => "neoforge",
            Self::ForgeLegacy => "forge",
            Self::DualFabricNeoForge => "fabric+neoforge",
            Self::DatapackOnly => "datapack",
        }
    }
}

impl fmt::Display for ModLoaderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which environment a mod may load in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModEnvironment {
    /// Client only — skipped on a dedicated server.
    Client,
    /// Dedicated server only.
    Server,
    /// Both sides (`"*"`/`BOTH`/undeclared).
    Both,
}

/// One declared dependency of a scanned mod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModDependency {
    /// The depended-on mod id.
    pub mod_id: String,
    /// Declared version constraint in the loader's syntax, if any.
    pub version_range: Option<String>,
    /// `true` when absence is fatal (Fabric `depends`, NeoForge
    /// `type="required"` / legacy `mandatory=true`); `false` for
    /// recommendations and optional integrations.
    pub required: bool,
}

/// Everything the unified mod table needs out of one JAR.
#[derive(Clone, Debug, PartialEq)]
pub struct ModJarDescriptor {
    /// Which loader ecosystem(s) this JAR declares.
    pub loader: ModLoaderKind,
    /// Primary mod id — `fabric.mod.json`'s `id` when a Fabric manifest is
    /// present (it is the canonical single id for dual jars), otherwise the
    /// first `[[mods]]` `modId`. Empty for [`ModLoaderKind::DatapackOnly`].
    pub mod_id: String,
    /// Every mod id this JAR claims (both manifests plus extra `[[mods]]`
    /// entries) — duplicate detection matches on any of them.
    pub declared_mod_ids: Vec<String>,
    /// Declared version string.
    pub version: String,
    /// Declared environment (Fabric `environment` / NeoForge minecraft-dep
    /// `side`; the more restrictive wins on dual jars).
    pub environment: ModEnvironment,
    /// Declared Minecraft range (`depends.minecraft` / the minecraft
    /// dependency's `versionRange`), parsed once so planners can call
    /// [`VersionRange::contains`] directly.
    pub declared_mc_range: Option<VersionRange>,
    /// `*.mixins.json` configs declared by the loader manifest(s). These
    /// feed [`crate::class_mixin_scanner`]'s class-limited scan, whose
    /// [`crate::mixin_lifter::MixinSite`]s flow into
    /// [`crate::mixin_lifter::lift_mixin_sites`] — the same pipeline for
    /// Fabric Intermediary and NeoForge Mojang mappings.
    pub mixin_configs: Vec<String>,
    /// Fabric `entrypoints` (all arrays flattened; `{adapter, value}`
    /// objects contribute their `value`). Empty for pure-Forge jars.
    pub entrypoints: Vec<String>,
    /// Merged dependency table (`required` wins on id collisions).
    pub dependencies: Vec<ModDependency>,
}

/// Error produced by [`scan_mod_jar`]. Every variant is fail-closed: the
/// archive is rejected outright, never partially trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JarScanError {
    /// The archive could not be opened or a manifest member failed the
    /// fail-closed ZIP checks (traversal, bomb, CRC, truncation).
    Zip(String),
    /// `fabric.mod.json` was not valid JSON.
    Json(String),
    /// A `*.mods.toml` member was not valid TOML.
    Toml(String),
    /// A manifest parsed but was structurally unusable (missing `id`,
    /// unknown `environment`, bad version range, …) or no loader manifest
    /// exists in a non-datapack archive.
    Manifest(String),
}

impl JarScanError {
    pub fn reason(&self) -> &str {
        match self {
            Self::Zip(m) | Self::Json(m) | Self::Toml(m) | Self::Manifest(m) => m,
        }
    }
}

impl fmt::Display for JarScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for JarScanError {}

fn jar_err(msg: impl Into<String>) -> JarScanError {
    JarScanError::Manifest(msg.into())
}

/// Mod ids the runtime itself provides: the game, the JVM, and the loader
/// layers mc-spine natively implements. Never reported as missing.
pub const RUNTIME_PROVIDED_MOD_IDS: &[&str] = &[
    "minecraft",
    "java",
    "fabricloader",
    "fabric-loader",
    "fabric",
    "fabric-api",
    "fabric_api",
    "neoforge",
    "forge",
    "javafml",
];

fn is_runtime_provided_mod(mod_id: &str) -> bool {
    RUNTIME_PROVIDED_MOD_IDS
        .iter()
        .any(|id| id.eq_ignore_ascii_case(mod_id))
}

// ---------------------------------------------------------------------------
// Minimal fail-closed TOML subset (enough for mods.toml / neoforge.mods.toml)
// ---------------------------------------------------------------------------

/// One TOML value in the subset `*.mods.toml` needs.
#[derive(Clone, Debug, PartialEq)]
enum TomlValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<TomlValue>),
    Table(BTreeMap<String, TomlValue>),
}

impl TomlValue {
    fn as_table(&self) -> Option<&BTreeMap<String, TomlValue>> {
        match self {
            Self::Table(t) => Some(t),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[TomlValue]> {
        match self {
            Self::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

const MAX_TOML_NESTING_DEPTH: usize = 32;

struct TomlParser<'a> {
    src: &'a [u8],
    pos: usize,
    depth: usize,
    /// Dotted paths of tables explicitly defined via `[a.b]` headers —
    /// re-defining one is a hard error (mirrors the TOML spec).
    explicit_tables: BTreeSet<String>,
}

impl<'a> TomlParser<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self {
            src,
            pos: 0,
            depth: 0,
            explicit_tables: BTreeSet::new(),
        }
    }

    fn err(&self, msg: impl fmt::Display) -> String {
        format!("TOML byte {}: {msg}", self.pos)
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn expect(&mut self, what: u8) -> Result<(), String> {
        match self.bump() {
            Some(b) if b == what => Ok(()),
            other => Err(self.err(format!(
                "expected '{}', found {}",
                what as char,
                other.map(|b| format!("'{}'", b as char)).unwrap_or_else(|| "EOF".to_string())
            ))),
        }
    }

    /// Spaces/tabs only (never newline).
    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    /// Whitespace, newlines, and `#` comments.
    fn skip_blanks(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.pos += 1,
                Some(b'#') => {
                    while !matches!(self.peek(), None | Some(b'\n')) {
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// After a value or header only trailing whitespace and an optional
    /// comment may precede the newline/EOF.
    fn end_statement(&mut self) -> Result<(), String> {
        self.skip_inline_ws();
        if self.peek() == Some(b'#') {
            while !matches!(self.peek(), None | Some(b'\n')) {
                self.pos += 1;
            }
        }
        match self.peek() {
            None | Some(b'\n') => Ok(()),
            Some(b'\r') => {
                self.pos += 1;
                if matches!(self.peek(), None | Some(b'\n')) {
                    Ok(())
                } else {
                    Err(self.err("expected newline"))
                }
            }
            Some(b) => Err(self.err(format!(
                "expected newline or comment, found '{}'",
                b as char
            ))),
        }
    }

    fn parse_keypart(&mut self) -> Result<String, String> {
        self.skip_inline_ws();
        match self.peek() {
            Some(b'"') => self.parse_basic_string(),
            Some(b'\'') => self.parse_literal_string(),
            Some(b) if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' => {
                let start = self.pos;
                while matches!(
                    self.peek(),
                    Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
                ) {
                    self.pos += 1;
                }
                Ok(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
            }
            other => Err(self.err(format!(
                "expected key, found {}",
                other
                    .map(|b| format!("'{}'", b as char))
                    .unwrap_or_else(|| "EOF".to_string())
            ))),
        }
    }

    fn parse_dotted_key(&mut self) -> Result<Vec<String>, String> {
        let mut parts = vec![self.parse_keypart()?];
        loop {
            self.skip_inline_ws();
            if self.peek() == Some(b'.') {
                if parts.len() >= MAX_TOML_NESTING_DEPTH {
                    return Err(self.err(format!(
                        "TOML dotted key exceeds maximum depth {MAX_TOML_NESTING_DEPTH}"
                    )));
                }
                self.pos += 1;
                parts.push(self.parse_keypart()?);
            } else {
                return Ok(parts);
            }
        }
    }

    fn parse_basic_string(&mut self) -> Result<String, String> {
        if self.peek_at(1) == Some(b'"') && self.peek_at(2) == Some(b'"') {
            self.pos += 3;
            return self.parse_multiline_basic();
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some(b'\n') | Some(b'\r') => {
                    return Err(self.err("unterminated basic string"))
                }
                Some(b'"') => return Ok(out),
                Some(b'\\') => self.parse_escape(&mut out)?,
                Some(b) if b < 0x20 => {
                    return Err(self.err("control character in basic string"))
                }
                Some(b) => {
                    // Collect the full UTF-8 sequence starting at b.
                    let start = self.pos - 1;
                    let len = utf8_len(b).ok_or_else(|| self.err("invalid UTF-8"))?;
                    let end = start + len;
                    if end > self.src.len() {
                        return Err(self.err("truncated UTF-8"));
                    }
                    let s = std::str::from_utf8(&self.src[start..end])
                        .map_err(|_| self.err("invalid UTF-8"))?;
                    out.push_str(s);
                    self.pos = end;
                }
            }
        }
    }

    fn parse_multiline_basic(&mut self) -> Result<String, String> {
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        } else if self.peek() == Some(b'\r') && self.peek_at(1) == Some(b'\n') {
            self.pos += 2;
        }
        let mut out = String::new();
        loop {
            if self.src[self.pos..].starts_with(b"\"\"\"") {
                self.pos += 3;
                return Ok(out);
            }
            match self.bump() {
                None => return Err(self.err("unterminated multiline basic string")),
                Some(b'\\') => {
                    // Line continuation: `\` + whitespace-to-newline trims.
                    let save = self.pos;
                    self.skip_inline_ws();
                    if matches!(self.peek(), Some(b'\r' | b'\n')) {
                        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                            self.pos += 1;
                        }
                    } else {
                        self.pos = save;
                        self.parse_escape(&mut out)?;
                    }
                }
                Some(b) => {
                    let start = self.pos - 1;
                    let len = utf8_len(b).ok_or_else(|| self.err("invalid UTF-8"))?;
                    let end = start + len;
                    if end > self.src.len() {
                        return Err(self.err("truncated UTF-8"));
                    }
                    let s = std::str::from_utf8(&self.src[start..end])
                        .map_err(|_| self.err("invalid UTF-8"))?;
                    out.push_str(s);
                    self.pos = end;
                }
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), String> {
        match self.bump() {
            Some(b'b') => out.push('\u{8}'),
            Some(b't') => out.push('\t'),
            Some(b'n') => out.push('\n'),
            Some(b'f') => out.push('\u{c}'),
            Some(b'r') => out.push('\r'),
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'u') => {
                let cp = self.parse_hex_cp(4)?;
                out.push(cp);
            }
            Some(b'U') => {
                let cp = self.parse_hex_cp(8)?;
                out.push(cp);
            }
            other => {
                return Err(self.err(format!(
                    "unsupported escape '\\{}'",
                    other
                        .map(|b| (b as char).to_string())
                        .unwrap_or_else(|| "EOF".to_string())
                )))
            }
        }
        Ok(())
    }

    fn parse_hex_cp(&mut self, digits: usize) -> Result<char, String> {
        if self.pos + digits > self.src.len() {
            return Err(self.err("truncated unicode escape"));
        }
        let hex = std::str::from_utf8(&self.src[self.pos..self.pos + digits])
            .map_err(|_| self.err("invalid unicode escape"))?;
        let v = u32::from_str_radix(hex, 16)
            .map_err(|_| self.err("invalid unicode escape"))?;
        self.pos += digits;
        char::from_u32(v).ok_or_else(|| self.err("unicode escape out of range"))
    }

    fn parse_literal_string(&mut self) -> Result<String, String> {
        if self.peek_at(1) == Some(b'\'') && self.peek_at(2) == Some(b'\'') {
            self.pos += 3;
            if self.peek() == Some(b'\n') {
                self.pos += 1;
            } else if self.peek() == Some(b'\r') && self.peek_at(1) == Some(b'\n') {
                self.pos += 2;
            }
            let start = self.pos;
            while !self.src[self.pos..].starts_with(b"'''") {
                if self.bump().is_none() {
                    return Err(self.err("unterminated multiline literal string"));
                }
            }
            let end = self.pos;
            self.pos += 3;
            return std::str::from_utf8(&self.src[start..end])
                .map(str::to_string)
                .map_err(|_| self.err("invalid UTF-8 in literal string"));
        }
        self.pos += 1;
        let start = self.pos;
        loop {
            match self.bump() {
                None | Some(b'\n') | Some(b'\r') => {
                    return Err(self.err("unterminated literal string"))
                }
                Some(b'\'') => {
                    return std::str::from_utf8(&self.src[start..self.pos - 1])
                        .map(str::to_string)
                        .map_err(|_| self.err("invalid UTF-8 in literal string"));
                }
                _ => {}
            }
        }
    }

    fn parse_number(&mut self) -> Result<TomlValue, String> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        // inf / nan literals.
        for (word, v) in [
            ("inf", f64::INFINITY),
            ("nan", f64::NAN),
        ] {
            if self.src[self.pos..].starts_with(word.as_bytes()) {
                let neg = self.src[start] == b'-';
                self.pos += word.len();
                return Ok(TomlValue::Float(if neg { -v } else { v }));
            }
        }
        // Radix-prefixed integers.
        if self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x' | b'o' | b'b')) {
            let radix = match self.peek_at(1) {
                Some(b'x') => 16,
                Some(b'o') => 8,
                _ => 2,
            };
            self.pos += 2;
            let ds = self.pos;
            while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == b'_') {
                self.pos += 1;
            }
            let digits: String = String::from_utf8_lossy(&self.src[ds..self.pos])
                .replace('_', "");
            return i64::from_str_radix(&digits, radix)
                .map(TomlValue::Int)
                .map_err(|_| self.err("invalid radix integer"));
        }
        while matches!(
            self.peek(),
            Some(c) if c.is_ascii_digit() || c == b'_'
        ) {
            self.pos += 1;
        }
        let mut is_float = false;
        if self.peek() == Some(b'.')
            && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit())
        {
            is_float = true;
            self.pos += 1;
            while matches!(
                self.peek(),
                Some(c) if c.is_ascii_digit() || c == b'_'
            ) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(
                self.peek(),
                Some(c) if c.is_ascii_digit() || c == b'_'
            ) {
                self.pos += 1;
            }
        }
        let tok = String::from_utf8_lossy(&self.src[start..self.pos]).replace('_', "");
        if is_float {
            tok.parse::<f64>()
                .map(TomlValue::Float)
                .map_err(|_| self.err(format!("invalid float '{tok}'")))
        } else {
            tok.parse::<i64>()
                .map(TomlValue::Int)
                .map_err(|_| self.err(format!("invalid integer '{tok}'")))
        }
    }

    fn parse_array(&mut self) -> Result<TomlValue, String> {
        if self.depth >= MAX_TOML_NESTING_DEPTH {
            return Err(self.err(format!(
                "TOML nesting exceeds maximum depth {MAX_TOML_NESTING_DEPTH}"
            )));
        }
        self.depth += 1;
        let res = self.parse_array_inner();
        self.depth -= 1;
        res
    }

    fn parse_array_inner(&mut self) -> Result<TomlValue, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        loop {
            self.skip_blanks();
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Ok(TomlValue::Array(items));
            }
            items.push(self.parse_value()?);
            self.skip_blanks();
            match self.bump() {
                Some(b',') => {}
                Some(b']') => return Ok(TomlValue::Array(items)),
                other => {
                    return Err(self.err(format!(
                        "expected ',' or ']' in array, found {}",
                        other
                            .map(|b| format!("'{}'", b as char))
                            .unwrap_or_else(|| "EOF".to_string())
                    )))
                }
            }
        }
    }

    fn parse_inline_table(&mut self) -> Result<TomlValue, String> {
        if self.depth >= MAX_TOML_NESTING_DEPTH {
            return Err(self.err(format!(
                "TOML nesting exceeds maximum depth {MAX_TOML_NESTING_DEPTH}"
            )));
        }
        self.depth += 1;
        let res = self.parse_inline_table_inner();
        self.depth -= 1;
        res
    }

    fn parse_inline_table_inner(&mut self) -> Result<TomlValue, String> {
        self.expect(b'{')?;
        let mut table = BTreeMap::new();
        loop {
            self.skip_inline_ws();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Ok(TomlValue::Table(table));
            }
            let key = self.parse_dotted_key()?;
            self.skip_inline_ws();
            self.expect(b'=')?;
            self.skip_inline_ws();
            let value = self.parse_value()?;
            insert_dotted(&mut table, &key, value).map_err(|e| self.err(e))?;
            self.skip_inline_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => return Ok(TomlValue::Table(table)),
                other => {
                    return Err(self.err(format!(
                        "expected ',' or '}}' in inline table, found {}",
                        other
                            .map(|b| format!("'{}'", b as char))
                            .unwrap_or_else(|| "EOF".to_string())
                    )))
                }
            }
        }
    }

    fn parse_value(&mut self) -> Result<TomlValue, String> {
        match self.peek() {
            Some(b'"') => self.parse_basic_string().map(TomlValue::Str),
            Some(b'\'') => self.parse_literal_string().map(TomlValue::Str),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_inline_table(),
            Some(b't') | Some(b'f') => {
                let start = self.pos;
                while matches!(
                    self.peek(),
                    Some(c) if c.is_ascii_alphabetic()
                ) {
                    self.pos += 1;
                }
                match &self.src[start..self.pos] {
                    b"true" => Ok(TomlValue::Bool(true)),
                    b"false" => Ok(TomlValue::Bool(false)),
                    _ => Err(self.err("unrecognized bare-word value")),
                }
            }
            Some(b'+') | Some(b'-') | Some(b'i') | Some(b'n') | Some(b'0'..=b'9') => {
                self.parse_number()
            }
            other => Err(self.err(format!(
                "expected value, found {}",
                other
                    .map(|b| format!("'{}'", b as char))
                    .unwrap_or_else(|| "EOF".to_string())
            ))),
        }
    }

    fn parse_document(&mut self) -> Result<BTreeMap<String, TomlValue>, String> {
        let mut root = BTreeMap::new();
        let mut cur_path: Vec<String> = Vec::new();
        loop {
            self.skip_blanks();
            if self.peek().is_none() {
                return Ok(root);
            }
            if self.peek() == Some(b'[') {
                if self.peek_at(1) == Some(b'[') {
                    self.pos += 2;
                    let path = self.parse_dotted_key()?;
                    self.skip_inline_ws();
                    self.expect(b']')?;
                    self.expect(b']')?;
                    let parent = table_at_path(&mut root, &path[..path.len() - 1])
                        .map_err(|e| self.err(e))?;
                    let last = path.last().unwrap().clone();
                    let entry = parent
                        .entry(last.clone())
                        .or_insert_with(|| TomlValue::Array(Vec::new()));
                    match entry {
                        TomlValue::Array(items) => items.push(TomlValue::Table(BTreeMap::new())),
                        _ => {
                            return Err(self.err(format!(
                                "'{last}' is already defined as a non-array value"
                            )))
                        }
                    }
                    cur_path = path;
                } else {
                    self.pos += 1;
                    let path = self.parse_dotted_key()?;
                    self.skip_inline_ws();
                    self.expect(b']')?;
                    let joined = path.join("\u{1}");
                    if self.explicit_tables.contains(&joined) {
                        return Err(self.err("duplicate table header"));
                    }
                    self.explicit_tables.insert(joined);
                    let parent = table_at_path(&mut root, &path[..path.len() - 1])
                        .map_err(|e| self.err(e))?;
                    let last = path.last().unwrap().clone();
                    match parent.get(&last) {
                        None => {
                            parent.insert(last, TomlValue::Table(BTreeMap::new()));
                        }
                        Some(TomlValue::Table(_)) => {}
                        Some(_) => {
                            return Err(self.err("table header conflicts with a non-table value"))
                        }
                    }
                    cur_path = path;
                }
                self.end_statement()?;
            } else {
                let key = self.parse_dotted_key()?;
                self.skip_inline_ws();
                self.expect(b'=')?;
                self.skip_inline_ws();
                let value = self.parse_value()?;
                let table = table_at_path(&mut root, &cur_path).map_err(|e| self.err(e))?;
                insert_dotted(table, &key, value).map_err(|e| self.err(e))?;
                self.end_statement()?;
            }
        }
    }
}

fn utf8_len(first: u8) -> Option<usize> {
    Some(match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    })
}

/// Walk `path` from the root, creating implicit tables as needed and
/// descending into the last element of arrays-of-tables (the `[[a]]` then
/// `[a.b]` pattern). Errors on scalar/array-of-scalars collisions.
fn table_at_path<'a>(
    mut table: &'a mut BTreeMap<String, TomlValue>,
    path: &[String],
) -> Result<&'a mut BTreeMap<String, TomlValue>, String> {
    for seg in path {
        let v = table
            .entry(seg.clone())
            .or_insert_with(|| TomlValue::Table(BTreeMap::new()));
        table = match v {
            TomlValue::Table(t) => t,
            TomlValue::Array(items) => match items.last_mut() {
                Some(TomlValue::Table(t)) => t,
                _ => {
                    return Err(format!(
                        "cannot define a table inside scalar array '{seg}'"
                    ))
                }
            },
            _ => return Err(format!("'{seg}' is already a non-table value")),
        };
    }
    Ok(table)
}

/// Insert `value` under a dotted key, creating implicit sub-tables.
fn insert_dotted(
    table: &mut BTreeMap<String, TomlValue>,
    key: &[String],
    value: TomlValue,
) -> Result<(), String> {
    let mut t = table;
    for seg in &key[..key.len() - 1] {
        let v = t
            .entry(seg.clone())
            .or_insert_with(|| TomlValue::Table(BTreeMap::new()));
        t = match v {
            TomlValue::Table(inner) => inner,
            _ => return Err(format!("'{}' is already a non-table value", seg)),
        };
    }
    let last = &key[key.len() - 1];
    if t.contains_key(last) {
        return Err(format!("duplicate key '{last}'"));
    }
    t.insert(last.clone(), value);
    Ok(())
}

fn parse_toml(text: &str) -> Result<BTreeMap<String, TomlValue>, JarScanError> {
    TomlParser::new(text.as_bytes())
        .parse_document()
        .map_err(JarScanError::Toml)
}

// ---------------------------------------------------------------------------
// fabric.mod.json extraction
// ---------------------------------------------------------------------------

struct FabricMeta {
    mod_id: String,
    version: String,
    environment: ModEnvironment,
    mc_range: Option<String>,
    mixin_configs: Vec<String>,
    entrypoints: Vec<String>,
    dependencies: Vec<ModDependency>,
}

fn json_map_str<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
) -> Option<&'a str> {
    obj.get(key).and_then(JsonValue::as_str)
}

fn valid_fabric_mod_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn parse_fabric_meta(bytes: &[u8]) -> Result<FabricMeta, JarScanError> {
    let root = parse_json(bytes).map_err(|e| {
        JarScanError::Json(format!("fabric.mod.json: {e:?}"))
    })?;
    let obj = root
        .as_object()
        .ok_or_else(|| jar_err("fabric.mod.json: root must be an object"))?;
    if let Some(sv) = obj.get("schemaVersion") {
        match sv.as_u64() {
            Some(1) => {}
            Some(v) => {
                return Err(jar_err(format!(
                    "fabric.mod.json: unsupported schemaVersion {v} (expected 1)"
                )))
            }
            None => return Err(jar_err("fabric.mod.json: schemaVersion must be 1")),
        }
    }
    let mod_id = json_map_str(obj, "id")
        .ok_or_else(|| jar_err("fabric.mod.json: missing string 'id'"))?
        .to_string();
    if !valid_fabric_mod_id(&mod_id) {
        return Err(jar_err(format!(
            "fabric.mod.json: invalid mod id '{mod_id}'"
        )));
    }
    let version = json_map_str(obj, "version")
        .ok_or_else(|| jar_err("fabric.mod.json: missing string 'version'"))?
        .to_string();
    let environment = match json_map_str(obj, "environment") {
        None | Some("*") | Some("both") => ModEnvironment::Both,
        Some("client") => ModEnvironment::Client,
        Some("server") => ModEnvironment::Server,
        Some(other) => {
            return Err(jar_err(format!(
                "fabric.mod.json: unknown environment '{other}'"
            )))
        }
    };

    let mut dependencies = Vec::new();
    let mut mc_range = None;
    for (table_key, required) in [("depends", true), ("recommends", false)] {
        let Some(table) = obj.get(table_key) else { continue };
        let Some(table) = table.as_object() else {
            return Err(jar_err(format!(
                "fabric.mod.json: '{table_key}' must be an object"
            )));
        };
        for (dep_id, dep_val) in table {
            let Some(range) = dep_val.as_str() else {
                return Err(jar_err(format!(
                    "fabric.mod.json: '{table_key}.{dep_id}' must be a version string"
                )));
            };
            if dep_id == "minecraft" {
                mc_range = Some(range.to_string());
            }
            dependencies.push(ModDependency {
                mod_id: dep_id.clone(),
                version_range: Some(range.to_string()),
                required,
            });
        }
    }

    let mut entrypoints = Vec::new();
    if let Some(ep) = obj.get("entrypoints") {
        let Some(ep_obj) = ep.as_object() else {
            return Err(jar_err("fabric.mod.json: 'entrypoints' must be an object"));
        };
        for (ep_name, list) in ep_obj {
            let Some(items) = list.as_array() else {
                return Err(jar_err(format!(
                    "fabric.mod.json: entrypoints.{ep_name} must be an array"
                )));
            };
            for item in items {
                if let Some(s) = item.as_str() {
                    entrypoints.push(s.to_string());
                } else if let Some(v) = item
                    .as_object()
                    .and_then(|o| o.get("value"))
                    .and_then(JsonValue::as_str)
                {
                    entrypoints.push(v.to_string());
                } else {
                    return Err(jar_err(format!(
                        "fabric.mod.json: bad entrypoint entry under '{ep_name}'"
                    )));
                }
            }
        }
    }

    let mut mixin_configs = Vec::new();
    match obj.get("mixins") {
        None => {}
        Some(JsonValue::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    mixin_configs.push(s.to_string());
                } else if let Some(cfg) = item
                    .as_object()
                    .and_then(|o| o.get("config"))
                    .and_then(JsonValue::as_str)
                {
                    mixin_configs.push(cfg.to_string());
                } else {
                    return Err(jar_err(
                        "fabric.mod.json: bad 'mixins' entry (expected string or {config})",
                    ));
                }
            }
        }
        // `"mixins": "mod.mixins.json"` shorthand.
        Some(JsonValue::String(s)) => mixin_configs.push(s.clone()),
        Some(_) => {
            return Err(jar_err(
                "fabric.mod.json: 'mixins' must be an array or string",
            ))
        }
    }

    Ok(FabricMeta {
        mod_id,
        version,
        environment,
        mc_range,
        mixin_configs,
        entrypoints,
        dependencies,
    })
}

// ---------------------------------------------------------------------------
// neoforge.mods.toml / mods.toml extraction
// ---------------------------------------------------------------------------

struct ForgeMeta {
    mod_ids: Vec<String>,
    version: String,
    environment: ModEnvironment,
    mc_range: Option<String>,
    mixin_configs: Vec<String>,
    dependencies: Vec<ModDependency>,
}

fn toml_req_str<'a>(
    table: &'a BTreeMap<String, TomlValue>,
    key: &str,
    ctx: &str,
) -> Result<&'a str, JarScanError> {
    table
        .get(key)
        .and_then(TomlValue::as_str)
        .ok_or_else(|| jar_err(format!("{ctx}: missing string '{key}'")))
}

fn parse_forge_meta(bytes: &[u8], ctx: &str) -> Result<ForgeMeta, JarScanError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| JarScanError::Toml(format!("{ctx}: manifest is not UTF-8")))?;
    let root = parse_toml(text)?;
    let mut meta = ForgeMeta {
        mod_ids: Vec::new(),
        version: String::new(),
        environment: ModEnvironment::Both,
        mc_range: None,
        mixin_configs: Vec::new(),
        dependencies: Vec::new(),
    };

    let mods = root
        .get("mods")
        .and_then(TomlValue::as_array)
        .ok_or_else(|| jar_err(format!("{ctx}: missing '[[mods]]' table")))?;
    for m in mods {
        let Some(mt) = m.as_table() else {
            return Err(jar_err(format!("{ctx}: '[[mods]]' entry must be a table")));
        };
        let mod_id = toml_req_str(mt, "modId", ctx)?.to_string();
        if meta.mod_ids.is_empty() {
            if let Some(v) = mt.get("version").and_then(TomlValue::as_str) {
                meta.version = v.to_string();
            }
        }
        meta.mod_ids.push(mod_id);
    }
    if meta.mod_ids.is_empty() {
        return Err(jar_err(format!("{ctx}: '[[mods]]' declares no mod")));
    }

    if let Some(mixins) = root.get("mixins").and_then(TomlValue::as_array) {
        for (i, m) in mixins.iter().enumerate() {
            let Some(mt) = m.as_table() else {
                return Err(jar_err(format!("{ctx}: '[[mixins]]' entry {i} must be a table")));
            };
            meta.mixin_configs
                .push(toml_req_str(mt, "config", &format!("{ctx}: mixins[{i}]"))?.to_string());
        }
    }

    if let Some(deps_root) = root.get("dependencies").and_then(TomlValue::as_table) {
        for (owner_id, dep_list) in deps_root {
            let Some(entries) = dep_list.as_array() else {
                return Err(jar_err(format!(
                    "{ctx}: 'dependencies.{owner_id}' must be an array of tables"
                )));
            };
            for (i, entry) in entries.iter().enumerate() {
                let dctx = format!("{ctx}: dependencies.{owner_id}[{i}]");
                let Some(dt) = entry.as_table() else {
                    return Err(jar_err(format!("{dctx}: must be a table")));
                };
                let dep_id = toml_req_str(dt, "modId", &dctx)?.to_string();
                let dep_type = dt
                    .get("type")
                    .and_then(TomlValue::as_str)
                    .unwrap_or("required");
                let required = match dep_type {
                    "required" => true,
                    "optional" => false,
                    // Conflicts/discs are not load requirements.
                    "incompatible" | "disc" => continue,
                    other => {
                        return Err(jar_err(format!("{dctx}: unknown type '{other}'")))
                    }
                };
                // Legacy `mandatory = false` overrides `required`.
                let required = match dt.get("mandatory").and_then(TomlValue::as_bool) {
                    Some(false) => false,
                    _ => required,
                };
                let version_range = dt
                    .get("versionRange")
                    .and_then(TomlValue::as_str)
                    .map(str::to_string);
                let side = dt
                    .get("side")
                    .and_then(TomlValue::as_str)
                    .unwrap_or("BOTH");
                if dep_id == "minecraft" {
                    meta.mc_range = version_range.clone();
                    meta.environment = match side {
                        "BOTH" => ModEnvironment::Both,
                        "CLIENT" => ModEnvironment::Client,
                        "SERVER" => ModEnvironment::Server,
                        other => {
                            return Err(jar_err(format!("{dctx}: unknown side '{other}'")))
                        }
                    };
                }
                meta.dependencies.push(ModDependency {
                    mod_id: dep_id,
                    version_range,
                    required,
                });
            }
        }
    }
    Ok(meta)
}

// ---------------------------------------------------------------------------
// JAR scan + dual-loader plan
// ---------------------------------------------------------------------------

fn merge_environment(a: ModEnvironment, b: ModEnvironment) -> ModEnvironment {
    match (a, b) {
        (ModEnvironment::Client, _) | (_, ModEnvironment::Client) => ModEnvironment::Client,
        (ModEnvironment::Server, _) | (_, ModEnvironment::Server) => ModEnvironment::Server,
        _ => ModEnvironment::Both,
    }
}

fn merge_dependencies(
    a: Vec<ModDependency>,
    b: Vec<ModDependency>,
) -> Vec<ModDependency> {
    let mut out: Vec<ModDependency> = Vec::new();
    for dep in a.into_iter().chain(b) {
        match out.iter_mut().find(|d| d.mod_id == dep.mod_id) {
            Some(existing) => {
                existing.required |= dep.required;
                if existing.version_range.is_none() {
                    existing.version_range = dep.version_range;
                }
            }
            None => out.push(dep),
        }
    }
    out
}

fn dedup_keep_order(list: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    list.into_iter().filter(|s| seen.insert(s.clone())).collect()
}

/// Scan one complete mod JAR in memory, reading `fabric.mod.json` and
/// `META-INF/neoforge.mods.toml` / `META-INF/mods.toml` through
/// [`crate::modpack`]'s fail-closed ZIP reader (traversal, zip-bomb,
/// truncated, encrypted, and CRC-corrupt members all reject).
///
/// Returns [`JarScanError::Manifest`] when the archive carries no loader
/// manifest at all *and* no `pack.mcmeta`/`data/` datapack evidence — a
/// bare unknown JAR is never silently trusted.
pub fn scan_mod_jar(jar_bytes: &[u8]) -> Result<ModJarDescriptor, JarScanError> {
    scan_mod_jar_named(jar_bytes, "mod JAR")
}

/// [`scan_mod_jar`] with an operator-facing display name used in errors.
pub fn scan_mod_jar_named(
    jar_bytes: &[u8],
    display: &str,
) -> Result<ModJarDescriptor, JarScanError> {
    let entries = modpack::zip_member_list(jar_bytes, display)
        .map_err(|e| JarScanError::Zip(format!("{display}: {}", e.reason())))?;

    let mut fabric_member = None;
    let mut neoforge_member = None;
    let mut forge_member = None;
    let mut has_pack_mcmeta = false;
    let mut has_data_dir = false;
    for entry in &entries {
        match entry.name.as_str() {
            "fabric.mod.json" => fabric_member = Some(entry),
            "META-INF/neoforge.mods.toml" => neoforge_member = Some(entry),
            "META-INF/mods.toml" => forge_member = Some(entry),
            "pack.mcmeta" => has_pack_mcmeta = true,
            _ => {
                if entry.name.starts_with("data/") {
                    has_data_dir = true;
                }
            }
        }
    }

    let read = |entry: &modpack::ZipMemberEntry| -> Result<Vec<u8>, JarScanError> {
        modpack::read_zip_member(jar_bytes, entry, display)
            .map_err(|e| JarScanError::Zip(format!("{display}: {}", e.reason())))
    };

    let fabric = fabric_member
        .map(|e| read(e).and_then(|b| parse_fabric_meta(&b)))
        .transpose()?;
    let forge_file = neoforge_member.is_some();
    let forge = neoforge_member
        .or(forge_member)
        .map(|e| {
            read(e).and_then(|b| {
                parse_forge_meta(&b, if forge_file {
                    "neoforge.mods.toml"
                } else {
                    "mods.toml"
                })
            })
        })
        .transpose()?;

    if fabric.is_none() && forge.is_none() {
        if has_pack_mcmeta || has_data_dir {
            return Ok(ModJarDescriptor {
                loader: ModLoaderKind::DatapackOnly,
                mod_id: String::new(),
                declared_mod_ids: Vec::new(),
                version: String::new(),
                environment: ModEnvironment::Both,
                declared_mc_range: None,
                mixin_configs: Vec::new(),
                entrypoints: Vec::new(),
                dependencies: Vec::new(),
            });
        }
        return Err(jar_err(format!(
            "{display}: no loader manifest (fabric.mod.json / META-INF/*.mods.toml) and no datapack evidence"
        )));
    }

    let loader = match (&fabric, &forge, forge_file) {
        (Some(_), Some(_), _) => ModLoaderKind::DualFabricNeoForge,
        (Some(_), None, _) => ModLoaderKind::Fabric,
        (None, Some(_), true) => ModLoaderKind::NeoForge,
        (None, Some(_), false) => ModLoaderKind::ForgeLegacy,
        (None, None, _) => unreachable!(),
    };

    let fab_id = fabric.as_ref().map(|f| f.mod_id.as_str());
    let mut declared_mod_ids = Vec::new();
    if let Some(id) = fab_id {
        declared_mod_ids.push(id.to_string());
    }
    if let Some(f) = &forge {
        declared_mod_ids.extend(
            f.mod_ids
                .iter()
                .filter(|id| Some(id.as_str()) != fab_id)
                .cloned(),
        );
    }
    let declared_mod_ids = dedup_keep_order(declared_mod_ids);

    let mod_id = fabric
        .as_ref()
        .map(|f| f.mod_id.clone())
        .or_else(|| forge.as_ref().and_then(|f| f.mod_ids.first().cloned()))
        .unwrap_or_default();
    let version = fabric
        .as_ref()
        .map(|f| f.version.clone())
        .filter(|v| !v.is_empty())
        .or_else(|| forge.as_ref().map(|f| f.version.clone()))
        .unwrap_or_default();
    let environment = merge_environment(
        fabric
            .as_ref()
            .map(|f| f.environment)
            .unwrap_or(ModEnvironment::Both),
        forge
            .as_ref()
            .map(|f| f.environment)
            .unwrap_or(ModEnvironment::Both),
    );
    let declared_mc_range = match fabric
        .as_ref()
        .and_then(|f| f.mc_range.clone())
        .or_else(|| forge.as_ref().and_then(|f| f.mc_range.clone()))
    {
        Some(declared) => Some(VersionRange::parse(&declared).map_err(|e| {
            jar_err(format!(
                "{display}: declared Minecraft range '{declared}' is invalid ({})",
                e.reason()
            ))
        })?),
        None => None,
    };
    let mixin_configs = dedup_keep_order(
        fabric
            .as_ref()
            .map(|f| f.mixin_configs.clone())
            .unwrap_or_default()
            .into_iter()
            .chain(
                forge
                    .as_ref()
                    .map(|f| f.mixin_configs.clone())
                    .unwrap_or_default(),
            )
            .collect(),
    );
    let dependencies = merge_dependencies(
        fabric
            .as_ref()
            .map(|f| f.dependencies.clone())
            .unwrap_or_default(),
        forge
            .as_ref()
            .map(|f| f.dependencies.clone())
            .unwrap_or_default(),
    );

    Ok(ModJarDescriptor {
        loader,
        mod_id,
        declared_mod_ids,
        version,
        environment,
        declared_mc_range,
        mixin_configs,
        entrypoints: fabric.map(|f| f.entrypoints).unwrap_or_default(),
        dependencies,
    })
}

// ---------------------------------------------------------------------------
// Dual-loader coexistence planner
// ---------------------------------------------------------------------------

/// One mod (or datapack) in the merged table.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedMod {
    /// `mods/` entry file name the descriptor was scanned from.
    pub jar_name: String,
    /// Effective mod id (manifest id, else inferred from the file name).
    pub mod_id: String,
    /// Every id the JAR claims.
    pub declared_mod_ids: Vec<String>,
    pub loader: ModLoaderKind,
    pub version: String,
    pub environment: ModEnvironment,
    pub declared_mc_range: Option<VersionRange>,
    /// `*.mixins.json` configs handed to
    /// [`crate::class_mixin_scanner`] → [`crate::mixin_lifter`].
    pub mixin_configs: Vec<String>,
    pub entrypoints: Vec<String>,
    pub dependencies: Vec<ModDependency>,
}

/// A JAR that could not be scanned at all (corrupt archive, bad manifest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedJar {
    pub jar_name: String,
    pub reason: String,
}

/// One mod id claimed by more than one live JAR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateModId {
    pub mod_id: String,
    /// Every conflicting `mods/` file name, first claimant first.
    pub jar_names: Vec<String>,
}

/// A required dependency absent from the unified table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDependency {
    /// Mod requiring the dependency.
    pub mod_id: String,
    /// The dependency id no live JAR provides.
    pub required_mod_id: String,
    pub version_range: Option<String>,
}

/// Result of planning a mixed Fabric + NeoForge `mods/` directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DualLoaderPlan {
    /// Merged native/Tier-2 mod table, sorted by mod id. Contains both
    /// [`ModLoaderKind::Fabric`] and [`ModLoaderKind::NeoForge`] entries —
    /// they coexist in one table; their `mixin_configs` (Intermediary for
    /// Fabric, Mojang mappings for NeoForge) feed
    /// [`crate::mixin_lifter`] transparently.
    pub unified_mods: Vec<PlannedMod>,
    /// Client-only mods skipped on this dedicated server.
    pub skipped_client_only: Vec<PlannedMod>,
    /// Perf mods skipped because [`crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS`]
    /// already implements them natively.
    pub skipped_superseded: Vec<PlannedMod>,
    /// Archives that failed the fail-closed scan (corrupt ZIP, bad
    /// manifest) — reported, never silently dropped.
    pub rejected_jars: Vec<RejectedJar>,
    /// Mod ids claimed by more than one live JAR (the first claimant in
    /// sorted order stays in `unified_mods`).
    pub duplicate_conflicts: Vec<DuplicateModId>,
    /// Required dependencies no live JAR provides.
    pub missing_dependencies: Vec<MissingDependency>,
}

impl DualLoaderPlan {
    /// Every mixin config the unified table declares, deduplicated in plan
    /// order — the names [`crate::class_mixin_scanner`] looks up.
    pub fn all_mixin_configs(&self) -> Vec<String> {
        dedup_keep_order(
            self.unified_mods
                .iter()
                .flat_map(|m| m.mixin_configs.clone())
                .collect(),
        )
    }

    /// Mod ids present in the unified table (primary ids).
    pub fn unified_mod_ids(&self) -> Vec<&str> {
        self.unified_mods.iter().map(|m| m.mod_id.as_str()).collect()
    }
}

fn planned_from(
    jar_name: &str,
    desc: &ModJarDescriptor,
    declared_ids: Vec<String>,
    effective_id: String,
) -> PlannedMod {
    PlannedMod {
        jar_name: jar_name.to_string(),
        mod_id: effective_id,
        declared_mod_ids: declared_ids,
        loader: desc.loader,
        version: desc.version.clone(),
        environment: desc.environment,
        declared_mc_range: desc.declared_mc_range.clone(),
        mixin_configs: desc.mixin_configs.clone(),
        entrypoints: desc.entrypoints.clone(),
        dependencies: desc.dependencies.clone(),
    }
}

/// Merge a `mods/` directory holding any mix of Fabric and NeoForge/Forge
/// JARs into one unified native/Tier-2 mod table.
///
/// `jars` is `(file_name, jar_bytes)` pairs — real ZIP/JAR bytes as read
/// from disk; each is scanned fail-closed by [`scan_mod_jar`]. The plan:
/// - keeps loadable server-side mods (and datapack archives) in
///   `unified_mods`, sorted by mod id;
/// - skips `environment = "client"` mods into `skipped_client_only`;
/// - skips [`crate::fallback::BUILTIN_SUPERSEDED_PERF_MODS`] into
///   `skipped_superseded` *before* any other judgment, exactly like the
///   tier classifier;
/// - reports corrupt/unscannable archives in `rejected_jars` (fail closed
///   — a bad JAR is loudly listed, never silently skipped);
/// - records mod ids claimed by multiple live JARs in
///   `duplicate_conflicts` (first claimant in file-name order wins);
/// - flags required dependencies no live JAR provides in
///   `missing_dependencies` (loader/game ids in
///   [`RUNTIME_PROVIDED_MOD_IDS`] are always satisfied).
pub fn plan_dual_loader_mods_dir<'a, I>(jars: I) -> DualLoaderPlan
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut jars: Vec<(&str, &[u8])> = jars.into_iter().collect();
    jars.sort_by(|a, b| a.0.cmp(b.0));

    let mut plan = DualLoaderPlan::default();
    let mut candidates: Vec<PlannedMod> = Vec::new();

    for (name, bytes) in jars {
        let desc = match scan_mod_jar_named(bytes, name) {
            Ok(d) => d,
            Err(e) => {
                plan.rejected_jars.push(RejectedJar {
                    jar_name: name.to_string(),
                    reason: e.reason().to_string(),
                });
                continue;
            }
        };
        let declared_ids = if desc.declared_mod_ids.is_empty() {
            modpack::infer_candidate_mod_id(name).into_iter().collect()
        } else {
            desc.declared_mod_ids.clone()
        };
        let effective_id = if !desc.mod_id.is_empty() {
            desc.mod_id.clone()
        } else {
            modpack::infer_candidate_mod_id(name).unwrap_or_else(|| name.to_string())
        };
        let planned = planned_from(name, &desc, declared_ids, effective_id);
        if desc.environment == ModEnvironment::Client {
            plan.skipped_client_only.push(planned);
        } else if planned
            .declared_mod_ids
            .iter()
            .any(|id| is_builtin_superseded_mod(id))
        {
            plan.skipped_superseded.push(planned);
        } else {
            candidates.push(planned);
        }
    }

    // Duplicate detection: every declared id of a surviving candidate is
    // claimed; a later claimant loses its whole JAR to the conflict (fail
    // closed — aliases cannot silently merge two different mods).
    let mut claimed: BTreeMap<String, usize> = BTreeMap::new();
    let mut keep = vec![true; candidates.len()];
    for (i, cand) in candidates.iter().enumerate() {
        for id in &cand.declared_mod_ids {
            match claimed.get(id) {
                Some(&first) => {
                    keep[i] = false;
                    let first_jar = candidates[first].jar_name.clone();
                    match plan
                        .duplicate_conflicts
                        .iter_mut()
                        .find(|c| c.mod_id == *id)
                    {
                        Some(c) => {
                            if !c.jar_names.contains(&cand.jar_name) {
                                c.jar_names.push(cand.jar_name.clone());
                            }
                        }
                        None => plan.duplicate_conflicts.push(DuplicateModId {
                            mod_id: id.clone(),
                            jar_names: vec![first_jar, cand.jar_name.clone()],
                        }),
                    }
                }
                None => {
                    claimed.insert(id.clone(), i);
                }
            }
        }
    }
    plan.unified_mods = candidates
        .into_iter()
        .zip(keep)
        .filter_map(|(m, k)| k.then_some(m))
        .collect();
    plan.unified_mods.sort_by(|a, b| a.mod_id.cmp(&b.mod_id));
    plan.duplicate_conflicts
        .sort_by(|a, b| a.mod_id.cmp(&b.mod_id));

    let provided: BTreeSet<&str> = plan
        .unified_mods
        .iter()
        .flat_map(|m| m.declared_mod_ids.iter().map(String::as_str))
        .collect();
    for m in &plan.unified_mods {
        for dep in &m.dependencies {
            if !dep.required || is_runtime_provided_mod(&dep.mod_id) {
                continue;
            }
            if !provided.contains(dep.mod_id.as_str()) {
                plan.missing_dependencies.push(MissingDependency {
                    mod_id: m.mod_id.clone(),
                    required_mod_id: dep.mod_id.clone(),
                    version_range: dep.version_range.clone(),
                });
            }
        }
    }
    plan.missing_dependencies
        .sort_by(|a, b| (&a.mod_id, &a.required_mod_id).cmp(&(&b.mod_id, &b.required_mod_id)));

    plan
}

// SPDX-License-Identifier: LGPL-3.0-or-later
//! Phase 1 Seamless Dimension Transfer (`megu:portal_seamless_v1`, `MSP1`).
//!
//! A zero-dependency wire contract plus two sans-IO state machines that let a
//! supported client cross a dimension portal with **no "Loading Terrain"
//! black screen**: the server prefetches destination chunks into a standby
//! cache while the player is still approaching the portal, then commits the
//! dimension swap in a single frame while the atmosphere crossfades.
//!
//! ## `MSP1` wire shape
//!
//! Every packet is `b"MSP1"` + `version(1)` + `tag(1)` + a big-endian field
//! body. Dimension ids are `VarInt(len) + UTF-8` capped at 128 *bytes*;
//! every float must be finite; `prefetch_radius` is capped at
//! [`MAX_PREFETCH_RADIUS`] so a single portal can never request more than
//! `(2*3+1)^2 = 49` chunks — the standby cache is strictly RAM-bounded.
//! Wrong magic, wrong version, truncated bodies, trailing garbage, forged
//! lengths and non-finite floats all fail closed through
//! [`SeamlessPortalError`]; nothing panics on attacker-controlled input.
//!
//! * tag `0x01` [`SeamlessPortalPacket::PrefetchHint`] — S2C: player is
//!   within range of a portal; warm destination standby chunks in the
//!   background without disturbing the current world.
//! * tag `0x02` [`SeamlessPortalPacket::PrefetchCancel`] — S2C: player left
//!   the hysteresis band; release the standby prefetch cache.
//! * tag `0x03` [`SeamlessPortalPacket::CommitTransfer`] — S2C: swap the
//!   active dimension in one frame with preserved momentum and a smooth
//!   sky/fog crossfade.
//! * tag `0x04` [`SeamlessPortalPacket::PrefetchAck`] — C2S: how many
//!   destination chunks are hot in standby RAM.
//!
//! ## Layers
//!
//! * [`PortalPrefetchHint`], [`SeamlessDimCommit`],
//!   [`encode_seamless_portal_packet`]/[`decode_seamless_portal_packet`] —
//!   the codec.
//! * [`resolve_portal_destination`] + [`enumerate_prefetch_chunks`] — the
//!   generic dimension/portal coordinate mapper (vanilla 1:8 nether scaling,
//!   the End platform, and arbitrary mod dimensions via
//!   [`PortalKind::Custom`]) and the nearest-first chunk planner.
//! * [`PortalProximityTracker`] — server-side tick-driven proximity gate
//!   with trigger/cancel hysteresis and a Vanilla-client fallback flag.
//! * [`SeamlessDimTransitionState`] — client-side standby-prefetch +
//!   crossfade state machine driving
//!   [`SeamlessDimTransitionState::blended_sky_and_fog`] and
//!   [`SeamlessDimTransitionState::should_suppress_loading_screen`].
//!
//! Everything is tick/`Instant`-free on the server side: the tracker takes
//! `now_tick: u64` from the caller, so no `SystemTime` appears anywhere.

use std::collections::BTreeSet;
use std::fmt;

use crate::wire::{ProtoError, WireReader, WireWriter};

/// Plugin-message channel carrying `MSP1` packets.
pub const SEAMLESS_PORTAL_CHANNEL: &str = "megu:portal_seamless_v1";

/// Frame magic: `b"MSP1"`.
pub const SEAMLESS_PORTAL_MAGIC: [u8; 4] = *b"MSP1";

/// Wire version. Only `1` is defined; anything else fails closed.
pub const SEAMLESS_PORTAL_VERSION: u8 = 1;

/// Hard cap on `prefetch_radius`: at most `(2*3+1)^2 = 49` standby chunks
/// per portal, keeping client RAM strictly bounded.
pub const MAX_PREFETCH_RADIUS: u8 = 3;

/// Upper bound on a crossfade duration in milliseconds.
pub const MAX_CROSSFADE_MS: u16 = 1500;

/// Default crossfade duration when a commit does not specify one.
pub const DEFAULT_CROSSFADE_MS: u16 = 280;

/// Vanilla world border on X/Z; scaled portal coordinates clamp here.
pub const WORLD_BORDER_LIMIT: f64 = 29_999_872.0;

/// Fallback landing for End → Overworld transfers before the world's real
/// spawn point is resolved (world spawn is server state, not wire state).
pub const OVERWORLD_SPAWN_FALLBACK: [f64; 3] = [0.0, 64.0, 0.0];

/// Hard cap on a dimension id's *byte* length on the wire.
pub const MAX_DIM_ID_BYTES: usize = 128;

/// Packet discriminants inside the `MSP1` frame.
const TAG_PREFETCH_HINT: u8 = 0x01;
const TAG_PREFETCH_CANCEL: u8 = 0x02;
const TAG_COMMIT_TRANSFER: u8 = 0x03;
const TAG_PREFETCH_ACK: u8 = 0x04;

/// Every failure the `MSP1` codec or helpers can produce. Wire-level
/// truncation/UTF-8/length failures surface as [`Self::Wire`]; everything
/// semantic to this protocol has its own variant.
#[derive(Clone, Debug, PartialEq)]
pub enum SeamlessPortalError {
    /// Frame did not start with `b"MSP1"`.
    BadMagic,
    /// Frame version byte was not [`SEAMLESS_PORTAL_VERSION`].
    UnsupportedVersion(u8),
    /// Packet tag byte has no defined meaning.
    UnknownTag(u8),
    /// A float field decoded to `NaN` or an infinity.
    NonFiniteFloat(&'static str),
    /// `target_dim_id` exceeded [`MAX_DIM_ID_BYTES`] bytes.
    DimIdTooLong { bytes: usize, max: usize },
    /// `prefetch_radius` exceeded [`MAX_PREFETCH_RADIUS`].
    RadiusOutOfBounds { radius: u8, max: u8 },
    /// Low-level wire failure (truncation, bad UTF-8, forged lengths).
    Wire(ProtoError),
}

impl fmt::Display for SeamlessPortalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "bad MSP1 magic"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported MSP1 version {v}"),
            Self::UnknownTag(t) => write!(f, "unknown MSP1 packet tag {t:#04x}"),
            Self::NonFiniteFloat(field) => write!(f, "non-finite float in {field}"),
            Self::DimIdTooLong { bytes, max } => {
                write!(f, "dimension id of {bytes} bytes exceeds {max}")
            }
            Self::RadiusOutOfBounds { radius, max } => {
                write!(f, "prefetch radius {radius} exceeds {max}")
            }
            Self::Wire(e) => write!(f, "wire error: {e}"),
        }
    }
}

impl std::error::Error for SeamlessPortalError {}

impl From<ProtoError> for SeamlessPortalError {
    fn from(e: ProtoError) -> Self {
        Self::Wire(e)
    }
}

/// S2C hint sent when a player approaches a portal: warm the destination
/// dimension's standby chunks in the background without disturbing the
/// current world.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalPrefetchHint {
    /// Correlates hint → cancel → commit → ack for one approach.
    pub transfer_id: u64,
    /// Destination dimension id (e.g. `"minecraft:the_nether"`).
    pub target_dim_id: String,
    /// Destination chunk the player will land in — prefetched FIRST.
    pub target_center_chunk: [i32; 2],
    /// Chunk prefetch radius, capped at [`MAX_PREFETCH_RADIUS`].
    pub prefetch_radius: u8,
    /// Portal block position in the source dimension.
    pub portal_pos: [f64; 3],
    /// Intended spawn point in the destination dimension.
    pub target_spawn: [f64; 3],
    pub target_yaw: f32,
    pub target_pitch: f32,
    /// Destination sky color, used for the commit crossfade.
    pub target_sky_rgb: [f32; 3],
    /// Destination fog color, used for the commit crossfade.
    pub target_fog_rgb: [f32; 3],
}

/// S2C commit: swap the active dimension in a single frame, preserving
/// momentum and crossfading the atmosphere — zero "Loading Terrain" screen.
#[derive(Clone, Debug, PartialEq)]
pub struct SeamlessDimCommit {
    pub transfer_id: u64,
    pub target_dim_id: String,
    /// Landing position in the destination dimension.
    pub landing_pos: [f64; 3],
    /// Landing velocity; combined with `keep_momentum` for momentum carry.
    pub landing_vel: [f32; 3],
    pub landing_yaw: f32,
    pub landing_pitch: f32,
    /// Crossfade duration in milliseconds, capped at [`MAX_CROSSFADE_MS`].
    pub crossfade_ms: u16,
    /// Whether horizontal momentum carries through the portal.
    pub keep_momentum: bool,
}

/// The four `MSP1` packet variants. Direction is implied by the variant:
/// hints/cancels/commits are S2C, acks are C2S.
#[derive(Clone, Debug, PartialEq)]
pub enum SeamlessPortalPacket {
    PrefetchHint(PortalPrefetchHint),
    PrefetchCancel {
        /// The [`PortalPrefetchHint::transfer_id`] being released.
        transfer_id: u64,
    },
    CommitTransfer(SeamlessDimCommit),
    PrefetchAck {
        transfer_id: u64,
        ready_chunks: u16,
        total_chunks: u16,
    },
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

fn sanitize_f64(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn sanitize_f32(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn sanitize_f64x3(v: [f64; 3]) -> [f64; 3] {
    [sanitize_f64(v[0]), sanitize_f64(v[1]), sanitize_f64(v[2])]
}

fn sanitize_f32x3(v: [f32; 3]) -> [f32; 3] {
    [sanitize_f32(v[0]), sanitize_f32(v[1]), sanitize_f32(v[2])]
}

/// Truncate `s` to at most [`MAX_DIM_ID_BYTES`] bytes on a UTF-8 boundary so
/// encoding can never produce a packet the decoder would reject.
fn sanitize_dim_id(s: &str) -> &str {
    if s.len() <= MAX_DIM_ID_BYTES {
        return s;
    }
    let mut end = MAX_DIM_ID_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn write_dim_id(w: &mut WireWriter, s: &str) {
    w.write_bytes(s.as_bytes());
}

fn write_f64x3(w: &mut WireWriter, v: [f64; 3]) {
    for x in v {
        w.write_f64(sanitize_f64(x));
    }
}

fn write_f32x3(w: &mut WireWriter, v: [f32; 3]) {
    for x in v {
        w.write_f32(sanitize_f32(x));
    }
}

fn read_f64_checked(r: &mut WireReader<'_>, field: &'static str) -> Result<f64, SeamlessPortalError> {
    let v = r.read_f64()?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(SeamlessPortalError::NonFiniteFloat(field))
    }
}

fn read_f32_checked(r: &mut WireReader<'_>, field: &'static str) -> Result<f32, SeamlessPortalError> {
    let v = r.read_f32()?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(SeamlessPortalError::NonFiniteFloat(field))
    }
}

fn read_f64x3(r: &mut WireReader<'_>, field: &'static str) -> Result<[f64; 3], SeamlessPortalError> {
    Ok([
        read_f64_checked(r, field)?,
        read_f64_checked(r, field)?,
        read_f64_checked(r, field)?,
    ])
}

fn read_f32x3(r: &mut WireReader<'_>, field: &'static str) -> Result<[f32; 3], SeamlessPortalError> {
    Ok([
        read_f32_checked(r, field)?,
        read_f32_checked(r, field)?,
        read_f32_checked(r, field)?,
    ])
}

fn read_dim_id(r: &mut WireReader<'_>) -> Result<String, SeamlessPortalError> {
    let raw = r.read_bytes(MAX_DIM_ID_BYTES)?;
    let text = std::str::from_utf8(raw).map_err(|_| ProtoError::InvalidUtf8)?;
    Ok(text.to_owned())
}

fn encode_prefetch_hint(w: &mut WireWriter, h: &PortalPrefetchHint) {
    w.write_u64(h.transfer_id);
    write_dim_id(w, sanitize_dim_id(&h.target_dim_id));
    w.write_i32(h.target_center_chunk[0]);
    w.write_i32(h.target_center_chunk[1]);
    w.write_u8(h.prefetch_radius.min(MAX_PREFETCH_RADIUS));
    write_f64x3(w, sanitize_f64x3(h.portal_pos));
    write_f64x3(w, sanitize_f64x3(h.target_spawn));
    w.write_f32(sanitize_f32(h.target_yaw));
    w.write_f32(sanitize_f32(h.target_pitch));
    write_f32x3(w, sanitize_f32x3(h.target_sky_rgb));
    write_f32x3(w, sanitize_f32x3(h.target_fog_rgb));
}

fn decode_prefetch_hint(r: &mut WireReader<'_>) -> Result<PortalPrefetchHint, SeamlessPortalError> {
    let transfer_id = r.read_u64()?;
    let target_dim_id = read_dim_id(r)?;
    let target_center_chunk = [r.read_i32()?, r.read_i32()?];
    let prefetch_radius = r.read_u8()?;
    if prefetch_radius > MAX_PREFETCH_RADIUS {
        return Err(SeamlessPortalError::RadiusOutOfBounds {
            radius: prefetch_radius,
            max: MAX_PREFETCH_RADIUS,
        });
    }
    let portal_pos = read_f64x3(r, "portal_pos")?;
    let target_spawn = read_f64x3(r, "target_spawn")?;
    let target_yaw = read_f32_checked(r, "target_yaw")?;
    let target_pitch = read_f32_checked(r, "target_pitch")?;
    let target_sky_rgb = read_f32x3(r, "target_sky_rgb")?;
    let target_fog_rgb = read_f32x3(r, "target_fog_rgb")?;
    Ok(PortalPrefetchHint {
        transfer_id,
        target_dim_id,
        target_center_chunk,
        prefetch_radius,
        portal_pos,
        target_spawn,
        target_yaw,
        target_pitch,
        target_sky_rgb,
        target_fog_rgb,
    })
}

fn encode_commit(w: &mut WireWriter, c: &SeamlessDimCommit) {
    w.write_u64(c.transfer_id);
    write_dim_id(w, sanitize_dim_id(&c.target_dim_id));
    write_f64x3(w, sanitize_f64x3(c.landing_pos));
    write_f32x3(w, sanitize_f32x3(c.landing_vel));
    w.write_f32(sanitize_f32(c.landing_yaw));
    w.write_f32(sanitize_f32(c.landing_pitch));
    w.write_u16(c.crossfade_ms.min(MAX_CROSSFADE_MS));
    w.write_bool(c.keep_momentum);
}

fn decode_commit(r: &mut WireReader<'_>) -> Result<SeamlessDimCommit, SeamlessPortalError> {
    let transfer_id = r.read_u64()?;
    let target_dim_id = read_dim_id(r)?;
    let landing_pos = read_f64x3(r, "landing_pos")?;
    let landing_vel = read_f32x3(r, "landing_vel")?;
    let landing_yaw = read_f32_checked(r, "landing_yaw")?;
    let landing_pitch = read_f32_checked(r, "landing_pitch")?;
    let crossfade_ms = r.read_u16()?;
    let keep_momentum = r.read_bool()?;
    Ok(SeamlessDimCommit {
        transfer_id,
        target_dim_id,
        landing_pos,
        landing_vel,
        landing_yaw,
        landing_pitch,
        crossfade_ms,
        keep_momentum,
    })
}

/// Encode one `MSP1` packet. The encoder is total and fail-closed: it
/// sanitizes rather than emits anything the decoder would reject — non-finite
/// floats become `0.0`, `prefetch_radius` clamps to
/// [`MAX_PREFETCH_RADIUS`], `crossfade_ms` clamps to [`MAX_CROSSFADE_MS`],
/// and dimension ids truncate to [`MAX_DIM_ID_BYTES`] on a UTF-8 boundary.
pub fn encode_seamless_portal_packet(packet: &SeamlessPortalPacket) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.raw(&SEAMLESS_PORTAL_MAGIC);
    w.write_u8(SEAMLESS_PORTAL_VERSION);
    match packet {
        SeamlessPortalPacket::PrefetchHint(h) => {
            w.write_u8(TAG_PREFETCH_HINT);
            encode_prefetch_hint(&mut w, h);
        }
        SeamlessPortalPacket::PrefetchCancel { transfer_id } => {
            w.write_u8(TAG_PREFETCH_CANCEL);
            w.write_u64(*transfer_id);
        }
        SeamlessPortalPacket::CommitTransfer(c) => {
            w.write_u8(TAG_COMMIT_TRANSFER);
            encode_commit(&mut w, c);
        }
        SeamlessPortalPacket::PrefetchAck {
            transfer_id,
            ready_chunks,
            total_chunks,
        } => {
            w.write_u8(TAG_PREFETCH_ACK);
            w.write_u64(*transfer_id);
            w.write_u16(*ready_chunks);
            w.write_u16(*total_chunks);
        }
    }
    w.into_vec()
}

/// Decode one `MSP1` packet, failing closed on any malformed input.
pub fn decode_seamless_portal_packet(
    bytes: &[u8],
) -> Result<SeamlessPortalPacket, SeamlessPortalError> {
    if bytes.len() < SEAMLESS_PORTAL_MAGIC.len() + 2 {
        return Err(SeamlessPortalError::Wire(ProtoError::UnexpectedEof {
            needed: SEAMLESS_PORTAL_MAGIC.len() + 2,
            remaining: bytes.len(),
        }));
    }
    if bytes[..4] != SEAMLESS_PORTAL_MAGIC {
        return Err(SeamlessPortalError::BadMagic);
    }
    let version = bytes[4];
    if version != SEAMLESS_PORTAL_VERSION {
        return Err(SeamlessPortalError::UnsupportedVersion(version));
    }
    let mut r = WireReader::new(&bytes[5..]);
    let tag = r.read_u8()?;
    let packet = match tag {
        TAG_PREFETCH_HINT => SeamlessPortalPacket::PrefetchHint(decode_prefetch_hint(&mut r)?),
        TAG_PREFETCH_CANCEL => SeamlessPortalPacket::PrefetchCancel {
            transfer_id: r.read_u64()?,
        },
        TAG_COMMIT_TRANSFER => SeamlessPortalPacket::CommitTransfer(decode_commit(&mut r)?),
        TAG_PREFETCH_ACK => SeamlessPortalPacket::PrefetchAck {
            transfer_id: r.read_u64()?,
            ready_chunks: r.read_u16()?,
            total_chunks: r.read_u16()?,
        },
        other => return Err(SeamlessPortalError::UnknownTag(other)),
    };
    r.finish()?;
    Ok(packet)
}

// ---------------------------------------------------------------------------
// Portal coordinate mapping + chunk prefetch planner
// ---------------------------------------------------------------------------

/// How a portal maps a position in the source dimension to a destination.
/// Vanilla portals are recognized by [`PortalKind::Nether`]/[`PortalKind::End`];
/// modded dimensions (Twilight Forest, Aether, custom `.mmod` content) use
/// [`PortalKind::Custom`] so nothing is hardcoded per-mod.
#[derive(Clone, Debug, PartialEq)]
pub enum PortalKind {
    /// Vanilla nether portal: exact 1:8 / 8:1 X/Z scaling between
    /// `minecraft:overworld` and `minecraft:the_nether`.
    Nether,
    /// Vanilla end portal / stronghold exit: fixed platform out, world-spawn
    /// fallback back.
    End,
    /// Vanilla end gateway: hops inside `minecraft:the_end`; from any other
    /// dimension it behaves like [`PortalKind::End`].
    EndGateway,
    /// Any modded or custom portal. `coord_scale` multiplies destination
    /// X/Z (sanitized to a finite value `> 0`, else `1.0`);
    /// `fixed_spawn` short-circuits scaling entirely.
    Custom {
        target_dim_id: String,
        coord_scale: f64,
        fixed_spawn: Option<[f64; 3]>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VanillaDim {
    Overworld,
    Nether,
    End,
}

fn classify_vanilla_dim(dim: &str) -> Option<VanillaDim> {
    match dim {
        "minecraft:overworld" | "overworld" => Some(VanillaDim::Overworld),
        "minecraft:the_nether" | "the_nether" | "minecraft:nether" | "nether" => {
            Some(VanillaDim::Nether)
        }
        "minecraft:the_end" | "the_end" | "minecraft:end" | "end" => Some(VanillaDim::End),
        _ => None,
    }
}

fn clamp_border(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    v.clamp(-WORLD_BORDER_LIMIT, WORLD_BORDER_LIMIT)
}

fn scaled_pos(portal_pos: [f64; 3], scale: f64) -> [f64; 3] {
    [
        clamp_border(sanitize_f64(portal_pos[0]) * scale),
        sanitize_f64(portal_pos[1]),
        clamp_border(sanitize_f64(portal_pos[2]) * scale),
    ]
}

/// Resolve where `portal_pos` in `source_dim` lands, returning
/// `(target_dim_id, landing_pos)`. Vanilla overworld↔nether uses exact
/// 1:8/8:1 X/Z scaling clamped to the world border; overworld→End lands on
/// the obsidian platform `(100.5, 49.0, 0.5)`; End→Overworld falls back to
/// [`OVERWORLD_SPAWN_FALLBACK`] until the real spawn is resolved. Custom
/// kinds apply a sanitized `coord_scale` or return `fixed_spawn` verbatim,
/// so modded dimensions need no per-mod code here.
pub fn resolve_portal_destination(
    source_dim: &str,
    portal_pos: [f64; 3],
    kind: &PortalKind,
) -> (String, [f64; 3]) {
    match kind {
        PortalKind::Nether => match classify_vanilla_dim(source_dim) {
            Some(VanillaDim::Overworld) => (
                "minecraft:the_nether".to_string(),
                scaled_pos(portal_pos, 0.125),
            ),
            Some(VanillaDim::Nether) => (
                "minecraft:overworld".to_string(),
                scaled_pos(portal_pos, 8.0),
            ),
            // Nether portals are only defined between the vanilla pair;
            // anything else passes through untouched.
            _ => (source_dim.to_string(), sanitize_f64x3(portal_pos)),
        },
        PortalKind::End | PortalKind::EndGateway => match classify_vanilla_dim(source_dim) {
            Some(VanillaDim::End) => ("minecraft:overworld".to_string(), OVERWORLD_SPAWN_FALLBACK),
            _ => ("minecraft:the_end".to_string(), [100.5, 49.0, 0.5]),
        },
        PortalKind::Custom {
            target_dim_id,
            coord_scale,
            fixed_spawn,
        } => {
            if let Some(spawn) = fixed_spawn {
                return (target_dim_id.clone(), sanitize_f64x3(*spawn));
            }
            let scale = if coord_scale.is_finite() && *coord_scale > 0.0 {
                *coord_scale
            } else {
                1.0
            };
            (target_dim_id.clone(), scaled_pos(portal_pos, scale))
        }
    }
}

/// Enumerate the `(2r+1)^2` chunk coordinates to prefetch around `center`,
/// ordered nearest-first: ring `k` (Chebyshev distance `k` from center) is
/// emitted before ring `k+1`, and ring `0` is exactly `center` — so the
/// landing chunk streams FIRST. `radius` clamps to
/// [`MAX_PREFETCH_RADIUS`], bounding the result to at most 49 chunks.
pub fn enumerate_prefetch_chunks(center: [i32; 2], radius: u8) -> Vec<[i32; 2]> {
    let r = i32::from(radius.min(MAX_PREFETCH_RADIUS));
    let (cx, cz) = (center[0], center[1]);
    let side = 2 * r + 1;
    let mut out = Vec::with_capacity((side * side) as usize);
    for k in 0..=r {
        if k == 0 {
            out.push([cx, cz]);
            continue;
        }
        // Top and bottom rows of the ring, then the side columns between
        // them (corners already emitted by the rows).
        for dx in -k..=k {
            out.push([cx + dx, cz - k]);
            out.push([cx + dx, cz + k]);
        }
        for dz in -k + 1..k {
            out.push([cx - k, cz + dz]);
            out.push([cx + k, cz + dz]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Server-side proximity prefetch tracker
// ---------------------------------------------------------------------------

/// A portal the tracker can approach: a stable `key` plus the
/// [`PortalPrefetchHint`] template to emit. The tracker fills in
/// `transfer_id` and `portal_pos`-relative bookkeeping itself.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalSite {
    /// Stable identity used for hysteresis (same portal keeps one session).
    pub key: u64,
    /// Hint template; `transfer_id` is overwritten on each start.
    pub hint: PortalPrefetchHint,
}

/// What the tracker wants the caller to do this tick.
#[derive(Clone, Debug, PartialEq)]
pub enum ProximityAction {
    /// Nothing to send.
    None,
    /// Player entered the trigger band: send this prefetch hint.
    StartPrefetch(PortalPrefetchHint),
    /// Player left the cancel band (or the portal vanished): release the
    /// standby cache for this `transfer_id`.
    CancelPrefetch(u64),
}

#[derive(Clone, Debug)]
struct ActivePrefetch {
    site_key: u64,
    transfer_id: u64,
    portal_pos: [f64; 3],
    since_tick: u64,
}

/// Deterministic, tick-driven proximity gate. No `SystemTime` anywhere:
/// the caller supplies `now_tick` and player/portal positions.
///
/// Hysteresis: a prefetch starts at `trigger_distance` (default 16.0) but
/// only cancels past `cancel_distance` (default 22.0), so hovering near the
/// threshold cannot thrash the standby cache.
///
/// `client_supports_seamless` records whether the client advertised
/// [`SEAMLESS_PORTAL_CHANNEL`]; when `false`,
/// [`PortalProximityTracker::can_use_seamless_commit`] returns `false` and
/// the caller should send the standard Protocol 767 `Respawn` instead.
#[derive(Clone, Debug)]
pub struct PortalProximityTracker {
    trigger_distance: f64,
    cancel_distance: f64,
    client_supports_seamless: bool,
    active: Option<ActivePrefetch>,
    next_transfer_id: u64,
}

impl PortalProximityTracker {
    /// Tracker with the default 16.0/22.0 hysteresis band.
    pub fn new(client_supports_seamless: bool) -> Self {
        Self {
            trigger_distance: 16.0,
            cancel_distance: 22.0,
            client_supports_seamless,
            active: None,
            next_transfer_id: 1,
        }
    }

    /// Tracker with an explicit hysteresis band (`cancel` should exceed
    /// `trigger`; values are used as given).
    pub fn with_distances(
        trigger_distance: f64,
        cancel_distance: f64,
        client_supports_seamless: bool,
    ) -> Self {
        Self {
            trigger_distance,
            cancel_distance,
            client_supports_seamless,
            active: None,
            next_transfer_id: 1,
        }
    }

    /// Whether the tracked client advertised `megu:portal_seamless_v1`.
    /// `false` means Vanilla-fallback: standard `Respawn`, no commit.
    pub fn client_supports_seamless(&self) -> bool {
        self.client_supports_seamless
    }

    /// Gate for the seamless commit path: only clients that advertised the
    /// channel may receive [`SeamlessPortalPacket::CommitTransfer`].
    pub fn can_use_seamless_commit(&self) -> bool {
        self.client_supports_seamless
    }

    /// The in-flight prefetch session, if any.
    pub fn active_transfer_id(&self) -> Option<u64> {
        self.active.as_ref().map(|a| a.transfer_id)
    }

    /// Caller tick at which the active prefetch started, if any.
    pub fn active_since_tick(&self) -> Option<u64> {
        self.active.as_ref().map(|a| a.since_tick)
    }

    /// Advance the tracker one tick.
    ///
    /// `player_pos` is the player's position; `nearest_portal` is the
    /// closest [`PortalSite`] (or `None`); `now_tick` is the caller's
    /// monotonic tick counter, recorded as the prefetch start tick.
    pub fn update(
        &mut self,
        player_pos: [f64; 3],
        nearest_portal: Option<&PortalSite>,
        now_tick: u64,
    ) -> ProximityAction {
        if let Some(active) = &self.active {
            let same_site = nearest_portal.is_some_and(|s| s.key == active.site_key);
            let portal_pos = match (same_site, nearest_portal) {
                (true, Some(site)) => site.hint.portal_pos,
                // Portal list changed: measure against the *active* portal's
                // stored position so walking away still cancels cleanly.
                _ => active.portal_pos,
            };
            let dist = distance_3d(player_pos, portal_pos);
            // Non-finite distances fail closed as "far away".
            let dist = if dist.is_finite() { dist } else { f64::MAX };
            if dist > self.cancel_distance {
                let transfer_id = active.transfer_id;
                self.active = None;
                return ProximityAction::CancelPrefetch(transfer_id);
            }
            return ProximityAction::None;
        }

        let Some(site) = nearest_portal else {
            return ProximityAction::None;
        };
        let dist = distance_3d(player_pos, site.hint.portal_pos);
        let dist = if dist.is_finite() { dist } else { f64::MAX };
        if dist > self.trigger_distance {
            return ProximityAction::None;
        }

        self.next_transfer_id = self.next_transfer_id.wrapping_add(1);
        let transfer_id = self.next_transfer_id;
        let mut hint = site.hint.clone();
        hint.transfer_id = transfer_id;
        self.active = Some(ActivePrefetch {
            site_key: site.key,
            transfer_id,
            portal_pos: site.hint.portal_pos,
            since_tick: now_tick,
        });
        ProximityAction::StartPrefetch(hint)
    }

    /// Drop any active prefetch without emitting a cancel (e.g. on disconnect).
    pub fn reset(&mut self) {
        self.active = None;
    }
}

fn distance_3d(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

// ---------------------------------------------------------------------------
// Client-side zero-loading-screen crossfade state machine
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct StandbyPrefetch {
    transfer_id: u64,
    dim_id: String,
    /// Exact chunk set the hint asked for — acks only count these.
    expected: BTreeSet<(i32, i32)>,
    ready: BTreeSet<(i32, i32)>,
    total_chunks: u16,
    target_sky_rgb: [f32; 3],
    target_fog_rgb: [f32; 3],
}

#[derive(Clone, Debug)]
struct Crossfade {
    from_sky_rgb: [f32; 3],
    from_fog_rgb: [f32; 3],
    to_sky_rgb: [f32; 3],
    to_fog_rgb: [f32; 3],
    elapsed_ms: f32,
    duration_ms: f32,
}

/// Client-side state for the seamless path: tracks the standby-prefetched
/// destination dimension and the post-commit atmosphere crossfade.
///
/// Lifecycle: [`Self::on_prefetch_hint`] arms standby →
/// [`Self::mark_standby_chunk_ready`] streams [`SeamlessPortalPacket::PrefetchAck`]s
/// → [`Self::on_commit_transfer`] starts the crossfade and records the
/// committed dimension → [`Self::advance_ms`] progresses it →
/// [`Self::blended_sky_and_fog`] yields eased colors until it returns `None`.
/// [`Self::should_suppress_loading_screen`] stays `true` for the committed
/// dimension, suppressing the black "Loading Terrain" screen entirely.
#[derive(Clone, Debug)]
pub struct SeamlessDimTransitionState {
    standby: Option<StandbyPrefetch>,
    crossfade: Option<Crossfade>,
    current_sky_rgb: [f32; 3],
    current_fog_rgb: [f32; 3],
    committed_dim_id: Option<String>,
}

impl Default for SeamlessDimTransitionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SeamlessDimTransitionState {
    pub fn new() -> Self {
        Self {
            standby: None,
            crossfade: None,
            // Neutral daytime-ish defaults; the renderer overwrites these via
            // on_commit_transfer's `from` capture or set_current_atmosphere.
            current_sky_rgb: [0.47, 0.65, 1.0],
            current_fog_rgb: [0.75, 0.85, 1.0],
            committed_dim_id: None,
        }
    }

    /// Record the *current* dimension's atmosphere so the next commit
    /// crossfades from the right colors.
    pub fn set_current_atmosphere(&mut self, sky_rgb: [f32; 3], fog_rgb: [f32; 3]) {
        self.current_sky_rgb = sanitize_f32x3(sky_rgb);
        self.current_fog_rgb = sanitize_f32x3(fog_rgb);
    }

    /// The standby dimension currently being prefetched, if any.
    pub fn standby_dim_id(&self) -> Option<&str> {
        self.standby.as_ref().map(|s| s.dim_id.as_str())
    }

    /// Standby chunks ready / total for the in-flight prefetch.
    pub fn standby_progress(&self) -> Option<(u16, u16)> {
        self.standby
            .as_ref()
            .map(|s| (s.ready.len() as u16, s.total_chunks))
    }

    /// In-flight transfer id, if a prefetch is armed.
    pub fn standby_transfer_id(&self) -> Option<u64> {
        self.standby.as_ref().map(|s| s.transfer_id)
    }

    /// Arm (or re-arm) standby prefetch for a hint. Any previous standby is
    /// replaced; an in-flight crossfade is unaffected.
    pub fn on_prefetch_hint(&mut self, hint: &PortalPrefetchHint) {
        let chunks = enumerate_prefetch_chunks(hint.target_center_chunk, hint.prefetch_radius);
        let expected: BTreeSet<(i32, i32)> = chunks.iter().map(|c| (c[0], c[1])).collect();
        self.standby = Some(StandbyPrefetch {
            transfer_id: hint.transfer_id,
            dim_id: sanitize_dim_id(&hint.target_dim_id).to_string(),
            total_chunks: chunks.len() as u16,
            expected,
            ready: BTreeSet::new(),
            target_sky_rgb: sanitize_f32x3(hint.target_sky_rgb),
            target_fog_rgb: sanitize_f32x3(hint.target_fog_rgb),
        });
    }

    /// Mark one standby chunk resident. Returns a [`SeamlessPortalPacket::PrefetchAck`]
    /// whenever progress advances (new in-range chunk), `None` for duplicate,
    /// out-of-range, or absent standby state.
    pub fn mark_standby_chunk_ready(&mut self, cx: i32, cz: i32) -> Option<SeamlessPortalPacket> {
        let standby = self.standby.as_mut()?;
        let key = (cx, cz);
        if !standby.expected.contains(&key) || !standby.ready.insert(key) {
            return None;
        }
        Some(SeamlessPortalPacket::PrefetchAck {
            transfer_id: standby.transfer_id,
            ready_chunks: standby.ready.len() as u16,
            total_chunks: standby.total_chunks,
        })
    }

    /// Release standby state for `transfer_id`. Returns `true` if a standby
    /// was actually dropped.
    pub fn on_prefetch_cancel(&mut self, transfer_id: u64) -> bool {
        match &self.standby {
            Some(s) if s.transfer_id == transfer_id => {
                self.standby = None;
                true
            }
            _ => false,
        }
    }

    /// Commit the dimension swap: consume the matching standby (if any),
    /// start the sky/fog crossfade toward the standby's target atmosphere,
    /// and record the committed dimension for
    /// [`Self::should_suppress_loading_screen`].
    pub fn on_commit_transfer(&mut self, commit: &SeamlessDimCommit) {
        let standby = self
            .standby
            .take()
            .filter(|s| s.transfer_id == commit.transfer_id);
        let (to_sky, to_fog) = match &standby {
            Some(s) => (s.target_sky_rgb, s.target_fog_rgb),
            // Commit without a matching hint: crossfade current → current
            // (static atmosphere) rather than guessing.
            None => (self.current_sky_rgb, self.current_fog_rgb),
        };
        let duration_ms = f32::from(commit.crossfade_ms.min(MAX_CROSSFADE_MS));
        self.crossfade = Some(Crossfade {
            from_sky_rgb: self.current_sky_rgb,
            from_fog_rgb: self.current_fog_rgb,
            to_sky_rgb: to_sky,
            to_fog_rgb: to_fog,
            elapsed_ms: 0.0,
            duration_ms,
        });
        self.committed_dim_id = Some(sanitize_dim_id(&commit.target_dim_id).to_string());
    }

    /// Advance the crossfade by `dt_ms`. Non-finite or non-positive deltas
    /// are ignored. When the duration elapses the current atmosphere snaps
    /// to the target and [`Self::blended_sky_and_fog`] starts returning
    /// `None`.
    pub fn advance_ms(&mut self, dt_ms: f32) {
        let Some(cf) = &mut self.crossfade else {
            return;
        };
        if !dt_ms.is_finite() || dt_ms <= 0.0 {
            return;
        }
        cf.elapsed_ms += dt_ms;
        if cf.elapsed_ms >= cf.duration_ms {
            self.current_sky_rgb = cf.to_sky_rgb;
            self.current_fog_rgb = cf.to_fog_rgb;
            self.crossfade = None;
        }
    }

    /// While a crossfade is in flight, returns the smoothstep-eased
    /// `(sky_rgb, fog_rgb, progress)` triple; `None` before any commit and
    /// after the crossfade finishes.
    pub fn blended_sky_and_fog(&self) -> Option<([f32; 3], [f32; 3], f32)> {
        let cf = self.crossfade.as_ref()?;
        let raw = if cf.duration_ms > 0.0 {
            (cf.elapsed_ms / cf.duration_ms).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let t = raw * raw * (3.0 - 2.0 * raw); // smoothstep
        let sky = lerp3(cf.from_sky_rgb, cf.to_sky_rgb, t);
        let fog = lerp3(cf.from_fog_rgb, cf.to_fog_rgb, t);
        Some((sky, fog, t))
    }

    /// `true` when `target_dim` was committed through the seamless path —
    /// the renderer must suppress the black "Loading Terrain" screen.
    pub fn should_suppress_loading_screen(&self, target_dim: &str) -> bool {
        self.committed_dim_id.as_deref() == Some(target_dim)
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hint() -> PortalPrefetchHint {
        PortalPrefetchHint {
            transfer_id: 42,
            target_dim_id: "minecraft:the_nether".to_string(),
            target_center_chunk: [10, -4],
            prefetch_radius: 2,
            portal_pos: [80.0, 64.0, -32.0],
            target_spawn: [10.0, 64.0, -4.0],
            target_yaw: 90.5,
            target_pitch: -12.25,
            target_sky_rgb: [0.9, 0.4, 0.2],
            target_fog_rgb: [0.5, 0.2, 0.1],
        }
    }

    fn sample_commit() -> SeamlessDimCommit {
        SeamlessDimCommit {
            transfer_id: 42,
            target_dim_id: "minecraft:the_nether".to_string(),
            landing_pos: [10.0, 65.0, -4.0],
            landing_vel: [0.0, 0.0, 4.2],
            landing_yaw: 90.5,
            landing_pitch: -12.25,
            crossfade_ms: DEFAULT_CROSSFADE_MS,
            keep_momentum: true,
        }
    }

    /// Hand-build a raw PrefetchHint body so tests can inject hostile bytes
    /// the sanitizer would otherwise scrub.
    fn raw_hint_bytes(radius: u8, yaw_bits: u32) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.raw(&SEAMLESS_PORTAL_MAGIC);
        w.write_u8(SEAMLESS_PORTAL_VERSION);
        w.write_u8(TAG_PREFETCH_HINT);
        w.write_u64(7);
        w.write_bytes(b"minecraft:the_nether");
        w.write_i32(1);
        w.write_i32(2);
        w.write_u8(radius);
        for _ in 0..6 {
            w.write_f64(0.0);
        }
        w.write_f32(f32::from_bits(yaw_bits));
        w.write_f32(0.0);
        for _ in 0..6 {
            w.write_f32(0.0);
        }
        w.into_vec()
    }

    fn raw_hint_bytes_with_dim(dim: &[u8]) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.raw(&SEAMLESS_PORTAL_MAGIC);
        w.write_u8(SEAMLESS_PORTAL_VERSION);
        w.write_u8(TAG_PREFETCH_HINT);
        w.write_u64(7);
        w.write_bytes(dim);
        w.write_i32(1);
        w.write_i32(2);
        w.write_u8(1);
        for _ in 0..6 {
            w.write_f64(0.0);
        }
        for _ in 0..8 {
            w.write_f32(0.0);
        }
        w.into_vec()
    }

    // -- 1. Wire codec -------------------------------------------------------

    #[test]
    fn codec_roundtrips_all_four_variants() {
        let packets = [
            SeamlessPortalPacket::PrefetchHint(sample_hint()),
            SeamlessPortalPacket::PrefetchCancel { transfer_id: 42 },
            SeamlessPortalPacket::CommitTransfer(sample_commit()),
            SeamlessPortalPacket::PrefetchAck {
                transfer_id: 42,
                ready_chunks: 17,
                total_chunks: 25,
            },
        ];
        for packet in &packets {
            let bytes = encode_seamless_portal_packet(packet);
            assert_eq!(&bytes[..4], b"MSP1");
            assert_eq!(bytes[4], SEAMLESS_PORTAL_VERSION);
            let decoded = decode_seamless_portal_packet(&bytes).unwrap();
            assert_eq!(&decoded, packet);
        }
    }

    #[test]
    fn codec_rejects_adversarial_input() {
        let good = encode_seamless_portal_packet(&SeamlessPortalPacket::PrefetchHint(sample_hint()));

        // Wrong magic.
        let mut bad = good.clone();
        bad[0] = b'X';
        assert_eq!(
            decode_seamless_portal_packet(&bad),
            Err(SeamlessPortalError::BadMagic)
        );

        // Wrong version.
        let mut bad = good.clone();
        bad[4] = 2;
        assert_eq!(
            decode_seamless_portal_packet(&bad),
            Err(SeamlessPortalError::UnsupportedVersion(2))
        );

        // Truncated payload.
        assert!(decode_seamless_portal_packet(&good[..good.len() - 1]).is_err());
        assert!(decode_seamless_portal_packet(&[]).is_err());
        assert!(decode_seamless_portal_packet(&good[..5]).is_err());

        // Trailing garbage.
        let mut bad = good.clone();
        bad.push(0);
        assert!(decode_seamless_portal_packet(&bad).is_err());

        // Unknown tag.
        let mut bad = good.clone();
        bad[5] = 0x7f;
        assert_eq!(
            decode_seamless_portal_packet(&bad),
            Err(SeamlessPortalError::UnknownTag(0x7f))
        );

        // NaN / infinite floats fail closed.
        for bits in [f32::NAN.to_bits(), f32::INFINITY.to_bits(), f32::NEG_INFINITY.to_bits()]
        {
            assert!(matches!(
                decode_seamless_portal_packet(&raw_hint_bytes(1, bits)),
                Err(SeamlessPortalError::NonFiniteFloat(_))
            ));
        }

        // prefetch_radius above the cap.
        assert!(matches!(
            decode_seamless_portal_packet(&raw_hint_bytes(MAX_PREFETCH_RADIUS + 1, 0)),
            Err(SeamlessPortalError::RadiusOutOfBounds { .. })
        ));
        // Radius at the cap decodes fine.
        assert!(decode_seamless_portal_packet(&raw_hint_bytes(MAX_PREFETCH_RADIUS, 0)).is_ok());

        // Dimension id over 128 bytes.
        let long_dim = vec![b'a'; MAX_DIM_ID_BYTES + 1];
        assert!(matches!(
            decode_seamless_portal_packet(&raw_hint_bytes_with_dim(&long_dim)),
            Err(SeamlessPortalError::Wire(ProtoError::LengthOutOfBounds { .. }))
        ));
        // Exactly 128 bytes is fine.
        let ok_dim = vec![b'a'; MAX_DIM_ID_BYTES];
        assert!(decode_seamless_portal_packet(&raw_hint_bytes_with_dim(&ok_dim)).is_ok());
    }

    #[test]
    fn codec_encoder_sanitizes_hostile_fields() {
        let mut hint = sample_hint();
        hint.prefetch_radius = 200;
        hint.target_yaw = f32::NAN;
        hint.target_dim_id = "x".repeat(300);
        let decoded = decode_seamless_portal_packet(&encode_seamless_portal_packet(
            &SeamlessPortalPacket::PrefetchHint(hint),
        ))
        .unwrap();
        let SeamlessPortalPacket::PrefetchHint(h) = decoded else {
            panic!("expected hint");
        };
        assert_eq!(h.prefetch_radius, MAX_PREFETCH_RADIUS);
        assert_eq!(h.target_yaw, 0.0);
        assert!(h.target_dim_id.len() <= MAX_DIM_ID_BYTES);
    }

    // -- 2. Coordinate mapping ----------------------------------------------

    #[test]
    fn nether_scaling_is_exact_eighth() {
        let (dim, pos) = resolve_portal_destination(
            "minecraft:overworld",
            [800.0, 64.0, -160.0],
            &PortalKind::Nether,
        );
        assert_eq!(dim, "minecraft:the_nether");
        assert_eq!(pos, [100.0, 64.0, -20.0]);

        let (dim, pos) = resolve_portal_destination(
            "minecraft:the_nether",
            [100.0, 40.0, -20.0],
            &PortalKind::Nether,
        );
        assert_eq!(dim, "minecraft:overworld");
        assert_eq!(pos, [800.0, 40.0, -160.0]);
    }

    #[test]
    fn nether_scaling_clamps_to_world_border() {
        let (_, pos) = resolve_portal_destination(
            "minecraft:overworld",
            [f64::MAX, 64.0, -f64::MAX],
            &PortalKind::Nether,
        );
        assert_eq!(pos[0], WORLD_BORDER_LIMIT);
        assert_eq!(pos[2], -WORLD_BORDER_LIMIT);
    }

    #[test]
    fn end_portal_platform_and_spawn_fallback() {
        let (dim, pos) =
            resolve_portal_destination("minecraft:overworld", [0.0, 30.0, 0.0], &PortalKind::End);
        assert_eq!(dim, "minecraft:the_end");
        assert_eq!(pos, [100.5, 49.0, 0.5]);

        let (dim, pos) =
            resolve_portal_destination("minecraft:the_end", [100.5, 49.0, 0.5], &PortalKind::End);
        assert_eq!(dim, "minecraft:overworld");
        assert_eq!(pos, OVERWORLD_SPAWN_FALLBACK);
    }

    #[test]
    fn custom_mod_dimensions_scale_or_fixed_spawn() {
        // Twilight Forest-style: coord scaling into a custom dimension.
        let kind = PortalKind::Custom {
            target_dim_id: "twilightforest:twilight_forest".to_string(),
            coord_scale: 0.5,
            fixed_spawn: None,
        };
        let (dim, pos) =
            resolve_portal_destination("minecraft:overworld", [400.0, 80.0, -200.0], &kind);
        assert_eq!(dim, "twilightforest:twilight_forest");
        assert_eq!(pos, [200.0, 80.0, -100.0]);

        // Aether-style: fixed spawn point wins over scaling.
        let kind = PortalKind::Custom {
            target_dim_id: "mymod:sky_realm".to_string(),
            coord_scale: 2.0,
            fixed_spawn: Some([1234.0, 90.0, -567.0]),
        };
        let (dim, pos) = resolve_portal_destination("minecraft:overworld", [1.0, 2.0, 3.0], &kind);
        assert_eq!(dim, "mymod:sky_realm");
        assert_eq!(pos, [1234.0, 90.0, -567.0]);

        // Degenerate scales sanitize to identity rather than NaN/inf.
        for bad in [0.0, -3.0, f64::NAN, f64::INFINITY] {
            let kind = PortalKind::Custom {
                target_dim_id: "mymod:x".to_string(),
                coord_scale: bad,
                fixed_spawn: None,
            };
            let (_, pos) = resolve_portal_destination("minecraft:overworld", [10.0, 64.0, 20.0], &kind);
            assert_eq!(pos, [10.0, 64.0, 20.0]);
        }
    }

    // -- 3. Chunk prefetch planner ------------------------------------------

    #[test]
    fn enumerate_prefetch_chunks_is_nearest_first_and_bounded() {
        // Radius 0: just the landing chunk.
        assert_eq!(enumerate_prefetch_chunks([5, -3], 0), vec![[5, -3]]);

        // Radius 3: exactly 49 unique chunks, center first.
        let chunks = enumerate_prefetch_chunks([5, -3], MAX_PREFETCH_RADIUS);
        assert_eq!(chunks.len(), 49);
        assert_eq!(chunks[0], [5, -3]);
        let unique: BTreeSet<(i32, i32)> = chunks.iter().map(|c| (c[0], c[1])).collect();
        assert_eq!(unique.len(), 49);
        for c in &chunks {
            assert!((c[0] - 5).abs() <= 3 && (c[1] + 3).abs() <= 3);
        }
        // Chebyshev-nearest-first: no ring-2 chunk before all ring-1 chunks.
        let ring1_end = chunks.iter().position(|c| {
            (c[0] - 5).abs().max((c[1] + 3).abs()) == 2
        });
        assert!(ring1_end.is_some());
        assert!(chunks[..ring1_end.unwrap()].iter().all(|c| {
            (c[0] - 5).abs().max((c[1] + 3).abs()) <= 1
        }));

        // Radius above the cap clamps to 49 chunks.
        assert_eq!(enumerate_prefetch_chunks([0, 0], 200).len(), 49);
    }

    // -- 4. Proximity tracker hysteresis ------------------------------------

    fn site(portal_pos: [f64; 3]) -> PortalSite {
        PortalSite {
            key: 7,
            hint: PortalPrefetchHint {
                transfer_id: 0,
                portal_pos,
                ..sample_hint()
            },
        }
    }

    #[test]
    fn proximity_tracker_hysteresis_has_no_thrash() {
        let mut tracker = PortalProximityTracker::new(true);
        let portal = site([0.0, 64.0, 0.0]);

        // 14m away: inside the 16m trigger band → start.
        let action = tracker.update([14.0, 64.0, 0.0], Some(&portal), 100);
        let ProximityAction::StartPrefetch(hint) = action else {
            panic!("expected StartPrefetch, got {action:?}");
        };
        assert_eq!(tracker.active_transfer_id(), Some(hint.transfer_id));

        // 18m away: outside trigger but inside the 22m cancel band → hold.
        for tick in 101..110 {
            assert_eq!(
                tracker.update([18.0, 64.0, 0.0], Some(&portal), tick),
                ProximityAction::None
            );
        }
        assert!(tracker.active_transfer_id().is_some());

        // Past 22m: clean cancel with the same transfer id.
        assert_eq!(
            tracker.update([23.0, 64.0, 0.0], Some(&portal), 110),
            ProximityAction::CancelPrefetch(hint.transfer_id)
        );
        assert_eq!(tracker.active_transfer_id(), None);

        // Re-approach starts a fresh transfer id.
        let action = tracker.update([10.0, 64.0, 0.0], Some(&portal), 111);
        let ProximityAction::StartPrefetch(hint2) = action else {
            panic!("expected StartPrefetch");
        };
        assert_ne!(hint2.transfer_id, hint.transfer_id);
    }

    #[test]
    fn proximity_tracker_vanilla_client_falls_back() {
        let mut tracker = PortalProximityTracker::new(false);
        let portal = site([0.0, 64.0, 0.0]);
        assert!(!tracker.can_use_seamless_commit());
        assert!(!tracker.client_supports_seamless());
        // Hints may still be computed, but the commit gate stays closed so the
        // caller sends a standard Protocol 767 Respawn instead.
        assert!(matches!(
            tracker.update([5.0, 64.0, 0.0], Some(&portal), 1),
            ProximityAction::StartPrefetch(_)
        ));
        assert!(!tracker.can_use_seamless_commit());
    }

    #[test]
    fn proximity_tracker_far_player_and_missing_portal_are_noops() {
        let mut tracker = PortalProximityTracker::new(true);
        let portal = site([0.0, 64.0, 0.0]);
        assert_eq!(tracker.update([100.0, 64.0, 0.0], Some(&portal), 1), ProximityAction::None);
        assert_eq!(tracker.update([0.0, 64.0, 0.0], None, 2), ProximityAction::None);
        // NaN player position fails closed (treated as far).
        assert_eq!(
            tracker.update([f64::NAN, 64.0, 0.0], Some(&portal), 3),
            ProximityAction::None
        );
    }

    // -- 5. Client crossfade state machine ----------------------------------

    #[test]
    fn seamless_lifecycle_suppresses_loading_and_crossfades() {
        let mut state = SeamlessDimTransitionState::new();
        state.set_current_atmosphere([0.1, 0.2, 0.3], [0.4, 0.5, 0.6]);

        let hint = sample_hint(); // radius 2 → 25 chunks, target sky [0.9,0.4,0.2]
        state.on_prefetch_hint(&hint);
        assert_eq!(state.standby_dim_id(), Some("minecraft:the_nether"));
        assert_eq!(state.standby_progress(), Some((0, 25)));

        // Stream every enumerated chunk; each in-range chunk acks exactly once.
        let mut last_ack = None;
        for c in enumerate_prefetch_chunks(hint.target_center_chunk, hint.prefetch_radius) {
            if let Some(pkt) = state.mark_standby_chunk_ready(c[0], c[1]) {
                last_ack = Some(pkt);
            }
        }
        // Out-of-range and duplicate chunks produce nothing.
        assert!(state.mark_standby_chunk_ready(999, 999).is_none());
        assert!(state.mark_standby_chunk_ready(10, -4).is_none());
        let Some(SeamlessPortalPacket::PrefetchAck {
            ready_chunks,
            total_chunks,
            transfer_id,
        }) = last_ack
        else {
            panic!("expected final PrefetchAck");
        };
        assert_eq!((ready_chunks, total_chunks), (25, 25));
        assert_eq!(transfer_id, hint.transfer_id);
        assert_eq!(state.standby_progress(), Some((25, 25)));

        // Commit: loading screen suppressed, crossfade starts at progress 0.
        let mut commit = sample_commit();
        commit.crossfade_ms = 200;
        state.on_commit_transfer(&commit);
        assert!(state.should_suppress_loading_screen("minecraft:the_nether"));
        assert!(!state.should_suppress_loading_screen("minecraft:overworld"));
        assert!(state.standby_dim_id().is_none());

        let (sky0, fog0, p0) = state.blended_sky_and_fog().expect("crossfade active");
        assert_eq!(p0, 0.0);
        assert_eq!(sky0, [0.1, 0.2, 0.3]);
        assert_eq!(fog0, [0.4, 0.5, 0.6]);

        // Half duration: smoothstep(0.5) == 0.5 — colors midway, no NaN.
        state.advance_ms(100.0);
        let (sky, fog, p) = state.blended_sky_and_fog().unwrap();
        assert!((p - 0.5).abs() < 1e-6);
        for v in sky.iter().chain(fog.iter()).chain([&p]) {
            assert!(v.is_finite());
        }
        assert!((sky[0] - 0.5).abs() < 1e-6); // lerp(0.1 → 0.9) at t=0.5

        // Finish: target colors reached, then blended returns None forever.
        state.advance_ms(100.0);
        assert!(state.blended_sky_and_fog().is_none());
        state.advance_ms(50.0);
        assert!(state.blended_sky_and_fog().is_none());
        assert!(state.should_suppress_loading_screen("minecraft:the_nether"));

        // Non-finite / non-positive deltas never corrupt the state.
        let mut fresh = SeamlessDimTransitionState::new();
        fresh.on_prefetch_hint(&hint);
        let mut c2 = sample_commit();
        c2.transfer_id = hint.transfer_id;
        fresh.on_commit_transfer(&c2);
        fresh.advance_ms(f32::NAN);
        fresh.advance_ms(-5.0);
        let (_, _, p) = fresh.blended_sky_and_fog().unwrap();
        assert_eq!(p, 0.0);
    }

    #[test]
    fn prefetch_cancel_and_mismatched_ids_are_safe() {
        let mut state = SeamlessDimTransitionState::new();
        state.on_prefetch_hint(&sample_hint());
        // Wrong transfer id: standby survives.
        assert!(!state.on_prefetch_cancel(999));
        assert!(state.standby_dim_id().is_some());
        // Right id: dropped; further chunk-ready calls are no-ops.
        assert!(state.on_prefetch_cancel(42));
        assert!(state.standby_dim_id().is_none());
        assert!(state.mark_standby_chunk_ready(10, -4).is_none());
    }

    #[test]
    fn commit_without_standby_still_crossfades_safely() {
        let mut state = SeamlessDimTransitionState::new();
        state.set_current_atmosphere([1.0, 1.0, 1.0], [1.0, 1.0, 1.0]);
        state.on_commit_transfer(&sample_commit());
        assert!(state.should_suppress_loading_screen("minecraft:the_nether"));
        // Static crossfade (current → current): progress advances, colors stay put.
        state.advance_ms(140.0);
        let (sky, _, p) = state.blended_sky_and_fog().unwrap();
        assert!(p.is_finite() && p > 0.0);
        assert_eq!(sky, [1.0, 1.0, 1.0]);
    }
}

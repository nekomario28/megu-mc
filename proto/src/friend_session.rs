// SPDX-License-Identifier: LGPL-3.0-or-later
//! One-click friend multiplayer: a human-readable `MEGU-XXXX-XXXX-XXXX`
//! invite code, pure-`std` NAT traversal wire formats (NAT-PMP RFC 6886,
//! PCP RFC 6887, UPnP IGD/SSDP+SOAP, STUN RFC 5389) and a compact
//! self-hostable `megu-relay` (`MGR1`) fallback framing protocol — zero
//! external crates, zero central-server lock-in.
//!
//! A player opens a world, explicitly publishes it, and the client walks a
//! deterministic transport plan: direct LAN sharing for [`PublishScope::LanOnly`],
//! otherwise explicit-consent port mapping first, STUN hole-punching next,
//! and a self-hostable relay tunnel as the last-resort fallback. Nothing
//! here opens a router port on its own: NAT mappings are created only
//! inside an explicit publish, and [`PortLeaseGuard`] guarantees the unmap
//! request fires exactly once — on `close()` or on `Drop` — so a port can
//! never linger open after the world closes.
//!
//! ## Invite codes
//!
//! [`InviteCode`] is 60 bits rendered Crockford Base32
//! (`0123456789ABCDEFGHJKMNPQRSTVWXYZ` — `I`/`L`/`O`/`U` are absent so the
//! code cannot be misread) as `MEGU-XXXX-XXXX-XXXX`:
//!
//! ```text
//! bit 59..56  version (4 bits, == 1)
//! bit 55..54  transport mode (2 bits: Direct | HolePunch | SelfHostedRelay)
//! bit 53..44  session/endpoint rendezvous id (10 bits)
//! bit 43..34  expiry slot (10 bits of 32-minute slots, mod 1024)
//! bit 33..16  one-time join token (18 bits)
//! bit 15..0   CRC-16/CCITT-FALSE over the 44 payload bits
//! ```
//!
//! Parsing is deliberately tolerant on surface details — case-insensitive,
//! optional hyphens, optional `MEGU` prefix, `o → 0`, `i/l → 1` — and
//! fail-closed on anything else: wrong digit count, a non-ASCII byte, a
//! character outside the alphabet, a CRC mismatch (typo or tampering), an
//! unknown version, the reserved mode value, or an expiry slot that is
//! already past ([`InviteError::Expired`]). The truncated expiry counter
//! aliases only for codes whose true expiry lies more than
//! [`MAX_INVITE_TTL_SLOTS`] slots (~11.4 days) in the past; by then the
//! one-time join token is spent and the 10-bit session id long recycled,
//! so the join still fails closed downstream.
//!
//! ## NAT traversal codecs
//!
//! * [`encode_natpmp_request`] / [`decode_natpmp_response`] — RFC 6886
//!   external-address and port-mapping datagrams for gateway port 5351.
//! * [`encode_pcp_map_request`] / [`decode_pcp_map_response`] — RFC 6887
//!   version-2 MAP-opcode requests with 96-bit mapping nonces verified on
//!   every response.
//! * [`ssdp_msearch_request`] / [`parse_ssdp_response`],
//!   [`soap_add_port_mapping_body`] / [`soap_delete_port_mapping_body`] /
//!   [`parse_upnp_action_response`] — UPnP IGD discovery and the two WAN
//!   port-mapping SOAP calls. The XML scanner is bounded, namespace-aware
//!   and refuses `<!DOCTYPE`/`<!ENTITY` outright — no XXE surface.
//!
//! ## Consent gating
//!
//! [`FriendPublishPolicy`] defaults to `require_online_auth = true`: a
//! published session never bypasses legitimate Minecraft account
//! authentication (EULA), and `require_online_auth = false` validates only
//! under [`PublishScope::LanOnly`]. [`plan_publish`] returns the ordered
//! [`PublishStep`] list; [`PublishStep::AcquireNatMap`] is emitted only
//! when the user explicitly chose
//! [`PublishConsent::PublishToFriends`] with `allow_port_mapping` — the
//! LAN-share path contains no mapping step even if requested.
//!
//! ## Layers
//!
//! * [`InviteCode`] + [`encode_invite_code`] / [`decode_invite_code`] —
//!   the invite codec.
//! * [`PortLeaseGuard`] — the RAII port-lease wrapper: the unmap callback
//!   runs exactly once, on `close()` or on `Drop`, whichever comes first.
//! * [`HolePunchMachine`] — the deterministic
//!   `Idle -> ProbingStun -> Punching { attempts } -> Connected |
//!   FallbackRelay | Failed` coordinator.
//! * [`RelayFrame`] + [`encode_relay_frame`] / [`decode_relay_frame`] —
//!   the `MGR1` self-host relay codec capped at
//!   [`MAX_RELAY_FRAME_BYTES`], and [`TokenBucket`] /
//!   [`RelaySessionBudget`], the per-session token-bucket rate limiter
//!   that keeps a relay from being turned into an amplifier.
#![forbid(unsafe_code)]

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use megu_core::random::Xoroshiro128pp;

use crate::wire::WireReader;

// ---------- checksums ----------

/// CRC-16/CCITT-FALSE: poly `0x1021`, init `0xFFFF`, no reflection, no
/// final XOR. Backs the [`InviteCode`] checksum.
pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub use megu_core::hash::crc32_ieee;

// ---------- invite code ----------

/// Crockford Base32 alphabet: no `I`, `L`, `O` or `U`.
pub const INVITE_CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Number of Crockford digits in a rendered code (`XXXX-XXXX-XXXX`).
pub const INVITE_CODE_DIGITS: usize = 12;

/// Wire version stamped into every issued [`InviteCode`].
pub const INVITE_VERSION: u8 = 1;

/// Seconds per invite expiry slot (32 minutes).
pub const INVITE_EXPIRY_SLOT_SECS: u64 = 1920;

/// Expiry slots per wrap of the 10-bit slot counter (~22.8 days).
pub const INVITE_EXPIRY_SLOTS: u64 = 1024;

/// Maximum distance a parsed expiry slot may lie ahead of `now`:
/// 512 slots ≈ 11.4 days. Codes issued with a longer TTL are rejected at
/// mint; codes whose true expiry is already past decode to a delta above
/// this bound and fail closed as [`InviteError::Expired`].
pub const MAX_INVITE_TTL_SLOTS: u64 = 512;

/// Maximum TTL accepted by [`InviteCode::issue`].
pub const MAX_INVITE_TTL_SECS: u64 = MAX_INVITE_TTL_SLOTS * INVITE_EXPIRY_SLOT_SECS;

/// Largest legal `session_id` (10 bits).
pub const MAX_INVITE_SESSION_ID: u16 = 1023;

/// Largest legal `join_token` (18 bits).
pub const MAX_INVITE_JOIN_TOKEN: u32 = 262_143;

/// How a friend reaches a published world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FriendTransportMode {
    /// Direct TCP to a published endpoint (LAN or mapped port).
    Direct,
    /// UDP/TCP hole-punched via STUN-discovered reflexive address.
    HolePunch,
    /// Through a self-hostable `megu-relay` (`MGR1`) tunnel.
    SelfHostedRelay,
}

impl FriendTransportMode {
    fn tag(self) -> u64 {
        match self {
            Self::Direct => 0,
            Self::HolePunch => 1,
            Self::SelfHostedRelay => 2,
        }
    }

    fn from_tag(tag: u64) -> Result<Self, InviteError> {
        match tag {
            0 => Ok(Self::Direct),
            1 => Ok(Self::HolePunch),
            2 => Ok(Self::SelfHostedRelay),
            _ => Err(InviteError::ReservedMode),
        }
    }
}

/// Why an invite code string was rejected. Every failure is a hard stop —
/// a malformed, mistyped, tampered or expired code never degrades to a
/// guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InviteError {
    /// After normalisation the code was not exactly
    /// [`INVITE_CODE_DIGITS`] digits.
    BadLength,
    /// A non-ASCII byte or a character outside the Crockford alphabet.
    BadCharacter(u8),
    /// CRC-16 did not match the payload: typo or tampering.
    BadChecksum,
    /// The version field was not [`INVITE_VERSION`].
    UnsupportedVersion(u8),
    /// The two-bit transport mode carried the reserved value `3`.
    ReservedMode,
    /// The expiry slot lies at or behind `now`, or further ahead than
    /// [`MAX_INVITE_TTL_SLOTS`] (a stale code's slot wraps there too).
    Expired,
    /// [`InviteCode::issue`] was asked for a TTL outside
    /// `1..=MAX_INVITE_TTL_SLOTS` slots.
    TtlOutOfRange,
    /// [`InviteCode::issue`] was given a field wider than its bit budget.
    FieldOutOfRange(&'static str),
}

impl fmt::Display for InviteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadLength => write!(f, "invite code is not 12 digits"),
            Self::BadCharacter(b) => write!(f, "invite code holds byte 0x{b:02X}"),
            Self::BadChecksum => write!(f, "invite code checksum mismatch"),
            Self::UnsupportedVersion(v) => write!(f, "invite code version {v} unsupported"),
            Self::ReservedMode => write!(f, "invite code uses reserved transport mode"),
            Self::Expired => write!(f, "invite code expired"),
            Self::TtlOutOfRange => write!(f, "invite TTL out of range"),
            Self::FieldOutOfRange(w) => write!(f, "invite field out of range: {w}"),
        }
    }
}

impl std::error::Error for InviteError {}

/// A parsed/issued `MEGU-XXXX-XXXX-XXXX` invite. The code is a rendezvous
/// capability: the 10-bit `session_id` names the session on whichever
/// rendezvous plane the `transport` implies (LAN beacon, hole-punch
/// coordinator, relay table) and the 18-bit `join_token` is a one-time
/// admission secret — single-use by contract and rate-limited by
/// [`RelaySessionBudget`] on the relay path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteCode {
    /// How the friend should reach the world.
    pub transport: FriendTransportMode,
    /// 10-bit rendezvous/session key (`<= MAX_INVITE_SESSION_ID`).
    pub session_id: u16,
    /// 18-bit one-time join secret (`<= MAX_INVITE_JOIN_TOKEN`).
    pub join_token: u32,
    /// Raw 10-bit expiry slot counter, mod [`INVITE_EXPIRY_SLOTS`].
    expiry_slot: u16,
    /// Absolute expiry decoded against the `now` used at issue/parse.
    pub expires_at_unix_secs: u64,
}

impl InviteCode {
    /// Mint a code. `ttl_secs` is rounded up to whole
    /// [`INVITE_EXPIRY_SLOT_SECS`] slots and must land in
    /// `1..=MAX_INVITE_TTL_SLOTS` slots (~32 minutes to ~11.4 days).
    pub fn issue(
        transport: FriendTransportMode,
        session_id: u16,
        join_token: u32,
        now_unix_secs: u64,
        ttl_secs: u64,
    ) -> Result<Self, InviteError> {
        if session_id > MAX_INVITE_SESSION_ID {
            return Err(InviteError::FieldOutOfRange("session_id"));
        }
        if join_token > MAX_INVITE_JOIN_TOKEN {
            return Err(InviteError::FieldOutOfRange("join_token"));
        }
        let slots = ttl_secs.div_ceil(INVITE_EXPIRY_SLOT_SECS);
        if slots == 0 || slots > MAX_INVITE_TTL_SLOTS {
            return Err(InviteError::TtlOutOfRange);
        }
        let slot_now = now_unix_secs / INVITE_EXPIRY_SLOT_SECS;
        let expiry_slot = ((slot_now + slots) % INVITE_EXPIRY_SLOTS) as u16;
        Ok(Self {
            transport,
            session_id,
            join_token,
            expiry_slot,
            expires_at_unix_secs: (slot_now + slots) * INVITE_EXPIRY_SLOT_SECS,
        })
    }

    /// Mint a code drawing `session_id` and `join_token` from `rng`.
    pub fn issue_with_rng(
        rng: &mut Xoroshiro128pp,
        transport: FriendTransportMode,
        now_unix_secs: u64,
        ttl_secs: u64,
    ) -> Result<Self, InviteError> {
        let session_id = rng.next_int_bounded(i32::from(MAX_INVITE_SESSION_ID) + 1) as u16;
        let join_token = rng.next_int_bounded(MAX_INVITE_JOIN_TOKEN as i32 + 1) as u32;
        Self::issue(transport, session_id, join_token, now_unix_secs, ttl_secs)
    }

    /// The raw 10-bit expiry slot carried on the wire.
    pub fn expiry_slot(&self) -> u16 {
        self.expiry_slot
    }

    /// Render `MEGU-XXXX-XXXX-XXXX`.
    pub fn render(&self) -> String {
        render_invite(self.pack())
    }

    /// Pack version, mode, session, slot, token and CRC into 60 bits.
    fn pack(&self) -> u64 {
        let payload: u64 = (u64::from(INVITE_VERSION) << 40)
            | (self.transport.tag() << 38)
            | (u64::from(self.session_id) << 28)
            | (u64::from(self.expiry_slot) << 18)
            | u64::from(self.join_token);
        let crc = u64::from(crc16_ccitt(&payload.to_be_bytes()));
        (payload << 16) | crc
    }

    /// Parse a code back, verifying checksum, version, mode and expiry
    /// against `now_unix_secs`.
    pub fn parse(text: &str, now_unix_secs: u64) -> Result<Self, InviteError> {
        let word = decode_invite_code(text)?;
        let payload = word >> 16;
        let crc = (word & 0xFFFF) as u16;
        if crc16_ccitt(&payload.to_be_bytes()) != crc {
            return Err(InviteError::BadChecksum);
        }
        let version = (payload >> 40) as u8;
        if version != INVITE_VERSION {
            return Err(InviteError::UnsupportedVersion(version));
        }
        let transport = FriendTransportMode::from_tag((payload >> 38) & 0x3)?;
        let session_id = ((payload >> 28) & 0x3FF) as u16;
        let expiry_slot = ((payload >> 18) & 0x3FF) as u16;
        let join_token = (payload & 0x3_FFFF) as u32;

        let slot_now = now_unix_secs / INVITE_EXPIRY_SLOT_SECS;
        let delta = (i64::from(expiry_slot) - (slot_now % INVITE_EXPIRY_SLOTS) as i64)
            .rem_euclid(INVITE_EXPIRY_SLOTS as i64);
        if delta == 0 || delta > MAX_INVITE_TTL_SLOTS as i64 {
            return Err(InviteError::Expired);
        }
        Ok(Self {
            transport,
            session_id,
            join_token,
            expiry_slot,
            expires_at_unix_secs: (slot_now + delta as u64) * INVITE_EXPIRY_SLOT_SECS,
        })
    }
}

/// Encode a packed 60-bit invite word as `MEGU-XXXX-XXXX-XXXX`.
pub fn encode_invite_code(code: &InviteCode) -> String {
    code.render()
}

fn render_invite(word: u64) -> String {
    let mut out = String::with_capacity(4 + INVITE_CODE_DIGITS + 2);
    out.push_str("MEGU-");
    for i in 0..INVITE_CODE_DIGITS {
        let shift = 55 - i * 5;
        let d = ((word >> shift) & 0x1F) as usize;
        out.push(INVITE_CODE_ALPHABET[d] as char);
        if i == 3 || i == 7 {
            out.push('-');
        }
    }
    out
}

fn decode_crockford(byte: u8) -> Option<u64> {
    let b = byte.to_ascii_uppercase();
    let b = match b {
        b'O' => b'0',
        b'I' | b'L' => b'1',
        _ => b,
    };
    INVITE_CODE_ALPHABET
        .iter()
        .position(|&c| c == b)
        .map(|p| p as u64)
}

/// Parse an invite code string into its 60-bit payload+checksum word,
/// tolerating case, hyphens, the `MEGU` prefix and `O`/`I`/`L`
/// misreadings — and failing closed on everything else.
///
/// The `MEGU` prefix is unambiguous: `U` is absent from the Crockford
/// alphabet, so a leading `M-E-G-U` can never be mistaken for digits.
pub fn decode_invite_code(text: &str) -> Result<u64, InviteError> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b' ') {
        i += 1;
    }
    if bytes.len() - i >= 4 && bytes[i..i + 4].eq_ignore_ascii_case(b"MEGU") {
        i += 4;
    }
    let mut digits = [0u64; INVITE_CODE_DIGITS];
    let mut n = 0usize;
    for &b in &bytes[i..] {
        if b == b'-' || b == b' ' {
            continue;
        }
        if !b.is_ascii() {
            return Err(InviteError::BadCharacter(b));
        }
        match decode_crockford(b) {
            Some(v) => {
                if n >= INVITE_CODE_DIGITS {
                    return Err(InviteError::BadLength);
                }
                digits[n] = v;
                n += 1;
            }
            None => return Err(InviteError::BadCharacter(b)),
        }
    }
    if n != INVITE_CODE_DIGITS {
        return Err(InviteError::BadLength);
    }
    let mut word = 0u64;
    for d in digits {
        word = (word << 5) | d;
    }
    Ok(word)
}

// ---------- publish policy & consent ----------

/// Where a published session may be reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishScope {
    /// LAN broadcast/beacon only — no internet exposure at all.
    LanOnly,
    /// Reachable from the public internet via the transport plan.
    PublicInternet,
}

/// EULA-preserving rules attached to every publish. The default keeps
/// `require_online_auth` on: a published world never relaxes legitimate
/// Minecraft account verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FriendPublishPolicy {
    /// Reachability scope.
    pub scope: PublishScope,
    /// Require joining players to hold authenticated (online-mode)
    /// accounts. May be disabled only for [`PublishScope::LanOnly`].
    pub require_online_auth: bool,
}

impl Default for FriendPublishPolicy {
    fn default() -> Self {
        Self {
            scope: PublishScope::PublicInternet,
            require_online_auth: true,
        }
    }
}

/// Policy validation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyViolation {
    /// `require_online_auth = false` is only legal for
    /// [`PublishScope::LanOnly`] — offline guests on the public internet
    /// would bypass account authentication.
    OfflineAuthRequiresLan,
}

impl fmt::Display for PolicyViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OfflineAuthRequiresLan => write!(
                f,
                "offline-mode guests are only allowed for LAN-only scope"
            ),
        }
    }
}

impl std::error::Error for PolicyViolation {}

impl FriendPublishPolicy {
    /// Enforce the auth rule: offline mode is a LAN-local escape hatch,
    /// never an internet one.
    pub fn validate(&self) -> Result<(), PolicyViolation> {
        if !self.require_online_auth && self.scope != PublishScope::LanOnly {
            return Err(PolicyViolation::OfflineAuthRequiresLan);
        }
        Ok(())
    }
}

/// What the user actually clicked. Port mapping exists only inside
/// [`PublishConsent::PublishToFriends`] with `allow_port_mapping` —
/// there is no path from "share" to an open router port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishConsent {
    /// The user dismissed the dialog: publish nothing.
    Cancelled,
    /// "Share to LAN": local discovery only, never a NAT mapping.
    ShareLanOnly,
    /// "Publish to friends": internet reachability. `allow_port_mapping`
    /// is the separate checkbox that alone permits UPnP/NAT-PMP/PCP
    /// mapping attempts.
    PublishToFriends {
        /// User explicitly allowed router port mapping.
        allow_port_mapping: bool,
    },
}

/// One ordered step of a publish plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishStep {
    /// Advertise on the LAN beacon only.
    LanDiscoveryOnly,
    /// Ask the gateway for a port lease (UPnP, then NAT-PMP/PCP); the
    /// lease is wrapped in [`PortLeaseGuard`] so it always unmaps.
    AcquireNatMap,
    /// Discover the reflexive address via STUN, then hole-punch.
    ProbeStunAndPunch,
    /// Fall back to a self-hostable `megu-relay` tunnel.
    OpenRelayTunnel,
}

/// `plan_publish` failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishError {
    /// The user cancelled; nothing is published.
    ConsentDenied,
    /// The policy itself is inconsistent.
    Policy(PolicyViolation),
    /// The chosen consent cannot reach the requested scope (a LAN share
    /// can never satisfy `PublishScope::PublicInternet` callers, and a
    /// publish without port mapping still works via punch/relay, so this
    /// is reserved for future scope mismatches).
    ScopeMismatch,
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsentDenied => write!(f, "publish cancelled by user"),
            Self::Policy(v) => write!(f, "publish policy rejected: {v}"),
            Self::ScopeMismatch => write!(f, "consent cannot satisfy the scope"),
        }
    }
}

impl std::error::Error for PublishError {}

impl From<PolicyViolation> for PublishError {
    fn from(v: PolicyViolation) -> Self {
        Self::Policy(v)
    }
}

/// Compute the ordered transport plan for a publish request. The consent
/// object is the *only* source of permission: `AcquireNatMap` appears in
/// the plan iff the user explicitly published with `allow_port_mapping`.
/// A `ShareLanOnly` consent yields `[LanDiscoveryOnly]` even when the
/// policy scope is `PublicInternet` — the plan widens silently.
pub fn plan_publish(
    policy: &FriendPublishPolicy,
    consent: PublishConsent,
) -> Result<Vec<PublishStep>, PublishError> {
    match consent {
        PublishConsent::Cancelled => Err(PublishError::ConsentDenied),
        PublishConsent::ShareLanOnly => Ok(vec![PublishStep::LanDiscoveryOnly]),
        PublishConsent::PublishToFriends {
            allow_port_mapping,
        } => {
            policy.validate()?;
            if policy.scope == PublishScope::LanOnly {
                return Ok(vec![PublishStep::LanDiscoveryOnly]);
            }
            let mut steps = Vec::with_capacity(3);
            if allow_port_mapping {
                steps.push(PublishStep::AcquireNatMap);
            }
            steps.push(PublishStep::ProbeStunAndPunch);
            steps.push(PublishStep::OpenRelayTunnel);
            Ok(steps)
        }
    }
}

// ---------- NAT-PMP (RFC 6886) ----------

/// Gateway UDP port for NAT-PMP and PCP.
pub const NATPMP_SERVER_PORT: u16 = 5351;

/// L4 protocol a mapping applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatProtocol {
    /// UDP (NAT-PMP opcode 1, PCP protocol 17).
    Udp,
    /// TCP (NAT-PMP opcode 2, PCP protocol 6).
    Tcp,
}

impl NatProtocol {
    fn pmp_opcode(self) -> u8 {
        match self {
            Self::Udp => 1,
            Self::Tcp => 2,
        }
    }

    /// IANA protocol number used by PCP's `Protocol` field.
    pub fn iana_number(self) -> u8 {
        match self {
            Self::Udp => 17,
            Self::Tcp => 6,
        }
    }

    /// UPnP `NewProtocol` literal.
    pub fn upnp_name(self) -> &'static str {
        match self {
            Self::Udp => "UDP",
            Self::Tcp => "TCP",
        }
    }
}

/// A port-mapping request shared by NAT-PMP, PCP and UPnP encoders.
/// `lifetime_secs == 0` deletes the mapping (NAT-PMP §3.3, PCP §15,
/// UPnP: `NewLeaseDuration = 0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NatMapRequest {
    /// Mapped protocol.
    pub protocol: NatProtocol,
    /// LAN-side port the game listens on.
    pub internal_port: u16,
    /// Desired WAN-side port; 0 = no preference.
    pub suggested_external_port: u16,
    /// Lease seconds; 0 deletes.
    pub lifetime_secs: u32,
}

/// A NAT-PMP request datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatPmpRequest {
    /// Opcode 0: learn the gateway's external IPv4 address.
    ExternalAddress,
    /// Opcode 1/2: create or delete a port mapping.
    MapPort(NatMapRequest),
}

/// Encode an RFC 6886 request: `version(1)=0 | op(1)` then the
/// opcode-specific body.
pub fn encode_natpmp_request(req: &NatPmpRequest) -> Vec<u8> {
    match *req {
        NatPmpRequest::ExternalAddress => vec![0, 0],
        NatPmpRequest::MapPort(m) => {
            let mut out = Vec::with_capacity(12);
            out.extend_from_slice(&[0, m.protocol.pmp_opcode(), 0, 0]);
            out.extend_from_slice(&m.internal_port.to_be_bytes());
            out.extend_from_slice(&m.suggested_external_port.to_be_bytes());
            out.extend_from_slice(&m.lifetime_secs.to_be_bytes());
            out
        }
    }
}

/// A decoded NAT-PMP response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatPmpResponse {
    /// Opcode 128: gateway epoch + external IPv4 address.
    ExternalAddress {
        /// Seconds since the gateway's NAT table last reset.
        epoch_secs: u32,
        /// The WAN IPv4 address.
        address: Ipv4Addr,
    },
    /// Opcode 129/130: a mapping grant (or deletion acknowledgement,
    /// lifetime 0).
    Mapped {
        /// Gateway epoch.
        epoch_secs: u32,
        /// The mapped protocol.
        protocol: NatProtocol,
        /// Internal port echoed back.
        internal_port: u16,
        /// Assigned external port.
        assigned_external_port: u16,
        /// Granted lifetime in seconds.
        lifetime_secs: u32,
    },
}

/// Everything that can be wrong with a NAT-PMP/PCP datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NatMapError {
    /// Fewer bytes than the message requires.
    Truncated,
    /// Unexpected protocol version byte.
    BadVersion(u8),
    /// The R/opcode discriminator says this is not a response.
    NotAResponse,
    /// Unknown or mismatched opcode.
    BadOpcode(u8),
    /// The gateway reported a failure code.
    Gateway(u8),
    /// A PCP mapping nonce did not echo our request.
    NonceMismatch,
    /// A PCP address field was not a legal IPv4/IPv6 value.
    BadAddress,
}

impl fmt::Display for NatMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "NAT message truncated"),
            Self::BadVersion(v) => write!(f, "unexpected NAT protocol version {v}"),
            Self::NotAResponse => write!(f, "datagram is not a response"),
            Self::BadOpcode(o) => write!(f, "unexpected opcode {o}"),
            Self::Gateway(c) => write!(f, "gateway returned error code {c}"),
            Self::NonceMismatch => write!(f, "PCP nonce mismatch"),
            Self::BadAddress => write!(f, "illegal address in response"),
        }
    }
}

impl std::error::Error for NatMapError {}

/// Decode an RFC 6886 response. `op` must be `128 + request_opcode`,
/// version 0, with the exact body length for the opcode; a non-zero
/// result code fails closed as [`NatMapError::Gateway`].
pub fn decode_natpmp_response(bytes: &[u8]) -> Result<NatPmpResponse, NatMapError> {
    if bytes.len() < 12 {
        return Err(NatMapError::Truncated);
    }
    if bytes[0] != 0 {
        return Err(NatMapError::BadVersion(bytes[0]));
    }
    let op = bytes[1];
    if op & 0x80 == 0 {
        return Err(NatMapError::NotAResponse);
    }
    let req_op = op & 0x7F;
    if req_op > 2 {
        return Err(NatMapError::BadOpcode(op));
    }
    let result = u16::from_be_bytes([bytes[2], bytes[3]]);
    if result != 0 {
        return Err(NatMapError::Gateway(result as u8));
    }
    let epoch_secs = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if req_op == 0 {
        if bytes.len() != 12 {
            return Err(NatMapError::Truncated);
        }
        return Ok(NatPmpResponse::ExternalAddress {
            epoch_secs,
            address: Ipv4Addr::new(bytes[8], bytes[9], bytes[10], bytes[11]),
        });
    }
    if bytes.len() != 16 {
        return Err(NatMapError::Truncated);
    }
    let protocol = if req_op == 1 {
        NatProtocol::Udp
    } else {
        NatProtocol::Tcp
    };
    Ok(NatPmpResponse::Mapped {
        epoch_secs,
        protocol,
        internal_port: u16::from_be_bytes([bytes[8], bytes[9]]),
        assigned_external_port: u16::from_be_bytes([bytes[10], bytes[11]]),
        lifetime_secs: u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
    })
}

// ---------- PCP (RFC 6887) ----------

/// PCP protocol version (RFC 6887 finalises version 2).
pub const PCP_VERSION: u8 = 2;

/// MAP opcode (create/renew/delete a port mapping).
pub const PCP_OPCODE_MAP: u8 = 1;

/// Mapping nonce width: 96 bits.
pub const PCP_MAP_NONCE_BYTES: usize = 12;

/// Total size of a MAP request/response without options: 24 header + 36.
pub const PCP_MAP_MESSAGE_BYTES: usize = 60;

fn ip_to_pcp_bytes(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    }
}

fn pcp_bytes_to_ip(octets: [u8; 16]) -> IpAddr {
    let v6 = Ipv6Addr::from(octets);
    match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    }
}

/// Encode an RFC 6887 MAP request (60 bytes):
/// `(ver<<1)|R | opcode | reserved(16) | lifetime(32) | client IP (128)`
/// then nonce(96), IANA protocol(8), reserved(24), internal port,
/// suggested external port, suggested external address (v4-mapped).
pub fn encode_pcp_map_request(
    client_ip: IpAddr,
    nonce: [u8; PCP_MAP_NONCE_BYTES],
    req: &NatMapRequest,
    suggested_external_addr: Option<IpAddr>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(PCP_MAP_MESSAGE_BYTES);
    out.push(PCP_VERSION << 1); // R = 0 (request)
    out.push(PCP_OPCODE_MAP);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&req.lifetime_secs.to_be_bytes());
    out.extend_from_slice(&ip_to_pcp_bytes(client_ip));
    out.extend_from_slice(&nonce);
    out.push(req.protocol.iana_number());
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&req.internal_port.to_be_bytes());
    out.extend_from_slice(&req.suggested_external_port.to_be_bytes());
    out.extend_from_slice(&ip_to_pcp_bytes(
        suggested_external_addr.unwrap_or(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
    ));
    out
}

/// A decoded PCP MAP response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcpMapResponse {
    /// Server epoch (for reboot detection, RFC 6887 §8.5).
    pub epoch_secs: u32,
    /// Granted lifetime in seconds.
    pub lifetime_secs: u32,
    /// The echoed mapping nonce.
    pub nonce: [u8; PCP_MAP_NONCE_BYTES],
    /// IANA protocol number echoed by the server.
    pub protocol: u8,
    /// Internal port echoed by the server.
    pub internal_port: u16,
    /// The external port the gateway actually assigned.
    pub assigned_external_port: u16,
    /// The external address the gateway assigned.
    pub assigned_external_addr: IpAddr,
}

/// Decode a PCP MAP response, verifying version 2, the response bit, the
/// MAP opcode, a zero result code and the echoed `expected_nonce`.
/// Trailing option bytes are ignored — RFC 6887 options are
/// self-delimiting and this codec requests none.
pub fn decode_pcp_map_response(
    bytes: &[u8],
    expected_nonce: &[u8; PCP_MAP_NONCE_BYTES],
) -> Result<PcpMapResponse, NatMapError> {
    if bytes.len() < PCP_MAP_MESSAGE_BYTES {
        return Err(NatMapError::Truncated);
    }
    if bytes[0] >> 1 != PCP_VERSION {
        return Err(NatMapError::BadVersion(bytes[0] >> 1));
    }
    if bytes[0] & 1 == 0 {
        return Err(NatMapError::NotAResponse);
    }
    if bytes[1] & 0x7F != PCP_OPCODE_MAP {
        return Err(NatMapError::BadOpcode(bytes[1] & 0x7F));
    }
    let result = bytes[3];
    if result != 0 {
        return Err(NatMapError::Gateway(result));
    }
    let lifetime_secs = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let epoch_secs = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let mut nonce = [0u8; PCP_MAP_NONCE_BYTES];
    nonce.copy_from_slice(&bytes[24..36]);
    if &nonce != expected_nonce {
        return Err(NatMapError::NonceMismatch);
    }
    let mut addr = [0u8; 16];
    addr.copy_from_slice(&bytes[44..60]);
    Ok(PcpMapResponse {
        epoch_secs,
        lifetime_secs,
        nonce,
        protocol: bytes[36],
        internal_port: u16::from_be_bytes([bytes[40], bytes[41]]),
        assigned_external_port: u16::from_be_bytes([bytes[42], bytes[43]]),
        assigned_external_addr: pcp_bytes_to_ip(addr),
    })
}

// ---------- UPnP IGD: SSDP + SOAP ----------

/// SSDP multicast destination.
pub const SSDP_MULTICAST_ADDR: &str = "239.255.255.250:1900";

/// SSDP search target for an Internet Gateway Device.
pub const UPNP_IGD_DEVICE: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";

/// WANIPConnection service type (routed IGDs).
pub const UPNP_WANIP_SERVICE: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";

/// WANPPPConnection service type (bridged/DSL IGDs).
pub const UPNP_WANPPP_SERVICE: &str = "urn:schemas-upnp-org:service:WANPPPConnection:1";

/// Hard cap on any single SSDP response or device-description document.
pub const UPNP_MAX_XML_BYTES: usize = 64 * 1024;

/// SSDP/UPnP failure modes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpnpError {
    /// Input exceeded [`UPNP_MAX_XML_BYTES`] or the SSDP cap.
    Oversize,
    /// Input was not printable HTTP/XML text.
    NotAscii,
    /// SSDP datagram was not an `HTTP/1.1 200` response.
    NotResponse,
    /// A required SSDP header or XML element was absent.
    Missing(&'static str),
    /// The XML contained `<!DOCTYPE` or `<!ENTITY` — XXE is refused
    /// outright.
    ForbiddenDoctype,
    /// A field destined for an HTTP request line/header or an XML tag
    /// held a byte that could inject or split it: CR, LF, NUL, a space,
    /// a quote, a non-printable/non-ASCII byte, or a URL whose scheme or
    /// authority is not the expected `http(s)` origin. Carries the field
    /// name for logging.
    InvalidToken(&'static str),
    /// The gateway answered a SOAP Fault; carries `<errorCode>` (e.g.
    /// 402 `Invalid Args`, 501 `Action Failed`, 718 `ConflictInMapping`).
    Soap { code: u32 },
}

impl fmt::Display for UpnpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize => write!(f, "UPnP document too large"),
            Self::NotAscii => write!(f, "UPnP payload was not text"),
            Self::NotResponse => write!(f, "not an SSDP success response"),
            Self::Missing(w) => write!(f, "UPnP response missing {w}"),
            Self::ForbiddenDoctype => write!(f, "UPnP XML declares a DOCTYPE/ENTITY"),
            Self::InvalidToken(w) => write!(f, "UPnP {w} is not a safe token"),
            Self::Soap { code } => write!(f, "UPnP SOAP fault, errorCode {code}"),
        }
    }
}

impl std::error::Error for UpnpError {}

/// Build an SSDP `M-SEARCH` request for `st` (e.g.
/// [`UPNP_IGD_DEVICE`]). `st` must be non-empty printable ASCII without
/// CR/LF and at most 128 bytes — injection into the request line is
/// impossible by construction.
pub fn ssdp_msearch_request(st: &str) -> Result<Vec<u8>, UpnpError> {
    if st.is_empty()
        || st.len() > 128
        || !st.bytes().all(|b| (0x20..=0x7E).contains(&b))
    {
        return Err(UpnpError::NotAscii);
    }
    Ok(format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_MULTICAST_ADDR}\r\n\
         MAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n"
    )
    .into_bytes())
}

/// One discovered IGD from an SSDP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsdpDevice {
    /// `LOCATION:` — URL of the device description document.
    pub location: String,
    /// `ST:` echo.
    pub st: String,
    /// `USN:` unique service name.
    pub usn: String,
    /// `SERVER:` string, if present.
    pub server: String,
}

/// Parse an SSDP `HTTP/1.1 200 OK` response: headers are matched
/// case-insensitively, `LOCATION` and `ST` are required, anything else
/// fails closed.
pub fn parse_ssdp_response(bytes: &[u8]) -> Result<SsdpDevice, UpnpError> {
    if bytes.len() > 8192 {
        return Err(UpnpError::Oversize);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| UpnpError::NotAscii)?;
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap_or("");
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        return Err(UpnpError::NotResponse);
    }
    let mut location = None;
    let mut st = None;
    let mut usn = None;
    let mut server = String::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_uppercase().as_str() {
            "LOCATION" => location = Some(value.to_string()),
            "ST" => st = Some(value.to_string()),
            "USN" => usn = Some(value.to_string()),
            "SERVER" => server = value.to_string(),
            _ => {}
        }
    }
    Ok(SsdpDevice {
        location: location.ok_or(UpnpError::Missing("LOCATION"))?,
        st: st.ok_or(UpnpError::Missing("ST"))?,
        usn: usn.unwrap_or_default(),
        server,
    })
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

// ---------- HTTP/SOAP injection guards ----------
//
// Everything that lands in a request line, a header value or an XML tag
// name is whitelisted byte-by-byte *before* formatting. A malicious IGD
// answering SSDP with a doctored device description — `controlURL`
// carrying `\r\nX-Injected:`, a `LOCATION` pointing at `file:` or a
// quoted `host` — can never split a request or smuggle a second one.

/// Bytes legal in an HTTP `authority` component: hostname, IPv4 literal,
/// bracketed IPv6 literal, `:port`. Whitespace, `@` (userinfo smuggle),
/// `/`, quotes and control bytes are all absent.
fn is_authority_byte(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b':' | b'[' | b']')
}

/// Bytes legal in an origin-form request target: RFC 3986 `pchar`, `/`
/// and `?`. No whitespace, controls, `"`, `<`, `>`, `\` or `#`.
fn is_request_target_byte(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
        | b'-' | b'.' | b'_' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
        | b'*' | b'+' | b',' | b';' | b'=' | b':' | b'@' | b'/' | b'?' | b'%')
}

/// Bytes legal inside a SOAP service URN or action name: printable ASCII
/// except `"` (would escape the `SOAPACTION`/`xmlns` quotes), `<` and
/// `>` (would escape the tag name).
fn is_soap_token_byte(b: u8) -> bool {
    (0x21..=0x7E).contains(&b) && !matches!(b, b'"' | b'<' | b'>')
}

fn validate_authority(host: &str) -> Result<&str, UpnpError> {
    if host.is_empty() || host.len() > 255 || !host.bytes().all(is_authority_byte) {
        return Err(UpnpError::InvalidToken("host"));
    }
    Ok(host)
}

fn validate_request_target(path: &str) -> Result<&str, UpnpError> {
    // `//` would be a network-path reference (`//authority/path`), which
    // some stacks reinterpret as an authority — refuse it outright.
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.len() > 2048
        || !path.bytes().all(is_request_target_byte)
    {
        return Err(UpnpError::InvalidToken("control_path"));
    }
    Ok(path)
}

fn validate_soap_token<'a>(s: &'a str, field: &'static str) -> Result<&'a str, UpnpError> {
    if s.is_empty() || s.len() > 256 || !s.bytes().all(is_soap_token_byte) {
        return Err(UpnpError::InvalidToken(field));
    }
    Ok(s)
}

/// `true` when `url` opens with a URI scheme (`alpha *( alnum / "+" /
/// "-" / "." ) ":"`) before any `/` — used to reject every scheme that
/// is not `http`/`https`.
fn has_foreign_scheme(url: &str) -> bool {
    let Some(colon) = url.find(':') else {
        return false;
    };
    if let Some(slash) = url.find('/') {
        if slash < colon {
            return false; // the ':' belongs to the path, not a scheme
        }
    }
    let scheme = &url[..colon];
    !scheme.is_empty()
        && scheme
            .bytes()
            .enumerate()
            .all(|(i, b)| {
                if i == 0 {
                    b.is_ascii_alphabetic()
                } else {
                    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
                }
            })
}

/// Split an absolute `http://`/`https://` URL into its validated
/// `(authority, request-target)` pair. `Ok(None)` means the input was
/// not an absolute URL at all; any other scheme fails closed.
fn split_http_url(url: &str) -> Result<Option<(String, String)>, UpnpError> {
    let rest = if let Some(r) = url.strip_prefix("http://") {
        r
    } else if let Some(r) = url.strip_prefix("https://") {
        r
    } else {
        if has_foreign_scheme(url) {
            return Err(UpnpError::InvalidToken("url"));
        }
        return Ok(None);
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    validate_authority(authority)?;
    validate_request_target(path)?;
    Ok(Some((authority.to_string(), path.to_string())))
}

/// The validated `HOST` + request-target a SOAP POST is sent to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpnpControlTarget {
    /// HTTP `authority` (`host[:port]` or `[v6]:port`).
    pub host: String,
    /// Origin-form request target (`/path[?query]`).
    pub path: String,
}

/// Resolve a `<controlURL>` against the SSDP `LOCATION` it was found in.
/// Absolute control URLs keep their own origin — but only if that origin
/// is `http`/`https` with a clean authority; any other scheme
/// (`file:`, `gopher:`, `ftp:`, …) or any byte that could inject into
/// the request line fails closed. Relative control URLs resolve against
/// the location's directory, per the UPnP base rules.
pub fn resolve_upnp_control_target(
    control_url: &str,
    device_location: &str,
) -> Result<UpnpControlTarget, UpnpError> {
    let control_url = control_url.trim();
    if control_url.is_empty() {
        return Err(UpnpError::InvalidToken("controlURL"));
    }
    if let Some((host, path)) = split_http_url(control_url)? {
        return Ok(UpnpControlTarget { host, path });
    }
    let (base_host, base_path) =
        split_http_url(device_location)?.ok_or(UpnpError::InvalidToken("LOCATION"))?;
    let path = if control_url.starts_with('/') {
        control_url.to_string()
    } else {
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..=i],
            None => "/",
        };
        let mut p = String::with_capacity(dir.len() + control_url.len());
        p.push_str(dir);
        p.push_str(control_url);
        p
    };
    validate_request_target(&path)?;
    Ok(UpnpControlTarget {
        host: base_host,
        path,
    })
}

/// Wrap `inner` (already-element XML) in a SOAP envelope for `action` on
/// `service` (e.g. [`UPNP_WANIP_SERVICE`]). `service` and `action` are
/// whitelisted: they land inside a quoted `xmlns` attribute and a tag
/// name, so `"`/`<`/`>` can never appear.
pub fn soap_envelope(service: &str, action: &str, inner: &str) -> Result<String, UpnpError> {
    let service = validate_soap_token(service, "service")?;
    let action = validate_soap_token(action, "action")?;
    Ok(format!(
        "<?xml version=\"1.0\"?>\r\n\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service}\">{inner}</u:{action}>\
         </s:Body></s:Envelope>\r\n"
    ))
}

/// SOAP body for `AddPortMapping`: `NewRemoteHost` may be `""` (wildcard
/// — the WAN port answers for any remote host).
pub fn soap_add_port_mapping_body(
    service: &str,
    external_port: u16,
    protocol: NatProtocol,
    internal_port: u16,
    internal_client: Ipv4Addr,
    description: &str,
    lease_secs: u32,
) -> Result<String, UpnpError> {
    let inner = format!(
        "<NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{external_port}</NewExternalPort>\
         <NewProtocol>{}</NewProtocol>\
         <NewInternalPort>{internal_port}</NewInternalPort>\
         <NewInternalClient>{internal_client}</NewInternalClient>\
         <NewEnabled>1</NewEnabled>\
         <NewPortMappingDescription>{}</NewPortMappingDescription>\
         <NewLeaseDuration>{lease_secs}</NewLeaseDuration>",
        protocol.upnp_name(),
        xml_escape(description),
    );
    soap_envelope(service, "AddPortMapping", &inner)
}

/// SOAP body for `DeletePortMapping` — the unmap [`PortLeaseGuard`]
/// ultimately arranges.
pub fn soap_delete_port_mapping_body(
    service: &str,
    external_port: u16,
    protocol: NatProtocol,
) -> Result<String, UpnpError> {
    let inner = format!(
        "<NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{external_port}</NewExternalPort>\
         <NewProtocol>{}</NewProtocol>",
        protocol.upnp_name()
    );
    soap_envelope(service, "DeletePortMapping", &inner)
}

/// A complete `POST` request for a SOAP action against `control_path` on
/// `host` (the authority part of the SSDP `LOCATION`). Every field that
/// lands on the request line or in a header is validated first:
/// `host` must be a clean authority, `control_path` a clean origin-form
/// target, `service`/`action` quote- and angle-bracket-free — CR, LF,
/// NUL, whitespace, non-printable or non-ASCII bytes anywhere are an
/// [`UpnpError::InvalidToken`], never a formatted request.
pub fn upnp_http_post_request(
    host: &str,
    control_path: &str,
    service: &str,
    action: &str,
    body: &str,
) -> Result<Vec<u8>, UpnpError> {
    let host = validate_authority(host)?;
    let control_path = validate_request_target(control_path)?;
    let service = validate_soap_token(service, "service")?;
    let action = validate_soap_token(action, "action")?;
    Ok(format!(
        "POST {control_path} HTTP/1.1\r\nHOST: {host}\r\n\
         CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         CONTENT-LENGTH: {}\r\n\
         SOAPACTION: \"{service}#{action}\"\r\n\
         CONNECTION: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes())
}

fn xml_check_bounded(xml: &[u8]) -> Result<&str, UpnpError> {
    if xml.len() > UPNP_MAX_XML_BYTES {
        return Err(UpnpError::Oversize);
    }
    let text = std::str::from_utf8(xml).map_err(|_| UpnpError::NotAscii)?;
    let lower = text.to_ascii_lowercase();
    if lower.contains("<!doctype") || lower.contains("<!entity") {
        return Err(UpnpError::ForbiddenDoctype);
    }
    Ok(text)
}

/// Extract the inner text of the first element whose local name matches
/// `want` (namespace prefixes ignored). Returns `None` when absent.
fn xml_extract<'a>(xml: &'a str, want: &str) -> Option<&'a str> {
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let end = after.find('>')?;
        let tag = after[..end].trim_end_matches('/');
        let tag_name = tag
            .split([' ', '\t', '\r', '\n'])
            .next()
            .unwrap_or("");
        let local = tag_name.rsplit(':').next().unwrap_or(tag_name);
        if local != want || tag_name.starts_with('/') {
            rest = &after[end..];
            continue;
        }
        let body_start = open + 1 + end + 1;
        let body = &rest[body_start..];
        // Find the matching close tag: `</prefix:want>` — namespace may differ.
        let mut scan = body;
        while let Some(close) = scan.find("</") {
            let tail = &scan[close + 2..];
            let cend = tail.find('>')?;
            let cname = tail[..cend].trim();
            if cname.rsplit(':').next().unwrap_or(cname) == want {
                return Some(&body[..close + (body.len() - scan.len())]);
            }
            scan = &tail[cend + 1..];
        }
        return None;
    }
    None
}

/// Parse a SOAP action response. Success is silence: `AddPortMapping`
/// and `DeletePortMapping` return empty bodies on success. A `<Fault>`
/// carrying `<errorCode>` fails closed as [`UpnpError::Soap`].
pub fn parse_upnp_action_response(xml: &[u8]) -> Result<(), UpnpError> {
    let text = xml_check_bounded(xml)?;
    if let Some(code) = xml_extract(text, "errorCode") {
        let code = code.trim().parse::<u32>().unwrap_or(0);
        return Err(UpnpError::Soap { code });
    }
    if xml_extract(text, "Fault").is_some() {
        return Err(UpnpError::Soap { code: 0 });
    }
    Ok(())
}

/// Find the `<controlURL>` for `service_type` in an IGD device
/// description document — scanned, never entity-expanded.
pub fn parse_device_description_control_url(
    xml: &[u8],
    service_type: &str,
) -> Result<String, UpnpError> {
    let text = xml_check_bounded(xml)?;
    let mut rest = text;
    while let Some(pos) = rest.find("<service>") {
        let block_end = rest.find("</service>").ok_or(UpnpError::Missing("service"))?;
        let block = &rest[pos..block_end];
        if let Some(st) = xml_extract(block, "serviceType") {
            if st.trim() == service_type {
                let url = xml_extract(block, "controlURL")
                    .ok_or(UpnpError::Missing("controlURL"))?;
                return Ok(url.trim().to_string());
            }
        }
        rest = &rest[block_end + "</service>".len()..];
    }
    Err(UpnpError::Missing("serviceType"))
}

// ---------- port lease guard ----------

/// A granted WAN port lease. Produced by whichever of the NAT-PMP/PCP/
/// UPnP exchanges succeeded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortLease {
    /// Mapped protocol.
    pub protocol: NatProtocol,
    /// LAN port the game listens on.
    pub internal_port: u16,
    /// WAN port the gateway assigned.
    pub external_port: u16,
    /// WAN address, when the gateway reported one.
    pub external_addr: Option<IpAddr>,
    /// Granted lifetime in seconds.
    pub granted_lifetime_secs: u32,
}

/// RAII wrapper guaranteeing the unmap request fires **exactly once** —
/// on [`PortLeaseGuard::close`] or on `Drop`, whichever comes first.
/// `unmap` is the mechanism-specific delete hook (UPnP
/// `DeletePortMapping`, NAT-PMP/PCP `lifetime = 0` request); because it
/// is an `FnMut` closure the guard stays sans-IO and fully testable.
///
/// Dropping a `PortLeaseGuard` without `close()` still runs the hook —
/// a world that crashes, times out or is simply forgotten can never
/// leave a router port open behind it.
pub struct PortLeaseGuard<F: FnMut(&PortLease)> {
    lease: PortLease,
    unmap: Option<F>,
}

impl<F: FnMut(&PortLease)> PortLeaseGuard<F> {
    /// Wrap `lease`; `unmap` is invoked with the lease on close/drop.
    pub fn new(lease: PortLease, unmap: F) -> Self {
        Self {
            lease,
            unmap: Some(unmap),
        }
    }

    /// The wrapped lease.
    pub fn lease(&self) -> &PortLease {
        &self.lease
    }

    /// Explicitly close: run the unmap hook now and disarm `Drop`.
    /// Returns the lease that was torn down.
    pub fn close(mut self) -> PortLease {
        if let Some(mut unmap) = self.unmap.take() {
            unmap(&self.lease);
        }
        self.lease.clone()
    }
}

impl<F: FnMut(&PortLease)> Drop for PortLeaseGuard<F> {
    fn drop(&mut self) {
        if let Some(mut unmap) = self.unmap.take() {
            unmap(&self.lease);
        }
    }
}

impl<F: FnMut(&PortLease)> fmt::Debug for PortLeaseGuard<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortLeaseGuard")
            .field("lease", &self.lease)
            .field("armed", &self.unmap.is_some())
            .finish()
    }
}

// ---------- STUN binding (RFC 5389) ----------

/// The RFC 5389 magic cookie.
pub const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;

/// Binding Request method type.
pub const STUN_BINDING_REQUEST: u16 = 0x0001;
/// Binding Success Response type.
pub const STUN_BINDING_RESPONSE: u16 = 0x0101;
/// Binding Error Response type.
pub const STUN_BINDING_ERROR: u16 = 0x0111;
/// `MAPPED-ADDRESS` attribute.
pub const STUN_ATTR_MAPPED_ADDRESS: u16 = 0x0001;
/// `XOR-MAPPED-ADDRESS` attribute.
pub const STUN_ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// `ERROR-CODE` attribute.
pub const STUN_ATTR_ERROR_CODE: u16 = 0x0009;

/// STUN decode failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StunError {
    /// Fewer than 20 header bytes, or a declared length that overruns.
    Truncated,
    /// Wrong magic cookie — probably not STUN at all.
    BadCookie,
    /// The transaction id does not match the request we sent.
    TxIdMismatch,
    /// A binding error response carrying its 300-699 code.
    Server { code: u16 },
    /// An attribute overran the message or its padding.
    AttrTruncated,
    /// An address attribute was neither IPv4 nor IPv6.
    UnknownAddrFamily(u8),
    /// Neither `XOR-MAPPED-ADDRESS` nor `MAPPED-ADDRESS` was present.
    NoMappedAddress,
    /// The message type was not a binding response/error.
    NotBindingResponse(u16),
}

impl fmt::Display for StunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "STUN message truncated"),
            Self::BadCookie => write!(f, "STUN magic cookie mismatch"),
            Self::TxIdMismatch => write!(f, "STUN transaction id mismatch"),
            Self::Server { code } => write!(f, "STUN server error {code}"),
            Self::AttrTruncated => write!(f, "STUN attribute truncated"),
            Self::UnknownAddrFamily(fam) => write!(f, "STUN address family {fam}"),
            Self::NoMappedAddress => write!(f, "no MAPPED-ADDRESS attribute"),
            Self::NotBindingResponse(t) => write!(f, "STUN message type 0x{t:04X}"),
        }
    }
}

impl std::error::Error for StunError {}

/// A verified Binding Response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StunBinding {
    /// Our reflexive address as the server sees it.
    pub mapped: SocketAddr,
    /// `true` when `XOR-MAPPED-ADDRESS` supplied the answer.
    pub via_xor: bool,
}

/// Encode a 20-byte Binding Request for `txid`.
pub fn encode_stun_binding_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[0..2].copy_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
    // length already zero; cookie + txid follow.
    out[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    out[8..20].copy_from_slice(txid);
    out
}

fn decode_stun_addr(
    bytes: &[u8],
    xor: bool,
    txid: &[u8; 12],
) -> Result<SocketAddr, StunError> {
    if bytes.len() < 4 {
        return Err(StunError::AttrTruncated);
    }
    let family = bytes[1];
    let raw_port = u16::from_be_bytes([bytes[2], bytes[3]]);
    match family {
        0x01 => {
            if bytes.len() < 8 {
                return Err(StunError::AttrTruncated);
            }
            let port = if xor {
                raw_port ^ (STUN_MAGIC_COOKIE >> 16) as u16
            } else {
                raw_port
            };
            let mut ip = [0u8; 4];
            ip.copy_from_slice(&bytes[4..8]);
            if xor {
                for (b, k) in ip.iter_mut().zip(STUN_MAGIC_COOKIE.to_be_bytes()) {
                    *b ^= k;
                }
            }
            Ok(SocketAddr::from((Ipv4Addr::from(ip), port)))
        }
        0x02 => {
            if bytes.len() < 20 {
                return Err(StunError::AttrTruncated);
            }
            let port = if xor {
                raw_port ^ (STUN_MAGIC_COOKIE >> 16) as u16
            } else {
                raw_port
            };
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&bytes[4..20]);
            if xor {
                let mut key = [0u8; 16];
                key[..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
                key[4..].copy_from_slice(txid);
                for (b, k) in ip.iter_mut().zip(key) {
                    *b ^= k;
                }
            }
            Ok(SocketAddr::from((Ipv6Addr::from(ip), port)))
        }
        other => Err(StunError::UnknownAddrFamily(other)),
    }
}

/// Decode a Binding Response, enforcing cookie and `expected_txid`.
/// `XOR-MAPPED-ADDRESS` wins over `MAPPED-ADDRESS`; a `0x0111` error
/// response yields [`StunError::Server`].
pub fn decode_stun_binding_response(
    bytes: &[u8],
    expected_txid: &[u8; 12],
) -> Result<StunBinding, StunError> {
    if bytes.len() < 20 {
        return Err(StunError::Truncated);
    }
    let msg_type = u16::from_be_bytes([bytes[0], bytes[1]]);
    if msg_type != STUN_BINDING_RESPONSE && msg_type != STUN_BINDING_ERROR {
        return Err(StunError::NotBindingResponse(msg_type));
    }
    let msg_len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    if msg_len % 4 != 0 || 20 + msg_len > bytes.len() {
        return Err(StunError::Truncated);
    }
    if u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) != STUN_MAGIC_COOKIE {
        return Err(StunError::BadCookie);
    }
    let mut txid = [0u8; 12];
    txid.copy_from_slice(&bytes[8..20]);
    if &txid != expected_txid {
        return Err(StunError::TxIdMismatch);
    }

    let attrs = &bytes[20..20 + msg_len];
    let mut mapped = None;
    let mut xor_mapped = None;
    let mut error_code = None;
    let mut rest = attrs;
    while !rest.is_empty() {
        if rest.len() < 4 {
            return Err(StunError::AttrTruncated);
        }
        let atype = u16::from_be_bytes([rest[0], rest[1]]);
        let alen = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
        let padded = (alen + 3) & !3;
        if rest.len() < 4 + padded {
            return Err(StunError::AttrTruncated);
        }
        let value = &rest[4..4 + alen];
        match atype {
            STUN_ATTR_XOR_MAPPED_ADDRESS => {
                xor_mapped = Some(decode_stun_addr(value, true, &txid)?);
            }
            STUN_ATTR_MAPPED_ADDRESS => {
                mapped = Some(decode_stun_addr(value, false, &txid)?);
            }
            STUN_ATTR_ERROR_CODE if alen >= 4 => {
                // reserved(21) | class(3) | number(8)
                error_code = Some(
                    u16::from(value[2] & 0x07) * 100 + u16::from(value[3]),
                );
            }
            _ => {}
        }
        rest = &rest[4 + padded..];
    }

    if msg_type == STUN_BINDING_ERROR {
        return Err(StunError::Server {
            code: error_code.unwrap_or(0),
        });
    }
    match xor_mapped.or(mapped) {
        Some(mapped) => Ok(StunBinding {
            mapped,
            via_xor: xor_mapped.is_some(),
        }),
        None => Err(StunError::NoMappedAddress),
    }
}

// ---------- hole-punch coordinator ----------

/// States of the deterministic hole-punch machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HolePunchState {
    /// Not started.
    Idle,
    /// STUN Binding Request in flight.
    ProbingStun,
    /// Punch probes in flight; `attempts` so far.
    Punching {
        /// How many probe rounds have been sent.
        attempts: u8,
    },
    /// A peer packet arrived: the path is open.
    Connected(SocketAddr),
    /// Punching gave up; use the relay tunnel.
    FallbackRelay,
    /// Terminal failure (no relay fallback allowed).
    Failed,
}

/// Tuning for [`HolePunchMachine`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HolePunchConfig {
    /// STUN retransmissions before declaring discovery impossible.
    pub stun_retries: u8,
    /// Probe rounds before abandoning the punch.
    pub max_punch_attempts: u8,
    /// Whether exhaustion may fall back to the relay.
    pub relay_fallback: bool,
}

impl Default for HolePunchConfig {
    fn default() -> Self {
        Self {
            stun_retries: 3,
            max_punch_attempts: 8,
            relay_fallback: true,
        }
    }
}

/// Inputs to [`HolePunchMachine::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HolePunchEvent {
    /// User pressed publish: begin STUN discovery.
    Start,
    /// A verified Binding Response arrived.
    StunBound(SocketAddr),
    /// The STUN request timed out.
    StunTimeout,
    /// The peer address we should punch toward (from the rendezvous).
    PeerKnown(SocketAddr),
    /// A probe round elapsed unanswered.
    PunchAttemptTimedOut,
    /// A packet arrived from the peer: the hole is open.
    PeerReached(SocketAddr),
    /// The user cancelled.
    Abort,
}

/// What the caller must do after a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HolePunchAction {
    /// (Re)send a STUN Binding Request.
    SendStunRequest,
    /// Send a punch probe to this address.
    SendPunchProbe(SocketAddr),
    /// The path is open — hand the socket to the session layer.
    ReportConnected(SocketAddr),
    /// Open an `MGR1` relay tunnel instead.
    OpenRelayTunnel,
    /// Report terminal failure.
    ReportFailed,
    /// Tear everything down.
    AbortAll,
}

/// The deterministic `Idle -> ProbingStun -> Punching -> Connected |
/// FallbackRelay | Failed` coordinator. Sans-IO: the caller drives
/// timers and sockets and feeds [`HolePunchEvent`]s back.
#[derive(Clone, Debug)]
pub struct HolePunchMachine {
    cfg: HolePunchConfig,
    state: HolePunchState,
    stun_attempts: u8,
    mapped: Option<SocketAddr>,
    peer: Option<SocketAddr>,
}

impl HolePunchMachine {
    /// Fresh machine in [`HolePunchState::Idle`].
    pub fn new(cfg: HolePunchConfig) -> Self {
        Self {
            cfg,
            state: HolePunchState::Idle,
            stun_attempts: 0,
            mapped: None,
            peer: None,
        }
    }

    /// Current state.
    pub fn state(&self) -> HolePunchState {
        self.state
    }

    /// The reflexive address STUN reported, once known.
    pub fn mapped_addr(&self) -> Option<SocketAddr> {
        self.mapped
    }

    /// Advance on `event`, returning the actions to perform (possibly
    /// none — events on terminal states are ignored).
    pub fn step(&mut self, event: HolePunchEvent) -> Vec<HolePunchAction> {
        if event == HolePunchEvent::Abort {
            if !matches!(
                self.state,
                HolePunchState::Connected(_)
                    | HolePunchState::FallbackRelay
                    | HolePunchState::Failed
            ) {
                self.state = HolePunchState::Failed;
                return vec![HolePunchAction::AbortAll];
            }
            return Vec::new();
        }
        match self.state {
            HolePunchState::Idle => match event {
                HolePunchEvent::Start => {
                    self.state = HolePunchState::ProbingStun;
                    self.stun_attempts = 1;
                    vec![HolePunchAction::SendStunRequest]
                }
                _ => Vec::new(),
            },
            HolePunchState::ProbingStun => match event {
                HolePunchEvent::StunBound(addr) => {
                    self.mapped = Some(addr);
                    self.state = HolePunchState::Punching { attempts: 0 };
                    // Wait for the rendezvous to supply the peer endpoint.
                    Vec::new()
                }
                HolePunchEvent::StunTimeout => {
                    if self.stun_attempts < self.cfg.stun_retries {
                        self.stun_attempts += 1;
                        vec![HolePunchAction::SendStunRequest]
                    } else {
                        self.exhausted()
                    }
                }
                _ => Vec::new(),
            },
            HolePunchState::Punching { attempts } => match event {
                HolePunchEvent::PeerKnown(addr) => {
                    self.peer = Some(addr);
                    self.state = HolePunchState::Punching { attempts: 1 };
                    vec![HolePunchAction::SendPunchProbe(addr)]
                }
                HolePunchEvent::PunchAttemptTimedOut => {
                    let Some(peer) = self.peer else {
                        return self.exhausted();
                    };
                    let next = attempts.saturating_add(1);
                    if next >= self.cfg.max_punch_attempts {
                        self.exhausted()
                    } else {
                        self.state = HolePunchState::Punching { attempts: next };
                        vec![HolePunchAction::SendPunchProbe(peer)]
                    }
                }
                HolePunchEvent::PeerReached(addr) => {
                    self.state = HolePunchState::Connected(addr);
                    vec![HolePunchAction::ReportConnected(addr)]
                }
                _ => Vec::new(),
            },
            HolePunchState::Connected(_)
            | HolePunchState::FallbackRelay
            | HolePunchState::Failed => Vec::new(),
        }
    }

    fn exhausted(&mut self) -> Vec<HolePunchAction> {
        if self.cfg.relay_fallback {
            self.state = HolePunchState::FallbackRelay;
            vec![HolePunchAction::OpenRelayTunnel]
        } else {
            self.state = HolePunchState::Failed;
            vec![HolePunchAction::ReportFailed]
        }
    }
}

// ---------- megu-relay (MGR1) framing ----------

/// Frame magic: `b"MGR1"`.
pub const RELAY_MAGIC: [u8; 4] = *b"MGR1";

/// Hard cap on a complete `MGR1` frame including the length prefix:
/// 64 KiB. Larger frames are refused before any allocation.
pub const MAX_RELAY_FRAME_BYTES: usize = 64 * 1024;

/// `RegisterHost`/`JoinByToken` name fields cap at 64 bytes of UTF-8.
pub const MAX_RELAY_NAME_BYTES: usize = 64;

/// An `MGR1` frame. Wire shape:
///
/// ```text
/// frame = u32be(total_frame_bytes) + body
/// body  = "MGR1" + tag(1) + fields        (fields big-endian)
/// ```
///
/// `total_frame_bytes` counts itself: legal values are `9..=65536`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayFrame {
    /// `0x01` — host opens a session on the relay:
    /// `u64 session_id | u64 join_token | u16 max_guests | u16 name_len + name`.
    RegisterHost {
        /// Rendezvous key friends dial with.
        session_id: u64,
        /// One-time join secret the relay checks guests against.
        join_token: u64,
        /// Guest cap for this session.
        max_guests: u16,
        /// Display name (≤ [`MAX_RELAY_NAME_BYTES`] bytes).
        world_name: String,
    },
    /// `0x02` — guest presents a session id + token:
    /// `u64 session_id | u64 join_token | u16 name_len + name`.
    JoinByToken {
        /// Session the guest wants.
        session_id: u64,
        /// The one-time secret from the invite.
        join_token: u64,
        /// Guest display name (≤ [`MAX_RELAY_NAME_BYTES`] bytes).
        guest_name: String,
    },
    /// `0x03` — opaque game bytes inside a stream:
    /// `u32 stream_id | u32 len | payload | u32 crc32(payload)`.
    DataChunk {
        /// Multiplexed stream this chunk belongs to.
        stream_id: u32,
        /// The opaque bytes.
        payload: Vec<u8>,
    },
    /// `0x04` — liveness ping: `u64 nonce`.
    Heartbeat {
        /// Echo value.
        nonce: u64,
    },
    /// `0x05` — liveness pong: `u64 nonce`.
    HeartbeatAck {
        /// Echoed value.
        nonce: u64,
    },
    /// `0x06` — host tears the session down: `u64 session_id | u64 join_token`.
    Revoke {
        /// Session to end.
        session_id: u64,
        /// Proves the sender registered this session.
        join_token: u64,
    },
    /// `0x07` — relay → host: `u8 ok` (`0`/`1`).
    RevokeAck {
        /// Whether the session was found and closed.
        accepted: bool,
    },
    /// `0x08` — relay → host: session live: `u64 session_id`.
    RegisterAck {
        /// Confirmed session id.
        session_id: u64,
    },
    /// `0x09` — relay → guest: admitted: `u32 stream_id`.
    JoinAck {
        /// The guest's assigned stream.
        stream_id: u32,
    },
    /// `0x0A` — relay → guest: refused: `u8 reason`.
    JoinReject {
        /// `0` unknown session, `1` bad token, `2` session full,
        /// `3` rate-limited.
        reason: u8,
    },
}

/// `MGR1` decode/encode failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayError {
    /// Declared length out of bounds or short of the actual bytes.
    BadLength,
    /// The `MGR1` magic did not match.
    BadMagic,
    /// Unknown frame tag byte.
    BadTag(u8),
    /// A field ran past the frame body.
    Truncated,
    /// Trailing bytes after the last declared field.
    TrailingGarbage,
    /// A name exceeded [`MAX_RELAY_NAME_BYTES`].
    NameTooLong,
    /// A `DataChunk` CRC-32 did not match its payload.
    BadChunkCrc,
    /// Frame would exceed [`MAX_RELAY_FRAME_BYTES`].
    Oversize,
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadLength => write!(f, "MGR1 frame length out of bounds"),
            Self::BadMagic => write!(f, "MGR1 magic mismatch"),
            Self::BadTag(t) => write!(f, "MGR1 unknown tag 0x{t:02X}"),
            Self::Truncated => write!(f, "MGR1 frame truncated"),
            Self::TrailingGarbage => write!(f, "MGR1 trailing bytes"),
            Self::NameTooLong => write!(f, "MGR1 name too long"),
            Self::BadChunkCrc => write!(f, "MGR1 data chunk CRC mismatch"),
            Self::Oversize => write!(f, "MGR1 frame exceeds 64 KiB"),
        }
    }
}

impl std::error::Error for RelayError {}

const TAG_REGISTER_HOST: u8 = 0x01;
const TAG_JOIN_BY_TOKEN: u8 = 0x02;
const TAG_DATA_CHUNK: u8 = 0x03;
const TAG_HEARTBEAT: u8 = 0x04;
const TAG_HEARTBEAT_ACK: u8 = 0x05;
const TAG_REVOKE: u8 = 0x06;
const TAG_REVOKE_ACK: u8 = 0x07;
const TAG_REGISTER_ACK: u8 = 0x08;
const TAG_JOIN_ACK: u8 = 0x09;
const TAG_JOIN_REJECT: u8 = 0x0A;

/// Encode `frame` as `u32be(len) + "MGR1" + tag + fields`. Returns
/// [`RelayError::Oversize`] if the result would exceed
/// [`MAX_RELAY_FRAME_BYTES`].
pub fn encode_relay_frame(frame: &RelayFrame) -> Result<Vec<u8>, RelayError> {
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(&RELAY_MAGIC);
    match frame {
        RelayFrame::RegisterHost {
            session_id,
            join_token,
            max_guests,
            world_name,
        } => {
            if world_name.len() > MAX_RELAY_NAME_BYTES {
                return Err(RelayError::NameTooLong);
            }
            body.push(TAG_REGISTER_HOST);
            body.extend_from_slice(&session_id.to_be_bytes());
            body.extend_from_slice(&join_token.to_be_bytes());
            body.extend_from_slice(&max_guests.to_be_bytes());
            body.extend_from_slice(&(world_name.len() as u16).to_be_bytes());
            body.extend_from_slice(world_name.as_bytes());
        }
        RelayFrame::JoinByToken {
            session_id,
            join_token,
            guest_name,
        } => {
            if guest_name.len() > MAX_RELAY_NAME_BYTES {
                return Err(RelayError::NameTooLong);
            }
            body.push(TAG_JOIN_BY_TOKEN);
            body.extend_from_slice(&session_id.to_be_bytes());
            body.extend_from_slice(&join_token.to_be_bytes());
            body.extend_from_slice(&(guest_name.len() as u16).to_be_bytes());
            body.extend_from_slice(guest_name.as_bytes());
        }
        RelayFrame::DataChunk { stream_id, payload } => {
            body.push(TAG_DATA_CHUNK);
            body.extend_from_slice(&stream_id.to_be_bytes());
            body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            body.extend_from_slice(payload);
            body.extend_from_slice(&crc32_ieee(payload).to_be_bytes());
        }
        RelayFrame::Heartbeat { nonce } => {
            body.push(TAG_HEARTBEAT);
            body.extend_from_slice(&nonce.to_be_bytes());
        }
        RelayFrame::HeartbeatAck { nonce } => {
            body.push(TAG_HEARTBEAT_ACK);
            body.extend_from_slice(&nonce.to_be_bytes());
        }
        RelayFrame::Revoke {
            session_id,
            join_token,
        } => {
            body.push(TAG_REVOKE);
            body.extend_from_slice(&session_id.to_be_bytes());
            body.extend_from_slice(&join_token.to_be_bytes());
        }
        RelayFrame::RevokeAck { accepted } => {
            body.push(TAG_REVOKE_ACK);
            body.push(u8::from(*accepted));
        }
        RelayFrame::RegisterAck { session_id } => {
            body.push(TAG_REGISTER_ACK);
            body.extend_from_slice(&session_id.to_be_bytes());
        }
        RelayFrame::JoinAck { stream_id } => {
            body.push(TAG_JOIN_ACK);
            body.extend_from_slice(&stream_id.to_be_bytes());
        }
        RelayFrame::JoinReject { reason } => {
            body.push(TAG_JOIN_REJECT);
            body.push(*reason);
        }
    }
    let total = 4 + body.len();
    if total > MAX_RELAY_FRAME_BYTES {
        return Err(RelayError::Oversize);
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode exactly one `MGR1` frame from `bytes` (length prefix
/// included). The declared length must equal `bytes.len()` exactly —
/// trailing bytes are an error, never silently absorbed.
pub fn decode_relay_frame(bytes: &[u8]) -> Result<RelayFrame, RelayError> {
    if bytes.len() < 4 {
        return Err(RelayError::Truncated);
    }
    let declared = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if !(9..=MAX_RELAY_FRAME_BYTES).contains(&declared) || declared != bytes.len() {
        return Err(RelayError::BadLength);
    }
    let body = &bytes[4..];
    if body[..4] != RELAY_MAGIC {
        return Err(RelayError::BadMagic);
    }
    let tag = body[4];
    let mut r = WireReader::new(&body[5..]);
    let trunc = |_| RelayError::Truncated;
    let name = |r: &mut WireReader<'_>| -> Result<String, RelayError> {
        let len = usize::from(r.read_u16().map_err(trunc)?);
        if len > MAX_RELAY_NAME_BYTES {
            return Err(RelayError::NameTooLong);
        }
        let b = r.take(len).map_err(trunc)?;
        String::from_utf8(b.to_vec()).map_err(|_| RelayError::Truncated)
    };

    let frame = match tag {
        TAG_REGISTER_HOST => RelayFrame::RegisterHost {
            session_id: r.read_u64().map_err(trunc)?,
            join_token: r.read_u64().map_err(trunc)?,
            max_guests: r.read_u16().map_err(trunc)?,
            world_name: name(&mut r)?,
        },
        TAG_JOIN_BY_TOKEN => RelayFrame::JoinByToken {
            session_id: r.read_u64().map_err(trunc)?,
            join_token: r.read_u64().map_err(trunc)?,
            guest_name: name(&mut r)?,
        },
        TAG_DATA_CHUNK => {
            let stream_id = r.read_u32().map_err(trunc)?;
            let len = r.read_u32().map_err(trunc)? as usize;
            let payload = r.take(len).map_err(trunc)?.to_vec();
            let crc = r.read_u32().map_err(trunc)?;
            if crc32_ieee(&payload) != crc {
                return Err(RelayError::BadChunkCrc);
            }
            RelayFrame::DataChunk { stream_id, payload }
        }
        TAG_HEARTBEAT => RelayFrame::Heartbeat {
            nonce: r.read_u64().map_err(trunc)?,
        },
        TAG_HEARTBEAT_ACK => RelayFrame::HeartbeatAck {
            nonce: r.read_u64().map_err(trunc)?,
        },
        TAG_REVOKE => RelayFrame::Revoke {
            session_id: r.read_u64().map_err(trunc)?,
            join_token: r.read_u64().map_err(trunc)?,
        },
        TAG_REVOKE_ACK => RelayFrame::RevokeAck {
            accepted: r.read_u8().map_err(trunc)? != 0,
        },
        TAG_REGISTER_ACK => RelayFrame::RegisterAck {
            session_id: r.read_u64().map_err(trunc)?,
        },
        TAG_JOIN_ACK => RelayFrame::JoinAck {
            stream_id: r.read_u32().map_err(trunc)?,
        },
        TAG_JOIN_REJECT => RelayFrame::JoinReject {
            reason: r.read_u8().map_err(trunc)?,
        },
        other => return Err(RelayError::BadTag(other)),
    };
    if r.remaining() != 0 {
        return Err(RelayError::TrailingGarbage);
    }
    Ok(frame)
}

// ---------- token-bucket rate limiter ----------

/// A byte-denominated token bucket. `Instant` is caller-supplied, so the
/// limiter is fully deterministic and testable.
#[derive(Clone, Debug)]
pub struct TokenBucket {
    capacity: u32,
    tokens: u64,
    refill_per_sec: u32,
    /// Sub-token refill remainder, in nano-token units.
    carry: u128,
    last: Instant,
}

impl TokenBucket {
    /// A full bucket: `capacity` bytes available immediately, refilling
    /// at `refill_per_sec` bytes/second.
    pub fn new(capacity: u32, refill_per_sec: u32, now: Instant) -> Self {
        Self {
            capacity,
            tokens: u64::from(capacity),
            refill_per_sec,
            carry: 0,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last);
        let nanos = elapsed.as_nanos();
        if nanos == 0 {
            return;
        }
        self.last = now;
        if self.refill_per_sec == 0 {
            return;
        }
        let total = self.carry + nanos * u128::from(self.refill_per_sec);
        let add = total / 1_000_000_000;
        self.carry = total % 1_000_000_000;
        self.tokens = (self.tokens.saturating_add(add.min(u64::MAX as u128) as u64))
            .min(u64::from(self.capacity));
    }

    /// Current available bytes (post-refill at `now`).
    pub fn available(&mut self, now: Instant) -> u32 {
        self.refill(now);
        self.tokens.min(u64::from(self.capacity)) as u32
    }

    /// Attempt to spend `cost` bytes. Always refuses `cost > capacity`.
    pub fn try_consume(&mut self, now: Instant, cost: u32) -> bool {
        self.refill(now);
        if cost > self.capacity || u64::from(cost) > self.tokens {
            return false;
        }
        self.tokens -= u64::from(cost);
        true
    }
}

/// Per-session relay budget: frames per second *and* bytes per second.
/// Both must admit a frame; a relay that enforces this on ingress can
/// never become an amplification reflector for a compromised session.
#[derive(Clone, Debug)]
pub struct RelaySessionBudget {
    /// Frame-count bucket (e.g. 64 frames/s sustained).
    pub frames: TokenBucket,
    /// Byte bucket sized to the uplink share (e.g. 256 KiB/s).
    pub bytes: TokenBucket,
}

impl RelaySessionBudget {
    /// `frames_per_sec` / `bytes_per_sec` sustained rates; burst capacity
    /// is one second of each.
    pub fn new(frames_per_sec: u32, bytes_per_sec: u32, now: Instant) -> Self {
        Self {
            frames: TokenBucket::new(frames_per_sec, frames_per_sec, now),
            bytes: TokenBucket::new(bytes_per_sec, bytes_per_sec, now),
        }
    }

    /// Admit a frame of `frame_bytes` (whole wire bytes). Frames larger
    /// than [`MAX_RELAY_FRAME_BYTES`] are refused before the buckets run.
    pub fn allow(&mut self, now: Instant, frame_bytes: usize) -> bool {
        if frame_bytes > MAX_RELAY_FRAME_BYTES {
            return false;
        }
        // Probe both buckets first; only commit when both admit so a
        // refused frame never spends half its budget.
        let frames_ok = {
            let mut probe = self.frames.clone();
            probe.try_consume(now, 1)
        };
        let bytes_ok = {
            let mut probe = self.bytes.clone();
            probe.try_consume(now, frame_bytes as u32)
        };
        if !(frames_ok && bytes_ok) {
            return false;
        }
        self.frames.try_consume(now, 1);
        self.bytes.try_consume(now, frame_bytes as u32);
        true
    }
}

// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-proto`: the canonical, version-aware Minecraft Java protocol core.
//!
//! This crate is a standalone, zero-dependency library so the same protocol
//! knowledge can be shared by the megu server (`src/play_protocol.rs`), the
//! megu client (`client-app/src/protocol.rs`) and any future tooling, and so
//! new Minecraft versions slot in as *data* (a version pack) rather than as a
//! second copy of the codec.
//!
//! Layers, bottom to top:
//!
//! * [`wire`] — byte-level primitives: `VarInt`/`VarLong`, big-endian scalars,
//!   bounded strings, UUIDs, packed block positions, multi-word bit sets and
//!   packet frames (`VarInt(len) + VarInt(id) + payload`). Everything fails
//!   closed through [`wire::ProtoError`]; nothing in the crate panics on
//!   malformed, truncated or adversarial input.
//! * [`version`] — [`version::ProtocolVersion`] and the structured
//!   [`version::VersionCapabilities`] downstream code switches on instead of
//!   scattering version-string `if`s.
//! * [`table`] — [`table::ProtocolTable`], the data-driven bidirectional
//!   `resource_name <-> protocol_id` map for packet ids (per
//!   [`table::PacketPhase`]/[`table::PacketDir`]) and the core registries
//!   (`minecraft:block`, `minecraft:item`, `minecraft:entity_type`,
//!   `minecraft:menu`, `minecraft:data_component_type`), plus
//!   [`table::IdRemapper`] for O(1) id translation between two versions of one
//!   registry. [`table::ProtocolTable::v767_builtin`] ships the verified 1.21.1
//!   tables, [`table::ProtocolTable::v777_builtin`] the 26.3 packet tables;
//!   [`table::ProtocolTable::from_version_pack_json`] loads the same shape
//!   from normalized DataGenerator reports at runtime.
//! * [`codec`] — the canonical packet IR ([`codec::S2CPacket`],
//!   [`codec::C2SPacket`]), the [`codec::ProtocolCodec`] trait and
//!   [`codec::CodecV767`], the protocol-767 implementation.
//! * [`router`] — the multi-version glue: [`router::TableDrivenCodec`] (a
//!   codec over any `VersionCapabilities` + `ProtocolTable` pair, e.g. a
//!   loaded 1.21.4 pack), [`router::SessionRemapper`] (per-connection
//!   registry-id translation) and [`router::VersionRouter`] (codec selection
//!   by handshake protocol number).
//! * [`resolver`] — vanilla's `_minecraft._tcp.<host>.` SRV redirect lookup
//!   and address resolution, pure `std` DNS (RFC 1035/2782).
//! * [`voice_relay`] — the clean-room `VCR1` UDP proximity voice relay: a
//!   compact datagram codec ([`voice_relay::VoicePacket`]), a sans-IO router
//!   ([`voice_relay::VoiceRelayState`]) with distance/dimension culling,
//!   whisper radius, group channels and mute/deafen/block, and the live
//!   non-blocking [`voice_relay::UdpVoiceRelay`] over `std::net::UdpSocket`.
//! * [`friend_relay`] — the port-forwarding-free friend-invite relay
//!   (`FRL2`): Crockford Base32 invite codes ([`friend_relay::InviteCode`])
//!   rendered `MEGU-XXXX-XXXX`, a compact length-prefixed stream-multiplexing
//!   wire protocol ([`friend_relay::FriendPacket`]), the sans-IO
//!   [`friend_relay::FriendRelayRouter`] enforcing `max_guests`, `Instant`-TTL
//!   invite expiry, constant-time secret comparison and per-host stream
//!   isolation, and the live non-blocking [`friend_relay::TcpFriendRelay`]
//!   bridging one host tunnel to many guest TCP connections.
//! * [`seamless_portal`] — Phase 1 Seamless Dimension Transfer
//!   (`megu:portal_seamless_v1`, `MSP1`): the fail-closed packet codec
//!   ([`seamless_portal::SeamlessPortalPacket`]), the generic portal
//!   coordinate mapper ([`seamless_portal::resolve_portal_destination`]),
//!   the nearest-first chunk prefetch planner
//!   ([`seamless_portal::enumerate_prefetch_chunks`]), the server-side
//!   hysteresis tracker ([`seamless_portal::PortalProximityTracker`]) and
//!   the client-side zero-loading-screen crossfade machine
//!   ([`seamless_portal::SeamlessDimTransitionState`]).
//! * [`frl_e2ee`] — the `FRL3` zero-trust end-to-end encryption layer that
//!   runs inside `friend_relay`'s `StreamData`: pure-Rust RFC 7748 X25519,
//!   RFC 8439 ChaCha20-Poly1305 AEAD and HKDF-SHA256, an invite-PSK ephemeral
//!   handshake plus a Noise-IK-style mutual static-identity handshake with
//!   a [`frl_e2ee::FriendAllowlist`], and the directional
//!   [`frl_e2ee::Frl3Cipher`] record layer — after which the relay sees only
//!   ciphertext.

pub mod codec;
pub mod item_components;
pub mod channel_router;
mod data_v767;
mod data_v777;
pub mod friend_relay;
pub mod frl_auth;
pub mod frl_e2ee;
pub mod resolver;
pub mod router;
pub mod seamless_portal;
pub mod session;
pub mod table;
pub mod version;
pub mod voice_relay;
pub mod wire;

pub use codec::{
    C2SPacket, CodecV767, EquipmentSlot, ItemStack, LoginProperty, MenuKind,
    ProtocolCodec, RawComponent, RegistryDataEntry, S2CPacket, TeamInfo, TeamOperation,
    MAX_COOKIE_PAYLOAD_BYTES,
};
pub use channel_router::{
    classify_channel, decode_register_payload, encode_register_payload, validate_channel_name,
    ChannelDirection, ChannelFirewall, ChannelFirewallConfig, ChannelFirewallMetrics,
    ChannelHandlerKind, ChannelNameError, DropReason, FirewallDisconnectReason, FirewallVerdict,
    OperatorRuleError, RegisterPayloadError, ThrottleReason, MAX_BULK_PAYLOAD_BYTES,
    MAX_CHANNEL_NAME_BYTES, MAX_REGISTERED_CHANNELS_PER_PEER, MAX_REGISTER_PAYLOAD_BYTES,
};
pub use friend_relay::{
    ExpiredHost, FriendPacket, FriendRelayConfig, FriendRelayMetrics, FriendRelayRouter,
    Frl3Stream, Frl3StreamError, GuestStream, HostSession, InviteCode, RevokeError,
    TcpFriendRelay, FRIEND_MAGIC, MAX_FRL2_FRAME_BYTES, MAX_STREAM_PAYLOAD_BYTES,
};
pub use frl_e2ee::{
    aead_open, aead_seal, format_friend_id, friend_host_accept,
    friend_host_accept_with_secret, hkdf_expand, hkdf_extract, host_accept,
    host_accept_with_secret, parse_friend_id, random_32_bytes, sas_code, x25519,
    x25519_public, E2eeError, FriendAllowlist, Frl3Ack, Frl3Cipher, Frl3FriendAck,
    Frl3FriendGuestHandshake, Frl3FriendHello, Frl3GuestHandshake, Frl3GuestToken, Frl3Hello,
    Frl3Identity, Frl3Message, MAX_E2EE_RECORD_BYTES,
};
pub use resolver::{resolve as resolve_server_address, ResolveError};
pub use router::{SessionRemapSummary, SessionRemapper, TableDrivenCodec, VersionRouter};
pub use session::{
    text_component, ServerSession, ServerSessionConfig, SessionEvent, SessionOutput, SessionPhase,
};
pub use table::{IdRemapper, PacketDir, PacketPhase, ProtocolTable, Registry, RemapReport};
pub use version::{
    BlockStateJsonFormat, NoTargetVersion, ProtocolVersion, RecipeSyncMode,
    VersionCapabilities,
};
pub use voice_relay::{
    RouteReject, UdpVoiceRelay, VoiceDelivery, VoiceGroupKind, VoicePacket, VoicePlayerState,
    VoiceRelayConfig, VoiceRelayMetrics, VoiceRelayState, MAX_OPUS_PAYLOAD_BYTES,
    MAX_VOICE_DATAGRAM_BYTES, VOICE_MAGIC, VOICE_RATE_WINDOW,
};
pub use wire::{BitSet, BlockPos, ProtoError, WireReader, WireWriter};

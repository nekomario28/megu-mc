// SPDX-License-Identifier: LGPL-3.0-or-later
//! Deterministic, sans-IO CustomPayload routing for protocol 767 and 777.
//!
//! Call [`ChannelFirewall::inspect`] for each payload with the session's tick
//! and direction. Only `Route` verdicts may reach capability handlers. The
//! caller owns peer identities and must call [`ChannelFirewall::remove_peer`]
//! when a connection ends. The tick rate is 20 ticks per second.

#![deny(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

pub const MAX_CHANNEL_NAME_BYTES: usize = 128;
pub const MAX_REGISTERED_CHANNELS_PER_PEER: usize = 256;
pub const MAX_REGISTER_PAYLOAD_BYTES: usize = 32_768;
pub const MAX_BULK_PAYLOAD_BYTES: usize = 1_048_576;
const TICKS_PER_SECOND: u128 = 20;
const MAX_OPERATOR_RULES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelNameError {
    Empty,
    TooLong,
    InvalidFormat,
    Traversal,
}

/// Check the canonical ASCII `namespace:path` resource location.
pub fn validate_channel_name(channel: &str) -> Result<(), ChannelNameError> {
    if channel.is_empty() {
        return Err(ChannelNameError::Empty);
    }
    if channel.len() > MAX_CHANNEL_NAME_BYTES {
        return Err(ChannelNameError::TooLong);
    }
    let (namespace, path) = channel
        .split_once(':')
        .ok_or(ChannelNameError::InvalidFormat)?;
    if namespace.is_empty()
        || path.is_empty()
        || !namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
        })
        || !path.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_./-".contains(&byte)
        })
    {
        return Err(ChannelNameError::InvalidFormat);
    }
    if matches!(namespace, "." | "..") || path.split('/').any(|part| matches!(part, "." | "..")) {
        return Err(ChannelNameError::Traversal);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterPayloadError {
    PayloadTooLarge,
    TooManyChannels,
    EmptyChannel,
    InvalidUtf8,
    InvalidChannel(ChannelNameError),
    DuplicateChannel,
}

/// Decode a NUL-separated `minecraft:register` or `minecraft:unregister`
/// payload. Empty payloads represent an empty list. Empty fields and a
/// trailing NUL are rejected to avoid ambiguous encodings.
pub fn decode_register_payload(payload: &[u8]) -> Result<Vec<String>, RegisterPayloadError> {
    if payload.len() > MAX_REGISTER_PAYLOAD_BYTES {
        return Err(RegisterPayloadError::PayloadTooLarge);
    }
    if payload.is_empty() {
        return Ok(Vec::new());
    }
    let mut channels = Vec::new();
    let mut seen = BTreeSet::new();
    for item in payload.split(|byte| *byte == 0) {
        if item.is_empty() {
            return Err(RegisterPayloadError::EmptyChannel);
        }
        if channels.len() >= MAX_REGISTERED_CHANNELS_PER_PEER {
            return Err(RegisterPayloadError::TooManyChannels);
        }
        let name = std::str::from_utf8(item).map_err(|_| RegisterPayloadError::InvalidUtf8)?;
        validate_channel_name(name).map_err(RegisterPayloadError::InvalidChannel)?;
        if !seen.insert(name) {
            return Err(RegisterPayloadError::DuplicateChannel);
        }
        channels.push(name.to_owned());
    }
    Ok(channels)
}

/// Encode canonical, unique resource locations with NUL separators.
pub fn encode_register_payload<S: AsRef<str>>(
    channels: &[S],
) -> Result<Vec<u8>, RegisterPayloadError> {
    if channels.len() > MAX_REGISTERED_CHANNELS_PER_PEER {
        return Err(RegisterPayloadError::TooManyChannels);
    }
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, channel) in channels.iter().enumerate() {
        let channel = channel.as_ref();
        validate_channel_name(channel).map_err(RegisterPayloadError::InvalidChannel)?;
        if !seen.insert(channel) {
            return Err(RegisterPayloadError::DuplicateChannel);
        }
        let separator = usize::from(index > 0);
        if result
            .len()
            .saturating_add(separator)
            .saturating_add(channel.len())
            > MAX_REGISTER_PAYLOAD_BYTES
        {
            return Err(RegisterPayloadError::PayloadTooLarge);
        }
        if index > 0 {
            result.push(0);
        }
        result.extend_from_slice(channel.as_bytes());
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelHandlerKind {
    Brand,
    Register,
    Unregister,
    NativeSeasonSync,
    NativeModSync,
    NativeMagicSync,
    NativeAccessorySync,
    NativeMediaTransfer,
    NativeClaimsSync,
    AppleSkinHud,
    MapFairPlay,
    OperatorAllowedModChannel,
    UnknownDropSilently,
}

/// Classify built-in capabilities independently of operator policy.
pub fn classify_channel(channel: &str) -> ChannelHandlerKind {
    if validate_channel_name(channel).is_err() {
        return ChannelHandlerKind::UnknownDropSilently;
    }
    match channel {
        "minecraft:brand" => ChannelHandlerKind::Brand,
        "minecraft:register" => ChannelHandlerKind::Register,
        "minecraft:unregister" => ChannelHandlerKind::Unregister,
        "megu:season_v1" => ChannelHandlerKind::NativeSeasonSync,
        "megu:mod_sync_v1" => ChannelHandlerKind::NativeModSync,
        "megu:magic_v1" => ChannelHandlerKind::NativeMagicSync,
        "megu:accessory_v1" => ChannelHandlerKind::NativeAccessorySync,
        "megu:media_v1" => ChannelHandlerKind::NativeMediaTransfer,
        "megu:claims_v1" | "openpartiesandclaims:claims_sync" | "ftbchunks:claims_sync" => {
            ChannelHandlerKind::NativeClaimsSync
        }
        "appleskin:exhaustion_sync" | "appleskin:saturation_sync" => {
            ChannelHandlerKind::AppleSkinHud
        }
        _ if channel.starts_with("xaerominimap:")
            || channel.starts_with("xaeroworldmap:")
            || channel.starts_with("journeymap:") =>
        {
            ChannelHandlerKind::MapFairPlay
        }
        _ => ChannelHandlerKind::UnknownDropSilently,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelDirection {
    Serverbound,
    Clientbound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelFirewallConfig {
    pub max_payload_bytes_default: usize,
    pub max_payload_bytes_bulk: usize,
    pub max_packets_per_second_per_peer: u64,
    pub max_bytes_per_second_per_peer: u64,
    pub max_unknown_channels_per_peer: usize,
    pub strike_disconnect_threshold: u32,
}

impl Default for ChannelFirewallConfig {
    fn default() -> Self {
        Self {
            max_payload_bytes_default: MAX_REGISTER_PAYLOAD_BYTES,
            max_payload_bytes_bulk: MAX_BULK_PAYLOAD_BYTES,
            max_packets_per_second_per_peer: 100,
            max_bytes_per_second_per_peer: MAX_BULK_PAYLOAD_BYTES as u64,
            max_unknown_channels_per_peer: MAX_REGISTERED_CHANNELS_PER_PEER,
            strike_disconnect_threshold: 5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperatorRuleError {
    InvalidChannel(ChannelNameError),
    TooManyRules,
    NotAllowed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    UnknownChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThrottleReason {
    MalformedChannel,
    OversizedPayload,
    InvalidRegisterPayload,
    RegisteredChannelLimit,
    UnknownChannelLimit,
    PacketRateExceeded,
    ByteRateExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirewallDisconnectReason {
    MalformedChannel,
    OversizedPayload,
    InvalidRegisterPayload,
    RegisteredChannelLimit,
    UnknownChannelLimit,
    PacketRateExceeded,
    ByteRateExceeded,
}

impl From<ThrottleReason> for FirewallDisconnectReason {
    fn from(reason: ThrottleReason) -> Self {
        match reason {
            ThrottleReason::MalformedChannel => Self::MalformedChannel,
            ThrottleReason::OversizedPayload => Self::OversizedPayload,
            ThrottleReason::InvalidRegisterPayload => Self::InvalidRegisterPayload,
            ThrottleReason::RegisteredChannelLimit => Self::RegisteredChannelLimit,
            ThrottleReason::UnknownChannelLimit => Self::UnknownChannelLimit,
            ThrottleReason::PacketRateExceeded => Self::PacketRateExceeded,
            ThrottleReason::ByteRateExceeded => Self::ByteRateExceeded,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FirewallVerdict {
    Route {
        handler: ChannelHandlerKind,
        channel: String,
    },
    DropSilently {
        reason: DropReason,
    },
    Throttle {
        reason: ThrottleReason,
        strikes: u32,
    },
    Disconnect {
        reason: FirewallDisconnectReason,
        strikes: u32,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelFirewallMetrics {
    pub routed: u64,
    pub dropped: u64,
    pub throttled: u64,
    pub disconnected: u64,
    pub bytes_inspected: u64,
}

#[derive(Debug)]
struct PeerState {
    last_tick: u64,
    packet_tokens: u128,
    byte_tokens: u128,
    registered: BTreeSet<String>,
    unknown: BTreeSet<String>,
    strikes: u32,
    disconnected: Option<FirewallDisconnectReason>,
}

impl PeerState {
    fn new(config: &ChannelFirewallConfig, tick: u64) -> Self {
        Self {
            last_tick: tick,
            packet_tokens: u128::from(config.max_packets_per_second_per_peer) * TICKS_PER_SECOND,
            byte_tokens: u128::from(config.max_bytes_per_second_per_peer) * TICKS_PER_SECOND,
            registered: BTreeSet::new(),
            unknown: BTreeSet::new(),
            strikes: 0,
            disconnected: None,
        }
    }

    fn refill(&mut self, config: &ChannelFirewallConfig, tick: u64) {
        let elapsed = tick.saturating_sub(self.last_tick);
        self.last_tick = self.last_tick.max(tick);
        let packet_rate = u128::from(config.max_packets_per_second_per_peer);
        let byte_rate = u128::from(config.max_bytes_per_second_per_peer);
        self.packet_tokens = self
            .packet_tokens
            .saturating_add(u128::from(elapsed).saturating_mul(packet_rate))
            .min(packet_rate * TICKS_PER_SECOND);
        self.byte_tokens = self
            .byte_tokens
            .saturating_add(u128::from(elapsed).saturating_mul(byte_rate))
            .min(byte_rate * TICKS_PER_SECOND);
    }

    fn violation(&mut self, reason: ThrottleReason, threshold: u32) -> FirewallVerdict {
        self.strikes = self.strikes.saturating_add(1);
        if self.strikes >= threshold.max(1) {
            let reason = FirewallDisconnectReason::from(reason);
            self.disconnected = Some(reason);
            FirewallVerdict::Disconnect {
                reason,
                strikes: self.strikes,
            }
        } else {
            FirewallVerdict::Throttle {
                reason,
                strikes: self.strikes,
            }
        }
    }
}

/// One independent token bucket and two bounded channel sets per connection.
/// No wall clock, network IO, mutexes, or crate dependencies are involved.
#[derive(Debug)]
pub struct ChannelFirewall {
    config: ChannelFirewallConfig,
    peers: BTreeMap<u64, PeerState>,
    operator_rules: BTreeSet<String>,
    bulk_channels: BTreeSet<String>,
    metrics: ChannelFirewallMetrics,
}

impl ChannelFirewall {
    pub fn new(mut config: ChannelFirewallConfig) -> Self {
        // Caller configuration cannot lift the allocation and payload ceilings.
        config.max_payload_bytes_default =
            config.max_payload_bytes_default.min(MAX_BULK_PAYLOAD_BYTES);
        config.max_payload_bytes_bulk = config.max_payload_bytes_bulk.min(MAX_BULK_PAYLOAD_BYTES);
        config.max_unknown_channels_per_peer = config
            .max_unknown_channels_per_peer
            .min(MAX_REGISTERED_CHANNELS_PER_PEER);
        Self {
            config,
            peers: BTreeMap::new(),
            operator_rules: BTreeSet::new(),
            bulk_channels: BTreeSet::new(),
            metrics: ChannelFirewallMetrics::default(),
        }
    }

    /// Allow one exact channel or an entire namespace (`create:*`).
    pub fn allow_operator_channel(&mut self, rule: &str) -> Result<(), OperatorRuleError> {
        validate_operator_rule(rule)?;
        if !self.operator_rules.contains(rule) && self.operator_rules.len() >= MAX_OPERATOR_RULES {
            return Err(OperatorRuleError::TooManyRules);
        }
        self.operator_rules.insert(rule.to_owned());
        Ok(())
    }

    /// Grant an exact built-in or operator-allowed channel the bulk size cap
    /// for serverbound traffic. Clientbound traffic uses the bulk cap already.
    pub fn allow_bulk_channel(&mut self, channel: &str) -> Result<(), OperatorRuleError> {
        validate_channel_name(channel).map_err(OperatorRuleError::InvalidChannel)?;
        if self.handler_for(channel) == ChannelHandlerKind::UnknownDropSilently {
            return Err(OperatorRuleError::NotAllowed);
        }
        if !self.bulk_channels.contains(channel) && self.bulk_channels.len() >= MAX_OPERATOR_RULES {
            return Err(OperatorRuleError::TooManyRules);
        }
        self.bulk_channels.insert(channel.to_owned());
        Ok(())
    }

    pub fn handler_for(&self, channel: &str) -> ChannelHandlerKind {
        let builtin = classify_channel(channel);
        if builtin != ChannelHandlerKind::UnknownDropSilently {
            return builtin;
        }
        if validate_channel_name(channel).is_ok()
            && (self.operator_rules.contains(channel)
                || channel.split_once(':').is_some_and(|(namespace, _)| {
                    let mut rule = String::with_capacity(namespace.len() + 2);
                    rule.push_str(namespace);
                    rule.push_str(":*");
                    self.operator_rules.contains(&rule)
                }))
        {
            ChannelHandlerKind::OperatorAllowedModChannel
        } else {
            ChannelHandlerKind::UnknownDropSilently
        }
    }

    /// Inspect one entire CustomPayload body. The caller supplies a unique
    /// connection id, protocol tick and packet direction.
    pub fn inspect(
        &mut self,
        peer_id: u64,
        tick: u64,
        direction: ChannelDirection,
        channel: &str,
        payload: &[u8],
    ) -> FirewallVerdict {
        self.metrics.bytes_inspected = self
            .metrics
            .bytes_inspected
            .saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        let handler = self.handler_for(channel);
        let size_limit =
            if direction == ChannelDirection::Clientbound || self.bulk_channels.contains(channel) {
                self.config.max_payload_bytes_bulk
            } else {
                self.config.max_payload_bytes_default
            };
        let peer = self
            .peers
            .entry(peer_id)
            .or_insert_with(|| PeerState::new(&self.config, tick));
        let was_disconnected = peer.disconnected.is_some();
        let verdict = inspect_peer(
            peer,
            &self.config,
            tick,
            channel,
            payload,
            handler,
            size_limit,
        );
        match &verdict {
            FirewallVerdict::Route { .. } => {
                self.metrics.routed = self.metrics.routed.saturating_add(1);
            }
            FirewallVerdict::DropSilently { .. } => {
                self.metrics.dropped = self.metrics.dropped.saturating_add(1);
            }
            FirewallVerdict::Throttle { .. } => {
                self.metrics.throttled = self.metrics.throttled.saturating_add(1);
            }
            FirewallVerdict::Disconnect { .. } if !was_disconnected => {
                self.metrics.disconnected = self.metrics.disconnected.saturating_add(1);
            }
            FirewallVerdict::Disconnect { .. } => {}
        }
        verdict
    }

    /// Negotiation is informational; the caller can gate outbound sends on it.
    pub fn is_peer_channel_supported(&self, peer_id: u64, channel: &str) -> bool {
        self.peers
            .get(&peer_id)
            .is_some_and(|peer| peer.registered.contains(channel))
    }

    pub fn peer_unknown_channel_count(&self, peer_id: u64) -> usize {
        self.peers
            .get(&peer_id)
            .map_or(0, |peer| peer.unknown.len())
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Release both negotiated and unknown channel sets on disconnect.
    pub fn remove_peer(&mut self, peer_id: u64) -> bool {
        self.peers.remove(&peer_id).is_some()
    }

    pub fn metrics(&self) -> ChannelFirewallMetrics {
        self.metrics
    }
}

fn validate_operator_rule(rule: &str) -> Result<(), OperatorRuleError> {
    if let Some(namespace) = rule.strip_suffix(":*") {
        let mut probe = String::with_capacity(namespace.len() + 2);
        probe.push_str(namespace);
        probe.push_str(":x");
        validate_channel_name(&probe).map_err(OperatorRuleError::InvalidChannel)
    } else {
        validate_channel_name(rule).map_err(OperatorRuleError::InvalidChannel)
    }
}

fn inspect_peer(
    peer: &mut PeerState,
    config: &ChannelFirewallConfig,
    tick: u64,
    channel: &str,
    payload: &[u8],
    handler: ChannelHandlerKind,
    size_limit: usize,
) -> FirewallVerdict {
    if let Some(reason) = peer.disconnected {
        return FirewallVerdict::Disconnect {
            reason,
            strikes: peer.strikes,
        };
    }
    peer.refill(config, tick);
    let threshold = config.strike_disconnect_threshold;
    if validate_channel_name(channel).is_err() {
        return peer.violation(ThrottleReason::MalformedChannel, threshold);
    }
    if payload.len() > size_limit {
        return peer.violation(ThrottleReason::OversizedPayload, threshold);
    }
    if peer.packet_tokens < TICKS_PER_SECOND {
        return peer.violation(ThrottleReason::PacketRateExceeded, threshold);
    }
    peer.packet_tokens -= TICKS_PER_SECOND;
    let byte_cost = (payload.len() as u128) * TICKS_PER_SECOND;
    if peer.byte_tokens < byte_cost {
        return peer.violation(ThrottleReason::ByteRateExceeded, threshold);
    }
    peer.byte_tokens -= byte_cost;

    if matches!(
        handler,
        ChannelHandlerKind::Register | ChannelHandlerKind::Unregister
    ) {
        let names = match decode_register_payload(payload) {
            Ok(names) => names,
            Err(_) => return peer.violation(ThrottleReason::InvalidRegisterPayload, threshold),
        };
        if handler == ChannelHandlerKind::Register {
            let new_count = names
                .iter()
                .filter(|name| !peer.registered.contains(name.as_str()))
                .count();
            if peer.registered.len().saturating_add(new_count) > MAX_REGISTERED_CHANNELS_PER_PEER {
                return peer.violation(ThrottleReason::RegisteredChannelLimit, threshold);
            }
            peer.registered.extend(names);
        } else {
            for name in names {
                peer.registered.remove(&name);
            }
        }
    }

    if handler == ChannelHandlerKind::UnknownDropSilently {
        if !peer.unknown.contains(channel) {
            if peer.unknown.len() >= config.max_unknown_channels_per_peer {
                return peer.violation(ThrottleReason::UnknownChannelLimit, threshold);
            }
            peer.unknown.insert(channel.to_owned());
        }
        return FirewallVerdict::DropSilently {
            reason: DropReason::UnknownChannel,
        };
    }

    FirewallVerdict::Route {
        handler,
        channel: channel.to_owned(),
    }
}

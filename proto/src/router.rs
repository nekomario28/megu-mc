// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Multi-version glue: the pieces that turn "one hardcoded 1.21.1 codec" into
//! "a codec selected per connection, speaking a table loaded per version".
//!
//! Three roles:
//!
//! * [`TableDrivenCodec`] — a [`ProtocolCodec`] over *any*
//!   `(VersionCapabilities, ProtocolTable)` pair. Packet ids and menu-type
//!   registry ids resolve through the table at run time, so a loaded
//!   1.21.4/769 `VersionPack` with renumbered play packets works without a
//!   second codec implementation — packet bodies are written exactly once in
//!   `codec.rs` and shared with [`crate::codec::CodecV767`].
//! * [`SessionRemapper`] — per-connection id translation for the five core
//!   registries (`block`, `item`, `entity_type`, `menu`,
//!   `data_component_type`), built once at handshake between the canonical
//!   (server-native) table and the negotiated client table. Remaps key on
//!   resource name — never on "id plus a guessed offset" — and report every
//!   name that exists on only one side through [`SessionRemapSummary`].
//! * [`VersionRouter`] — the handshake entry point: registered codecs by
//!   protocol number, [`VersionRouter::select`] returning an
//!   `Arc<dyn ProtocolCodec>` or failing closed with
//!   [`ProtoError::UnsupportedVersion`].

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::codec::{self, C2SPacket, CodecV767, ProtocolCodec, S2CPacket};
use crate::table::{IdRemapper, PacketPhase, ProtocolTable, Registry, RemapReport};
use crate::version::{ProtocolVersion, VersionCapabilities};
use crate::wire::ProtoError;

/// The five registries a session translates. These names are the contract
/// every [`ProtocolTable`] — built-in or pack-loaded — is expected to carry.
const REGISTRY_BLOCK: &str = "minecraft:block";
const REGISTRY_ITEM: &str = "minecraft:item";
const REGISTRY_ENTITY_TYPE: &str = "minecraft:entity_type";
const REGISTRY_MENU: &str = "minecraft:menu";
const REGISTRY_DATA_COMPONENT: &str = "minecraft:data_component_type";

// ---------------------------------------------------------------------------
// SessionRemapper
// ---------------------------------------------------------------------------

/// Coverage of one [`SessionRemapper`]: the [`RemapReport`] each of the five
/// registries produced at build time. `unmapped_*` entries are names that
/// exist on only one side — entity types a newer client knows that the
/// canonical version cannot name, and vice versa.
#[derive(Clone, Debug, Default)]
pub struct SessionRemapSummary {
    /// `minecraft:block` coverage.
    pub blocks: RemapReport,
    /// `minecraft:item` coverage.
    pub items: RemapReport,
    /// `minecraft:entity_type` coverage.
    pub entities: RemapReport,
    /// `minecraft:menu` coverage.
    pub menus: RemapReport,
    /// `minecraft:data_component_type` coverage.
    pub components: RemapReport,
}

impl SessionRemapSummary {
    /// `true` when every entry in every registry had a counterpart on the
    /// other side — the zero-loss session.
    pub fn is_clean(&self) -> bool {
        [
            &self.blocks,
            &self.items,
            &self.entities,
            &self.menus,
            &self.components,
        ]
        .iter()
        .all(|r| r.unmapped_canonical.is_empty() && r.unmapped_wire.is_empty())
    }
}

/// Per-connection id translation between the canonical table (the server's
/// native version) and the negotiated client table.
///
/// Build once when the handshake settles, then translate every registry id
/// crossing the boundary: canonical ids heading to the client go through
/// `*_to_wire`, client ids heading inward go through `*_from_wire`.
/// Translation is `Option`-returning by design — a name the other version
/// does not have is `None`, and the caller decides how to fail (vanilla
/// substitutes or disconnects; megu must never silently emit a wrong id).
///
/// When both tables declare the same protocol version the remapper is
/// *identity*: translation is a free pass-through. The fast path assumes an
/// honest pack — a table that claims a version while carrying different
/// contents is malformed input, not something a remap can paper over.
#[derive(Debug)]
pub struct SessionRemapper {
    identity: bool,
    blocks: IdRemapper,
    items: IdRemapper,
    entities: IdRemapper,
    menus: IdRemapper,
    components: IdRemapper,
    summary: SessionRemapSummary,
}

impl SessionRemapper {
    /// Build the five dense O(1) remappers between `canonical` and `client`.
    ///
    /// Fails closed when the versions differ and a required registry is
    /// missing from either table — silently skipping a registry would let
    /// ids pass through untranslated. On an identity session a missing
    /// registry is missing on both sides, so pass-through stays correct.
    pub fn build(canonical: &ProtocolTable, client: &ProtocolTable) -> Result<Self, ProtoError> {
        let identity = canonical.protocol_version().is_some()
            && canonical.protocol_version() == client.protocol_version();
        let remapper = |name: &'static str| -> Result<IdRemapper, ProtoError> {
            match (canonical.registry(name), client.registry(name)) {
                (Some(c), Some(w)) => Ok(IdRemapper::build(c, w)),
                _ if identity => Ok(IdRemapper::build(&Registry::new(), &Registry::new())),
                _ => Err(ProtoError::VersionPack(
                    "session remap: required registry missing",
                )),
            }
        };
        let blocks = remapper(REGISTRY_BLOCK)?;
        let items = remapper(REGISTRY_ITEM)?;
        let entities = remapper(REGISTRY_ENTITY_TYPE)?;
        let menus = remapper(REGISTRY_MENU)?;
        let components = remapper(REGISTRY_DATA_COMPONENT)?;
        let summary = SessionRemapSummary {
            blocks: blocks.report().clone(),
            items: items.report().clone(),
            entities: entities.report().clone(),
            menus: menus.report().clone(),
            components: components.report().clone(),
        };
        Ok(Self {
            identity,
            blocks,
            items,
            entities,
            menus,
            components,
            summary,
        })
    }

    /// `true` when both tables declared the same protocol version — all
    /// translations are pass-through.
    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// Registry coverage computed at build time.
    pub fn summary(&self) -> &SessionRemapSummary {
        &self.summary
    }

    /// `minecraft:block` canonical id -> wire id.
    pub fn block_to_wire(&self, canonical_id: u32) -> Option<u32> {
        if self.identity {
            return Some(canonical_id);
        }
        self.blocks.to_wire(canonical_id)
    }

    /// `minecraft:block` wire id -> canonical id.
    pub fn block_from_wire(&self, wire_id: u32) -> Option<u32> {
        if self.identity {
            return Some(wire_id);
        }
        self.blocks.from_wire(wire_id)
    }

    /// `minecraft:item` canonical id -> wire id.
    pub fn item_to_wire(&self, canonical_id: u32) -> Option<u32> {
        if self.identity {
            return Some(canonical_id);
        }
        self.items.to_wire(canonical_id)
    }

    /// `minecraft:item` wire id -> canonical id.
    pub fn item_from_wire(&self, wire_id: u32) -> Option<u32> {
        if self.identity {
            return Some(wire_id);
        }
        self.items.from_wire(wire_id)
    }

    /// `minecraft:entity_type` canonical id -> wire id.
    pub fn entity_to_wire(&self, canonical_id: u32) -> Option<u32> {
        if self.identity {
            return Some(canonical_id);
        }
        self.entities.to_wire(canonical_id)
    }

    /// `minecraft:entity_type` wire id -> canonical id.
    pub fn entity_from_wire(&self, wire_id: u32) -> Option<u32> {
        if self.identity {
            return Some(wire_id);
        }
        self.entities.from_wire(wire_id)
    }

    /// `minecraft:menu` canonical id -> wire id.
    pub fn menu_to_wire(&self, canonical_id: u32) -> Option<u32> {
        if self.identity {
            return Some(canonical_id);
        }
        self.menus.to_wire(canonical_id)
    }

    /// `minecraft:menu` wire id -> canonical id.
    pub fn menu_from_wire(&self, wire_id: u32) -> Option<u32> {
        if self.identity {
            return Some(wire_id);
        }
        self.menus.from_wire(wire_id)
    }

    /// `minecraft:data_component_type` canonical id -> wire id.
    pub fn component_to_wire(&self, canonical_id: u32) -> Option<u32> {
        if self.identity {
            return Some(canonical_id);
        }
        self.components.to_wire(canonical_id)
    }

    /// `minecraft:data_component_type` wire id -> canonical id.
    pub fn component_from_wire(&self, wire_id: u32) -> Option<u32> {
        if self.identity {
            return Some(wire_id);
        }
        self.components.from_wire(wire_id)
    }
}

// ---------------------------------------------------------------------------
// TableDrivenCodec
// ---------------------------------------------------------------------------

/// A [`ProtocolCodec`] over any `(VersionCapabilities, ProtocolTable)` pair.
///
/// This is the multi-version twin of [`CodecV767`]: the packet IR and every
/// body encoder/decoder are shared verbatim — this shell only carries the
/// table ids resolve through and the capability profile wire-shape deltas
/// key on. A 1.21.4 table loaded from a `VersionPack` therefore gets a full
/// codec — renumbered play packets, renumbered `minecraft:menu` ids, the
/// flag-byte `player_input` — with no packet-format code of its own.
///
/// Anything the table cannot resolve fails closed: a missing packet entry is
/// [`ProtoError::UnknownPacketId`], a menu kind absent from the
/// `minecraft:menu` registry is [`ProtoError::Malformed`], an unknown wire id
/// is [`ProtoError::UnknownPacketId`]/[`ProtoError::InvalidDiscriminant`].
/// Nothing here ever guesses an id.
#[derive(Debug)]
pub struct TableDrivenCodec {
    table: ProtocolTable,
    capabilities: VersionCapabilities,
    phase: PacketPhase,
}

impl TableDrivenCodec {
    /// Codec for `version` over `table` — the normal constructor. `version`
    /// supplies the capability profile; `table` supplies every id.
    pub fn for_version(version: ProtocolVersion, table: ProtocolTable) -> Self {
        Self {
            table,
            capabilities: version.capabilities(),
            phase: PacketPhase::Handshake,
        }
    }

    /// Codec over an explicit `(capabilities, table)` pair — the escape
    /// hatch for a pack whose wire shapes differ from any released version,
    /// e.g. a snapshot between two capability profiles.
    pub fn with_capabilities(
        capabilities: VersionCapabilities,
        table: ProtocolTable,
    ) -> Self {
        Self {
            table,
            capabilities,
            phase: PacketPhase::Handshake,
        }
    }

    /// The phase the next [`ProtocolCodec::decode_s2c`]/`decode_c2s`
    /// resolves packet ids in.
    pub fn phase(&self) -> PacketPhase {
        self.phase
    }

    /// Advance the phase after a transition packet, mirroring
    /// [`CodecV767::set_phase`].
    pub fn set_phase(&mut self, phase: PacketPhase) {
        self.phase = phase;
    }

    /// Decode one clientbound frame as `phase` without changing codec state.
    pub fn decode_s2c_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<S2CPacket, ProtoError> {
        codec::decode_s2c_in(&self.capabilities, &self.table, phase, frame)
    }

    /// Decode one serverbound frame as `phase` without changing codec state.
    pub fn decode_c2s_in(&self, phase: PacketPhase, frame: &[u8]) -> Result<C2SPacket, ProtoError> {
        codec::decode_c2s_in(&self.table, &self.capabilities, phase, frame)
    }
}

impl ProtocolCodec for TableDrivenCodec {
    fn capabilities(&self) -> &VersionCapabilities {
        &self.capabilities
    }

    fn table(&self) -> &ProtocolTable {
        &self.table
    }

    fn encode_s2c(&self, packet: &S2CPacket, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        codec::with_body_scratch(|body| {
            let (phase, name) =
                codec::encode_s2c_body_into(&self.capabilities, &self.table, packet, body)?;
            codec::framed(
                &self.table,
                phase,
                crate::table::PacketDir::Clientbound,
                name,
                body,
                out,
            )
        })
    }

    fn decode_s2c(&self, frame: &[u8]) -> Result<S2CPacket, ProtoError> {
        self.decode_s2c_in(self.phase, frame)
    }

    fn encode_c2s(&self, packet: &C2SPacket, out: &mut Vec<u8>) -> Result<(), ProtoError> {
        codec::with_body_scratch(|body| {
            let (phase, name) = codec::encode_c2s_body_into(&self.capabilities, packet, body)?;
            codec::framed(
                &self.table,
                phase,
                crate::table::PacketDir::Serverbound,
                name,
                body,
                out,
            )
        })
    }

    fn decode_c2s(&self, frame: &[u8]) -> Result<C2SPacket, ProtoError> {
        self.decode_c2s_in(self.phase, frame)
    }
}

// ---------------------------------------------------------------------------
// VersionRouter
// ---------------------------------------------------------------------------

/// Registered codecs by handshake protocol number.
///
/// The router is how a listener stays version-agnostic: the handshake packet
/// carries a protocol number, [`VersionRouter::select`] answers the codec
/// that speaks it — or fails closed with
/// [`ProtoError::UnsupportedVersion`], listing what *is* registered, rather
/// than falling back to the wrong table (the "talk 767 to a 769 client"
/// bug this type exists to prevent).
///
/// `Default` is an empty router — nothing selectable until [`Self::register`]
/// runs. [`Self::with_v767_default`] is the one-version configuration
/// matching today's behaviour.
#[derive(Default)]
pub struct VersionRouter {
    codecs: BTreeMap<i32, Arc<dyn ProtocolCodec>>,
}

impl VersionRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Router carrying the built-in 1.21.1/767 codec.
    pub fn with_v767_default() -> Self {
        let mut router = Self::new();
        router.register(Arc::new(CodecV767::new()));
        router
    }

    /// Router carrying the two versions of the current target set:
    /// 1.21.1/767 through [`CodecV767`] and 26.3/777 through a
    /// [`TableDrivenCodec`] over [`ProtocolTable::v777_builtin`].
    pub fn with_v767_and_v777() -> Self {
        let mut router = Self::with_v767_default();
        router.register(Arc::new(TableDrivenCodec::for_version(
            ProtocolVersion::V26_3,
            ProtocolTable::v777_builtin(),
        )));
        router
    }

    /// Register `codec` under the protocol number its capabilities declare
    /// ([`VersionCapabilities::protocol_version`]). Re-registering a version
    /// replaces the previous codec — registration order is irrelevant.
    pub fn register(&mut self, codec: Arc<dyn ProtocolCodec>) {
        self.codecs
            .insert(codec.capabilities().protocol_version, codec);
    }

    /// Codec for the protocol number the peer sent in its handshake.
    ///
    /// Fails closed on an unregistered number: [`ProtocolVersion::Custom`]
    /// numbers are *not* silently mapped to the nearest codec — a version
    /// must be explicitly registered to be served.
    pub fn select(&self, protocol_version: i32) -> Result<Arc<dyn ProtocolCodec>, ProtoError> {
        self.codecs
            .get(&protocol_version)
            .cloned()
            .ok_or_else(|| ProtoError::UnsupportedVersion {
                requested: protocol_version,
                supported: self.supported_versions(),
            })
    }

    /// Registered protocol numbers, ascending — also the `supported` list
    /// carried by [`ProtoError::UnsupportedVersion`].
    pub fn supported_versions(&self) -> Vec<i32> {
        self.codecs.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{
        C2SPacket, EquipmentSlot, ItemStack, MenuKind, RawComponent, RegistryDataEntry,
        S2CPacket, TeamOperation,
    };
    use crate::table::Registry;
    use crate::wire::{read_frame, ProtoError};

    /// Anonymous-NBT `{"text":"..."}` — the same helper shape codec tests use.
    fn text_component(text: &str) -> Vec<u8> {
        let mut v = vec![10]; // TAG_Compound
        v.push(8); // TAG_String
        v.extend_from_slice(&4u16.to_be_bytes());
        v.extend_from_slice(b"text");
        v.extend_from_slice(&(text.len() as u16).to_be_bytes());
        v.extend_from_slice(text.as_bytes());
        v.push(0);
        v
    }

    /// A dense registry = `names` at `0..len`, with `extra` inserted at
    /// `extra_at` — how each synthetic-769 registry gains its shift.
    fn shifted(names: Vec<&str>, extra: &str, extra_at: usize) -> Registry {
        let mut all = names;
        all.insert(extra_at, extra);
        let mut reg = Registry::new();
        for (id, &name) in all.iter().enumerate() {
            reg.insert(name, id as u32).unwrap();
        }
        reg
    }

    /// The synthetic 1.21.4 pack. Real vanilla 769 renumbering is broader,
    /// but every delta a session depends on is present: shifted play packet
    /// ids, `minecraft:creaking` inserted mid-`entity_type`, menus/blocks/
    /// items/components renumbered, and — through `V1_21_4` capabilities —
    /// the flag-byte `player_input`.
    fn synthetic_769() -> ProtocolTable {
        let canonical = ProtocolTable::v767_builtin();
        let mut table = ProtocolTable::from_version_pack_json(
            r#"{"protocol_version": 769, "phases": {"play": {
                "clientbound": {"keep_alive": 39, "open_screen": 52,
                                "ticking_state": 115},
                "serverbound": {"keep_alive": 25, "player_input": 39}
            }}}"#,
            r#"{"registries": {}}"#,
        )
        .unwrap();

        // entity_type: 1.21.4 inserts `creaking` after `cow` (vanilla also
        // adds `creaking_transient`; one name exercises the shift). One
        // canonical-only name is dropped to exercise unmapped_canonical —
        // `zombie`, well past `creeper`, so the drop cannot cancel the
        // `creaking` shift under test.
        let names: Vec<&str> = canonical
            .registry("minecraft:entity_type")
            .unwrap()
            .entries()
            .map(|(_, n)| n)
            .filter(|n| *n != "minecraft:zombie")
            .collect();
        let cow_at = names.iter().position(|n| *n == "minecraft:cow").unwrap();
        table.insert_registry(
            "minecraft:entity_type",
            shifted(names, "minecraft:creaking", cow_at + 1),
        );

        // menus/blocks/items/components: a pack-local first entry shifts
        // every real id by one — the remap must follow names, not offsets.
        for registry in [
            "minecraft:menu",
            "minecraft:block",
            "minecraft:item",
            "minecraft:data_component_type",
        ] {
            let names: Vec<&str> = canonical
                .registry(registry)
                .unwrap()
                .entries()
                .map(|(_, n)| n)
                .collect();
            table.insert_registry(
                registry,
                shifted(names, "minecraft:pack_local_synthetic", 0),
            );
        }
        table
    }

    fn codec_769() -> TableDrivenCodec {
        let mut c = TableDrivenCodec::for_version(ProtocolVersion::V1_21_4, synthetic_769());
        c.set_phase(PacketPhase::Play);
        c
    }

    // -- TableDrivenCodec ---------------------------------------------------

    #[test]
    fn table_driven_v767_is_byte_identical_to_codec_v767() {
        // Same table + same capabilities must produce the same bytes — the
        // two shells share one engine, and this pins that.
        let mut td = TableDrivenCodec::for_version(
            ProtocolVersion::V1_21_1,
            ProtocolTable::v767_builtin(),
        );
        td.set_phase(PacketPhase::Play);
        let mut v767 = CodecV767::new();
        v767.set_phase(PacketPhase::Play);
        let packets = [
            S2CPacket::KeepAlive { id: -42 },
            S2CPacket::OpenScreen {
                container_id: 7,
                menu: MenuKind::Anvil,
                title: text_component("hi"),
            },
            S2CPacket::TickingState {
                tick_rate: 20.0,
                is_frozen: false,
            },
        ];
        for p in &packets {
            let mut a = Vec::new();
            let mut b = Vec::new();
            td.encode_s2c(p, &mut a).unwrap();
            v767.encode_s2c(p, &mut b).unwrap();
            assert_eq!(a, b);
            assert_eq!(td.decode_s2c(&a).unwrap(), *p);
        }
        let input = C2SPacket::PlayerInput {
            sideways: -0.5,
            forward: 0.98,
            jump: true,
            unmount: false,
        };
        let mut a = Vec::new();
        let mut b = Vec::new();
        td.encode_c2s(&input, &mut a).unwrap();
        v767.encode_c2s(&input, &mut b).unwrap();
        assert_eq!(a, b);
        assert_eq!(td.decode_c2s(&a).unwrap(), input);
    }

    #[test]
    fn table_driven_769_uses_shifted_packet_ids() {
        let c769 = codec_769();
        let mut v767 = CodecV767::new();
        v767.set_phase(PacketPhase::Play);

        let mut f767 = Vec::new();
        let mut f769 = Vec::new();
        v767.encode_s2c(&S2CPacket::KeepAlive { id: 9 }, &mut f767)
            .unwrap();
        c769.encode_s2c(&S2CPacket::KeepAlive { id: 9 }, &mut f769)
            .unwrap();
        // Same payload, different wire id: 0x26 in 767, 0x27 in the pack.
        let (id767, body767) = read_frame(&f767).unwrap();
        let (id769, body769) = read_frame(&f769).unwrap();
        assert_eq!(id767, 0x26);
        assert_eq!(id769, 39);
        assert_eq!(body767, body769);
        assert_eq!(c769.decode_s2c(&f769).unwrap(), S2CPacket::KeepAlive { id: 9 });
    }

    #[test]
    fn table_driven_769_open_screen_resolves_menu_through_its_table() {
        // The pack's menu registry shifted every menu id +1; encode picks
        // the pack's id, decode maps it back by name — no constants.
        let c769 = codec_769();
        let t769 = c769.table();
        let anvil_769 = t769.registry_id("minecraft:menu", "minecraft:anvil").unwrap();
        assert_eq!(
            anvil_769,
            ProtocolTable::v767_builtin()
                .registry_id("minecraft:menu", "minecraft:anvil")
                .unwrap()
                + 1
        );

        let packet = S2CPacket::OpenScreen {
            container_id: 3,
            menu: MenuKind::Anvil,
            title: text_component("anvil"),
        };
        let mut frame = Vec::new();
        c769.encode_s2c(&packet, &mut frame).unwrap();
        let (id, _) = read_frame(&frame).unwrap();
        assert_eq!(id, 52); // the pack's open_screen id
        assert_eq!(c769.decode_s2c(&frame).unwrap(), packet);
    }

    #[test]
    fn table_driven_769_player_input_is_flag_byte() {
        // The real 1.21.1 vs 1.21.4 wire delta: 767 writes
        // `f32,f32,u8`; the pack writes one `Input` flag byte.
        let c769 = codec_769();
        let mut frame = Vec::new();
        c769.encode_c2s(
            &C2SPacket::PlayerInputFlags {
                forward: true,
                backward: false,
                left: false,
                right: true,
                jump: true,
                shift: false,
                sprint: true,
            },
            &mut frame,
        )
        .unwrap();
        let (id, body) = read_frame(&frame).unwrap();
        assert_eq!(id, 39);
        assert_eq!(body, &[0x01 | 0x08 | 0x10 | 0x40]);
        assert_eq!(
            c769.decode_c2s(&frame).unwrap(),
            C2SPacket::PlayerInputFlags {
                forward: true,
                backward: false,
                left: false,
                right: true,
                jump: true,
                shift: false,
                sprint: true,
            }
        );
    }

    #[test]
    fn player_input_cross_form_encoding() {
        // The float form encodes under the flag capability by quantizing to
        // signs; the flag form encodes under 767 as vanilla ±1.0 impulses.
        let c769 = codec_769();
        let mut frame = Vec::new();
        c769.encode_c2s(
            &C2SPacket::PlayerInput {
                sideways: 0.5, // left
                forward: -1.0, // backward
                jump: true,
                unmount: true, // -> shift
            },
            &mut frame,
        )
        .unwrap();
        assert_eq!(
            c769.decode_c2s(&frame).unwrap(),
            C2SPacket::PlayerInputFlags {
                forward: false,
                backward: true,
                left: true,
                right: false,
                jump: true,
                shift: true,
                sprint: false,
            }
        );

        let mut v767 = CodecV767::new();
        v767.set_phase(PacketPhase::Play);
        let mut frame = Vec::new();
        v767.encode_c2s(
            &C2SPacket::PlayerInputFlags {
                forward: true,
                backward: false,
                left: false,
                right: true, // sideways impulse -1.0
                jump: false,
                shift: true, // -> unmount
                sprint: true, // dropped: 767 has no wire bit for it
            },
            &mut frame,
        )
        .unwrap();
        assert_eq!(
            v767.decode_c2s(&frame).unwrap(),
            C2SPacket::PlayerInput {
                sideways: -1.0,
                forward: 1.0,
                jump: false,
                unmount: true,
            }
        );
    }

    #[test]
    fn table_driven_fails_closed_on_missing_packet() {
        // `ticking_step` exists in neither direction of the pack's play
        // table — the codec cannot fake an id for it.
        let mut c = TableDrivenCodec::for_version(ProtocolVersion::V1_21_4, synthetic_769());
        c.set_phase(PacketPhase::Play);
        let err = c
            .encode_s2c(&S2CPacket::TickingStep { steps: 1 }, &mut Vec::new())
            .unwrap_err();
        assert!(matches!(err, ProtoError::UnknownPacketId { .. }));
    }

    #[test]
    fn table_driven_fails_closed_on_missing_menu_registry() {
        let table = ProtocolTable::from_version_pack_json(
            r#"{"protocol_version": 769, "phases": {"play": {
                "clientbound": {"open_screen": 52}}}}"#,
            r#"{"registries": {}}"#,
        )
        .unwrap();
        let c = TableDrivenCodec::for_version(ProtocolVersion::V1_21_4, table);
        let err = c
            .encode_s2c(
                &S2CPacket::OpenScreen {
                    container_id: 1,
                    menu: MenuKind::Anvil,
                    title: text_component("x"),
                },
                &mut Vec::new(),
            )
            .unwrap_err();
        assert!(matches!(err, ProtoError::Malformed(_)));
    }

    #[test]
    fn table_driven_fails_closed_on_unknown_wire_id() {
        let c769 = codec_769();
        let mut frame = Vec::new();
        crate::wire::write_frame(&mut frame, 0x7f, &[]);
        assert!(matches!(
            c769.decode_s2c(&frame).unwrap_err(),
            ProtoError::UnknownPacketId { .. }
        ));
    }

    // -- SessionRemapper -----------------------------------------------------

    #[test]
    fn remapper_identity_passthrough() {
        let canonical = ProtocolTable::v767_builtin();
        let r = SessionRemapper::build(&canonical, &canonical).unwrap();
        assert!(r.is_identity());
        assert_eq!(r.block_to_wire(5), Some(5));
        assert_eq!(r.block_from_wire(5), Some(5));
        assert_eq!(r.entity_to_wire(100), Some(100));
        // Same table, nothing unmapped.
        assert!(r.summary().is_clean());
    }

    #[test]
    fn remapper_entity_creaking_shift() {
        let canonical = ProtocolTable::v767_builtin();
        let t769 = synthetic_769();
        let r = SessionRemapper::build(&canonical, &t769).unwrap();
        assert!(!r.is_identity());

        let creg = canonical.registry("minecraft:entity_type").unwrap();
        let wreg = t769.registry("minecraft:entity_type").unwrap();
        // `creaking` exists only on the wire side, inserted before `creeper`:
        // every entity from `creeper` on is canonical_id + 1 on the wire.
        let cow = creg.id("minecraft:cow").unwrap();
        let creeper = creg.id("minecraft:creeper").unwrap();
        assert_eq!(r.entity_to_wire(cow), Some(cow)); // before the insertion
        assert_eq!(r.entity_to_wire(creeper), Some(creeper + 1));
        assert_eq!(r.entity_from_wire(creeper + 1), Some(creeper));
        // The inserted wire name has no canonical meaning.
        assert_eq!(r.entity_from_wire(wreg.id("minecraft:creaking").unwrap()), None);
        // `zombie` was dropped from the pack: canonical-only, unmapped.
        let zombie = creg.id("minecraft:zombie").unwrap();
        assert_eq!(r.entity_to_wire(zombie), None);

        let report = &r.summary().entities;
        assert!(report
            .unmapped_canonical
            .iter()
            .any(|(_, n)| n == "minecraft:zombie"));
        assert!(report
            .unmapped_wire
            .iter()
            .any(|(_, n)| n == "minecraft:creaking"));
    }

    #[test]
    fn remapper_menus_items_blocks_components_shift() {
        let canonical = ProtocolTable::v767_builtin();
        let t769 = synthetic_769();
        let r = SessionRemapper::build(&canonical, &t769).unwrap();

        for registry in [
            "minecraft:menu",
            "minecraft:block",
            "minecraft:item",
            "minecraft:data_component_type",
        ] {
            assert_eq!(
                t769.registry(registry).unwrap().id("minecraft:pack_local_synthetic"),
                Some(0)
            );
        }

        // Menu: anvil moved +1 on the wire.
        let anvil = canonical
            .registry_id("minecraft:menu", "minecraft:anvil")
            .unwrap();
        assert_eq!(r.menu_to_wire(anvil), Some(anvil + 1));
        assert_eq!(r.menu_from_wire(anvil + 1), Some(anvil));
        assert_eq!(r.menu_from_wire(0), None); // pack-local menu id 0

        // Item: everything shifted +1 by the synthetic first entry.
        let item = canonical
            .registry_id("minecraft:item", "minecraft:stone")
            .unwrap();
        assert_eq!(r.item_to_wire(item), Some(item + 1));
        assert_eq!(r.item_from_wire(item + 1), Some(item));

        // Block: same shift.
        let block = canonical
            .registry_id("minecraft:block", "minecraft:stone")
            .unwrap();
        assert_eq!(r.block_to_wire(block), Some(block + 1));
        assert_eq!(r.block_from_wire(block + 1), Some(block));

        // Data components: same shift.
        let comp = canonical
            .registry_id("minecraft:data_component_type", "minecraft:damage")
            .unwrap();
        assert_eq!(r.component_to_wire(comp), Some(comp + 1));
        assert_eq!(r.component_from_wire(comp + 1), Some(comp));
        // The inserted pack-local name is unmapped on the canonical side.
        let synth = t769
            .registry_id("minecraft:menu", "minecraft:pack_local_synthetic")
            .unwrap();
        assert_eq!(r.menu_from_wire(synth), None);
    }

    #[test]
    fn remapper_missing_registry_fails_closed() {
        let canonical = ProtocolTable::v767_builtin();
        // A non-identity table carrying no registries at all.
        let sparse = ProtocolTable::from_version_pack_json(
            r#"{"protocol_version": 769, "phases": {}}"#,
            r#"{"registries": {}}"#,
        )
        .unwrap();
        assert!(matches!(
            SessionRemapper::build(&canonical, &sparse),
            Err(ProtoError::VersionPack(_))
        ));
    }

    // -- VersionRouter --------------------------------------------------------

    #[test]
    fn router_default_selects_v767_and_rejects_others() {
        let router = VersionRouter::with_v767_default();
        assert_eq!(router.supported_versions(), vec![767]);
        let codec = router.select(767).unwrap();
        assert_eq!(
            codec.capabilities().protocol_version,
            ProtocolVersion::V1_21_1.number()
        );

        let err = router.select(769).err().unwrap();
        match err {
            ProtoError::UnsupportedVersion {
                requested,
                supported,
            } => {
                assert_eq!(requested, 769);
                assert_eq!(supported, vec![767]);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
        // `Custom` numbers are not silently mapped either.
        assert!(matches!(
            router.select(ProtocolVersion::Custom(999).number()).err().unwrap(),
            ProtoError::UnsupportedVersion { .. }
        ));
        // An empty router rejects even 767.
        assert!(matches!(
            VersionRouter::new().select(767).err().unwrap(),
            ProtoError::UnsupportedVersion { .. }
        ));
    }

    #[test]
    fn router_serves_registered_table_driven_codec() {
        let mut router = VersionRouter::with_v767_default();
        router.register(Arc::new(codec_769()));
        assert_eq!(router.supported_versions(), vec![767, 769]);

        let codec = router.select(769).unwrap();
        // The selected Arc<dyn ProtocolCodec> speaks the pack's ids.
        let mut frame = Vec::new();
        codec
            .encode_s2c(&S2CPacket::KeepAlive { id: 1 }, &mut frame)
            .unwrap();
        let (id, _) = read_frame(&frame).unwrap();
        assert_eq!(id, 39);
        assert_eq!(
            codec.table().protocol_version(),
            Some(769)
        );

        assert!(matches!(
            router.select(768).err().unwrap(),
            ProtoError::UnsupportedVersion { .. }
        ));
    }

    #[test]
    fn router_reregister_replaces() {
        let mut router = VersionRouter::with_v767_default();
        router.register(Arc::new(codec_769()));
        router.register(Arc::new(codec_769())); // replaces, no dup entry
        assert_eq!(router.supported_versions(), vec![767, 769]);
    }

    // -- Dual-version 767 + 777 router -----------------------------------------

    fn codec_777() -> TableDrivenCodec {
        let mut c = TableDrivenCodec::for_version(
            ProtocolVersion::V26_3,
            ProtocolTable::v777_builtin(),
        );
        c.set_phase(PacketPhase::Play);
        c
    }

    #[test]
    fn router_dual_selects_767_and_777_and_rejects_others() {
        let router = VersionRouter::with_v767_and_v777();
        assert_eq!(router.supported_versions(), vec![767, 777]);

        let c767 = router.select(767).unwrap();
        assert_eq!(c767.capabilities().protocol_version, 767);
        assert!(!c767.capabilities().player_input_flags);
        assert_eq!(c767.table().protocol_version(), Some(767));

        let c777 = router.select(777).unwrap();
        assert_eq!(c777.capabilities().protocol_version, 777);
        assert_eq!(c777.capabilities().mc_version, "26.3");
        assert_eq!(c777.capabilities().data_pack_format_tuple, (121, 0));
        assert!(c777.capabilities().player_input_flags);
        assert_eq!(c777.table().protocol_version(), Some(777));

        for other in [765, 768, 769, 770, 776, 778] {
            assert!(matches!(
                router.select(other).err(),
                Some(ProtoError::UnsupportedVersion { .. })
            ));
        }
    }

    #[test]
    fn router_dual_routes_handshakes_to_distinct_codecs() {
        let router = VersionRouter::with_v767_and_v777();
        for (proto, mc) in [(767, "1.21.1"), (777, "26.3")] {
            // Same wire id 0x00 in both versions: the codec a handshake
            // decodes under is what the router selects afterwards.
            let codec = router.select(proto).unwrap();
            let mut frame = Vec::new();
            codec
                .encode_c2s(
                    &C2SPacket::Handshake {
                        protocol_version: proto,
                        server_address: "example.org".into(),
                        server_port: 25565,
                        next_state: 2,
                    },
                    &mut frame,
                )
                .unwrap();
            // Fresh codecs start in the handshake phase, so the trait's
            // `decode_c2s` resolves the frame there.
            let decoded = codec.decode_c2s(&frame).unwrap();
            let C2SPacket::Handshake {
                protocol_version, ..
            } = decoded
            else {
                panic!("expected handshake, got {decoded:?}")
            };
            assert_eq!(protocol_version, proto);
            // The routed codec is the one speaking the negotiated version.
            let routed = router.select(protocol_version).unwrap();
            assert_eq!(routed.capabilities().mc_version, mc);
        }
    }

    /// `(packet, 767 wire id, 777 wire id)` for the canonical S2C surface.
    #[test]
    fn router_dual_s2c_packets_roundtrip_with_distinct_ids() {
        let router = VersionRouter::with_v767_and_v777();
        let mut c767 = CodecV767::new();
        c767.set_phase(PacketPhase::Play);
        let c777 = codec_777();
        // Prove the selected codecs are the two different implementations.
        let selected767 = router.select(767).unwrap();
        let selected777 = router.select(777).unwrap();
        assert_eq!(selected767.capabilities().protocol_version, 767);
        assert_eq!(selected777.capabilities().protocol_version, 777);

        let damage_767 = ProtocolTable::v767_builtin()
            .registry_id("minecraft:data_component_type", "minecraft:damage")
            .unwrap();
        let damage_777 = ProtocolTable::v777_builtin()
            .registry_id("minecraft:data_component_type", "minecraft:damage")
            .unwrap();
        assert_ne!(damage_767, damage_777); // registry renumbered, names didn't

        let stack_for = |damage: u32| ItemStack {
            item_id: 1,
            count: 1,
            added: vec![RawComponent {
                type_id: damage,
                data: vec![0x2a], // VarInt 42
            }],
            removed: vec![],
        };

        let cases: [(S2CPacket, S2CPacket, u32, u32, PacketPhase); 11] = [
            // (767 form, 777 form, expected 767 id, expected 777 id, phase)
            (
                S2CPacket::KeepAlive { id: -42 },
                S2CPacket::KeepAlive { id: -42 },
                0x26,
                45,
                PacketPhase::Play,
            ),
            (
                S2CPacket::TickingState {
                    tick_rate: 20.0,
                    is_frozen: true,
                },
                S2CPacket::TickingState {
                    tick_rate: 20.0,
                    is_frozen: true,
                },
                0x71,
                130,
                PacketPhase::Play,
            ),
            (
                S2CPacket::TickingStep { steps: 7 },
                S2CPacket::TickingStep { steps: 7 },
                0x72,
                131,
                PacketPhase::Play,
            ),
            (
                S2CPacket::SetPlayerTeam {
                    name: "alpha".into(),
                    operation: TeamOperation::AddMembers {
                        members: vec!["steve".into()],
                    },
                },
                S2CPacket::SetPlayerTeam {
                    name: "alpha".into(),
                    operation: TeamOperation::AddMembers {
                        members: vec!["steve".into()],
                    },
                },
                0x60,
                111,
                PacketPhase::Play,
            ),
            (
                S2CPacket::SetEquipment {
                    entity_id: 99,
                    slots: vec![EquipmentSlot {
                        slot: 5,
                        item: stack_for(damage_767),
                    }],
                },
                S2CPacket::SetEquipment {
                    entity_id: 99,
                    slots: vec![EquipmentSlot {
                        slot: 5,
                        item: stack_for(damage_777),
                    }],
                },
                0x5b,
                104,
                PacketPhase::Play,
            ),
            (
                S2CPacket::SetPassengers {
                    vehicle: 8,
                    passengers: vec![1, 2],
                },
                S2CPacket::SetPassengers {
                    vehicle: 8,
                    passengers: vec![1, 2],
                },
                0x5f,
                109,
                PacketPhase::Play,
            ),
            (
                S2CPacket::OpenScreen {
                    container_id: 3,
                    menu: MenuKind::Anvil,
                    title: text_component("anvil"),
                },
                S2CPacket::OpenScreen {
                    container_id: 3,
                    menu: MenuKind::Anvil,
                    title: text_component("anvil"),
                },
                0x33,
                60,
                PacketPhase::Play,
            ),
            (
                S2CPacket::TabList {
                    header: text_component("h"),
                    footer: text_component("f"),
                },
                S2CPacket::TabList {
                    header: text_component("h"),
                    footer: text_component("f"),
                },
                0x6d,
                125,
                PacketPhase::Play,
            ),
            (
                S2CPacket::ConfigRegistryData {
                    registry_id: "minecraft:dimension_type".into(),
                    entries: vec![RegistryDataEntry {
                        entry_id: "minecraft:overworld".into(),
                        data: None,
                    }],
                },
                S2CPacket::ConfigRegistryData {
                    registry_id: "minecraft:dimension_type".into(),
                    entries: vec![RegistryDataEntry {
                        entry_id: "minecraft:overworld".into(),
                        data: None,
                    }],
                },
                0x07,
                0x07, // registry_data kept id 7 between 767 and 777
                PacketPhase::Configuration,
            ),
            (
                S2CPacket::ConfigKeepAlive { id: 4242 },
                S2CPacket::ConfigKeepAlive { id: 4242 },
                0x04,
                0x04,
                PacketPhase::Configuration,
            ),
            (
                S2CPacket::LoginSuccess {
                    uuid: [0x55; 16],
                    username: "steve".into(),
                    properties: vec![],
                },
                S2CPacket::LoginSuccess {
                    uuid: [0x55; 16],
                    username: "steve".into(),
                    properties: vec![],
                },
                0x02,
                0x02,
                PacketPhase::Login,
            ),
        ];

        for (p767, p777, id767, id777, phase) in cases {
            let mut f767 = Vec::new();
            let mut f777 = Vec::new();
            c767.encode_s2c(&p767, &mut f767).unwrap();
            c777.encode_s2c(&p777, &mut f777).unwrap();
            // The router-selected codecs emit identical frames — the Arc they
            // hand out is a live codec, not just a registry entry.
            let mut s767 = Vec::new();
            let mut s777 = Vec::new();
            selected767.encode_s2c(&p767, &mut s767).unwrap();
            selected777.encode_s2c(&p777, &mut s777).unwrap();
            assert_eq!(s767, f767);
            assert_eq!(s777, f777);
            let (w767, body767) = read_frame(&f767).unwrap();
            let (w777, body777) = read_frame(&f777).unwrap();
            assert_eq!((w767, w777), (id767 as i32, id777 as i32));
            if p767 == p777 {
                assert_eq!(body767, body777, "body drift on {p767:?}");
            }
            assert_eq!(c767.decode_s2c_in(phase, &f767).unwrap(), p767);
            assert_eq!(c777.decode_s2c_in(phase, &f777).unwrap(), p777);
        }
    }

    #[test]
    fn router_dual_chunk_block_disconnect_packets_roundtrip() {
        use crate::wire::{BitSet, BlockPos};

        let router = VersionRouter::with_v767_and_v777();
        let mut c767 = CodecV767::new();
        c767.set_phase(PacketPhase::Play);
        let c777 = codec_777();
        let selected767 = router.select(767).unwrap();
        let selected777 = router.select(777).unwrap();

        // (packet, wire id under 767, wire id under 777) — every id resolves
        // through the builtin tables, so 767 vs 777 renumbering shows up as
        // distinct frame bytes for the same IR.
        let cases: Vec<(S2CPacket, i32, i32, PacketPhase)> = vec![
            (
                S2CPacket::BlockUpdate {
                    pos: BlockPos::new(-30, 64, 12),
                    state_id: 0x1234,
                },
                0x09,
                0x08,
                PacketPhase::Play,
            ),
            (
                S2CPacket::ForgetLevelChunk { cx: -1, cz: 2 },
                0x21,
                0x26,
                PacketPhase::Play,
            ),
            (
                S2CPacket::PlayDisconnect {
                    reason: text_component("bye"),
                },
                0x1d,
                0x20,
                PacketPhase::Play,
            ),
            (
                S2CPacket::ConfigDisconnect {
                    reason: text_component("cfg"),
                },
                0x02,
                0x02,
                PacketPhase::Configuration,
            ),
        ];

        for (packet, id767, id777, phase) in cases {
            let mut f767 = Vec::new();
            let mut f777 = Vec::new();
            c767.encode_s2c(&packet, &mut f767).unwrap();
            c777.encode_s2c(&packet, &mut f777).unwrap();
            let mut s767 = Vec::new();
            let mut s777 = Vec::new();
            selected767.encode_s2c(&packet, &mut s767).unwrap();
            selected777.encode_s2c(&packet, &mut s777).unwrap();
            assert_eq!(s767, f767);
            assert_eq!(s777, f777);
            let (w767, body767) = read_frame(&f767).unwrap();
            let (w777, body777) = read_frame(&f777).unwrap();
            assert_eq!((w767, w777), (id767, id777));
            assert_eq!(body767, body777, "body drift on {packet:?}");
            assert_eq!(c767.decode_s2c_in(phase, &f767).unwrap(), packet);
            assert_eq!(c777.decode_s2c_in(phase, &f777).unwrap(), packet);
        }

        // forget_level_chunk packs `ChunkPos` as `z << 32 | (x as u32)` —
        // negative coordinates must roundtrip through the high/low halves.
        let mut f = Vec::new();
        c777.encode_s2c(
            &S2CPacket::ForgetLevelChunk { cx: -1, cz: 2 },
            &mut f,
        )
        .unwrap();
        let (_, body) = read_frame(&f).unwrap();
        assert_eq!(
            body,
            (0x0000_0002_ffff_ffffi64).to_be_bytes(),
            "ChunkPos packing"
        );

        // The same LightUpdateMask IR emits u64-word masks under 767 and
        // little-endian byte-array masks (trailing zero bytes dropped) under
        // 777; both decode back to identical masks.
        let light = S2CPacket::LightUpdateMask {
            chunk_x: -2,
            chunk_z: 3,
            sky_mask: BitSet::from_words(vec![0x20, 0x40]),
            block_mask: BitSet::from_u64(0x1),
            empty_sky_mask: BitSet::new(),
            empty_block_mask: BitSet::from_u64(0x2),
            sky_light: vec![vec![0xff; 2048], vec![0xee; 2048]],
            block_light: vec![vec![0x11; 2048]],
        };
        let mut f767 = Vec::new();
        let mut f777 = Vec::new();
        c767.encode_s2c(&light, &mut f767).unwrap();
        c777.encode_s2c(&light, &mut f777).unwrap();
        let (_, body767) = read_frame(&f767).unwrap();
        let (_, body777) = read_frame(&f777).unwrap();
        assert_ne!(body767, body777, "mask encoding must differ by version");
        // VarInt(-2) is 5 bytes + VarInt(3) 1 byte, so the sky mask starts at
        // byte 6 in both bodies.
        // 767 sky mask: VarInt(2 words) + BE words.
        assert_eq!(&body767[6..7], &[2]);
        assert_eq!(&body767[7..15], &0x20u64.to_be_bytes());
        assert_eq!(&body767[15..23], &0x40u64.to_be_bytes());
        // 777 sky mask: VarInt(9 bytes) + LE bytes of both words with the
        // trailing zeros dropped.
        assert_eq!(&body777[6..7], &[9]);
        assert_eq!(&body777[7..8], &[0x20]);
        assert_eq!(&body777[15..16], &[0x40]);
        assert_eq!(c767.decode_s2c_in(PacketPhase::Play, &f767).unwrap(), light);
        assert_eq!(c777.decode_s2c_in(PacketPhase::Play, &f777).unwrap(), light);
    }

    #[test]
    fn router_dual_player_input_wire_shapes_differ() {
        let router = VersionRouter::with_v767_and_v777();
        let mut c767 = CodecV767::new();
        c767.set_phase(PacketPhase::Play);
        let c777 = codec_777();
        assert_eq!(router.select(767).unwrap().capabilities().protocol_version, 767);
        assert_eq!(router.select(777).unwrap().capabilities().protocol_version, 777);

        // 767: id 0x26, `f32 sideways, f32 forward, u8 flags`.
        let mut f767 = Vec::new();
        c767
            .encode_c2s(
                &C2SPacket::PlayerInput {
                    sideways: -0.5,
                    forward: 0.98,
                    jump: true,
                    unmount: false,
                },
                &mut f767,
            )
            .unwrap();
        let (id767, body767) = read_frame(&f767).unwrap();
        assert_eq!(id767, 0x26);
        assert_eq!(body767.len(), 9);
        assert_eq!(
            c767.decode_c2s_in(PacketPhase::Play, &f767).unwrap(),
            C2SPacket::PlayerInput {
                sideways: -0.5,
                forward: 0.98,
                jump: true,
                unmount: false,
            }
        );

        // 777: id 43, one `Input` flag byte — decoded as PlayerInputFlags.
        let flags = C2SPacket::PlayerInputFlags {
            forward: true,
            backward: false,
            left: false,
            right: true,
            jump: true,
            shift: false,
            sprint: true,
        };
        let mut f777 = Vec::new();
        c777.encode_c2s(&flags, &mut f777).unwrap();
        let (id777, body777) = read_frame(&f777).unwrap();
        assert_eq!(id777, 43);
        assert_eq!(body777, &[0x01 | 0x08 | 0x10 | 0x40]);
        assert_eq!(
            c777.decode_c2s_in(PacketPhase::Play, &f777).unwrap(),
            flags
        );

        // The float IR under 777 quantizes to the same flag form.
        let mut f = Vec::new();
        c777
            .encode_c2s(
                &C2SPacket::PlayerInput {
                    sideways: -1.0,
                    forward: 1.0,
                    jump: false,
                    unmount: true,
                },
                &mut f,
            )
            .unwrap();
        assert_eq!(
            c777.decode_c2s_in(PacketPhase::Play, &f).unwrap(),
            C2SPacket::PlayerInputFlags {
                forward: true,
                backward: false,
                left: false,
                right: true,
                jump: false,
                shift: true,
                sprint: false,
            }
        );
    }
}

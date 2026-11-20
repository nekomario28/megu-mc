//! The wire format of the command packets, protocol 767 (Minecraft 1.21.1).
//!
//! `crate::brigadier` models the tree and the dispatcher; this module serializes what that tree
//! already holds, and parses/answers what a client asks for. It is wire only: nothing in
//! `main.rs` sends these packets yet, `run_command_ctx` is untouched, and no 1.21.1-specific
//! argument type (entity selector, `block_pos`, `resource_location`, NBT, components) is
//! implemented - those are slice 3.
//!
//! # Provenance of every constant below
//!
//! There is no packet-id or parser-id table in this repository, and guessing one would put a
//! wrong byte on the wire that no offline test could catch. Every number here was therefore read
//! out of the pinned 1.21.1 server jar and, where it is a registry id, printed by a probe that
//! bootstrapped that jar's own registries. The reading aids are recorded outside the tree
//! (`~/mc-spine-local/audit/brigadier-slice2-*/evidence/`), as `tools/vanilla_src.sh` requires.
//!
//! Inputs (SHA-1 checked against the pins in `tools/vanilla_src.sh`):
//!
//! - `server.jar` `59353fb40c36d304f2035d51e7d6e6baa98dc05c`
//! - official mappings `server.txt` `03f8985492bda0afc0898465341eb0acef35f570`
//! - inner `META-INF/versions/1.21.1/server-1.21.1.jar` `a8ae1f986e20cc91442b6b2b64d62bcd065618d7`
//! - bundled `META-INF/libraries/com/mojang/brigadier/1.3.10/brigadier-1.3.10.jar`
//!   `d15b53a14cf20fdcaa98f731af5dda654452c010`; `version.json` says `"protocol_version": 767`.
//!
//! ## Packet ids
//!
//! Read from `net.minecraft.network.protocol.game.GameProtocols` (obfuscated `agh`) with
//! `javap -p -c`: both play protocols are built by registering packet types in order, so a
//! type's id is the index of its registration. The three command packets name their own type
//! singletons in `getType()`: `acn.a()` returns `agg.s`, `acm.a()` returns `agg.r`, `agu.a()`
//! returns `agg.bp`. Counting the clientbound registrations in `agh.a(zl)` and the serverbound
//! ones in `agh.b(zl)` gives:
//!
//! | packet | class | registration index |
//! |---|---|---|
//! | clientbound command suggestions response | `acm` | 16 = `0x10` |
//! | clientbound commands | `acn` | 17 = `0x11` |
//! | serverbound command suggestion | `agu` | 11 = `0x0b` |
//!
//! The clientbound ID immediately before these is `minecraft:clear_titles` at `0x0f`.
//!
//! ## Parser ids
//!
//! The Commands packet writes `BuiltInRegistries.COMMAND_ARGUMENT_TYPE.getId(info)` (obfuscated
//! `lt.u.a(...)`, a VarInt) for every argument node. The registry is populated by
//! `ArgumentTypeInfos` (obfuscated `ip`) in its bootstrap order, and ids are the registration
//! order from zero. Confirmed at runtime by bootstrapping the jar (`SharedConstants` +
//! `Bootstrap`, then `getId` per type):
//!
//! | vanilla id | type | our [`ArgumentTypeInfo`] |
//! |---|---|---|
//! | 0 | `brigadier:bool` | `Bool` |
//! | 1 | `brigadier:float` | `Float` |
//! | 2 | `brigadier:double` | `Double` |
//! | 3 | `brigadier:integer` | `Integer` |
//! | 4 | `brigadier:long` | `Long` |
//! | 5 | `brigadier:string` | `String` |
//!
//! Anything from 6 (`minecraft:entity`) upward belongs to argument types this slice does not
//! own, so a node holding one is refused ([`WireError::UnsupportedArgumentType`]) rather than
//! serialized under a guessed id.
//!
//! ## Node layout
//!
//! `ClientboundCommandsPacket.NodeBuilder.write` (obfuscated `acn$b.a(vw)`) writes, in order:
//! the flags byte, the children as a VarInt array, the redirect index when flag `0x08` is set,
//! and then the node's own data (`acn$a.a(vw)` for an argument, `acn$c.a(vw)` for a literal).
//! The flags are set in `acn.a(CommandNode, Object2IntMap)`: `0x01` literal, `0x02` argument,
//! `0x04` has a command, `0x08` has a redirect, `0x10` has custom suggestions. Note that the
//! literal/argument bits are *not* the other way round: an argument node carries `0x02`.
//!
//! The packet body is the node count, the nodes, then the root's index; node indices are
//! assigned breadth-first from the root, enqueuing each node's children in their own order and
//! then its redirect target, skipping nodes already seen (same method).
//!
//! ## Parser data
//!
//! - numeric types: one flags byte, bit `0x01` "has minimum", bit `0x02` "has maximum", followed
//!   by the present bound as a big-endian `int`/`long`/`float`/`double`. A bound is absent when
//!   it is the full range (`Integer.MIN_VALUE`/`Integer.MAX_VALUE` for ints and longs,
//!   `-Float.MAX_VALUE`/`Float.MAX_VALUE` and the same for `double`) - the constants `iu` compares
//!   against are `-3.4028235E38f`/`3.4028235E38f`. `IntegerArgumentType.integer()` therefore
//!   writes a flags byte of `0x00` and no values at all.
//! - `brigadier:string`: the string type as a VarInt: `0` single word, `1` quotable phrase,
//!   `2` greedy phrase.
//! - a named custom suggestion provider: a resource location (UTF string) written after the
//!   parser data.
//!
//! ## Suggestions packets
//!
//! `ServerboundCommandSuggestionPacket.write` (obfuscated `agu.a(vw)`) is a VarInt transaction id
//! and a UTF string capped at 32500 characters - there is no cursor field in 767.
//! `ClientboundCommandSuggestionsPacket` is a record `(id, start, length, entries)` whose
//! `(id, Suggestions)` constructor takes `start = range.getStart()` and
//! `length = range.getLength()`, so the mapping from Brigadier's `StringRange` is exact. Each
//! entry is the suggestion text plus an optional component tooltip; only the absent tooltip is
//! implemented here (see [`SuggestionTooltip`]).

#![allow(dead_code)] // no consumer yet: a later slice sends these packets.

use crate::brigadier::{ArgumentTypeInfo, CommandNode, StringType, Suggestions};
use std::collections::HashMap;
use std::fmt;

/// `minecraft:commands`, clientbound play, protocol 767. Provenance: module docs.
pub const CLIENTBOUND_COMMANDS: i32 = 0x11;

/// `minecraft:command_suggestions`, clientbound play, protocol 767. Provenance: module docs.
pub const CLIENTBOUND_COMMAND_SUGGESTIONS: i32 = 0x10;

/// `minecraft:command_suggestion`, serverbound play, protocol 767. Provenance: module docs.
pub const SERVERBOUND_COMMAND_SUGGESTION: i32 = 0x0b;

/// Node flag: a literal node. Provenance: module docs.
pub const NODE_LITERAL: u8 = 0x01;
/// Node flag: an argument node. Provenance: module docs.
pub const NODE_ARGUMENT: u8 = 0x02;
/// Node flag: the node has a command to run.
pub const NODE_EXECUTABLE: u8 = 0x04;
/// Node flag: the node redirects, and a redirect index follows the children.
pub const NODE_REDIRECT: u8 = 0x08;
/// Node flag: an argument node has a custom suggestion provider, written after its parser data.
pub const NODE_SUGGESTIONS: u8 = 0x10;

/// Parser id of `brigadier:bool`. Provenance: module docs.
pub const PARSER_BOOL: i32 = 0;
/// Parser id of `brigadier:float`.
pub const PARSER_FLOAT: i32 = 1;
/// Parser id of `brigadier:double`.
pub const PARSER_DOUBLE: i32 = 2;
/// Parser id of `brigadier:integer`.
pub const PARSER_INTEGER: i32 = 3;
/// Parser id of `brigadier:long`.
pub const PARSER_LONG: i32 = 4;
/// Parser id of `brigadier:string`.
pub const PARSER_STRING: i32 = 5;

/// `FriendlyByteBuf.writeUtf`'s default cap, in characters: node names and resource locations.
const MAX_UTF: usize = 32767;
/// `ServerboundCommandSuggestionPacket`'s own cap on the typed text.
const MAX_REQUEST_TEXT: usize = 32500;

/// A malformed or unserializable value. Every variant is a refusal: nothing here guesses a byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// The tree carries a custom suggestion provider and no name for it. Brigadier keys a
    /// provider by the function; the packet needs the registry name vanilla would have.
    UnnamedSuggestionProvider { node: String },
    /// A node holds an argument type this slice does not serialize (a 1.21.1-only selection
    /// parser, or a third-party type). Refused rather than written under a guessed parser id.
    UnsupportedArgumentType { node: String },
    /// A node the walk reached but never indexed; a tree cannot produce this, and a serializer
    /// that silently omitted the child would produce a different tree on the client.
    UnindexedNode { node: String },
    /// A suggestion carries a tooltip. 1.21.1 writes it as a network NBT component, which this
    /// slice does not implement; the entry is modelled so the refusal is explicit.
    TooltipNotImplemented { text: String },
    /// A string field is longer than the field's own cap.
    TextTooLong {
        field: &'static str,
        length: usize,
        maximum: usize,
    },
    /// A number does not fit the VarInt the field is written as.
    NumberTooLarge { field: &'static str, value: u64 },
    /// A resource location is not `namespace:path` with vanilla's character set.
    InvalidResourceLocation { text: String },
    /// The buffer ended inside a field.
    Truncated { needed: usize, remaining: usize },
    /// A VarInt ran past five bytes.
    VarIntTooLong,
    /// A UTF field's bytes are not UTF-8.
    InvalidUtf8,
    /// Bytes remained after the last field of a packet body.
    TrailingBytes { remaining: usize },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnnamedSuggestionProvider { node } => write!(
                f,
                "node '{node}' has a custom suggestion provider with no registry name"
            ),
            Self::UnsupportedArgumentType { node } => write!(
                f,
                "node '{node}' holds an argument type this slice does not serialize"
            ),
            Self::UnindexedNode { node } => write!(f, "node '{node}' was reached but never indexed"),
            Self::TooltipNotImplemented { text } => write!(
                f,
                "suggestion '{text}' carries a tooltip, which is not implemented"
            ),
            Self::TextTooLong {
                field,
                length,
                maximum,
            } => write!(f, "{field} is {length} characters, more than {maximum}"),
            Self::NumberTooLarge { field, value } => {
                write!(f, "{field} value {value} does not fit a VarInt")
            }
            Self::InvalidResourceLocation { text } => {
                write!(f, "'{text}' is not a resource location")
            }
            Self::Truncated { needed, remaining } => {
                write!(f, "needed {needed} bytes, {remaining} left")
            }
            Self::VarIntTooLong => write!(f, "variable integer longer than five bytes"),
            Self::InvalidUtf8 => write!(f, "UTF field is not valid UTF-8"),
            Self::TrailingBytes { remaining } => {
                write!(f, "{remaining} bytes left after the last field")
            }
        }
    }
}

impl std::error::Error for WireError {}

// ---------------------------------------------------------------------------------------
// Packet primitives
// ---------------------------------------------------------------------------------------

/// Protocol VarInt: unsigned LEB128 of the 32-bit two's complement form, so a negative number
/// is always five bytes. This mirrors `main.rs::write_varint`, which this module must not reach
/// into: the module is offline-testable and its own bytes are the contract.
fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut rest = value as u32;
    loop {
        let byte = (rest & 0x7f) as u8;
        rest >>= 7;
        if rest == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// `FriendlyByteBuf.writeUtf`: the byte length as a VarInt, then the bytes, capped in characters.
fn write_utf(
    out: &mut Vec<u8>,
    text: &str,
    maximum: usize,
    field: &'static str,
) -> Result<(), WireError> {
    let length = text.chars().count();
    if length > maximum {
        return Err(WireError::TextTooLong {
            field,
            length,
            maximum,
        });
    }
    let bytes = text.as_bytes();
    let len = i32::try_from(bytes.len()).map_err(|_| WireError::NumberTooLarge {
        field,
        value: bytes.len() as u64,
    })?;
    write_varint(out, len);
    out.extend_from_slice(bytes);
    Ok(())
}

fn frame(packet_id: i32, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + 1);
    write_varint(&mut body, packet_id);
    body.extend_from_slice(payload);
    let mut out = Vec::with_capacity(body.len() + 1);
    write_varint(&mut out, body.len() as i32);
    out.extend_from_slice(&body);
    out
}

struct WireReader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> WireReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.cursor
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], WireError> {
        if count > self.remaining() {
            return Err(WireError::Truncated {
                needed: count,
                remaining: self.remaining(),
            });
        }
        let part = &self.bytes[self.cursor..self.cursor + count];
        self.cursor += count;
        Ok(part)
    }

    /// Protocol VarInt: at most five bytes, the last of which carries no more than four bits.
    fn read_varint(&mut self) -> Result<i32, WireError> {
        let mut value = 0u32;
        for index in 0..5 {
            let byte = self.take(1)?[0];
            if index == 4 && byte > 0x0f {
                return Err(WireError::VarIntTooLong);
            }
            value |= ((byte & 0x7f) as u32) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(value as i32);
            }
        }
        Err(WireError::VarIntTooLong)
    }

    fn read_utf(&mut self, maximum: usize, field: &'static str) -> Result<String, WireError> {
        let length = self.read_varint()?;
        if length < 0 {
            // A length above `i32::MAX` cannot be a byte count; it is a malformed field, not a
            // truncated one, and reporting it as `Truncated` would point the reader at the buffer.
            return Err(WireError::NumberTooLarge {
                field,
                value: length as u32 as u64,
            });
        }
        let bytes = self.take(length as usize)?;
        let text = std::str::from_utf8(bytes).map_err(|_| WireError::InvalidUtf8)?;
        let characters = text.chars().count();
        if characters > maximum {
            return Err(WireError::TextTooLong {
                field,
                length: characters,
                maximum,
            });
        }
        Ok(text.to_string())
    }
}

// ---------------------------------------------------------------------------------------
// Resource locations and suggestion provider names
// ---------------------------------------------------------------------------------------

/// `net.minecraft.resources.ResourceLocation`, as far as the wire needs it: a namespace and a
/// path, both restricted to vanilla's character sets.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceLocation {
    namespace: String,
    path: String,
}

impl ResourceLocation {
    /// `ResourceLocation.tryParse`: one optional `namespace:` prefix and a path.
    pub fn parse(text: &str) -> Result<Self, WireError> {
        let mut parts = text.split(':');
        // `split` always yields at least one piece, so this cannot be empty-ended.
        let first = parts.next().unwrap_or_default();
        match (parts.next(), parts.next()) {
            (None, _) => Self::checked("minecraft", first, text),
            (Some(path), None) => Self::checked(first, path, text),
            _ => Err(WireError::InvalidResourceLocation {
                text: text.to_string(),
            }),
        }
    }

    pub fn new(namespace: &str, path: &str) -> Result<Self, WireError> {
        Self::checked(namespace, path, &format!("{namespace}:{path}"))
    }

    /// Vanilla's `validNamespaceChar`/`validPathChar` rule, with the caller's own spelling of the
    /// location kept for the error so a message names what was actually given.
    fn checked(namespace: &str, path: &str, text: &str) -> Result<Self, WireError> {
        let valid = !namespace.is_empty()
            && !path.is_empty()
            && namespace.chars().all(valid_namespace_char)
            && path.chars().all(valid_path_char);
        if !valid {
            return Err(WireError::InvalidResourceLocation {
                text: text.to_string(),
            });
        }
        Ok(Self {
            namespace: namespace.to_string(),
            path: path.to_string(),
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for ResourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.namespace, self.path)
    }
}

fn valid_namespace_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.')
}

fn valid_path_char(c: char) -> bool {
    valid_namespace_char(c) || c == '/'
}

/// A suggestion provider's registry name - what the `0x10` flag makes the packet carry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SuggestionProviderId(ResourceLocation);

impl SuggestionProviderId {
    pub fn new(location: ResourceLocation) -> Self {
        Self(location)
    }

    pub fn parse(text: &str) -> Result<Self, WireError> {
        Ok(Self(ResourceLocation::parse(text)?))
    }

    pub fn location(&self) -> &ResourceLocation {
        &self.0
    }
}

impl fmt::Display for SuggestionProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Registry names for the suggestion providers a tree carries, keyed by node identity.
///
/// Vanilla names providers in `SuggestionProviders` and can look a name up from the provider
/// function. The Rust port has no such registry - a provider is an `Arc<dyn Fn>` - so the owner
/// of the tree supplies the names here. A node that has a provider and no name is refused rather
/// than written with a borrowed or invented location.
#[derive(Clone, Debug, Default)]
pub struct SuggestionProviderNames {
    by_node: HashMap<usize, SuggestionProviderId>,
}

impl SuggestionProviderNames {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<S>(&mut self, node: &CommandNode<S>, id: SuggestionProviderId) {
        self.by_node.insert(node.identity(), id);
    }

    pub fn with<S>(mut self, node: &CommandNode<S>, id: SuggestionProviderId) -> Self {
        self.insert(node, id);
        self
    }

    pub fn get<S>(&self, node: &CommandNode<S>) -> Option<&SuggestionProviderId> {
        self.by_node.get(&node.identity())
    }
}

// ---------------------------------------------------------------------------------------
// Commands (clientbound)
// ---------------------------------------------------------------------------------------

/// The Commands packet body for an unnamed-provider tree, i.e. one without custom providers.
pub fn commands_payload<S>(root: &CommandNode<S>) -> Result<Vec<u8>, WireError> {
    commands_payload_named(root, &SuggestionProviderNames::new())
}

/// The Commands packet body: node count, nodes, root index.
pub fn commands_payload_named<S>(
    root: &CommandNode<S>,
    names: &SuggestionProviderNames,
) -> Result<Vec<u8>, WireError> {
    let (order, index) = index_nodes(root);
    let mut out = Vec::new();
    write_varint(&mut out, wire_varint(order.len(), "node count")?);
    for node in &order {
        write_node(&mut out, node, &index, names)?;
    }
    // The root is dequeued first, so its index is always 0 - written from the map anyway, so a
    // different assignment cannot silently disagree with the indices above.
    let root_index = *index
        .get(&root.identity())
        .ok_or_else(|| WireError::UnindexedNode {
            node: root.name().to_string(),
        })?;
    write_varint(&mut out, wire_varint(root_index, "root index")?);
    Ok(out)
}

/// The whole clientbound packet: length-prefixed, id `0x10`, then the body.
pub fn commands_packet<S>(root: &CommandNode<S>) -> Result<Vec<u8>, WireError> {
    Ok(frame(CLIENTBOUND_COMMANDS, &commands_payload(root)?))
}

pub fn commands_packet_named<S>(
    root: &CommandNode<S>,
    names: &SuggestionProviderNames,
) -> Result<Vec<u8>, WireError> {
    Ok(frame(
        CLIENTBOUND_COMMANDS,
        &commands_payload_named(root, names)?,
    ))
}

/// A count or index as the VarInt the packet writes it as. `field` names the field so an
/// out-of-range value says which one it was, instead of blaming the node array for a suggestion
/// range.
fn wire_varint(value: usize, field: &'static str) -> Result<i32, WireError> {
    i32::try_from(value).map_err(|_| WireError::NumberTooLarge {
        field,
        value: value as u64,
    })
}

/// Breadth-first node indexing, exactly as `ClientboundCommandsPacket` assigns it: the root
/// first, then for each node its children in their own order and then its redirect target,
/// skipping a node already seen. A node reachable twice (a redirect back to an ancestor, as
/// `execute` uses) keeps its first index, which is what makes the redirect index meaningful.
fn index_nodes<S>(root: &CommandNode<S>) -> (Vec<CommandNode<S>>, HashMap<usize, usize>) {
    let mut order: Vec<CommandNode<S>> = Vec::new();
    let mut index: HashMap<usize, usize> = HashMap::new();
    let mut queue: std::collections::VecDeque<CommandNode<S>> = std::collections::VecDeque::new();
    queue.push_back(root.clone());
    while let Some(node) = queue.pop_front() {
        let identity = node.identity();
        if index.contains_key(&identity) {
            continue;
        }
        index.insert(identity, order.len());
        order.push(node.clone());
        for child in node.children() {
            queue.push_back(child);
        }
        if let Some(target) = node.redirect() {
            queue.push_back(target);
        }
    }
    (order, index)
}

fn write_node<S>(
    out: &mut Vec<u8>,
    node: &CommandNode<S>,
    index: &HashMap<usize, usize>,
    names: &SuggestionProviderNames,
) -> Result<(), WireError> {
    let redirect = node.redirect();
    let provider = if node.is_argument() {
        node.custom_suggestions()
    } else {
        None
    };
    let mut flags = 0u8;
    if node.is_literal() {
        flags |= NODE_LITERAL;
    }
    if node.is_argument() {
        flags |= NODE_ARGUMENT;
    }
    if node.command().is_some() {
        flags |= NODE_EXECUTABLE;
    }
    if redirect.is_some() {
        flags |= NODE_REDIRECT;
    }
    if provider.is_some() {
        flags |= NODE_SUGGESTIONS;
    }
    out.push(flags);

    let children = node.children();
    write_varint(out, wire_varint(children.len(), "child count")?);
    for child in &children {
        let child_index = *index
            .get(&child.identity())
            .ok_or_else(|| WireError::UnindexedNode {
                node: child.name().to_string(),
            })?;
        write_varint(out, wire_varint(child_index, "child index")?);
    }

    if let Some(target) = &redirect {
        let target_index = *index
            .get(&target.identity())
            .ok_or_else(|| WireError::UnindexedNode {
                node: target.name().to_string(),
            })?;
        write_varint(out, wire_varint(target_index, "redirect index")?);
    }

    if node.is_literal() {
        write_utf(out, node.name(), MAX_UTF, "literal")?;
    }
    if node.is_argument() {
        write_utf(out, node.name(), MAX_UTF, "argument name")?;
        let info = node
            .argument_type()
            .and_then(|argument_type| argument_type.type_info())
            .ok_or_else(|| WireError::UnsupportedArgumentType {
                node: node.name().to_string(),
            })?;
        write_parser(out, info);
        if provider.is_some() {
            let id = names
                .get(node)
                .ok_or_else(|| WireError::UnnamedSuggestionProvider {
                    node: node.name().to_string(),
                })?;
            write_utf(out, &id.to_string(), MAX_UTF, "suggestion provider")?;
        }
    }
    Ok(())
}

/// The parser id and parser data of one argument node. The `match` is exhaustive on purpose: a
/// new [`ArgumentTypeInfo`] variant stops this module compiling, so a type cannot reach the wire
/// without its serializer.
fn write_parser(out: &mut Vec<u8>, info: ArgumentTypeInfo) {
    match info {
        ArgumentTypeInfo::Bool => write_varint(out, PARSER_BOOL),
        ArgumentTypeInfo::Float { minimum, maximum } => write_numeric_range(
            out,
            PARSER_FLOAT,
            (minimum, maximum),
            (-f32::MAX, f32::MAX),
            |value| value.to_be_bytes().to_vec(),
        ),
        ArgumentTypeInfo::Double { minimum, maximum } => write_numeric_range(
            out,
            PARSER_DOUBLE,
            (minimum, maximum),
            (-f64::MAX, f64::MAX),
            |value| value.to_be_bytes().to_vec(),
        ),
        ArgumentTypeInfo::Integer { minimum, maximum } => write_numeric_range(
            out,
            PARSER_INTEGER,
            (minimum, maximum),
            (i32::MIN, i32::MAX),
            |value| value.to_be_bytes().to_vec(),
        ),
        ArgumentTypeInfo::Long { minimum, maximum } => write_numeric_range(
            out,
            PARSER_LONG,
            (minimum, maximum),
            (i64::MIN, i64::MAX),
            |value| value.to_be_bytes().to_vec(),
        ),
        ArgumentTypeInfo::String { string_type } => {
            write_varint(out, PARSER_STRING);
            write_varint(
                out,
                match string_type {
                    StringType::SingleWord => 0,
                    StringType::QuotablePhrase => 1,
                    StringType::GreedyPhrase => 2,
                },
            );
        }
    }
}

/// The range form shared by `integer`, `long`, `float` and `double`: one flags byte, bit `0x01`
/// "has a minimum" and bit `0x02` "has a maximum", then each present bound big-endian. A bound
/// equal to the type's own full range is *absent*, so `IntegerArgumentType.integer()` writes only
/// the zero flags byte and no values at all.
fn write_numeric_range<T: PartialEq>(
    out: &mut Vec<u8>,
    parser_id: i32,
    (minimum, maximum): (T, T),
    (absent_minimum, absent_maximum): (T, T),
    to_bytes: fn(T) -> Vec<u8>,
) {
    write_varint(out, parser_id);
    let mut flags = 0u8;
    if minimum != absent_minimum {
        flags |= 0x01;
    }
    if maximum != absent_maximum {
        flags |= 0x02;
    }
    out.push(flags);
    if flags & 0x01 != 0 {
        out.extend_from_slice(&to_bytes(minimum));
    }
    if flags & 0x02 != 0 {
        out.extend_from_slice(&to_bytes(maximum));
    }
}

// ---------------------------------------------------------------------------------------
// Command suggestions (serverbound request, clientbound response)
// ---------------------------------------------------------------------------------------

/// `ServerboundCommandSuggestionPacket`: the transaction id the client will answer, and the text
/// typed so far. Protocol 767 carries no cursor field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSuggestionsRequest {
    pub transaction_id: i32,
    pub text: String,
}

impl CommandSuggestionsRequest {
    pub fn new(transaction_id: i32, text: impl Into<String>) -> Self {
        Self {
            transaction_id,
            text: text.into(),
        }
    }
}

impl Default for CommandSuggestionsRequest {
    fn default() -> Self {
        Self::new(0, "")
    }
}

pub fn command_suggestions_request_payload(
    request: &CommandSuggestionsRequest,
) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::new();
    write_varint(&mut out, request.transaction_id);
    write_utf(&mut out, &request.text, MAX_REQUEST_TEXT, "request text")?;
    Ok(out)
}

pub fn command_suggestions_request_packet(
    request: &CommandSuggestionsRequest,
) -> Result<Vec<u8>, WireError> {
    Ok(frame(
        SERVERBOUND_COMMAND_SUGGESTION,
        &command_suggestions_request_payload(request)?,
    ))
}

/// Parse a whole `ServerboundCommandSuggestionPacket` body (no packet id, no length prefix).
pub fn parse_command_suggestions_request(
    payload: &[u8],
) -> Result<CommandSuggestionsRequest, WireError> {
    let mut reader = WireReader::new(payload);
    let transaction_id = reader.read_varint()?;
    let text = reader.read_utf(MAX_REQUEST_TEXT, "request text")?;
    if reader.remaining() != 0 {
        return Err(WireError::TrailingBytes {
            remaining: reader.remaining(),
        });
    }
    Ok(CommandSuggestionsRequest {
        transaction_id,
        text,
    })
}

/// The tooltip of one suggestion.
///
/// `ClientboundCommandSuggestionsPacket`'s entry is `(String, Optional<Component>)`. Only the
/// absent tooltip is implemented: a present one is a network NBT component, which this slice does
/// not own. The variant exists so that a caller holding a tooltip is refused by name rather than
/// silently serialized without it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuggestionTooltip {
    /// A chat component, written as network NBT in 1.21.1 - not implemented in this slice.
    Component(ComponentNbt),
}

/// Placeholder for the network NBT a component tooltip is written as. Deliberately has no
/// serializer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentNbt;

/// One suggestion in a response: the text plus an optional tooltip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuggestionEntry {
    pub text: String,
    pub tooltip: Option<SuggestionTooltip>,
}

impl SuggestionEntry {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tooltip: None,
        }
    }
}

/// `ClientboundCommandSuggestionsPacket`: the transaction id being answered, the replaced range,
/// and the suggestions in the order the dispatcher produced them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSuggestionsResponse {
    pub transaction_id: i32,
    pub start: usize,
    pub length: usize,
    pub entries: Vec<SuggestionEntry>,
}

impl CommandSuggestionsResponse {
    /// Build the response for an answer from
    /// [`CommandDispatcher::get_completion_suggestions`](crate::brigadier::CommandDispatcher::get_completion_suggestions).
    /// `start` is the range's start and `length` its length - the same two numbers the packet's
    /// own `(id, Suggestions)` constructor takes.
    pub fn from_suggestions(transaction_id: i32, suggestions: &Suggestions) -> Self {
        let range = suggestions.range();
        Self {
            transaction_id,
            start: range.start(),
            length: range.len(),
            entries: suggestions
                .list()
                .iter()
                .map(|suggestion| SuggestionEntry {
                    text: suggestion.text().to_string(),
                    tooltip: suggestion.tooltip().map(|_| {
                        // The baseline stores a tooltip's text; the wire form is a component.
                        SuggestionTooltip::Component(ComponentNbt)
                    }),
                })
                .collect(),
        }
    }
}

pub fn command_suggestions_response_payload(
    response: &CommandSuggestionsResponse,
) -> Result<Vec<u8>, WireError> {
    for entry in &response.entries {
        if entry.tooltip.is_some() {
            return Err(WireError::TooltipNotImplemented {
                text: entry.text.clone(),
            });
        }
    }
    let mut out = Vec::new();
    write_varint(&mut out, response.transaction_id);
    write_varint(&mut out, wire_varint(response.start, "suggestion start")?);
    write_varint(&mut out, wire_varint(response.length, "suggestion length")?);
    write_varint(&mut out, wire_varint(response.entries.len(), "suggestion count")?);
    for entry in &response.entries {
        write_utf(&mut out, &entry.text, MAX_UTF, "suggestion text")?;
        // `Optional<Component>`: one absent-tooltip flag byte in the form implemented here.
        out.push(0x00);
    }
    Ok(out)
}

pub fn command_suggestions_response_packet(
    response: &CommandSuggestionsResponse,
) -> Result<Vec<u8>, WireError> {
    Ok(frame(
        CLIENTBOUND_COMMAND_SUGGESTIONS,
        &command_suggestions_response_payload(response)?,
    ))
}

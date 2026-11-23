//! Message values shared by the landing dispatcher and its Java backend.
//! This module owns no world state, threads, callbacks, or backend selection.

#[derive(Clone, Copy, Debug)]
pub enum Loader {
    Fabric,
    NeoForge,
}

#[derive(Clone, Copy, Debug)]
pub struct Trample {
    /// Dimension slot (`DimId::index() as u8`: 0 = Overworld, 1 = Nether, 2 = End).
    pub dim: u8,
    pub position: (i32, i32, i32),
    pub state: u32,
    // read by the Java adapter (java_compat.rs passes it as a JNI float), so it
    // is only dead in builds without that feature
    #[cfg_attr(not(feature = "java-compat"), allow(dead_code))]
    pub distance: f32,
}

#[cfg(feature = "java-compat")]
pub struct Request {
    pub trample: Trample,
    pub reply: std::sync::mpsc::SyncSender<Result<bool, String>>,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MutationOp {
    SetBlockIf {
        dim: u8,
        pos: (i32, i32, i32),
        new_state: u32,
        expected_state: Option<u32>,
    },
    SendSystemMessage {
        player_eid: i32,
        message: String,
    },
    RegisterCommand {
        literal: String,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct S1EventOutcome {
    pub(crate) canceled: bool,
    pub(crate) break_exp: Option<i32>,
    pub(crate) chat_message: Option<String>,
    pub(crate) mutations: Vec<MutationOp>,
}

/// Decode the `S1` batch emitted by NeoForge's `MutationBuffer`. Each record
/// uses tab-separated fields, and the final `end` line is required.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decode_mutation_batch(wire: &str) -> Result<S1EventOutcome, String> {
    let mut lines = wire.split('\n');
    let canceled = match lines.next() {
        Some("S1\t0") => false,
        Some("S1\t1") => true,
        _ => return Err("invalid S1 batch header".into()),
    };
    let mut outcome = S1EventOutcome {
        canceled,
        break_exp: None,
        chat_message: None,
        mutations: Vec::new(),
    };

    while let Some(line) = lines.next() {
        if line == "end" {
            return if lines.next().is_none() {
                Ok(outcome)
            } else {
                Err("trailing records after S1 batch terminator".into())
            };
        }
        if canceled {
            return Err("canceled S1 batch must be empty".into());
        }
        let fields: Vec<_> = line.split('\t').collect();
        match fields.as_slice() {
            ["block", dim, x, y, z, state, expected] => {
                let dim = parse_integer(dim)?;
                if dim > 2 {
                    return Err(format!("invalid S1 dimension slot {dim}"));
                }
                outcome.mutations.push(MutationOp::SetBlockIf {
                    dim,
                    pos: (parse_integer(x)?, parse_integer(y)?, parse_integer(z)?),
                    new_state: parse_integer(state)?,
                    expected_state: match *expected {
                        "-" => None,
                        _ => Some(parse_integer(expected)?),
                    },
                });
            }
            ["message", eid, message] => outcome.mutations.push(MutationOp::SendSystemMessage {
                player_eid: parse_integer(eid)?,
                message: unescape(message)?,
            }),
            ["command", literal] => outcome.mutations.push(MutationOp::RegisterCommand {
                literal: unescape(literal)?,
            }),
            ["exp", exp] if outcome.break_exp.is_none() => {
                outcome.break_exp = Some(parse_integer(exp)?);
            }
            ["chat", message] if outcome.chat_message.is_none() => {
                outcome.chat_message = Some(unescape(message)?);
            }
            _ => return Err("malformed S1 batch record".into()),
        }
    }
    Err("missing S1 batch terminator".into())
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_integer<T: std::str::FromStr>(field: &str) -> Result<T, String> {
    field
        .parse()
        .map_err(|_| format!("invalid S1 integer {field:?}"))
}

#[cfg_attr(not(test), allow(dead_code))]
fn unescape(field: &str) -> Result<String, String> {
    let mut decoded = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(ch) = chars.next() {
        decoded.push(match ch {
            '\\' => match chars.next() {
                Some('\\') => '\\',
                Some('n') => '\n',
                Some('r') => '\r',
                Some('t') => '\t',
                _ => return Err("invalid S1 string escape".into()),
            },
            '\r' => return Err("unescaped carriage return in S1 string".into()),
            _ => ch,
        });
    }
    Ok(decoded)
}

#[cfg_attr(not(any(test, feature = "neoforge-events")), allow(dead_code))]
pub(crate) const FIRST_MOD_BLOCK_STATE_ID: u32 = 26_684;
#[cfg_attr(not(any(test, feature = "neoforge-events")), allow(dead_code))]
pub(crate) const FIRST_MOD_ITEM_ID: u32 = 1_333;

#[cfg_attr(not(any(test, feature = "neoforge-events")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ModRegistry {
    Block,
    Item,
}

#[cfg_attr(not(any(test, feature = "neoforge-events")), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModRegistryEntry {
    pub(crate) registry: ModRegistry,
    pub(crate) name: String,
    pub(crate) runtime_id: u32,
}

/// Validated, registry/name-ordered view of the current JVM's dynamic IDs.
#[cfg_attr(not(any(test, feature = "neoforge-events")), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModRegistrySnapshot {
    entries: Vec<ModRegistryEntry>,
}

#[cfg_attr(not(any(test, feature = "neoforge-events")), allow(dead_code))]
impl ModRegistrySnapshot {
    /// Decode the bootstrap's S2 table. The terminator, tuple order and
    /// contiguous per-registry IDs are part of the wire contract.
    pub(crate) fn from_wire(wire: &str) -> Result<Self, String> {
        let mut lines = wire.split('\n');
        if lines.next() != Some("S2") {
            return Err("invalid S2 registry header".into());
        }
        let mut entries = Vec::new();
        let mut previous = None;
        let mut next_block = FIRST_MOD_BLOCK_STATE_ID;
        let mut next_item = FIRST_MOD_ITEM_ID;
        for line in lines.by_ref() {
            if line == "end" {
                return if lines.next().is_none() {
                    Ok(Self { entries })
                } else {
                    Err("trailing records after S2 registry terminator".into())
                };
            }
            let mut fields = line.split('\t');
            let (registry, next_id) = match fields.next() {
                Some("minecraft:block") => (ModRegistry::Block, &mut next_block),
                Some("minecraft:item") => (ModRegistry::Item, &mut next_item),
                _ => return Err(format!("unknown S2 registry record {line:?}")),
            };
            let (Some(id), Some(name), None) = (fields.next(), fields.next(), fields.next()) else {
                return Err(format!("malformed S2 registry record {line:?}"));
            };
            let (namespace, path) = mod_name_parts(name)?;
            let order = (registry, namespace, path);
            if previous.is_some_and(|last| order <= last) {
                return Err(format!(
                    "duplicate or out-of-order S2 registry entry {name:?}"
                ));
            }
            if id != next_id.to_string() {
                return Err(format!(
                    "invalid S2 registry ID {id:?} for {name:?}, expected {next_id}"
                ));
            }
            entries.push(ModRegistryEntry {
                registry,
                name: name.into(),
                runtime_id: *next_id,
            });
            *next_id = next_id.checked_add(1).ok_or("S2 registry ID overflow")?;
            previous = Some(order);
        }
        Err("missing S2 registry terminator".into())
    }

    #[cfg(feature = "neoforge-events")]
    pub(crate) fn runtime_id(&self, registry: ModRegistry, name: &str) -> Option<u32> {
        let (namespace, path) = mod_name_parts(name).ok()?;
        let index = self
            .entries
            .binary_search_by(|entry| {
                let (entry_namespace, entry_path) = entry.name.split_once(':').unwrap();
                (entry.registry, entry_namespace, entry_path).cmp(&(registry, namespace, path))
            })
            .ok()?;
        Some(self.entries[index].runtime_id)
    }

    pub(crate) fn entries(&self) -> &[ModRegistryEntry] {
        &self.entries
    }

    /// Encode the dynamic block-state portion of a palette. Each S2 block has
    /// one state, so its canonical resource location fully identifies it.
    #[allow(dead_code)] // The world-save integration consumes this codec separately.
    pub(crate) fn encode_block_palette(&self, states: &[u32]) -> Result<Vec<&str>, String> {
        states
            .iter()
            .map(|&id| {
                id.checked_sub(FIRST_MOD_BLOCK_STATE_ID)
                    .and_then(|index| self.entries.get(index as usize))
                    .filter(|entry| entry.registry == ModRegistry::Block)
                    .map(|entry| entry.name.as_str())
                    .ok_or_else(|| format!("unknown dynamic block-state ID {id}"))
            })
            .collect()
    }

    /// Resolve names against this boot's snapshot. A missing block or an
    /// invalid name fails the whole palette before the caller can use it.
    #[allow(dead_code)]
    pub(crate) fn decode_block_palette(
        &self,
        names: &[impl AsRef<str>],
    ) -> Result<Vec<u32>, String> {
        let block_count = self
            .entries
            .partition_point(|entry| entry.registry == ModRegistry::Block);
        let blocks = &self.entries[..block_count];
        names
            .iter()
            .map(|name| {
                let name = name.as_ref();
                let parts = mod_name_parts(name)?;
                let index = blocks
                    .binary_search_by_key(&parts, |entry| {
                        entry.name.split_once(':').expect("validated S2 block name")
                    })
                    .map_err(|_| format!("missing dynamic block {name:?}"))?;
                Ok(blocks[index].runtime_id)
            })
            .collect()
    }
}

#[cfg_attr(not(any(test, feature = "neoforge-events")), allow(dead_code))]
fn mod_name_parts(name: &str) -> Result<(&str, &str), String> {
    let common_char =
        |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte);
    if let Some((namespace, path)) = name.split_once(':') {
        if !namespace.is_empty()
            && namespace != "minecraft"
            && namespace.bytes().all(common_char)
            && !path.is_empty()
            && path.bytes().all(|byte| common_char(byte) || byte == b'/')
        {
            return Ok((namespace, path));
        }
    }
    Err(format!("invalid dynamic registry name {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s2_registry_uses_canonical_names_and_separate_id_spaces() {
        assert_eq!(FIRST_MOD_ITEM_ID as usize, crate::ITEM_NAMES.len());
        let snapshot = ModRegistrySnapshot::from_wire(concat!(
            "S2\n",
            "minecraft:block\t26684\tmod:stone\n",
            "minecraft:block\t26685\tmod-x:sub/path\n",
            "minecraft:item\t1333\tmod:stone\n",
            "minecraft:item\t1334\tmod:item_only\n",
            "end",
        ));
        // Within one namespace, paths are also ordered.
        assert!(snapshot.is_err());
        let snapshot = ModRegistrySnapshot::from_wire(concat!(
            "S2\n",
            "minecraft:block\t26684\tmod:stone\n",
            "minecraft:block\t26685\tmod-x:sub/path\n",
            "minecraft:item\t1333\tmod:item_only\n",
            "minecraft:item\t1334\tmod:stone\n",
            "end",
        ))
        .unwrap();
        assert_eq!(
            snapshot.entries()[3],
            ModRegistryEntry {
                registry: ModRegistry::Item,
                name: "mod:stone".into(),
                runtime_id: 1334,
            },
        );
        let names = snapshot
            .encode_block_palette(&[26685, 26684, 26685])
            .unwrap();
        assert_eq!(names, ["mod-x:sub/path", "mod:stone", "mod-x:sub/path"]);
        assert_eq!(
            snapshot.decode_block_palette(&names).unwrap(),
            [26685, 26684, 26685]
        );
        assert!(snapshot.encode_block_palette(&[26684, 26686]).is_err());
        assert!(snapshot.encode_block_palette(&[26683]).is_err());
        assert!(snapshot
            .decode_block_palette(&["mod:stone", "mod:item_only"])
            .is_err());
        assert!(snapshot
            .decode_block_palette(&["mod:stone[age=1]"])
            .is_err());
        assert!(ModRegistrySnapshot::from_wire("S2\nend")
            .unwrap()
            .entries()
            .is_empty());
    }

    #[test]
    fn s2_registry_rejects_incomplete_or_corrupt_snapshots() {
        for wire in [
            "S1\nend",
            "S2",
            "S2\nend\n",
            "S2\nminecraft:block\t26684\tmod:stone",
            "S2\nminecraft:block\t26684\tmod:stone\tother\nend",
            "S2\nminecraft:block\t26684\tmod:stone\nminecraft:block\t26685\tmod:stone\nend",
            "S2\nminecraft:item\t1333\tmod:tool\nminecraft:block\t26684\tmod:stone\nend",
            "S2\nminecraft:block\t26684\tmod:stone\nminecraft:block\t26684\tmod:wood\nend",
            "S2\nminecraft:block\t26685\tmod:stone\nend",
            "S2\nminecraft:block\t26683\tmod:stone\nend",
            "S2\nminecraft:item\t+1333\tmod:tool\nend",
            "S2\nminecraft:item\t01333\tmod:tool\nend",
            "S2\nminecraft:item\t4294967296\tmod:tool\nend",
            "S2\nminecraft:item\t1333\tminecraft:stick\nend",
            "S2\nminecraft:item\t1333\tmod:\nend",
            "S2\nminecraft:item\t1333\t:tool\nend",
            "S2\nminecraft:item\t1333\tmod:TOOL\nend",
            "S2\nminecraft:item\t1333\tmod/tool\nend",
            "S2\nminecraft:fluid\t1333\tmod:fluid\nend",
        ] {
            assert!(
                ModRegistrySnapshot::from_wire(wire).is_err(),
                "accepted {wire:?}"
            );
        }
    }

    #[test]
    fn decodes_ordered_mutations_and_event_changes() {
        let wire = concat!(
            "S1\t0\n",
            "message\t-42\tHello\\nNether\\t\\\\\\r 🌍\n",
            "block\t2\t-9\t70\t3\t4294967295\t11\n",
            "block\t0\t1\t-64\t0\t0\t-\n",
            "command\tgreet\\\\world\n",
            "exp\t7\n",
            "chat\t\n",
            "end",
        );
        assert_eq!(
            decode_mutation_batch(wire).unwrap(),
            S1EventOutcome {
                canceled: false,
                break_exp: Some(7),
                chat_message: Some(String::new()),
                mutations: vec![
                    MutationOp::SendSystemMessage {
                        player_eid: -42,
                        message: "Hello\nNether\t\\\r 🌍".into(),
                    },
                    MutationOp::SetBlockIf {
                        dim: 2,
                        pos: (-9, 70, 3),
                        new_state: u32::MAX,
                        expected_state: Some(11),
                    },
                    MutationOp::SetBlockIf {
                        dim: 0,
                        pos: (1, -64, 0),
                        new_state: 0,
                        expected_state: None,
                    },
                    MutationOp::RegisterCommand {
                        literal: "greet\\world".into(),
                    },
                ],
            }
        );
    }

    #[test]
    fn cancellation_requires_an_empty_batch() {
        let decoded = decode_mutation_batch("S1\t1\nend").unwrap();
        assert!(decoded.canceled);
        assert_eq!(decoded.break_exp, None);
        assert_eq!(decoded.chat_message, None);
        assert!(decoded.mutations.is_empty());
        assert!(decode_mutation_batch("S1\t1\ncommand\tgreet\nend").is_err());
        assert!(decode_mutation_batch("S1\t1\nexp\t7\nend").is_err());
    }

    #[test]
    fn rejects_malformed_or_truncated_batches() {
        for wire in [
            "",
            "S2\t0\nend",
            "S1\t2\nend",
            "S1\t0\ncommand\tgreet",
            "S1\t0\nend\ncommand\tgreet",
            "S1\t0\n\nend",
            "S1\t0\nunknown\t1\nend",
            "S1\t0\nblock\t0\t1\t2\t3\t4\nend",
            "S1\t0\nblock\t0\t2147483648\t2\t3\t4\t5\nend",
            "S1\t0\nblock\t0\t1\t2\t3\t4294967296\t5\nend",
            "S1\t0\nblock\t0\t1\t2\t3\t4\t-1\nend",
            "S1\t0\nmessage\tNaN\ttext\nend",
            "S1\t0\nchat\textra\tfield\nend",
            "S1\t0\nchat\tbad\rtext\nend",
            "S1\t0\nchat\tbad\\x\nend",
            "S1\t0\nchat\tbad\\\nend",
            "S1\t0\nexp\t3\nexp\t4\nend",
            "S1\t0\nchat\tfirst\nchat\tsecond\nend",
        ] {
            assert!(decode_mutation_batch(wire).is_err(), "accepted {wire:?}");
        }
    }

    #[test]
    fn rejects_out_of_range_dimension_slots() {
        for dim in ["3", "255", "256", "-1"] {
            assert!(
                decode_mutation_batch(&format!("S1\t0\nblock\t{dim}\t1\t2\t3\t4\t5\nend")).is_err()
            );
        }
    }
}

#[cfg(any(test, feature = "neoforge-events"))]
pub(crate) const S3_CAPACITY: usize = 256 * 1024;
#[cfg(any(test, feature = "neoforge-events"))]
pub(crate) const S3_MAX_RECORDS: usize = 4096;
#[cfg(any(test, feature = "neoforge-events"))]
const S3_MAX_MESSAGE: usize = 4096;
#[cfg(any(test, feature = "neoforge-events"))]
const S3_HEADER_SIZE: usize = 16;
#[cfg(any(test, feature = "neoforge-events"))]
const S3_REQUEST_SIZE: usize = 28;
#[cfg(any(test, feature = "neoforge-events"))]
const S3_REQUEST_HEADER: [u8; 4] = [b'S', b'3', 1, 0];
#[cfg(any(test, feature = "neoforge-events"))]
const S3_RESPONSE_HEADER: [u8; 4] = [b'S', b'3', 1, 1];

// The production world owner supplies activations when it adopts the S3 adapter.
#[cfg(any(test, feature = "neoforge-events"))]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum S3Activation {
    EntityTick = 0,
    BlockEntityTick = 1,
    Container = 2,
    ScheduledBlockTick = 3,
}

#[cfg(any(test, feature = "neoforge-events"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct S3TickRequest {
    pub(crate) activation: S3Activation,
    pub(crate) dim: u8,
    pub(crate) kind: u16,
    pub(crate) id: i32,
    pub(crate) generation: u32,
    pub(crate) pos: (i32, i32, i32),
    pub(crate) state: u32,
}

#[cfg(any(test, feature = "neoforge-events"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum S3BatchOp<'wire> {
    SetBlockIf {
        dim: u8,
        pos: (i32, i32, i32),
        new_state: u32,
        expected_state: u32,
    },
    ScheduleTick {
        dim: u8,
        pos: (i32, i32, i32),
        delay: i32,
        priority: i8,
    },
    ContainerSlotDelta {
        dim: u8,
        pos: (i32, i32, i32),
        slot: u16,
        item_id: u32,
        count: u8,
    },
    SendSystemMessage {
        player_eid: i32,
        message: &'wire str,
    },
}

#[cfg(any(test, feature = "neoforge-events"))]
pub(crate) fn encode_s3_request_batch(
    out: &mut [u8],
    sequence: u64,
    requests: &[S3TickRequest],
) -> Result<usize, &'static str> {
    if requests.len() > S3_MAX_RECORDS || out.len() > S3_CAPACITY {
        return Err("S3 request exceeds capacity");
    }
    let len = S3_HEADER_SIZE + requests.len() * S3_REQUEST_SIZE;
    if out.len() < len {
        return Err("S3 request buffer too small");
    }
    if sequence == 0 || requests.iter().any(|r| r.dim > 2 || r.generation == 0) {
        return Err("invalid S3 request identity");
    }
    out[..4].copy_from_slice(&S3_REQUEST_HEADER);
    out[4..8].copy_from_slice(&(requests.len() as u32).to_le_bytes());
    out[8..16].copy_from_slice(&sequence.to_le_bytes());
    for (out, request) in out[S3_HEADER_SIZE..len]
        .as_chunks_mut::<S3_REQUEST_SIZE>().0.iter_mut()
        .zip(requests)
    {
        out[0] = request.activation as u8;
        out[1] = request.dim;
        out[2..4].copy_from_slice(&request.kind.to_le_bytes());
        out[4..8].copy_from_slice(&request.id.to_le_bytes());
        out[8..12].copy_from_slice(&request.generation.to_le_bytes());
        out[12..16].copy_from_slice(&request.pos.0.to_le_bytes());
        out[16..20].copy_from_slice(&request.pos.1.to_le_bytes());
        out[20..24].copy_from_slice(&request.pos.2.to_le_bytes());
        out[24..28].copy_from_slice(&request.state.to_le_bytes());
    }
    Ok(len)
}

/// Validate every mutation before exposing any of them. Iteration reads the
/// original immutable bytes again, so no mutation array or string is allocated.
#[cfg(any(test, feature = "neoforge-events"))]
pub(crate) fn decode_s3_response_batch<'wire>(
    wire: &'wire [u8],
    sequence: u64,
    requests: &[S3TickRequest],
) -> Result<impl ExactSizeIterator<Item = S3BatchOp<'wire>> + 'wire, &'static str> {
    if wire.len() > S3_CAPACITY || requests.len() > S3_MAX_RECORDS {
        return Err("S3 response exceeds capacity");
    }
    let mut reader = S3Reader(wire);
    if reader.take::<4>()? != S3_RESPONSE_HEADER {
        return Err("invalid S3 response header");
    }
    let count = reader.u32()? as usize;
    if count > S3_MAX_RECORDS {
        return Err("S3 response record limit exceeded");
    }
    if sequence == 0 || u64::from_le_bytes(reader.take()?) != sequence {
        return Err("S3 response sequence mismatch");
    }
    let mut records = reader;
    for _ in 0..count {
        let (source, generation, op) = reader.record()?;
        let request = requests.get(source).ok_or("unknown S3 mutation source")?;
        if generation == 0 || generation != request.generation {
            return Err("S3 mutation generation mismatch");
        }
        let dim = match op {
            S3BatchOp::SetBlockIf { dim, .. }
            | S3BatchOp::ScheduleTick { dim, .. }
            | S3BatchOp::ContainerSlotDelta { dim, .. } => dim,
            S3BatchOp::SendSystemMessage { .. } => request.dim,
        };
        if dim != request.dim || dim > 2 {
            return Err("invalid S3 mutation dimension");
        }
    }
    if !reader.0.is_empty() {
        return Err("trailing bytes in S3 response");
    }
    Ok((0..count).map(move |_| records.record().expect("validated S3 response").2))
}

#[cfg(any(test, feature = "neoforge-events"))]
#[derive(Clone, Copy)]
struct S3Reader<'wire>(&'wire [u8]);

#[cfg(any(test, feature = "neoforge-events"))]
impl<'wire> S3Reader<'wire> {
    fn bytes(&mut self, len: usize) -> Result<&'wire [u8], &'static str> {
        let (field, rest) = self
            .0
            .split_at_checked(len)
            .ok_or("truncated S3 response")?;
        self.0 = rest;
        Ok(field)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        Ok(self.bytes(N)?.try_into().expect("exact field length"))
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn i32(&mut self) -> Result<i32, &'static str> {
        Ok(i32::from_le_bytes(self.take()?))
    }

    fn pos(&mut self) -> Result<(i32, i32, i32), &'static str> {
        Ok((self.i32()?, self.i32()?, self.i32()?))
    }

    fn record(&mut self) -> Result<(usize, u32, S3BatchOp<'wire>), &'static str> {
        let tag = self.u8()?;
        let source = self.u32()? as usize;
        let generation = self.u32()?;
        let op = match tag {
            1 => S3BatchOp::SetBlockIf {
                dim: self.u8()?,
                pos: self.pos()?,
                new_state: self.u32()?,
                expected_state: self.u32()?,
            },
            2 => {
                let dim = self.u8()?;
                let pos = self.pos()?;
                let delay = self.i32()?;
                let priority = self.u8()? as i8;
                if delay < 0 || !(-3..=3).contains(&priority) {
                    return Err("invalid S3 tick delay or priority");
                }
                S3BatchOp::ScheduleTick {
                    dim,
                    pos,
                    delay,
                    priority,
                }
            }
            3 => {
                let dim = self.u8()?;
                let pos = self.pos()?;
                let slot = u16::from_le_bytes(self.take()?);
                let item_id = self.u32()?;
                let count = self.u8()?;
                if count > 99 || (item_id == 0) != (count == 0) {
                    return Err("invalid S3 container stack");
                }
                S3BatchOp::ContainerSlotDelta {
                    dim,
                    pos,
                    slot,
                    item_id,
                    count,
                }
            }
            4 => {
                let player_eid = self.i32()?;
                let len = u16::from_le_bytes(self.take()?) as usize;
                if len > S3_MAX_MESSAGE {
                    return Err("S3 message exceeds capacity");
                }
                let message = std::str::from_utf8(self.bytes(len)?)
                    .map_err(|_| "invalid S3 message UTF-8")?;
                S3BatchOp::SendSystemMessage {
                    player_eid,
                    message,
                }
            }
            _ => return Err("unknown S3 mutation opcode"),
        };
        Ok((source, generation, op))
    }
}

#[cfg(test)]
mod s3_tests {
    use super::*;

    const REQUEST: S3TickRequest = S3TickRequest {
        activation: S3Activation::BlockEntityTick,
        dim: 2,
        kind: 0x1234,
        id: -2,
        generation: 0x8000_0003,
        pos: (1, 2, -3),
        state: 0xffff_ffff,
    };

    #[test]
    fn s3_request_encoding_is_little_endian_and_bounded() {
        let mut out = [0u8; S3_HEADER_SIZE + S3_REQUEST_SIZE];
        assert_eq!(encode_s3_request_batch(&mut out, 3, &[REQUEST]), Ok(44));
        assert_eq!(
            out,
            *b"S3\x01\x00\x01\x00\x00\x00\x03\x00\x00\x00\x00\x00\x00\x00\
              \x01\x02\x34\x12\xfe\xff\xff\xff\x03\x00\x00\x80\x01\x00\x00\x00\
              \x02\x00\x00\x00\xfd\xff\xff\xff\xff\xff\xff\xff"
        );
        assert!(encode_s3_request_batch(&mut out[..43], 3, &[REQUEST]).is_err());
        assert!(encode_s3_request_batch(&mut out, 0, &[]).is_err());
        assert!(
            encode_s3_request_batch(&mut out, 3, &[S3TickRequest { dim: 3, ..REQUEST }]).is_err()
        );
        assert!(encode_s3_request_batch(
            &mut out,
            3,
            &[S3TickRequest {
                generation: 0,
                ..REQUEST
            }]
        )
        .is_err());
        assert!(encode_s3_request_batch(&mut out, 3, &[REQUEST; S3_MAX_RECORDS + 1]).is_err());
    }

    #[test]
    fn s3_decoder_borrows_utf8_and_rejects_a_trailing_partial_record() {
        let mut wire = Vec::from(S3_RESPONSE_HEADER);
        wire.extend(1u32.to_le_bytes());
        wire.extend(3u64.to_le_bytes());
        wire.push(4);
        wire.extend(0u32.to_le_bytes());
        wire.extend(REQUEST.generation.to_le_bytes());
        wire.extend(REQUEST.id.to_le_bytes());
        let text = "梱包 📦";
        wire.extend((text.len() as u16).to_le_bytes());
        wire.extend(text.as_bytes());
        let mut ops = decode_s3_response_batch(&wire, 3, &[REQUEST]).unwrap();
        let Some(S3BatchOp::SendSystemMessage {
            player_eid,
            message,
        }) = ops.next()
        else {
            panic!("expected message");
        };
        assert_eq!((player_eid, message), (REQUEST.id, text));
        assert_eq!(message.as_ptr(), wire[31..].as_ptr());
        drop(ops);
        wire[4] = 2;
        wire.push(4);
        assert!(decode_s3_response_batch(&wire, 3, &[REQUEST]).is_err());
    }
}

/// Protocol 767 Configuration Registry Data bodies, including packet ID and
/// excluding transport length/compression framing. Entries carry names in S2
/// runtime-ID order with an absent optional NBT value.
#[allow(dead_code)] // The configuration owner decides where to send these packets.
pub(crate) fn build_config_registry_sync_packets(
    snapshot: &ModRegistrySnapshot,
) -> Result<Vec<Vec<u8>>, &'static str> {
    fn varint(out: &mut Vec<u8>, value: usize) {
        megu_core::varint::write_varlong_i64(value as i64, out);
    }

    fn string(out: &mut Vec<u8>, value: &str) {
        megu_core::varint::write_mc_string(value, out);
    }

    let (blocks, items) = snapshot.entries.split_at(
        snapshot
            .entries
            .partition_point(|entry| entry.registry == ModRegistry::Block),
    );
    [("minecraft:block", blocks), ("minecraft:item", items)]
        .into_iter()
        .filter(|(_, entries)| !entries.is_empty())
        .map(|(registry, entries)| {
            let mut packet = vec![0x07];
            string(&mut packet, registry);
            if entries.len() > i32::MAX as usize {
                return Err("too many Configuration Registry Data entries");
            }
            varint(&mut packet, entries.len());
            for entry in entries {
                // Snapshot names contain only ASCII. The ResourceLocation
                // codec decodes them using Minecraft's default string limit.
                if entry.name.len() > 32767 {
                    return Err("oversized Configuration Registry Data entry name");
                }
                string(&mut packet, &entry.name);
                packet.push(0);
            }
            Ok(packet)
        })
        .collect()
}

#[cfg(any(test, feature = "neoforge-events"))]
pub(crate) const S4_MAX_PAYLOAD: usize = 32 * 1024;
#[cfg(any(test, feature = "neoforge-events"))]
pub(crate) const S4_MAX_CHANNEL: usize = 256;
#[cfg(any(test, feature = "neoforge-events"))]
pub(crate) const S4_PAYLOAD_CAPACITY: usize = 256 * 1024;
#[cfg(any(test, feature = "neoforge-events"))]
const S4_MAX_ENVELOPES: usize = 64;
#[cfg(any(test, feature = "neoforge-events"))]
const S4_PAYLOAD_MAGIC: [u8; 4] = [b'S', b'4', 1, 0];

#[cfg(any(test, feature = "neoforge-events"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CustomPayloadEnvelope {
    pub(crate) channel: String,
    /// None broadcasts for S2C and is mandatory on C2S.
    pub(crate) target_eid: Option<i32>,
    pub(crate) bytes: Vec<u8>,
}

#[cfg(any(test, feature = "neoforge-events"))]
fn valid_s4_channel(channel: &str) -> bool {
    channel.len() <= S4_MAX_CHANNEL && mod_name_parts(channel).is_ok()
}

/// v1: magic, u16 envelope count, then length-prefixed channel, optional
/// target tag and EID, and length-prefixed payload, all little-endian.
#[cfg(any(test, feature = "neoforge-events"))]
#[allow(dead_code)] // Available to the host owner.
pub(crate) fn encode_s4_payload_batch(
    envelopes: &[CustomPayloadEnvelope],
) -> Result<Vec<u8>, &'static str> {
    if envelopes.len() > S4_MAX_ENVELOPES {
        return Err("too many S4 envelopes");
    }
    let mut out = Vec::from(S4_PAYLOAD_MAGIC);
    out.extend_from_slice(&(envelopes.len() as u16).to_le_bytes());
    for envelope in envelopes {
        let channel = envelope.channel.as_bytes();
        if !valid_s4_channel(&envelope.channel) {
            return Err("invalid S4 channel");
        }
        if envelope.bytes.len() > S4_MAX_PAYLOAD {
            return Err("oversized S4 payload");
        }
        let target_size = usize::from(envelope.target_eid.is_some()) * 4;
        if out.len() + 2 + channel.len() + 1 + target_size + 4 + envelope.bytes.len()
            > S4_PAYLOAD_CAPACITY
        {
            return Err("oversized S4 payload batch");
        }
        out.extend_from_slice(&(channel.len() as u16).to_le_bytes());
        out.extend_from_slice(channel);
        out.push(envelope.target_eid.is_some().into());
        if let Some(target) = envelope.target_eid {
            out.extend_from_slice(&target.to_le_bytes());
        }
        out.extend_from_slice(&(envelope.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&envelope.bytes);
    }
    Ok(out)
}

#[cfg(any(test, feature = "neoforge-events"))]
#[allow(dead_code)] // Available to the host owner.
pub(crate) fn decode_s4_payload_batch(
    mut bytes: &[u8],
) -> Result<Vec<CustomPayloadEnvelope>, &'static str> {
    fn take<'a>(input: &mut &'a [u8], size: usize) -> Result<&'a [u8], &'static str> {
        let (field, rest) = input
            .split_at_checked(size)
            .ok_or("truncated S4 payload frame")?;
        *input = rest;
        Ok(field)
    }

    fn number<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], &'static str> {
        Ok(take(input, N)?.try_into().expect("exact field length"))
    }

    if bytes.len() > S4_PAYLOAD_CAPACITY || number::<4>(&mut bytes)? != S4_PAYLOAD_MAGIC {
        return Err("invalid S4 payload frame");
    }
    let count = u16::from_le_bytes(number(&mut bytes)?) as usize;
    if count > S4_MAX_ENVELOPES {
        return Err("too many S4 envelopes");
    }
    let mut envelopes = Vec::with_capacity(count);
    for _ in 0..count {
        let length = u16::from_le_bytes(number(&mut bytes)?) as usize;
        if length > S4_MAX_CHANNEL {
            return Err("oversized S4 channel");
        }
        let channel = std::str::from_utf8(take(&mut bytes, length)?)
            .map_err(|_| "invalid S4 channel UTF-8")?;
        if !valid_s4_channel(channel) {
            return Err("invalid S4 channel");
        }
        let target_eid = match take(&mut bytes, 1)?[0] {
            0 => None,
            1 => Some(i32::from_le_bytes(number(&mut bytes)?)),
            _ => return Err("invalid S4 target tag"),
        };
        let length = u32::from_le_bytes(number(&mut bytes)?) as usize;
        if length > S4_MAX_PAYLOAD {
            return Err("oversized S4 payload");
        }
        envelopes.push(CustomPayloadEnvelope {
            channel: channel.into(),
            target_eid,
            bytes: take(&mut bytes, length)?.to_vec(),
        });
    }
    if !bytes.is_empty() {
        return Err("trailing bytes in S4 payload frame");
    }
    Ok(envelopes)
}

#[cfg(any(test, feature = "neoforge-events"))]
const S4_MAX_SLOTS: usize = 256;
#[cfg(any(test, feature = "neoforge-events"))]
const S4_MAX_CONTAINER_OPS: usize = 256;

#[cfg(any(test, feature = "neoforge-events"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct S4ItemStack {
    pub(crate) item_id: u32,
    pub(crate) count: u8,
}

#[cfg(any(test, feature = "neoforge-events"))]
impl S4ItemStack {
    #[cfg(test)]
    pub(crate) const EMPTY: Self = Self {
        item_id: 0,
        count: 0,
    };

    fn validate(self, limit: u8) -> Result<(), &'static str> {
        if self.count > limit || (self.item_id == 0) != (self.count == 0) {
            return Err("invalid S4 item stack");
        }
        Ok(())
    }

    fn decode(wire: &[i32], limit: u8) -> Result<Self, &'static str> {
        let stack = Self {
            item_id: wire[0] as u32,
            count: u8::try_from(wire[1]).map_err(|_| "invalid S4 stack count")?,
        };
        stack.validate(limit)?;
        Ok(stack)
    }

    fn encode(self, limit: u8) -> Result<[i32; 2], &'static str> {
        self.validate(limit)?;
        Ok([self.item_id as i32, self.count.into()])
    }
}

#[cfg(any(test, feature = "neoforge-events"))]
#[allow(dead_code)] // Available to the host owner.
#[derive(Clone, Copy, Debug)]
pub(crate) enum S4BlockSide {
    Down,
    Up,
    North,
    South,
    West,
    East,
}

#[cfg(any(test, feature = "neoforge-events"))]
#[allow(dead_code)] // Available to the host owner.
#[derive(Debug)]
pub(crate) struct S4ContainerSnapshot {
    pub(crate) dim: u8,
    pub(crate) pos: (i32, i32, i32),
    pub(crate) state: u32,
    pub(crate) side: Option<S4BlockSide>,
    pub(crate) slot_limit: u8,
    pub(crate) slots: Vec<S4ItemStack>,
}

#[cfg(any(test, feature = "neoforge-events"))]
#[allow(dead_code)] // Available to the host owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum S4ContainerOp {
    Read {
        slot: u16,
    },
    Insert {
        slot: u16,
        stack: S4ItemStack,
        simulate: bool,
    },
    Extract {
        slot: u16,
        amount: u32,
        simulate: bool,
    },
}

#[cfg(any(test, feature = "neoforge-events"))]
impl S4ContainerOp {
    fn slot(self) -> u16 {
        match self {
            Self::Read { slot } | Self::Insert { slot, .. } | Self::Extract { slot, .. } => slot,
        }
    }

    fn writes(self) -> bool {
        matches!(
            self,
            Self::Insert {
                simulate: false,
                ..
            } | Self::Extract {
                simulate: false,
                ..
            }
        )
    }
}

#[cfg(any(test, feature = "neoforge-events"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct S4SlotResult {
    pub(crate) limit: u8,
    pub(crate) stack: S4ItemStack,
}

#[cfg(any(test, feature = "neoforge-events"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct S4SlotMutation {
    pub(crate) dim: u8,
    pub(crate) pos: (i32, i32, i32),
    pub(crate) slot: u16,
    pub(crate) expected_state: u32,
    pub(crate) expected: S4ItemStack,
    pub(crate) replacement: S4ItemStack,
}

#[cfg(any(test, feature = "neoforge-events"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct S4ContainerOutcome {
    pub(crate) handler_slots: u16,
    pub(crate) results: Vec<S4SlotResult>,
    pub(crate) mutations: Vec<S4SlotMutation>,
}

#[cfg(any(test, feature = "neoforge-events"))]
#[allow(dead_code)] // Available to the host owner.
impl S4ContainerSnapshot {
    pub(crate) fn encode_query(&self, ops: &[S4ContainerOp]) -> Result<Vec<i32>, &'static str> {
        if self.dim > 2
            || !(1..=99).contains(&self.slot_limit)
            || self.slots.len() > S4_MAX_SLOTS
            || ops.len() > S4_MAX_CONTAINER_OPS
        {
            return Err("invalid S4 container snapshot or operation count");
        }
        let (x, y, z) = self.pos;
        let mut wire = Vec::with_capacity(9 + 2 * self.slots.len() + 4 * ops.len());
        wire.extend([
            self.dim.into(),
            x,
            y,
            z,
            self.state as i32,
            self.side.map_or(-1, |side| side as i32),
            self.slot_limit.into(),
            self.slots.len() as i32,
            ops.len() as i32,
        ]);
        for stack in &self.slots {
            wire.extend(stack.encode(self.slot_limit)?);
        }
        for op in ops {
            let (tag, item, count) = match *op {
                S4ContainerOp::Read { .. } => (0, 0, 0),
                S4ContainerOp::Insert {
                    stack, simulate, ..
                } => {
                    let [item, count] = stack.encode(99)?;
                    (1 + i32::from(simulate), item, count)
                }
                S4ContainerOp::Extract {
                    amount, simulate, ..
                } => (
                    3 + i32::from(simulate),
                    0,
                    i32::try_from(amount).map_err(|_| "invalid S4 extraction amount")?,
                ),
            };
            wire.extend([tag, op.slot().into(), item, count]);
        }
        Ok(wire)
    }

    pub(crate) fn response_len(&self, ops: &[S4ContainerOp]) -> usize {
        1 + 3 * ops.len() + 2 * self.slots.len()
    }

    pub(crate) fn decode_response(
        &self,
        ops: &[S4ContainerOp],
        mut wire: &[i32],
    ) -> Result<Option<S4ContainerOutcome>, &'static str> {
        let Some((&handler_slots, rest)) = wire.split_first() else {
            return Ok(None);
        };
        wire = rest;
        if !(0..=S4_MAX_SLOTS as i32).contains(&handler_slots)
            || wire.len() + 1 != self.response_len(ops)
            || ops.iter().any(|op| i32::from(op.slot()) >= handler_slots)
        {
            return Err("invalid S4 capability response");
        }
        let (results, slots) = wire.split_at(3 * ops.len());
        let results = results
            .as_chunks::<3>().0.iter()
            .map(|value| {
                Ok(S4SlotResult {
                    limit: u8::try_from(value[0])
                        .ok()
                        .filter(|limit| *limit <= 99)
                        .ok_or("invalid S4 result slot limit")?,
                    stack: S4ItemStack::decode(&value[1..], 99)?,
                })
            })
            .collect::<Result<Vec<_>, &'static str>>()?;
        let mut mutations = Vec::new();
        let writes = ops.iter().any(|op| op.writes());
        for (slot, (wire, &expected)) in slots.as_chunks::<2>().0.iter().zip(&self.slots).enumerate() {
            let replacement = S4ItemStack::decode(wire, self.slot_limit)?;
            if replacement != expected {
                if !writes {
                    return Err("mutation in read-only S4 capability response");
                }
                mutations.push(S4SlotMutation {
                    dim: self.dim,
                    pos: self.pos,
                    slot: slot as u16,
                    expected_state: self.state,
                    expected,
                    replacement,
                });
            }
        }
        Ok(Some(S4ContainerOutcome {
            handler_slots: handler_slots as u16,
            results,
            mutations,
        }))
    }
}

#[cfg(test)]
mod s4_tests {
    use super::*;

    #[test]
    fn payload_batch_rejects_every_truncation_and_invalid_fields() {
        let channel = "fixture:echo";
        let envelopes = [
            CustomPayloadEnvelope {
                channel: channel.into(),
                target_eid: Some(i32::MIN),
                bytes: vec![0xda, 0xff],
            },
            CustomPayloadEnvelope {
                channel: "fixture:empty".into(),
                target_eid: None,
                bytes: vec![],
            },
        ];
        let encoded = encode_s4_payload_batch(&envelopes).unwrap();
        assert_eq!(decode_s4_payload_batch(&encoded).unwrap(), envelopes);
        for end in 0..encoded.len() {
            assert!(decode_s4_payload_batch(&encoded[..end]).is_err());
        }
        let target = 8 + channel.len();
        let payload_len = target + 5;
        let mut invalid = encoded.clone();
        invalid[target] = 2;
        assert_eq!(
            decode_s4_payload_batch(&invalid),
            Err("invalid S4 target tag")
        );
        invalid = encoded.clone();
        invalid[payload_len..payload_len + 4]
            .copy_from_slice(&((S4_MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert_eq!(
            decode_s4_payload_batch(&invalid),
            Err("oversized S4 payload")
        );
        invalid = encoded;
        invalid.push(0);
        assert_eq!(
            decode_s4_payload_batch(&invalid),
            Err("trailing bytes in S4 payload frame")
        );
    }

    #[test]
    fn capability_response_rejects_mutations_in_a_read_only_batch() {
        let snapshot = S4ContainerSnapshot {
            dim: 0,
            pos: (1, 2, 3),
            state: FIRST_MOD_BLOCK_STATE_ID,
            side: None,
            slot_limit: 16,
            slots: vec![S4ItemStack::EMPTY],
        };
        let ops = [S4ContainerOp::Read { slot: 0 }];
        let with_mutation = [1, 16, 0, 0, 1, 4];
        assert_eq!(
            snapshot.decode_response(&ops, &with_mutation),
            Err("mutation in read-only S4 capability response")
        );
        assert!(snapshot.decode_response(&ops, &with_mutation[..5]).is_err());
        assert_eq!(snapshot.decode_response(&ops, &[]).unwrap(), None);
    }

    #[test]
    fn registry_packet_rejects_names_beyond_the_protocol_string_limit() {
        let wire = |length: usize| {
            format!(
                "S2\nminecraft:item\t1333\tfixture:{}\nend",
                "x".repeat(length - 8)
            )
        };
        let max = ModRegistrySnapshot::from_wire(&wire(32767)).unwrap();
        assert!(build_config_registry_sync_packets(&max).is_ok());
        let oversized = ModRegistrySnapshot::from_wire(&wire(32768)).unwrap();
        assert_eq!(
            build_config_registry_sync_packets(&oversized),
            Err("oversized Configuration Registry Data entry name"),
        );
    }

    #[test]
    fn registry_packet_uses_multibyte_count_and_entry_length() {
        let mut wire = String::from("S2\n");
        for index in 0..128 {
            use std::fmt::Write;
            writeln!(
                wire,
                "minecraft:item\t{}\tfixture:{index:03}_{}",
                FIRST_MOD_ITEM_ID + index,
                "x".repeat(116),
            )
            .unwrap();
        }
        wire.push_str("end");
        let snapshot = ModRegistrySnapshot::from_wire(&wire).unwrap();
        let packets = build_config_registry_sync_packets(&snapshot).unwrap();
        let [packet] = packets.as_slice() else {
            panic!("expected one item packet")
        };
        let first = b"\x07\x0eminecraft:item\x80\x01\x80\x01fixture:000_";
        assert!(packet.starts_with(first));
        assert_eq!(&packet[first.len()..first.len() + 116], &[b'x'; 116]);
        assert_eq!(packet[first.len() + 116], 0);
        assert!(build_config_registry_sync_packets(
            &ModRegistrySnapshot::from_wire("S2\nend").unwrap()
        )
        .unwrap()
        .is_empty());
    }
}

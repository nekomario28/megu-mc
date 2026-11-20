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

    #[allow(dead_code)] // For the consumer of the validated startup table.
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

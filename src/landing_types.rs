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

#[cfg(test)]
mod tests {
    use super::*;

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

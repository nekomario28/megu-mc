//! Byte-exact tests for [`crate::brigadier_wire`].
//!
//! Every expected byte sequence here is either computed by hand from the layout documented in
//! `brigadier_wire`'s module docs, or captured from the pinned 1.21.1 server jar by building the
//! same tree in Java and letting the jar's own `ClientboundCommandsPacket` write it (the probe is
//! recorded in `~/mc-spine-local/audit/brigadier-slice2-*/evidence/probe-output.txt`). Both kinds
//! of case assert the bytes, never just success: a serializer that writes a plausible-looking but
//! wrong tree passes every `is_ok` assertion and puts an unloadable tab-completion list on a
//! client.

use crate::brigadier::*;
use crate::brigadier_wire::*;

/// Every tree in this module has a source type but no source-dependent behaviour, so `()` is it.
type Source = ();

/// `hex("03 00 01")` - the readable form of a byte expectation. A malformed literal fails the
/// test with the token that broke, rather than silently comparing fewer bytes.
fn hex(text: &str) -> Vec<u8> {
    text.split_whitespace()
        .map(|token| {
            assert_eq!(token.len(), 2, "hex token '{token}' is not two characters");
            u8::from_str_radix(token, 16).unwrap_or_else(|_| panic!("hex token '{token}'"))
        })
        .collect()
}

fn literal(name: &str) -> CommandNode<Source> {
    ArgumentBuilder::<Source>::literal(name).build()
}

fn executable_literal(name: &str) -> CommandNode<Source> {
    ArgumentBuilder::<Source>::literal(name)
        .executes(|_| Ok(1))
        .build()
}

fn argument(name: &str, argument_type: Box<dyn ArgumentType<Source>>) -> CommandNode<Source> {
    ArgumentBuilder::<Source>::argument(name, argument_type).build()
}

fn executable_argument(
    name: &str,
    argument_type: Box<dyn ArgumentType<Source>>,
) -> CommandNode<Source> {
    ArgumentBuilder::<Source>::argument(name, argument_type)
        .executes(|_| Ok(2))
        .build()
}

fn tree(children: Vec<CommandNode<Source>>) -> CommandNode<Source> {
    let root = CommandNode::<Source>::root();
    for child in children {
        root.add_child(child).expect("a built child is never a root");
    }
    root
}

fn provider() -> SuggestionProvider<Source> {
    std::sync::Arc::new(|_context, builder| Ok(builder.build()))
}

// ---------------------------------------------------------------------------------------
// Commands: node array
// ---------------------------------------------------------------------------------------

#[test]
fn an_empty_tree_is_one_root_node_and_the_root_index() {
    // count 1 | node 0: flags 0 (root), 0 children | root index 0
    assert_eq!(commands_payload(&tree(vec![])).unwrap(), hex("01 00 00 00"));
}

#[test]
fn a_single_executable_literal_has_the_literal_and_executable_flags() {
    // count 2
    // node 0 (root):   flags 00, 1 child -> 1
    // node 1 ("test"): flags 01|04 = 05, 0 children, literal "test" = 04 74 65 73 74
    // root index 0
    assert_eq!(
        commands_payload(&tree(vec![executable_literal("test")])).unwrap(),
        hex("02
             00 01 01
             05 00 04 74 65 73 74
             00")
    );
}

#[test]
fn a_literal_and_an_integer_argument_match_the_jar_byte_for_byte() {
    // Captured from the jar: root -> "test" (executable) -> "count" integer(0,100) (executable).
    // count 3 | root: 00, child 1 | "test": 05, child 2, "test" |
    // "count": 02|04 = 06, 0 children, "count", parser 3 (integer),
    //          range flags 03 (has min, has max), min 0, max 100 | root 0
    let count = executable_argument(
        "count",
        Box::new(IntegerArgumentType::integer_between(0, 100)),
    );
    let test = {
        let node = executable_literal("test");
        node.add_child(count).unwrap();
        node
    };
    let payload = commands_payload(&tree(vec![test])).unwrap();
    assert_eq!(
        payload,
        hex("03
             00 01 01
             05 01 02 04 74 65 73 74
             06 00 05 63 6f 75 6e 74 03 03 00 00 00 00 00 00 00 64
             00")
    );
    // 1 (count) + 3 (root: flags, child count, child index) + 8 ("test": flags, child count,
    // child index, name length, name) + 18 ("count") + 1 (root index) = 31.
    assert_eq!(payload.len(), 31);
}

#[test]
fn a_redirect_writes_the_index_of_the_node_it_points_at() {
    // Captured from the jar: root -> "a" (executable), root -> "alias" (redirect -> "a").
    // The redirect node's index is 1, not its position after the children array - a serializer
    // that wrote the redirect *node's own* index (2) or its offset in the child list (0) would
    // still produce a tree the client accepts and silently redirects somewhere else.
    let target = executable_literal("a");
    let alias = ArgumentBuilder::<Source>::literal("alias")
        .redirect(target.clone())
        .build();
    let payload = commands_payload(&tree(vec![target, alias])).unwrap();
    assert_eq!(
        payload,
        hex("03
             00 02 01 02
             05 00 01 61
             09 00 01 05 61 6c 69 61 73
             00")
    );
}

#[test]
fn a_redirect_to_an_already_indexed_node_does_not_add_a_second_node() {
    // `execute` redirects to the root, so a tree that redirects to an ancestor must not index the
    // root twice: count 2, root (1 child), "run" (flags 01|08, 0 children, redirect -> 0, "run").
    let root = CommandNode::<Source>::root();
    let run = ArgumentBuilder::<Source>::literal("run")
        .redirect(root.clone())
        .build();
    root.add_child(run).unwrap();
    assert_eq!(
        commands_payload(&root).unwrap(),
        hex("02
             00 01 01
             09 00 00 03 72 75 6e
             00")
    );
}

#[test]
fn node_indices_are_assigned_breadth_first() {
    // root -> a, root -> b, a -> a1, a -> a2. Breadth-first gives b index 2 and a's children
    // 3,4; depth-first would give b index 4. The jar assigns breadth-first
    // (`acn.a(RootCommandNode)`: a queue, children then redirect), and the client rebuilds the
    // tree in the order the indices imply, so the order is part of the format.
    let a = literal("a");
    a.add_child(literal("a1")).unwrap();
    a.add_child(literal("a2")).unwrap();
    let root = tree(vec![a, literal("b")]);
    assert_eq!(
        commands_payload(&root).unwrap(),
        hex("05
             00 02 01 02
             01 02 03 04 01 61
             01 00 01 62
             01 00 02 61 31
             01 00 02 61 32
             00")
    );
}

#[test]
fn every_primitive_argument_type_matches_the_jar_byte_for_byte() {
    // Captured from the jar: root -> "base" -> one argument of each primitive type, in this
    // order. The payload exercises every parser id and every parser-data form at once.
    let base = literal("base");
    let arguments: Vec<CommandNode<Source>> = vec![
        argument("b", Box::new(BoolArgumentType::bool())),
        argument(
            "f",
            Box::new(FloatArgumentType::float_between(-1.0, 1.0)),
        ),
        argument(
            "d",
            Box::new(DoubleArgumentType::double_between(0.5, 2.5)),
        ),
        argument(
            "i",
            Box::new(IntegerArgumentType::integer_between(0, 100)),
        ),
        argument("l", Box::new(LongArgumentType::long_between(7, 9))),
        argument("sw", Box::new(StringArgumentType::word())),
        argument("sq", Box::new(StringArgumentType::string())),
        argument("sg", Box::new(StringArgumentType::greedy_string())),
    ];
    for node in arguments {
        base.add_child(node).unwrap();
    }
    let payload = commands_payload(&tree(vec![base])).unwrap();
    assert_eq!(
        payload,
        hex("0a
             00 01 01
             01 08 02 03 04 05 06 07 08 09 04 62 61 73 65
             02 00 01 62 00
             02 00 01 66 01 03 bf 80 00 00 3f 80 00 00
             02 00 01 64 02 03 3f e0 00 00 00 00 00 00 40 04 00 00 00 00 00 00
             02 00 01 69 03 03 00 00 00 00 00 00 00 64
             02 00 01 6c 04 03 00 00 00 00 00 00 00 07 00 00 00 00 00 00 00 09
             02 00 02 73 77 05 00
             02 00 02 73 71 05 01
             02 00 02 73 67 05 02
             00")
    );
    assert_eq!(payload.len(), 118);
}

#[test]
fn an_unbounded_numeric_argument_writes_a_zero_flags_byte_and_no_bounds() {
    // Captured from the jar. This is the case a naive "flags then always two values" serializer
    // gets wrong: `integer()` is the full range, so both bound bits are clear and no value
    // follows (`03 00`), while `integerAtLeast(5)` has only the minimum bit (`03 01 00 00 00 05`).
    let base = literal("base");
    let arguments: Vec<CommandNode<Source>> = vec![
        argument("a0", Box::new(IntegerArgumentType::integer())),
        argument("a1", Box::new(IntegerArgumentType::integer_at_least(5))),
        argument("a2", Box::new(FloatArgumentType::float_arg())),
        argument("a3", Box::new(FloatArgumentType::float_at_least(0.5))),
        argument("a4", Box::new(DoubleArgumentType::double_arg())),
        argument("a5", Box::new(DoubleArgumentType::double_at_least(0.5))),
        argument("a6", Box::new(LongArgumentType::long_arg())),
        argument("a7", Box::new(LongArgumentType::long_at_least(3))),
    ];
    for node in arguments {
        base.add_child(node).unwrap();
    }
    let payload = commands_payload(&tree(vec![base])).unwrap();
    assert_eq!(
        payload,
        hex("0a
             00 01 01
             01 08 02 03 04 05 06 07 08 09 04 62 61 73 65
             02 00 02 61 30 03 00
             02 00 02 61 31 03 01 00 00 00 05
             02 00 02 61 32 01 00
             02 00 02 61 33 01 01 3f 00 00 00
             02 00 02 61 34 02 00
             02 00 02 61 35 02 01 3f e0 00 00 00 00 00 00
             02 00 02 61 36 04 00
             02 00 02 61 37 04 01 00 00 00 00 00 00 00 03
             00")
    );
    assert_eq!(payload.len(), 100);
}

#[test]
fn a_named_suggestion_provider_is_written_as_a_resource_location_after_the_parser_data() {
    // Captured from the jar: root -> argument "s" (string, word) with provider
    // `minecraft:ask_server`. flags 02|10 = 12, then name, parser 5, mode 00, then the location.
    let node = ArgumentBuilder::<Source>::argument("s", Box::new(StringArgumentType::word()))
        .suggests(provider())
        .build();
    let names = SuggestionProviderNames::new().with(
        &node,
        SuggestionProviderId::parse("minecraft:ask_server").unwrap(),
    );
    assert_eq!(
        commands_payload_named(&tree(vec![node.clone()]), &names).unwrap(),
        hex("02 00 01 01 12 00 01 73 05 00 14 6d 69 6e 65 63 72 61 66 74 3a 61 73 6b 5f 73 65 72 76 65 72 00")
    );
}

#[test]
fn the_packet_is_the_body_with_the_protocol_prefixes_and_nothing_else() {
    // The payload API is what a caller frames itself; the packet API must be the payload with the
    // protocol's two prefixes in front of it and nothing else changed.
    let count = executable_argument(
        "count",
        Box::new(IntegerArgumentType::integer_between(0, 100)),
    );
    let test = executable_literal("test");
    test.add_child(count).unwrap();
    let root = tree(vec![test]);
    let payload = commands_payload(&root).unwrap();

    // body = VarInt id 0x10 + 31 payload bytes = 32 bytes, so the frame starts with 0x20.
    let mut expected = hex("20 10");
    expected.extend(payload.iter().copied());
    assert_eq!(commands_packet(&root).unwrap(), expected);
    assert_eq!(&expected[..2], &[0x20, 0x10]);
}

// ---------------------------------------------------------------------------------------
// Commands: refusals
// ---------------------------------------------------------------------------------------

/// An argument type this module does not own. `type_info` falls back to the trait's `None`, which
/// is how a 1.21.1-only or third-party parser will look from here.
struct Opaque;

impl ArgumentType<Source> for Opaque {
    fn parse(&self, _reader: &mut StringReader) -> Result<ArgumentValue, CommandSyntaxException> {
        Ok(ArgumentValue::new(()))
    }

    fn box_clone(&self) -> Box<dyn ArgumentType<Source>> {
        Box::new(Opaque)
    }
}

#[test]
fn an_argument_type_this_slice_does_not_own_is_refused_not_guessed() {
    let node = argument("selector", Box::new(Opaque));
    assert_eq!(
        commands_payload(&tree(vec![node])),
        Err(WireError::UnsupportedArgumentType {
            node: "selector".to_string()
        })
    );
}

#[test]
fn a_custom_provider_without_a_registry_name_is_refused() {
    let node = ArgumentBuilder::<Source>::argument("s", Box::new(StringArgumentType::word()))
        .suggests(provider())
        .build();
    assert_eq!(
        commands_payload(&tree(vec![node])),
        Err(WireError::UnnamedSuggestionProvider {
            node: "s".to_string()
        })
    );
}

#[test]
fn a_node_name_past_the_field_cap_is_refused() {
    let long = "x".repeat(32768);
    assert_eq!(
        commands_payload(&tree(vec![literal(&long)])),
        Err(WireError::TextTooLong {
            field: "literal",
            length: 32768,
            maximum: 32767,
        })
    );
}

// ---------------------------------------------------------------------------------------
// Command suggestions: serverbound request
// ---------------------------------------------------------------------------------------

#[test]
fn the_request_serializes_to_its_transaction_id_and_text() {
    // Captured from the jar: `new ServerboundCommandSuggestionPacket(42, "hel")`.
    let request = CommandSuggestionsRequest::new(42, "hel");
    assert_eq!(
        command_suggestions_request_payload(&request).unwrap(),
        hex("2a 03 68 65 6c")
    );
    // body = id 0x0b + 5 bytes = 6, so the frame is 06 | 0b | 2a 03 68 65 6c
    assert_eq!(
        command_suggestions_request_packet(&request).unwrap(),
        hex("06 0b 2a 03 68 65 6c")
    );
}

#[test]
fn a_request_round_trips_and_refuses_leftover_bytes() {
    for (id, text) in [(0, ""), (42, "hel"), (-1, "say hi"), (7, "üñí")] {
        let request = CommandSuggestionsRequest::new(id, text);
        let payload = command_suggestions_request_payload(&request).unwrap();
        assert_eq!(parse_command_suggestions_request(&payload), Ok(request));
    }
    // A trailing byte is a field this parser does not know: refusing is the only safe answer,
    // because the alternative is silently dropping whatever the client meant by it.
    let mut payload = command_suggestions_request_payload(&CommandSuggestionsRequest::new(1, "a"))
        .unwrap();
    payload.push(0x00);
    assert_eq!(
        parse_command_suggestions_request(&payload),
        Err(WireError::TrailingBytes { remaining: 1 })
    );
}

#[test]
fn a_truncated_request_is_refused_instead_of_panicking() {
    // VarInt length 5 with three bytes present.
    assert_eq!(
        parse_command_suggestions_request(&hex("01 05 61 62 63")),
        Err(WireError::Truncated {
            needed: 5,
            remaining: 3
        })
    );
    // An empty body ends inside the transaction id.
    assert_eq!(
        parse_command_suggestions_request(&[]),
        Err(WireError::Truncated {
            needed: 1,
            remaining: 0
        })
    );
    // Six continuation bytes cannot be a five-byte VarInt.
    assert_eq!(
        parse_command_suggestions_request(&hex("ff ff ff ff ff 01")),
        Err(WireError::VarIntTooLong)
    );
}

#[test]
fn a_request_text_past_the_packets_own_cap_is_refused() {
    let request = CommandSuggestionsRequest::new(1, "a".repeat(32501));
    assert_eq!(
        command_suggestions_request_payload(&request),
        Err(WireError::TextTooLong {
            field: "request text",
            length: 32501,
            maximum: 32500
        })
    );
    // Exactly at the cap is fine.
    assert!(command_suggestions_request_payload(&CommandSuggestionsRequest::new(
        1,
        "a".repeat(32500)
    ))
    .is_ok());
}

// ---------------------------------------------------------------------------------------
// Command suggestions: clientbound response
// ---------------------------------------------------------------------------------------

#[test]
fn the_response_serializes_to_id_start_length_count_and_entries() {
    // Captured from the jar: `new ClientboundCommandSuggestionsPacket(42, new Suggestions(
    // StringRange.between(1, 4), ["help", "hello"]))` -> start 1, length 3.
    let response = CommandSuggestionsResponse {
        transaction_id: 42,
        start: 1,
        length: 3,
        entries: vec![SuggestionEntry::new("help"), SuggestionEntry::new("hello")],
    };
    assert_eq!(
        command_suggestions_response_payload(&response).unwrap(),
        hex("2a 01 03 02 04 68 65 6c 70 00 05 68 65 6c 6c 6f 00")
    );
    assert_eq!(
        command_suggestions_response_packet(&response).unwrap(),
        hex("12 0f 2a 01 03 02 04 68 65 6c 70 00 05 68 65 6c 6c 6f 00")
    );
}

#[test]
fn the_dispatchers_string_range_becomes_the_packets_start_and_length() {
    // The mapping is the whole reason this module exists: `start` is the range's start and
    // `length` its length, whatever cursor the client sent.
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(ArgumentBuilder::literal("help").executes(|_| Ok(1)))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("hello").executes(|_| Ok(1)))
        .unwrap();
    let parse = dispatcher.parse("hel", ());
    let suggestions = dispatcher.get_completion_suggestions(&parse);
    assert_eq!(suggestions.range(), StringRange::between(0, 3));

    let response = CommandSuggestionsResponse::from_suggestions(42, &suggestions);
    // The dispatcher sorts its answer (`Suggestions::create`), and "hello" sorts before "help":
    // the comparator walks the text, so index 3 decides with 'l' < 'p'.
    assert_eq!(
        response,
        CommandSuggestionsResponse {
            transaction_id: 42,
            start: 0,
            length: 3,
            entries: vec![SuggestionEntry::new("hello"), SuggestionEntry::new("help")],
        }
    );
    assert_eq!(
        command_suggestions_response_payload(&response).unwrap(),
        hex("2a 00 03 02 05 68 65 6c 6c 6f 00 04 68 65 6c 70 00")
    );
}

#[test]
fn a_suggestion_tooltip_is_modelled_and_refused() {
    let response = CommandSuggestionsResponse {
        transaction_id: 1,
        start: 0,
        length: 0,
        entries: vec![SuggestionEntry {
            text: "help".to_string(),
            tooltip: Some(SuggestionTooltip::Component(ComponentNbt)),
        }],
    };
    assert_eq!(
        command_suggestions_response_payload(&response),
        Err(WireError::TooltipNotImplemented {
            text: "help".to_string()
        })
    );
    // The same response without the tooltip serializes, so the refusal is the tooltip and not
    // something else about the packet.
    let mut plain = response.clone();
    plain.entries[0].tooltip = None;
    assert!(command_suggestions_response_payload(&plain).is_ok());
}

// ---------------------------------------------------------------------------------------
// Packet primitives
// ---------------------------------------------------------------------------------------

#[test]
fn a_negative_varint_is_five_bytes_and_round_trips() {
    // -1 is the unsigned 0xFFFFFFFF, so seven bits per byte for five bytes: ff ff ff ff 0f.
    let request = CommandSuggestionsRequest::new(-1, "");
    assert_eq!(
        command_suggestions_request_payload(&request).unwrap(),
        hex("ff ff ff ff 0f 00")
    );
    assert_eq!(
        parse_command_suggestions_request(&hex("ff ff ff ff 0f 00")),
        Ok(CommandSuggestionsRequest::new(-1, ""))
    );
    // i32::MIN is the widest negative the protocol can carry.
    let request = CommandSuggestionsRequest::new(i32::MIN, "");
    let payload = command_suggestions_request_payload(&request).unwrap();
    assert_eq!(payload.len(), 6);
    assert_eq!(parse_command_suggestions_request(&payload), Ok(request));
}

#[test]
fn a_resource_location_keeps_vanillas_character_rules() {
    assert_eq!(
        ResourceLocation::parse("minecraft:ask_server")
            .unwrap()
            .to_string(),
        "minecraft:ask_server"
    );
    // No namespace means `minecraft`, as `ResourceLocation.tryParse` does.
    assert_eq!(ResourceLocation::parse("foo").unwrap().to_string(), "minecraft:foo");
    for bad in ["", ":", "a:", ":b", "a:b:c", "Minecraft:foo", "a:b c", "a:b!"] {
        assert_eq!(
            ResourceLocation::parse(bad),
            Err(WireError::InvalidResourceLocation {
                text: bad.to_string()
            }),
            "'{bad}' should be rejected"
        );
    }
    // Paths may contain '/' but namespaces may not.
    assert!(ResourceLocation::parse("a:b/c_d.e-f").is_ok());
    assert!(ResourceLocation::parse("a/b:c").is_err());
}

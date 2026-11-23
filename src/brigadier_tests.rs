//! The parts of Brigadier's own test suite that pin behaviour, ported to this module.
//!
//! Every case here asserts a value a client would see - a cursor position, a range, the number
//! of merged context nodes, the exact usage line, the exact exception message - rather than only
//! `is_ok`/`is_err`. The Java originals are `StringReaderTest`, `CommandDispatcherTest`,
//! `CommandDispatcherUsagesTest`, `CommandSuggestionsTest`, `SuggestionsBuilderTest`,
//! `SuggestionsTest`, `AbstractCommandNodeTest` and the argument-type tests; each Rust case names
//! the behaviour it pins, and two of them are the anchors of the mutation entries in
//! `tools/mutation_check.sh`.

use crate::brigadier::*;

#[derive(Clone, Debug, PartialEq)]
struct Source {
    allowed: bool,
}

fn allowed() -> Source {
    Source { allowed: true }
}

fn denied() -> Source {
    Source { allowed: false }
}

/// A source that carries a number, for the redirect-modifier arithmetic the Java suite uses.
type IntSource = i32;

// ---------------------------------------------------------------------------------------
// StringReader
// ---------------------------------------------------------------------------------------

#[test]
fn reader_cursor_and_peek_basics() {
    let mut reader = StringReader::new("abc");
    assert!(reader.can_read());
    assert!(reader.can_read_n(3));
    assert!(!reader.can_read_n(4));
    assert_eq!(reader.peek(), Some('a'));
    assert_eq!(reader.peek_at(2), Some('c'));
    assert_eq!(reader.remaining_length(), 3);
    assert_eq!(reader.total_length(), 3);
    reader.skip();
    reader.skip();
    assert_eq!(reader.cursor(), 2);
    assert_eq!(reader.read(), Some('c'));
    assert_eq!(reader.cursor(), 3);
    assert!(!reader.can_read());
    assert_eq!(reader.peek(), None);
    assert_eq!(reader.read_text(), "abc");
    assert_eq!(reader.remaining(), "");
}

#[test]
fn reader_skip_whitespace_counts_every_space_tab_and_newline() {
    let mut reader = StringReader::new(" \t \t\nHello!");
    reader.skip_whitespace();
    assert_eq!(reader.cursor(), 5);
    assert_eq!(reader.remaining(), "Hello!");

    let mut none = StringReader::new("Hello!");
    none.skip_whitespace();
    assert_eq!(none.cursor(), 0);

    // U+00A0 is not whitespace to the Java reader, so it must not be skipped here either.
    let mut nbsp = StringReader::new("\u{a0}Hello!");
    nbsp.skip_whitespace();
    assert_eq!(nbsp.cursor(), 0);
}

#[test]
fn reader_reads_unquoted_words_up_to_the_separator() {
    let mut reader = StringReader::new("hello world");
    assert_eq!(reader.read_unquoted_string(), "hello");
    assert_eq!(reader.read_text(), "hello");
    assert_eq!(reader.remaining(), " world");

    let mut empty = StringReader::new("");
    assert_eq!(empty.read_unquoted_string(), "");
    assert_eq!(empty.read_text(), "");
    assert_eq!(empty.remaining(), "");

    // A leading space is not part of an unquoted word.
    let mut leading = StringReader::new(" hello world");
    assert_eq!(leading.read_unquoted_string(), "");
    assert_eq!(leading.read_text(), "");
    assert_eq!(leading.remaining(), " hello world");
}

#[test]
fn reader_quoted_strings_unescape_only_the_terminator_and_the_escape() {
    let mut double = StringReader::new("\"hello \\\"world\\\"\"");
    assert_eq!(double.read_quoted_string().unwrap(), "hello \"world\"");
    assert_eq!(double.read_text(), "\"hello \\\"world\\\"\"");
    assert_eq!(double.remaining(), "");

    let mut escaped_escape = StringReader::new("\"\\\\o/\"");
    assert_eq!(escaped_escape.read_quoted_string().unwrap(), "\\o/");

    let mut single = StringReader::new("'hello world'");
    assert_eq!(single.read_quoted_string().unwrap(), "hello world");

    let mut mixed = StringReader::new("'hello \"world\"'");
    assert_eq!(mixed.read_quoted_string().unwrap(), "hello \"world\"");

    let mut empty = StringReader::new("\"\" hello world");
    assert_eq!(empty.read_quoted_string().unwrap(), "");
    assert_eq!(empty.read_text(), "\"\"");
    assert_eq!(empty.remaining(), " hello world");

    // `readString` takes the quoted branch only when the next character opens a quote.
    let mut phrase = StringReader::new("\"hello world\"foo bar");
    assert_eq!(phrase.read_string().unwrap(), "hello world");
    assert_eq!(phrase.remaining(), "foo bar");
    let mut word = StringReader::new("hello world");
    assert_eq!(word.read_string().unwrap(), "hello");
    assert_eq!(word.remaining(), " world");
}

#[test]
fn reader_quoted_string_errors_report_the_type_and_the_cursor() {
    let mut no_open = StringReader::new("hello world\"");
    let error = no_open.read_quoted_string().unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderExpectedStartOfQuote);
    assert_eq!(error.cursor(), 0);
    assert_eq!(error.raw_message(), "Expected quote to start a string");

    let mut no_close = StringReader::new("\"hello world");
    let error = no_close.read_quoted_string().unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderExpectedEndOfQuote);
    assert_eq!(error.cursor(), 12);
    assert_eq!(error.raw_message(), "Unclosed quoted string");

    let mut invalid_escape = StringReader::new("\"hello\\nworld\"");
    let error = invalid_escape.read_quoted_string().unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderInvalidEscape);
    assert_eq!(error.cursor(), 7);
    assert_eq!(
        error.raw_message(),
        "Invalid escape sequence 'n' in quoted string"
    );

    // Escaping the other quote inside a single-quoted string is invalid in Brigadier too.
    let mut wrong_quote = StringReader::new("'hello\\\"'world");
    let error = wrong_quote.read_quoted_string().unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderInvalidEscape);
    assert_eq!(error.cursor(), 7);
}

#[test]
fn reader_numbers_parse_their_values_and_reset_the_cursor_when_invalid() {
    let mut integer = StringReader::new("1234567890 foo bar");
    assert_eq!(integer.read_int().unwrap(), 1_234_567_890);
    assert_eq!(integer.read_text(), "1234567890");
    assert_eq!(integer.remaining(), " foo bar");

    let mut negative = StringReader::new("-1234567890");
    assert_eq!(negative.read_int().unwrap(), -1_234_567_890);

    // The number scan stops at the first character that cannot be part of a number.
    let mut immediate = StringReader::new("1234567890foo bar");
    assert_eq!(immediate.read_int().unwrap(), 1_234_567_890);
    assert_eq!(immediate.remaining(), "foo bar");

    let mut invalid_int = StringReader::new("12.34");
    let error = invalid_int.read_int().unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderInvalidInt);
    assert_eq!(error.cursor(), 0);
    assert_eq!(error.raw_message(), "Invalid integer '12.34'");
    assert_eq!(invalid_int.cursor(), 0, "a failed number read rewinds");

    let mut expected_int = StringReader::new("");
    let error = expected_int.read_int().unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderExpectedInt);
    assert_eq!(error.cursor(), 0);

    let mut long = StringReader::new("1234567890");
    assert_eq!(long.read_long().unwrap(), 1_234_567_890i64);

    let mut invalid_long = StringReader::new("12.34");
    assert_eq!(
        invalid_long.read_long().unwrap_err().kind(),
        CommandExceptionType::ReaderInvalidLong
    );

    let mut double = StringReader::new("12.34 foo bar");
    assert_eq!(double.read_double().unwrap(), 12.34);
    assert_eq!(double.read_text(), "12.34");
    assert_eq!(double.remaining(), " foo bar");

    let mut invalid_double = StringReader::new("12.34.56");
    let error = invalid_double.read_double().unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderInvalidDouble);
    assert_eq!(error.cursor(), 0);

    let mut float = StringReader::new("12.34");
    assert_eq!(float.read_float().unwrap(), 12.34f32);
    let mut invalid_float = StringReader::new("12.34.56");
    assert_eq!(
        invalid_float.read_float().unwrap_err().kind(),
        CommandExceptionType::ReaderInvalidFloat
    );

    // A leading-dot decimal is a number in both implementations.
    let mut leading_dot = StringReader::new(".5");
    assert_eq!(leading_dot.read_double().unwrap(), 0.5);
}

#[test]
fn reader_booleans_and_expect_report_their_errors() {
    let mut yes = StringReader::new("true");
    assert!(yes.read_boolean().unwrap());
    assert_eq!(yes.read_text(), "true");
    let mut no = StringReader::new("false");
    assert!(!no.read_boolean().unwrap());

    let mut invalid = StringReader::new("tuesday");
    let error = invalid.read_boolean().unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderInvalidBool);
    assert_eq!(error.cursor(), 0);
    assert_eq!(
        error.raw_message(),
        "Invalid bool, expected true or false but found 'tuesday'"
    );
    assert_eq!(invalid.cursor(), 0);

    let mut empty = StringReader::new("");
    assert_eq!(
        empty.read_boolean().unwrap_err().kind(),
        CommandExceptionType::ReaderExpectedBool
    );

    let mut abc = StringReader::new("abc");
    abc.expect('a').unwrap();
    assert_eq!(abc.cursor(), 1);

    let mut bcd = StringReader::new("bcd");
    let error = bcd.expect('a').unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderExpectedSymbol);
    assert_eq!(error.cursor(), 0);
    assert_eq!(error.raw_message(), "Expected 'a'");
}

#[test]
fn exception_message_renders_the_position_and_the_here_marker() {
    let mut reader = StringReader::new("12.34");
    let error = reader.read_int().unwrap_err();
    assert_eq!(
        error.full_message(),
        "Invalid integer '12.34' at position 0: <--[HERE]"
    );

    // Past the ten-character window Java prefixes the context with "...".
    let mut long = StringReader::new("abcdefghijklmno");
    long.set_cursor(13);
    let error = BUILT_IN_EXCEPTIONS
        .reader_expected_int()
        .create_with_context(&long);
    assert_eq!(error.cursor(), 13);
    assert_eq!(error.context().unwrap(), "...defghijklm<--[HERE]");

    // `create()` carries no position and therefore no suffix.
    let bare = BUILT_IN_EXCEPTIONS.reader_expected_bool().create();
    assert_eq!(bare.cursor(), -1);
    assert_eq!(bare.full_message(), "Expected bool");
    assert_eq!(bare.context(), None);
}

#[test]
fn exception_message_keeps_java_decimal_formatting_for_doubles() {
    let mut reader = StringReader::new("0.5");
    let error = DoubleArgumentType::double_at_least(1.0)
        .parse_with_source(&mut reader, &allowed())
        .unwrap_err();
    assert_eq!(
        error.raw_message(),
        "Double must not be less than 1.0, found 0.5"
    );
    assert_eq!(error.cursor(), 0);
}

// ---------------------------------------------------------------------------------------
// Tree: children, merging, ordering
// ---------------------------------------------------------------------------------------

#[test]
fn add_child_merges_by_name_instead_of_duplicating() {
    let node = CommandNode::<Source>::root();
    node.add_child(CommandNode::literal("child1")).unwrap();
    node.add_child(CommandNode::literal("child2")).unwrap();
    node.add_child(CommandNode::literal("child1")).unwrap();

    assert_eq!(node.child_count(), 2);
    let names: Vec<String> = node
        .children()
        .iter()
        .map(|child| child.name().to_string())
        .collect();
    assert_eq!(names, vec!["child1".to_string(), "child2".to_string()]);
}

#[test]
fn add_child_merges_grandchildren_into_the_existing_child() {
    let node = CommandNode::<Source>::root();
    node.add_child(
        ArgumentBuilder::literal("child")
            .then(ArgumentBuilder::literal("grandchild1"))
            .build(),
    )
    .unwrap();
    node.add_child(
        ArgumentBuilder::literal("child")
            .then(ArgumentBuilder::literal("grandchild2"))
            .build(),
    )
    .unwrap();

    assert_eq!(node.child_count(), 1);
    let child = node.child("child").expect("merged child");
    assert_eq!(child.child_count(), 2);
    assert!(child.child("grandchild1").is_some());
    assert!(child.child("grandchild2").is_some());
}

#[test]
fn add_child_keeps_an_existing_command_and_takes_a_newer_one() {
    let marker = |value: i32| {
        move |_context: &CommandContext<Source>| Ok(value)
    };

    let node = CommandNode::<Source>::root();
    node.add_child(
        ArgumentBuilder::literal("child")
            .executes(marker(1))
            .build(),
    )
    .unwrap();
    node.add_child(ArgumentBuilder::literal("child").build())
        .unwrap();
    let dispatcher = CommandDispatcher::with_root(node.clone());
    assert_eq!(
        dispatcher.execute("child", allowed()).unwrap(),
        1,
        "a child without a command must not erase the one already merged"
    );

    node.add_child(
        ArgumentBuilder::literal("child")
            .executes(marker(2))
            .build(),
    )
    .unwrap();
    assert_eq!(
        dispatcher.execute("child", allowed()).unwrap(),
        2,
        "a child with a command replaces the merged one"
    );
}

#[test]
fn a_root_node_cannot_become_a_child() {
    let node = CommandNode::<Source>::root();
    assert_eq!(
        node.add_child(CommandNode::root()),
        Err(TreeError::RootAsChild)
    );
}

#[test]
fn literals_sort_before_arguments_then_by_their_key() {
    let literal_b = CommandNode::<Source>::literal("b");
    let literal_a = CommandNode::<Source>::literal("a");
    let argument = CommandNode::argument("a", Box::new(IntegerArgumentType::integer()));

    assert_eq!(literal_a.compare_to(&literal_b), std::cmp::Ordering::Less);
    assert_eq!(
        literal_b.compare_to(&argument),
        std::cmp::Ordering::Less,
        "a literal sorts before an argument even when the argument's name sorts first"
    );
    assert_eq!(
        argument.compare_to(&literal_a),
        std::cmp::Ordering::Greater
    );
    // Vanilla orders the Commands packet's children by this key.
    assert_eq!(argument.usage_text(), "<a>");
    assert_eq!(literal_a.usage_text(), "a");
    assert_eq!(literal_a.sorted_key(), "a");
}

// ---------------------------------------------------------------------------------------
// Dispatcher: parse, execute, requires
// ---------------------------------------------------------------------------------------

#[test]
fn execute_runs_the_registered_command_and_returns_its_result() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(ArgumentBuilder::literal("foo").executes(|_| Ok(42)))
        .unwrap();
    assert_eq!(dispatcher.execute("foo", allowed()).unwrap(), 42);

    // Vanilla parses "/foo" from offset 1.
    let mut reader = StringReader::new("/foo");
    reader.set_cursor(1);
    assert_eq!(dispatcher.execute_reader(reader, allowed()).unwrap(), 42);
}

#[test]
fn registering_the_same_literal_twice_merges_both_subtrees() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(ArgumentBuilder::literal("base").then(
            ArgumentBuilder::literal("foo").executes(|_| Ok(42)),
        ))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("base").then(
            ArgumentBuilder::literal("bar").executes(|_| Ok(7)),
        ))
        .unwrap();

    assert_eq!(dispatcher.execute("base foo", allowed()).unwrap(), 42);
    assert_eq!(dispatcher.execute("base bar", allowed()).unwrap(), 7);
    let base = dispatcher.root().child("base").expect("merged base");
    assert_eq!(base.child_count(), 2);
}

#[test]
fn unknown_command_reports_position_zero() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(ArgumentBuilder::<Source>::literal("bar"))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::<Source>::literal("baz"))
        .unwrap();

    let error = dispatcher.execute("foo", allowed()).unwrap_err();
    assert_eq!(
        error.kind(),
        CommandExceptionType::DispatcherUnknownCommand
    );
    assert_eq!(error.cursor(), 0);
    assert_eq!(error.raw_message(), "Unknown command");
}

#[test]
fn a_denied_requirement_makes_the_command_look_unknown() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("foo")
                .requires(|source: &Source| source.allowed)
                .executes(|_| Ok(1)),
        )
        .unwrap();

    assert_eq!(dispatcher.execute("foo", allowed()).unwrap(), 1);
    let error = dispatcher.execute("foo", denied()).unwrap_err();
    assert_eq!(
        error.kind(),
        CommandExceptionType::DispatcherUnknownCommand
    );
    assert_eq!(error.cursor(), 0);
}

#[test]
fn unknown_subcommand_reports_the_position_after_the_separator() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(ArgumentBuilder::literal("foo").executes(|_| Ok(42)))
        .unwrap();

    let error = dispatcher.execute("foo bar", allowed()).unwrap_err();
    assert_eq!(
        error.kind(),
        CommandExceptionType::DispatcherUnknownArgument
    );
    assert_eq!(error.cursor(), 4);
    assert_eq!(error.raw_message(), "Incorrect argument for command");
}

#[test]
fn a_missing_separator_makes_the_whole_command_unknown() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("foo")
                .then(ArgumentBuilder::argument(
                    "bar",
                    Box::new(IntegerArgumentType::integer()),
                ))
                .executes(|_| Ok(42)),
        )
        .unwrap();

    let error = dispatcher.execute("foo$", allowed()).unwrap_err();
    assert_eq!(
        error.kind(),
        CommandExceptionType::DispatcherUnknownCommand
    );
    assert_eq!(error.cursor(), 0);
}

#[test]
fn an_argument_syntax_error_is_the_one_reported() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("foo")
                .then(ArgumentBuilder::argument(
                    "bar",
                    Box::new(IntegerArgumentType::integer()),
                ))
                .executes(|_| Ok(42)),
        )
        .unwrap();

    let error = dispatcher.execute("foo bar", allowed()).unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::ReaderExpectedInt);
    assert_eq!(error.cursor(), 4);
}

#[test]
fn an_incomplete_literal_or_argument_leaves_the_separator_unread() {
    let literal = CommandDispatcher::<Source>::new();
    literal
        .register(
            ArgumentBuilder::literal("foo").then(
                ArgumentBuilder::literal("bar").executes(|_| Ok(42)),
            ),
        )
        .unwrap();
    let parse = literal.parse("foo ", allowed());
    assert_eq!(parse.reader().remaining(), " ");
    assert_eq!(parse.context().nodes().len(), 1);

    let argument = CommandDispatcher::<Source>::new();
    argument
        .register(
            ArgumentBuilder::literal("foo").then(
                ArgumentBuilder::argument("bar", Box::new(IntegerArgumentType::integer()))
                    .executes(|_| Ok(42)),
            ),
        )
        .unwrap();
    let parse = argument.parse("foo ", allowed());
    assert_eq!(parse.reader().remaining(), " ");
    assert_eq!(parse.context().nodes().len(), 1);
}

#[test]
fn the_deepest_matching_child_wins_between_two_arguments() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("test")
                .then(
                    ArgumentBuilder::argument("incorrect", Box::new(IntegerArgumentType::integer()))
                        .executes(|_| Ok(1)),
                )
                .then(
                    ArgumentBuilder::argument("right", Box::new(IntegerArgumentType::integer())).then(
                        ArgumentBuilder::argument("sub", Box::new(IntegerArgumentType::integer()))
                            .executes(|_| Ok(100)),
                    ),
                ),
        )
        .unwrap();

    assert_eq!(dispatcher.execute("test 1 2", allowed()).unwrap(), 100);
    let parse = dispatcher.parse("test 1 2", allowed());
    assert_eq!(parse.context().nodes().len(), 3);
    assert_eq!(parse.context().nodes()[1].node().name(), "right");
    assert_eq!(parse.context().nodes()[2].node().name(), "sub");
}

// ---------------------------------------------------------------------------------------
// Redirects, forks and context chains
// ---------------------------------------------------------------------------------------

#[test]
fn a_redirect_chain_builds_one_context_per_hop_with_its_own_range() {
    let dispatcher = CommandDispatcher::<Source>::new();
    let concrete = dispatcher
        .register(ArgumentBuilder::literal("actual").executes(|_| Ok(42)))
        .unwrap();
    let redirected = dispatcher
        .register(ArgumentBuilder::literal("redirected").redirect(dispatcher.root().clone()))
        .unwrap();

    let input = "redirected redirected actual";
    let parse = dispatcher.parse(input, allowed());
    let outer = parse.context();

    assert_eq!(outer.range().get(input), "redirected");
    assert_eq!(outer.nodes().len(), 1);
    assert!(outer.root_node().same_node(dispatcher.root()));
    assert_eq!(outer.nodes()[0].range(), outer.range());
    assert!(outer.nodes()[0].node().same_node(&redirected));

    let first = outer.child().expect("first redirect context");
    assert_eq!(first.range().get(input), "redirected");
    assert_eq!(first.nodes().len(), 1);
    assert!(first.root_node().same_node(dispatcher.root()));
    assert_eq!(first.nodes()[0].range(), first.range());
    assert!(first.nodes()[0].node().same_node(&redirected));

    let second = first.child().expect("second redirect context");
    assert_eq!(second.range().get(input), "actual");
    assert_eq!(second.nodes().len(), 1);
    assert!(second.root_node().same_node(dispatcher.root()));
    assert_eq!(second.nodes()[0].range(), second.range());
    assert!(second.nodes()[0].node().same_node(&concrete));

    let built = outer.clone().build(input);
    let chain = ContextChain::try_flatten(&built)
        .expect("a redirect chain that ends in an executable");
    assert_eq!(chain.modifier_count(), 2);
    assert_eq!(dispatcher.execute_parse(parse).unwrap(), 42);
}

#[test]
fn a_redirect_modifier_threads_the_modified_source_through_every_hop() {
    let dispatcher = CommandDispatcher::<IntSource>::new();
    let root = dispatcher.root().clone();
    dispatcher
        .register(ArgumentBuilder::literal("add").then(
            ArgumentBuilder::argument("value", Box::new(IntegerArgumentType::integer())).redirect_single(
                root.clone(),
                |context| context.source() + IntegerArgumentType::get_integer(context, "value").unwrap(),
            ),
        ))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("blank").redirect(root.clone()))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("run").executes(|context| Ok(*context.source())))
        .unwrap();

    assert_eq!(dispatcher.execute("run", 0).unwrap(), 0);
    assert_eq!(dispatcher.execute("run", 1).unwrap(), 1);
    assert_eq!(dispatcher.execute("add 5 run", 1).unwrap(), 6);
    assert_eq!(dispatcher.execute("add 5 add 6 run", 2).unwrap(), 13);
    assert_eq!(dispatcher.execute("add 5 blank run", 1).unwrap(), 6);
    assert_eq!(dispatcher.execute("blank add 5 run", 1).unwrap(), 6);
    assert_eq!(dispatcher.execute("add 5 blank add 6 run", 2).unwrap(), 13);
    assert_eq!(dispatcher.execute("add 5 blank blank add 6 run", 2).unwrap(), 13);
}

#[test]
fn a_fork_runs_the_command_once_per_source_and_counts_them() {
    let dispatcher = CommandDispatcher::<IntSource>::new();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_by_command = seen.clone();
    dispatcher
        .register(ArgumentBuilder::literal("run").executes(move |context| {
            seen_by_command.lock().unwrap_or_else(|e| e.into_inner()).push(*context.source());
            Ok(*context.source())
        }))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("split").fork(dispatcher.root().clone(), |_| {
            Ok(vec![9, 10, 11])
        }))
        .unwrap();

    assert_eq!(dispatcher.execute("split run", 0).unwrap(), 3);
    assert_eq!(*seen.lock().unwrap_or_else(|e| e.into_inner()), vec![9, 10, 11]);
}

#[test]
fn a_forked_failure_is_counted_as_zero_rather_than_aborting() {
    let dispatcher = CommandDispatcher::<IntSource>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("run").executes(|context| match *context.source() {
                10 => Err(BUILT_IN_EXCEPTIONS.reader_expected_bool().create()),
                other => Ok(other),
            }),
        )
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("split").fork(dispatcher.root().clone(), |_| {
            Ok(vec![9, 10, 11])
        }))
        .unwrap();

    assert_eq!(
        dispatcher.execute("split run", 0).unwrap(),
        2,
        "9 and 11 succeed, 10 fails and is dropped"
    );
}

#[test]
fn a_non_forked_failure_propagates_the_original_exception() {
    let dispatcher = CommandDispatcher::<IntSource>::new();
    let thrown = BUILT_IN_EXCEPTIONS.reader_expected_bool().create();
    let thrown_by_command = thrown.clone();
    dispatcher
        .register(
            ArgumentBuilder::literal("crash")
                .executes(move |_| Err(thrown_by_command.clone())),
        )
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("redirect").redirect(dispatcher.root().clone()))
        .unwrap();

    let error = dispatcher.execute("redirect crash", 0).unwrap_err();
    assert_eq!(error, thrown);
    assert_eq!(error.cursor(), -1);
}

#[test]
fn an_empty_fork_result_executes_nothing_and_returns_zero() {
    let dispatcher = CommandDispatcher::<IntSource>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("foo").then(
                ArgumentBuilder::literal("bar").then(
                    ArgumentBuilder::argument("value", Box::new(IntegerArgumentType::integer()))
                        .executes(|context| {
                            Ok(IntegerArgumentType::get_integer(context, "value")
                                .expect("the argument was parsed"))
                        }),
                ),
            ),
        )
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("baz").fork(dispatcher.root().child("foo").unwrap(), |_| {
            Ok(Vec::new())
        }))
        .unwrap();

    assert_eq!(dispatcher.execute("baz bar 100", 0).unwrap(), 0);
}

#[test]
fn a_result_consumer_is_told_about_every_successful_run() {
    let dispatcher = CommandDispatcher::<IntSource>::new();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log_for_consumer = log.clone();
    let mut dispatcher = dispatcher;
    dispatcher.set_consumer(std::sync::Arc::new(move |context, success, result| {
        log_for_consumer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((*context.source(), success, result));
    }));
    dispatcher
        .register(ArgumentBuilder::literal("run").executes(|context| Ok(*context.source())))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("split").fork(dispatcher.root().clone(), |_| {
            Ok(vec![9, 10, 11])
        }))
        .unwrap();

    assert_eq!(dispatcher.execute("split run", 0).unwrap(), 3);
    assert_eq!(
        *log.lock().unwrap_or_else(|e| e.into_inner()),
        vec![(9, true, 9), (10, true, 10), (11, true, 11)]
    );

    // A failure is reported as such, and in non-forked mode still propagates.
    log.lock().unwrap_or_else(|e| e.into_inner()).clear();
    dispatcher
        .register(
            ArgumentBuilder::literal("crash")
                .executes(|_| Err(BUILT_IN_EXCEPTIONS.reader_expected_bool().create())),
        )
        .unwrap();
    assert!(dispatcher.execute("crash", 4).is_err());
    assert_eq!(*log.lock().unwrap_or_else(|e| e.into_inner()), vec![(4, false, 0)]);
}

#[test]
fn a_redirect_that_does_not_reach_a_command_is_an_unknown_command() {
    let dispatcher = CommandDispatcher::<Source>::new();
    let foo = dispatcher
        .register(
            ArgumentBuilder::literal("foo")
                .then(
                    ArgumentBuilder::literal("bar").then(
                        ArgumentBuilder::argument("value", Box::new(IntegerArgumentType::integer()))
                            .executes(|context| {
                                Ok(IntegerArgumentType::get_integer(context, "value")
                                    .expect("the argument was parsed"))
                            }),
                    ),
                )
                .then(ArgumentBuilder::literal("awa").executes(|_| Ok(2))),
        )
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("baz").redirect(foo))
        .unwrap();

    let error = dispatcher.execute("baz bar", allowed()).unwrap_err();
    assert_eq!(
        error.kind(),
        CommandExceptionType::DispatcherUnknownCommand
    );
}

// ---------------------------------------------------------------------------------------
// Suggestions
// ---------------------------------------------------------------------------------------

fn texts(suggestions: &Suggestions) -> Vec<String> {
    suggestions
        .list()
        .iter()
        .map(|suggestion| suggestion.text().to_string())
        .collect()
}

#[test]
fn completion_lists_root_commands_with_an_empty_input() {
    let dispatcher = CommandDispatcher::<Source>::new();
    for name in ["foo", "bar", "baz"] {
        dispatcher
            .register(ArgumentBuilder::<Source>::literal(name))
            .unwrap();
    }

    let parse = dispatcher.parse("", allowed());
    let suggestions = dispatcher.get_completion_suggestions(&parse);
    assert_eq!(suggestions.range(), StringRange::at(0));
    assert_eq!(texts(&suggestions), vec!["bar", "baz", "foo"]);
}

#[test]
fn completion_narrows_to_the_partial_word_and_reports_its_range() {
    let dispatcher = CommandDispatcher::<Source>::new();
    for name in ["foo", "bar", "baz"] {
        dispatcher
            .register(ArgumentBuilder::<Source>::literal(name))
            .unwrap();
    }

    let parse = dispatcher.parse("b", allowed());
    let suggestions = dispatcher.get_completion_suggestions(&parse);
    assert_eq!(suggestions.range(), StringRange::between(0, 1));
    assert_eq!(texts(&suggestions), vec!["bar", "baz"]);

    // A cursor offset is honoured, exactly as the client's partial command is.
    let mut offset = StringReader::new("Zb");
    offset.set_cursor(1);
    let parse = dispatcher.parse_reader(offset, allowed());
    let suggestions = dispatcher.get_completion_suggestions(&parse);
    assert_eq!(suggestions.range(), StringRange::between(1, 2));
    assert_eq!(texts(&suggestions), vec!["bar", "baz"]);
}

#[test]
fn completion_after_a_separator_lists_the_children_at_that_position() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("parent")
                .then(ArgumentBuilder::literal("foo"))
                .then(ArgumentBuilder::literal("bar"))
                .then(ArgumentBuilder::literal("baz")),
        )
        .unwrap();

    let parse = dispatcher.parse("parent ", allowed());
    let suggestions = dispatcher.get_completion_suggestions(&parse);
    assert_eq!(suggestions.range(), StringRange::at(7));
    assert_eq!(texts(&suggestions), vec!["bar", "baz", "foo"]);
}

#[test]
fn completion_for_a_bool_argument_offers_both_spellings() {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("flag").then(ArgumentBuilder::argument(
                "value",
                Box::new(BoolArgumentType::bool()),
            )),
        )
        .unwrap();

    let parse = dispatcher.parse("flag ", allowed());
    let suggestions = dispatcher.get_completion_suggestions(&parse);
    assert_eq!(texts(&suggestions), vec!["false", "true"]);
}

#[test]
fn suggestion_builder_replaces_the_partial_word_and_sorts_like_java() {
    let mut builder = SuggestionsBuilder::new("Hello w", 6);
    let single = builder.suggest("world!").build();
    assert_eq!(
        single.list(),
        [Suggestion::new(StringRange::between(6, 7), "world!")]
    );
    assert_eq!(single.range(), StringRange::between(6, 7));

    // Suggesting exactly what is already typed is a no-op.
    let mut complete = SuggestionsBuilder::new("Hello w", 6);
    assert!(complete.suggest("w").build().is_empty());

    // Integer suggestions sort among themselves by value, not by text; text sorts
    // case-insensitively. A list that mixes the two is *not* totally ordered (Java's
    // IntegerSuggestion.compareTo compares numerically with an integer and textually with a
    // string, and "4" > "30" as text while 4 < 30 as numbers), so the exact interleaving is
    // whatever the sort algorithm produces. Java's TimSort and Rust's stable sort disagree
    // there; this pins the order inside each kind, which is what a suggestion list can promise.
    let mut mixed = SuggestionsBuilder::new("Hello w", 6);
    mixed
        .suggest("11")
        .suggest("22")
        .suggest("33")
        .suggest("a")
        .suggest("b")
        .suggest("c")
        .suggest_int(2)
        .suggest_int(4)
        .suggest_int(6)
        .suggest_int(8)
        .suggest_int(30)
        .suggest_int(32)
        .suggest("3a")
        .suggest("a3");
    let built = mixed.build();
    let integers: Vec<i32> = built
        .list()
        .iter()
        .filter_map(|suggestion| suggestion.integer_value())
        .collect();
    assert_eq!(integers, vec![2, 4, 6, 8, 30, 32]);
    let words: Vec<String> = built
        .list()
        .iter()
        .filter(|suggestion| suggestion.integer_value().is_none())
        .map(|suggestion| suggestion.text().to_string())
        .collect();
    assert_eq!(words, vec!["11", "22", "33", "3a", "a", "a3", "b", "c"]);
    assert_eq!(built.list().len(), 14);

    let mut textual = SuggestionsBuilder::new("Hello w", 6);
    textual.suggest("2").suggest("30").suggest("4");
    assert_eq!(texts(&textual.build()), vec!["2", "30", "4"]);
}

#[test]
fn merging_suggestions_expands_each_range_to_the_merged_one() {
    let a = Suggestions::new(
        StringRange::at(5),
        vec![
            Suggestion::new(StringRange::at(5), "ar"),
            Suggestion::new(StringRange::at(5), "az"),
            Suggestion::new(StringRange::at(5), "Az"),
        ],
    );
    let b = Suggestions::new(
        StringRange::between(4, 5),
        vec![
            Suggestion::new(StringRange::between(4, 5), "foo"),
            Suggestion::new(StringRange::between(4, 5), "qux"),
            Suggestion::new(StringRange::between(4, 5), "apple"),
            Suggestion::new(StringRange::between(4, 5), "Bar"),
        ],
    );

    let merged = Suggestions::merge("foo b", &[a, b]);
    assert_eq!(merged.range(), StringRange::between(4, 5));
    assert_eq!(
        texts(&merged),
        vec!["apple", "bar", "Bar", "baz", "bAz", "foo", "qux"]
    );
    assert_eq!(texts(&Suggestions::merge("foo b", &[])), Vec::<String>::new());
}

#[test]
fn a_suggestion_replaces_only_its_own_range_in_the_input() {
    let suggestion = Suggestion::new(StringRange::between(4, 5), "bar");
    assert_eq!(suggestion.apply("foo b"), "foo bar");
    let whole = Suggestion::new(StringRange::between(0, 5), "baz");
    assert_eq!(whole.apply("foo b"), "baz");
}

// ---------------------------------------------------------------------------------------
// Argument types and contexts
// ---------------------------------------------------------------------------------------

#[test]
fn integer_argument_enforces_its_bounds_with_positions() {
    let mut too_low = StringReader::new("5");
    let error = IntegerArgumentType::integer_at_least(10)
        .parse_with_source(&mut too_low, &allowed())
        .unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::IntegerTooLow);
    assert_eq!(error.raw_message(), "Integer must not be less than 10, found 5");
    assert_eq!(error.cursor(), 0);

    let mut too_high = StringReader::new("500");
    let error = IntegerArgumentType::integer_between(0, 100)
        .parse_with_source(&mut too_high, &allowed())
        .unwrap_err();
    assert_eq!(error.kind(), CommandExceptionType::IntegerTooHigh);
    assert_eq!(error.raw_message(), "Integer must not be more than 100, found 500");

    let mut ok = StringReader::new("42");
    let value = IntegerArgumentType::integer_between(0, 100)
        .parse_with_source(&mut ok, &allowed())
        .unwrap();
    assert_eq!(value.get::<i32>(), Some(&42));
    assert_eq!(value.type_name(), "i32");
    assert_eq!(ok.cursor(), 2);
}

#[test]
fn string_argument_types_read_word_phrase_and_greedy() {
    let mut word = StringReader::new("hello world");
    assert_eq!(
        StringArgumentType::word()
            .parse_with_source(&mut word, &allowed())
            .unwrap()
            .get::<String>(),
        Some(&"hello".to_string())
    );

    let mut phrase = StringReader::new("\"hello world\" tail");
    assert_eq!(
        StringArgumentType::string()
            .parse_with_source(&mut phrase, &allowed())
            .unwrap()
            .get::<String>(),
        Some(&"hello world".to_string())
    );

    let mut greedy = StringReader::new("hello world tail");
    let value = StringArgumentType::greedy_string()
        .parse_with_source(&mut greedy, &allowed())
        .unwrap();
    assert_eq!(value.get::<String>(), Some(&"hello world tail".to_string()));
    assert_eq!(greedy.cursor(), greedy.total_length());
}

#[test]
fn escape_if_required_quotes_only_what_needs_it() {
    assert_eq!(StringArgumentType::escape_if_required("hello"), "hello");
    assert_eq!(StringArgumentType::escape_if_required("a_b-1.2+3"), "a_b-1.2+3");
    assert_eq!(
        StringArgumentType::escape_if_required("hello world"),
        "\"hello world\""
    );
    assert_eq!(
        StringArgumentType::escape_if_required("say \"hi\""),
        "\"say \\\"hi\\\"\""
    );
    // Java's loop over an empty string never runs, so the empty string comes back unquoted.
    assert_eq!(StringArgumentType::escape_if_required(""), "");
}

#[test]
fn the_context_hands_back_typed_arguments_by_name() {
    let dispatcher = CommandDispatcher::<Source>::new();
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let captured_by_command = captured.clone();
    dispatcher
        .register(
            ArgumentBuilder::literal("tp").then(
                ArgumentBuilder::argument("x", Box::new(IntegerArgumentType::integer())).then(
                    ArgumentBuilder::argument("who", Box::new(StringArgumentType::word())).executes(
                        move |context| {
                        *captured_by_command.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
                            "{}:{}",
                            IntegerArgumentType::get_integer(context, "x").expect("x was parsed"),
                            StringArgumentType::get_string(context, "who").expect("who was parsed")
                        ));
                        Ok(1)
                    },
                    ),
                ),
            ),
        )
        .unwrap();

    assert_eq!(dispatcher.execute("tp 5 alpha", allowed()).unwrap(), 1);
    assert_eq!(captured.lock().unwrap_or_else(|e| e.into_inner()).as_deref(), Some("5:alpha"));

    // The two `getArgument` failure cases keep Java's messages.
    let parse = dispatcher.parse("tp 5 alpha", allowed());
    let context = parse.context().build("tp 5 alpha");
    let missing = context.argument::<i32>("nope").unwrap_err();
    assert_eq!(
        missing,
        ArgumentError::NoSuchArgument("nope".to_string())
    );
    assert_eq!(
        missing.message(),
        "No such argument 'nope' exists on this command"
    );
    let wrong = context.argument::<String>("x").unwrap_err();
    assert_eq!(
        wrong.message(),
        "Argument 'x' is defined as i32, not alloc::string::String"
    );
    assert_eq!(context.argument::<i32>("x").copied().unwrap(), 5);
    // The argument ranges are the spans in the parsed input.
    assert_eq!(
        context.arguments()["x"].range(),
        StringRange::between(3, 4)
    );
}

#[test]
fn a_node_can_be_turned_back_into_an_equivalent_builder() {
    let dispatcher = CommandDispatcher::<Source>::new();
    let node = dispatcher
        .register(
            ArgumentBuilder::literal("foo")
                .requires(|_| true)
                .then(ArgumentBuilder::argument("x", Box::new(IntegerArgumentType::integer())))
                .executes(|_| Ok(3)),
        )
        .unwrap();

    // Java's createBuilder recreates the node without its children; the command, requirement,
    // redirect and argument type come across.
    let rebuilt = node.create_builder().build();
    assert_eq!(rebuilt.name(), "foo");
    assert_eq!(rebuilt.child_count(), 0);
    assert!(rebuilt.command().is_some());

    let argument = node.child("x").expect("the argument child");
    let rebuilt_argument = argument.create_builder().build();
    assert_eq!(rebuilt_argument.usage_text(), "<x>");
    let argument_type = rebuilt_argument.argument_type().expect("type survives");
    assert_eq!(argument_type.examples(), vec!["0", "123", "-123"]);
    let mut reader = StringReader::new("7");
    assert_eq!(
        argument_type.parse_with_source(&mut reader, &allowed()).unwrap().get::<i32>(),
        Some(&7)
    );
}

// ---------------------------------------------------------------------------------------
// Usage output (what the client's "usage" line shows)
// ---------------------------------------------------------------------------------------

fn usage_dispatcher() -> CommandDispatcher<Source> {
    let dispatcher = CommandDispatcher::<Source>::new();
    dispatcher
        .register(
            ArgumentBuilder::literal("a")
                .then(
                    ArgumentBuilder::literal("1")
                        .then(ArgumentBuilder::literal("i").executes(|_| Ok(0)))
                        .then(ArgumentBuilder::literal("ii").executes(|_| Ok(0))),
                )
                .then(
                    ArgumentBuilder::literal("2")
                        .then(ArgumentBuilder::literal("i").executes(|_| Ok(0)))
                        .then(ArgumentBuilder::literal("ii").executes(|_| Ok(0))),
                ),
        )
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("h").executes(|_| Ok(0)).then(
            ArgumentBuilder::literal("1").then(
                ArgumentBuilder::literal("i").executes(|_| Ok(0)),
            ),
        ).then(ArgumentBuilder::literal("2").then(
            ArgumentBuilder::literal("i").then(
                ArgumentBuilder::literal("ii").executes(|_| Ok(0)),
            ),
        )).then(ArgumentBuilder::literal("3").executes(|_| Ok(0))))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("j").redirect(dispatcher.root().clone()))
        .unwrap();
    let h = dispatcher.root().child("h").unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("k").redirect(h))
        .unwrap();
    dispatcher
        .register(
            ArgumentBuilder::literal("d")
                .requires(|_: &Source| false)
                .executes(|_| Ok(0)),
        )
        .unwrap();
    dispatcher
}

#[test]
fn all_usage_lists_every_executable_path_and_skips_denied_commands() {
    let dispatcher = usage_dispatcher();
    assert_eq!(
        dispatcher.get_all_usage(dispatcher.root(), &allowed(), true),
        vec![
            "a 1 i",
            "a 1 ii",
            "a 2 i",
            "a 2 ii",
            "h",
            "h 1 i",
            "h 2 i ii",
            "h 3",
            "j ...",
            "k -> h",
        ]
    );
    // `restricted = false` mentions the denied command too.
    assert!(dispatcher
        .get_all_usage(dispatcher.root(), &allowed(), false)
        .contains(&"d".to_string()));
}

#[test]
fn smart_usage_compresses_siblings_the_way_vanilla_shows_them() {
    let dispatcher = usage_dispatcher();
    let usage: Vec<String> = dispatcher
        .get_smart_usage(dispatcher.root(), &allowed())
        .into_iter()
        .map(|(_, usage)| usage)
        .collect();
    assert_eq!(
        usage,
        vec![
            "a (1|2)",
            "h [1|2|3]",
            "j ...",
            "k -> h",
        ]
    );

    let h = dispatcher.root().child("h").unwrap();
    let inside: Vec<String> = dispatcher
        .get_smart_usage(&h, &allowed())
        .into_iter()
        .map(|(_, usage)| usage)
        .collect();
    assert_eq!(inside, vec!["[1] i", "[2] i ii", "[3]"]);
}

#[test]
fn paths_and_node_lookup_round_trip() {
    let dispatcher = CommandDispatcher::<Source>::new();
    let bar = ArgumentBuilder::literal("bar").build();
    dispatcher
        .register(ArgumentBuilder::literal("foo").then_node(bar.clone()))
        .unwrap();

    assert_eq!(
        dispatcher.get_path(&bar),
        vec!["foo".to_string(), "bar".to_string()]
    );
    let found = dispatcher
        .find_node(&["foo".to_string(), "bar".to_string()])
        .expect("the path resolves");
    assert!(found.same_node(&bar));
    assert!(dispatcher.find_node(&["foo".to_string(), "nope".to_string()]).is_none());
}

// ---------------------------------------------------------------------------------------
// Reader units (the documented deviation, pinned so it cannot drift silently)
// ---------------------------------------------------------------------------------------

#[test]
fn a_quoted_multi_byte_character_advances_the_cursor_by_its_utf8_length() {
    let mut reader = StringReader::new("\"日本\"");
    assert_eq!(reader.read_quoted_string().unwrap(), "日本");
    // Six bytes for two characters, plus the two quotes.
    assert_eq!(reader.cursor(), 8);
    assert_eq!(reader.total_length(), 8);
    assert_eq!(reader.remaining(), "");
}
// ---------------------------------------------------------------------------------------
// Vanilla command tokens and entity selectors
// ---------------------------------------------------------------------------------------

#[test]
fn command_tokenizer_preserves_quoted_and_nested_argument_text() {
    let selector = r#"@e[type=minecraft:cow, name="Bob Smith",nbt={Items:[{name:'A, B [C]'}]}]"#;
    let message = r#""猫 \"says\" \\ hi""#;
    let single = r#"'it\'s fine'"#;
    let snbt = r#"{text:"a } b", list:[1, 2]}"#;
    let input = format!(" \t/command {selector}\t{message} {single} {snbt}  last\n");
    assert_eq!(
        split_command_args_preserving_quotes(&input).unwrap(),
        ["/command", selector, message, single, snbt, "last"]
    );
    assert_eq!(
        split_command_args_preserving_quotes("say \"\"\u{a0}word [] {}").unwrap(),
        ["say", "\"\"\u{a0}word", "[]", "{}"]
    );
    assert!(split_command_args_preserving_quotes(" \t\n")
        .unwrap()
        .is_empty());
}

#[test]
fn command_tokenizer_rejects_unclosed_or_crossed_delimiters() {
    for (input, error) in [
        ("say \"unfinished", "Unclosed quoted string"),
        (r#"say 'unfinished\"#, "Unclosed quoted string"),
        (r#"say "bad\q""#, "Invalid escape"),
        ("kill @e[type=cow", "Expected ']'"),
        ("data {foo:[1, 2]", "Expected '}'"),
        ("data {[}]", "Unexpected '}'"),
        ("kill @e]", "Unexpected ']'"),
    ] {
        let actual = split_command_args_preserving_quotes(input).unwrap_err();
        assert!(actual.contains(error), "{input:?} returned {actual}");
    }
}

fn parse_selector(input: &str) -> Result<SelectorSpec, String> {
    SelectorSpec::parse(input, |id| {
        matches!(
            id,
            "minecraft:player" | "minecraft:cow" | "minecraft:pig" | "minecraft:item"
        )
    })
}

fn selector_source() -> SelectorSource<'static> {
    SelectorSource {
        position: [0.0; 3],
        dimension: "minecraft:overworld",
        executor: None,
    }
}

fn candidate(id: i32, entity_type: &'static str, position: [f64; 3]) -> SelectorCandidate<'static> {
    SelectorCandidate {
        id,
        entity_type,
        position,
        bounds: Some([
            [position[0] - 0.3, position[1], position[2] - 0.3],
            [position[0] + 0.3, position[1] + 1.0, position[2] + 0.3],
        ]),
        dimension: "minecraft:overworld",
        alive: true,
        living: entity_type != "minecraft:item",
        ..SelectorCandidate::default()
    }
}

fn selector_ids<'a>(selected: Vec<&SelectorCandidate<'a>>) -> Vec<i32> {
    selected.into_iter().map(|entity| entity.id).collect()
}

fn selected_ids(
    selector: &str,
    source: &SelectorSource<'_>,
    candidates: &[SelectorCandidate<'_>],
) -> Vec<i32> {
    let selector = parse_selector(selector).unwrap();
    selector_ids(select_entities(
        &selector,
        source,
        candidates,
        |_, _| None,
        |end| end - 1,
    ))
}

#[test]
fn selector_variables_use_vanilla_defaults_and_executor() {
    let mut source = selector_source();
    source.executor = Some(4);
    let mut candidates = [
        candidate(1, "minecraft:player", [0.0, 9.0, 0.0]),
        candidate(2, "minecraft:cow", [0.0, 0.0, 0.0]),
        candidate(3, "minecraft:player", [2.0, 0.0, 0.0]),
        candidate(4, "minecraft:cow", [1.0, 0.0, 0.0]),
    ];
    candidates[3].alive = false;
    candidates[3].dimension = "minecraft:the_nether";
    for (selector, expected) in [
        ("@p", &[3][..]),
        ("@a", &[1, 3]),
        ("@r", &[1]),
        ("@e", &[1, 2, 3]),
        ("@n", &[2]),
        ("@s", &[4]),
        ("@s[x=1,dy=0,type=cow]", &[4]),
        ("@s[type=!cow]", &[]),
    ] {
        assert_eq!(
            selected_ids(selector, &source, &candidates),
            expected,
            "{selector}"
        );
    }
    source.executor = None;
    assert_eq!(selected_ids("@s", &source, &candidates), []);
}

#[test]
fn selector_sorting_is_stable_and_limits_apply_after_ordering() {
    let source = selector_source();
    let candidates = [
        candidate(1, "minecraft:player", [3.0, 0.0, 0.0]),
        candidate(2, "minecraft:player", [0.0, -1.0, 0.0]),
        candidate(3, "minecraft:player", [0.0, 0.0, 1.0]),
        candidate(4, "minecraft:player", [0.0, 6.0, 0.0]),
    ];
    for (selector, expected) in [
        ("@p[limit=2]", &[2, 3][..]),
        ("@a[sort=nearest,limit=3]", &[2, 3, 1]),
        ("@e[sort=furthest,limit=3]", &[4, 1, 2]),
        ("@n[sort=arbitrary,limit=2]", &[1, 2]),
        ("@e[sort=nearest,x=3,limit=1]", &[1]),
        ("@a[sort=nearest,limit=100]", &[2, 3, 1, 4]),
    ] {
        assert_eq!(
            selected_ids(selector, &source, &candidates),
            expected,
            "{selector}"
        );
    }

    let mut draws = Vec::new();
    let selector = parse_selector("@r[distance=..3,limit=2]").unwrap();
    let selected = select_entities(
        &selector,
        &source,
        &candidates,
        |_, _| None,
        |end| {
            draws.push(end);
            0
        },
    );
    assert_eq!(draws, [3, 2]);
    assert_eq!(selector_ids(selected), [2, 3]);
    let only_candidate = &candidates[..1];
    assert_eq!(
        selector_ids(select_entities(
            &selector,
            &source,
            only_candidate,
            |_, _| None,
            |_| { panic!("a one-element shuffle needs no draw") }
        )),
        [1]
    );
}

#[test]
fn selector_distance_ranges_include_bounds_and_use_all_axes() {
    let source = selector_source();
    let mut candidates = [
        candidate(1, "minecraft:cow", [0.0, 0.0, 0.0]),
        candidate(2, "minecraft:cow", [0.0, 3.0, 0.0]),
        candidate(3, "minecraft:cow", [3.0, 0.0, 4.0]),
        candidate(4, "minecraft:cow", [0.0, 0.0, 5.01]),
        candidate(5, "minecraft:cow", [0.0, 2.0, 0.0]),
    ];
    candidates[4].dimension = "minecraft:the_nether";
    for (selector, expected) in [
        ("@e[distance=..5]", &[1, 2, 3][..]),
        ("@e[distance=2..]", &[2, 3, 4]),
        ("@e[distance=2..5]", &[2, 3]),
        ("@e[distance=3]", &[2]),
        ("@e[distance=0]", &[1]),
        ("@e[distance=..5.01]", &[1, 2, 3, 4]),
        ("@e[x=3,y=0,z=4,distance=0]", &[3]),
        ("@e[x=0]", &[1, 2, 3, 4]),
        ("@e", &[1, 2, 3, 4, 5]),
    ] {
        assert_eq!(
            selected_ids(selector, &source, &candidates),
            expected,
            "{selector}"
        );
    }
}

#[test]
fn selector_delta_volume_intersects_hitboxes_and_defaults_missing_axes_to_one_block() {
    let mut source = selector_source();
    source.position = [8.0, 6.0, 7.0];
    let candidates = [
        candidate(1, "minecraft:cow", [8.0, 6.0, 7.0]),
        candidate(2, "minecraft:cow", [5.8, 6.0, 7.0]),
        candidate(3, "minecraft:cow", [8.9, 6.0, 9.1]),
        candidate(4, "minecraft:cow", [8.0, 7.0, 7.0]),
        candidate(5, "minecraft:cow", [5.0, 6.0, 7.0]),
    ];
    assert_eq!(
        selected_ids("@e[dx=-2,dz=1]", &source, &candidates),
        [1, 2, 3]
    );
    assert_eq!(
        selected_ids("@e[dx=-2,dz=1,distance=..2]", &source, &candidates),
        [1]
    );
    assert_eq!(
        selected_ids("@e[x=8,y=7,z=7,dx=0,dy=-1,dz=0]", &source, &candidates),
        [1, 4]
    );
    assert_eq!(selected_ids("@e[dx=0]", &source, &candidates), [1]);
}

#[test]
fn selector_rotation_wraps_endpoints_and_open_ranges() {
    let source = selector_source();
    let mut candidates = [
        candidate(1, "minecraft:cow", [0.0; 3]),
        candidate(2, "minecraft:cow", [0.0; 3]),
        candidate(3, "minecraft:cow", [0.0; 3]),
        candidate(4, "minecraft:cow", [0.0; 3]),
    ];
    for (entity, (pitch, yaw)) in
        candidates
            .iter_mut()
            .zip([(365.0, 175.0), (-2.0, -175.0), (5.0, 2.0), (10.0, 180.0)])
    {
        entity.pitch = pitch;
        entity.yaw = yaw;
    }
    for (selector, expected) in [
        ("@e[y_rotation=170..-170]", &[1, 2, 4][..]),
        ("@e[y_rotation=0..360]", &[]),
        ("@e[x_rotation=..5]", &[1, 3]),
        ("@e[x_rotation=5..]", &[1, 2, 3, 4]),
        ("@e[x_rotation=5,y_rotation=170..190]", &[1]),
        ("@e[y_rotation=180]", &[4]),
    ] {
        assert_eq!(
            selected_ids(selector, &source, &candidates),
            expected,
            "{selector}"
        );
    }
}

#[test]
fn selector_player_fields_and_team_filters_respect_entity_kind() {
    use SelectorGameMode::*;

    let source = selector_source();
    let mut candidates = [
        candidate(1, "minecraft:player", [0.0; 3]),
        candidate(2, "minecraft:player", [0.0; 3]),
        candidate(3, "minecraft:player", [0.0; 3]),
        candidate(4, "minecraft:player", [0.0; 3]),
        candidate(5, "minecraft:cow", [0.0; 3]),
        candidate(6, "minecraft:item", [0.0; 3]),
    ];
    for (entity, (mode, level)) in
        candidates
            .iter_mut()
            .zip([(Survival, 0), (Creative, 3), (Adventure, 5), (Spectator, 6)])
    {
        entity.gamemode = Some(mode);
        entity.level = Some(level);
    }
    candidates[0].team = Some("red");
    candidates[1].team = Some("blue");
    candidates[4].team = Some("red");
    for (selector, expected) in [
        ("@e[level=3..5]", &[2, 3][..]),
        ("@a[level=3..,gamemode=!spectator,gamemode=!creative]", &[3]),
        ("@e[gamemode=survival]", &[1]),
        ("@e[gamemode=creative]", &[2]),
        ("@e[gamemode=adventure]", &[3]),
        ("@e[gamemode=spectator]", &[4]),
        ("@e[team=red]", &[1, 5]),
        ("@e[team=!red,team=blue]", &[2]),
        ("@e[team=]", &[3, 4]),
        ("@e[team=!]", &[1, 2, 5]),
    ] {
        assert_eq!(
            selected_ids(selector, &source, &candidates),
            expected,
            "{selector}"
        );
    }
}

#[test]
fn selector_names_types_and_tags_support_exclusions_and_empty_tag_checks() {
    let source = selector_source();
    let mut candidates = [
        candidate(1, "minecraft:cow", [0.0; 3]),
        candidate(2, "minecraft:pig", [0.0; 3]),
        candidate(3, "minecraft:player", [0.0; 3]),
        candidate(4, "minecraft:item", [0.0; 3]),
    ];
    candidates[0].name = "Bob \"Smith\"";
    candidates[1].name = "Pig";
    candidates[2].name = "Alice";
    candidates[0].type_tags = &["minecraft:animals", "test:herd"];
    candidates[1].type_tags = &["minecraft:animals"];
    candidates[0].tags = &["a", "c"];
    candidates[1].tags = &["a", "b"];
    candidates[2].tags = &["b"];
    for (selector, expected) in [
        (r#"@e[name="Bob \"Smith\""]"#, &[1][..]),
        ("@e[name='Bob \"Smith\"']", &[1]),
        ("@e[name=Pig]", &[2]),
        ("@e[name=!Pig,name=!Alice]", &[1, 4]),
        ("@e[type=cow]", &[1]),
        ("@e[type=minecraft:player]", &[3]),
        ("@e[type=!cow,type=!pig]", &[3, 4]),
        ("@e[type=#animals,type=#test:herd,type=cow]", &[1]),
        ("@e[type=#animals,type=!#test:herd]", &[2]),
        ("@e[type=!#test:missing]", &[1, 2, 3, 4]),
        ("@e[tag=a,tag=!b]", &[1]),
        ("@e[tag=]", &[4]),
        ("@e[tag=!]", &[1, 2, 3]),
    ] {
        assert_eq!(
            selected_ids(selector, &source, &candidates),
            expected,
            "{selector}"
        );
    }
}

#[test]
fn selector_score_ranges_and_advancement_maps_require_present_entries() {
    let source = selector_source();
    let mut candidates = [
        candidate(1, "minecraft:player", [0.0; 3]),
        candidate(2, "minecraft:player", [0.0; 3]),
        candidate(3, "minecraft:player", [0.0; 3]),
        candidate(4, "minecraft:cow", [0.0; 3]),
    ];
    candidates[0].scores.extend([("points", 5), ("debt", -3)]);
    candidates[1].scores.extend([("points", 1), ("debt", 0)]);
    candidates[2].scores.insert("points", 4);
    candidates[3].scores.extend([("points", 5), ("debt", -3)]);
    candidates[0].advancements.insert(
        "minecraft:story/root",
        SelectorAdvancementProgress {
            done: true,
            criteria: [("ready", true)].into(),
        },
    );
    candidates[0].advancements.insert(
        "test:work",
        SelectorAdvancementProgress {
            done: false,
            criteria: [("first", true), ("last", false)].into(),
        },
    );
    candidates[1].advancements.insert(
        "test:work",
        SelectorAdvancementProgress {
            done: false,
            criteria: [("first", true)].into(),
        },
    );
    for (selector, expected) in [
        ("@e[scores={points=1..5,debt=-3}]", &[1, 4][..]),
        ("@e[scores={points=..4 debt=0..}]", &[2]),
        ("@e[scores={points=5,points=1}]", &[2]),
        ("@e[scores={}]", &[1, 2, 3, 4]),
        ("@e[advancements={}]", &[1, 2, 3, 4]),
        (
            "@e[advancements={story/root=true,test:work={first=true,last=false}}]",
            &[1],
        ),
        ("@e[advancements={test:work=false}]", &[1, 2]),
        ("@e[advancements={test:missing=false}]", &[]),
        ("@e[advancements={test:work={missing=false}}]", &[]),
    ] {
        assert_eq!(
            selected_ids(selector, &source, &candidates),
            expected,
            "{selector}"
        );
    }
}

#[test]
fn selector_nbt_keeps_raw_text_and_matches_compounds_lists_and_typed_arrays() {
    let source = selector_source();
    let nbt = SelectorNbt::parse(
        r#"{CustomName:"Bob Smith",data:{hp:10s, EXTRA:1d},Items:[{id:"a",count:2b},{id:"b"}],arr:[I;1,2],other:[]}"#,
    )
    .unwrap();
    let mut candidates = [
        candidate(1, "minecraft:cow", [0.0; 3]),
        candidate(2, "minecraft:cow", [0.0; 3]),
        candidate(3, "minecraft:cow", [0.0; 3]),
    ];
    let empty_nbt = SelectorNbt::parse("{}").unwrap();
    candidates[0].nbt = Some(&nbt);
    candidates[1].nbt = Some(&empty_nbt);
    let raw = r#"{ CustomName : 'Bob Smith', data:{hp:10S},Items:[{id:b},{id:a}], arr:[I;1, 2] }"#;
    let selector = format!("@e[nbt={raw},nbt=!{{other:[1]}}]");
    let spec = parse_selector(&selector).unwrap();
    match &spec.filters()[0] {
        SelectorFilter::Nbt {
            raw: actual,
            negated,
            ..
        } => {
            assert_eq!(actual, raw);
            assert!(!negated);
        }
        filter => panic!("expected NBT, got {filter:?}"),
    }
    assert_eq!(selected_ids(&selector, &source, &candidates), [1]);
    for (selector, expected) in [
        ("@e[nbt={Items:[{id:a},{id:a}]}]", &[1][..]),
        ("@e[nbt={Items:[{id:a},{id:a},{id:a}]}]", &[]),
        ("@e[nbt={Items:[]}]", &[]),
        ("@e[nbt={other:[]}]", &[1]),
        ("@e[nbt={arr:[I;2,1]}]", &[]),
        ("@e[nbt={arr:[1,2]}]", &[]),
        ("@e[nbt={data:{hp:10}}]", &[]),
        ("@e[nbt=!{arr:[I;1,2]}]", &[2]),
    ] {
        assert_eq!(
            selected_ids(selector, &source, &candidates),
            expected,
            "{selector}"
        );
    }
}

#[test]
fn selector_predicates_support_negation_and_missing_registry_entries() {
    let candidates = [
        candidate(1, "minecraft:cow", [0.0; 3]),
        candidate(2, "minecraft:cow", [0.0; 3]),
        candidate(3, "minecraft:cow", [0.0; 3]),
        candidate(4, "minecraft:cow", [0.0; 3]),
    ];
    let spec = parse_selector("@e[predicate=ready,predicate=!test:blocked]").unwrap();
    let matched = select_entities(
        &spec,
        &selector_source(),
        &candidates,
        |entity, id| match id {
            "minecraft:ready" => Some(entity.id != 2),
            "test:blocked" => match entity.id {
                1 => Some(false),
                3 => None,
                _ => Some(true),
            },
            _ => panic!("unexpected resource location {id}"),
        },
        |_| panic!("arbitrary ordering needs no draw"),
    );
    assert_eq!(selector_ids(matched), [1]);
}

#[test]
fn selector_parser_reports_invalid_variables_options_ranges_and_snbt() {
    for (selector, message) in [
        ("Bob", "Expected '@'"),
        ("@q", "Expected one of"),
        ("@", "Expected one of"),
        ("@ex", "Unexpected selector input"),
        ("@e[x=0] tail", "Unexpected selector input"),
        ("@e[", "Expected ']'"),
        ("@e[x=1 y=2]", "Expected ']'"),
        ("@e[unknown=1]", "Unknown selector option"),
        ("@e[sort=reverse]", "Unknown selector sort"),
        ("@e[gamemode=unknown]", "Unknown selector gamemode"),
        ("@e[type=missing]", "Unknown entity type"),
        ("@e[type=foo/bar:cow]", "Invalid selector resource"),
        ("@e[predicate=test:one:two]", "Invalid selector resource"),
        ("@e[name='unfinished]", "Unclosed quoted string"),
        ("@e[name=\"bad\\q\"]", "Invalid escape"),
        ("@e[x=~]", "Expected double"),
        ("@e[dx=+3]", "Expected double"),
        ("@e[distance=..]", "Expected a range bound"),
        ("@e[distance=5..2]", "Range bounds are reversed"),
        ("@e[distance=-1..]", "Range must be non-negative"),
        ("@e[level=..-1]", "Range must be non-negative"),
        ("@e[level=1.5]", "Invalid range bound"),
        ("@e[y_rotation=1..2..3]", "Expected ']'"),
        ("@e[scores={points=3..2}]", "Range bounds are reversed"),
        ("@e[scores={points=2147483648}]", "Invalid range bound"),
        ("@e[scores={points=1]", "Expected '='"),
        ("@e[advancements={test:a=maybe}]", "Invalid bool"),
        ("@e[advancements={test:a={c=}}]", "Expected bool"),
        ("@e[nbt=[1,2]]", "Expected '{'"),
        ("@e[nbt={name}]", "Expected ':'"),
        ("@e[nbt={name:}]", "Expected an SNBT value"),
        ("@e[nbt={a:[1,2b]}]", "Mixed element types"),
        ("@e[nbt={a:[I;1b]}]", "Mixed element types"),
        ("@e[nbt={a:[X;1]}]", "Unknown SNBT array type"),
        ("@e[limit=0]", "Selector limit must be positive"),
        ("@e[limit=-2]", "Selector limit must be positive"),
        ("@e[limit=2147483648]", "Invalid integer"),
    ] {
        let actual = parse_selector(selector).unwrap_err();
        assert!(actual.contains(message), "{selector:?} returned {actual}");
    }
}

#[test]
fn selector_parser_enforces_vanilla_option_applicability() {
    for selector in [
        "@p[type=player]",
        "@a[type=!cow]",
        "@r[type=#animals]",
        "@s[sort=nearest]",
        "@s[limit=1]",
        "@e[x=1,x=2]",
        "@e[distance=1,distance=2]",
        "@n[sort=nearest,sort=arbitrary]",
        "@n[limit=3,limit=2]",
        "@e[name=A,name=!B]",
        "@e[name=!A,name=B]",
        "@e[gamemode=creative,gamemode=!survival]",
        "@e[gamemode=!creative,gamemode=survival]",
        "@e[team=red,team=!blue]",
        "@e[type=cow,type=!pig]",
        "@e[type=!pig,type=#animals]",
        "@e[scores={},scores={x=1}]",
        "@e[advancements={},advancements={test:one=false}]",
    ] {
        let actual = parse_selector(selector).unwrap_err();
        assert!(
            actual.contains("Inapplicable selector option"),
            "{selector:?} returned {actual}"
        );
    }
    for selector in [
        "@e[]",
        "@n[ tag = a,tag=b,]",
        "@e[\"name\"= ! 'Bob Smith',name=!Alice]",
        "@e[type=#animals,type=!cow,type=!pig]",
        "@e[nbt={a:[B;true,-2b],b:[L;3L],floats:[1e2f,2f]}]",
    ] {
        assert!(parse_selector(selector).is_ok(), "{selector}");
    }
}

#[test]
fn selector_point_candidates_obey_lower_inclusive_volume_bounds() {
    let source = selector_source();
    let mut candidates = [
        candidate(1, "minecraft:cow", [0.0; 3]),
        candidate(2, "minecraft:cow", [0.0, 1.0, 0.0]),
        candidate(3, "minecraft:cow", [-1.0, 0.0, 0.0]),
    ];
    for candidate in &mut candidates {
        candidate.bounds = None;
    }
    assert_eq!(selected_ids("@e[dx=0]", &source, &candidates), [1]);
    assert_eq!(selected_ids("@e[distance=0]", &source, &candidates), [1]);
    assert_eq!(selected_ids("@e[distance=1]", &source, &candidates), [2, 3]);
}

// ---------------------------------------------------------------------------------------
// Vanilla 1.21.1 /execute subcommand AST and parser
// ---------------------------------------------------------------------------------------

fn coord(text: &str) -> ExecuteCoordinate {
    ExecuteCoordinate::parse(text).unwrap()
}

fn pos(x: &str, y: &str, z: &str) -> ExecutePosition {
    ExecutePosition {
        x: coord(x),
        y: coord(y),
        z: coord(z),
    }
}

fn run_of(command: &str) -> String {
    ExecuteCommand::parse(command).unwrap().run.unwrap()
}

#[test]
fn execute_coordinates_distinguish_absolute_relative_and_local_forms() {
    assert_eq!(coord("4"), ExecuteCoordinate::Absolute(4.0));
    assert_eq!(coord("-1.5"), ExecuteCoordinate::Absolute(-1.5));
    assert_eq!(coord("~"), ExecuteCoordinate::Relative(0.0));
    assert_eq!(coord("~-2"), ExecuteCoordinate::Relative(-2.0));
    assert_eq!(coord("^"), ExecuteCoordinate::Local(0.0));
    assert_eq!(coord("^1.5"), ExecuteCoordinate::Local(1.5));
    for bad in ["abc", "NaN", "inf", "~oops", "^oops"] {
        assert!(
            ExecuteCoordinate::parse(bad).is_err(),
            "{bad} should be rejected"
        );
    }
}

#[test]
fn execute_context_modifiers_parse_into_distinct_clauses() {
    let command = ExecuteCommand::parse(concat!(
        "execute as @a at @s positioned ~1 ~2 ~3 positioned as @p ",
        "positioned over motion_blocking rotated 90 -45 rotated as @e ",
        "facing ^ ^ ^1 facing entity @p eyes align xz anchored feet ",
        "in the_nether summon minecraft:cow on attacker run say hi"
    ))
    .unwrap();
    assert_eq!(
        command.clauses,
        vec![
            ExecuteClause::Modifier(ExecuteModifier::As {
                targets: "@a".into()
            }),
            ExecuteClause::Modifier(ExecuteModifier::At {
                targets: "@s".into()
            }),
            ExecuteClause::Modifier(ExecuteModifier::Positioned {
                pos: pos("~1", "~2", "~3")
            }),
            ExecuteClause::Modifier(ExecuteModifier::PositionedAs {
                targets: "@p".into()
            }),
            ExecuteClause::Modifier(ExecuteModifier::PositionedOver {
                heightmap: ExecuteHeightmap::MotionBlocking
            }),
            ExecuteClause::Modifier(ExecuteModifier::Rotated {
                yaw: coord("90"),
                pitch: coord("-45")
            }),
            ExecuteClause::Modifier(ExecuteModifier::RotatedAs {
                targets: "@e".into()
            }),
            ExecuteClause::Modifier(ExecuteModifier::Facing {
                pos: pos("^", "^", "^1")
            }),
            ExecuteClause::Modifier(ExecuteModifier::FacingEntity {
                targets: "@p".into(),
                anchor: ExecuteAnchor::Eyes
            }),
            ExecuteClause::Modifier(ExecuteModifier::Align {
                axes: ExecuteAxes {
                    x: true,
                    y: false,
                    z: true
                }
            }),
            ExecuteClause::Modifier(ExecuteModifier::Anchored {
                anchor: ExecuteAnchor::Feet
            }),
            ExecuteClause::Modifier(ExecuteModifier::In {
                dimension: "minecraft:the_nether".into()
            }),
            ExecuteClause::Modifier(ExecuteModifier::Summon {
                entity: "minecraft:cow".into()
            }),
            ExecuteClause::Modifier(ExecuteModifier::On {
                relation: ExecuteRelation::Attacker
            }),
        ]
    );
    assert_eq!(command.run.as_deref(), Some("say hi"));
}

#[test]
fn execute_every_on_relation_and_heightmap_spelling_is_accepted() {
    use ExecuteRelation::*;
    for (relation, expected) in [
        ("attacker", Attacker),
        ("controller", Controller),
        ("leasher", Leasher),
        ("origin", Origin),
        ("owner", Owner),
        ("passengers", Passengers),
        ("target", Target),
        ("vehicle", Vehicle),
    ] {
        let command = ExecuteCommand::parse(&format!("execute on {relation} run say ok")).unwrap();
        assert_eq!(
            command.clauses,
            vec![ExecuteClause::Modifier(ExecuteModifier::On { relation: expected })]
        );
    }
    for (name, expected) in [
        ("world_surface", ExecuteHeightmap::WorldSurface),
        ("ocean_floor", ExecuteHeightmap::OceanFloor),
        ("motion_blocking", ExecuteHeightmap::MotionBlocking),
        (
            "motion_blocking_no_leaves",
            ExecuteHeightmap::MotionBlockingNoLeaves,
        ),
    ] {
        let command =
            ExecuteCommand::parse(&format!("execute positioned over {name} run say ok")).unwrap();
        assert_eq!(
            command.clauses,
            vec![ExecuteClause::Modifier(
                ExecuteModifier::PositionedOver { heightmap: expected }
            )]
        );
    }
}

#[test]
fn execute_every_condition_kind_parses_with_its_exact_argument_order() {
    let cases: &[(&str, ExecuteConditionKind)] = &[
        (
            "if block ~ ~ ~ minecraft:stone",
            ExecuteConditionKind::Block {
                pos: pos("~", "~", "~"),
                predicate: "minecraft:stone".into(),
            },
        ),
        (
            "unless blocks 0 0 0 1 1 1 2 2 2 all",
            ExecuteConditionKind::Blocks {
                start: pos("0", "0", "0"),
                end: pos("1", "1", "1"),
                destination: pos("2", "2", "2"),
                mode: ExecuteBlocksMode::All,
            },
        ),
        (
            "if blocks ~ ~ ~ ~1 ~1 ~1 ~2 ~2 ~2 masked",
            ExecuteConditionKind::Blocks {
                start: pos("~", "~", "~"),
                end: pos("~1", "~1", "~1"),
                destination: pos("~2", "~2", "~2"),
                mode: ExecuteBlocksMode::Masked,
            },
        ),
        (
            "if entity @a",
            ExecuteConditionKind::Entity {
                targets: "@a".into(),
            },
        ),
        (
            "if score @s kills matches 1..10",
            ExecuteConditionKind::Score {
                target: "@s".into(),
                objective: "kills".into(),
                test: ExecuteScoreTest::Matches(ExecuteRange {
                    min: Some(1),
                    max: Some(10),
                }),
            },
        ),
        (
            "unless score @a deaths = @s kills",
            ExecuteConditionKind::Score {
                target: "@a".into(),
                objective: "deaths".into(),
                test: ExecuteScoreTest::Compare(
                    ExecuteScoreComparison::Equals,
                    ExecuteScoreSource {
                        target: "@s".into(),
                        objective: "kills".into(),
                    },
                ),
            },
        ),
        (
            "if biome ~ ~ ~ minecraft:forest",
            ExecuteConditionKind::Biome {
                pos: pos("~", "~", "~"),
                biome: "minecraft:forest".into(),
            },
        ),
        (
            "if dimension minecraft:the_end",
            ExecuteConditionKind::Dimension {
                dimension: "minecraft:the_end".into(),
            },
        ),
        (
            "if loaded ~ ~ ~",
            ExecuteConditionKind::Loaded {
                pos: pos("~", "~", "~"),
            },
        ),
        (
            "unless predicate foo:bar",
            ExecuteConditionKind::Predicate {
                id: "foo:bar".into(),
            },
        ),
        (
            "if data block ~ ~ ~ Items",
            ExecuteConditionKind::Data {
                target: ExecuteDataTarget::Block(pos("~", "~", "~")),
                path: "Items".into(),
                nbt: None,
            },
        ),
        (
            "if data entity @s Inventory[0] {id:\"minecraft:stone\"}",
            ExecuteConditionKind::Data {
                target: ExecuteDataTarget::Entity("@s".into()),
                path: "Inventory[0]".into(),
                nbt: Some("{id:\"minecraft:stone\"}".into()),
            },
        ),
        (
            "unless data storage foo:bar path",
            ExecuteConditionKind::Data {
                target: ExecuteDataTarget::Storage("foo:bar".into()),
                path: "path".into(),
                nbt: None,
            },
        ),
    ];
    for (text, kind) in cases {
        let command = ExecuteCommand::parse(&format!("execute {text} run say ok")).unwrap();
        let expected_check = if text.starts_with("unless") {
            ExecuteCheck::Unless
        } else {
            ExecuteCheck::If
        };
        assert_eq!(
            command.clauses,
            vec![ExecuteClause::Condition(ExecuteConditional {
                check: expected_check,
                kind: kind.clone(),
            })],
            "case {text}"
        );
    }
}

#[test]
fn execute_score_comparisons_cover_all_six_forms_and_ranges() {
    for (op, expected) in [
        ("<", ExecuteScoreComparison::LessThan),
        ("<=", ExecuteScoreComparison::LessEquals),
        ("=", ExecuteScoreComparison::Equals),
        (">=", ExecuteScoreComparison::GreaterEquals),
        (">", ExecuteScoreComparison::GreaterThan),
    ] {
        let command =
            ExecuteCommand::parse(&format!("execute if score @s obj {op} @p other run x")).unwrap();
        match &command.clauses[0] {
            ExecuteClause::Condition(ExecuteConditional {
                kind: ExecuteConditionKind::Score { test, .. },
                ..
            }) => assert_eq!(
                *test,
                ExecuteScoreTest::Compare(
                    expected,
                    ExecuteScoreSource {
                        target: "@p".into(),
                        objective: "other".into(),
                    }
                )
            ),
            other => panic!("unexpected clause: {other:?}"),
        }
    }
    for (range, min, max) in [
        ("0", Some(0), Some(0)),
        ("..3", None, Some(3)),
        ("2..", Some(2), None),
        ("..", None, None),
    ] {
        let command =
            ExecuteCommand::parse(&format!("execute if score @s obj matches {range} run x"))
                .unwrap();
        match &command.clauses[0] {
            ExecuteClause::Condition(ExecuteConditional {
                kind: ExecuteConditionKind::Score { test, .. },
                ..
            }) => assert_eq!(
                *test,
                ExecuteScoreTest::Matches(ExecuteRange { min, max }),
                "range {range}"
            ),
            other => panic!("unexpected clause: {other:?}"),
        }
    }
}

#[test]
fn execute_every_store_target_parses_with_its_exact_argument_order() {
    let cases: &[(&str, ExecuteStoreTarget)] = &[
        (
            "score @s objective",
            ExecuteStoreTarget::Score {
                targets: "@s".into(),
                objective: "objective".into(),
            },
        ),
        (
            "bossbar foo:bar value",
            ExecuteStoreTarget::Bossbar {
                id: "foo:bar".into(),
                field: ExecuteBossbarField::Value,
            },
        ),
        (
            "bossbar foo:bar max",
            ExecuteStoreTarget::Bossbar {
                id: "foo:bar".into(),
                field: ExecuteBossbarField::Max,
            },
        ),
        (
            "block ~ ~ ~ path int 1.5",
            ExecuteStoreTarget::Block {
                pos: pos("~", "~", "~"),
                path: "path".into(),
                nbt_type: ExecuteNbtType::Int,
                scale: 1.5,
            },
        ),
        (
            "entity @e path byte 0.5",
            ExecuteStoreTarget::Entity {
                target: "@e".into(),
                path: "path".into(),
                nbt_type: ExecuteNbtType::Byte,
                scale: 0.5,
            },
        ),
        (
            "storage foo:bar path double -2",
            ExecuteStoreTarget::Storage {
                id: "foo:bar".into(),
                path: "path".into(),
                nbt_type: ExecuteNbtType::Double,
                scale: -2.0,
            },
        ),
    ];
    for (text, target) in cases {
        for (kind_text, kind) in [("result", ExecuteStoreKind::Result), ("success", ExecuteStoreKind::Success)] {
            let command = ExecuteCommand::parse(&format!("execute store {kind_text} {text} run x"))
                .unwrap();
            assert_eq!(
                command.clauses,
                vec![ExecuteClause::Store(ExecuteStore {
                    kind,
                    target: target.clone(),
                })],
                "case {kind_text} {text}"
            );
        }
    }
    for nbt_type in ["byte", "short", "int", "long", "float", "double"] {
        let command =
            ExecuteCommand::parse(&format!("execute store result storage a:b p {nbt_type} 1 run x"))
                .unwrap();
        match &command.clauses[0] {
            ExecuteClause::Store(ExecuteStore {
                target: ExecuteStoreTarget::Storage { nbt_type: actual, .. },
                ..
            }) => assert_eq!(format!("{actual:?}").to_ascii_lowercase(), nbt_type),
            other => panic!("unexpected clause: {other:?}"),
        }
    }
}

#[test]
fn execute_chains_modifiers_conditions_and_stores_in_source_order() {
    let command = ExecuteCommand::parse(concat!(
        "execute as @a store success score @s c if entity @e ",
        "positioned ~ ~ ~ unless block ~ ~ ~ air rotated as @p run ",
        "  say   keep   spacing "
    ))
    .unwrap();
    assert_eq!(
        command.clauses.iter().map(|clause| match clause {
            ExecuteClause::Modifier(ExecuteModifier::As { .. }) => "as",
            ExecuteClause::Store(_) => "store",
            ExecuteClause::Condition(ExecuteConditional { check: ExecuteCheck::If, .. }) => "if",
            ExecuteClause::Modifier(ExecuteModifier::Positioned { .. }) => "positioned",
            ExecuteClause::Condition(ExecuteConditional { check: ExecuteCheck::Unless, .. }) => "unless",
            ExecuteClause::Modifier(ExecuteModifier::RotatedAs { .. }) => "rotated as",
            other => panic!("unexpected clause: {other:?}"),
        }).collect::<Vec<_>>(),
        ["as", "store", "if", "positioned", "unless", "rotated as"]
    );
    assert_eq!(command.run.as_deref(), Some("say   keep   spacing"));
}

#[test]
fn execute_terminal_check_without_run_is_valid() {
    let command = ExecuteCommand::parse("execute if block ~ ~ ~ air").unwrap();
    assert!(command.run.is_none());
    assert_eq!(
        command.clauses,
        vec![ExecuteClause::Condition(ExecuteConditional {
            check: ExecuteCheck::If,
            kind: ExecuteConditionKind::Block {
                pos: pos("~", "~", "~"),
                predicate: "air".into(),
            },
        })]
    );
    let command = ExecuteCommand::parse("/execute unless entity @a").unwrap();
    assert!(command.run.is_none());
    assert_eq!(command.clauses.len(), 1);
}

#[test]
fn execute_run_command_is_kept_byte_exact_including_quotes_and_delimiters() {
    let command =
        ExecuteCommand::parse("execute as @s run say \"hello world\" [a,b] {x:1}  tail").unwrap();
    assert_eq!(
        command.run.as_deref(),
        Some("say \"hello world\" [a,b] {x:1}  tail")
    );
    assert_eq!(run_of("execute run execute as @p run tp @s ~ ~ ~"), "execute as @p run tp @s ~ ~ ~");
}

#[test]
fn execute_parser_rejects_unknown_subcommands_and_malformed_arguments() {
    for command in [
        "execute bogus @s run say x",
        "execute as @s",
        "execute if bogus run x",
        "execute if block ~ ~ ~",
        "execute if blocks 0 0 0 1 1 1 2 2 2 nonsense",
        "execute if score @s obj ~ @p obj run x",
        "execute if score @s obj matches 10..1 run x",
        "execute if data nonsense path",
        "execute store maybe score @s obj run x",
        "execute store result bossbar a:b nonsense run x",
        "execute store result storage a:b p int nope run x",
        "execute store result storage a:b p int NaN run x",
        "execute positioned over not_a_heightmap run x",
        "execute facing entity @p middle run x",
        "execute on sibling run x",
        "execute align xyx run x",
        "execute align q run x",
        "execute anchored side run x",
        "execute rotated 90 run x",
        "execute positioned ~ ~ run x",
        "execute run",
    ] {
        let result = std::panic::catch_unwind(|| ExecuteCommand::parse(command));
        assert!(
            matches!(result, Ok(Err(_))),
            "{command:?} should be rejected, got {result:?}"
        );
    }
}

fn execute_console(command: &str) -> String {
    let source = crate::CmdCtx {
        me: None,
        x: 0.5,
        y: 80.0,
        z: 0.5,
    };
    crate::run_command_ctx(&source, command, &mut None)
}

#[test]
fn execute_pipeline_forks_and_carries_each_entity_position_and_rotation() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let mut peers = Vec::new();
    for (index, (name, x, yaw)) in [("ExecuteA", 10.5, 0.0), ("ExecuteB", 20.5, 90.0)]
        .into_iter()
        .enumerate()
    {
        let (mut player, peer) = crate::effect_tests::client_pair();
        player.name = name.into();
        player.eid += index as i32;
        *player.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, 80.0, 30.5, yaw, 0.0, true);
        crate::clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(std::sync::Arc::new(player));
        peers.push(peer);
    }

    assert_eq!(
        execute_console("/execute as @a at @s anchored eyes if entity @s[distance=..0.1] run setblock ^ ^ ^2 glass"),
        "executed 2 command(s)"
    );
    assert_eq!(
        execute_console("/execute if entity @a"),
        "Test passed, count: 2"
    );
    let glass = crate::name_to_state("glass").unwrap();
    assert_eq!(crate::block_at(10, 81, 32), glass);
    assert_eq!(crate::block_at(18, 81, 30), glass);
    assert_eq!(
        execute_console("/execute positioned -0.1 80 0.5 align x rotated as ExecuteB positioned ^ ^ ^2 if entity @a unless entity Missing run setblock ~ ~ ~ stone"),
        "executed 1 command(s)"
    );
    assert_eq!(crate::block_at(-3, 80, 0), 1);

    assert_eq!(
        execute_console(
            "/execute positioned as ExecuteA facing entity ExecuteB feet run setblock ^ ^ ^2 stone"
        ),
        "executed 1 command(s)"
    );
    assert_eq!(crate::block_at(12, 80, 30), 1);
    assert_eq!(
        execute_console("/execute as @e[type=pig] run setblock 40 80 40 stone"),
        "executed 0 command(s)"
    );
    assert_eq!(crate::block_at(40, 80, 40), 0);

    execute_console("/scoreboard objectives add exec dummy");
    execute_console("/scoreboard players set ExecuteA exec 4");
    execute_console("/scoreboard players set ExecuteB exec 7");
    assert_eq!(
        execute_console("/execute store result score Last exec run execute as @a run scoreboard players get @s exec"),
        "executed 2 command(s)"
    );
    assert_eq!(
        crate::scoreboard().lock().unwrap_or_else(|e| e.into_inner()).get("Last", "exec"),
        Some(7)
    );

    crate::pearls().lock().unwrap_or_else(|e| e.into_inner()).push(crate::Pearl {
        eid: 9000,
        owner: 4242,
        x: 0.5,
        y: 80.0,
        z: 0.5,
        dx: 0.0,
        dy: 0.0,
        dz: 0.0,
        ttl: 1,
    });
    assert_eq!(
        execute_console("/execute as @e[type=ender_pearl] on origin if entity @s[name=ExecuteA]"),
        "Test passed"
    );
    assert_eq!(
        execute_console("/execute as ExecuteA on vehicle run say skipped"),
        "executed 0 command(s)"
    );
}

#[test]
#[cfg(megu_vanilla_data)]
fn execute_pipeline_runs_in_selected_dimension_and_evaluates_world_conditions() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    let dim = crate::DimId::Nether;
    assert_eq!(
        execute_console("/execute positioned 64 80 -64 in the_nether if dimension the_nether run setblock ~ ~ ~ stone"),
        "executed 1 command(s)"
    );
    assert_eq!(crate::block_at_dim(dim, 8, 80, -8), 1);
    assert_eq!(crate::block_at(8, 80, -8), 0);
    assert_eq!(
        execute_console("/execute in the_nether if block 8 80 -8 stone"),
        "Test passed"
    );
    assert_eq!(
        execute_console(
            "/execute positioned 12 80 12 rotated 180 0 facing 12 81 12 run setblock ^ ^ ^3 stone"
        ),
        "executed 1 command(s)"
    );
    assert_eq!(crate::block_at(12, 83, 12), 1);
    crate::forceloaded().lock().unwrap_or_else(|e| e.into_inner()).insert((2, 2));
    assert_eq!(
        execute_console("/execute positioned 32.5 90 32.5 if loaded ~ ~ ~ if biome ~ ~ ~ plains run setblock ~ ~ ~ stone"),
        "executed 1 command(s)"
    );
    assert_eq!(
        execute_console("/execute positioned 32.5 0 32.5 positioned over world_surface if block ~ ~-1 ~ stone if blocks 32 90 32 32 90 32 ~ ~-1 ~ all"),
        "Test passed"
    );
    crate::predicates().lock().unwrap_or_else(|e| e.into_inner()).insert(
        "test:always".into(),
        r#"{"condition":"minecraft:time_check","period":1,"value":0}"#.into(),
    );
    assert_eq!(
        execute_console("/execute if predicate test:always unless loaded 10000 80 10000"),
        "Test passed"
    );
}

#[test]
fn execute_pipeline_runs_nested_commands_and_stores_result_and_success_separately() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    execute_console("/scoreboard objectives add exec dummy");
    execute_console("/scoreboard players set initial exec 6");
    execute_console("/bossbar add test:execute Result");

    assert_eq!(
        execute_console("/execute if score initial exec matches 1..10 store result score result exec store success score success exec store result bossbar test:execute value store result storage test:execute results.scaled double 0.5 run scoreboard players add initial exec 2"),
        "Stored 8 in exec for result"
    );
    assert_eq!(
        execute_console("/bossbar get test:execute value"),
        "Bossbar test:execute value is 8"
    );
    assert_eq!(
        execute_console("/execute if score initial exec = result exec if score success exec matches 1 if data storage test:execute results{scaled:4.0d}"),
        "Test passed"
    );
    assert_eq!(
        execute_console("/execute store result score nested exec run execute if score initial exec > success exec"),
        "Stored 1 in exec for nested"
    );
    assert_eq!(
        execute_console("/execute store success score failure exec run execute unless score initial exec matches 8"),
        "Test failed"
    );
    assert_eq!(
        execute_console(
            "/execute store success score zero exec run scoreboard players get failure exec"
        ),
        "executed 1 command(s)"
    );
    assert_eq!(
        execute_console("/execute if score failure exec matches 0 if score zero exec matches 1"),
        "Test passed"
    );
    assert_eq!(
        execute_console("/execute store success score unknown exec run missing_command"),
        "Unknown command: missing_command"
    );
    assert_eq!(
        crate::scoreboard().lock().unwrap_or_else(|e| e.into_inner()).get("unknown", "exec"),
        Some(0)
    );
    assert_eq!(
        execute_console("/execute store success score untouched exec unless score initial exec matches 8 run say skipped"),
        "Test failed"
    );
    assert_eq!(
        crate::scoreboard().lock().unwrap_or_else(|e| e.into_inner()).get("untouched", "exec"),
        None
    );
    assert_eq!(
        execute_console("/execute if score initial exec matches 8 run say It's [fine"),
        "executed 1 command(s)"
    );
}

#[test]
fn execute_pipeline_summons_and_stores_in_live_entity_and_block_data() {
    if crate::test_support::isolated_world_test() {
        return;
    }
    execute_console("/scoreboard objectives add exec dummy");
    execute_console("/scoreboard players set number exec 4");
    assert_eq!(
        execute_console("/execute positioned 12.5 80 16.5 summon cow store result entity @s Health float 2 run scoreboard players get number exec"),
        "executed 1 command(s)"
    );
    assert_eq!(
        execute_console(
            "/execute as @e[type=cow] if data entity @s {Health:8f} run tag @s add tested"
        ),
        "executed 1 command(s)"
    );
    assert_eq!(
        execute_console("/execute if entity @e[type=cow,tag=tested,nbt={Health:8f}]"),
        "Test passed"
    );

    let pos = (50, 80, 50);
    crate::set_block(pos.0, pos.1, pos.2, crate::name_to_state("chest").unwrap());
    let item = crate::name_to_item("stone").unwrap();
    let components = vec![(crate::COMP_CUSTOM_DATA, vec![10, 0])];
    crate::chests().lock().unwrap_or_else(|e| e.into_inner()).insert(
        crate::ContainerKey::ow(pos),
        [(0, (item, 1, components.clone()))].into(),
    );
    assert_eq!(
        execute_console("/execute store result block 50 80 50 Items[0].count int 2 run scoreboard players get number exec"),
        "executed 1 command(s)"
    );
    assert_eq!(
        execute_console(
            r#"/execute if data block 50 80 50 Items[{id:"minecraft:stone", count:8}]"#
        ),
        "Test passed"
    );
    assert_eq!(
        crate::chests().lock().unwrap_or_else(|e| e.into_inner())[&crate::ContainerKey::ow(pos)][&0],
        (item, 8, components)
    );
}

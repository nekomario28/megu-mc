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
            seen_by_command.lock().unwrap().push(*context.source());
            Ok(*context.source())
        }))
        .unwrap();
    dispatcher
        .register(ArgumentBuilder::literal("split").fork(dispatcher.root().clone(), |_| {
            Ok(vec![9, 10, 11])
        }))
        .unwrap();

    assert_eq!(dispatcher.execute("split run", 0).unwrap(), 3);
    assert_eq!(*seen.lock().unwrap(), vec![9, 10, 11]);
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
            .unwrap()
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
        *log.lock().unwrap(),
        vec![(9, true, 9), (10, true, 10), (11, true, 11)]
    );

    // A failure is reported as such, and in non-forked mode still propagates.
    log.lock().unwrap().clear();
    dispatcher
        .register(
            ArgumentBuilder::literal("crash")
                .executes(|_| Err(BUILT_IN_EXCEPTIONS.reader_expected_bool().create())),
        )
        .unwrap();
    assert!(dispatcher.execute("crash", 4).is_err());
    assert_eq!(*log.lock().unwrap(), vec![(4, false, 0)]);
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
                        *captured_by_command.lock().unwrap() = Some(format!(
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
    assert_eq!(captured.lock().unwrap().as_deref(), Some("5:alpha"));

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

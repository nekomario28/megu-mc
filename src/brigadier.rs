//! Brigadier's core, ported from Mojang's Brigadier 1.3.10 (MIT).
//!
//! Vanilla 1.21.1 parses every command with `com.mojang:brigadier:1.3.10` (the jar is
//! bundled in the 1.21.1 server jar, `META-INF/libraries.list`, sha1
//! `c8ee4136e474ac7723ca2b432ec8d1a2bc88ef7d1ec57c314ba9e33cdc83dd75`). This module is a
//! Rust translation of that library's core: the reader, the command tree, the dispatcher,
//! the context/chain model, the suggestion model, and the six primitive argument types.
//!
//! Reference: <https://github.com/Mojang/brigadier>, pinned commit
//! `b5419b186e4882290a188d11d6e661d4f56bd8fd` (2024-08-07). Upstream has no `1.3.10` git
//! tag - its tags stop at 1.0.18 and `gradle.properties` declares `majorMinor: 1.3` - so the
//! pin is the last commit before 1.21.1 shipped (2024-08-08) and the one whose API surface
//! matches the bundled jar: `javap com.mojang.brigadier.context.SuggestionContext` on the
//! 1.21.1 jar shows `parent` + `startPos` only, i.e. it predates the December 2024
//! suggestion-context commits. See THIRD_PARTY.md for the attribution.
//!
//! # What this slice is
//!
//! A standalone library module. Nothing in `main.rs` calls it yet: `run_command_ctx` still
//! owns the handwritten parser, and no packet is built from this tree. The wire format of the
//! Commands packet and of the suggestions exchange lives in `src/brigadier_wire.rs`, which reads
//! this module and is read by nothing; sending either packet is still a later slice. Wiring,
//! the remaining 1.21.1 argument types,
//! `.mcfunction` execution and the JVM-shim bridge are later slices too.
//!
//! # Deliberate deviations from the Java code
//!
//! Each of these is a place where a literal translation would not be Rust:
//!
//! - **Synchronous suggestions.** `ArgumentType.listSuggestions` and
//!   `CommandDispatcher.getCompletionSuggestions` return `CompletableFuture` upstream. They
//!   are plain values here; Java's async is an implementation detail of its call sites.
//! - **Cursor units are UTF-8 bytes.** Java indexes UTF-16 code units. Every position this
//!   port reports for ASCII input (which is all vanilla command syntax, and every number
//!   literal) is identical to Java's; a non-ASCII character inside a quoted string moves the
//!   cursor by its UTF-8 length instead of by 1-2 UTF-16 units. Byte cursors keep `&str`
//!   slicing total, which is the property that makes the reader safe to translate.
//! - **`getArgument(name, Class)` is `argument::<T>(name)`.** Java checks assignability
//!   against the wrapper of the primitive `Class`; here the stored value is `Arc<dyn Any>`
//!   and the request is a concrete Rust type. Same two error cases, same messages.
//! - **Nodes are reference counted, not garbage collected.** A redirect to an ancestor (which
//!   `execute` relies on: `redirect(root)`) is a reference cycle, and a dropped dispatcher
//!   does not free the nodes it was part of. Vanilla command trees live for the process
//!   lifetime; the alternative (arena indices) would have made every node field an index.
//!   Children and the command callback are behind a lock because Java's `addChild` mutates an
//!   existing child in place; that mutation is what makes `register` merge subtrees, and it is
//!   pinned by `CommandNode::add_child` tests.
//! - **`CommandContextBuilder` does not hold the dispatcher.** Upstream exposes
//!   `getDispatcher()`; nothing in the parse/execute path reads it, so the field is gone.
//! - **Not ported in this slice:** `ContextChain.nextStage`/`getStage` staged execution (the
//!   vanilla `execute ... run` flow), `CommandNode.findAmbiguities`, and the Commands tree
//!   packet builder. They are named here so a later slice does not mistake their absence for
//!   an oversight.
#![allow(dead_code)] // no consumer yet: slice 2 wires this into run_command_ctx.

use std::any::Any;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The string that separates one argument from the next (`CommandDispatcher.ARGUMENT_SEPARATOR`).
pub const ARGUMENT_SEPARATOR: &str = " ";
/// The char form of [`ARGUMENT_SEPARATOR`].
pub const ARGUMENT_SEPARATOR_CHAR: char = ' ';

const USAGE_OPTIONAL_OPEN: &str = "[";
const USAGE_OPTIONAL_CLOSE: &str = "]";
const USAGE_REQUIRED_OPEN: &str = "(";
const USAGE_REQUIRED_CLOSE: &str = ")";
const USAGE_OR: &str = "|";

pub fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

pub fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// A parsed argument value, type-erased the way Java's `ParsedArgument<S, ?>` erases `T`.
///
/// The concrete type's name is kept beside the value because Java's wrong-type message names it
/// (`Argument 'foo' is defined as Integer, not class java.lang.String`) and `dyn Any` cannot be
/// asked for it after erasure.
#[derive(Clone)]
pub struct ArgumentValue {
    value: Arc<dyn Any + Send + Sync>,
    type_name: &'static str,
}

impl ArgumentValue {
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self {
            value: Arc::new(value),
            type_name: std::any::type_name::<T>(),
        }
    }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.value.downcast_ref::<T>()
    }

    /// `result.getClass().getSimpleName()`: the erased value's Rust type name.
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }
}

impl fmt::Debug for ArgumentValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ArgumentValue").field(&self.type_name).finish()
    }
}

/// Java's `Command<S>`: returns the command's result count or a syntax exception.
pub type CommandFn<S> = dyn Fn(&CommandContext<S>) -> Result<i32, CommandSyntaxException> + Send + Sync;
pub type Command<S> = Arc<CommandFn<S>>;

/// Java's `RedirectModifier<S>`: turns one source into the sources the child commands run on.
pub type RedirectModifierFn<S> =
    dyn Fn(&CommandContext<S>) -> Result<Vec<S>, CommandSyntaxException> + Send + Sync;
pub type RedirectModifier<S> = Arc<RedirectModifierFn<S>>;

/// Java's `Predicate<S>` on a node: `requires(...)`.
pub type Requirement<S> = Arc<dyn Fn(&S) -> bool + Send + Sync>;

/// Java's `ResultConsumer<S>`: told about every command attempt, forked or not.
pub type ResultConsumer<S> = Arc<dyn Fn(&CommandContext<S>, bool, i32) + Send + Sync>;

/// Java's `SuggestionProvider<S>`.
pub type SuggestionProvider<S> = Arc<
    dyn Fn(&CommandContext<S>, &mut SuggestionsBuilder) -> Result<Suggestions, CommandSyntaxException>
        + Send
        + Sync,
>;

pub fn requirement_any<S>() -> Requirement<S> {
    Arc::new(|_| true)
}

// ---------------------------------------------------------------------------------------
// StringReader
// ---------------------------------------------------------------------------------------

/// `com.mojang.brigadier.StringReader`.
///
/// The cursor is a byte offset (see the module note on units). Every operation that moves it
/// moves by a whole character, so it always sits on a `char` boundary and `string[cursor..]`
/// is valid. Where Java throws `StringIndexOutOfBoundsException` for reading past the end,
/// these methods return `None`; every caller in the port checks `can_read` first, as the Java
/// callers do.
#[derive(Clone, Debug)]
pub struct StringReader {
    string: String,
    cursor: usize,
}

const SYNTAX_ESCAPE: char = '\\';
const SYNTAX_DOUBLE_QUOTE: char = '"';
const SYNTAX_SINGLE_QUOTE: char = '\'';

impl StringReader {
    pub fn new(string: impl Into<String>) -> Self {
        Self {
            string: string.into(),
            cursor: 0,
        }
    }

    pub fn string(&self) -> &str {
        &self.string
    }

    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor;
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// `getRemainingLength`: in bytes.
    pub fn remaining_length(&self) -> usize {
        self.string.len() - self.cursor
    }

    /// `getTotalLength`: in bytes.
    pub fn total_length(&self) -> usize {
        self.string.len()
    }

    /// `getRead`.
    pub fn read_text(&self) -> &str {
        &self.string[..self.cursor]
    }

    /// `getRemaining`.
    pub fn remaining(&self) -> &str {
        &self.string[self.cursor..]
    }

    pub fn can_read(&self) -> bool {
        self.can_read_n(1)
    }

    pub fn can_read_n(&self, length: usize) -> bool {
        self.cursor + length <= self.string.len()
    }

    pub fn peek(&self) -> Option<char> {
        self.string[self.cursor..].chars().next()
    }

    pub fn peek_at(&self, offset: usize) -> Option<char> {
        self.string[self.cursor..].chars().nth(offset)
    }

    pub fn read(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.cursor += c.len_utf8();
        Some(c)
    }

    pub fn skip(&mut self) {
        if let Some(c) = self.peek() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn is_allowed_number(c: char) -> bool {
        c.is_ascii_digit() || c == '.' || c == '-'
    }

    pub fn is_quoted_string_start(c: char) -> bool {
        c == SYNTAX_DOUBLE_QUOTE || c == SYNTAX_SINGLE_QUOTE
    }

    /// `Character.isWhitespace`: Java's rule excludes the non-breaking spaces that Rust's
    /// `char::is_whitespace` accepts, so a command containing U+00A0 is not silently split.
    pub fn is_whitespace(c: char) -> bool {
        c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{2007}' | '\u{202f}')
    }

    pub fn skip_whitespace(&mut self) {
        while self.can_read() && self.peek().is_some_and(Self::is_whitespace) {
            self.skip();
        }
    }

    fn scan_number(&mut self) -> (usize, usize) {
        let start = self.cursor;
        while self.can_read() && self.peek().is_some_and(Self::is_allowed_number) {
            self.skip();
        }
        (start, self.cursor)
    }

    pub fn read_int(&mut self) -> Result<i32, CommandSyntaxException> {
        let (start, end) = self.scan_number();
        let number = self.string[start..end].to_string();
        if number.is_empty() {
            return Err(BUILT_IN_EXCEPTIONS.reader_expected_int().create_with_context(self));
        }
        match number.parse::<i32>() {
            Ok(value) => Ok(value),
            Err(_) => {
                self.cursor = start;
                Err(BUILT_IN_EXCEPTIONS.reader_invalid_int().create_with_context(self, &number))
            }
        }
    }

    pub fn read_long(&mut self) -> Result<i64, CommandSyntaxException> {
        let (start, end) = self.scan_number();
        let number = self.string[start..end].to_string();
        if number.is_empty() {
            return Err(BUILT_IN_EXCEPTIONS.reader_expected_long().create_with_context(self));
        }
        match number.parse::<i64>() {
            Ok(value) => Ok(value),
            Err(_) => {
                self.cursor = start;
                Err(BUILT_IN_EXCEPTIONS.reader_invalid_long().create_with_context(self, &number))
            }
        }
    }

    pub fn read_double(&mut self) -> Result<f64, CommandSyntaxException> {
        let (start, end) = self.scan_number();
        let number = self.string[start..end].to_string();
        if number.is_empty() {
            return Err(BUILT_IN_EXCEPTIONS.reader_expected_double().create_with_context(self));
        }
        match number.parse::<f64>() {
            Ok(value) => Ok(value),
            Err(_) => {
                self.cursor = start;
                Err(BUILT_IN_EXCEPTIONS.reader_invalid_double().create_with_context(self, &number))
            }
        }
    }

    pub fn read_float(&mut self) -> Result<f32, CommandSyntaxException> {
        let (start, end) = self.scan_number();
        let number = self.string[start..end].to_string();
        if number.is_empty() {
            return Err(BUILT_IN_EXCEPTIONS.reader_expected_float().create_with_context(self));
        }
        match number.parse::<f32>() {
            Ok(value) => Ok(value),
            Err(_) => {
                self.cursor = start;
                Err(BUILT_IN_EXCEPTIONS.reader_invalid_float().create_with_context(self, &number))
            }
        }
    }

    pub fn is_allowed_in_unquoted_string(c: char) -> bool {
        c.is_ascii_digit()
            || c.is_ascii_uppercase()
            || c.is_ascii_lowercase()
            || c == '_'
            || c == '-'
            || c == '.'
            || c == '+'
    }

    pub fn read_unquoted_string(&mut self) -> String {
        let start = self.cursor;
        while self.can_read() && self.peek().is_some_and(Self::is_allowed_in_unquoted_string) {
            self.skip();
        }
        self.string[start..self.cursor].to_string()
    }

    pub fn read_quoted_string(&mut self) -> Result<String, CommandSyntaxException> {
        if !self.can_read() {
            return Ok(String::new());
        }
        let next = self.peek().unwrap_or(SYNTAX_DOUBLE_QUOTE);
        if !Self::is_quoted_string_start(next) {
            return Err(BUILT_IN_EXCEPTIONS
                .reader_expected_start_of_quote()
                .create_with_context(self));
        }
        self.skip();
        self.read_string_until(next)
    }

    pub fn read_string_until(&mut self, terminator: char) -> Result<String, CommandSyntaxException> {
        let mut result = String::new();
        let mut escaped = false;
        while self.can_read() {
            let Some(c) = self.read() else { break };
            if escaped {
                if c == terminator || c == SYNTAX_ESCAPE {
                    result.push(c);
                    escaped = false;
                } else {
                    self.set_cursor(self.cursor - c.len_utf8());
                    return Err(BUILT_IN_EXCEPTIONS
                        .reader_invalid_escape()
                        .create_with_context(self, &c.to_string()));
                }
            } else if c == SYNTAX_ESCAPE {
                escaped = true;
            } else if c == terminator {
                return Ok(result);
            } else {
                result.push(c);
            }
        }

        Err(BUILT_IN_EXCEPTIONS.reader_expected_end_of_quote().create_with_context(self))
    }

    pub fn read_string(&mut self) -> Result<String, CommandSyntaxException> {
        if !self.can_read() {
            return Ok(String::new());
        }
        let next = self.peek().unwrap_or(SYNTAX_DOUBLE_QUOTE);
        if Self::is_quoted_string_start(next) {
            self.skip();
            return self.read_string_until(next);
        }
        Ok(self.read_unquoted_string())
    }

    pub fn read_boolean(&mut self) -> Result<bool, CommandSyntaxException> {
        let start = self.cursor;
        let value = self.read_string()?;
        if value.is_empty() {
            return Err(BUILT_IN_EXCEPTIONS.reader_expected_bool().create_with_context(self));
        }
        match value.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => {
                self.cursor = start;
                Err(BUILT_IN_EXCEPTIONS.reader_invalid_bool().create_with_context(self, &value))
            }
        }
    }

    pub fn expect(&mut self, c: char) -> Result<(), CommandSyntaxException> {
        if !self.can_read() || self.peek() != Some(c) {
            return Err(BUILT_IN_EXCEPTIONS
                .reader_expected_symbol()
                .create_with_context(self, &c.to_string()));
        }
        self.skip();
        Ok(())
    }

    /// `String: substring(start, end)` with both ends clamped to char boundaries - the port's
    /// equivalent of Java's `substring`, used where a cursor can meet a multi-byte character.
    fn slice(&self, start: usize, end: usize) -> &str {
        let start = floor_char_boundary(&self.string, start.min(self.string.len()));
        let end = floor_char_boundary(&self.string, end.min(self.string.len()));
        &self.string[start..end.max(start)]
    }
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

// ---------------------------------------------------------------------------------------
// Vanilla command tokens and entity selectors
// ---------------------------------------------------------------------------------------

/// Split on whitespace at the top level, retaining the original quotes and escapes.
pub(crate) fn split_command_args_preserving_quotes(input: &str) -> Result<Vec<String>, String> {
    let mut reader = StringReader::new(input);
    let mut args = Vec::new();
    let mut delimiters = Vec::new();
    reader.skip_whitespace();
    let mut start = reader.cursor();

    while let Some(c) = reader.peek() {
        match c {
            '"' | '\'' => {
                reader.read_quoted_string().map_err(|err| err.to_string())?;
            }
            '[' | '{' => {
                delimiters.push(if c == '[' { ']' } else { '}' });
                reader.skip();
            }
            ']' | '}' => {
                if delimiters.pop() != Some(c) {
                    return Err(format!("Unexpected '{c}' at byte {}", reader.cursor()));
                }
                reader.skip();
            }
            c if delimiters.is_empty() && StringReader::is_whitespace(c) => {
                args.push(input[start..reader.cursor()].to_owned());
                reader.skip_whitespace();
                start = reader.cursor();
            }
            _ => reader.skip(),
        }
    }

    if let Some(end) = delimiters.pop() {
        return Err(format!("Expected '{end}' at byte {}", reader.cursor()));
    }
    if start < reader.cursor() {
        args.push(input[start..].to_owned());
    }
    Ok(args)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectorVariable {
    NearestPlayer,
    AllPlayers,
    RandomPlayer,
    AllEntities,
    SelfEntity,
    NearestEntity,
}

impl SelectorVariable {
    fn players_only(self) -> bool {
        matches!(
            self,
            Self::NearestPlayer | Self::AllPlayers | Self::RandomPlayer
        )
    }

    fn requires_alive(self) -> bool {
        matches!(self, Self::AllEntities | Self::NearestEntity)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectorSort {
    Nearest,
    Furthest,
    Random,
    Arbitrary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectorGameMode {
    Survival,
    Creative,
    Adventure,
    Spectator,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectorRange<T> {
    pub min: Option<T>,
    pub max: Option<T>,
}

impl<T: Copy + PartialOrd> SelectorRange<T> {
    fn contains(&self, value: T) -> bool {
        self.min.is_none_or(|min| value >= min) && self.max.is_none_or(|max| value <= max)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SelectorAdvancement {
    Done(bool),
    Criteria(HashMap<String, bool>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SelectorFilter {
    Name(String, bool),
    GameMode(SelectorGameMode, bool),
    Team(String, bool),
    Type {
        id: String,
        tag: bool,
        negated: bool,
    },
    Tag(String, bool),
    Nbt {
        raw: String,
        value: SelectorNbt,
        negated: bool,
    },
    Scores(HashMap<String, SelectorRange<i32>>),
    Advancements(HashMap<String, SelectorAdvancement>),
    Predicate(String, bool),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectorSpec {
    variable: SelectorVariable,
    origin: [Option<f64>; 3],
    delta: [Option<f64>; 3],
    distance: Option<SelectorRange<f64>>,
    level: Option<SelectorRange<i32>>,
    pitch: Option<SelectorRange<f32>>,
    yaw: Option<SelectorRange<f32>>,
    limit: usize,
    sort: SelectorSort,
    filters: Vec<SelectorFilter>,
}

impl SelectorSpec {
    /// Parse one complete selector. The caller provides the entity registry lookup for
    /// canonical IDs such as `minecraft:cow`. Entity-type tags are resolved at match time.
    pub(crate) fn parse(
        input: &str,
        entity_type_exists: impl Fn(&str) -> bool,
    ) -> Result<Self, String> {
        use SelectorVariable::*;

        let mut reader = StringReader::new(input);
        reader.expect('@').map_err(|err| err.to_string())?;
        let variable = match reader.read() {
            Some('p') => NearestPlayer,
            Some('a') => AllPlayers,
            Some('r') => RandomPlayer,
            Some('e') => AllEntities,
            Some('s') => SelfEntity,
            Some('n') => NearestEntity,
            _ => return Err("Expected one of @p, @a, @r, @e, @s, @n".into()),
        };
        let mut spec = Self {
            variable,
            origin: [None; 3],
            delta: [None; 3],
            distance: None,
            level: None,
            pitch: None,
            yaw: None,
            limit: if matches!(variable, AllPlayers | AllEntities) {
                i32::MAX as usize
            } else {
                1
            },
            sort: match variable {
                NearestPlayer | NearestEntity => SelectorSort::Nearest,
                RandomPlayer => SelectorSort::Random,
                _ => SelectorSort::Arbitrary,
            },
            filters: Vec::new(),
        };
        if selector_take(&mut reader, '[') {
            spec.read_options(&mut reader, entity_type_exists)
                .map_err(|err| err.to_string())?;
        }
        if reader.can_read() {
            return Err(format!(
                "Unexpected selector input at byte {}",
                reader.cursor()
            ));
        }
        Ok(spec)
    }

    fn read_options(
        &mut self,
        reader: &mut StringReader,
        entity_type_exists: impl Fn(&str) -> bool,
    ) -> Result<(), CommandSyntaxException> {
        let mut locked = HashSet::new();
        let mut exclusions = HashSet::new();
        reader.skip_whitespace();
        while reader.can_read() && reader.peek() != Some(']') {
            reader.skip_whitespace();
            let key = reader.read_string()?;
            if locked.contains(&key)
                || (key == "type" && self.variable.players_only())
                || (matches!(key.as_str(), "limit" | "sort")
                    && self.variable == SelectorVariable::SelfEntity)
            {
                return Err(selector_error(
                    reader,
                    format!("Inapplicable selector option '{key}'"),
                ));
            }

            reader.skip_whitespace();
            reader.expect('=')?;
            reader.skip_whitespace();
            let negated = matches!(
                key.as_str(),
                "name" | "gamemode" | "team" | "type" | "tag" | "nbt" | "predicate"
            ) && selector_take(reader, '!');
            reader.skip_whitespace();
            if exclusions.contains(&key) && !negated {
                return Err(selector_error(
                    reader,
                    format!("Inapplicable selector option '{key}'"),
                ));
            }

            let mut repeatable = false;
            match key.as_str() {
                "x" => self.origin[0] = Some(reader.read_double()?),
                "y" => self.origin[1] = Some(reader.read_double()?),
                "z" => self.origin[2] = Some(reader.read_double()?),
                "dx" => self.delta[0] = Some(reader.read_double()?),
                "dy" => self.delta[1] = Some(reader.read_double()?),
                "dz" => self.delta[2] = Some(reader.read_double()?),
                "distance" => self.distance = Some(read_selector_range(reader, Some(0.0), false)?),
                "level" => self.level = Some(read_selector_range(reader, Some(0), false)?),
                "x_rotation" => self.pitch = Some(read_selector_range(reader, None, true)?),
                "y_rotation" => self.yaw = Some(read_selector_range(reader, None, true)?),
                "limit" => {
                    let limit = reader.read_int()?;
                    if limit < 1 {
                        return Err(selector_error(reader, "Selector limit must be positive"));
                    }
                    self.limit = limit as usize;
                }
                "sort" => {
                    self.sort = match reader.read_unquoted_string().as_str() {
                        "nearest" => SelectorSort::Nearest,
                        "furthest" => SelectorSort::Furthest,
                        "random" => SelectorSort::Random,
                        "arbitrary" => SelectorSort::Arbitrary,
                        _ => return Err(selector_error(reader, "Unknown selector sort")),
                    };
                }
                "name" | "gamemode" | "team" | "type" | "tag" | "nbt" | "scores"
                | "advancements" | "predicate" => {
                    let filter = read_selector_filter(reader, &key, negated, &entity_type_exists)?;
                    if negated && matches!(key.as_str(), "name" | "gamemode" | "type") {
                        exclusions.insert(key.clone());
                        repeatable = true;
                    } else {
                        repeatable = matches!(
                            filter,
                            SelectorFilter::Tag(..)
                                | SelectorFilter::Nbt { .. }
                                | SelectorFilter::Predicate(..)
                                | SelectorFilter::Team(_, true)
                                | SelectorFilter::Type { tag: true, .. }
                        );
                    }
                    self.filters.push(filter);
                }
                _ => {
                    return Err(selector_error(
                        reader,
                        format!("Unknown selector option '{key}'"),
                    ))
                }
            }
            if !repeatable {
                locked.insert(key);
            }
            reader.skip_whitespace();
            if !selector_take(reader, ',') {
                break;
            }
        }
        reader.expect(']')
    }

    pub(crate) fn filters(&self) -> &[SelectorFilter] {
        &self.filters
    }
}

fn selector_error(reader: &StringReader, message: impl Into<String>) -> CommandSyntaxException {
    CommandSyntaxException::with_context(
        CommandExceptionType::DispatcherParseException,
        message,
        reader.string(),
        reader.cursor(),
    )
}

fn selector_take(reader: &mut StringReader, c: char) -> bool {
    if reader.peek() == Some(c) {
        reader.skip();
        true
    } else {
        false
    }
}

fn read_selector_range<T: Copy + PartialOrd + std::str::FromStr>(
    reader: &mut StringReader,
    floor: Option<T>,
    wraps: bool,
) -> Result<SelectorRange<T>, CommandSyntaxException> {
    fn bound<T: std::str::FromStr>(
        reader: &mut StringReader,
    ) -> Result<Option<T>, CommandSyntaxException> {
        let start = reader.cursor();
        while reader.peek().is_some_and(StringReader::is_allowed_number)
            && !reader.remaining().starts_with("..")
        {
            reader.skip();
        }
        if start == reader.cursor() {
            return Ok(None);
        }
        let value = &reader.string()[start..reader.cursor()];
        value
            .parse()
            .map(Some)
            .map_err(|_| selector_error(reader, "Invalid range bound"))
    }

    let min = bound(reader)?;
    let max = if reader.remaining().starts_with("..") {
        reader.set_cursor(reader.cursor() + 2);
        bound(reader)?
    } else {
        min
    };
    if min.is_none() && max.is_none() {
        return Err(selector_error(reader, "Expected a range bound"));
    }
    if !wraps && min.zip(max).is_some_and(|(min, max)| min > max) {
        return Err(selector_error(reader, "Range bounds are reversed"));
    }
    if floor.is_some_and(|floor| min.is_some_and(|v| v < floor) || max.is_some_and(|v| v < floor)) {
        return Err(selector_error(reader, "Range must be non-negative"));
    }
    Ok(SelectorRange { min, max })
}

fn read_selector_resource(reader: &mut StringReader) -> Result<String, CommandSyntaxException> {
    let resource_char =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-');
    let start = reader.cursor();
    while reader
        .peek()
        .is_some_and(|c| resource_char(c) || matches!(c, ':' | '/'))
    {
        reader.skip();
    }
    let resource = &reader.string()[start..reader.cursor()];
    let (namespace, path) = resource.split_once(':').unwrap_or(("", resource));
    if !namespace.chars().all(resource_char) || !path.chars().all(|c| resource_char(c) || c == '/')
    {
        return Err(selector_error(reader, "Invalid selector resource location"));
    }
    let namespace = if namespace.is_empty() {
        "minecraft"
    } else {
        namespace
    };
    Ok(format!("{namespace}:{path}"))
}

fn read_selector_map<K: Eq + std::hash::Hash, V>(
    reader: &mut StringReader,
    read_key: impl Fn(&mut StringReader) -> Result<K, CommandSyntaxException>,
    read_value: impl Fn(&mut StringReader) -> Result<V, CommandSyntaxException>,
) -> Result<HashMap<K, V>, CommandSyntaxException> {
    reader.expect('{')?;
    reader.skip_whitespace();
    let mut entries = HashMap::new();
    while reader.can_read() && reader.peek() != Some('}') {
        reader.skip_whitespace();
        let key = read_key(reader)?;
        reader.skip_whitespace();
        reader.expect('=')?;
        reader.skip_whitespace();
        entries.insert(key, read_value(reader)?);
        reader.skip_whitespace();
        // Vanilla's score and advancement maps allow the comma to be omitted.
        selector_take(reader, ',');
    }
    reader.expect('}')?;
    Ok(entries)
}

fn read_selector_filter(
    reader: &mut StringReader,
    key: &str,
    negated: bool,
    entity_type_exists: &impl Fn(&str) -> bool,
) -> Result<SelectorFilter, CommandSyntaxException> {
    let word = |reader: &mut StringReader| Ok(reader.read_unquoted_string());
    Ok(match key {
        "name" => SelectorFilter::Name(reader.read_string()?, negated),
        "gamemode" => {
            let mode = match reader.read_unquoted_string().as_str() {
                "survival" => SelectorGameMode::Survival,
                "creative" => SelectorGameMode::Creative,
                "adventure" => SelectorGameMode::Adventure,
                "spectator" => SelectorGameMode::Spectator,
                _ => return Err(selector_error(reader, "Unknown selector gamemode")),
            };
            SelectorFilter::GameMode(mode, negated)
        }
        "team" => SelectorFilter::Team(reader.read_unquoted_string(), negated),
        "type" => {
            let tag = selector_take(reader, '#');
            reader.skip_whitespace();
            let id = read_selector_resource(reader)?;
            if !tag && !entity_type_exists(&id) {
                return Err(selector_error(
                    reader,
                    format!("Unknown entity type '{id}'"),
                ));
            }
            SelectorFilter::Type { id, tag, negated }
        }
        "tag" => SelectorFilter::Tag(reader.read_unquoted_string(), negated),
        "nbt" => {
            let start = reader.cursor();
            let value = SelectorNbt(read_selector_snbt_compound(reader, 0)?);
            SelectorFilter::Nbt {
                raw: reader.string()[start..reader.cursor()].to_owned(),
                value,
                negated,
            }
        }
        "scores" => SelectorFilter::Scores(read_selector_map(reader, word, |reader| {
            read_selector_range(reader, None, false)
        })?),
        "advancements" => SelectorFilter::Advancements(read_selector_map(
            reader,
            read_selector_resource,
            |reader| {
                if reader.peek() == Some('{') {
                    read_selector_map(reader, word, StringReader::read_boolean)
                        .map(SelectorAdvancement::Criteria)
                } else {
                    reader.read_boolean().map(SelectorAdvancement::Done)
                }
            },
        )?),
        "predicate" => SelectorFilter::Predicate(read_selector_resource(reader)?, negated),
        _ => unreachable!("non-filter selector option"),
    })
}

/// A parsed SNBT compound for selector queries or entity snapshots.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectorNbt(SnbtValue);

impl SelectorNbt {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut reader = StringReader::new(text);
        reader.skip_whitespace();
        let value = read_selector_snbt_compound(&mut reader, 0).map_err(|err| err.to_string())?;
        reader.skip_whitespace();
        if reader.can_read() {
            return Err(
                selector_error(&reader, "Unexpected input after SNBT compound").to_string(),
            );
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, PartialEq)]
enum SnbtValue {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Text(String),
    List(Vec<Self>),
    Compound(HashMap<String, Self>),
    Array(SnbtKind, Vec<Self>),
}

impl SnbtValue {
    fn kind(&self) -> SnbtKind {
        match self {
            Self::Byte(_) => SnbtKind::Byte,
            Self::Short(_) => SnbtKind::Short,
            Self::Int(_) => SnbtKind::Int,
            Self::Long(_) => SnbtKind::Long,
            Self::Float(_) => SnbtKind::Float,
            Self::Double(_) => SnbtKind::Double,
            Self::Text(_) => SnbtKind::Text,
            Self::List(_) => SnbtKind::List,
            Self::Compound(_) => SnbtKind::Compound,
            Self::Array(kind, _) => *kind,
        }
    }

    // Vanilla compares compounds by subset. A list pattern is unordered, requires
    // enough elements, and may reuse the same matching entry. Typed arrays are exact.
    fn matches(&self, actual: &Self) -> bool {
        match (self, actual) {
            (Self::Compound(expected), Self::Compound(actual)) => expected
                .iter()
                .all(|(key, value)| actual.get(key).is_some_and(|actual| value.matches(actual))),
            (Self::List(expected), Self::List(actual)) if expected.is_empty() => actual.is_empty(),
            (Self::List(expected), Self::List(actual)) => {
                expected.len() <= actual.len()
                    && expected
                        .iter()
                        .all(|item| actual.iter().any(|actual| item.matches(actual)))
            }
            _ => self == actual,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnbtKind {
    Byte,
    Short,
    Int,
    Long,
    Float,
    Double,
    Text,
    List,
    Compound,
    ByteArray,
    IntArray,
    LongArray,
}

fn read_selector_snbt_compound(
    reader: &mut StringReader,
    depth: usize,
) -> Result<SnbtValue, CommandSyntaxException> {
    reader.expect('{')?;
    reader.skip_whitespace();
    let mut entries = HashMap::new();
    while reader.can_read() && reader.peek() != Some('}') {
        let key = reader.read_string()?;
        if key.is_empty() {
            return Err(selector_error(reader, "Expected an SNBT key"));
        }
        reader.skip_whitespace();
        reader.expect(':')?;
        entries.insert(key, read_selector_snbt_value(reader, depth + 1)?);
        reader.skip_whitespace();
        if !selector_take(reader, ',') {
            break;
        }
        reader.skip_whitespace();
    }
    reader.expect('}')?;
    Ok(SnbtValue::Compound(entries))
}

fn read_selector_snbt_value(
    reader: &mut StringReader,
    depth: usize,
) -> Result<SnbtValue, CommandSyntaxException> {
    if depth > 512 {
        return Err(selector_error(reader, "SNBT nesting exceeds 512 levels"));
    }
    reader.skip_whitespace();
    match reader.peek() {
        Some('{') => read_selector_snbt_compound(reader, depth),
        Some('[') => read_selector_snbt_list(reader, depth),
        Some(c) if StringReader::is_quoted_string_start(c) => {
            reader.read_quoted_string().map(SnbtValue::Text)
        }
        _ => {
            let value = reader.read_unquoted_string();
            if value.is_empty() {
                return Err(selector_error(reader, "Expected an SNBT value"));
            }
            Ok(snbt_literal(value))
        }
    }
}

fn read_selector_snbt_list(
    reader: &mut StringReader,
    depth: usize,
) -> Result<SnbtValue, CommandSyntaxException> {
    use SnbtKind::*;

    reader.expect('[')?;
    let (kind, mut element_kind) = if reader.peek_at(1) == Some(';')
        && !reader
            .peek()
            .is_some_and(StringReader::is_quoted_string_start)
    {
        let kinds = match reader.read() {
            Some('B') => (ByteArray, Some(Byte)),
            Some('I') => (IntArray, Some(Int)),
            Some('L') => (LongArray, Some(Long)),
            _ => return Err(selector_error(reader, "Unknown SNBT array type")),
        };
        reader.skip();
        kinds
    } else {
        (List, None)
    };
    reader.skip_whitespace();
    let mut entries = Vec::new();
    while reader.can_read() && reader.peek() != Some(']') {
        let item = read_selector_snbt_value(reader, depth + 1)?;
        if *element_kind.get_or_insert(item.kind()) != item.kind() {
            return Err(selector_error(
                reader,
                "Mixed element types in SNBT list or array",
            ));
        }
        entries.push(item);
        reader.skip_whitespace();
        if !selector_take(reader, ',') {
            break;
        }
        reader.skip_whitespace();
    }
    reader.expect(']')?;
    Ok(if kind == List {
        SnbtValue::List(entries)
    } else {
        SnbtValue::Array(kind, entries)
    })
}

fn snbt_literal(value: String) -> SnbtValue {
    use SnbtValue::*;

    fn unsigned(text: &str) -> &str {
        text.strip_prefix(['-', '+']).unwrap_or(text)
    }

    fn digits(text: &str) -> bool {
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
    }

    fn integer(text: &str) -> bool {
        let text = unsigned(text);
        digits(text) && (text == "0" || !text.starts_with('0'))
    }

    fn decimal(text: &str, dot_required: bool) -> bool {
        let (mantissa, exponent) = text
            .split_once(['e', 'E'])
            .map_or((text, None), |(base, exp)| (base, Some(exp)));
        if exponent.is_some_and(|exp| !digits(unsigned(exp))) {
            return false;
        }
        let mantissa = unsigned(mantissa);
        if let Some((whole, fraction)) = mantissa.split_once('.') {
            (digits(whole) && (fraction.is_empty() || digits(fraction)))
                || (whole.is_empty() && digits(fraction))
        } else {
            !dot_required && digits(mantissa)
        }
    }

    if value.eq_ignore_ascii_case("true") {
        return Byte(1);
    }
    if value.eq_ignore_ascii_case("false") {
        return Byte(0);
    }
    let (number, suffix) = value.split_at(value.len() - 1);
    let parsed = match suffix.as_bytes()[0].to_ascii_lowercase() {
        b'b' if integer(number) => number.parse().ok().map(Byte),
        b's' if integer(number) => number.parse().ok().map(Short),
        b'l' if integer(number) => number.parse().ok().map(Long),
        b'f' if decimal(number, false) => number.parse().ok().map(Float),
        b'd' if decimal(number, false) => number.parse().ok().map(Double),
        _ if integer(&value) => value.parse().ok().map(Int),
        _ if decimal(&value, true) => value.parse().ok().map(Double),
        _ => None,
    };
    parsed.unwrap_or(Text(value))
}

#[derive(Clone, Debug)]
pub(crate) struct SelectorSource<'a> {
    pub position: [f64; 3],
    pub dimension: &'a str,
    pub executor: Option<i32>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SelectorCandidate<'a> {
    pub id: i32,
    pub name: &'a str,
    /// Canonical resource location, including its namespace.
    pub entity_type: &'a str,
    pub position: [f64; 3],
    pub dimension: &'a str,
    /// Absolute hitbox corners in min/max order. Without a hitbox, test the position
    /// against the volume's lower-inclusive, upper-exclusive bounds.
    pub bounds: Option<[[f64; 3]; 2]>,
    pub alive: bool,
    pub living: bool,
    pub pitch: f32,
    pub yaw: f32,
    pub level: Option<i32>,
    pub gamemode: Option<SelectorGameMode>,
    pub team: Option<&'a str>,
    pub tags: &'a [&'a str],
    /// Canonical type-tag IDs that contain this entity type.
    pub type_tags: &'a [&'a str],
    /// Entity save data used by NBT filters, including SelectedItem when applicable.
    pub nbt: Option<&'a SelectorNbt>,
    pub scores: HashMap<&'a str, i32>,
    pub advancements: HashMap<&'a str, SelectorAdvancementProgress<'a>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SelectorAdvancementProgress<'a> {
    pub done: bool,
    pub criteria: HashMap<&'a str, bool>,
}

impl SelectorFilter {
    fn matches(
        &self,
        candidate: &SelectorCandidate<'_>,
        check_predicate: &mut impl FnMut(&SelectorCandidate<'_>, &str) -> Option<bool>,
    ) -> bool {
        use SelectorFilter::*;

        let is_player = candidate.entity_type == "minecraft:player";
        match self {
            Name(name, negated) => (candidate.name == name) != *negated,
            GameMode(mode, negated) => {
                is_player
                    && candidate
                        .gamemode
                        .is_some_and(|actual| (actual == *mode) != *negated)
            }
            Team(team, negated) => {
                (candidate.living || is_player)
                    && ((candidate.team.unwrap_or("") == team) != *negated)
            }
            Type { id, tag, negated } => {
                let matches = if *tag {
                    candidate.type_tags.contains(&id.as_str())
                } else {
                    candidate.entity_type == id
                };
                matches != *negated
            }
            Tag(tag, negated) => {
                let matches = if tag.is_empty() {
                    candidate.tags.is_empty()
                } else {
                    candidate.tags.contains(&tag.as_str())
                };
                matches != *negated
            }
            Nbt { value, negated, .. } => candidate
                .nbt
                .is_some_and(|actual| value.0.matches(&actual.0) != *negated),
            Predicate(id, negated) => {
                check_predicate(candidate, id).is_some_and(|matches| matches != *negated)
            }
            Scores(scores) => scores.iter().all(|(objective, range)| {
                candidate
                    .scores
                    .get(objective.as_str())
                    .is_some_and(|&score| range.contains(score))
            }),
            Advancements(advancements) => {
                advancements.is_empty()
                    || (is_player
                        && advancements.iter().all(|(id, expected)| {
                            candidate
                                .advancements
                                .get(id.as_str())
                                .is_some_and(|actual| match expected {
                                    SelectorAdvancement::Done(done) => actual.done == *done,
                                    SelectorAdvancement::Criteria(criteria) => {
                                        criteria.iter().all(|(name, done)| {
                                            actual.criteria.get(name.as_str()) == Some(done)
                                        })
                                    }
                                })
                        }))
            }
        }
    }
}

fn selector_angle_matches(range: &SelectorRange<f32>, angle: f32) -> bool {
    fn wrap(angle: f32) -> f32 {
        let angle = angle % 360.0;
        if angle >= 180.0 {
            angle - 360.0
        } else if angle < -180.0 {
            angle + 360.0
        } else {
            angle
        }
    }

    let angle = wrap(angle);
    let min = wrap(range.min.unwrap_or(0.0));
    let max = wrap(range.max.unwrap_or(359.0));
    if min > max {
        angle >= min || angle <= max
    } else {
        angle >= min && angle <= max
    }
}

fn selector_distance_squared(origin: [f64; 3], position: [f64; 3]) -> f64 {
    let [x, y, z] = std::array::from_fn(|axis| position[axis] - origin[axis]);
    x * x + y * y + z * z
}

/// Filter eligible candidates, order them, then apply the selector's maximum result count.
/// Supply eligible candidates in world iteration order with globally unique IDs. Candidates
/// without NBT or a predicate result also fail negated checks against that data.
/// `random_index(end)` draws a uniform index in `0..end` from the caller's RNG.
pub(crate) fn select_entities<'a, 'data>(
    spec: &SelectorSpec,
    source: &SelectorSource<'_>,
    candidates: &'a [SelectorCandidate<'data>],
    mut check_predicate: impl FnMut(&SelectorCandidate<'_>, &str) -> Option<bool>,
    mut random_index: impl FnMut(usize) -> usize,
) -> Vec<&'a SelectorCandidate<'data>> {
    let origin = std::array::from_fn(|axis| spec.origin[axis].unwrap_or(source.position[axis]));
    let local = spec.origin.iter().any(Option::is_some)
        || spec.delta.iter().any(Option::is_some)
        || spec.distance.is_some();
    let current = spec.variable == SelectorVariable::SelfEntity;
    let volume: Option<[[f64; 3]; 2]> = if spec.delta.iter().any(Option::is_some) {
        let delta = spec.delta.map(|size| size.unwrap_or(0.0));
        Some([
            std::array::from_fn(|axis| origin[axis] + delta[axis].min(0.0)),
            std::array::from_fn(|axis| origin[axis] + (delta[axis].max(0.0) + 1.0)),
        ])
    } else {
        spec.distance
            .as_ref()
            .and_then(|range| range.max)
            .map(|max| {
                [
                    origin.map(|coordinate| coordinate - max),
                    origin.map(|coordinate| coordinate + (max + 1.0)),
                ]
            })
    };
    let distance = spec.distance.as_ref().map(|range| SelectorRange {
        min: range.min.map(|bound| bound * bound),
        max: range.max.map(|bound| bound * bound),
    });

    let mut selected = Vec::new();
    for candidate in candidates {
        let is_player = candidate.entity_type == "minecraft:player";
        if (current && source.executor != Some(candidate.id))
            || (!current && local && candidate.dimension != source.dimension)
            || (spec.variable.players_only() && !is_player)
            || (spec.variable.requires_alive() && !candidate.alive)
            || !spec
                .filters
                .iter()
                .all(|filter| filter.matches(candidate, &mut check_predicate))
            || spec
                .pitch
                .as_ref()
                .is_some_and(|range| !selector_angle_matches(range, candidate.pitch))
            || spec
                .yaw
                .as_ref()
                .is_some_and(|range| !selector_angle_matches(range, candidate.yaw))
            || spec.level.as_ref().is_some_and(|range| {
                !is_player || !candidate.level.is_some_and(|level| range.contains(level))
            })
        {
            continue;
        }
        if let Some([min, max]) = volume {
            let intersects = match candidate.bounds {
                Some([entity_min, entity_max]) => (0..3)
                    .all(|axis| min[axis] < entity_max[axis] && max[axis] > entity_min[axis]),
                None => (0..3).all(|axis| {
                    min[axis] <= candidate.position[axis] && candidate.position[axis] < max[axis]
                }),
            };
            if !intersects {
                continue;
            }
        }
        if distance.as_ref().is_some_and(|range| {
            !range.contains(selector_distance_squared(origin, candidate.position))
        }) {
            continue;
        }
        selected.push(candidate);
        if spec.sort == SelectorSort::Arbitrary && selected.len() == spec.limit {
            break;
        }
    }

    match spec.sort {
        SelectorSort::Nearest => {
            selected.sort_by(|a, b| {
                selector_distance_squared(origin, a.position)
                    .total_cmp(&selector_distance_squared(origin, b.position))
            });
        }
        SelectorSort::Furthest => {
            selected.sort_by(|a, b| {
                selector_distance_squared(origin, b.position)
                    .total_cmp(&selector_distance_squared(origin, a.position))
            });
        }
        SelectorSort::Random => {
            for end in (2..=selected.len()).rev() {
                selected.swap(end - 1, random_index(end));
            }
        }
        SelectorSort::Arbitrary => {}
    }
    selected.truncate(spec.limit);
    selected
}

// ---------------------------------------------------------------------------------------
// Vanilla 1.21.1 /execute subcommand AST and parser
// ---------------------------------------------------------------------------------------

// Grammar cross-checked against the bundled 1.21.1 server jar (the obfuscated class `amt`,
// `javap -p -c amt.class`): `run` redirects to root, `if`/`unless` take the condition tree,
// `positioned`/`rotated`/`facing` each have their `as`/`over`/`entity` children, and `store`
// takes `result`/`success` before the data-holder tree. The data holders and the numeric NBT
// type names come from `aph` and `fl$g` respectively.

/// One numeric coordinate of an `/execute` position or rotation argument. `~` is relative to
/// the execution origin and `^` is local to the executor's rotation, matching vanilla's
/// `CoordinateArgument`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ExecuteCoordinate {
    Absolute(f64),
    Relative(f64),
    Local(f64),
}

impl ExecuteCoordinate {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let (kind, rest) = if let Some(rest) = text.strip_prefix('~') {
            ("relative", rest)
        } else if let Some(rest) = text.strip_prefix('^') {
            ("local", rest)
        } else {
            ("absolute", text)
        };
        let value = if rest.is_empty() {
            0.0
        } else {
            rest.parse::<f64>()
                .map_err(|_| format!("Invalid {kind} coordinate '{text}'"))?
        };
        if !value.is_finite() {
            return Err(format!("Non-finite coordinate '{text}'"));
        }
        Ok(match kind {
            "relative" => Self::Relative(value),
            "local" => Self::Local(value),
            _ => Self::Absolute(value),
        })
    }
}

/// The three coordinates of an `/execute` position, rotation-vector or block-position argument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ExecutePosition {
    pub x: ExecuteCoordinate,
    pub y: ExecuteCoordinate,
    pub z: ExecuteCoordinate,
}

/// `eyes` or `feet`, used by `facing entity` and `anchored`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteAnchor {
    Eyes,
    Feet,
}

/// The command-legal heightmaps (`fj` in the jar filters out the `*_WG` generation variants).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteHeightmap {
    WorldSurface,
    OceanFloor,
    MotionBlocking,
    MotionBlockingNoLeaves,
}

/// The entity relationships accepted by `on <relation>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteRelation {
    Attacker,
    Controller,
    Leasher,
    Origin,
    Owner,
    Passengers,
    Target,
    Vehicle,
}

/// The axis mask accepted by `align <axes>`. Vanilla stores an `EnumSet`, so the spelling
/// order is not significant but duplicate axes are rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecuteAxes {
    pub x: bool,
    pub y: bool,
    pub z: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteBlocksMode {
    All,
    Masked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteScoreComparison {
    LessThan,
    LessEquals,
    Equals,
    GreaterEquals,
    GreaterThan,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExecuteScoreSource {
    pub target: String,
    pub objective: String,
}

/// An inclusive integer range, as accepted by `if score ... matches <range>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecuteRange<T> {
    pub min: Option<T>,
    pub max: Option<T>,
}

impl ExecuteRange<i32> {
    fn parse(text: &str) -> Result<Self, String> {
        let get = |value: &str| -> Result<i32, String> {
            value
                .parse::<i32>()
                .map_err(|_| format!("Invalid integer range bound '{value}'"))
        };
        let (min, max) = match text.split_once("..") {
            Some((low, high)) => (
                if low.is_empty() { None } else { Some(get(low)?) },
                if high.is_empty() {
                    None
                } else {
                    Some(get(high)?)
                },
            ),
            None => {
                let value = get(text)?;
                (Some(value), Some(value))
            }
        };
        if matches!((min, max), (Some(low), Some(high)) if low > high) {
            return Err(format!("Range minimum exceeds maximum in '{text}'"));
        }
        Ok(Self { min, max })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExecuteScoreTest {
    Matches(ExecuteRange<i32>),
    Compare(ExecuteScoreComparison, ExecuteScoreSource),
}

/// The data holder used by `if/unless data` and by `store <result|success>`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExecuteDataTarget {
    Block(ExecutePosition),
    Entity(String),
    Storage(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExecuteConditionKind {
    Block {
        pos: ExecutePosition,
        predicate: String,
    },
    Blocks {
        start: ExecutePosition,
        end: ExecutePosition,
        destination: ExecutePosition,
        mode: ExecuteBlocksMode,
    },
    Entity {
        targets: String,
    },
    Score {
        target: String,
        objective: String,
        test: ExecuteScoreTest,
    },
    Biome {
        pos: ExecutePosition,
        biome: String,
    },
    Dimension {
        dimension: String,
    },
    Loaded {
        pos: ExecutePosition,
    },
    Predicate {
        id: String,
    },
    Data {
        target: ExecuteDataTarget,
        path: String,
        nbt: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteCheck {
    If,
    Unless,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExecuteConditional {
    pub check: ExecuteCheck,
    pub kind: ExecuteConditionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteStoreKind {
    Result,
    Success,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteNbtType {
    Byte,
    Short,
    Int,
    Long,
    Float,
    Double,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecuteBossbarField {
    Value,
    Max,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExecuteStoreTarget {
    Block {
        pos: ExecutePosition,
        path: String,
        nbt_type: ExecuteNbtType,
        scale: f64,
    },
    Bossbar {
        id: String,
        field: ExecuteBossbarField,
    },
    Entity {
        target: String,
        path: String,
        nbt_type: ExecuteNbtType,
        scale: f64,
    },
    Score {
        targets: String,
        objective: String,
    },
    Storage {
        id: String,
        path: String,
        nbt_type: ExecuteNbtType,
        scale: f64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExecuteStore {
    pub kind: ExecuteStoreKind,
    pub target: ExecuteStoreTarget,
}

/// A context-modifying, conditional or storing clause of an `/execute` pipeline.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExecuteClause {
    Modifier(ExecuteModifier),
    Condition(ExecuteConditional),
    Store(ExecuteStore),
}

/// The non-terminal `/execute` subcommands, in the order they appear in the pipeline.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExecuteModifier {
    As {
        targets: String,
    },
    At {
        targets: String,
    },
    Positioned {
        pos: ExecutePosition,
    },
    PositionedAs {
        targets: String,
    },
    PositionedOver {
        heightmap: ExecuteHeightmap,
    },
    Rotated {
        yaw: ExecuteCoordinate,
        pitch: ExecuteCoordinate,
    },
    RotatedAs {
        targets: String,
    },
    Facing {
        pos: ExecutePosition,
    },
    FacingEntity {
        targets: String,
        anchor: ExecuteAnchor,
    },
    Align {
        axes: ExecuteAxes,
    },
    Anchored {
        anchor: ExecuteAnchor,
    },
    In {
        dimension: String,
    },
    Summon {
        entity: String,
    },
    On {
        relation: ExecuteRelation,
    },
}

/// A parsed `/execute` pipeline. `clauses` keeps source order; `run` is the verbatim command
/// text after `run`. A pipeline that ends in `if`/`unless` without `run` has `run == None`
/// and is a valid terminal check. The optional leading `/execute` of a chat command is
/// accepted and stripped.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExecuteCommand {
    pub clauses: Vec<ExecuteClause>,
    pub run: Option<String>,
}

/// A command argument together with its end offset in the original input. The offset keeps the
/// text after `run` byte-exact (including quotes and repeated spaces).
#[derive(Clone, Debug)]
struct ExecuteArg {
    text: String,
    end: usize,
}

fn split_execute_args(input: &str) -> Result<Vec<ExecuteArg>, String> {
    let texts = split_command_args_preserving_quotes(input)?;
    let mut cursor = 0usize;
    let mut args = Vec::with_capacity(texts.len());
    for text in texts {
        let offset = input[cursor..]
            .find(&text)
            .ok_or_else(|| format!("Could not locate argument '{text}'"))?;
        cursor += offset + text.len();
        args.push(ExecuteArg {
            text,
            end: cursor,
        });
    }
    Ok(args)
}

fn resource_id(raw: &str) -> String {
    match raw.split_once(':') {
        Some((namespace, path)) if !namespace.is_empty() => format!("{namespace}:{path}"),
        _ => format!("minecraft:{raw}"),
    }
}

struct ExecuteParser<'a> {
    input: &'a str,
    args: Vec<ExecuteArg>,
    index: usize,
}

impl ExecuteParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.index).map(|arg| arg.text.as_str())
    }

    fn take(&mut self, what: &str) -> Result<String, String> {
        match self.args.get(self.index) {
            Some(arg) => {
                let text = arg.text.clone();
                self.index += 1;
                Ok(text)
            }
            None => Err(format!("Expected {what}")),
        }
    }

    fn read_targets(&mut self) -> Result<String, String> {
        self.take("entity targets")
    }

    fn read_position(&mut self) -> Result<ExecutePosition, String> {
        let x = ExecuteCoordinate::parse(&self.take("x coordinate")?)?;
        let y = ExecuteCoordinate::parse(&self.take("y coordinate")?)?;
        let z = ExecuteCoordinate::parse(&self.take("z coordinate")?)?;
        Ok(ExecutePosition { x, y, z })
    }

    fn read_rotation(&mut self) -> Result<(ExecuteCoordinate, ExecuteCoordinate), String> {
        let yaw = ExecuteCoordinate::parse(&self.take("yaw")?)?;
        let pitch = ExecuteCoordinate::parse(&self.take("pitch")?)?;
        Ok((yaw, pitch))
    }

    fn read_anchor(&mut self) -> Result<ExecuteAnchor, String> {
        match self.take("anchor")?.as_str() {
            "eyes" => Ok(ExecuteAnchor::Eyes),
            "feet" => Ok(ExecuteAnchor::Feet),
            other => Err(format!("Invalid anchor '{other}'")),
        }
    }

    fn read_heightmap(&mut self) -> Result<ExecuteHeightmap, String> {
        match self.take("heightmap")?.to_ascii_lowercase().as_str() {
            "world_surface" => Ok(ExecuteHeightmap::WorldSurface),
            "ocean_floor" => Ok(ExecuteHeightmap::OceanFloor),
            "motion_blocking" => Ok(ExecuteHeightmap::MotionBlocking),
            "motion_blocking_no_leaves" => Ok(ExecuteHeightmap::MotionBlockingNoLeaves),
            other => Err(format!("Invalid heightmap '{other}'")),
        }
    }

    fn read_axes(&mut self) -> Result<ExecuteAxes, String> {
        let text = self.take("axes")?;
        let mut axes = ExecuteAxes {
            x: false,
            y: false,
            z: false,
        };
        if text.is_empty() {
            return Err("Expected at least one alignment axis".into());
        }
        for axis in text.chars() {
            let slot = match axis {
                'x' => &mut axes.x,
                'y' => &mut axes.y,
                'z' => &mut axes.z,
                other => return Err(format!("Invalid alignment axis '{other}'")),
            };
            if *slot {
                return Err(format!("Duplicate alignment axis '{axis}'"));
            }
            *slot = true;
        }
        Ok(axes)
    }

    fn read_relation(&mut self) -> Result<ExecuteRelation, String> {
        use ExecuteRelation::*;
        match self.take("entity relation")?.as_str() {
            "attacker" => Ok(Attacker),
            "controller" => Ok(Controller),
            "leasher" => Ok(Leasher),
            "origin" => Ok(Origin),
            "owner" => Ok(Owner),
            "passengers" => Ok(Passengers),
            "target" => Ok(Target),
            "vehicle" => Ok(Vehicle),
            other => Err(format!("Invalid entity relation '{other}'")),
        }
    }

    fn read_condition(&mut self, check: ExecuteCheck) -> Result<ExecuteConditional, String> {
        let word = self.take("condition")?;
        let kind = match word.as_str() {
            "block" => ExecuteConditionKind::Block {
                pos: self.read_position()?,
                predicate: self.take("block predicate")?,
            },
            "blocks" => ExecuteConditionKind::Blocks {
                start: self.read_position()?,
                end: self.read_position()?,
                destination: self.read_position()?,
                mode: match self.take("mask mode")?.as_str() {
                    "all" => ExecuteBlocksMode::All,
                    "masked" => ExecuteBlocksMode::Masked,
                    other => return Err(format!("Invalid mask mode '{other}'")),
                },
            },
            "entity" => ExecuteConditionKind::Entity {
                targets: self.read_targets()?,
            },
            "score" => {
                let target = self.take("score target")?;
                let objective = self.take("score objective")?;
                let comparison = self.take("score comparison")?;
                let test = if comparison == "matches" {
                    ExecuteScoreTest::Matches(ExecuteRange::parse(&self.take("score range")?)?)
                } else {
                    let comparison = match comparison.as_str() {
                        "<" => ExecuteScoreComparison::LessThan,
                        "<=" => ExecuteScoreComparison::LessEquals,
                        "=" => ExecuteScoreComparison::Equals,
                        ">=" => ExecuteScoreComparison::GreaterEquals,
                        ">" => ExecuteScoreComparison::GreaterThan,
                        other => return Err(format!("Invalid score comparison '{other}'")),
                    };
                    ExecuteScoreTest::Compare(
                        comparison,
                        ExecuteScoreSource {
                            target: self.take("source target")?,
                            objective: self.take("source objective")?,
                        },
                    )
                };
                ExecuteConditionKind::Score {
                    target,
                    objective,
                    test,
                }
            }
            "biome" => ExecuteConditionKind::Biome {
                pos: self.read_position()?,
                biome: resource_id(&self.take("biome")?),
            },
            "dimension" => ExecuteConditionKind::Dimension {
                dimension: resource_id(&self.take("dimension")?),
            },
            "loaded" => ExecuteConditionKind::Loaded {
                pos: self.read_position()?,
            },
            "predicate" => ExecuteConditionKind::Predicate {
                id: resource_id(&self.take("predicate id")?),
            },
            "data" => {
                let source = self.take("data source")?;
                let target = match source.as_str() {
                    "block" => ExecuteDataTarget::Block(self.read_position()?),
                    "entity" => ExecuteDataTarget::Entity(self.take("entity target")?),
                    "storage" => ExecuteDataTarget::Storage(resource_id(&self.take("storage id")?)),
                    other => return Err(format!("Invalid data source '{other}'")),
                };
                let path = self.take("data path")?;
                let nbt = match self.peek() {
                    Some(token) if token.starts_with('{') || token.starts_with("!{") => {
                        Some(self.take("nbt predicate")?)
                    }
                    _ => None,
                };
                ExecuteConditionKind::Data { target, path, nbt }
            }
            other => return Err(format!("Unknown execute condition '{other}'")),
        };
        Ok(ExecuteConditional { check, kind })
    }

    fn read_nbt_type(&mut self) -> Result<ExecuteNbtType, String> {
        match self.take("nbt type")?.as_str() {
            "byte" => Ok(ExecuteNbtType::Byte),
            "short" => Ok(ExecuteNbtType::Short),
            "int" => Ok(ExecuteNbtType::Int),
            "long" => Ok(ExecuteNbtType::Long),
            "float" => Ok(ExecuteNbtType::Float),
            "double" => Ok(ExecuteNbtType::Double),
            other => Err(format!("Invalid numeric nbt type '{other}'")),
        }
    }

    fn read_scale(&mut self) -> Result<f64, String> {
        let text = self.take("scale")?;
        let value = text
            .parse::<f64>()
            .map_err(|_| format!("Invalid scale '{text}'"))?;
        if !value.is_finite() {
            return Err(format!("Non-finite scale '{text}'"));
        }
        Ok(value)
    }

    fn read_store(&mut self) -> Result<ExecuteStore, String> {
        let kind = match self.take("store kind")?.as_str() {
            "result" => ExecuteStoreKind::Result,
            "success" => ExecuteStoreKind::Success,
            other => return Err(format!("Invalid store kind '{other}'")),
        };
        let source = self.take("store target")?;
        let target = match source.as_str() {
            "score" => ExecuteStoreTarget::Score {
                targets: self.take("score targets")?,
                objective: self.take("score objective")?,
            },
            "bossbar" => {
                let id = self.take("bossbar id")?;
                let field = match self.take("bossbar field")?.as_str() {
                    "value" => ExecuteBossbarField::Value,
                    "max" => ExecuteBossbarField::Max,
                    other => return Err(format!("Invalid bossbar field '{other}'")),
                };
                ExecuteStoreTarget::Bossbar { id, field }
            }
            "block" => ExecuteStoreTarget::Block {
                pos: self.read_position()?,
                path: self.take("data path")?,
                nbt_type: self.read_nbt_type()?,
                scale: self.read_scale()?,
            },
            "entity" => ExecuteStoreTarget::Entity {
                target: self.take("entity target")?,
                path: self.take("data path")?,
                nbt_type: self.read_nbt_type()?,
                scale: self.read_scale()?,
            },
            "storage" => ExecuteStoreTarget::Storage {
                id: resource_id(&self.take("storage id")?),
                path: self.take("data path")?,
                nbt_type: self.read_nbt_type()?,
                scale: self.read_scale()?,
            },
            other => return Err(format!("Invalid store target '{other}'")),
        };
        Ok(ExecuteStore { kind, target })
    }

    fn read_modifier(&mut self, word: &str) -> Result<ExecuteModifier, String> {
        match word {
            "as" => Ok(ExecuteModifier::As {
                targets: self.read_targets()?,
            }),
            "at" => Ok(ExecuteModifier::At {
                targets: self.read_targets()?,
            }),
            "positioned" => match self.peek() {
                Some("as") => {
                    self.index += 1;
                    Ok(ExecuteModifier::PositionedAs {
                        targets: self.read_targets()?,
                    })
                }
                Some("over") => {
                    self.index += 1;
                    Ok(ExecuteModifier::PositionedOver {
                        heightmap: self.read_heightmap()?,
                    })
                }
                _ => Ok(ExecuteModifier::Positioned {
                    pos: self.read_position()?,
                }),
            },
            "rotated" => match self.peek() {
                Some("as") => {
                    self.index += 1;
                    Ok(ExecuteModifier::RotatedAs {
                        targets: self.read_targets()?,
                    })
                }
                _ => {
                    let (yaw, pitch) = self.read_rotation()?;
                    Ok(ExecuteModifier::Rotated { yaw, pitch })
                }
            },
            "facing" => match self.peek() {
                Some("entity") => {
                    self.index += 1;
                    Ok(ExecuteModifier::FacingEntity {
                        targets: self.read_targets()?,
                        anchor: self.read_anchor()?,
                    })
                }
                _ => Ok(ExecuteModifier::Facing {
                    pos: self.read_position()?,
                }),
            },
            "align" => Ok(ExecuteModifier::Align {
                axes: self.read_axes()?,
            }),
            "anchored" => Ok(ExecuteModifier::Anchored {
                anchor: self.read_anchor()?,
            }),
            "in" => Ok(ExecuteModifier::In {
                dimension: resource_id(&self.take("dimension")?),
            }),
            "summon" => Ok(ExecuteModifier::Summon {
                entity: resource_id(&self.take("entity type")?),
            }),
            "on" => Ok(ExecuteModifier::On {
                relation: self.read_relation()?,
            }),
            other => Err(format!("Unknown execute subcommand '{other}'")),
        }
    }
}

impl ExecuteCommand {
    /// Parse a complete `/execute` command. Both `execute ...` and `/execute ...` are accepted;
    /// the pipeline may end at `run <command>` or at a terminal `if`/`unless` check.
    pub(crate) fn parse(input: &str) -> Result<Self, String> {
        let mut parser = ExecuteParser {
            input,
            args: split_execute_args(input)?,
            index: 0,
        };
        if let Some(first) = parser.peek() {
            let first = first.strip_prefix('/').unwrap_or(first).to_owned();
            if first == "execute" {
                parser.index += 1;
            }
        }
        let mut clauses = Vec::new();
        while let Some(word) = parser.peek().map(str::to_owned) {
            let clause = match word.as_str() {
                "run" => {
                    parser.index += 1;
                    let command = parser.input[parser.args[parser.index - 1].end..]
                        .trim()
                        .to_owned();
                    if command.is_empty() {
                        return Err("Expected a command after 'run'".into());
                    }
                    return Ok(Self {
                        clauses,
                        run: Some(command),
                    });
                }
                "if" | "unless" => {
                    parser.index += 1;
                    let check = if word == "if" {
                        ExecuteCheck::If
                    } else {
                        ExecuteCheck::Unless
                    };
                    ExecuteClause::Condition(parser.read_condition(check)?)
                }
                "store" => {
                    parser.index += 1;
                    ExecuteClause::Store(parser.read_store()?)
                }
                other => {
                    parser.index += 1;
                    ExecuteClause::Modifier(parser.read_modifier(other)?)
                }
            };
            clauses.push(clause);
        }
        // Vanilla's `execute` node is not executable on its own: a pipeline without `run`
        // must end in a condition that acts as the terminal check.
        if !matches!(clauses.last(), Some(ExecuteClause::Condition(_))) {
            return Err("Expected 'run' or a terminal 'if'/'unless' condition".into());
        }
        Ok(Self {
            clauses,
            run: None,
        })
    }
}

// ---------------------------------------------------------------------------------------
// StringRange
// ---------------------------------------------------------------------------------------

/// `com.mojang.brigadier.context.StringRange`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StringRange {
    start: usize,
    end: usize,
}

impl StringRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn at(pos: usize) -> Self {
        Self::new(pos, pos)
    }

    pub fn between(start: usize, end: usize) -> Self {
        Self::new(start, end)
    }

    pub fn encompassing(a: StringRange, b: StringRange) -> Self {
        Self::new(a.start.min(b.start), a.end.max(b.end))
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn end(&self) -> usize {
        self.end
    }

    pub fn get(&self, input: &str) -> String {
        input[self.start.min(input.len())..self.end.min(input.len())].to_string()
    }

    pub fn get_from(&self, reader: &StringReader) -> String {
        reader.slice(self.start, self.end).to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }
}

impl fmt::Display for StringRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StringRange{{start={}, end={}}}", self.start, self.end)
    }
}

// ---------------------------------------------------------------------------------------
// Exceptions
// ---------------------------------------------------------------------------------------

/// Which of Brigadier's built-in exception types produced a [`CommandSyntaxException`].
///
/// Java identifies them by object identity (`BUILT_IN_EXCEPTIONS.integerTooLow()` and friends,
/// an overridable `BuiltInExceptionProvider`). The provider's method names are kept verbatim in
/// [`CommandExceptionType::id`] so a port of the message layer (vanilla's translatable keys)
/// can map one to one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommandExceptionType {
    DoubleTooLow,
    DoubleTooHigh,
    FloatTooLow,
    FloatTooHigh,
    IntegerTooLow,
    IntegerTooHigh,
    LongTooLow,
    LongTooHigh,
    LiteralIncorrect,
    ReaderExpectedStartOfQuote,
    ReaderExpectedEndOfQuote,
    ReaderInvalidEscape,
    ReaderInvalidBool,
    ReaderInvalidInt,
    ReaderExpectedInt,
    ReaderInvalidLong,
    ReaderExpectedLong,
    ReaderInvalidDouble,
    ReaderExpectedDouble,
    ReaderInvalidFloat,
    ReaderExpectedFloat,
    ReaderExpectedBool,
    ReaderExpectedSymbol,
    DispatcherUnknownCommand,
    DispatcherUnknownArgument,
    DispatcherExpectedArgumentSeparator,
    DispatcherParseException,
}

impl CommandExceptionType {
    /// The `BuiltInExceptionProvider` method name that produces this type.
    pub fn id(&self) -> &'static str {
        match self {
            Self::DoubleTooLow => "doubleTooLow",
            Self::DoubleTooHigh => "doubleTooHigh",
            Self::FloatTooLow => "floatTooLow",
            Self::FloatTooHigh => "floatTooHigh",
            Self::IntegerTooLow => "integerTooLow",
            Self::IntegerTooHigh => "integerTooHigh",
            Self::LongTooLow => "longTooLow",
            Self::LongTooHigh => "longTooHigh",
            Self::LiteralIncorrect => "literalIncorrect",
            Self::ReaderExpectedStartOfQuote => "readerExpectedStartOfQuote",
            Self::ReaderExpectedEndOfQuote => "readerExpectedEndOfQuote",
            Self::ReaderInvalidEscape => "readerInvalidEscape",
            Self::ReaderInvalidBool => "readerInvalidBool",
            Self::ReaderInvalidInt => "readerInvalidInt",
            Self::ReaderExpectedInt => "readerExpectedInt",
            Self::ReaderInvalidLong => "readerInvalidLong",
            Self::ReaderExpectedLong => "readerExpectedLong",
            Self::ReaderInvalidDouble => "readerInvalidDouble",
            Self::ReaderExpectedDouble => "readerExpectedDouble",
            Self::ReaderInvalidFloat => "readerInvalidFloat",
            Self::ReaderExpectedFloat => "readerExpectedFloat",
            Self::ReaderExpectedBool => "readerExpectedBool",
            Self::ReaderExpectedSymbol => "readerExpectedSymbol",
            Self::DispatcherUnknownCommand => "dispatcherUnknownCommand",
            Self::DispatcherUnknownArgument => "dispatcherUnknownArgument",
            Self::DispatcherExpectedArgumentSeparator => "dispatcherExpectedArgumentSeparator",
            Self::DispatcherParseException => "dispatcherParseException",
        }
    }
}

/// `com.mojang.brigadier.exceptions.CommandSyntaxException`.
///
/// Carries the same four things the Java class does: the exception type, the raw message, the
/// input it was created against, and the cursor. `cursor` is `-1` when the exception was made
/// without a reader (`create()`), which is what suppresses the ` at position N: ...` suffix -
/// that suffix and its `<--[HERE]` context window are what a vanilla client is shown.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandSyntaxException {
    kind: CommandExceptionType,
    message: String,
    input: Option<String>,
    cursor: i32,
}

impl CommandSyntaxException {
    /// `CONTEXT_AMOUNT`: characters of input shown before the `<--[HERE]` marker.
    pub const CONTEXT_AMOUNT: usize = 10;

    pub fn new(kind: CommandExceptionType, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            input: None,
            cursor: -1,
        }
    }

    pub fn with_context(
        kind: CommandExceptionType,
        message: impl Into<String>,
        input: &str,
        cursor: usize,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            input: Some(input.to_string()),
            cursor: cursor as i32,
        }
    }

    pub fn kind(&self) -> CommandExceptionType {
        self.kind
    }

    /// `getRawMessage`: the message without the position suffix.
    pub fn raw_message(&self) -> &str {
        &self.message
    }

    /// `getInput`.
    pub fn input(&self) -> Option<&str> {
        self.input.as_deref()
    }

    /// `getCursor`; `-1` when the exception carries no position.
    pub fn cursor(&self) -> i32 {
        self.cursor
    }

    /// `getContext`: the `<--[HERE]` window, or `None` without input/position.
    pub fn context(&self) -> Option<String> {
        let input = self.input.as_deref()?;
        if self.cursor < 0 {
            return None;
        }
        let mut builder = String::new();
        let cursor = (self.cursor as usize).min(input.len());
        let cursor = floor_char_boundary(input, cursor);
        if cursor > Self::CONTEXT_AMOUNT {
            builder.push_str("...");
        }
        let window_start = floor_char_boundary(input, cursor.saturating_sub(Self::CONTEXT_AMOUNT));
        builder.push_str(&input[window_start..cursor]);
        builder.push_str("<--[HERE]");
        Some(builder)
    }

    /// `getMessage`: the raw message plus, when known, the position and the context window.
    pub fn full_message(&self) -> String {
        let mut message = self.message.clone();
        if let Some(context) = self.context() {
            message.push_str(&format!(" at position {}: {}", self.cursor, context));
        }
        message
    }
}

impl fmt::Display for CommandSyntaxException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full_message())
    }
}

impl std::error::Error for CommandSyntaxException {}

/// `SimpleCommandExceptionType`: one fixed message, no arguments.
#[derive(Clone, Copy, Debug)]
pub struct SimpleCommandExceptionType {
    kind: CommandExceptionType,
    message: &'static str,
}

impl SimpleCommandExceptionType {
    pub const fn new(kind: CommandExceptionType, message: &'static str) -> Self {
        Self { kind, message }
    }

    /// `create()`: no input, no position.
    pub fn create(&self) -> CommandSyntaxException {
        CommandSyntaxException::new(self.kind, self.message)
    }

    pub fn create_with_context(&self, reader: &StringReader) -> CommandSyntaxException {
        CommandSyntaxException::with_context(
            self.kind,
            self.message,
            reader.string(),
            reader.cursor(),
        )
    }

    pub fn kind(&self) -> CommandExceptionType {
        self.kind
    }

    pub fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for SimpleCommandExceptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

/// `DynamicCommandExceptionType`: a message built from one argument.
#[derive(Clone, Copy, Debug)]
pub struct DynamicCommandExceptionType {
    kind: CommandExceptionType,
    make: fn(&str) -> String,
}

impl DynamicCommandExceptionType {
    pub const fn new(kind: CommandExceptionType, make: fn(&str) -> String) -> Self {
        Self { kind, make }
    }

    pub fn create(&self, arg: &str) -> CommandSyntaxException {
        CommandSyntaxException::new(self.kind, (self.make)(arg))
    }

    pub fn create_with_context(&self, reader: &StringReader, arg: &str) -> CommandSyntaxException {
        CommandSyntaxException::with_context(
            self.kind,
            (self.make)(arg),
            reader.string(),
            reader.cursor(),
        )
    }

    pub fn kind(&self) -> CommandExceptionType {
        self.kind
    }
}

/// `Dynamic2CommandExceptionType`: a message built from two arguments.
///
/// Java passes two `Object`s and the provider concatenates them; here the caller formats them,
/// which is the same text with the box gone.
#[derive(Clone, Copy, Debug)]
pub struct Dynamic2CommandExceptionType {
    kind: CommandExceptionType,
    make: fn(&str, &str) -> String,
}

impl Dynamic2CommandExceptionType {
    pub const fn new(kind: CommandExceptionType, make: fn(&str, &str) -> String) -> Self {
        Self { kind, make }
    }

    pub fn create(&self, a: &str, b: &str) -> CommandSyntaxException {
        CommandSyntaxException::new(self.kind, (self.make)(a, b))
    }

    pub fn create_with_context(
        &self,
        reader: &StringReader,
        a: &str,
        b: &str,
    ) -> CommandSyntaxException {
        CommandSyntaxException::with_context(
            self.kind,
            (self.make)(a, b),
            reader.string(),
            reader.cursor(),
        )
    }

    pub fn kind(&self) -> CommandExceptionType {
        self.kind
    }
}

/// The formatting Java's `Double.toString` / `Float.toString` produce for the values these
/// messages carry: an integral value keeps a `.0` tail (`1.0`, not `1`).
fn java_decimal(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e7 {
        return format!("{value:.1}");
    }
    format!("{value:?}")
}

/// `com.mojang.brigadier.exceptions.BuiltInExceptions`, as one const table.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltInExceptions;

pub const BUILT_IN_EXCEPTIONS: BuiltInExceptions = BuiltInExceptions;

impl BuiltInExceptions {
    pub const fn double_too_low(&self) -> Dynamic2CommandExceptionType {
        Dynamic2CommandExceptionType::new(CommandExceptionType::DoubleTooLow, |found, min| {
            format!("Double must not be less than {min}, found {found}")
        })
    }

    pub const fn double_too_high(&self) -> Dynamic2CommandExceptionType {
        Dynamic2CommandExceptionType::new(CommandExceptionType::DoubleTooHigh, |found, max| {
            format!("Double must not be more than {max}, found {found}")
        })
    }

    pub const fn float_too_low(&self) -> Dynamic2CommandExceptionType {
        Dynamic2CommandExceptionType::new(CommandExceptionType::FloatTooLow, |found, min| {
            format!("Float must not be less than {min}, found {found}")
        })
    }

    pub const fn float_too_high(&self) -> Dynamic2CommandExceptionType {
        Dynamic2CommandExceptionType::new(CommandExceptionType::FloatTooHigh, |found, max| {
            format!("Float must not be more than {max}, found {found}")
        })
    }

    pub const fn integer_too_low(&self) -> Dynamic2CommandExceptionType {
        Dynamic2CommandExceptionType::new(CommandExceptionType::IntegerTooLow, |found, min| {
            format!("Integer must not be less than {min}, found {found}")
        })
    }

    pub const fn integer_too_high(&self) -> Dynamic2CommandExceptionType {
        Dynamic2CommandExceptionType::new(CommandExceptionType::IntegerTooHigh, |found, max| {
            format!("Integer must not be more than {max}, found {found}")
        })
    }

    pub const fn long_too_low(&self) -> Dynamic2CommandExceptionType {
        Dynamic2CommandExceptionType::new(CommandExceptionType::LongTooLow, |found, min| {
            format!("Long must not be less than {min}, found {found}")
        })
    }

    pub const fn long_too_high(&self) -> Dynamic2CommandExceptionType {
        Dynamic2CommandExceptionType::new(CommandExceptionType::LongTooHigh, |found, max| {
            format!("Long must not be more than {max}, found {found}")
        })
    }

    pub const fn literal_incorrect(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(CommandExceptionType::LiteralIncorrect, |expected| {
            format!("Expected literal {expected}")
        })
    }

    pub const fn reader_expected_start_of_quote(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(
            CommandExceptionType::ReaderExpectedStartOfQuote,
            "Expected quote to start a string",
        )
    }

    pub const fn reader_expected_end_of_quote(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(
            CommandExceptionType::ReaderExpectedEndOfQuote,
            "Unclosed quoted string",
        )
    }

    pub const fn reader_invalid_escape(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(CommandExceptionType::ReaderInvalidEscape, |character| {
            format!("Invalid escape sequence '{character}' in quoted string")
        })
    }

    pub const fn reader_invalid_bool(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(CommandExceptionType::ReaderInvalidBool, |value| {
            format!("Invalid bool, expected true or false but found '{value}'")
        })
    }

    pub const fn reader_invalid_int(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(CommandExceptionType::ReaderInvalidInt, |value| {
            format!("Invalid integer '{value}'")
        })
    }

    pub const fn reader_expected_int(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(CommandExceptionType::ReaderExpectedInt, "Expected integer")
    }

    pub const fn reader_invalid_long(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(CommandExceptionType::ReaderInvalidLong, |value| {
            format!("Invalid long '{value}'")
        })
    }

    pub const fn reader_expected_long(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(CommandExceptionType::ReaderExpectedLong, "Expected long")
    }

    pub const fn reader_invalid_double(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(CommandExceptionType::ReaderInvalidDouble, |value| {
            format!("Invalid double '{value}'")
        })
    }

    pub const fn reader_expected_double(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(CommandExceptionType::ReaderExpectedDouble, "Expected double")
    }

    pub const fn reader_invalid_float(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(CommandExceptionType::ReaderInvalidFloat, |value| {
            format!("Invalid float '{value}'")
        })
    }

    pub const fn reader_expected_float(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(CommandExceptionType::ReaderExpectedFloat, "Expected float")
    }

    pub const fn reader_expected_bool(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(CommandExceptionType::ReaderExpectedBool, "Expected bool")
    }

    pub const fn reader_expected_symbol(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(CommandExceptionType::ReaderExpectedSymbol, |symbol| {
            format!("Expected '{symbol}'")
        })
    }

    pub const fn dispatcher_unknown_command(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(
            CommandExceptionType::DispatcherUnknownCommand,
            "Unknown command",
        )
    }

    pub const fn dispatcher_unknown_argument(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(
            CommandExceptionType::DispatcherUnknownArgument,
            "Incorrect argument for command",
        )
    }

    pub const fn dispatcher_expected_argument_separator(&self) -> SimpleCommandExceptionType {
        SimpleCommandExceptionType::new(
            CommandExceptionType::DispatcherExpectedArgumentSeparator,
            "Expected whitespace to end one argument, but found trailing data",
        )
    }

    pub const fn dispatcher_parse_exception(&self) -> DynamicCommandExceptionType {
        DynamicCommandExceptionType::new(
            CommandExceptionType::DispatcherParseException,
            |message| format!("Could not parse command: {message}"),
        )
    }
}

// ---------------------------------------------------------------------------------------
// Argument types
// ---------------------------------------------------------------------------------------

/// `com.mojang.brigadier.arguments.ArgumentType<S>`.
///
/// Java's interface is `ArgumentType<T>` - parameterized by the value type, with the source as
/// a method argument. In Rust the value type is erased (`ArgValue`) and the source is the
/// type parameter, so an implementation is written once per source type rather than once per
/// `(source, value)` pair. `parse_with_source` is Java's `parse(reader, source)` overload.
pub trait ArgumentType<S>: Send + Sync {
    fn parse(&self, reader: &mut StringReader) -> Result<ArgumentValue, CommandSyntaxException>;

    fn parse_with_source(
        &self,
        reader: &mut StringReader,
        _source: &S,
    ) -> Result<ArgumentValue, CommandSyntaxException> {
        self.parse(reader)
    }

    fn list_suggestions(
        &self,
        _context: &CommandContext<S>,
        _builder: &mut SuggestionsBuilder,
    ) -> Result<Suggestions, CommandSyntaxException> {
        Ok(Suggestions::empty())
    }

    fn examples(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// A description of this type for consumers that must name it rather than call it.
    ///
    /// Vanilla keys `ArgumentTypeInfos` by the argument type's *class* and the Commands packet
    /// writes that registry id, so a serializer has to know which concrete type it holds. Rust
    /// erases the concrete type behind this trait object, and the Commands packet is the one
    /// consumer that cannot call its way around the erasure. Each implementation owned here
    /// states its own shape; the default is `None`, meaning "this module does not own the type",
    /// which refuses to be described instead of being guessed at.
    fn type_info(&self) -> Option<ArgumentTypeInfo> {
        None
    }

    /// A copy of this type as a boxed trait object, so `CommandNode::create_builder` can build a
    /// fresh node of the same type (Java reuses the type instance).
    fn box_clone(&self) -> Box<dyn ArgumentType<S>>;
}

/// What an [`ArgumentType`] is, without its parser: the measurement that
/// `net.minecraft.commands.synchronization.ArgumentTypeInfo` carries alongside its serializer.
///
/// This is data, not the wire format - the serializer lives in `crate::brigadier_wire`, so this
/// module stays a leaf with no crate-internal dependency. Fields are `Copy` so a node can answer
/// by value without lending its boxed type out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ArgumentTypeInfo {
    Bool,
    Float { minimum: f32, maximum: f32 },
    Double { minimum: f64, maximum: f64 },
    Integer { minimum: i32, maximum: i32 },
    Long { minimum: i64, maximum: i64 },
    String { string_type: StringType },
}

fn boxed<T: Any + Send + Sync>(value: T) -> ArgumentValue {
    ArgumentValue::new(value)
}

/// `IntegerArgumentType`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IntegerArgumentType {
    minimum: i32,
    maximum: i32,
}

impl IntegerArgumentType {
    pub fn integer() -> Self {
        Self::integer_between(i32::MIN, i32::MAX)
    }

    pub fn integer_at_least(minimum: i32) -> Self {
        Self::integer_between(minimum, i32::MAX)
    }

    pub fn integer_between(minimum: i32, maximum: i32) -> Self {
        Self { minimum, maximum }
    }

    pub fn minimum(&self) -> i32 {
        self.minimum
    }

    pub fn maximum(&self) -> i32 {
        self.maximum
    }

    /// `IntegerArgumentType.getInteger(context, name)`.
    pub fn get_integer<S>(context: &CommandContext<S>, name: &str) -> Result<i32, ArgumentError> {
        context.argument::<i32>(name).copied()
    }
}

impl<S> ArgumentType<S> for IntegerArgumentType {
    fn parse(&self, reader: &mut StringReader) -> Result<ArgumentValue, CommandSyntaxException> {
        let start = reader.cursor();
        let result = reader.read_int()?;
        if result < self.minimum {
            reader.set_cursor(start);
            return Err(BUILT_IN_EXCEPTIONS.integer_too_low().create_with_context(
                reader,
                &result.to_string(),
                &self.minimum.to_string(),
            ));
        }
        if result > self.maximum {
            reader.set_cursor(start);
            return Err(BUILT_IN_EXCEPTIONS.integer_too_high().create_with_context(
                reader,
                &result.to_string(),
                &self.maximum.to_string(),
            ));
        }
        Ok(boxed(result))
    }

    fn examples(&self) -> Vec<&'static str> {
        vec!["0", "123", "-123"]
    }
    fn type_info(&self) -> Option<ArgumentTypeInfo> {
        Some(ArgumentTypeInfo::Integer {
            minimum: self.minimum,
            maximum: self.maximum,
        })
    }
    fn box_clone(&self) -> Box<dyn ArgumentType<S>> {
        Box::new(*self)
    }
}

/// `LongArgumentType`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LongArgumentType {
    minimum: i64,
    maximum: i64,
}

impl LongArgumentType {
    pub fn long_arg() -> Self {
        Self::long_between(i64::MIN, i64::MAX)
    }

    pub fn long_at_least(minimum: i64) -> Self {
        Self::long_between(minimum, i64::MAX)
    }

    pub fn long_between(minimum: i64, maximum: i64) -> Self {
        Self { minimum, maximum }
    }

    pub fn minimum(&self) -> i64 {
        self.minimum
    }

    pub fn maximum(&self) -> i64 {
        self.maximum
    }

    /// `LongArgumentType.getLong(context, name)`.
    pub fn get_long<S>(context: &CommandContext<S>, name: &str) -> Result<i64, ArgumentError> {
        context.argument::<i64>(name).copied()
    }
}

impl<S> ArgumentType<S> for LongArgumentType {
    fn parse(&self, reader: &mut StringReader) -> Result<ArgumentValue, CommandSyntaxException> {
        let start = reader.cursor();
        let result = reader.read_long()?;
        if result < self.minimum {
            reader.set_cursor(start);
            return Err(BUILT_IN_EXCEPTIONS.long_too_low().create_with_context(
                reader,
                &result.to_string(),
                &self.minimum.to_string(),
            ));
        }
        if result > self.maximum {
            reader.set_cursor(start);
            return Err(BUILT_IN_EXCEPTIONS.long_too_high().create_with_context(
                reader,
                &result.to_string(),
                &self.maximum.to_string(),
            ));
        }
        Ok(boxed(result))
    }

    fn examples(&self) -> Vec<&'static str> {
        vec!["0", "123", "-123"]
    }
    fn type_info(&self) -> Option<ArgumentTypeInfo> {
        Some(ArgumentTypeInfo::Long {
            minimum: self.minimum,
            maximum: self.maximum,
        })
    }
    fn box_clone(&self) -> Box<dyn ArgumentType<S>> {
        Box::new(*self)
    }
}

/// `DoubleArgumentType`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DoubleArgumentType {
    minimum: f64,
    maximum: f64,
}

impl DoubleArgumentType {
    pub fn double_arg() -> Self {
        Self::double_between(-f64::MAX, f64::MAX)
    }

    pub fn double_at_least(minimum: f64) -> Self {
        Self::double_between(minimum, f64::MAX)
    }

    pub fn double_between(minimum: f64, maximum: f64) -> Self {
        Self { minimum, maximum }
    }

    pub fn minimum(&self) -> f64 {
        self.minimum
    }

    pub fn maximum(&self) -> f64 {
        self.maximum
    }

    /// `DoubleArgumentType.getDouble(context, name)`.
    pub fn get_double<S>(context: &CommandContext<S>, name: &str) -> Result<f64, ArgumentError> {
        context.argument::<f64>(name).copied()
    }
}

impl<S> ArgumentType<S> for DoubleArgumentType {
    fn parse(&self, reader: &mut StringReader) -> Result<ArgumentValue, CommandSyntaxException> {
        let start = reader.cursor();
        let result = reader.read_double()?;
        if result < self.minimum {
            reader.set_cursor(start);
            return Err(BUILT_IN_EXCEPTIONS.double_too_low().create_with_context(
                reader,
                &java_decimal(result),
                &java_decimal(self.minimum),
            ));
        }
        if result > self.maximum {
            reader.set_cursor(start);
            return Err(BUILT_IN_EXCEPTIONS.double_too_high().create_with_context(
                reader,
                &java_decimal(result),
                &java_decimal(self.maximum),
            ));
        }
        Ok(boxed(result))
    }

    fn examples(&self) -> Vec<&'static str> {
        vec!["0", "1.2", ".5", "-1", "-.5", "-1234.56"]
    }
    fn type_info(&self) -> Option<ArgumentTypeInfo> {
        Some(ArgumentTypeInfo::Double {
            minimum: self.minimum,
            maximum: self.maximum,
        })
    }
    fn box_clone(&self) -> Box<dyn ArgumentType<S>> {
        Box::new(*self)
    }
}

/// `FloatArgumentType`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloatArgumentType {
    minimum: f32,
    maximum: f32,
}

impl FloatArgumentType {
    pub fn float_arg() -> Self {
        Self::float_between(-f32::MAX, f32::MAX)
    }

    pub fn float_at_least(minimum: f32) -> Self {
        Self::float_between(minimum, f32::MAX)
    }

    pub fn float_between(minimum: f32, maximum: f32) -> Self {
        Self { minimum, maximum }
    }

    pub fn minimum(&self) -> f32 {
        self.minimum
    }

    pub fn maximum(&self) -> f32 {
        self.maximum
    }

    /// `FloatArgumentType.getFloat(context, name)`.
    pub fn get_float<S>(context: &CommandContext<S>, name: &str) -> Result<f32, ArgumentError> {
        context.argument::<f32>(name).copied()
    }
}

impl<S> ArgumentType<S> for FloatArgumentType {
    fn parse(&self, reader: &mut StringReader) -> Result<ArgumentValue, CommandSyntaxException> {
        let start = reader.cursor();
        let result = reader.read_float()?;
        if result < self.minimum {
            reader.set_cursor(start);
            return Err(BUILT_IN_EXCEPTIONS.float_too_low().create_with_context(
                reader,
                &java_decimal(result as f64),
                &java_decimal(self.minimum as f64),
            ));
        }
        if result > self.maximum {
            reader.set_cursor(start);
            return Err(BUILT_IN_EXCEPTIONS.float_too_high().create_with_context(
                reader,
                &java_decimal(result as f64),
                &java_decimal(self.maximum as f64),
            ));
        }
        Ok(boxed(result))
    }

    fn examples(&self) -> Vec<&'static str> {
        vec!["0", "1.2", ".5", "-1", "-.5", "-1234.56"]
    }
    fn type_info(&self) -> Option<ArgumentTypeInfo> {
        Some(ArgumentTypeInfo::Float {
            minimum: self.minimum,
            maximum: self.maximum,
        })
    }
    fn box_clone(&self) -> Box<dyn ArgumentType<S>> {
        Box::new(*self)
    }
}

/// `BoolArgumentType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoolArgumentType;

impl BoolArgumentType {
    pub fn bool() -> Self {
        Self
    }

    /// `BoolArgumentType.getBool(context, name)`.
    pub fn get_bool<S>(context: &CommandContext<S>, name: &str) -> Result<bool, ArgumentError> {
        context.argument::<bool>(name).copied()
    }
}

impl<S> ArgumentType<S> for BoolArgumentType {
    fn parse(&self, reader: &mut StringReader) -> Result<ArgumentValue, CommandSyntaxException> {
        Ok(boxed(reader.read_boolean()?))
    }

    fn type_info(&self) -> Option<ArgumentTypeInfo> {
        Some(ArgumentTypeInfo::Bool)
    }

    fn box_clone(&self) -> Box<dyn ArgumentType<S>> {
        Box::new(*self)
    }

    fn list_suggestions(
        &self,
        _context: &CommandContext<S>,
        builder: &mut SuggestionsBuilder,
    ) -> Result<Suggestions, CommandSyntaxException> {
        if "true".starts_with(builder.remaining_lower_case()) {
            builder.suggest("true");
        }
        if "false".starts_with(builder.remaining_lower_case()) {
            builder.suggest("false");
        }
        Ok(builder.build())
    }

    fn examples(&self) -> Vec<&'static str> {
        vec!["true", "false"]
    }
}

/// `StringArgumentType.StringType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringType {
    SingleWord,
    QuotablePhrase,
    GreedyPhrase,
}

impl StringType {
    pub fn examples(&self) -> Vec<&'static str> {
        match self {
            Self::SingleWord => vec!["word", "words_with_underscores"],
            Self::QuotablePhrase => vec!["\"quoted phrase\"", "word", "\"\""],
            Self::GreedyPhrase => vec!["word", "words with spaces", "\"and symbols\""],
        }
    }
}

/// `StringArgumentType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringArgumentType {
    kind: StringType,
}

impl StringArgumentType {
    /// `StringArgumentType.word()`.
    pub fn word() -> Self {
        Self {
            kind: StringType::SingleWord,
        }
    }

    /// `StringArgumentType.string()`.
    pub fn string() -> Self {
        Self {
            kind: StringType::QuotablePhrase,
        }
    }

    /// `StringArgumentType.greedyString()`.
    pub fn greedy_string() -> Self {
        Self {
            kind: StringType::GreedyPhrase,
        }
    }

    pub fn kind(&self) -> StringType {
        self.kind
    }

    /// `StringArgumentType.getString(context, name)`.
    pub fn get_string<S>(context: &CommandContext<S>, name: &str) -> Result<String, ArgumentError> {
        context.argument::<String>(name).cloned()
    }

    /// `StringArgumentType.escapeIfRequired`: quote and escape only when the text cannot be
    /// written as a single unquoted word.
    pub fn escape_if_required(input: &str) -> String {
        if input.chars().all(StringReader::is_allowed_in_unquoted_string) {
            return input.to_string();
        }
        let mut result = String::from("\"");
        for c in input.chars() {
            if c == '\\' || c == '"' {
                result.push('\\');
            }
            result.push(c);
        }
        result.push('"');
        result
    }
}

impl<S> ArgumentType<S> for StringArgumentType {
    fn parse(&self, reader: &mut StringReader) -> Result<ArgumentValue, CommandSyntaxException> {
        let text = match self.kind {
            StringType::GreedyPhrase => {
                let text = reader.remaining().to_string();
                reader.set_cursor(reader.total_length());
                text
            }
            StringType::SingleWord => reader.read_unquoted_string(),
            StringType::QuotablePhrase => reader.read_string()?,
        };
        Ok(boxed(text))
    }

    fn examples(&self) -> Vec<&'static str> {
        self.kind.examples()
    }

    fn type_info(&self) -> Option<ArgumentTypeInfo> {
        Some(ArgumentTypeInfo::String {
            string_type: self.kind,
        })
    }

    fn box_clone(&self) -> Box<dyn ArgumentType<S>> {
        Box::new(*self)
    }
}

// ---------------------------------------------------------------------------------------
// Suggestions
// ---------------------------------------------------------------------------------------

/// `com.mojang.brigadier.suggestion.Suggestion`.
///
/// Java models `IntegerSuggestion` as a subclass; here the only difference that shows up in the
/// suggestion protocol - how the entry sorts and what it carries - is a field, so one list type
/// holds both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    range: StringRange,
    text: String,
    tooltip: Option<String>,
    integer_value: Option<i32>,
}

impl Suggestion {
    pub fn new(range: StringRange, text: impl Into<String>) -> Self {
        Self {
            range,
            text: text.into(),
            tooltip: None,
            integer_value: None,
        }
    }

    pub fn with_tooltip(range: StringRange, text: impl Into<String>, tooltip: &str) -> Self {
        Self {
            range,
            text: text.into(),
            tooltip: Some(tooltip.to_string()),
            integer_value: None,
        }
    }

    /// `new IntegerSuggestion(range, value)`.
    pub fn integer(range: StringRange, value: i32) -> Self {
        Self {
            range,
            text: value.to_string(),
            tooltip: None,
            integer_value: Some(value),
        }
    }

    pub fn range(&self) -> StringRange {
        self.range
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn tooltip(&self) -> Option<&str> {
        self.tooltip.as_deref()
    }

    /// `IntegerSuggestion.getValue`.
    pub fn integer_value(&self) -> Option<i32> {
        self.integer_value
    }

    /// `apply(input)`: the whole command with this suggestion's range replaced by its text.
    pub fn apply(&self, input: &str) -> String {
        if self.range.start() == 0 && self.range.end() == input.len() {
            return self.text.clone();
        }
        let mut result = String::new();
        if self.range.start() > 0 {
            result.push_str(&input[..self.range.start()]);
        }
        result.push_str(&self.text);
        if self.range.end() < input.len() {
            result.push_str(&input[self.range.end()..]);
        }
        result
    }

    /// `compareTo`: numeric between two integer suggestions, textual otherwise.
    pub fn compare_to(&self, other: &Suggestion) -> Ordering {
        if let (Some(a), Some(b)) = (self.integer_value, other.integer_value) {
            return a.cmp(&b);
        }
        self.text.cmp(&other.text)
    }

    /// `compareToIgnoreCase`, which `IntegerSuggestion` overrides with `compareTo`.
    pub fn compare_to_ignore_case(&self, other: &Suggestion) -> Ordering {
        if self.integer_value.is_some() && other.integer_value.is_some() {
            return self.compare_to(other);
        }
        java_compare_to_ignore_case(&self.text, &other.text)
    }

    /// `expand(command, range)`.
    pub fn expand(&self, command: &str, range: StringRange) -> Suggestion {
        if range == self.range {
            return self.clone();
        }
        let mut result = String::new();
        if range.start() < self.range.start() {
            result.push_str(&command[range.start()..self.range.start()]);
        }
        result.push_str(&self.text);
        if range.end() > self.range.end() {
            result.push_str(&command[self.range.end()..range.end()]);
        }
        Suggestion {
            range,
            text: result,
            tooltip: self.tooltip.clone(),
            integer_value: self.integer_value,
        }
    }
}

/// Java's `String.compareToIgnoreCase`: upper-case each char, fall back to lower-case on a
/// mismatch, compare the first difference.
fn java_compare_to_ignore_case(a: &str, b: &str) -> Ordering {
    let mut left = a.chars();
    let mut right = b.chars();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let mut folded_x = x.to_uppercase().next().unwrap_or(x);
                let mut folded_y = y.to_uppercase().next().unwrap_or(y);
                if folded_x != folded_y {
                    folded_x = folded_x.to_lowercase().next().unwrap_or(folded_x);
                    folded_y = folded_y.to_lowercase().next().unwrap_or(folded_y);
                }
                if folded_x != folded_y {
                    return folded_x.cmp(&folded_y);
                }
            }
        }
    }
}

/// `com.mojang.brigadier.suggestion.Suggestions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestions {
    range: StringRange,
    list: Vec<Suggestion>,
}

impl Suggestions {
    pub fn new(range: StringRange, list: Vec<Suggestion>) -> Self {
        Self { range, list }
    }

    /// `Suggestions.empty()`: the shared empty result, at position 0.
    pub fn empty() -> Self {
        Self::new(StringRange::at(0), Vec::new())
    }

    pub fn range(&self) -> StringRange {
        self.range
    }

    pub fn list(&self) -> &[Suggestion] {
        &self.list
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// `Suggestions.merge(command, input)`.
    pub fn merge(command: &str, input: &[Suggestions]) -> Suggestions {
        match input.len() {
            0 => return Self::empty(),
            1 => return input[0].clone(),
            _ => {}
        }
        let mut texts: Vec<Suggestion> = Vec::new();
        for suggestions in input {
            for suggestion in suggestions.list() {
                if !texts.contains(suggestion) {
                    texts.push(suggestion.clone());
                }
            }
        }
        Self::create(command, texts)
    }

    /// `Suggestions.create(command, suggestions)`.
    ///
    /// Sorted with Java's comparator and a stable sort. That comparator is a total order for a
    /// list of one kind (all text, or all integers) but not for a mixture: an `IntegerSuggestion`
    /// compares numerically against another integer and textually against a string, so "4" > "30"
    /// as text while 4 < 30 as numbers. For such a mixture the order is whatever the sort
    /// algorithm leaves; the port keeps the order inside each kind, which is what a suggestion
    /// list can promise (pinned in `brigadier_tests`).
    pub fn create(command: &str, suggestions: Vec<Suggestion>) -> Suggestions {
        if suggestions.is_empty() {
            return Self::empty();
        }
        let mut start = usize::MAX;
        let mut end = 0usize;
        for suggestion in &suggestions {
            start = start.min(suggestion.range().start());
            end = end.max(suggestion.range().end());
        }
        let range = StringRange::new(start, end);
        let mut expanded: Vec<Suggestion> = Vec::new();
        for suggestion in suggestions {
            let candidate = suggestion.expand(command, range);
            if !expanded.contains(&candidate) {
                expanded.push(candidate);
            }
        }
        expanded.sort_by(|a, b| a.compare_to_ignore_case(b));
        Suggestions::new(range, expanded)
    }
}

/// `com.mojang.brigadier.suggestion.SuggestionsBuilder`.
#[derive(Clone, Debug)]
pub struct SuggestionsBuilder {
    input: String,
    input_lower_case: String,
    start: usize,
    result: Vec<Suggestion>,
}

impl SuggestionsBuilder {
    pub fn new(input: &str, start: usize) -> Self {
        Self::with_lower_case(input, &input.to_lowercase(), start)
    }

    pub fn with_lower_case(input: &str, input_lower_case: &str, start: usize) -> Self {
        Self {
            input: input.to_string(),
            input_lower_case: input_lower_case.to_string(),
            start,
            result: Vec::new(),
        }
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn remaining(&self) -> &str {
        &self.input[self.start.min(self.input.len())..]
    }

    pub fn remaining_lower_case(&self) -> &str {
        &self.input_lower_case[self.start.min(self.input_lower_case.len())..]
    }

    fn full_range(&self) -> StringRange {
        StringRange::between(self.start, self.input.len())
    }

    pub fn build(&self) -> Suggestions {
        Suggestions::create(&self.input, self.result.clone())
    }

    pub fn suggest(&mut self, text: &str) -> &mut Self {
        if text == self.remaining() {
            return self;
        }
        self.result.push(Suggestion::new(self.full_range(), text));
        self
    }

    pub fn suggest_with_tooltip(&mut self, text: &str, tooltip: &str) -> &mut Self {
        if text == self.remaining() {
            return self;
        }
        self.result
            .push(Suggestion::with_tooltip(self.full_range(), text, tooltip));
        self
    }

    pub fn suggest_int(&mut self, value: i32) -> &mut Self {
        self.result.push(Suggestion::integer(self.full_range(), value));
        self
    }

    pub fn add(&mut self, other: &SuggestionsBuilder) -> &mut Self {
        self.result.extend(other.result.iter().cloned());
        self
    }

    pub fn create_offset(&self, start: usize) -> SuggestionsBuilder {
        SuggestionsBuilder::with_lower_case(&self.input, &self.input_lower_case, start)
    }

    pub fn restart(&self) -> SuggestionsBuilder {
        self.create_offset(self.start)
    }
}

// ---------------------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------------------

/// `com.mojang.brigadier.context.ParsedArgument`.
#[derive(Clone)]
pub struct ParsedArgument {
    range: StringRange,
    result: ArgumentValue,
}

impl ParsedArgument {
    pub fn new(start: usize, end: usize, result: ArgumentValue) -> Self {
        Self {
            range: StringRange::between(start, end),
            result,
        }
    }

    pub fn range(&self) -> StringRange {
        self.range
    }

    pub fn result(&self) -> &ArgumentValue {
        &self.result
    }

    pub fn value<T: 'static>(&self) -> Option<&T> {
        self.result.get::<T>()
    }
}

impl fmt::Debug for ParsedArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedArgument")
            .field("range", &self.range)
            .field("type", &self.result.type_name())
            .finish()
    }
}

/// What went wrong looking an argument up. Java throws `IllegalArgumentException` for both
/// cases; the two variants keep the same two messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgumentError {
    NoSuchArgument(String),
    WrongType {
        name: String,
        wanted: &'static str,
        defined: &'static str,
    },
}

impl ArgumentError {
    pub fn message(&self) -> String {
        match self {
            Self::NoSuchArgument(name) => {
                format!("No such argument '{name}' exists on this command")
            }
            Self::WrongType {
                name,
                wanted,
                defined,
            } => format!("Argument '{name}' is defined as {defined}, not {wanted}"),
        }
    }
}

impl fmt::Display for ArgumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// `com.mojang.brigadier.context.ParsedCommandNode`.
pub struct ParsedCommandNode<S> {
    node: CommandNode<S>,
    range: StringRange,
}

// A handle is two words; deriving would demand `S: Clone` for no reason.
impl<S> Clone for ParsedCommandNode<S> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            range: self.range,
        }
    }
}

impl<S> ParsedCommandNode<S> {
    pub fn new(node: CommandNode<S>, range: StringRange) -> Self {
        Self { node, range }
    }

    pub fn node(&self) -> &CommandNode<S> {
        &self.node
    }

    pub fn range(&self) -> StringRange {
        self.range
    }
}

impl<S> fmt::Debug for ParsedCommandNode<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}@{:?}", self.node, self.range)
    }
}

/// `com.mojang.brigadier.context.CommandContext`.
#[derive(Clone)]
pub struct CommandContext<S> {
    source: S,
    input: String,
    arguments: HashMap<String, ParsedArgument>,
    command: Option<Command<S>>,
    root_node: CommandNode<S>,
    nodes: Vec<ParsedCommandNode<S>>,
    range: StringRange,
    child: Option<Arc<CommandContext<S>>>,
    modifier: Option<RedirectModifier<S>>,
    forks: bool,
}

impl<S> CommandContext<S> {
    pub fn new(
        source: S,
        input: impl Into<String>,
        arguments: HashMap<String, ParsedArgument>,
        command: Option<Command<S>>,
        root_node: CommandNode<S>,
        nodes: Vec<ParsedCommandNode<S>>,
        range: StringRange,
        child: Option<Arc<CommandContext<S>>>,
        modifier: Option<RedirectModifier<S>>,
        forks: bool,
    ) -> Self {
        Self {
            source,
            input: input.into(),
            arguments,
            command,
            root_node,
            nodes,
            range,
            child,
            modifier,
            forks,
        }
    }

    pub fn child(&self) -> Option<&Arc<CommandContext<S>>> {
        self.child.as_ref()
    }

    pub fn last_child(&self) -> &CommandContext<S> {
        let mut result = self;
        while let Some(child) = result.child() {
            result = child;
        }
        result
    }

    pub fn command(&self) -> Option<&Command<S>> {
        self.command.as_ref()
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    /// `getArgument(name, Class)`: the stored value, if it is a `T`.
    pub fn argument<T: 'static>(&self, name: &str) -> Result<&T, ArgumentError> {
        let argument = self
            .arguments
            .get(name)
            .ok_or_else(|| ArgumentError::NoSuchArgument(name.to_string()))?;
        argument
            .value::<T>()
            .ok_or_else(|| ArgumentError::WrongType {
                name: name.to_string(),
                wanted: std::any::type_name::<T>(),
                defined: argument.result().type_name(),
            })
    }

    pub fn has_argument(&self, name: &str) -> bool {
        self.arguments.contains_key(name)
    }

    pub fn arguments(&self) -> &HashMap<String, ParsedArgument> {
        &self.arguments
    }

    pub fn redirect_modifier(&self) -> Option<&RedirectModifier<S>> {
        self.modifier.as_ref()
    }

    pub fn range(&self) -> StringRange {
        self.range
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn root_node(&self) -> &CommandNode<S> {
        &self.root_node
    }

    pub fn nodes(&self) -> &[ParsedCommandNode<S>] {
        &self.nodes
    }

    pub fn has_nodes(&self) -> bool {
        !self.nodes.is_empty()
    }

    pub fn is_forked(&self) -> bool {
        self.forks
    }
}

impl<S: Clone> CommandContext<S> {
    /// `copyFor(source)`. Java returns `this` when the source is the same object; the identity
    /// test is dropped here rather than adding a `PartialEq` bound to every source type.
    pub fn copy_for(&self, source: S) -> CommandContext<S> {
        let mut copy = self.clone();
        copy.source = source;
        copy
    }
}

impl<S: fmt::Debug> fmt::Debug for CommandContext<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandContext")
            .field("source", &self.source)
            .field("input", &self.input)
            .field("nodes", &self.nodes)
            .field("range", &self.range)
            .field("forks", &self.forks)
            .finish_non_exhaustive()
    }
}

/// `com.mojang.brigadier.context.CommandContextBuilder`.
#[derive(Clone)]
pub struct CommandContextBuilder<S> {
    arguments: HashMap<String, ParsedArgument>,
    root_node: CommandNode<S>,
    nodes: Vec<ParsedCommandNode<S>>,
    source: S,
    command: Option<Command<S>>,
    child: Option<Box<CommandContextBuilder<S>>>,
    range: StringRange,
    modifier: Option<RedirectModifier<S>>,
    forks: bool,
}

impl<S> CommandContextBuilder<S> {
    pub fn new(source: S, root_node: CommandNode<S>, start: usize) -> Self {
        Self {
            arguments: HashMap::new(),
            root_node,
            nodes: Vec::new(),
            source,
            command: None,
            child: None,
            range: StringRange::at(start),
            modifier: None,
            forks: false,
        }
    }

    pub fn with_source(&mut self, source: S) -> &mut Self {
        self.source = source;
        self
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    pub fn root_node(&self) -> &CommandNode<S> {
        &self.root_node
    }

    pub fn with_argument(&mut self, name: &str, argument: ParsedArgument) -> &mut Self {
        self.arguments.insert(name.to_string(), argument);
        self
    }

    pub fn arguments(&self) -> &HashMap<String, ParsedArgument> {
        &self.arguments
    }

    pub fn with_command(&mut self, command: Option<Command<S>>) -> &mut Self {
        self.command = command;
        self
    }

    pub fn with_node(&mut self, node: &CommandNode<S>, range: StringRange) -> &mut Self {
        self.nodes.push(ParsedCommandNode::new(node.clone(), range));
        self.range = StringRange::encompassing(self.range, range);
        self.modifier = node.redirect_modifier().cloned();
        self.forks = node.is_fork();
        self
    }

    pub fn with_child(&mut self, child: CommandContextBuilder<S>) -> &mut Self {
        self.child = Some(Box::new(child));
        self
    }

    pub fn child(&self) -> Option<&CommandContextBuilder<S>> {
        self.child.as_deref()
    }

    pub fn last_child(&self) -> &CommandContextBuilder<S> {
        let mut result = self;
        while let Some(child) = result.child() {
            result = child;
        }
        result
    }

    pub fn command(&self) -> Option<&Command<S>> {
        self.command.as_ref()
    }

    pub fn nodes(&self) -> &[ParsedCommandNode<S>] {
        &self.nodes
    }

    pub fn range(&self) -> StringRange {
        self.range
    }

    /// `findSuggestionContext(cursor)`.
    pub fn find_suggestion_context(&self, cursor: usize) -> SuggestionContext<S> {
        if self.range.start() > cursor {
            // Java throws IllegalStateException here; a caller asking for a cursor before the
            // input it parsed is a programming error, so the port panics with the same text.
            panic!("Can't find node before cursor");
        }
        if self.range.end() < cursor {
            if let Some(child) = self.child() {
                return child.find_suggestion_context(cursor);
            }
            if let Some(last) = self.nodes.last() {
                return SuggestionContext::new(last.node().clone(), last.range().end() + 1);
            }
            return SuggestionContext::new(self.root_node.clone(), self.range.start());
        }
        let mut previous = self.root_node.clone();
        for node in &self.nodes {
            let node_range = node.range();
            if node_range.start() <= cursor && cursor <= node_range.end() {
                return SuggestionContext::new(previous, node_range.start());
            }
            previous = node.node().clone();
        }
        SuggestionContext::new(previous, self.range.start())
    }
}

impl<S: Clone> CommandContextBuilder<S> {
    /// `copy()`: a builder that can be advanced without touching this one.
    pub fn copy(&self) -> Self {
        self.clone()
    }

    pub fn build(&self, input: &str) -> CommandContext<S> {
        CommandContext::new(
            self.source.clone(),
            input,
            self.arguments.clone(),
            self.command.clone(),
            self.root_node.clone(),
            self.nodes.clone(),
            self.range,
            self.child
                .as_ref()
                .map(|child| Arc::new(child.build(input))),
            self.modifier.clone(),
            self.forks,
        )
    }
}

impl<S: fmt::Debug> fmt::Debug for CommandContextBuilder<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandContextBuilder")
            .field("source", &self.source)
            .field("nodes", &self.nodes)
            .field("range", &self.range)
            .field("forks", &self.forks)
            .finish_non_exhaustive()
    }
}

/// `com.mojang.brigadier.context.SuggestionContext`.
///
/// 1.3.10's shape: `parent` and `startPos`. (Later Brigadier versions add the context builder;
/// the 1.21.1 jar does not have that field - see the module note on the pin.)
pub struct SuggestionContext<S> {
    pub parent: CommandNode<S>,
    pub start_pos: usize,
}

impl<S> SuggestionContext<S> {
    pub fn new(parent: CommandNode<S>, start_pos: usize) -> Self {
        Self { parent, start_pos }
    }
}

/// `com.mojang.brigadier.context.ContextChain`: the redirect/fork stages between the parsed
/// context and the executable one.
pub struct ContextChain<'a, S> {
    modifiers: Vec<&'a CommandContext<S>>,
    executable: &'a CommandContext<S>,
}

impl<'a, S: Clone> ContextChain<'a, S> {
    /// `tryFlatten`: `None` when the last context in the chain is not executable.
    pub fn try_flatten(root: &'a CommandContext<S>) -> Option<Self> {
        let mut modifiers = Vec::new();
        let mut current = root;
        loop {
            match current.child() {
                None => {
                    if current.command().is_none() {
                        return None;
                    }
                    return Some(Self {
                        modifiers,
                        executable: current,
                    });
                }
                Some(child) => {
                    modifiers.push(current);
                    current = child;
                }
            }
        }
    }

    /// `executeAll`: run every stage, multiplying sources at each modifier, and sum the
    /// executable's results - except in forked mode, where each run counts 1/0 and a failing
    /// one is dropped instead of aborting the rest.
    pub fn execute_all(
        &self,
        source: S,
        consumer: Option<&ResultConsumer<S>>,
    ) -> Result<i32, CommandSyntaxException> {
        if self.modifiers.is_empty() {
            return run_executable(self.executable, source, consumer, false);
        }
        let mut forked_mode = false;
        let mut current_sources = vec![source];
        for modifier in &self.modifiers {
            forked_mode |= modifier.is_forked();
            let mut next_sources = Vec::new();
            for source_to_run in current_sources {
                next_sources.extend(run_modifier(modifier, source_to_run, consumer, forked_mode)?);
            }
            if next_sources.is_empty() {
                return Ok(0);
            }
            current_sources = next_sources;
        }
        let mut result = 0;
        for execution_source in current_sources {
            result += run_executable(self.executable, execution_source, consumer, forked_mode)?;
        }
        Ok(result)
    }

    pub fn executable(&self) -> &CommandContext<S> {
        self.executable
    }

    pub fn modifier_count(&self) -> usize {
        self.modifiers.len()
    }
}

fn run_modifier<S: Clone>(
    modifier: &CommandContext<S>,
    source: S,
    consumer: Option<&ResultConsumer<S>>,
    forked_mode: bool,
) -> Result<Vec<S>, CommandSyntaxException> {
    let Some(redirect_modifier) = modifier.redirect_modifier() else {
        return Ok(vec![source]);
    };
    let context_to_use = modifier.copy_for(source);
    match redirect_modifier(&context_to_use) {
        Ok(sources) => Ok(sources),
        Err(error) => {
            notify(consumer, &context_to_use, false, 0);
            if forked_mode {
                Ok(Vec::new())
            } else {
                Err(error)
            }
        }
    }
}

fn run_executable<S: Clone>(
    executable: &CommandContext<S>,
    source: S,
    consumer: Option<&ResultConsumer<S>>,
    forked_mode: bool,
) -> Result<i32, CommandSyntaxException> {
    let context_to_use = executable.copy_for(source);
    // `ContextChain` is only constructed for a context that has a command.
    let command = context_to_use
        .command()
        .cloned()
        .expect("executable context has a command");
    match command(&context_to_use) {
        Ok(result) => {
            notify(consumer, &context_to_use, true, result);
            Ok(if forked_mode { 1 } else { result })
        }
        Err(error) => {
            notify(consumer, &context_to_use, false, 0);
            if forked_mode {
                Ok(0)
            } else {
                Err(error)
            }
        }
    }
}

pub fn notify<S>(consumer: Option<&ResultConsumer<S>>, context: &CommandContext<S>, success: bool, result: i32) {
    if let Some(consumer) = consumer {
        consumer(context, success, result);
    }
}

// ---------------------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum NodeKind {
    Root,
    Literal {
        literal: String,
        literal_lower_case: String,
    },
    Argument {
        name: String,
    },
}

struct NodeData<S> {
    kind: NodeKind,
    argument_type: Option<Box<dyn ArgumentType<S>>>,
    custom_suggestions: Option<SuggestionProvider<S>>,
    requirement: Requirement<S>,
    redirect: Option<CommandNode<S>>,
    modifier: Option<RedirectModifier<S>>,
    forks: bool,
    inner: RwLock<NodeInner<S>>,
}

struct NodeInner<S> {
    command: Option<Command<S>>,
    ordered: Vec<CommandNode<S>>,
    by_name: HashMap<String, usize>,
    literals: HashMap<String, usize>,
    arguments: Vec<usize>,
}

/// `com.mojang.brigadier.tree.CommandNode`, as one handle for the root, literal and argument
/// forms (Java uses a class hierarchy for the same three shapes).
///
/// Cloning a `CommandNode` clones the handle, not the node: two handles to the same node are
/// the same node, which is the identity Java's `==` comparisons test.
pub struct CommandNode<S> {
    data: Arc<NodeData<S>>,
}

// Cloning a handle clones the reference, never the node - and never the source type.
impl<S> Clone for CommandNode<S> {
    fn clone(&self) -> Self {
        Self {
            data: Arc::clone(&self.data),
        }
    }
}

impl<S> CommandNode<S> {
    pub fn root() -> Self {
        Self::from_parts(NodeKind::Root, None, None, requirement_any(), None, None, false)
    }

    pub fn literal(literal: impl Into<String>) -> Self {
        let literal = literal.into();
        let literal_lower_case = literal.to_lowercase();
        Self::from_parts(
            NodeKind::Literal {
                literal,
                literal_lower_case,
            },
            None,
            None,
            requirement_any(),
            None,
            None,
            false,
        )
    }

    pub fn argument(name: impl Into<String>, argument_type: Box<dyn ArgumentType<S>>) -> Self {
        Self::from_parts(
            NodeKind::Argument { name: name.into() },
            Some(argument_type),
            None,
            requirement_any(),
            None,
            None,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        kind: NodeKind,
        argument_type: Option<Box<dyn ArgumentType<S>>>,
        custom_suggestions: Option<SuggestionProvider<S>>,
        requirement: Requirement<S>,
        redirect: Option<CommandNode<S>>,
        modifier: Option<RedirectModifier<S>>,
        forks: bool,
    ) -> Self {
        Self {
            data: Arc::new(NodeData {
                kind,
                argument_type,
                custom_suggestions,
                requirement,
                redirect,
                modifier,
                forks,
                inner: RwLock::new(NodeInner {
                    command: None,
                    ordered: Vec::new(),
                    by_name: HashMap::new(),
                    literals: HashMap::new(),
                    arguments: Vec::new(),
                }),
            }),
        }
    }

    fn set_command(&self, command: Command<S>) {
        write_lock(&self.data.inner).command = Some(command);
    }

    pub fn same_node(&self, other: &CommandNode<S>) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }

    /// The node identity [`Self::same_node`] compares, as a value a map can key on: the address
    /// of the node the handles share. Stable while any handle to the node is alive, which is the
    /// whole time a tree that contains it is being walked.
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.data) as usize
    }

    pub fn command(&self) -> Option<Command<S>> {
        read_lock(&self.data.inner).command.clone()
    }

    pub fn children(&self) -> Vec<CommandNode<S>> {
        read_lock(&self.data.inner).ordered.clone()
    }

    pub fn child(&self, name: &str) -> Option<CommandNode<S>> {
        let inner = read_lock(&self.data.inner);
        inner.by_name.get(name).map(|index| inner.ordered[*index].clone())
    }

    pub fn child_count(&self) -> usize {
        read_lock(&self.data.inner).ordered.len()
    }

    pub fn redirect(&self) -> Option<CommandNode<S>> {
        self.data.redirect.clone()
    }

    pub fn redirect_modifier(&self) -> Option<&RedirectModifier<S>> {
        self.data.modifier.as_ref()
    }

    pub fn is_fork(&self) -> bool {
        self.data.forks
    }

    pub fn can_use(&self, source: &S) -> bool {
        (self.data.requirement)(source)
    }

    pub fn requirement(&self) -> &Requirement<S> {
        &self.data.requirement
    }

    pub fn is_root(&self) -> bool {
        matches!(self.data.kind, NodeKind::Root)
    }

    pub fn is_literal(&self) -> bool {
        matches!(self.data.kind, NodeKind::Literal { .. })
    }

    pub fn is_argument(&self) -> bool {
        matches!(self.data.kind, NodeKind::Argument { .. })
    }

    pub fn name(&self) -> &str {
        match &self.data.kind {
            NodeKind::Root => "",
            NodeKind::Literal { literal, .. } => literal,
            NodeKind::Argument { name } => name,
        }
    }

    pub fn usage_text(&self) -> String {
        match &self.data.kind {
            NodeKind::Root => String::new(),
            NodeKind::Literal { literal, .. } => literal.clone(),
            NodeKind::Argument { name } => format!("<{name}>"),
        }
    }

    /// `getSortedKey`: what `compareTo` orders siblings by - the literal text, or the
    /// argument's name. Vanilla uses this to order the children it puts in the Commands packet.
    pub fn sorted_key(&self) -> &str {
        self.name()
    }

    pub fn argument_type(&self) -> Option<&dyn ArgumentType<S>> {
        self.data.argument_type.as_deref()
    }

    pub fn custom_suggestions(&self) -> Option<&SuggestionProvider<S>> {
        self.data.custom_suggestions.as_ref()
    }

    /// `getExamples`: a literal is its own example; an argument asks its type.
    pub fn examples(&self) -> Vec<String> {
        match self.data.argument_type.as_deref() {
            Some(argument_type) => argument_type
                .examples()
                .into_iter()
                .map(str::to_string)
                .collect(),
            None => match &self.data.kind {
                NodeKind::Literal { literal, .. } => vec![literal.clone()],
                _ => Vec::new(),
            },
        }
    }

    /// `CommandNode.addChild`, including the merge Java performs when a child with the same
    /// name is already present: the existing node keeps its identity, takes the incoming
    /// command when it has one, and absorbs the incoming grandchildren.
    pub fn add_child(&self, node: CommandNode<S>) -> Result<(), TreeError> {
        if node.is_root() {
            return Err(TreeError::RootAsChild);
        }
        let name = node.name().to_string();
        let existing = {
            let inner = read_lock(&self.data.inner);
            inner.by_name.get(&name).map(|index| inner.ordered[*index].clone())
        };
        if let Some(existing) = existing {
            if let Some(command) = node.command() {
                existing.set_command(command);
            }
            for grandchild in node.children() {
                existing.add_child(grandchild)?;
            }
            return Ok(());
        }
        let mut inner = write_lock(&self.data.inner);
        if let Some(index) = inner.by_name.get(&name) {
            // Another thread inserted the same name first; merge below the lock-free path.
            let existing = inner.ordered[*index].clone();
            drop(inner);
            if let Some(command) = node.command() {
                existing.set_command(command);
            }
            for grandchild in node.children() {
                existing.add_child(grandchild)?;
            }
            return Ok(());
        }
        let index = inner.ordered.len();
        inner.by_name.insert(name.clone(), index);
        inner.ordered.push(node.clone());
        if node.is_literal() {
            inner.literals.insert(name, index);
        } else if node.is_argument() {
            inner.arguments.push(index);
        }
        Ok(())
    }

    /// `getRelevantNodes`: a literal that matches the next word wins outright; otherwise every
    /// argument child is a candidate. Java moves and restores the reader's cursor to read the
    /// next word; the port scans a copy of the string from the cursor instead.
    pub fn relevant_nodes(&self, input: &StringReader) -> Vec<CommandNode<S>> {
        let (has_literals, arguments) = {
            let inner = read_lock(&self.data.inner);
            (
                !inner.literals.is_empty(),
                inner
                    .arguments
                    .iter()
                    .map(|index| inner.ordered[*index].clone())
                    .collect::<Vec<_>>(),
            )
        };
        if !has_literals {
            return arguments;
        }
        let cursor = input.cursor();
        let mut scan = cursor;
        loop {
            match input.string()[scan..].chars().next() {
                None | Some(ARGUMENT_SEPARATOR_CHAR) => break,
                Some(c) => scan += c.len_utf8(),
            }
        }
        let text = &input.string()[cursor..scan];
        let literal = {
            let inner = read_lock(&self.data.inner);
            inner
                .literals
                .get(text)
                .map(|index| inner.ordered[*index].clone())
        };
        match literal {
            Some(literal) => vec![literal],
            None => arguments,
        }
    }

    /// `compareTo`: literals sort before arguments, then by `getSortedKey`.
    pub fn compare_to(&self, other: &CommandNode<S>) -> Ordering {
        if self.is_literal() == other.is_literal() {
            return self.sorted_key().cmp(other.sorted_key());
        }
        if other.is_literal() {
            Ordering::Greater
        } else {
            Ordering::Less
        }
    }

    /// `CommandNode.parse`.
    pub fn parse(
        &self,
        reader: &mut StringReader,
        context_builder: &mut CommandContextBuilder<S>,
    ) -> Result<(), CommandSyntaxException> {
        match &self.data.kind {
            NodeKind::Root => Ok(()),
            NodeKind::Literal { literal, .. } => {
                let start = reader.cursor();
                match self.parse_literal(reader) {
                    Some(end) => {
                        context_builder.with_node(self, StringRange::between(start, end));
                        Ok(())
                    }
                    None => Err(BUILT_IN_EXCEPTIONS
                        .literal_incorrect()
                        .create_with_context(reader, literal)),
                }
            }
            NodeKind::Argument { name } => {
                let start = reader.cursor();
                let Some(argument_type) = self.data.argument_type.as_deref() else {
                    return Err(BUILT_IN_EXCEPTIONS
                        .dispatcher_parse_exception()
                        .create_with_context(reader, "argument node without a type"));
                };
                let result = {
                    let source = context_builder.source();
                    argument_type.parse_with_source(reader, source)?
                };
                let parsed = ParsedArgument::new(start, reader.cursor(), result);
                let range = parsed.range();
                context_builder.with_argument(name, parsed);
                context_builder.with_node(self, range);
                Ok(())
            }
        }
    }

    /// `LiteralCommandNode.parse(reader)`: `Some(end)` when the literal is next, `None` when it
    /// is not (the cursor is left where it was).
    fn parse_literal(&self, reader: &mut StringReader) -> Option<usize> {
        let NodeKind::Literal { literal, .. } = &self.data.kind else {
            return None;
        };
        let start = reader.cursor();
        if reader.can_read_n(literal.len()) {
            let end = start + literal.len();
            if reader.string()[start..end] == *literal {
                reader.set_cursor(end);
                if !reader.can_read() || reader.peek() == Some(ARGUMENT_SEPARATOR_CHAR) {
                    return Some(end);
                }
                reader.set_cursor(start);
            }
        }
        None
    }

    /// `CommandNode.listSuggestions`.
    pub fn list_suggestions(
        &self,
        context: &CommandContext<S>,
        builder: &mut SuggestionsBuilder,
    ) -> Result<Suggestions, CommandSyntaxException> {
        match &self.data.kind {
            NodeKind::Root => Ok(Suggestions::empty()),
            NodeKind::Literal {
                literal,
                literal_lower_case,
            } => {
                if literal_lower_case.starts_with(builder.remaining_lower_case()) {
                    builder.suggest(literal);
                    Ok(builder.build())
                } else {
                    Ok(Suggestions::empty())
                }
            }
            NodeKind::Argument { .. } => {
                if let Some(provider) = self.data.custom_suggestions.as_ref() {
                    return provider(context, builder);
                }
                match self.data.argument_type.as_deref() {
                    Some(argument_type) => argument_type.list_suggestions(context, builder),
                    None => Ok(Suggestions::empty()),
                }
            }
        }
    }

    /// `CommandNode.isValidInput`.
    pub fn is_valid_input(&self, input: &str) -> bool {
        let mut reader = StringReader::new(input);
        match &self.data.kind {
            NodeKind::Root => false,
            NodeKind::Literal { .. } => self.parse_literal(&mut reader).is_some(),
            NodeKind::Argument { .. } => {
                let parsed = match self.data.argument_type.as_deref() {
                    Some(argument_type) => argument_type.parse(&mut reader),
                    None => return false,
                };
                parsed.is_ok() && (!reader.can_read() || reader.peek() == Some(ARGUMENT_SEPARATOR_CHAR))
            }
        }
    }

    /// `CommandNode.createBuilder`: a fresh node of the same shape, so a tree can be
    /// re-registered (vanilla does this when a datapack reload re-registers `/execute`).
    pub fn create_builder(&self) -> ArgumentBuilder<S> {
        let mut builder = match &self.data.kind {
            NodeKind::Root => panic!("Cannot convert root into a builder"),
            NodeKind::Literal { literal, .. } => ArgumentBuilder::literal(literal.clone()),
            NodeKind::Argument { name } => {
                let argument_type = self
                    .data
                    .argument_type
                    .as_ref()
                    .expect("an argument node has a type")
                    .box_clone();
                match self.data.custom_suggestions.clone() {
                    Some(suggestions) => {
                        ArgumentBuilder::argument(name.clone(), argument_type).suggests(suggestions)
                    }
                    None => ArgumentBuilder::argument(name.clone(), argument_type),
                }
            }
        };
        builder = builder.with_requirement(self.data.requirement.clone());
        if let Some(command) = self.command() {
            builder = builder.executes_command(command);
        }
        if self.data.redirect.is_some() || self.data.modifier.is_some() || self.data.forks {
            builder = builder.forward(
                self.data.redirect.clone(),
                self.data.modifier.clone(),
                self.data.forks,
            );
        }
        builder
    }
}

impl<S> fmt::Debug for CommandNode<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.data.kind {
            NodeKind::Root => write!(f, "<root>"),
            NodeKind::Literal { literal, .. } => write!(f, "<literal {literal}>"),
            NodeKind::Argument { name } => write!(f, "<argument {name}>"),
        }
    }
}

/// Rejected tree edits. Java throws `UnsupportedOperationException` for the first one and
/// `IllegalArgumentException`/"Cannot forward a node with children" for the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeError {
    RootAsChild,
    ChildrenOnRedirectedNode,
    ForwardWithChildren,
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootAsChild => f.write_str("Cannot add a RootCommandNode as a child to any other CommandNode"),
            Self::ChildrenOnRedirectedNode => f.write_str("Cannot add children to a redirected node"),
            Self::ForwardWithChildren => f.write_str("Cannot forward a node with children"),
        }
    }
}

impl std::error::Error for TreeError {}

// ---------------------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------------------

enum BuilderKind<S> {
    Literal(String),
    Argument {
        name: String,
        argument_type: Box<dyn ArgumentType<S>>,
        suggestions: Option<SuggestionProvider<S>>,
    },
    /// A node that already exists (Java's `then(CommandNode)` and `createBuilder` paths).
    Existing(CommandNode<S>),
}

/// `com.mojang.brigadier.builder.ArgumentBuilder`, with `LiteralArgumentBuilder` and
/// `RequiredArgumentBuilder` as the two constructors (Java has three classes because the
/// distinction is checked at compile time; the distinction is [`ArgumentBuilder::literal`] vs
/// [`ArgumentBuilder::argument`] here).
pub struct ArgumentBuilder<S> {
    kind: BuilderKind<S>,
    arguments: CommandNode<S>,
    command: Option<Command<S>>,
    requirement: Requirement<S>,
    target: Option<CommandNode<S>>,
    modifier: Option<RedirectModifier<S>>,
    forks: bool,
}

pub type LiteralArgumentBuilder<S> = ArgumentBuilder<S>;
pub type RequiredArgumentBuilder<S> = ArgumentBuilder<S>;

impl<S> ArgumentBuilder<S> {
    pub fn literal(literal: impl Into<String>) -> Self {
        Self::new(BuilderKind::Literal(literal.into()))
    }

    pub fn argument(name: impl Into<String>, argument_type: Box<dyn ArgumentType<S>>) -> Self {
        Self::new(BuilderKind::Argument {
            name: name.into(),
            argument_type,
            suggestions: None,
        })
    }

    fn from_existing(node: CommandNode<S>) -> Self {
        Self::new(BuilderKind::Existing(node))
    }

    fn new(kind: BuilderKind<S>) -> Self {
        Self {
            kind,
            arguments: CommandNode::root(),
            command: None,
            requirement: requirement_any(),
            target: None,
            modifier: None,
            forks: false,
        }
    }

    /// `then(builder)`: attach a child subtree. A redirected builder cannot take children.
    pub fn then(self, argument: ArgumentBuilder<S>) -> Self {
        self.then_node(argument.build())
    }

    /// `then(CommandNode)`.
    pub fn then_node(self, argument: CommandNode<S>) -> Self {
        if self.target.is_some() {
            panic!("{}", TreeError::ChildrenOnRedirectedNode);
        }
        self.arguments
            .add_child(argument)
            .expect("a built child is never a root");
        self
    }

    /// `getArguments`: the children attached so far, after merging by name.
    pub fn get_arguments(&self) -> Vec<CommandNode<S>> {
        self.arguments.children()
    }

    /// `executes(command)`.
    pub fn executes<F>(mut self, command: F) -> Self
    where
        F: Fn(&CommandContext<S>) -> Result<i32, CommandSyntaxException> + Send + Sync + 'static,
    {
        self.command = Some(Arc::new(command));
        self
    }

    /// `executes(command)` with an already-shared callback.
    pub fn executes_command(mut self, command: Command<S>) -> Self {
        self.command = Some(command);
        self
    }

    pub fn get_command(&self) -> Option<&Command<S>> {
        self.command.as_ref()
    }

    pub fn requires<F>(mut self, requirement: F) -> Self
    where
        F: Fn(&S) -> bool + Send + Sync + 'static,
    {
        self.requirement = Arc::new(requirement);
        self
    }

    pub fn with_requirement(mut self, requirement: Requirement<S>) -> Self {
        self.requirement = requirement;
        self
    }

    pub fn get_requirement(&self) -> &Requirement<S> {
        &self.requirement
    }

    /// `redirect(target)`.
    pub fn redirect(self, target: CommandNode<S>) -> Self {
        self.forward(Some(target), None, false)
    }

    /// `redirect(target, modifier)`: one source in, one source out.
    pub fn redirect_single<F>(self, target: CommandNode<S>, modifier: F) -> Self
    where
        F: Fn(&CommandContext<S>) -> S + Send + Sync + 'static,
    {
        let wrapped: RedirectModifier<S> = Arc::new(move |context| Ok(vec![modifier(context)]));
        self.forward(Some(target), Some(wrapped), false)
    }

    /// `fork(target, modifier)`: one source in, many sources out, failures tolerated.
    pub fn fork<F>(self, target: CommandNode<S>, modifier: F) -> Self
    where
        F: Fn(&CommandContext<S>) -> Result<Vec<S>, CommandSyntaxException> + Send + Sync + 'static,
    {
        self.forward(Some(target), Some(Arc::new(modifier)), true)
    }

    /// `forward(target, modifier, fork)`.
    pub fn forward(
        mut self,
        target: Option<CommandNode<S>>,
        modifier: Option<RedirectModifier<S>>,
        fork: bool,
    ) -> Self {
        if !self.arguments.children().is_empty() {
            panic!("{}", TreeError::ForwardWithChildren);
        }
        self.target = target;
        self.modifier = modifier;
        self.forks = fork;
        self
    }

    pub fn get_redirect(&self) -> Option<&CommandNode<S>> {
        self.target.as_ref()
    }

    pub fn get_redirect_modifier(&self) -> Option<&RedirectModifier<S>> {
        self.modifier.as_ref()
    }

    pub fn is_fork(&self) -> bool {
        self.forks
    }

    /// `suggests(provider)` on a required-argument builder.
    pub fn suggests(mut self, provider: SuggestionProvider<S>) -> Self {
        if let BuilderKind::Argument { suggestions, .. } = &mut self.kind {
            *suggestions = Some(provider);
        }
        self
    }

    pub fn build(self) -> CommandNode<S> {
        let node = match self.kind {
            BuilderKind::Existing(node) => node,
            BuilderKind::Literal(literal) => CommandNode::from_parts(
                {
                    let lower = literal.to_lowercase();
                    NodeKind::Literal {
                        literal,
                        literal_lower_case: lower,
                    }
                },
                None,
                None,
                self.requirement.clone(),
                self.target.clone(),
                self.modifier.clone(),
                self.forks,
            ),
            BuilderKind::Argument {
                name,
                argument_type,
                suggestions,
            } => CommandNode::from_parts(
                NodeKind::Argument { name },
                Some(argument_type),
                suggestions,
                self.requirement.clone(),
                self.target.clone(),
                self.modifier.clone(),
                self.forks,
            ),
        };
        if let Some(command) = self.command.clone() {
            node.set_command(command);
        }
        for argument in self.arguments.children() {
            node.add_child(argument)
                .expect("a built child is never a root");
        }
        node
    }
}

// ---------------------------------------------------------------------------------------
// Parse results
// ---------------------------------------------------------------------------------------

/// `com.mojang.brigadier.ParseResults`.
#[derive(Clone)]
pub struct ParseResults<S> {
    context: CommandContextBuilder<S>,
    reader: StringReader,
    exceptions: Vec<(CommandNode<S>, CommandSyntaxException)>,
}

impl<S> ParseResults<S> {
    pub fn new(
        context: CommandContextBuilder<S>,
        reader: StringReader,
        exceptions: Vec<(CommandNode<S>, CommandSyntaxException)>,
    ) -> Self {
        Self {
            context,
            reader,
            exceptions,
        }
    }

    pub fn context(&self) -> &CommandContextBuilder<S> {
        &self.context
    }

    pub fn reader(&self) -> &StringReader {
        &self.reader
    }

    pub fn exceptions(&self) -> &[(CommandNode<S>, CommandSyntaxException)] {
        &self.exceptions
    }
}

// ---------------------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------------------

/// `com.mojang.brigadier.CommandDispatcher`.
pub struct CommandDispatcher<S> {
    root: CommandNode<S>,
    consumer: Option<ResultConsumer<S>>,
}

impl<S: Clone> Default for CommandDispatcher<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Clone> CommandDispatcher<S> {
    pub fn new() -> Self {
        Self {
            root: CommandNode::root(),
            consumer: None,
        }
    }

    pub fn with_root(root: CommandNode<S>) -> Self {
        Self {
            root,
            consumer: None,
        }
    }

    /// `register(LiteralArgumentBuilder)`: build the subtree and merge it into the root.
    pub fn register(&self, command: ArgumentBuilder<S>) -> Result<CommandNode<S>, TreeError> {
        let built = command.build();
        self.root.add_child(built.clone())?;
        Ok(built)
    }

    pub fn set_consumer(&mut self, consumer: ResultConsumer<S>) {
        self.consumer = Some(consumer);
    }

    pub fn root(&self) -> &CommandNode<S> {
        &self.root
    }

    /// `execute(String, S)`.
    pub fn execute(&self, input: &str, source: S) -> Result<i32, CommandSyntaxException> {
        let parse = self.parse(input, source);
        self.execute_parse(parse)
    }

    /// `execute(StringReader, S)`.
    pub fn execute_reader(
        &self,
        reader: StringReader,
        source: S,
    ) -> Result<i32, CommandSyntaxException> {
        let parse = self.parse_reader(reader, source);
        self.execute_parse(parse)
    }

    /// `execute(ParseResults)`.
    pub fn execute_parse(&self, parse: ParseResults<S>) -> Result<i32, CommandSyntaxException> {
        if parse.reader.can_read() {
            if parse.exceptions.len() == 1 {
                return Err(parse.exceptions[0].1.clone());
            }
            if parse.context.range().is_empty() {
                return Err(BUILT_IN_EXCEPTIONS
                    .dispatcher_unknown_command()
                    .create_with_context(&parse.reader));
            }
            return Err(BUILT_IN_EXCEPTIONS
                .dispatcher_unknown_argument()
                .create_with_context(&parse.reader));
        }

        let command = parse.reader.string().to_string();
        let original = parse.context.build(&command);
        let Some(chain) = ContextChain::try_flatten(&original) else {
            notify(self.consumer.as_ref(), &original, false, 0);
            return Err(BUILT_IN_EXCEPTIONS
                .dispatcher_unknown_command()
                .create_with_context(&parse.reader));
        };
        chain.execute_all(original.source().clone(), self.consumer.as_ref())
    }

    /// `parse(String, S)`.
    pub fn parse(&self, command: &str, source: S) -> ParseResults<S> {
        self.parse_reader(StringReader::new(command), source)
    }

    /// `parse(StringReader, S)`.
    ///
    /// The reader's cursor is honoured (vanilla parses `"/foo"` from offset 1) and the returned
    /// `ParseResults` carries whichever reader the walk ended on - Java's original when no child
    /// matched at all, which is why an unknown top-level command reports cursor 0.
    pub fn parse_reader(&self, reader: StringReader, source: S) -> ParseResults<S> {
        let mut context = CommandContextBuilder::new(source, self.root.clone(), reader.cursor());
        self.parse_nodes(&self.root.clone(), &reader, &mut context)
    }

    fn parse_nodes(
        &self,
        node: &CommandNode<S>,
        original_reader: &StringReader,
        context_so_far: &mut CommandContextBuilder<S>,
    ) -> ParseResults<S> {
        let source = context_so_far.source().clone();
        let mut errors: Option<Vec<(CommandNode<S>, CommandSyntaxException)>> = None;
        let mut potentials: Vec<ParseResults<S>> = Vec::new();

        for child in node.relevant_nodes(original_reader) {
            if !child.can_use(&source) {
                continue;
            }
            let mut context = context_so_far.copy();
            let mut reader = original_reader.clone();
            // Java additionally wraps a RuntimeException from `child.parse` into
            // `dispatcherParseException`. A failed parse is a `Result` here, so the only thing
            // left that could unwind is a panic inside an argument type, and that is a bug to
            // see rather than to convert into a syntax error.
            let parsed = (|| -> Result<(), CommandSyntaxException> {
                child.parse(&mut reader, &mut context)?;
                if reader.can_read() && reader.peek() != Some(ARGUMENT_SEPARATOR_CHAR) {
                    return Err(BUILT_IN_EXCEPTIONS
                        .dispatcher_expected_argument_separator()
                        .create_with_context(&reader));
                }
                Ok(())
            })();
            if let Err(error) = parsed {
                errors.get_or_insert_with(Vec::new).push((child.clone(), error));
                continue;
            }

            context.with_command(child.command());
            let redirect = child.redirect();
            if reader.can_read_n(if redirect.is_none() { 2 } else { 1 }) {
                reader.skip();
                if let Some(target) = redirect {
                    let child_context =
                        CommandContextBuilder::new(source.clone(), target.clone(), reader.cursor());
                    let mut child_context = child_context;
                    let parse = self.parse_nodes(&target, &reader, &mut child_context);
                    // Java stores the child builder by reference; the port owns a copy of it.
                    // Nothing after this point mutates the child, so the two agree.
                    context.with_child(parse.context().clone());
                    return ParseResults::new(context, parse.reader().clone(), parse.exceptions().to_vec());
                }
                let mut child_context = context.clone();
                let parse = self.parse_nodes(&child, &reader, &mut child_context);
                potentials.push(parse);
            } else {
                potentials.push(ParseResults::new(context, reader, Vec::new()));
            }
        }

        if !potentials.is_empty() {
            if potentials.len() > 1 {
                potentials.sort_by(|a, b| {
                    let a_can_read = a.reader().can_read();
                    let b_can_read = b.reader().can_read();
                    if !a_can_read && b_can_read {
                        return Ordering::Less;
                    }
                    if a_can_read && !b_can_read {
                        return Ordering::Greater;
                    }
                    let a_empty = a.exceptions().is_empty();
                    let b_empty = b.exceptions().is_empty();
                    if a_empty && !b_empty {
                        return Ordering::Less;
                    }
                    if !a_empty && b_empty {
                        return Ordering::Greater;
                    }
                    Ordering::Equal
                });
            }
            return potentials.remove(0);
        }

        ParseResults::new(
            context_so_far.copy(),
            original_reader.clone(),
            errors.unwrap_or_default(),
        )
    }

    /// `getCompletionSuggestions(parse)`: suggestions at the end of the input.
    pub fn get_completion_suggestions(&self, parse: &ParseResults<S>) -> Suggestions {
        self.get_completion_suggestions_at(parse, parse.reader().total_length())
    }

    /// `getCompletionSuggestions(parse, cursor)`, synchronously.
    pub fn get_completion_suggestions_at(
        &self,
        parse: &ParseResults<S>,
        cursor: usize,
    ) -> Suggestions {
        let cursor = floor_char_boundary(parse.reader().string(), cursor.min(parse.reader().total_length()));
        let context = parse.context();
        let node_before_cursor = context.find_suggestion_context(cursor);
        let parent = node_before_cursor.parent.clone();
        let start = node_before_cursor.start_pos.min(cursor);

        let full_input = parse.reader().string().to_string();
        let truncated_input = full_input[..cursor].to_string();
        let truncated_input_lower_case = truncated_input.to_lowercase();

        let mut collected: Vec<Suggestions> = Vec::new();
        for node in parent.children() {
            let built = context.build(&truncated_input);
            let mut builder =
                SuggestionsBuilder::with_lower_case(&truncated_input, &truncated_input_lower_case, start);
            match node.list_suggestions(&built, &mut builder) {
                Ok(suggestions) => collected.push(suggestions),
                Err(_) => collected.push(Suggestions::empty()),
            }
        }
        Suggestions::merge(&full_input, &collected)
    }

    /// `getAllUsage`: every executable path under `node`, in "simple" form.
    pub fn get_all_usage(&self, node: &CommandNode<S>, source: &S, restricted: bool) -> Vec<String> {
        let mut result = Vec::new();
        self.collect_all_usage(node, source, &mut result, "", restricted);
        result
    }

    fn collect_all_usage(
        &self,
        node: &CommandNode<S>,
        source: &S,
        result: &mut Vec<String>,
        prefix: &str,
        restricted: bool,
    ) {
        if restricted && !node.can_use(source) {
            return;
        }
        if node.command().is_some() {
            result.push(prefix.to_string());
        }
        if let Some(redirect) = node.redirect() {
            let redirect_text = if redirect.same_node(&self.root) {
                "...".to_string()
            } else {
                format!("-> {}", redirect.usage_text())
            };
            let base = if prefix.is_empty() {
                node.usage_text()
            } else {
                prefix.to_string()
            };
            result.push(format!("{base}{ARGUMENT_SEPARATOR}{redirect_text}"));
        } else if node.child_count() > 0 {
            for child in node.children() {
                let child_prefix = if prefix.is_empty() {
                    child.usage_text()
                } else {
                    format!("{prefix}{ARGUMENT_SEPARATOR}{}", child.usage_text())
                };
                self.collect_all_usage(&child, source, result, &child_prefix, restricted);
            }
        }
    }

    /// `getSmartUsage`: the compressed "(1|2)", "[1]" form, one entry per usable child.
    pub fn get_smart_usage(
        &self,
        node: &CommandNode<S>,
        source: &S,
    ) -> Vec<(CommandNode<S>, String)> {
        let mut result = Vec::new();
        let optional = node.command().is_some();
        for child in node.children() {
            if let Some(usage) = self.smart_usage_of(&child, source, optional, false) {
                result.push((child, usage));
            }
        }
        result
    }

    fn smart_usage_of(
        &self,
        node: &CommandNode<S>,
        source: &S,
        optional: bool,
        deep: bool,
    ) -> Option<String> {
        if !node.can_use(source) {
            return None;
        }
        let self_text = if optional {
            format!("{USAGE_OPTIONAL_OPEN}{}{USAGE_OPTIONAL_CLOSE}", node.usage_text())
        } else {
            node.usage_text()
        };
        let child_optional = node.command().is_some();
        let open = if child_optional {
            USAGE_OPTIONAL_OPEN
        } else {
            USAGE_REQUIRED_OPEN
        };
        let close = if child_optional {
            USAGE_OPTIONAL_CLOSE
        } else {
            USAGE_REQUIRED_CLOSE
        };

        if !deep {
            if let Some(redirect) = node.redirect() {
                let redirect_text = if redirect.same_node(&self.root) {
                    "...".to_string()
                } else {
                    format!("-> {}", redirect.usage_text())
                };
                return Some(format!("{self_text}{ARGUMENT_SEPARATOR}{redirect_text}"));
            }
            let children: Vec<CommandNode<S>> = node
                .children()
                .into_iter()
                .filter(|child| child.can_use(source))
                .collect();
            if children.len() == 1 {
                if let Some(usage) =
                    self.smart_usage_of(&children[0], source, child_optional, child_optional)
                {
                    return Some(format!("{self_text}{ARGUMENT_SEPARATOR}{usage}"));
                }
            } else if children.len() > 1 {
                let mut child_usages: Vec<String> = Vec::new();
                for child in &children {
                    if let Some(usage) = self.smart_usage_of(child, source, child_optional, true) {
                        if !child_usages.contains(&usage) {
                            child_usages.push(usage);
                        }
                    }
                }
                if child_usages.len() == 1 {
                    let usage = child_usages[0].clone();
                    let wrapped = if child_optional {
                        format!("{USAGE_OPTIONAL_OPEN}{usage}{USAGE_OPTIONAL_CLOSE}")
                    } else {
                        usage
                    };
                    return Some(format!("{self_text}{ARGUMENT_SEPARATOR}{wrapped}"));
                }
                if child_usages.len() > 1 {
                    let mut builder = String::from(open);
                    let mut count = 0;
                    for child in &children {
                        if count > 0 {
                            builder.push_str(USAGE_OR);
                        }
                        builder.push_str(&child.usage_text());
                        count += 1;
                    }
                    if count > 0 {
                        builder.push_str(close);
                        return Some(format!("{self_text}{ARGUMENT_SEPARATOR}{builder}"));
                    }
                }
            }
        }

        Some(self_text)
    }

    /// `getPath(node)`: a path from the root to `node`, or empty when it is not in this tree.
    pub fn get_path(&self, target: &CommandNode<S>) -> Vec<String> {
        let mut paths: Vec<Vec<CommandNode<S>>> = Vec::new();
        collect_paths(&self.root, &mut paths, &mut Vec::new());
        for path in paths {
            if path.last().is_some_and(|node| node.same_node(target)) {
                return path
                    .into_iter()
                    .filter(|node| !node.is_root())
                    .map(|node| node.name().to_string())
                    .collect();
            }
        }
        Vec::new()
    }

    /// `findNode(path)`.
    pub fn find_node(&self, path: &[String]) -> Option<CommandNode<S>> {
        let mut node = self.root.clone();
        for name in path {
            node = node.child(name)?;
        }
        Some(node)
    }
}

fn collect_paths<S>(
    node: &CommandNode<S>,
    result: &mut Vec<Vec<CommandNode<S>>>,
    parents: &mut Vec<CommandNode<S>>,
) {
    let mut current = parents.clone();
    current.push(node.clone());
    result.push(current.clone());
    for child in node.children() {
        let mut next = current.clone();
        collect_paths(&child, result, &mut next);
    }
}

// ---------------------------------------------------------------------------------------
// Live front-end: parse-and-validate a command line against the caller's op level
// ---------------------------------------------------------------------------------------

/// How a [`BrigadierTree::parse_command_for_level`] call failed.
///
/// `UnknownRoot` and `InvalidBranch` wrap the `CommandSyntaxException` the walk produced;
/// `PermissionDenied` is raised before the walk starts, when the root literal's required
/// level exceeds the caller's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrigadierErrorKind {
    /// The first word is not a registered root literal (vanilla `dispatcherUnknownCommand`).
    UnknownRoot,
    /// The root matched but a later literal or argument did not (vanilla
    /// `dispatcherUnknownArgument`, or the argument node's own parse error).
    InvalidBranch,
    /// The root literal's required permission level exceeds the caller's. Raised before the
    /// tree walk so a denied command cannot be probed for syntax.
    PermissionDenied,
}

/// A live parse failure: the vanilla `CommandSyntaxException` (input + cursor, so `context()`
/// renders the `...<input><--[HERE]` window the client displays) plus the failure
/// classification and the root literal the line tried to run.
#[derive(Clone, Debug)]
pub struct BrigadierSyntaxError {
    kind: BrigadierErrorKind,
    exception: CommandSyntaxException,
    root_literal: String,
    required_level: Option<u8>,
}

impl BrigadierSyntaxError {
    /// Which stage rejected the line.
    pub fn kind(&self) -> BrigadierErrorKind {
        self.kind
    }

    /// The wrapped `CommandSyntaxException`.
    pub fn exception(&self) -> &CommandSyntaxException {
        &self.exception
    }

    /// `getMessage`: the message plus ` at position N: <context>` when the cursor is known.
    pub fn full_message(&self) -> String {
        self.exception.full_message()
    }

    /// `getRawMessage`.
    pub fn raw_message(&self) -> &str {
        self.exception.raw_message()
    }

    /// `getContext`: the `<--[HERE]` window, or `None` without input/position.
    pub fn context(&self) -> Option<String> {
        self.exception.context()
    }

    /// The root literal (first word) the line tried to run; `""` for an empty line.
    pub fn root_literal(&self) -> &str {
        &self.root_literal
    }

    /// The op level the root literal required — `Some` for `PermissionDenied` only.
    pub fn required_level(&self) -> Option<u8> {
        self.required_level
    }

    /// The input the exception was created against (no leading `/`).
    pub fn input(&self) -> Option<&str> {
        self.exception.input()
    }

    /// `getCursor`; `-1` when the exception carries no position.
    pub fn cursor(&self) -> i32 {
        self.exception.cursor()
    }
}

impl fmt::Display for BrigadierSyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.exception.fmt(f)
    }
}

impl std::error::Error for BrigadierSyntaxError {}

/// A line [`BrigadierTree::parse_command_for_level`] accepted: the resolved root literal plus
/// the built `CommandContext` (node path, argument values, the executable at the tail).
pub struct ParsedBrigadierCommand<S> {
    input: String,
    root_literal: String,
    context: CommandContext<S>,
}

impl<S> fmt::Debug for ParsedBrigadierCommand<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedBrigadierCommand")
            .field("input", &self.input)
            .field("root_literal", &self.root_literal)
            .field("path", &self.path())
            .finish_non_exhaustive()
    }
}

impl<S> ParsedBrigadierCommand<S> {
    /// The line as parsed — without a leading `/`.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The root literal the line resolved to. Alias names report themselves (`tp` stays `tp`;
    /// the tree does not canonicalize them, matching vanilla).
    pub fn root_literal(&self) -> &str {
        &self.root_literal
    }

    /// The built context: `nodes()`, `arguments()` and `command()` all read from it.
    pub fn context(&self) -> &CommandContext<S> {
        &self.context
    }

    /// The names of every node the walk consumed, root literal first.
    pub fn path(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut current = &self.context;
        loop {
            names.extend(
                current
                    .nodes()
                    .iter()
                    .map(|node| node.node().name().to_owned()),
            );
            match current.child() {
                Some(child) => current = child,
                None => return names,
            }
        }
    }

    /// `getArgument(name, Class)` against the executable tail context.
    pub fn argument<T: 'static>(&self, name: &str) -> Result<&T, ArgumentError> {
        self.context.last_child().argument(name)
    }
}

/// The live command front-end: one `CommandDispatcher` plus a `root literal -> required op
/// level` lookup. `parse_command_for_level` is what `Commands.performCommand` does ahead of
/// dispatch — the `hasPermission` gate on the root runs first, then the registered literal
/// and argument nodes walk the line and produce the vanilla `<--[HERE]` syntax context.
pub struct BrigadierTree<S> {
    dispatcher: CommandDispatcher<S>,
    required_level: Arc<dyn Fn(&str) -> u8 + Send + Sync>,
}

impl<S: Clone> BrigadierTree<S> {
    /// `dispatcher` is the registered tree to walk; `required_level` maps a root literal to
    /// the op level vanilla's `requires(...)`/`hasPermission` gate demands for it.
    pub fn new(
        dispatcher: CommandDispatcher<S>,
        required_level: impl Fn(&str) -> u8 + Send + Sync + 'static,
    ) -> Self {
        Self {
            dispatcher,
            required_level: Arc::new(required_level),
        }
    }

    /// The registered tree this front-end walks.
    pub fn dispatcher(&self) -> &CommandDispatcher<S> {
        &self.dispatcher
    }

    /// The op level `literal` requires under this tree's lookup.
    pub fn required_level(&self, literal: &str) -> u8 {
        (self.required_level)(literal)
    }

    /// Parse `line` (with or without the leading `/`) for a source at `op_level`.
    ///
    /// The root literal's required level is checked before the walk; a caller below it gets a
    /// `PermissionDenied` error whose exception reads `Unknown command` — the shape a literal
    /// the source cannot see takes in vanilla. A caller at or above it walks the tree; the
    /// error selection mirrors `CommandDispatcher.execute`: a lone recorded node exception
    /// wins, then the context range splits `Unknown command` from `Incorrect argument for
    /// command`, and a line whose tail node carries no command is not executable.
    pub fn parse_command_for_level(
        &self,
        line: &str,
        op_level: u8,
    ) -> Result<ParsedBrigadierCommand<S>, BrigadierSyntaxError>
    where
        S: Default,
    {
        let input = line.strip_prefix('/').unwrap_or(line).trim_start();
        let root_literal = input.split_whitespace().next().unwrap_or("").to_owned();
        let required = (self.required_level)(&root_literal);
        let syntax_error = |kind: BrigadierErrorKind, exception: CommandSyntaxException| {
            BrigadierSyntaxError {
                kind,
                exception,
                root_literal: root_literal.clone(),
                required_level: None,
            }
        };
        if op_level < required {
            let mut error = syntax_error(
                BrigadierErrorKind::PermissionDenied,
                BUILT_IN_EXCEPTIONS
                    .dispatcher_unknown_command()
                    .create_with_context(&StringReader::new(input)),
            );
            error.required_level = Some(required);
            return Err(error);
        }
        let results = self.dispatcher.parse(input, S::default());
        if results.reader().can_read() {
            let (kind, exception) = if results.exceptions().len() == 1 {
                (
                    BrigadierErrorKind::InvalidBranch,
                    results.exceptions()[0].1.clone(),
                )
            } else if results.context().range().is_empty() {
                (
                    BrigadierErrorKind::UnknownRoot,
                    BUILT_IN_EXCEPTIONS
                        .dispatcher_unknown_command()
                        .create_with_context(results.reader()),
                )
            } else {
                (
                    BrigadierErrorKind::InvalidBranch,
                    BUILT_IN_EXCEPTIONS
                        .dispatcher_unknown_argument()
                        .create_with_context(results.reader()),
                )
            };
            return Err(syntax_error(kind, exception));
        }
        let context = results.context().build(input);
        if ContextChain::try_flatten(&context).is_none() {
            return Err(syntax_error(
                BrigadierErrorKind::InvalidBranch,
                BUILT_IN_EXCEPTIONS
                    .dispatcher_unknown_command()
                    .create_with_context(results.reader()),
            ));
        }
        Ok(ParsedBrigadierCommand {
            input: input.to_owned(),
            root_literal,
            context,
        })
    }
}

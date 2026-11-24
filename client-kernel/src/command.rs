// SPDX-License-Identifier: LGPL-3.0-or-later
use crate::{brigadier, Error};
use std::ops::Range;

#[derive(Default)]
pub struct CommandDispatcher(brigadier::CommandDispatcher<()>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandParse {
    pub cursor: usize,
    pub executable: bool,
}

pub struct Suggestions(brigadier::Suggestions);

impl Suggestions {
    /// Byte range to replace in the original input.
    pub fn range(&self) -> Range<usize> {
        let range = self.0.range();
        range.start()..range.end()
    }

    pub fn len(&self) -> usize {
        self.0.list().len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        self.0.list().get(index).map(brigadier::Suggestion::text)
    }
}

impl CommandDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one executable root literal for parsing and completion.
    pub fn register_literal(&mut self, literal: &str) -> Result<(), Error> {
        if literal.is_empty()
            || literal
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(Error::InvalidArgument);
        }
        self.0
            .register(brigadier::ArgumentBuilder::literal(literal).executes(|_| Ok(1)))
            .map(|_| ())
            .map_err(|_| Error::InvalidArgument)
    }

    pub fn parse(&self, input: &str) -> CommandParse {
        let parsed = self.0.parse(input, ());
        CommandParse {
            cursor: parsed.reader().cursor(),
            executable: !parsed.reader().can_read() && parsed.context().command().is_some(),
        }
    }

    /// Complete at a UTF-8 byte boundary in the original input.
    pub fn complete_suggestions(&self, input: &str, cursor: usize) -> Result<Suggestions, Error> {
        if !input.is_char_boundary(cursor) {
            return Err(Error::OutOfRange);
        }
        let parsed = self.0.parse(input, ());
        Ok(Suggestions(
            self.0.get_completion_suggestions_at(&parsed, cursor),
        ))
    }
}

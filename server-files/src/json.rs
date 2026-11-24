//! The smallest JSON reader/writer that carries the Vanilla admin files.
//!
//! The implementation is the workspace-canonical [`megu_core::json`]:
//! [`parse`] fails closed — malformed input is a [`JsonError`], never a
//! partial document — and objects keep the file's key order (`Vec` of
//! pairs), so a parse -> serialize round trip preserves it. This module
//! keeps the historical `server_files::json` surface (`Json`, `JsonError`,
//! `MAX_DEPTH`, `parse`, `quote`) as thin aliases over the shared crate.

pub use megu_core::json::JsonError;

/// A parsed JSON value. `Object` pairs are in document order.
pub type Json = megu_core::json::JsonValue;

/// Deepest nesting [`parse`] accepts; the Vanilla files are `[{…}]`, two
/// deep. Matches `megu_core::json::JsonLimits::default().max_depth`.
pub const MAX_DEPTH: usize = 64;

/// Parse one JSON document; trailing non-whitespace is an error.
pub fn parse(text: &str) -> Result<Json, JsonError> {
    megu_core::json::parse_json(text)
}

/// A JSON string literal for `s` including the surrounding quotes.
pub fn quote(s: &str) -> String {
    megu_core::json::quote_json_string(s)
}

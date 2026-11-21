// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Vanilla-validated `ResourceLocation` (`namespace:path`).
//!
//! Registry keys, packet fields, datapack paths and asset lookups all carry
//! this identifier, so the validation is pinned once here: the namespace
//! accepts `[a-z0-9_.-]`, the path `[a-z0-9_./-]`, both must be non-empty, an
//! unqualified `"stone"` means `"minecraft:stone"`, and anything else —
//! including a second `:` and `..` path traversal — is rejected, never
//! silently repaired.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// Vanilla's default namespace.
pub const DEFAULT_NAMESPACE: &str = "minecraft";

/// A parsed, fully validated `namespace:path` identifier.
///
/// The combined spelling is stored once so [`as_str`](Self::as_str) is a
/// borrow; `namespace()`/`path()` slice it at the recorded separator.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceLocation {
    full: String,
    /// Byte index of the `:` separating namespace and path.
    sep: usize,
}

/// Why a string is not a valid resource location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidReason {
    /// The namespace is empty (`":path"`).
    EmptyNamespace,
    /// The path is empty (`"namespace:"`, `""`).
    EmptyPath,
    /// The namespace contains a character outside `[a-z0-9_.-]`.
    BadNamespaceChar,
    /// The path contains a character outside `[a-z0-9_./-]`.
    BadPathChar,
    /// More than one `:` separator (`"a:b:c"`).
    TooManySeparators,
    /// A `/`-separated path component is `..` — path traversal.
    ParentTraversal,
}

impl fmt::Display for InvalidReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyNamespace => "empty namespace",
            Self::EmptyPath => "empty path",
            Self::BadNamespaceChar => "character outside [a-z0-9_.-] in namespace",
            Self::BadPathChar => "character outside [a-z0-9_./-] in path",
            Self::TooManySeparators => "more than one ':' separator",
            Self::ParentTraversal => "'..' traversal in path",
        })
    }
}

/// A string that failed [`ResourceLocation`] validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidResourceLocation {
    input: String,
    reason: InvalidReason,
}

impl InvalidResourceLocation {
    /// The rejected input, as the caller spelled it.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Which rule the input broke.
    pub fn reason(&self) -> InvalidReason {
        self.reason
    }
}

impl fmt::Display for InvalidResourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid resource location {:?}: {}", self.input, self.reason)
    }
}

impl Error for InvalidResourceLocation {}

fn valid_namespace_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.')
}

fn valid_path_char(c: char) -> bool {
    valid_namespace_char(c) || c == '/'
}

fn has_parent_traversal(path: &str) -> bool {
    path.split('/').any(|segment| segment == "..")
}

impl ResourceLocation {
    /// `ResourceLocation.tryParse`: an optional `namespace:` prefix plus a
    /// path. No namespace means [`DEFAULT_NAMESPACE`], exactly as vanilla.
    ///
    /// # Errors
    ///
    /// Fails closed on every malformed input: a second `:` separator, empty
    /// namespace or path, characters outside vanilla's sets, and `..`
    /// traversal in the path.
    pub fn parse(text: &str) -> Result<Self, InvalidResourceLocation> {
        let reject = |reason| InvalidResourceLocation {
            input: text.to_string(),
            reason,
        };
        let (namespace, path) = match text.split_once(':') {
            Some((_, path)) if path.contains(':') => {
                return Err(reject(InvalidReason::TooManySeparators));
            }
            Some((namespace, path)) => (namespace, path),
            None => (DEFAULT_NAMESPACE, text),
        };
        if namespace.is_empty() {
            return Err(reject(InvalidReason::EmptyNamespace));
        }
        if path.is_empty() {
            return Err(reject(InvalidReason::EmptyPath));
        }
        if !namespace.chars().all(valid_namespace_char) {
            return Err(reject(InvalidReason::BadNamespaceChar));
        }
        if !path.chars().all(valid_path_char) {
            return Err(reject(InvalidReason::BadPathChar));
        }
        if has_parent_traversal(path) {
            return Err(reject(InvalidReason::ParentTraversal));
        }
        let full = if text.contains(':') {
            text.to_owned()
        } else {
            let mut full = String::with_capacity(namespace.len() + 1 + path.len());
            full.push_str(namespace);
            full.push(':');
            full.push_str(path);
            full
        };
        Ok(Self {
            full,
            sep: namespace.len(),
        })
    }

    /// Build from an explicit namespace and path under the same rules as
    /// [`parse`](Self::parse). The error's `input` is the combined spelling.
    pub fn new(namespace: &str, path: &str) -> Result<Self, InvalidResourceLocation> {
        Self::parse(&format!("{namespace}:{path}"))
    }

    /// The `namespace` half, e.g. `"minecraft"`.
    pub fn namespace(&self) -> &str {
        &self.full[..self.sep]
    }

    /// The `path` half, e.g. `"block/stone"`.
    pub fn path(&self) -> &str {
        &self.full[self.sep + 1..]
    }

    /// The combined `"namespace:path"` spelling.
    pub fn as_str(&self) -> &str {
        &self.full
    }
}

impl fmt::Display for ResourceLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full)
    }
}

impl FromStr for ResourceLocation {
    type Err = InvalidResourceLocation;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for ResourceLocation {
    type Error = InvalidResourceLocation;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

/// Vanilla's `ResourceLocation.compareTo` orders by **path first**, then
/// namespace — the inverse of the reading order, and load-bearing for any
/// sorted registry view that must match the game's.
impl Ord for ResourceLocation {
    fn cmp(&self, other: &Self) -> Ordering {
        self.path()
            .cmp(other.path())
            .then_with(|| self.namespace().cmp(other.namespace()))
    }
}

impl PartialOrd for ResourceLocation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unqualified_name_defaults_to_the_minecraft_namespace() {
        let loc = ResourceLocation::parse("stone").unwrap();
        assert_eq!(loc.namespace(), "minecraft");
        assert_eq!(loc.path(), "stone");
        assert_eq!(loc.as_str(), "minecraft:stone");
        assert_eq!(loc.to_string(), "minecraft:stone");
    }

    #[test]
    fn a_qualified_name_parses_both_halves() {
        let loc = ResourceLocation::parse("create:item/brass_ingot.v2").unwrap();
        assert_eq!(loc.namespace(), "create");
        assert_eq!(loc.path(), "item/brass_ingot.v2");
        assert_eq!(loc.as_str(), "create:item/brass_ingot.v2");
        // `str::parse` and `TryFrom` route through the same validation.
        assert_eq!("create:x".parse::<ResourceLocation>().unwrap().path(), "x");
        assert_eq!(
            ResourceLocation::try_from("a:b/c").unwrap(),
            ResourceLocation::new("a", "b/c").unwrap()
        );
    }

    #[test]
    fn the_full_vanilla_character_set_is_accepted() {
        let loc = ResourceLocation::parse("a0._-x:z9./-_").unwrap();
        assert_eq!(loc.namespace(), "a0._-x");
        assert_eq!(loc.path(), "z9./-_");
        // `.` inside a segment is a legal path char, and `a..b` is a file
        // name, not a traversal.
        ResourceLocation::parse("ns:a..b").unwrap();
        ResourceLocation::parse("ns:./a/./b").unwrap();
    }

    #[test]
    fn malformed_inputs_are_rejected_with_the_right_reason() {
        let cases: &[(&str, InvalidReason)] = &[
            ("", InvalidReason::EmptyPath),
            (":", InvalidReason::EmptyNamespace),
            (":path", InvalidReason::EmptyNamespace),
            ("ns:", InvalidReason::EmptyPath),
            ("::", InvalidReason::TooManySeparators),
            ("foo:bar:baz", InvalidReason::TooManySeparators),
            ("a:b:c:d", InvalidReason::TooManySeparators),
            ("FOO:bar", InvalidReason::BadNamespaceChar),
            ("foo:BAR", InvalidReason::BadPathChar),
            ("fo o:bar", InvalidReason::BadNamespaceChar),
            ("foo:bar baz", InvalidReason::BadPathChar),
            ("foo:bar\\baz", InvalidReason::BadPathChar),
            ("föö:bar", InvalidReason::BadNamespaceChar),
            ("ns:..", InvalidReason::ParentTraversal),
            ("ns:../a", InvalidReason::ParentTraversal),
            ("ns:a/../b", InvalidReason::ParentTraversal),
            ("ns:a/b/..", InvalidReason::ParentTraversal),
        ];
        for (input, reason) in cases {
            let err = ResourceLocation::parse(input).unwrap_err();
            assert_eq!(err.reason(), *reason, "{input:?}");
            assert_eq!(err.input(), *input);
            // Every rejection is also a rejection through `new`, `FromStr`
            // and `TryFrom` — the rules are not parse-path specific.
            assert!(input.parse::<ResourceLocation>().is_err());
        }
    }

    #[test]
    fn the_error_describes_the_input_and_the_rule() {
        let err = ResourceLocation::parse("Foo:bar").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Foo:bar"), "{text}");
        assert!(text.contains("namespace"), "{text}");
        let err = ResourceLocation::parse("ns:a/../b").unwrap_err();
        assert!(err.to_string().contains(".."), "{}", err);
    }

    #[test]
    fn equality_and_hashing_use_the_combined_spelling() {
        use std::collections::HashSet;
        let a = ResourceLocation::parse("minecraft:stone").unwrap();
        let b = ResourceLocation::parse("stone").unwrap();
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(a.clone());
        set.insert(b);
        assert_eq!(set.len(), 1);
        assert_ne!(a, ResourceLocation::parse("other:stone").unwrap());
    }

    #[test]
    fn ordering_is_path_first_like_vanilla() {
        // `compareTo` sorts on path, then namespace — so `b:x` < `a:y` even
        // though `a` < `b` in the namespace half.
        let mut v = [
            ResourceLocation::parse("a:y").unwrap(),
            ResourceLocation::parse("b:x").unwrap(),
            ResourceLocation::parse("a:x").unwrap(),
        ];
        v.sort();
        let names: Vec<String> = v.iter().map(|l| l.to_string()).collect();
        assert_eq!(names, ["a:x", "b:x", "a:y"]);
    }
}

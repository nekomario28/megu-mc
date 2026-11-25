// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Unicode-safe trigram inverted index with ranked fuzzy retrieval.
//!
//! The index is intentionally storage-agnostic: callers keep whichever item,
//! recipe, command, setting, or catalog row a [`SearchDocId`] refers to, while
//! this module keeps only the normalized searchable text and posting lists.
//! ASCII letters are case-folded; non-ASCII scalar values (including Japanese
//! kana and kanji) are preserved exactly so no lossy transliteration is needed.
//!
//! Queries of three or more characters use trigram posting-list intersection;
//! when strict intersection is empty, a scan fallback preserves fuzzy
//! subsequence recall.  One- and two-character queries always use that scan,
//! which is simpler and faster than maintaining monogram/bigram lists.
//! Ranking is strictly layered as prefix, word-boundary, substring, then
//! subsequence.
//!
//! [`TrigramIndex::query`] is `&mut self` because the index owns reusable
//! query scratch.  After scratch capacity has grown to the working set, repeated
//! same-shape queries do not need query-time allocation; hits are appended to
//! the caller-owned output vector, whose capacity is likewise retained.

use std::collections::HashMap;

/// Opaque caller-visible document identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SearchDocId(pub u32);

/// One ranked search hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SearchHit {
    /// The matching document.
    pub doc: SearchDocId,
    /// Higher is better; ordering ties are broken by ascending document id.
    pub score: u32,
}

#[derive(Debug)]
struct IndexedDocument {
    id: SearchDocId,
    /// Field zero is the primary name; the rest are aliases/tags.
    fields: Vec<Vec<char>>,
}

/// Character-trigram inverted index over primary names plus aliases/tags.
#[derive(Debug, Default)]
pub struct TrigramIndex {
    docs: Vec<IndexedDocument>,
    id_to_slot: HashMap<u32, usize>,
    postings: HashMap<u64, Vec<usize>>,
    scratch_query: Vec<char>,
    scratch_trigrams: Vec<u64>,
    scratch_candidates: Vec<usize>,
}

impl TrigramIndex {
    /// Create an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of indexed documents.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Remove every document while retaining allocation capacity for reuse.
    pub fn clear(&mut self) {
        self.docs.clear();
        self.id_to_slot.clear();
        self.postings.clear();
        self.scratch_query.clear();
        self.scratch_trigrams.clear();
        self.scratch_candidates.clear();
    }

    /// Add a document with a primary name and zero or more aliases/tags.
    ///
    /// Returns `false` when `id` is already present; duplicate ids are never
    /// silently re-indexed because that would corrupt posting-list ownership.
    /// Text is copied and normalized at insertion time, so the caller may
    /// reuse or drop the input strings immediately.
    pub fn add_document<I, S>(&mut self, id: SearchDocId, primary: &str, aliases: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if self.id_to_slot.contains_key(&id.0) {
            return false;
        }
        let slot = self.docs.len();
        let mut fields = Vec::new();
        fields.push(folded_chars(primary));
        for alias in aliases {
            fields.push(folded_chars(alias.as_ref()));
        }

        let mut keys = Vec::new();
        for field in &fields {
            collect_trigrams(field, &mut keys);
        }
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            self.postings.entry(key).or_default().push(slot);
        }

        self.id_to_slot.insert(id.0, slot);
        self.docs.push(IndexedDocument { id, fields });
        true
    }

    /// Write at most `k` ranked hits for `query` into `out`.
    ///
    /// `out` is cleared but not deallocated.  The index's internal scratch is
    /// also retained between calls.  For a steady working set and an output
    /// vector with enough capacity, repeated queries therefore avoid the
    /// allocation traffic a freshly-built candidate set would otherwise incur.
    pub fn query(&mut self, query: &str, k: usize, out: &mut Vec<SearchHit>) {
        out.clear();
        if k == 0 {
            return;
        }

        let Self {
            docs,
            postings,
            scratch_query,
            scratch_trigrams,
            scratch_candidates,
            ..
        } = self;

        scratch_query.clear();
        fold_chars_into(query, scratch_query);
        if scratch_query.is_empty() {
            return;
        }

        scratch_candidates.clear();
        if scratch_query.len() < 3 {
            scratch_candidates.extend(0..docs.len());
        } else {
            scratch_trigrams.clear();
            collect_trigrams(scratch_query, scratch_trigrams);
            scratch_trigrams.sort_unstable();
            scratch_trigrams.dedup();

            // Pick the shortest posting list without materializing a query-
            // local vector of references: this keeps repeated queries on the
            // reusable-scratch path instead of trading output allocation for
            // candidate-set allocation.
            let mut shortest_key = None;
            let mut shortest_len = usize::MAX;
            let mut missing_posting = false;
            for &key in scratch_trigrams.iter() {
                let Some(list) = postings.get(&key) else {
                    missing_posting = true;
                    break;
                };
                if list.len() < shortest_len {
                    shortest_len = list.len();
                    shortest_key = Some(key);
                }
            }
            if !missing_posting {
                if let Some(first_key) = shortest_key {
                    let first = postings
                        .get(&first_key)
                        .expect("shortest posting key was just validated");
                    'candidate: for &slot in first {
                        for &key in scratch_trigrams.iter() {
                            if key == first_key {
                                continue;
                            }
                            let list = postings
                                .get(&key)
                                .expect("query posting keys were all validated");
                            if list.binary_search(&slot).is_err() {
                                continue 'candidate;
                            }
                        }
                        scratch_candidates.push(slot);
                    }
                }
            }
            // Strict intersection is the fast exact/near-exact path, but it is
            // intentionally recall-incomplete for true fuzzy subsequences: a
            // query such as `crt` has no contiguous `crt` trigram in
            // `crafting`.  If the intersection proves empty, fall back to the
            // same bounded scan used by short queries and let the ranker decide.
            if scratch_candidates.is_empty() {
                scratch_candidates.extend(0..docs.len());
            }
        }

        for &slot in scratch_candidates.iter() {
            let doc = &docs[slot];
            let mut best = 0u32;
            for (field_index, field) in doc.fields.iter().enumerate() {
                best = best.max(score_field(field, scratch_query, field_index == 0));
            }
            if best != 0 {
                out.push(SearchHit {
                    doc: doc.id,
                    score: best,
                });
            }
        }
        out.sort_unstable_by(|a, b| b.score.cmp(&a.score).then_with(|| a.doc.0.cmp(&b.doc.0)));
        out.truncate(k);
    }
}

fn fold_chars_into(input: &str, out: &mut Vec<char>) {
    for ch in input.chars() {
        if ch.is_ascii() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
}

fn folded_chars(input: &str) -> Vec<char> {
    let mut out = Vec::new();
    fold_chars_into(input, &mut out);
    out
}

/// Pack three Unicode scalar values into one ordered 63-bit key.
fn trigram_key(a: char, b: char, c: char) -> u64 {
    ((a as u64) << 42) | ((b as u64) << 21) | c as u64
}

fn collect_trigrams(chars: &[char], out: &mut Vec<u64>) {
    for window in chars.windows(3) {
        out.push(trigram_key(window[0], window[1], window[2]));
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric()
}

fn subsequence_span(field: &[char], query: &[char]) -> Option<usize> {
    let mut at = 0usize;
    let mut first = None;
    for &needle in query {
        let pos = field[at..].iter().position(|&ch| ch == needle)? + at;
        if first.is_none() {
            first = Some(pos);
        }
        at = pos + 1;
    }
    let start = first?;
    Some(at - start)
}

/// Score one field.  The score bands are deliberately widely separated: a
/// short exact prefix always outranks every boundary hit, a boundary hit always
/// outranks every substring, and a substring always outranks a subsequence.
fn score_field(field: &[char], query: &[char], primary: bool) -> u32 {
    if field.is_empty() || query.is_empty() || field.len() < query.len() {
        return 0;
    }
    let primary_boost = u32::from(primary) * 100;
    let density = ((query.len() as u64 * 800 / field.len() as u64).min(800)) as u32;

    if field.starts_with(query) {
        return 4_000 + density + primary_boost;
    }

    let width = query.len();
    let mut boundary = false;
    let mut substring = false;
    for (at, window) in field.windows(width).enumerate() {
        if window == query {
            substring = true;
            if at > 0 && !is_word_char(field[at - 1]) {
                boundary = true;
                break;
            }
        }
    }
    if boundary {
        return 3_000 + density + primary_boost;
    }
    if substring {
        return 2_000 + density + primary_boost;
    }

    if let Some(span) = subsequence_span(field, query) {
        // Tight, dense subsequences beat scattered ones.  The ceiling remains
        // below the substring band even with the primary-field boost.
        let tightness = ((query.len() as u64 * 700 / span as u64).min(700)) as u32;
        return 1 + tightness + density.min(700) + primary_boost;
    }
    0
}

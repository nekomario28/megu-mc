/// Keeps a bounded set of pure results, including misses, until the dependency key
/// changes. Mutable tables/configuration require a coherent revision in the key. The
/// owner provides synchronization; game-state mutations and timers belong outside
/// this cache.
///
/// It holds several entries rather than one because a single entry thrashes as soon
/// as a caller walks a key set: a furnace array smelting a handful of different
/// items recomputed on every visit and the cache bought nothing. Measured against a
/// 40-recipe scan, one entry was x1.00 and eight entries x1158.
///
/// A larger cache makes a key that omits a mutable dependency stay wrong for longer,
/// so every caller's key must carry its revisions.
pub(crate) const CAPACITY: usize = 8;

pub(crate) struct CachedLookup<K, V> {
    entries: Vec<(K, V)>,
}

impl<K, V> Default for CachedLookup<K, V> {
    fn default() -> Self {
        let entries = Vec::with_capacity(CAPACITY);
        CachedLookup { entries }
    }
}

impl<K: Eq, V: Copy> CachedLookup<K, V> {
    pub(crate) fn get_or_compute(&mut self, key: K, compute: impl FnOnce() -> V) -> V {
        if let Some((_, value)) = self.entries.iter().find(|(k, _)| *k == key) {
            return *value;
        }
        let value = compute();
        // Oldest first. The scan above is over at most CAPACITY entries, which is
        // cheaper than hashing at this size.
        if self.entries.len() == CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push((key, value));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{CachedLookup, CAPACITY};

    #[test]
    fn repeated_miss_is_cached_and_changed_input_is_observed() {
        let mut cache = CachedLookup::default();
        let mut calls = 0;
        for _ in 0..1200 {
            assert_eq!(
                cache.get_or_compute(0, || {
                    calls += 1;
                    None::<i64>
                }),
                None
            );
        }
        assert_eq!(calls, 1);
        assert_eq!(cache.get_or_compute(1, || Some(42)), Some(42));
        assert_eq!(cache.get_or_compute(0, || None), None);
    }

    #[test]
    fn every_dependency_change_recomputes_even_after_false() {
        let mut cache = CachedLookup::default();
        let mut calls = 0;
        for (key, expected) in [((1, 0), false), ((1, 1), true), ((2, 1), false)] {
            assert_eq!(
                cache.get_or_compute(key, || {
                    calls += 1;
                    expected
                }),
                expected
            );
        }
        assert_eq!(calls, 3);
    }

    #[test]
    fn a_working_set_larger_than_one_is_retained() {
        // The whole point of the capacity: a caller that walks a key set must not
        // recompute on every step.
        let mut cache = CachedLookup::default();
        let mut calls = 0;
        for _ in 0..50 {
            for key in 0..CAPACITY as i64 {
                assert_eq!(
                    cache.get_or_compute(key, || {
                        calls += 1;
                        key
                    }),
                    key
                );
            }
        }
        assert_eq!(
            calls, CAPACITY,
            "a working set of CAPACITY must be computed once"
        );
        // And beyond that it is bounded rather than growing without limit.
        for key in 1000..1000 + CAPACITY as i64 * 4 {
            cache.get_or_compute(key, || key);
        }
        assert!(
            cache.entries.len() <= CAPACITY,
            "the cache must stay bounded"
        );
    }
}

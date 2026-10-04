/// Keeps one pure result, including misses, until its complete dependency key changes.
/// Mutable tables/configuration require a coherent revision in the key. The owner
/// provides synchronization; game-state mutations and timers belong outside this cache.
pub(crate) struct CachedLookup<K, V> {
    entry: Option<(K, V)>,
}

impl<K, V> Default for CachedLookup<K, V> {
    fn default() -> Self {
        Self { entry: None }
    }
}

impl<K: Eq, V: Copy> CachedLookup<K, V> {
    pub(crate) fn get_or_compute(&mut self, key: K, compute: impl FnOnce() -> V) -> V {
        if let Some((stored_key, value)) = &self.entry {
            if stored_key == &key {
                return *value;
            }
        }
        let value = compute();
        self.entry = Some((key, value));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::CachedLookup;

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
}

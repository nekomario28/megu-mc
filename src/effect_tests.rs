use super::*;

#[test]
fn totem_replaces_existing_effects_with_vanilla_grant() {
    let mut effects = vec![
        Eff { id: 18, amp: 2, left: 120 }, // poison must not survive death protection
        Eff { id: 9, amp: 4, left: 1 },
        Eff { id: 21, amp: 0, left: 40 },
        Eff { id: 21, amp: 0, left: 40 },
    ];
    // The production clock is still 4TPS. Compare its stored durations in
    // vanilla game ticks; remove this conversion during the atomic 20TPS cutover.
    let snapshot = |effects: &[Eff]| {
        let mut result: Vec<_> = effects.iter().map(|e| (e.id, e.amp, e.left * 5)).collect();
        result.sort_unstable();
        result
    };
    let expected = vec![(9, 1, 900), (11, 0, 800), (21, 1, 100)];
    totem_effects(&mut effects);
    assert_eq!(snapshot(&effects), expected);
    totem_effects(&mut effects);
    assert_eq!(snapshot(&effects), expected, "repeated grants must not duplicate effects");
}

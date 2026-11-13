use super::*;
use std::collections::BTreeMap;

#[test]
fn native_abandoned_mineshaft_loot_matches_all_19_seeds() {
    let tsv = include_str!("../tests/data/mineshaft_loot_1211.tsv");
    let mut expected: BTreeMap<i64, Vec<LootEntry>> = BTreeMap::new();
    for line in tsv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f: Vec<_> = line.split('\t').collect();
        assert_eq!(f.len(), 6, "bad line: {line:?}");
        let seed: i64 = f[0].parse().unwrap();
        let slot: i16 = f[1].parse().unwrap();
        let item: &'static str = Box::leak(f[2].to_string().into_boxed_str());
        let count: u8 = f[3].parse().unwrap();
        let enchantment = if f[4] == "-" {
            None
        } else {
            let name: &'static str = Box::leak(f[4].to_string().into_boxed_str());
            let level: i32 = f[5].parse().unwrap();
            Some((name, level))
        };
        expected.entry(seed).or_default().push(LootEntry {
            slot,
            item,
            count,
            enchantment,
        });
    }
    assert_eq!(expected.len(), 19);
    for (seed, want) in expected {
        let got = abandoned_mineshaft(seed);
        assert_eq!(got, want, "mismatch for seed {seed}");
    }
}

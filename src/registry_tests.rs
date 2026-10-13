use super::*;
use crate::feature::breg;

/// The block/item knowledge is spread across several hand-maintained tables, and
/// the gate the registry slice needs is also the tool that says whether they agree
/// today.
///
/// The first version of this test assumed `state_to_item` and `item_to_state` were
/// inverse mappings. They are not, and the failures it reported were the test being
/// wrong, not the tables: mining stone drops cobblestone while the stone item places
/// stone. They answer two different questions - *what does this block drop* and
/// *what does this item place* - so the invariant is not that they round-trip.
///
/// What can be required is that the item-to-state direction is self-consistent: the
/// block an item places must be the block named after that item.
#[test]
fn an_item_places_the_block_it_is_named_after() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let mut mismatches = Vec::new();
    let mut placeable = 0;
    for item in 0..4096i64 {
        let Some(st) = item_to_state(item) else { continue };
        placeable += 1;
        if (st as usize) >= reg.state_count() {
            mismatches.push(format!("item {item} -> state {st}, outside the registry"));
            continue;
        }
        let block = reg.name_of(st);
        match name_to_item(block) {
            Some(back) if back == item => {}
            Some(back) => mismatches.push(format!(
                "item {item} places {block}, whose name is item {back}"
            )),
            None => mismatches.push(format!("item {item} places {block}, which has no item")),
        }
    }
    assert!(placeable > 0, "the table must not be empty");
    assert!(
        mismatches.is_empty(),
        "{} placement mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// The drop direction cannot be checked by round-tripping either, but every block
/// that claims a drop must at least be a block the registry knows and can name, and
/// the drop must be inside the item id space the rest of the server uses.
#[test]
fn every_block_that_drops_something_is_a_block_the_registry_knows() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let mut problems = Vec::new();
    let mut with_drop = 0;
    for st in 0..reg.state_count() as u32 {
        let Some(item) = state_to_item(st) else { continue };
        with_drop += 1;
        if !(0..4096).contains(&item) {
            problems.push(format!("state {st} drops item {item}, outside the item space"));
        }
        if reg.name_of(st) == "minecraft:air" {
            problems.push(format!("state {st} drops item {item} but has no name"));
        }
    }
    assert!(with_drop > 0, "the drop table must not be empty");
    assert!(
        problems.is_empty(),
        "{} drop problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// The anti-xray list is resolved with `filter_map`, so a name that does not resolve
/// disappears without a sound. That is the silent-hole shape the registry tables are
/// prone to: the ore simply stays visible and nothing says so. Requiring the set to
/// have one state per name turns a typo into a failing test.
#[test]
fn every_hidden_block_name_resolves_to_the_block_it_names() {
    let Some(reg) = breg() else {
        panic!("the block registry must load from assets/");
    };
    let mut unresolved = Vec::new();
    for name in HIDDEN_BLOCK_NAMES {
        match name_to_state(name) {
            Some(st) => {
                let got = reg.name_of(st).trim_start_matches("minecraft:");
                if got != *name {
                    unresolved.push(format!("{name} resolved to state {st}, named {got}"));
                }
            }
            None => unresolved.push(format!("{name} did not resolve to any block")),
        }
    }
    assert!(
        unresolved.is_empty(),
        "{} hidden-block names are wrong:\n{}",
        unresolved.len(),
        unresolved.join("\n")
    );
    assert_eq!(
        hidden_states().len(),
        HIDDEN_BLOCK_NAMES.len(),
        "a name was dropped while building the hidden set"
    );
}

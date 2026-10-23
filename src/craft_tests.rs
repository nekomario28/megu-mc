use super::*;

// The fixture pack `datapacks/rectest` ships `recipe/gemforge.json`: the shaped
// pattern ["##"] keyed on diamond_block, yielding 4 emeralds. Its bounding box
// is 1x2, so inside a 3x3 grid the pattern can sit at x=0 or x=1 - and vanilla
// slides a shaped pattern over the whole grid (`RecipeMatcher.findMatches`),
// so both placements have to craft. The left-aligned case alone is not enough
// to tell an offset search apart from a search that only ever tries x=0.

fn row_of(slots: &[(i16, i64)]) -> HashMap<i16, Stack> {
    slots
        .iter()
        .map(|(s, it)| (*s, (*it, 1, Comps::new())))
        .collect()
}

#[test]
fn a_shaped_recipe_matches_at_a_non_zero_grid_offset() {
    load_datapacks();
    let diamond_block = name_to_item("minecraft:diamond_block").expect("diamond_block");
    let emerald = name_to_item("minecraft:emerald").expect("emerald");
    // y = 0 of a 3x3 grid: the slot for column x is x + 1.
    let top_row = |xs: &[i16]| {
        row_of(
            &xs.iter()
                .map(|x| (x + 1, diamond_block))
                .collect::<Vec<_>>(),
        )
    };

    assert_eq!(
        json_craft_match(&top_row(&[0, 1]), 3).map(|s| (s.0, s.1)),
        Some((emerald, 4)),
        "the left-aligned pattern must craft"
    );
    assert_eq!(
        json_craft_match(&top_row(&[1, 2]), 3).map(|s| (s.0, s.1)),
        Some((emerald, 4)),
        "the same pattern shifted one column right must craft"
    );
}

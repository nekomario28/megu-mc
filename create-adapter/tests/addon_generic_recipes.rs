// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use create_adapter::addon_profile::{
    advance_assembly_step, discover_addon_processing_recipes, parse_sequenced_assembly_json,
    AddonProcessingRecipe, AssemblyOutcome, AssemblyStepKind, SequencedAssemblyState,
};

const RECIPE_ID: &str = "example_ball_factory:craft_orb";
const RECIPE_JSON: &str = r#"{
  "type": "example_ball_factory:sequenced_assembly",
  "ingredient": {"item": "example_ball_factory:raw_orb"},
  "transitionalItem": {"id": "example_ball_factory:unfinished_orb"},
  "loops": 2,
  "sequence": [
    {"type": "example_ball_factory:deploying", "ingredients": [
      {"item": "example_ball_factory:unfinished_orb"},
      {"item": "example_ball_factory:binding"}
    ]},
    {"type": "example_ball_factory:filling", "ingredients": [
      {"item": "example_ball_factory:unfinished_orb"},
      {"fluid": "example_ball_factory:liquid", "amount": 250}
    ]},
    {"type": "example_ball_factory:pressing"}
  ],
  "results": [
    {"id": "example_ball_factory:finished_orb", "count": 2, "chance": 0.75},
    {"item": "example_ball_factory:shards", "count": 1, "chance": 0.25}
  ]
}"#;

fn steps() -> [AssemblyStepKind; 3] {
    [
        AssemblyStepKind::Deploying {
            held_item: "example_ball_factory:binding".into(),
        },
        AssemblyStepKind::Filling {
            fluid_id: "example_ball_factory:liquid".into(),
            amount_mb: 250,
        },
        AssemblyStepKind::Pressing,
    ]
}

#[test]
fn two_loops_keep_transitional_item_until_deterministic_output() {
    let recipe = parse_sequenced_assembly_json(RECIPE_ID, RECIPE_JSON).unwrap();
    assert_eq!(recipe.id, RECIPE_ID);
    assert_eq!(recipe.loops, 2);
    assert_eq!(recipe.sequence, steps());
    assert_eq!(recipe.results.len(), 2);

    for (roll, expected_item, expected_count) in [
        (0.0, "example_ball_factory:finished_orb", 2),
        (0.749, "example_ball_factory:finished_orb", 2),
        (0.75, "example_ball_factory:shards", 1),
        (0.999, "example_ball_factory:shards", 1),
    ] {
        let mut state = SequencedAssemblyState::new(&recipe);
        assert_eq!(state.current_item, recipe.ingredient_item);
        for loop_index in 0..2 {
            for (step_index, step) in steps().iter().enumerate() {
                let outcome = advance_assembly_step(&mut state, &recipe, step, roll);
                if loop_index == 1 && step_index == 2 {
                    assert_eq!(
                        outcome,
                        AssemblyOutcome::Completed {
                            item: expected_item.into(),
                            count: expected_count,
                        }
                    );
                } else {
                    assert!(matches!(outcome, AssemblyOutcome::InProgress { .. }));
                    assert_eq!(state.current_item, recipe.transitional_item);
                }
            }
            if loop_index == 0 {
                assert_eq!(state.completed_loops, 1);
                assert_eq!(state.next_step, 0);
            }
        }
        assert!(state.finished);
        assert_eq!(state.completed_loops, 2);
        assert_eq!(state.current_item, expected_item);
        let completed = state.clone();
        assert!(matches!(
            advance_assembly_step(&mut state, &recipe, &steps()[0], roll),
            AssemblyOutcome::Rejected { .. }
        ));
        assert_eq!(state, completed);
    }
}

#[test]
fn wrong_step_and_wrong_resources_are_rejected_without_consuming_progress() {
    let recipe = parse_sequenced_assembly_json(RECIPE_ID, RECIPE_JSON).unwrap();
    let mut state = SequencedAssemblyState::new(&recipe);
    let initial = state.clone();
    for invalid in [
        AssemblyStepKind::Pressing,
        AssemblyStepKind::Deploying {
            held_item: "example_ball_factory:wrong_binding".into(),
        },
    ] {
        assert!(matches!(
            advance_assembly_step(&mut state, &recipe, &invalid, 0.5),
            AssemblyOutcome::Rejected { .. }
        ));
        assert_eq!(state, initial);
    }
    assert!(matches!(
        advance_assembly_step(&mut state, &recipe, &steps()[0], 0.5),
        AssemblyOutcome::InProgress { .. }
    ));
    let after_deploying = state.clone();
    for invalid in [
        AssemblyStepKind::Filling {
            fluid_id: "example_ball_factory:wrong_liquid".into(),
            amount_mb: 250,
        },
        AssemblyStepKind::Filling {
            fluid_id: "example_ball_factory:liquid".into(),
            amount_mb: 500,
        },
    ] {
        assert!(matches!(
            advance_assembly_step(&mut state, &recipe, &invalid, 0.5),
            AssemblyOutcome::Rejected { .. }
        ));
        assert_eq!(state, after_deploying);
    }
    for step in steps().iter().skip(1).chain(steps().iter()).take(5) {
        assert!(matches!(
            advance_assembly_step(&mut state, &recipe, step, 0.0),
            AssemblyOutcome::InProgress { .. } | AssemblyOutcome::Completed { .. }
        ));
    }
}

#[test]
fn invalid_final_roll_keeps_state_ready_for_retry() {
    let recipe = parse_sequenced_assembly_json(RECIPE_ID, RECIPE_JSON).unwrap();
    let mut state = SequencedAssemblyState::new(&recipe);
    for step in steps().iter().cycle().take(5) {
        advance_assembly_step(&mut state, &recipe, step, 0.0);
    }
    let before = state.clone();
    for roll in [f32::NAN, -0.01, 1.0, f32::INFINITY] {
        assert!(matches!(
            advance_assembly_step(&mut state, &recipe, &steps()[2], roll),
            AssemblyOutcome::Rejected { .. }
        ));
        assert_eq!(state, before);
    }
    assert!(matches!(
        advance_assembly_step(&mut state, &recipe, &steps()[2], 0.0),
        AssemblyOutcome::Completed { .. }
    ));
}

#[test]
fn cutting_custom_step_and_nested_item_stacks_remain_data_driven() {
    let json = r#"{
        "type":"custom_process:sequenced_assembly",
        "ingredient":{"item":{"id":"custom_process:raw"}},
        "transitional_item":{"item":"custom_process:incomplete"},
        "loops":1,
        "sequence":[
            {"type":"custom_process:cutting"},
            {"type":"custom_process:polishing"}
        ],
        "result":{"item":{"id":"custom_process:finished","count":3}}
    }"#;
    let recipe = parse_sequenced_assembly_json("custom_process:widget", json).unwrap();
    assert_eq!(recipe.sequence[0], AssemblyStepKind::Cutting);
    assert_eq!(
        recipe.sequence[1],
        AssemblyStepKind::CustomStep("custom_process:polishing".into())
    );
    assert_eq!(
        recipe.results,
        vec![("custom_process:finished".into(), 3, 1.0)]
    );
    let mut state = SequencedAssemblyState::new(&recipe);
    assert!(matches!(
        advance_assembly_step(&mut state, &recipe, &recipe.sequence[0], 0.1),
        AssemblyOutcome::InProgress { .. }
    ));
    assert_eq!(state.current_item, "custom_process:incomplete");
    assert_eq!(
        advance_assembly_step(&mut state, &recipe, &recipe.sequence[1], 0.1),
        AssemblyOutcome::Completed {
            item: "custom_process:finished".into(),
            count: 3,
        }
    );
}

#[test]
fn malformed_and_oversized_assembly_data_fails_closed() {
    let oversized_steps = vec![r#"{"type":"example_ball_factory:pressing"}"#; 33].join(",");
    let oversized_recipe = format!(
        r#"{{"type":"example_ball_factory:sequenced_assembly","ingredient":{{"item":"x:y"}},"transitionalItem":{{"id":"x:z"}},"loops":1,"sequence":[{oversized_steps}],"results":[{{"id":"x:q","count":1}}]}}"#
    );
    let malformed = [
        "{".to_string(),
        "[]".to_string(),
        RECIPE_JSON.replace("\"loops\": 2", "\"loops\": 0"),
        RECIPE_JSON.replace("\"loops\": 2", "\"loops\": 65"),
        RECIPE_JSON.replace("\"loops\": 2", "\"loops\": 1.5"),
        RECIPE_JSON.replace("\"count\": 2", "\"count\": 4294967296"),
        RECIPE_JSON.replace("\"amount\": 250", "\"amount\": 0"),
        RECIPE_JSON.replace("\"amount\": 250", "\"amount\": 4294967296"),
        RECIPE_JSON.replace("\"chance\": 0.75", "\"chance\": -1"),
        RECIPE_JSON.replace("\"chance\": 0.75", "\"chance\": 1e400"),
        RECIPE_JSON
            .replace("\"chance\": 0.75", "\"chance\": 0")
            .replace("\"chance\": 0.25", "\"chance\": 0"),
        RECIPE_JSON.replace(
            r#""type": "example_ball_factory:sequenced_assembly""#,
            r#""type": "other:pressing""#,
        ),
        oversized_recipe,
    ];
    for json in malformed {
        assert!(
            parse_sequenced_assembly_json(RECIPE_ID, &json).is_err(),
            "accepted invalid recipe: {json}"
        );
    }

    let empty_sequence = RECIPE_JSON.replace(
        r#""sequence": [
    {"type": "example_ball_factory:deploying", "ingredients": [
      {"item": "example_ball_factory:unfinished_orb"},
      {"item": "example_ball_factory:binding"}
    ]},
    {"type": "example_ball_factory:filling", "ingredients": [
      {"item": "example_ball_factory:unfinished_orb"},
      {"fluid": "example_ball_factory:liquid", "amount": 250}
    ]},
    {"type": "example_ball_factory:pressing"}
  ]"#,
        r#""sequence": []"#,
    );
    assert_ne!(empty_sequence, RECIPE_JSON);
    assert!(parse_sequenced_assembly_json(RECIPE_ID, &empty_sequence).is_err());

    let larger_item_count = RECIPE_JSON.replace("\"count\": 2", "\"count\": 65");
    let recipe = parse_sequenced_assembly_json(RECIPE_ID, &larger_item_count).unwrap();
    assert_eq!(recipe.results[0].1, 65);

    let max_item_count = RECIPE_JSON.replace("\"count\": 2", "\"count\": 4294967295");
    let recipe = parse_sequenced_assembly_json(RECIPE_ID, &max_item_count).unwrap();
    assert_eq!(recipe.results[0].1, u32::MAX);

    let max_fluid_amount = RECIPE_JSON.replace("\"amount\": 250", "\"amount\": 4294967295");
    let recipe = parse_sequenced_assembly_json(RECIPE_ID, &max_fluid_amount).unwrap();
    assert_eq!(
        recipe.sequence[1],
        AssemblyStepKind::Filling {
            fluid_id: "example_ball_factory:liquid".into(),
            amount_mb: u32::MAX,
        }
    );
}

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct TestDatapack(PathBuf);

impl TestDatapack {
    fn new() -> Self {
        let number = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("addon-recipes-{}-{number}", std::process::id()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, json: &str) {
        let destination = self.0.join(relative);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, json).unwrap();
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDatapack {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn discovers_unknown_namespace_assembly_pressing_and_spouting_in_both_layouts() {
    let root = TestDatapack::new();
    root.write(
        "data/example_ball_factory/recipe/craft_orb.json",
        RECIPE_JSON,
    );
    root.write(
        "data/example_ball_factory/recipes/pressing/flatten_orb.json",
        r#"{"type":"example_ball_factory:pressing","ingredients":[{"item":"example_ball_factory:raw_orb"}],"results":[{"id":"example_ball_factory:flattened_orb","count":1}]}"#,
    );
    root.write(
        "data/another_custom_namespace/recipe/filling/imbue_orb.json",
        r#"{"type":"another_custom_namespace:filling","ingredients":[{"item":"example_ball_factory:raw_orb"},{"fluid":"another_custom_namespace:essence","amount":125}],"results":[{"id":"example_ball_factory:imbued_orb","count":1}]}"#,
    );
    root.write(
        "data/example_ball_factory/recipes/ignored.json",
        r#"{"type":"example_ball_factory:unrelated","ingredients":[]}"#,
    );

    let discovered = discover_addon_processing_recipes(root.path()).unwrap();
    assert_eq!(discovered.len(), 3);
    assert!(matches!(
        &discovered[0],
        AddonProcessingRecipe::Filling {
            id,
            fluid_id,
            amount_mb: 125,
            ..
        } if id == "another_custom_namespace:filling/imbue_orb"
            && fluid_id == "another_custom_namespace:essence"
    ));
    assert!(matches!(
        &discovered[1],
        AddonProcessingRecipe::SequencedAssembly(recipe) if recipe.id == RECIPE_ID
    ));
    assert!(matches!(
        &discovered[2],
        AddonProcessingRecipe::Pressing {
            id,
            ingredient_item,
            ..
        } if id == "example_ball_factory:pressing/flatten_orb"
            && ingredient_item == "example_ball_factory:raw_orb"
    ));
}

#[test]
fn discovery_rejects_corrupted_recognized_recipe() {
    let root = TestDatapack::new();
    root.write(
        "data/example_ball_factory/recipe/craft_orb.json",
        RECIPE_JSON,
    );
    root.write(
        "data/example_ball_factory/recipes/pressing/broken.json",
        "{truncated",
    );
    assert!(discover_addon_processing_recipes(root.path()).is_err());
}

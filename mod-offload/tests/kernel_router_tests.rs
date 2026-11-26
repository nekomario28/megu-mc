use mod_offload::combat_motion::{ActionSpec, HitShape};
use mod_offload::recipe_processor::{Drive, Recipe, Resource, ResourceKind, Stack};
use mod_offload::{
    DeclarativeProfile, FeatureProfile, GraphDomain, ModFeatureDescriptor, ModKernelRouter,
    RouterError, RoutingBudget, UniversalKernelKind,
};

fn budget() -> RoutingBudget {
    RoutingBudget {
        max_features_per_mod: 32,
        max_total_routes: 128,
        max_nodes_per_tick: 32,
    }
}

fn feature(id: &str) -> ModFeatureDescriptor {
    ModFeatureDescriptor {
        feature_id: id.into(),
        has_network_ports: true,
        network_domain: Some(GraphDomain::Kinetic),
        processing_recipes: true,
        sleep_on_idle: true,
        subworld_assembly: true,
        resource_ports: true,
        track_node: true,
        action_timeline: true,
        unsupported_custom_hooks: vec![],
    }
}

#[test]
fn arbitrary_mods_route_structurally_to_all_six_kernels() {
    let mut router = ModKernelRouter::default();
    let profile =
        |id: &str, domain, process, sleep, subworld, ports, track, action| FeatureProfile {
            feature_id: id.into(),
            network_domain: domain,
            processing_recipes: process,
            sleep_on_idle: sleep,
            subworld_assembly: subworld,
            resource_ports: ports,
            track_node: track,
            action_timeline: action,
        };
    let cases = [
        ("acme_tech", vec![feature("industrial_unit")]),
        (
            "rotary_profile",
            DeclarativeProfile {
                features: vec![profile(
                    "shaft",
                    Some(GraphDomain::Kinetic),
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                )],
            }
            .lower(),
        ),
        (
            "hearth_kitchen",
            DeclarativeProfile {
                features: vec![profile("pot", None, true, true, false, true, false, false)],
            }
            .lower(),
        ),
        (
            "sky_vessels",
            DeclarativeProfile {
                features: vec![profile(
                    "ship", None, false, false, true, true, false, false,
                )],
            }
            .lower(),
        ),
        (
            "blade_arts",
            DeclarativeProfile {
                features: vec![profile(
                    "combo", None, false, false, false, false, false, true,
                )],
            }
            .lower(),
        ),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (mod_id, descriptors) in cases {
        let report = router
            .register_mod_ir(mod_id, descriptors, budget())
            .unwrap();
        assert!(report.unrouted_features.is_empty());
        for binding in report.routed {
            assert_eq!(router.binding(binding.handle), Some(&binding));
            assert!(router.handle(mod_id, &binding.feature_id).is_some());
            seen.insert(binding.target);
        }
    }
    assert_eq!(
        seen,
        [
            UniversalKernelKind::GraphNetwork(GraphDomain::Kinetic),
            UniversalKernelKind::DueProcessMachine,
            UniversalKernelKind::Subworld6Dof,
            UniversalKernelKind::ConservationTransfer,
            UniversalKernelKind::TrackGraph,
            UniversalKernelKind::TimelineAction,
        ]
        .into_iter()
        .collect()
    );

    assert!(router.attach_sleeping_transport("hearth_kitchen", 1, 2));
    assert!(router.insert_transport_item("hearth_kitchen", 1, 42));
    assert!(router.attach_action(
        "blade_arts",
        ActionSpec {
            action_id: 1,
            anticipation_ticks: 0,
            active_ticks: 1,
            recovery_ticks: 1,
            cancel_after_tick: None,
            invulnerable_window: None,
            movement_speed_factor: 1.0,
            hit_shape: HitShape::RadialAoe { radius: 1.0 }
        },
        0
    ));
    assert!(router.attach_kinetic_node("acme_tech", 1, Default::default()));
    let tick = router.tick_routed_kernels(2, budget());
    assert!(tick.woken > 0);
    assert!(tick.routed_nodes_ticked > 0);
}

#[test]
fn idle_transport_sleeps_and_wakes_on_its_deadline() {
    let mut router = ModKernelRouter::default();
    assert!(router.attach_sleeping_transport("anything", 9, 3));
    let idle = router.tick_routed_kernels(0, budget());
    assert_eq!(idle.woken, 0);
    assert_eq!(idle.sleeping, 1);
    assert!(router.insert_transport_item("anything", 9, 7));
    let before_due = router.tick_routed_kernels(2, budget());
    assert_eq!(before_due.woken, 0);
    let due = router.tick_routed_kernels(3, budget());
    assert_eq!(due.woken, 1);
}

#[test]
fn custom_hooks_are_reported_and_budgets_fail_without_mutation() {
    let mut router = ModKernelRouter::default();
    let mut bad = ModFeatureDescriptor::new("hooked");
    bad.processing_recipes = true;
    bad.unsupported_custom_hooks
        .push("dynamic bytecode callback".into());
    let report = router
        .register_mod_ir("unknown_mod", vec![bad], budget())
        .unwrap();
    assert_eq!(report.routed.len(), 0);
    assert!(report.unrouted_features[0]
        .reason
        .contains("dynamic bytecode callback"));

    let good = feature("stable");
    let registered = router
        .register_mod_ir("unknown_mod", vec![good.clone()], budget())
        .unwrap();
    let count = registered.routed.len();
    let tiny = RoutingBudget {
        max_features_per_mod: 0,
        ..budget()
    };
    assert_eq!(
        router.register_mod_ir("another_mod", vec![feature("overflow")], tiny),
        Err(RouterError::BudgetExceeded)
    );
    assert_eq!(
        router
            .register_mod_ir("unknown_mod", vec![good], budget())
            .unwrap()
            .routed
            .len(),
        count
    );
}

#[test]
fn descriptors_with_no_routes_are_visible_in_diagnostics() {
    let report = ModKernelRouter::default()
        .register_mod_ir("mod", vec![ModFeatureDescriptor::new("plain")], budget())
        .unwrap();
    assert_eq!(report.unrouted_features.len(), 1);
    assert_eq!(
        report.unrouted_features[0].reason,
        "no supported structural traits"
    );
}

#[test]
fn due_recipe_machine_runs_through_the_recipe_kernel() {
    let mut router = ModKernelRouter::default();
    let item = Resource {
        kind: ResourceKind::Item,
        id: 4,
    };
    let output = Resource {
        kind: ResourceKind::Item,
        id: 5,
    };
    router
        .attach_recipe_machine(
            "hearth",
            3,
            4,
            4,
            vec![Recipe {
                id: 8,
                base_ticks: 1,
                inputs: vec![Stack {
                    resource: item,
                    amount: 1,
                }],
                outputs: vec![Stack {
                    resource: output,
                    amount: 1,
                }],
            }],
            Drive::Mechanical {
                rpm: 16,
                overstressed: false,
            },
        )
        .unwrap();
    assert_eq!(
        router
            .insert_recipe_input(
                "hearth",
                3,
                Stack {
                    resource: item,
                    amount: 1
                }
            )
            .unwrap(),
        1
    );
    let stats = router.tick_routed_kernels(1, budget());
    assert_eq!(stats.woken, 1);
    assert_eq!(stats.routed_nodes_ticked, 1);
}

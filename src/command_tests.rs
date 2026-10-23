use super::*;

// `/tag <targets> list <tag>` is a console/RCON path: players may only run
// help / list / time query (`command_allowed_for_player`). Vanilla reports one
// line per matched target, so a loop that returns after the first target
// silently drops the rest of the selector's matches.

#[test]
fn tag_list_reports_every_matched_target() {
    let _world = world_guard();
    let (mut first, _hold_a) = crate::effect_tests::client_pair();
    let (mut second, _hold_b) = crate::effect_tests::client_pair();
    first.name = "tag-list-alpha".into();
    second.name = "tag-list-beta".into();
    // `client_pair` hands out one fixed entity id; the selector resolves
    // targets by it, so the two must differ.
    first.eid = 4242;
    second.eid = 4243;
    let (ea, eb) = (first.eid, second.eid);
    clients().lock().unwrap().push(Arc::new(first));
    clients().lock().unwrap().push(Arc::new(second));

    let ctx = CmdCtx {
        me: None,
        x: 0.5,
        y: 16.0,
        z: 0.5,
    };
    let added = run_command_ctx(&ctx, "/tag @a add repo_test_tag", &mut None);
    let listed = run_command_ctx(&ctx, "/tag @a list repo_test_tag", &mut None);

    // put the shared registries back the way they were
    clients()
        .lock()
        .unwrap()
        .retain(|c| c.eid != ea && c.eid != eb);
    {
        let mut tm = tags().lock().unwrap();
        for st in tm.values_mut() {
            st.remove("repo_test_tag");
        }
        tm.retain(|_, st| !st.is_empty());
    }

    assert!(added.contains('2'), "both targets must be tagged: {added}");
    assert!(
        listed.contains("tag-list-alpha"),
        "first target missing from {listed}"
    );
    assert!(
        listed.contains("tag-list-beta"),
        "second target missing from {listed}"
    );
}

use std::time::Duration;

use bevy::prelude::*;

use crate::assert_focused;
use crate::commands::{Command, MoveFocus, Operation, SpaceSelector};
use crate::ecs::FocusedMarker;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::native_spaces::SpaceMovePending;
use crate::ecs::native_tabs::NativeTabsDirty;
use crate::events::Event;
use crate::manager::Window;
use crate::platform::{WinID, WorkspaceId};

use super::*;

const SOURCE: WorkspaceId = TEST_WORKSPACE_ID;
const TARGET: WorkspaceId = TEST_WORKSPACE_ID + 1;

fn boot(count: i32) -> TestHarness {
    let mut harness = TestHarness::new()
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![SOURCE, TARGET],
        )
        .with_windows(count);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    harness
}

fn boot_unexposed() -> TestHarness {
    let mut harness = TestHarness::new().with_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![SOURCE, TARGET],
    );
    let frame = IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
    for id in [0, 1] {
        let _unexposed = harness
            .mock_state
            .spawn_window(TEST_PROCESS_ID, SOURCE, id, frame);
    }
    harness.mock_state.merge_native_tabs(&[0, 1], 0);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    harness
}

fn merge(harness: &mut TestHarness) {
    harness.mock_state.merge_native_tabs(&[0, 1], 0);
    harness.advance(NATIVE_REACTION);
}

fn move_tabs(harness: &mut TestHarness, focus: MoveFocus) {
    harness.run(vec![Event::Command {
        command: Command::Window(Operation::SpaceMove(SpaceSelector::Next, focus)),
    }]);
}

fn row(world: &mut World, id: WinID) -> Option<(WorkspaceId, u32)> {
    let entity = find_window_entity(id, world);
    world.query::<&LayoutStrip>().iter(world).find_map(|strip| {
        strip
            .contains(entity)
            .then_some((strip.id(), strip.virtual_index))
    })
}

fn grouped(world: &mut World, ids: &[WinID]) -> bool {
    let entities = ids
        .iter()
        .map(|id| find_window_entity(*id, world))
        .collect::<Vec<_>>();
    world.query::<&LayoutStrip>().iter(world).any(|strip| {
        strip.tab_group(entities[0]).is_some_and(|group| {
            group.len() == entities.len() && entities.iter().all(|entity| group.contains(entity))
        })
    })
}

fn pending(world: &mut World) -> bool {
    world
        .query::<&SpaceMovePending>()
        .iter(world)
        .next()
        .is_some()
}

fn pan_with_geometry_echoes(harness: &mut TestHarness, duration: Duration) {
    let until = harness.world().resource::<Time>().elapsed() + duration;
    while harness.world().resource::<Time>().elapsed() < until {
        let world = harness.world();
        world.write_message(Event::Scroll { delta: 0.1 });
        world.write_message(Event::WindowMoved { window_id: 0 });
        world.write_message(Event::WindowResized { window_id: 0 });
        harness.advance(Duration::from_millis(20));
    }
}

#[test]
fn geometry_echoes_defer_expensive_reads_then_reconcile_detach_with_one_census() {
    let mut harness = boot(10);
    merge(&mut harness);
    harness.advance(Duration::from_secs(1));
    let before = harness.mock_state.native_tab_read_counts();

    harness.mock_state.detach_native_tab(1);
    pan_with_geometry_echoes(&mut harness, Duration::from_millis(600));
    assert!(grouped(harness.world(), &[0, 1]));
    assert_eq!(
        harness.mock_state.native_tab_read_counts(),
        before,
        "geometry echoes must not rescan AXWindows, titlebars, or the global CG census"
    );

    harness.advance(NATIVE_REACTION);
    assert!(!grouped(harness.world(), &[0, 1]));
    for id in [0, 1] {
        assert_eq!(row(harness.world(), id), Some((SOURCE, 0)));
    }
    let (censuses, application_lists, _) = harness.mock_state.native_tab_read_counts();
    assert_eq!(
        censuses - before.0,
        1,
        "all ten windows share one fresh census"
    );
    assert_eq!(application_lists - before.1, 1);
    harness.advance(Duration::from_secs(1));
    assert!(!pump_awake(harness.world()));
}

#[test]
fn selecting_an_unexposed_tab_during_swipe_discovers_and_focuses_it_promptly() {
    let mut harness = boot_unexposed();
    pan_with_geometry_echoes(&mut harness, NATIVE_REACTION);
    harness.mock_state.focus_window(1);
    pan_with_geometry_echoes(&mut harness, Duration::from_millis(80));

    assert_focused!(harness.world(), 1);
    assert!(grouped(harness.world(), &[0, 1]));
    assert!(harness.mock_state.window_memberships(0).is_empty());
    let world = harness.world();
    assert!(
        world
            .query::<&crate::ecs::Scrolling>()
            .iter(world)
            .any(|scrolling| scrolling.is_user_swiping)
    );
}

#[test]
fn move_echoes_do_not_starve_an_unsettled_semantic_observation() {
    let mut harness = boot(3);
    harness.mock_state.set_tab_queries_failing(0, true);
    harness.mock_state.merge_native_tabs(&[0, 1], 0);
    pan_with_geometry_echoes(&mut harness, NATIVE_REACTION);
    assert!(!grouped(harness.world(), &[0, 1]));

    // The focus notification has already been consumed. Only its pending
    // retry can learn that Cocoa settled before geometry notifications stop.
    harness.mock_state.set_tab_queries_failing(0, false);
    pan_with_geometry_echoes(&mut harness, Duration::from_millis(80));
    assert!(grouped(harness.world(), &[0, 1]));
    assert_focused!(harness.world(), 0);
    assert!(harness.mock_state.window_memberships(1).is_empty());
}

#[test]
fn geometry_echoes_do_not_extend_expired_semantic_retries() {
    let mut harness = boot(3);
    harness.mock_state.set_tab_queries_failing(0, true);
    harness.mock_state.merge_native_tabs(&[0, 1], 0);
    pan_with_geometry_echoes(&mut harness, NATIVE_DEADLINE);
    let after_deadline = harness.mock_state.native_tab_read_counts();

    harness.mock_state.set_tab_queries_failing(0, false);
    pan_with_geometry_echoes(&mut harness, NATIVE_REACTION);
    assert!(!grouped(harness.world(), &[0, 1]));
    assert_eq!(
        harness.mock_state.native_tab_read_counts(),
        after_deadline,
        "move echoes cannot keep an expired semantic retry scanning indefinitely"
    );

    harness.advance(NATIVE_REACTION);
    assert!(grouped(harness.world(), &[0, 1]));
    assert!(harness.mock_state.window_memberships(1).is_empty());
}

#[test]
fn semantic_success_preserves_a_pending_geometry_settle() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness.advance(Duration::from_secs(1));
    let world = harness.world();
    world.write_message(Event::WindowTitleChanged { window_id: 0 });
    world.write_message(Event::WindowMoved { window_id: 0 });
    harness.advance(Duration::from_millis(20));
    assert!(grouped(harness.world(), &[0, 1]));

    // The earlier geometry notification preceded Cocoa's titlebar update.
    // There is no second notification when the metadata catches up.
    harness.mock_state.detach_native_tab(1);
    harness.mock_state.drain_events();
    harness.advance(NATIVE_REACTION);
    assert!(!grouped(harness.world(), &[0, 1]));
    assert_eq!(row(harness.world(), 1), Some((SOURCE, 0)));
}

#[test]
fn late_merge_uses_titlebar_identity_not_desired_geometry() {
    let mut harness = boot(3);
    assert!(!grouped(harness.world(), &[0, 1]));
    // The layout already assigned separate slots; Cocoa merges after startup.
    merge(&mut harness);
    assert!(grouped(harness.world(), &[0, 1]));
    assert!(!grouped(harness.world(), &[0, 1, 2]));
    assert_eq!(harness.mock_state.window_memberships(0), vec![SOURCE]);
    assert!(harness.mock_state.window_memberships(1).is_empty());
    assert_focused!(harness.world(), 0);
}

#[test]
fn raising_the_strip_never_exposes_an_inactive_tab_during_selection_transfer() {
    let mut harness = boot(3);
    merge(&mut harness);
    let ordinary = find_window_entity(2, harness.world());
    harness.world().trigger(crate::ecs::RaiseWindow {
        entity: ordinary,
        with_strip: true,
    });
    assert!(harness.mock_state.window_memberships(1).is_empty());

    // The OS selected tab 1, but the cached titlebar still selects tab 0.
    harness.mock_state.focus_window(1);
    let selected = find_window_entity(1, harness.world());
    harness.world().trigger(crate::ecs::RaiseWindow {
        entity: selected,
        with_strip: true,
    });
    assert!(harness.mock_state.window_memberships(0).is_empty());
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 1);
    assert!(grouped(harness.world(), &[0, 1]));
}

#[test]
fn delayed_selection_metadata_never_reorders_the_old_tab_and_replays_selected_geometry() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness.mock_state.focus_window(1);
    harness.mock_state.set_tab_queries_failing(1, true);
    let old = find_window_entity(0, harness.world());
    harness
        .world()
        .get_mut::<crate::ecs::Position>(old)
        .unwrap()
        .0
        .x += 80;
    harness
        .world()
        .get_mut::<crate::ecs::Bounds>(old)
        .unwrap()
        .0
        .x += 120;
    harness.advance(NATIVE_REACTION);
    assert!(harness.mock_state.window_memberships(0).is_empty());

    harness.mock_state.set_tab_queries_failing(1, false);
    harness.advance(NATIVE_REACTION);
    assert!(harness.mock_state.window_memberships(0).is_empty());
    assert_focused!(harness.world(), 1);
    assert!(grouped(harness.world(), &[0, 1]));
    let selected = find_window_entity(1, harness.world());
    let world = harness.world();
    assert_eq!(
        world.get::<Window>(selected).unwrap().frame().size(),
        world.get::<crate::ecs::Bounds>(selected).unwrap().0
    );
}

#[test]
fn complete_observations_release_the_idle_pump_before_the_retry_deadline() {
    let mut harness = boot(3);
    harness.advance(Duration::from_secs(1));
    assert!(
        !pump_awake(harness.world()),
        "ordinary windows have no tab work"
    );

    merge(&mut harness);
    harness.advance(Duration::from_secs(1));
    assert!(grouped(harness.world(), &[0, 1]));
    assert!(
        !pump_awake(harness.world()),
        "the observed merge is complete"
    );

    move_tabs(&mut harness, MoveFocus::Stay);
    harness.advance(Duration::from_secs(1));
    assert!(grouped(harness.world(), &[0, 1]));
    assert_eq!(row(harness.world(), 1), Some((TARGET, 0)));
    assert!(
        !pump_awake(harness.world()),
        "the native tab move is complete"
    );
}

#[test]
fn unreadable_merge_retries_until_identity_settles_then_allows_idle() {
    let mut harness = boot(3);
    harness.mock_state.set_tab_queries_failing(0, true);
    merge(&mut harness);
    assert!(!grouped(harness.world(), &[0, 1]));
    assert!(pump_awake(harness.world()));

    // Cocoa settles without sending the resize/focus notification again.
    harness.mock_state.set_tab_queries_failing(0, false);
    harness.advance(NATIVE_REACTION);
    assert!(grouped(harness.world(), &[0, 1]));
    harness.advance(Duration::from_secs(1));
    assert!(!pump_awake(harness.world()));
}

#[test]
fn ordinary_overlap_and_empty_membership_do_not_establish_a_tab_group() {
    let mut harness = boot(3);
    let frame = IRect::new(100, 100, 700, 800);
    harness
        .mock_state
        .update_window(0, |window| window.frame = frame);
    harness
        .mock_state
        .update_window(1, |window| window.frame = frame);
    harness.mock_state.window_visible(1, false);
    harness.mock_state.set_window_memberships(1, vec![]);
    harness.run(vec![Event::WindowResized { window_id: 0 }]);
    assert!(!grouped(harness.world(), &[0, 1]));
    assert_eq!(row(harness.world(), 1), Some((SOURCE, 0)));
}

#[test]
fn independently_present_first_time_title_matches_do_not_authorize_a_group() {
    let mut harness = boot(3);
    harness.mock_state.merge_native_tabs(&[0, 1], 0);
    harness.mock_state.set_window_unordered(1, false);
    harness.advance(NATIVE_REACTION);
    assert!(!grouped(harness.world(), &[0, 1]));
    move_tabs(&mut harness, MoveFocus::Follow);
    assert!(harness.mock_state.workspace_moves().is_empty());
}

#[test]
fn established_native_identity_survives_ordered_in_inactive_tabs() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness.mock_state.focus_window(1);
    harness.mock_state.set_window_unordered(0, false);
    harness.advance(NATIVE_REACTION);
    assert!(grouped(harness.world(), &[0, 1]));
    assert_focused!(harness.world(), 1);
    assert_eq!(harness.mock_state.window_memberships(0), vec![SOURCE]);
    move_tabs(&mut harness, MoveFocus::Follow);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), TARGET);
    for id in [0, 1] {
        assert_eq!(harness.mock_state.window_memberships(id), vec![TARGET]);
        assert_eq!(row(harness.world(), id), Some((TARGET, 0)));
    }
    assert_focused!(harness.world(), 1);
}

#[test]
fn duplicate_titles_fail_closed_before_native_submission() {
    let mut harness = boot(3);
    harness
        .mock_state
        .update_window(2, |window| window.title = "Window 1".into());
    merge(&mut harness);
    assert!(!grouped(harness.world(), &[0, 1]));
    move_tabs(&mut harness, MoveFocus::Follow);
    assert!(harness.mock_state.workspace_moves().is_empty());
    assert_eq!(harness.mock_state.window_memberships(0), vec![SOURCE]);
    assert_eq!(row(harness.world(), 1), Some((SOURCE, 0)));
}

#[test]
fn unreadable_inactive_ax_preserves_cached_identity_and_allows_send() {
    let mut harness = boot(3);
    // The live window becomes AX-inaccessible only after its title was known.
    harness.mock_state.set_title_queries_failing(1, true);
    harness.mock_state.set_tab_queries_failing(1, true);
    merge(&mut harness);
    assert!(grouped(harness.world(), &[0, 1]));
    move_tabs(&mut harness, MoveFocus::Stay);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0, 1], TARGET)]
    );
    for id in [0, 1] {
        assert_eq!(row(harness.world(), id), Some((TARGET, 0)));
    }
    assert_eq!(harness.mock_state.window_memberships(1), vec![TARGET]);
    assert!(!pending(harness.world()));
    assert_focused!(harness.world(), 2);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SOURCE);
}

#[test]
fn unreadable_invalidated_title_is_not_reclassified_as_an_unknown_tab() {
    let mut harness = boot(3);
    harness.mock_state.set_title_queries_failing(1, true);
    harness.run(vec![Event::WindowTitleChanged { window_id: 1 }]);
    merge(&mut harness);
    assert!(!grouped(harness.world(), &[0, 1]));
    move_tabs(&mut harness, MoveFocus::Follow);
    assert!(harness.mock_state.workspace_moves().is_empty());
    assert_eq!(row(harness.world(), 0), Some((SOURCE, 0)));
    assert_eq!(row(harness.world(), 1), Some((SOURCE, 0)));
}

#[test]
fn late_detach_returns_each_live_member_to_its_own_slot() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness.mock_state.detach_native_tab(1);
    harness.advance(NATIVE_REACTION);
    assert!(!grouped(harness.world(), &[0, 1]));
    assert_eq!(harness.mock_state.window_memberships(0), vec![SOURCE]);
    assert_eq!(harness.mock_state.window_memberships(1), vec![SOURCE]);
    for id in [0, 1] {
        assert_eq!(row(harness.world(), id), Some((SOURCE, 0)));
    }
}

#[test]
fn detached_membership_settles_without_another_notification() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness.mock_state.detach_native_tab(1);
    harness.mock_state.set_window_memberships(1, vec![]);
    harness.advance(NATIVE_REACTION);
    assert_eq!(row(harness.world(), 1), Some((SOURCE, 0)));
    assert!(pump_awake(harness.world()));

    harness
        .mock_state
        .update_window(1, |window| window.workspace_id = TARGET);
    harness.mock_state.set_window_memberships(1, vec![TARGET]);
    harness.advance(NATIVE_REACTION);
    assert_eq!(row(harness.world(), 1), Some((TARGET, 0)));
    assert!(!grouped(harness.world(), &[0, 1]));
    harness.advance(Duration::from_secs(1));
    assert!(!pump_awake(harness.world()));
}

#[test]
fn spacesend_moves_visible_representative_and_all_logical_tabs() {
    let mut harness = boot(3);
    merge(&mut harness);
    move_tabs(&mut harness, MoveFocus::Stay);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0, 1], TARGET)]
    );
    assert_eq!(harness.mock_state.window_memberships(1), vec![TARGET]);
    for id in [0, 1] {
        assert_eq!(row(harness.world(), id), Some((TARGET, 0)));
    }
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SOURCE);
    assert_focused!(harness.world(), 2);
    assert!(!pending(harness.world()));

    user_switches_native_space(&mut harness, TARGET);
    for selected in [1, 0, 1] {
        harness.mock_state.focus_window(selected);
        harness.advance(NATIVE_REACTION);
        assert_eq!(
            harness.mock_state.window_memberships(selected),
            vec![TARGET]
        );
        assert!(
            harness
                .mock_state
                .window_memberships(1 - selected)
                .is_empty()
        );
        assert_focused!(harness.world(), selected);
        assert!(grouped(harness.world(), &[0, 1]));
        for id in [0, 1] {
            assert_eq!(row(harness.world(), id), Some((TARGET, 0)));
        }
    }
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

#[test]
fn partial_tab_assignment_never_confirms_or_follows_the_selected_subset() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    move_tabs(&mut harness, MoveFocus::Follow);
    harness
        .mock_state
        .update_window(0, |window| window.workspace_id = TARGET);
    harness.advance(NATIVE_DEADLINE);
    assert!(!pending(harness.world()));
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SOURCE);
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_memberships(1), vec![SOURCE]);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

#[test]
fn selection_transfer_while_native_move_pending_confirms_current_representative() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    move_tabs(&mut harness, MoveFocus::Follow);
    assert!(pending(harness.world()));
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert!(harness.mock_state.window_memberships(0).is_empty());
    assert_eq!(harness.mock_state.window_memberships(1), vec![SOURCE]);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert!(!pending(harness.world()));
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), TARGET);
    assert_focused!(harness.world(), 1);
    for selected in [0, 1] {
        harness.mock_state.focus_window(selected);
        harness.advance(NATIVE_REACTION);
        assert_focused!(harness.world(), selected);
        assert_eq!(
            harness.mock_state.window_memberships(selected),
            vec![TARGET]
        );
        assert!(
            harness
                .mock_state
                .window_memberships(1 - selected)
                .is_empty()
        );
        for id in [0, 1] {
            assert_eq!(row(harness.world(), id), Some((TARGET, 0)));
        }
    }
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0, 1], TARGET)]
    );
}

#[test]
fn unreadable_titlebar_after_submission_never_activates_or_retries_old_move() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    move_tabs(&mut harness, MoveFocus::Follow);
    harness.mock_state.set_tab_queries_failing(0, true);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert!(!pending(harness.world()));
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(row(harness.world(), 1), Some((TARGET, 0)));
    let focused = harness
        .world()
        .query_filtered::<&Window, With<FocusedMarker>>()
        .iter(harness.world())
        .map(|window| window.id())
        .collect::<Vec<_>>();
    assert!(!focused.contains(&0) && !focused.contains(&1));
    harness.advance(NATIVE_DEADLINE);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert!(
        harness
            .world()
            .query::<&NativeTabsDirty>()
            .iter(harness.world())
            .next()
            .is_none()
    );

    harness.mock_state.set_tab_queries_failing(0, false);
    harness.run(vec![Event::WindowResized { window_id: 0 }]);
    for id in [0, 1] {
        assert_eq!(row(harness.world(), id), Some((TARGET, 0)));
    }
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

#[test]
fn independently_moved_child_requires_its_own_nonempty_target_membership() {
    let mut harness = boot(4);
    merge(&mut harness);
    harness.mock_state.associate_window(0, 3);
    harness.advance(NATIVE_REACTION);
    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    move_tabs(&mut harness, MoveFocus::Follow);
    for id in [0, 1] {
        harness
            .mock_state
            .update_window(id, |window| window.workspace_id = TARGET);
    }
    harness.mock_state.set_window_memberships(3, vec![]);
    harness.advance(NATIVE_DEADLINE);
    assert!(!pending(harness.world()));
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0, 1, 3], TARGET)]
    );
    for id in [0, 1] {
        assert_eq!(row(harness.world(), id), Some((TARGET, 0)));
    }
    assert!(!grouped(harness.world(), &[0, 1, 3]));
    assert_eq!(row(harness.world(), 3), Some((SOURCE, 0)));
}

#[test]
fn closing_selected_tab_during_move_preserves_bounded_survivor_reconciliation() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    move_tabs(&mut harness, MoveFocus::Follow);
    harness.mock_state.settle_native_requests();
    harness.mock_state.os_close_window(0);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_memberships(1), vec![TARGET]);
    assert_eq!(row(harness.world(), 1), Some((TARGET, 0)));
    assert!(!pending(harness.world()));
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

#[test]
fn closing_a_native_tab_in_tabbed_display_preserves_cocoa_selection() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness.mock_state.focus_window(2);
    harness.advance(NATIVE_REACTION);
    harness.run(vec![
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
    ]);
    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 0);

    // Cocoa selects the surviving native tab before its titlebar can be read.
    harness.mock_state.set_tab_queries_failing(1, true);
    harness.mock_state.os_close_window(0);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 1);

    harness.mock_state.set_tab_queries_failing(1, false);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 1);
}

#[test]
fn unresolved_startup_tabs_refuse_move_until_each_real_window_is_discovered() {
    let mut harness = boot_unexposed();
    move_tabs(&mut harness, MoveFocus::Stay);
    assert!(harness.mock_state.workspace_moves().is_empty());
    assert_eq!(row(harness.world(), 0), Some((SOURCE, 0)));
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SOURCE);
    assert!(!pending(harness.world()));

    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert!(grouped(harness.world(), &[0, 1]));
    move_tabs(&mut harness, MoveFocus::Follow);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0, 1], TARGET)]
    );
    for selected in [0, 1] {
        harness.mock_state.focus_window(selected);
        harness.advance(NATIVE_REACTION);
        assert_focused!(harness.world(), selected);
        assert_eq!(
            harness.mock_state.window_memberships(selected),
            vec![TARGET]
        );
        for id in [0, 1] {
            assert_eq!(row(harness.world(), id), Some((TARGET, 0)));
        }
    }
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), TARGET);
}

#[test]
fn unreadable_exposed_tab_keeps_discovery_pending_without_a_second_focus_event() {
    let mut harness = boot_unexposed();
    harness.mock_state.focus_window(1);
    // The original focus discovery already missed the AXWindow; only the
    // application's existing incomplete tab observation is left to retry it.
    harness.mock_state.drain_events();
    harness.mock_state.set_tab_queries_failing(1, true);
    harness.advance(NATIVE_REACTION);
    assert!(
        harness
            .world()
            .query::<&Window>()
            .iter(harness.world())
            .all(|window| window.id() != 1)
    );
    assert!(pump_awake(harness.world()));

    harness.mock_state.set_tab_queries_failing(1, false);
    harness.advance(NATIVE_REACTION);
    assert!(grouped(harness.world(), &[0, 1]));
    harness.advance(Duration::from_secs(1));
    assert!(!pump_awake(harness.world()));
}

#[test]
fn duplicate_untracked_startup_title_refuses_native_submission() {
    let mut harness = boot_unexposed();
    harness
        .mock_state
        .update_window(1, |window| window.title = "Window 0".into());
    move_tabs(&mut harness, MoveFocus::Follow);
    assert!(harness.mock_state.workspace_moves().is_empty());
    assert_eq!(row(harness.world(), 0), Some((SOURCE, 0)));
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SOURCE);
}

#[test]
fn changed_native_tab_title_during_move_never_confirms_the_known_subset() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    move_tabs(&mut harness, MoveFocus::Follow);
    assert!(pending(harness.world()));
    harness
        .mock_state
        .update_window(1, |window| window.title = "Different startup tab".into());
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert!(!pending(harness.world()));
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SOURCE);
    harness.advance(NATIVE_DEADLINE);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert!(harness.mock_state.native_space_focuses().is_empty());
}

#[test]
fn detaching_during_move_restores_independent_confirmation_without_resubmission() {
    let mut harness = boot(3);
    merge(&mut harness);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    move_tabs(&mut harness, MoveFocus::Follow);
    harness.mock_state.detach_native_tab(1);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_DEADLINE);
    assert_eq!(harness.mock_state.window_memberships(0), vec![TARGET]);
    assert_eq!(harness.mock_state.window_memberships(1), vec![TARGET]);
    assert_eq!(row(harness.world(), 0), Some((TARGET, 0)));
    assert_eq!(row(harness.world(), 1), Some((TARGET, 0)));
    assert!(!grouped(harness.world(), &[0, 1]));
    assert!(!pending(harness.world()));
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0, 1], TARGET)]
    );
}

#[test]
fn selection_after_send_keeps_the_destination_virtual_row() {
    let mut harness = boot(3);
    // The destination's selected row is not its row zero.
    {
        let world = harness.world();
        for mut strip in world.query::<&mut LayoutStrip>().iter_mut(world) {
            if strip.id() == TARGET {
                strip.virtual_index = 2;
            }
        }
    }
    merge(&mut harness);
    move_tabs(&mut harness, MoveFocus::Stay);
    user_switches_native_space(&mut harness, TARGET);
    for selected in [1, 0] {
        harness.mock_state.focus_window(selected);
        harness.advance(NATIVE_REACTION);
        for id in [0, 1] {
            assert_eq!(row(harness.world(), id), Some((TARGET, 2)));
        }
        assert_focused!(harness.world(), selected);
        assert_eq!(
            harness.mock_state.window_memberships(selected),
            vec![TARGET]
        );
    }
}

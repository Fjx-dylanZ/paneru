use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::commands::{Command, Direction, MoveFocus, Operation, SpaceOperation, SpaceSelector};
use crate::config::{Config, MainOptions, WindowParams, parse_command};
use crate::ecs::display::FloatingLayer;
use crate::ecs::workspace::FollowSpacePending;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, FocusedMarker, FollowCurrentWorkspaceMarker,
    Initializing, InstantSpaceSwitch, ManualStripOffset, MissionControlActive,
    NativeFullscreenMarker, Position, Unmanaged, layout::LayoutStrip,
};
use crate::ecs::{RepositionMarker, SpawnWindowTrigger};
use crate::events::Event;
use crate::manager::{Display, Origin, Size, Window};
use crate::platform::{Modifiers, WinID};
use crate::{assert_focused, assert_window_at, assert_window_size};

use super::*;

#[test]
fn native_fullscreen_transition_removes_window_from_original_strip_without_focus_marker() {
    const FULLSCREEN_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 100;

    TestHarness::new()
        .with_windows(2)
        .on_iteration(0, |world, state| {
            let focused = world
                .query_filtered::<Entity, With<FocusedMarker>>()
                .iter(world)
                .collect::<Vec<_>>();
            for entity in focused {
                world.entity_mut(entity).remove::<FocusedMarker>();
            }

            state.update_window(0, |window| {
                window.workspace_id = FULLSCREEN_WORKSPACE_ID;
                window.is_full_screen = true;
            });
            state.activate_workspace(TEST_DISPLAY_ID, FULLSCREEN_WORKSPACE_ID, true);
        })
        .on_iteration(1, |world, _state| {
            let fullscreen_window = find_window_entity(0, world);
            let sibling_window = find_window_entity(1, world);
            let mut strips = world.query::<(&LayoutStrip, Option<&NativeFullscreenMarker>)>();

            let original_strip = strips
                .iter(world)
                .find_map(|(strip, marker)| {
                    (strip.id() == TEST_WORKSPACE_ID && marker.is_none()).then_some(strip)
                })
                .expect("original strip");
            assert!(
                !original_strip.contains(fullscreen_window),
                "fullscreen window must not leave a reserved column in the original strip"
            );
            assert!(original_strip.contains(sibling_window));

            let (fullscreen_strip, fullscreen_marker) = strips
                .iter(world)
                .find(|(strip, _)| strip.id() == FULLSCREEN_WORKSPACE_ID)
                .expect("fullscreen strip");
            assert!(fullscreen_strip.contains(fullscreen_window));
            assert!(fullscreen_marker.is_some());
        })
        .on_iteration(2, |world, _state| {
            let fullscreen_window = find_window_entity(0, world);
            let sibling_window = find_window_entity(1, world);
            let mut strips = world.query::<&LayoutStrip>();

            let original_strip = strips
                .iter(world)
                .find(|strip| strip.id() == TEST_WORKSPACE_ID)
                .expect("original strip");
            assert!(original_strip.contains(fullscreen_window));
            assert!(original_strip.contains(sibling_window));
            assert_eq!(
                original_strip
                    .index_of(fullscreen_window)
                    .expect("restored fullscreen window index"),
                0
            );
            assert!(
                strips
                    .iter(world)
                    .all(|strip| strip.id() != FULLSCREEN_WORKSPACE_ID)
            );
        })
        .run(vec![
            Event::Command {
                command: Command::PrintState,
            },
            Event::SpaceChanged,
            Event::SpaceDestroyed {
                space_id: FULLSCREEN_WORKSPACE_ID,
            },
        ]);
}

#[test]
fn frontmost_floating_window_is_focused_after_setup() {
    let mut params = WindowParams::new(".*", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, |world, _state| {
            assert_focused!(world, 0);
            let entity = find_window_entity(0, world);
            assert!(world.entity(entity).contains::<Unmanaged>());
        })
        .run(vec![Event::MenuOpened { window_id: 0 }]);
}

/// Regression: a floating window placed by a grid rule must land at the active
/// display's usable origin (menubar + padding offset), not at (0, 0). Dropping
/// the display bounds origin previously sent grid windows to the primary
/// display's top-left corner (and onto the wrong display in multi-display
/// setups).
#[test]
fn floating_grid_window_uses_active_display_usable_origin() {
    let options = MainOptions {
        padding_left: Some(40),
        padding_top: Some(15),
        ..MainOptions::default()
    };

    let mut params = WindowParams::new(".*", None);
    params.floating = Some(true);
    // Cell (0,0) spanning the full 1x1 grid: origin should equal the usable
    // top-left, independent of the display size.
    params.grid = Some("1:1:0:0:1:1".to_string());
    let config: Config = (options, vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .on_iteration(1, |world, state| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let frame = IRect::from_corners(origin, origin + size);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 0, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(3, |world, _state| {
            // usable origin = (pad_left, menubar + pad_top) = (40, 20 + 15).
            assert_window_at!(world, 0, 40, TEST_MENUBAR_HEIGHT + 15);
        })
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn follow_command_preserves_frame_and_leaves_window_floating_when_disabled() {
    let original_frame = Rc::new(Cell::new(IRect::default()));
    let captured = original_frame.clone();
    let enabled_frame = original_frame.clone();
    let disabled_frame = original_frame.clone();

    TestHarness::new()
        .with_windows(1)
        .with_focused_window(0)
        .on_iteration(0, move |world, _state| {
            captured.set(window_frame(world, 0));
        })
        .on_iteration(1, move |world, _state| {
            let entity = find_window_entity(0, world);
            assert!(
                world
                    .entity(entity)
                    .contains::<FollowCurrentWorkspaceMarker>()
            );
            assert!(matches!(
                world.get::<Unmanaged>(entity),
                Some(Unmanaged::Floating)
            ));
            assert_eq!(window_frame(world, 0), enabled_frame.get());
        })
        .on_iteration(2, move |world, _state| {
            let entity = find_window_entity(0, world);
            assert!(
                !world
                    .entity(entity)
                    .contains::<FollowCurrentWorkspaceMarker>()
            );
            assert!(matches!(
                world.get::<Unmanaged>(entity),
                Some(Unmanaged::Floating)
            ));
            assert_eq!(window_frame(world, 0), disabled_frame.get());
        })
        .run(vec![
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::Window(Operation::Follow(Some(true))),
            },
            Event::Command {
                command: Command::Window(Operation::Follow(Some(false))),
            },
        ]);
}

#[test]
fn configured_follower_moves_to_the_active_native_workspace() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut params = WindowParams::new(".*", None);
    params.follow = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();

    TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .on_iteration(0, |world, state| {
            let entity = find_window_entity(0, world);
            assert!(
                world
                    .entity(entity)
                    .contains::<FollowCurrentWorkspaceMarker>()
            );
            assert!(matches!(
                world.get::<Unmanaged>(entity),
                Some(Unmanaged::Floating)
            ));
            assert!(state.workspace_moves().is_empty());
            state.activate_workspace(TEST_DISPLAY_ID, NEXT_WORKSPACE_ID, false);
        })
        .on_iteration(1, |_world, state| {
            assert_eq!(state.window_workspace(0), NEXT_WORKSPACE_ID);
            assert_eq!(state.workspace_moves(), vec![(vec![0], NEXT_WORKSPACE_ID)]);
        })
        .run(vec![
            Event::Command {
                command: Command::PrintState,
            },
            Event::SpaceChanged,
        ]);
}

#[test]
fn configured_focus_skips_native_space_animation_once() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let config: Config = (
        MainOptions {
            skip_native_space_switch_animation: Some(true),
            ..default()
        },
        vec![],
    )
        .into();
    let harness = TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID],
        )
        .with_windows(1);
    harness
        .mock_state
        .activate_workspace(TEST_DISPLAY_ID, NEXT_WORKSPACE_ID, false);

    harness
        .on_iteration(0, |_world, state| state.focus_window(0))
        .on_iteration(1, |world, state| {
            assert_eq!(state.active_workspace(TEST_DISPLAY_ID), TEST_WORKSPACE_ID);
            assert_eq!(state.workspace_focuses(), vec![0]);
            let now = world.resource::<Time>().elapsed();
            assert!(
                world
                    .resource::<InstantSpaceSwitch>()
                    .suppress_focus_follows_mouse(now),
                "focus-follows-mouse remains suppressed after SpaceChanged"
            );
        })
        .run(vec![
            Event::Command {
                command: Command::PrintState,
            },
            Event::Command {
                command: Command::PrintState,
            },
        ]);
}

#[test]
fn native_space_commands_use_global_non_wrapping_order() {
    const SECOND_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;
    const THIRD_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 2;
    const OTHER_DISPLAY_ID: u32 = TEST_DISPLAY_ID + 1;
    const OTHER_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 10;
    const OTHER_LAST_WORKSPACE_ID: WorkspaceId = OTHER_WORKSPACE_ID + 1;

    TestHarness::new()
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, SECOND_WORKSPACE_ID, THIRD_WORKSPACE_ID],
        )
        .with_display(
            OTHER_DISPLAY_ID,
            IRect::new(
                TEST_DISPLAY_WIDTH,
                0,
                TEST_DISPLAY_WIDTH * 2,
                TEST_DISPLAY_HEIGHT,
            ),
            vec![OTHER_WORKSPACE_ID, OTHER_LAST_WORKSPACE_ID],
        )
        .on_iteration(0, |_world, state| {
            assert_eq!(state.active_display(), OTHER_DISPLAY_ID);
            assert_eq!(
                state.native_space_focuses(),
                vec![OTHER_WORKSPACE_ID],
                "number 4 focuses a Space already visible on another display"
            );
        })
        .on_iteration(1, |_world, state| {
            assert_eq!(
                state.native_space_focuses(),
                vec![OTHER_WORKSPACE_ID, OTHER_LAST_WORKSPACE_ID]
            );
        })
        .on_iteration(2, |_world, state| {
            assert_eq!(
                state.native_space_focuses(),
                vec![OTHER_WORKSPACE_ID, OTHER_LAST_WORKSPACE_ID],
                "next does not wrap past the last global Space"
            );
        })
        .on_iteration(3, |_world, state| {
            assert_eq!(
                state.native_space_focuses(),
                vec![
                    OTHER_WORKSPACE_ID,
                    OTHER_LAST_WORKSPACE_ID,
                    OTHER_WORKSPACE_ID
                ]
            );
        })
        .on_iteration(4, |_world, state| {
            assert_eq!(state.active_display(), TEST_DISPLAY_ID);
            assert_eq!(
                state.native_space_focuses(),
                vec![
                    OTHER_WORKSPACE_ID,
                    OTHER_LAST_WORKSPACE_ID,
                    OTHER_WORKSPACE_ID,
                    SECOND_WORKSPACE_ID
                ],
                "numeric selection can focus an empty Space"
            );
        })
        .run(vec![
            Event::Command {
                command: Command::Space(SpaceOperation::Focus(SpaceSelector::Number(4))),
            },
            Event::Command {
                command: Command::Space(SpaceOperation::Focus(SpaceSelector::Next)),
            },
            Event::Command {
                command: Command::Space(SpaceOperation::Focus(SpaceSelector::Next)),
            },
            Event::Command {
                command: Command::Space(SpaceOperation::Focus(SpaceSelector::Previous)),
            },
            Event::Command {
                command: Command::Space(SpaceOperation::Focus(SpaceSelector::Number(2))),
            },
        ]);
}

#[test]
fn native_space_commands_are_rejected_during_mission_control() {
    let mut harness = TestHarness::new().with_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1],
    );
    harness.world().resource_mut::<MissionControlActive>().0 = true;

    harness
        .on_iteration(0, |_world, state| {
            assert!(state.native_space_focuses().is_empty());
            assert_eq!(state.active_workspace(TEST_DISPLAY_ID), TEST_WORKSPACE_ID);
        })
        .run(vec![Event::Command {
            command: Command::Space(SpaceOperation::Focus(SpaceSelector::Next)),
        }]);
}

/// Long enough for a 50ms confirmation check to land after the virtual
/// window server applies a request, and for the resulting commands to flush.
const NATIVE_REACTION: Duration = Duration::from_millis(200);

/// Past the 2s confirmation deadline, measured from submission.
const NATIVE_DEADLINE: Duration = Duration::from_millis(2200);

/// A single-display harness whose one window is a configured follower and
/// whose display offers `workspaces` for the user to switch between.
fn follower_harness(workspaces: Vec<WorkspaceId>) -> TestHarness {
    let mut params = WindowParams::new(".*", None);
    params.follow = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();
    TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            workspaces,
        )
        .with_windows(1)
}

/// The user switches the test display to `workspace_id`, and the OS
/// notification arrives.
fn user_switches_native_space(harness: &mut TestHarness, workspace_id: WorkspaceId) {
    harness
        .mock_state
        .activate_workspace(TEST_DISPLAY_ID, workspace_id, false);
    harness.world().write_message(Event::SpaceChanged);
}

fn follower_move_pending(world: &mut World, id: WinID) -> bool {
    let entity = find_window_entity(id, world);
    world.entity(entity).contains::<FollowSpacePending>()
}

fn native_switch_pending(world: &World) -> bool {
    world.resource::<InstantSpaceSwitch>().is_pending()
}

/// How long the pending native switch has been waiting, as the ECS sees it.
fn switch_waited(world: &World) -> Duration {
    let now = world.resource::<Time>().elapsed();
    world.resource::<InstantSpaceSwitch>().waited(now)
}

/// The display the ECS treats as active.
fn ecs_active_display(world: &mut World) -> u32 {
    world
        .query_filtered::<&Display, With<ActiveDisplayMarker>>()
        .single(world)
        .expect("one active display")
        .id()
}

/// The native Spaces whose strips the ECS treats as active.
fn ecs_active_workspaces(world: &mut World) -> Vec<WorkspaceId> {
    world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .iter(world)
        .map(LayoutStrip::id)
        .collect()
}

/// Configuration with the native switch opt-in and nothing else.
fn instant_switch_config() -> Config {
    (
        MainOptions {
            skip_native_space_switch_animation: Some(true),
            ..default()
        },
        vec![],
    )
        .into()
}

/// The external display, to the right of the test display.
fn ext_display_bounds() -> IRect {
    IRect::new(
        TEST_DISPLAY_WIDTH,
        0,
        TEST_DISPLAY_WIDTH + EXT_DISPLAY_WIDTH,
        EXT_DISPLAY_HEIGHT,
    )
}

/// Whether `frame` sits entirely on the test display.
fn on_test_display(frame: IRect) -> bool {
    let test_bounds = IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT);
    test_bounds.contains(frame.min) && frame.max.x <= TEST_DISPLAY_WIDTH
}

/// A two-display harness whose one window, 100, is a configured follower on
/// the external display's Space. Layout animation is instant, so a confirmed
/// placement shows on the next frame.
fn cross_display_follower_harness() -> TestHarness {
    let mut params = WindowParams::new(".*", None);
    params.follow = Some(true);
    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            ..default()
        },
        vec![params],
    )
        .into();
    TestHarness::new()
        .with_config(config)
        .with_display(EXT_DISPLAY_ID, ext_display_bounds(), vec![EXT_WORKSPACE_ID])
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame.min.x += TEST_DISPLAY_WIDTH;
            window.frame.max.x += TEST_DISPLAY_WIDTH;
        })
}

/// Runs startup the way production does when windows sit on Spaces that are
/// not showing: across several frames. Windows found while `Initializing`
/// is still present are neither focused nor requested for a native switch,
/// so the first request is the one the test makes.
fn start_across_frames(harness: &mut TestHarness) {
    harness.hold_initialization();
    harness.advance(NATIVE_REACTION);
    harness.release_initialization();
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
}

/// A started single-display harness with the native switch opt-in whose
/// windows 0 and 1 both sit on a Space that is not showing, and whose window
/// server holds activations in flight. No switch has been requested yet.
fn deferred_hidden_space_harness() -> TestHarness {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = TestHarness::new()
        .with_config(instant_switch_config())
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID],
        )
        .with_workspace_window(0, NEXT_WORKSPACE_ID, |_| {})
        .with_workspace_window(1, NEXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(TEST_WINDOW_WIDTH, 0, 2 * TEST_WINDOW_WIDTH, 700);
        });
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    start_across_frames(&mut harness);
    assert!(
        harness.mock_state.workspace_focuses().is_empty()
            && !native_switch_pending(harness.world()),
        "startup requests no switch for windows on a Space that is not showing"
    );
    harness
}

#[test]
fn native_focus_request_survives_early_space_change_until_confirmed() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let config: Config = (
        MainOptions {
            skip_native_space_switch_animation: Some(true),
            ..default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID],
        )
        .with_windows(1);
    harness
        .mock_state
        .activate_workspace(TEST_DISPLAY_ID, NEXT_WORKSPACE_ID, false);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.workspace_focuses(), vec![0]);
    assert_eq!(harness.mock_state.pending_native_requests(), 1);
    assert!(native_switch_pending(harness.world()));

    // A Space notification that is not the requested switch arrives while the
    // request is in flight, together with a repeat of the focus notification.
    harness.world().write_message(Event::SpaceChanged);
    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert!(
        native_switch_pending(harness.world()),
        "an unrelated Space change does not stand in for confirmation"
    );
    assert_eq!(
        harness.mock_state.workspace_focuses(),
        vec![0],
        "a repeated focus notification does not resubmit the switch"
    );
    assert_eq!(
        harness.mock_state.active_workspace(TEST_DISPLAY_ID),
        NEXT_WORKSPACE_ID
    );
    assert!(
        harness
            .mock_state
            .native_space_focus_completions()
            .is_empty(),
        "focus policy waits for the window server"
    );

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.active_workspace(TEST_DISPLAY_ID),
        TEST_WORKSPACE_ID
    );
    assert_eq!(
        harness.mock_state.native_space_focus_completions(),
        vec![TEST_WORKSPACE_ID]
    );
    assert!(!native_switch_pending(harness.world()));
}

#[test]
fn command_focus_defers_cross_display_focus_until_native_confirmation() {
    const EXT_NEXT_WORKSPACE_ID: WorkspaceId = EXT_WORKSPACE_ID + 1;

    let mut harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        ext_display_bounds(),
        vec![EXT_WORKSPACE_ID, EXT_NEXT_WORKSPACE_ID],
    );
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::Space(SpaceOperation::Focus(SpaceSelector::Number(3))),
    }]);

    assert_eq!(
        harness.mock_state.native_space_focuses(),
        vec![EXT_NEXT_WORKSPACE_ID]
    );
    assert_eq!(harness.mock_state.pending_native_requests(), 1);
    assert_eq!(
        harness.mock_state.active_display(),
        TEST_DISPLAY_ID,
        "the source display keeps focus while the switch is in flight"
    );
    assert_eq!(harness.mock_state.cursor_position(), Origin::ZERO);
    assert!(
        harness
            .mock_state
            .native_space_focus_completions()
            .is_empty()
    );
    assert_eq!(ecs_active_display(harness.world()), TEST_DISPLAY_ID);
    assert_eq!(
        ecs_active_workspaces(harness.world()),
        vec![TEST_WORKSPACE_ID],
        "the ECS keeps the source display and Space until the switch is confirmed"
    );

    harness.world().write_message(Event::Command {
        command: Command::Space(SpaceOperation::Focus(SpaceSelector::Next)),
    });
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.native_space_focuses(),
        vec![EXT_NEXT_WORKSPACE_ID],
        "a second command waits for the in-flight switch"
    );

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.active_workspace(EXT_DISPLAY_ID),
        EXT_NEXT_WORKSPACE_ID
    );
    assert_eq!(
        harness.mock_state.native_space_focus_completions(),
        vec![EXT_NEXT_WORKSPACE_ID]
    );
    assert_eq!(harness.mock_state.active_display(), EXT_DISPLAY_ID);
    assert!(
        ext_display_bounds().contains(harness.mock_state.cursor_position()),
        "the cursor follows only once the Space is confirmed"
    );
    assert!(!native_switch_pending(harness.world()));
    assert_eq!(ecs_active_display(harness.world()), EXT_DISPLAY_ID);
    assert_eq!(
        ecs_active_workspaces(harness.world()),
        vec![EXT_NEXT_WORKSPACE_ID],
        "the ECS follows the confirmed switch onto the other display"
    );
}

#[test]
fn native_focus_pending_clears_when_the_target_space_vanishes() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;
    const THIRD_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 2;

    let config: Config = (
        MainOptions {
            skip_native_space_switch_animation: Some(true),
            ..default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID, THIRD_WORKSPACE_ID],
        )
        .with_workspace_window(0, NEXT_WORKSPACE_ID, |_| {});
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.workspace_focuses(), vec![0]);
    assert!(native_switch_pending(harness.world()));

    // The user closes the requested Desktop before the window server applies
    // the switch.
    harness
        .mock_state
        .remove_workspace(TEST_DISPLAY_ID, NEXT_WORKSPACE_ID);
    harness.advance(NATIVE_REACTION);
    assert!(
        !native_switch_pending(harness.world()),
        "a switch to a vanished Space goes idle instead of waiting out the deadline"
    );
    assert!(
        harness
            .mock_state
            .native_space_focus_completions()
            .is_empty()
    );
    assert_eq!(
        harness.mock_state.active_workspace(TEST_DISPLAY_ID),
        TEST_WORKSPACE_ID
    );

    // The abandoned switch no longer blocks a new request.
    harness.world().write_message(Event::Command {
        command: Command::Space(SpaceOperation::Focus(SpaceSelector::Next)),
    });
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.native_space_focuses(),
        vec![THIRD_WORKSPACE_ID]
    );
}

#[test]
fn follower_repositions_only_after_exact_membership_is_confirmed() {
    let mut harness = cross_display_follower_harness();
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    let frame_before = window_frame(harness.world(), 100);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![100], TEST_WORKSPACE_ID)]
    );
    assert_eq!(harness.mock_state.window_workspace(100), EXT_WORKSPACE_ID);
    assert!(follower_move_pending(harness.world(), 100));

    harness.advance(Duration::from_millis(500));
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "an unobserved move is not resubmitted"
    );
    assert_eq!(
        window_frame(harness.world(), 100),
        frame_before,
        "the follower stays put until its membership is confirmed"
    );

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(100), TEST_WORKSPACE_ID);
    let frame = window_frame(harness.world(), 100);
    assert!(
        on_test_display(frame),
        "follower at {frame:?} lands on the confirmed display"
    );
    assert!(!follower_move_pending(harness.world(), 100));
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

#[test]
fn follower_observes_an_uncertain_move_instead_of_resubmitting() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = follower_harness(vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID]);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Uncertain { applies: true });
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    assert!(harness.mock_state.workspace_moves().is_empty());

    user_switches_native_space(&mut harness, NEXT_WORKSPACE_ID);
    harness.advance(Duration::from_millis(500));
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], NEXT_WORKSPACE_ID)],
        "a request that may have applied is observed, not repeated"
    );
    assert_eq!(harness.mock_state.window_workspace(0), TEST_WORKSPACE_ID);
    assert!(follower_move_pending(harness.world(), 0));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), NEXT_WORKSPACE_ID);
    assert!(!follower_move_pending(harness.world(), 0));
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

#[test]
fn follower_drops_a_refused_move_without_retrying() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = follower_harness(vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID]);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Rejected);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    user_switches_native_space(&mut harness, NEXT_WORKSPACE_ID);
    harness.advance(Duration::from_millis(500));
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], NEXT_WORKSPACE_ID)],
        "a request refused before submission is neither observed nor retried"
    );
    assert_eq!(harness.mock_state.window_workspace(0), TEST_WORKSPACE_ID);
    assert!(!follower_move_pending(harness.world(), 0));

    // Only a genuinely new Space change asks again.
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Immediate);
    user_switches_native_space(&mut harness, TEST_WORKSPACE_ID);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "switching back to the Space the follower never left needs no move"
    );
    assert!(!follower_move_pending(harness.world(), 0));
}

#[test]
fn follower_retargets_only_after_the_in_flight_move_lands() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;
    const THIRD_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 2;

    let mut harness = follower_harness(vec![
        TEST_WORKSPACE_ID,
        NEXT_WORKSPACE_ID,
        THIRD_WORKSPACE_ID,
    ]);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    user_switches_native_space(&mut harness, NEXT_WORKSPACE_ID);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], NEXT_WORKSPACE_ID)]
    );

    user_switches_native_space(&mut harness, THIRD_WORKSPACE_ID);
    harness.advance(Duration::from_millis(500));
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "the newer target waits for the in-flight move to be observed"
    );
    assert_eq!(harness.mock_state.window_workspace(0), TEST_WORKSPACE_ID);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], NEXT_WORKSPACE_ID), (vec![0], THIRD_WORKSPACE_ID)]
    );
    assert_eq!(harness.mock_state.window_workspace(0), NEXT_WORKSPACE_ID);
    assert!(follower_move_pending(harness.world(), 0));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), THIRD_WORKSPACE_ID);
    assert!(!follower_move_pending(harness.world(), 0));
}

#[test]
fn follower_gives_up_after_the_deadline_and_queues_a_new_target() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;
    const THIRD_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 2;

    let mut harness = follower_harness(vec![
        TEST_WORKSPACE_ID,
        NEXT_WORKSPACE_ID,
        THIRD_WORKSPACE_ID,
    ]);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    user_switches_native_space(&mut harness, NEXT_WORKSPACE_ID);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], NEXT_WORKSPACE_ID)]
    );

    harness.advance(NATIVE_DEADLINE);
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "a move the window server never applies is not retried"
    );
    assert_eq!(harness.mock_state.window_workspace(0), TEST_WORKSPACE_ID);
    assert!(
        !follower_move_pending(harness.world(), 0),
        "an unconfirmed move goes idle after the deadline"
    );

    user_switches_native_space(&mut harness, THIRD_WORKSPACE_ID);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], NEXT_WORKSPACE_ID), (vec![0], THIRD_WORKSPACE_ID)],
        "a genuinely new target queues again"
    );
}

#[test]
fn follower_survives_its_destination_vanishing_mid_flight() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = follower_harness(vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID]);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    user_switches_native_space(&mut harness, NEXT_WORKSPACE_ID);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], NEXT_WORKSPACE_ID)]
    );

    // The user closes the destination Desktop; the window server drops the
    // in-flight move and the display falls back to the original Space.
    harness
        .mock_state
        .remove_workspace(TEST_DISPLAY_ID, NEXT_WORKSPACE_ID);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "no move is sent to a Space the window already occupies"
    );
    assert_eq!(harness.mock_state.window_workspace(0), TEST_WORKSPACE_ID);
    assert!(!follower_move_pending(harness.world(), 0));

    let world = harness.world();
    let entity = find_window_entity(0, world);
    assert!(
        world
            .entity(entity)
            .contains::<FollowCurrentWorkspaceMarker>()
    );
    assert!(matches!(
        world.get::<Unmanaged>(entity),
        Some(Unmanaged::Floating)
    ));
    assert_eq!(ecs_active_workspaces(world), vec![TEST_WORKSPACE_ID]);
}

#[test]
fn follower_found_during_multi_frame_initialization_is_carried_after_setup() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut params = WindowParams::new(".*", None);
    params.follow = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID],
        )
        .with_workspace_window(0, NEXT_WORKSPACE_ID, |_| {});
    harness.hold_initialization();

    // The follower is known, and marked, frames before initialization ends.
    harness.advance(NATIVE_REACTION);
    assert!(harness.world().contains_resource::<Initializing>());
    let entity = find_window_entity(0, harness.world());
    assert!(
        harness
            .world()
            .entity(entity)
            .contains::<FollowCurrentWorkspaceMarker>()
    );
    assert!(
        harness.mock_state.workspace_moves().is_empty(),
        "nothing is carried while initializing"
    );

    harness.release_initialization();
    harness.advance(NATIVE_REACTION);
    assert!(!harness.world().contains_resource::<Initializing>());
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], TEST_WORKSPACE_ID)],
        "a follower found during initialization is carried to the active Space once it ends"
    );
    assert_eq!(harness.mock_state.window_workspace(0), TEST_WORKSPACE_ID);
    assert!(!follower_move_pending(harness.world(), 0));
}

#[test]
fn focus_on_a_sibling_of_the_pending_space_does_not_resubmit_the_switch() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = deferred_hidden_space_harness();

    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.workspace_focuses(), vec![0]);
    assert_eq!(harness.mock_state.pending_native_requests(), 1);
    let waited_before = switch_waited(harness.world());

    // The app hands focus to a sibling on the same hidden Space while the
    // activation is still in flight.
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_focuses(),
        vec![0],
        "the in-flight activation is not submitted again for a sibling"
    );
    assert_eq!(harness.mock_state.pending_native_requests(), 1);
    assert_eq!(
        harness
            .world()
            .resource::<InstantSpaceSwitch>()
            .pending_target(),
        Some(NEXT_WORKSPACE_ID)
    );
    assert!(
        switch_waited(harness.world()) >= waited_before + NATIVE_REACTION,
        "the sibling's focus does not restart the confirmation deadline"
    );

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.active_workspace(TEST_DISPLAY_ID),
        NEXT_WORKSPACE_ID
    );
    assert!(!native_switch_pending(harness.world()));
}

#[test]
fn focus_on_a_window_with_unreadable_spaces_holds_while_a_switch_is_live() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = deferred_hidden_space_harness();

    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.workspace_focuses(), vec![0]);
    let waited_before = switch_waited(harness.world());

    // Focus moves to a window the window server cannot answer for while the
    // activation is in flight.
    harness.mock_state.set_window_queries_failing(1, true);
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_focuses(),
        vec![0],
        "a window whose Spaces cannot be read does not restart the switch"
    );
    assert_eq!(harness.mock_state.pending_native_requests(), 1);
    assert!(native_switch_pending(harness.world()));
    assert_eq!(
        harness
            .world()
            .resource::<InstantSpaceSwitch>()
            .pending_target(),
        Some(NEXT_WORKSPACE_ID)
    );
    assert!(
        switch_waited(harness.world()) >= waited_before + NATIVE_REACTION,
        "the unreadable window does not restart the confirmation deadline"
    );

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.active_workspace(TEST_DISPLAY_ID),
        NEXT_WORKSPACE_ID
    );
    assert!(!native_switch_pending(harness.world()));
}

#[test]
fn focus_follows_mouse_stays_suppressed_until_the_deferred_switch_settles() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    // The hidden window floats, so the only window under the pointer is the
    // tiled one on the Space that is still showing.
    let mut hidden = WindowParams::new("^Window 1$", None);
    hidden.floating = Some(true);
    let config: Config = (
        MainOptions {
            skip_native_space_switch_animation: Some(true),
            ..default()
        },
        vec![hidden],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID],
        )
        .with_windows(1)
        .with_workspace_window(1, NEXT_WORKSPACE_ID, |window| {
            window.frame = IRect::new(600, 0, 1000, 100);
        });
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    start_across_frames(&mut harness);
    assert_focused!(harness.world(), 0);
    assert!(
        harness.mock_state.workspace_focuses().is_empty(),
        "startup requests no switch for the hidden window"
    );

    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert!(native_switch_pending(harness.world()));
    assert_focused!(harness.world(), 1);

    // 1.5s into an activation the window server has not applied yet — past
    // the fixed focus-follows-mouse delay, before the confirmation deadline —
    // the pointer crosses the source Space's window.
    let over_source_window = Event::MouseMoved {
        point: CGPoint { x: 50.0, y: 500.0 },
        modifiers: Modifiers::empty(),
    };
    harness.advance(
        Duration::from_millis(1500)
            .checked_sub(NATIVE_REACTION)
            .expect("reaction precedes the mid-flight sample"),
    );
    assert!(native_switch_pending(harness.world()));
    harness.world().write_message(over_source_window.clone());
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 1);

    // Once the request has expired, the same motion focuses again.
    harness.advance(NATIVE_DEADLINE);
    assert!(!native_switch_pending(harness.world()));
    harness.world().write_message(over_source_window);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 0);
}

#[test]
fn deferred_space_command_times_out_and_the_next_command_submits_again() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = TestHarness::new().with_display(
        TEST_DISPLAY_ID,
        IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
        vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID],
    );
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::Space(SpaceOperation::Focus(SpaceSelector::Next)),
    }]);
    assert_eq!(
        harness.mock_state.native_space_focuses(),
        vec![NEXT_WORKSPACE_ID]
    );
    assert!(native_switch_pending(harness.world()));

    harness.advance(NATIVE_DEADLINE);
    assert!(
        !native_switch_pending(harness.world()),
        "an activation the window server never applies is given up on"
    );
    assert!(
        harness
            .mock_state
            .native_space_focus_completions()
            .is_empty(),
        "nothing is completed without confirmation"
    );
    assert_eq!(
        harness.mock_state.active_workspace(TEST_DISPLAY_ID),
        TEST_WORKSPACE_ID
    );
    assert_eq!(
        ecs_active_workspaces(harness.world()),
        vec![TEST_WORKSPACE_ID]
    );

    harness.world().write_message(Event::Command {
        command: Command::Space(SpaceOperation::Focus(SpaceSelector::Next)),
    });
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.native_space_focuses(),
        vec![NEXT_WORKSPACE_ID, NEXT_WORKSPACE_ID],
        "the expired request no longer blocks a new one"
    );
    assert!(native_switch_pending(harness.world()));
}

#[test]
fn eventless_cross_display_confirmation_refreshes_the_ecs_active_display() {
    const EXT_NEXT_WORKSPACE_ID: WorkspaceId = EXT_WORKSPACE_ID + 1;

    let mut harness = TestHarness::new().with_display(
        EXT_DISPLAY_ID,
        ext_display_bounds(),
        vec![EXT_WORKSPACE_ID, EXT_NEXT_WORKSPACE_ID],
    );
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    // The window server applies the switch without announcing it.
    harness.mock_state.set_native_activation_notifies(false);
    harness.run(vec![Event::Command {
        command: Command::Space(SpaceOperation::Focus(SpaceSelector::Number(3))),
    }]);
    assert_eq!(harness.mock_state.pending_native_requests(), 1);
    assert_eq!(ecs_active_display(harness.world()), TEST_DISPLAY_ID);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.native_space_focus_completions(),
        vec![EXT_NEXT_WORKSPACE_ID]
    );
    assert_eq!(harness.mock_state.active_display(), EXT_DISPLAY_ID);
    assert!(!native_switch_pending(harness.world()));
    assert_eq!(
        ecs_active_display(harness.world()),
        EXT_DISPLAY_ID,
        "the ECS learns of the confirmed switch without an OS notification"
    );
    assert_eq!(
        ecs_active_workspaces(harness.world()),
        vec![EXT_NEXT_WORKSPACE_ID]
    );
}

#[test]
fn follower_lands_when_an_attached_window_closes_mid_flight() {
    let mut harness = cross_display_follower_harness();
    harness.mock_state.attach_window(100, 101);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    let frame_before = window_frame(harness.world(), 100);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![100, 101], TEST_WORKSPACE_ID)],
        "the attached window travels in the follower's batch"
    );
    assert!(follower_move_pending(harness.world(), 100));

    // The sheet is dismissed while the move is in flight.
    harness.mock_state.os_close_window(101);
    harness.advance(NATIVE_REACTION);
    assert!(
        follower_move_pending(harness.world(), 100),
        "the follower itself is still unconfirmed"
    );
    assert_eq!(window_frame(harness.world(), 100), frame_before);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(100), TEST_WORKSPACE_ID);
    let frame = window_frame(harness.world(), 100);
    assert!(
        on_test_display(frame),
        "follower at {frame:?} is placed once its own move landed; its closed sheet never can"
    );
    assert!(!follower_move_pending(harness.world(), 100));
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

#[test]
fn follower_never_confirms_on_an_attached_window_whose_queries_fail() {
    let mut harness = cross_display_follower_harness();
    harness.mock_state.attach_window(100, 101);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    let frame_before = window_frame(harness.world(), 100);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![100, 101], TEST_WORKSPACE_ID)]
    );

    // The window server applies the move but can no longer answer for the
    // sheet: neither its Spaces nor whether it still exists.
    harness.mock_state.set_window_queries_failing(101, true);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(100), TEST_WORKSPACE_ID);
    assert_eq!(
        window_frame(harness.world(), 100),
        frame_before,
        "a member that cannot be asked about is not a confirmed one"
    );

    harness.advance(NATIVE_DEADLINE);
    assert_eq!(window_frame(harness.world(), 100), frame_before);
    assert!(!follower_move_pending(harness.world(), 100));
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "an unconfirmable move is not resubmitted"
    );
}

#[test]
fn follower_with_an_unmovable_attached_window_is_not_moved_without_it() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = follower_harness(vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID]);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    // A sheet the window server will not move opens on the follower.
    harness.mock_state.attach_window(0, 1);
    harness.mock_state.set_window_sticky(1, true);

    user_switches_native_space(&mut harness, NEXT_WORKSPACE_ID);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0, 1], NEXT_WORKSPACE_ID)],
        "the batch is offered whole; a live attached window is not dropped to make it movable"
    );
    assert_eq!(
        harness.mock_state.window_workspace(0),
        TEST_WORKSPACE_ID,
        "a batch the window server refuses moves nothing"
    );
    assert_eq!(harness.mock_state.pending_native_requests(), 0);
    assert!(!follower_move_pending(harness.world(), 0));

    harness.advance(NATIVE_DEADLINE);
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "a refused batch is not retried"
    );
}

#[test]
fn follower_leaves_out_an_attached_window_that_already_closed() {
    const NEXT_WORKSPACE_ID: WorkspaceId = TEST_WORKSPACE_ID + 1;

    let mut harness = follower_harness(vec![TEST_WORKSPACE_ID, NEXT_WORKSPACE_ID]);
    harness.mock_state.attach_window(0, 1);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);

    // The sheet is dismissed; the window server still lists the association.
    harness.mock_state.os_close_window(1);
    harness.advance(NATIVE_REACTION);

    user_switches_native_space(&mut harness, NEXT_WORKSPACE_ID);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], NEXT_WORKSPACE_ID)],
        "a window that is definitively gone is left out rather than failing the batch"
    );
    assert_eq!(harness.mock_state.window_workspace(0), NEXT_WORKSPACE_ID);
    assert!(!follower_move_pending(harness.world(), 0));
}

#[test]
fn test_dont_focus() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 }, // 0
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        }, // 1
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        }, // 2
        Event::Command {
            command: Command::PrintState,
        }, // 3
    ];

    let offscreen_right = TEST_DISPLAY_WIDTH - 5;

    let mut params = WindowParams::new(".*", None);
    params.dont_focus = Some(true);
    params.index = Some(100);
    let config: Config = (MainOptions::default(), vec![params]).into();

    let harness = TestHarness::new().with_config(config).with_windows(3);

    harness
        .on_iteration(1, move |world, state| {
            let origin = Origin::new(0, 0);
            let size = Size::new(TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
            let frame = IRect::from_corners(origin, origin + size);
            let window = state.spawn_window(TEST_PROCESS_ID, TEST_WORKSPACE_ID, 3, frame);
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(3, move |world, _| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 3, offscreen_right, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_focus_window_by_number() {
    assert!(parse_command(&["window", "focus", "0"]).is_err());
    let command = parse_command(&["window", "focus", "2"]).unwrap();

    TestHarness::new()
        .with_windows(3)
        .on_iteration(1, |world, _state| assert_focused!(world, 1))
        .on_iteration(2, |world, _state| assert_focused!(world, 1))
        .on_iteration(3, |world, _state| assert_focused!(world, 2))
        .run(vec![
            Event::MenuOpened { window_id: 0 },
            Event::Command {
                command: command.clone(),
            },
            Event::Command {
                command: Command::Window(Operation::Manage),
            },
            Event::Command { command },
        ]);
}

#[test]
fn test_offscreen_windows_preserve_height() {
    let expected_height = TEST_DISPLAY_HEIGHT - TEST_MENUBAR_HEIGHT;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(1, move |world, _state| {
            assert_window_size!(world, 4, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 3, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 2, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 1, TEST_WINDOW_WIDTH, expected_height);
            assert_window_size!(world, 0, TEST_WINDOW_WIDTH, expected_height);
        })
        .run(commands);
}

#[test]
fn test_sliver_smaller_than_edge_padding() {
    const PADDING: u16 = 8;
    const SLIVER: u16 = 1;

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
    ];

    let top_edge = TEST_MENUBAR_HEIGHT + i32::from(PADDING);
    let right_edge = TEST_DISPLAY_WIDTH - i32::from(PADDING);
    let offscreen_right = TEST_DISPLAY_WIDTH - i32::from(SLIVER);
    let offscreen_left = i32::from(SLIVER) - TEST_WINDOW_WIDTH;
    let left_edge = i32::from(PADDING);

    let config: Config = (
        MainOptions {
            sliver_width: Some(SLIVER),
            padding_top: Some(PADDING),
            padding_bottom: Some(PADDING),
            padding_left: Some(PADDING),
            padding_right: Some(PADDING),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .on_iteration(2, move |world, _state| {
            assert_window_at!(world, 0, left_edge, top_edge);
            assert_window_at!(world, 1, left_edge + TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 2, left_edge + 2 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 3, offscreen_right, top_edge);
            assert_window_at!(world, 4, offscreen_right, top_edge);
        })
        .on_iteration(3, move |world, _state| {
            assert_window_at!(world, 0, offscreen_left, top_edge);
            assert_window_at!(world, 1, offscreen_left, top_edge);
            assert_window_at!(world, 2, right_edge - 3 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 3, right_edge - 2 * TEST_WINDOW_WIDTH, top_edge);
            assert_window_at!(world, 4, right_edge - TEST_WINDOW_WIDTH, top_edge);
        })
        .run(commands);
}

fn perform_trackpad_swipe(harness: &mut TestHarness, delta: f64) {
    let world = harness.world();
    world.write_message(Event::TouchpadDown);
    world.write_message(Event::Swipe { delta, fingers: 3 });
    world.write_message(Event::TouchpadUp);
    harness.advance(Duration::from_secs(1));
}

#[test]
fn test_scrolling() {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(3);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ]);

    {
        let world = harness.world();
        assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
        assert_window_at!(world, 1, 400, TEST_MENUBAR_HEIGHT);
        assert_window_at!(world, 2, 800, TEST_MENUBAR_HEIGHT);
    }

    // A single event's delta is a fraction of the viewport travelled in one
    // frame, and the gesture velocity it produces is `delta / dt`. 0.04 over
    // a 20ms frame is two viewport widths per second — a brisk ordinary swipe.
    perform_trackpad_swipe(&mut harness, 0.04);

    // The strip has come to rest mid-scroll: still one contiguous run of
    // 400px columns, none of them parked at an edge sliver.
    let world = harness.world();
    assert_window_at!(world, 0, -130, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 1, 270, TEST_MENUBAR_HEIGHT);
    assert_window_at!(world, 2, 670, TEST_MENUBAR_HEIGHT);
}

#[test]
fn swipe_delta_is_not_integrated_twice_while_fingers_are_down() {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    harness.world().write_message(Event::TouchpadDown);
    harness.world().write_message(Event::Swipe {
        delta: 0.04,
        fingers: 3,
    });
    harness.advance(Duration::from_millis(20));

    let world = harness.world();
    let mut strips =
        world.query_filtered::<(&Position, &crate::ecs::Scrolling), With<ActiveWorkspaceMarker>>();
    let (position, scrolling) = strips.single(world).unwrap();

    // 0.04 trackpad units × 1024px viewport × default 0.35 sensitivity.
    // The constraint system rounds the resulting -14.336px to -14px.
    assert_eq!(position.x, -14);
    assert!(scrolling.velocity > 0.0);
    assert!(scrolling.is_user_swiping);
}

#[test]
fn touchpad_up_transitions_directly_to_inertia() {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    harness.world().write_message(Event::TouchpadDown);
    harness.world().write_message(Event::Swipe {
        delta: 0.04,
        fingers: 3,
    });
    harness.advance(Duration::from_millis(20));

    let position_before_lift = {
        let world = harness.world();
        let mut strips = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        strips.single(world).unwrap().x
    };

    harness.world().write_message(Event::TouchpadUp);
    harness.advance(Duration::from_millis(20));

    let world = harness.world();
    let mut strips =
        world.query_filtered::<(&Position, &crate::ecs::Scrolling), With<ActiveWorkspaceMarker>>();
    let (position, scrolling) = strips.single(world).unwrap();

    assert!(!scrolling.is_user_swiping);
    assert!(scrolling.velocity > 0.0);
    assert!(position.x < position_before_lift);
}

#[test]
fn inertia_survives_switching_displays_before_finger_lift() {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .with_display(
            EXT_DISPLAY_ID,
            IRect::new(
                TEST_DISPLAY_WIDTH,
                0,
                2 * TEST_DISPLAY_WIDTH,
                TEST_DISPLAY_HEIGHT,
            ),
            vec![EXT_WORKSPACE_ID],
        );
    for id in 100..103 {
        harness = harness.with_workspace_window(id, EXT_WORKSPACE_ID, |window| {
            window.frame.min.x += TEST_DISPLAY_WIDTH;
            window.frame.max.x += TEST_DISPLAY_WIDTH;
        });
    }
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    harness.world().write_message(Event::TouchpadDown);
    harness.world().write_message(Event::Swipe {
        delta: 0.04,
        fingers: 3,
    });
    harness.advance(Duration::from_millis(20));

    // Focus crosses displays before the release reaches the input reader:
    // the target Space is already current there, so only the display focus
    // policy is outstanding.
    let window_manager = harness.world().resource::<crate::manager::WindowManager>();
    assert!(window_manager.focus_native_space(EXT_WORKSPACE_ID).unwrap());
    window_manager
        .complete_native_space_focus(EXT_WORKSPACE_ID)
        .unwrap();
    harness.world().write_message(Event::DisplayChanged);
    harness.mock_state.focus_window(100);
    harness.advance(Duration::from_millis(100));
    harness.world().write_message(Event::TouchpadUp);
    harness.advance(Duration::from_millis(100));

    let window = find_window_entity(100, harness.world());
    let before_swipe = harness.world().get::<Position>(window).unwrap().x;

    harness.world().write_message(Event::TouchpadDown);
    harness.world().write_message(Event::Swipe {
        delta: 0.04,
        fingers: 3,
    });
    harness.advance(Duration::from_millis(20));
    let before_lift = harness.world().get::<Position>(window).unwrap().x;
    assert!(
        before_lift < before_swipe,
        "finger motion must reach the second display before testing its inertia"
    );

    harness.world().write_message(Event::TouchpadUp);
    harness.advance(Duration::from_millis(60));

    assert!(
        harness.world().get::<Position>(window).unwrap().x < before_lift,
        "the second display must keep moving after finger lift without a restart"
    );
}

#[test]
fn inertia_survives_a_short_frame() {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(8);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    harness.world().write_message(Event::TouchpadDown);
    harness.world().write_message(Event::Swipe {
        delta: 0.2,
        fingers: 3,
    });
    harness.advance(Duration::from_millis(20));
    harness.world().write_message(Event::TouchpadUp);
    harness.advance(Duration::from_millis(60));

    // A catch-up update can be much shorter than the usual animation frame.
    harness
        .app
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(1),
        ));
    harness.app.update();
    let window = find_window_entity(1, harness.world());
    let after_short_frame = harness.world().get::<Position>(window).unwrap().x;

    harness
        .app
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(20),
        ));
    harness.advance(Duration::from_millis(20));

    assert!(
        harness.world().get::<Position>(window).unwrap().x < after_short_frame,
        "a short frame must not discard the remaining momentum"
    );
}

#[test]
fn inertia_accumulates_subpixel_motion() {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    harness.world().write_message(Event::TouchpadDown);
    harness.world().write_message(Event::Swipe {
        delta: 0.01,
        fingers: 3,
    });
    harness.advance(Duration::from_millis(20));
    let window = find_window_entity(1, harness.world());
    let before_lift = harness.world().get::<Position>(window).unwrap().x;

    harness.world().write_message(Event::TouchpadUp);
    harness
        .app
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(1),
        ));
    // Each update moves less than half a pixel, but together they must coast.
    for _ in 0..60 {
        harness.advance(Duration::from_millis(20));
    }

    assert!(
        harness.world().get::<Position>(window).unwrap().x < before_lift - 5,
        "fractional inertia must accumulate instead of rounding away each frame"
    );
}

#[test]
fn stationary_fingers_do_not_start_inertia() {
    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();
    let mut harness = TestHarness::new().with_config(config).with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    harness.world().write_message(Event::TouchpadDown);
    harness.world().write_message(Event::Swipe {
        delta: 0.04,
        fingers: 3,
    });
    harness.advance(Duration::from_millis(20));

    let position_while_moving = {
        let world = harness.world();
        let mut strips = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        strips.single(world).unwrap().x
    };

    // No gesture deltas while the fingers remain down means they are
    // stationary, not lifted. Wait past the idle threshold and another frame
    // that would have integrated momentum if the gesture had ended.
    harness.advance(Duration::from_millis(100));
    harness.advance(Duration::from_millis(20));

    let world = harness.world();
    let mut strips =
        world.query_filtered::<(&Position, &crate::ecs::Scrolling), With<ActiveWorkspaceMarker>>();
    let (position, scrolling) = strips.single(world).unwrap();

    assert_eq!(position.x, position_while_moving);
    assert!(scrolling.velocity.abs() < f64::EPSILON);
    assert!(scrolling.is_user_swiping);
}

#[test]
fn phase_less_scroll_uses_idle_cleanup() {
    let mut harness = TestHarness::new().with_windows(3);
    harness.run(vec![Event::MenuOpened { window_id: 0 }]);

    harness.world().write_message(Event::Scroll { delta: 1.0 });
    harness.advance(Duration::from_millis(20));

    {
        let world = harness.world();
        let mut strips =
            world.query_filtered::<&crate::ecs::Scrolling, With<ActiveWorkspaceMarker>>();
        assert!(strips.single(world).unwrap().is_user_swiping);
    }

    harness.advance(Duration::from_millis(100));

    let world = harness.world();
    let mut strips = world.query_filtered::<&crate::ecs::Scrolling, With<ActiveWorkspaceMarker>>();
    assert!(strips.iter(world).next().is_none());
}

#[test]
#[allow(clippy::float_cmp)]
fn test_scrolling_stop() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
        Event::TouchpadDown,
    ];

    let config: Config = (
        MainOptions {
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(3)
        .on_iteration(3, |world, _state| {
            use crate::ecs::Scrolling;
            let mut query = world.query::<&Scrolling>();
            let scroll = query.single(world).unwrap();
            assert_eq!(scroll.velocity, 0.0);
            assert!(scroll.is_user_swiping);
        })
        .run(commands);
}

#[test]
fn test_window_hidden_ratio() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
    ];

    let config: Config = (
        MainOptions {
            window_hidden_ratio: Some(0.5),
            animation_speed: Some(10000.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(2, |world, _state| {
            let entity = find_window_entity(0, world);
            let window = world.get::<Window>(entity).expect("finding window");
            assert!(window.frame().min.x < 0);
        })
        .run(commands);
}

#[test]
fn test_window_swap_brings_focused_into_view() {
    // After Center, id=4 is at the centered position. Swap(Last) bubbles
    // id=4 to column 4 (layout x=1600); with the strip at +312 that would
    // put id=4 off-screen to the right (1912). ensure_visible_in_strip
    // scrolls the strip by exactly the shortfall so id=4 sits at the right
    // edge of the viewport (max.x - width = 624). The strip does NOT
    // re-anchor id=4 to its old centered position — there was room to the
    // right, so it slides there. id=0 takes the slot immediately to the
    // left.
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::Last)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let centered = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;
    let right_edge = TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH;

    TestHarness::new()
        .with_config(config)
        .with_windows(5)
        .on_iteration(2, move |world, _state| {
            assert_window_at!(world, 0, centered, TEST_MENUBAR_HEIGHT);
        })
        .on_iteration(4, move |world, _state| {
            assert_window_at!(world, 0, right_edge, TEST_MENUBAR_HEIGHT);
            assert_window_at!(
                world,
                4,
                right_edge - TEST_WINDOW_WIDTH,
                TEST_MENUBAR_HEIGHT
            );
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_window_swap_keeps_strip_when_in_view() {
    // Two windows fit the viewport. Swap(West) on the focused (right)
    // window swaps the columns: both new layout slots are still inside the
    // viewport with the strip where it is, so ensure_visible_in_strip does
    // nothing. The per-window animation slides each window into the other's
    // old position.
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::West)),
        },
    ];

    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(2, |world, _state| {
            assert_window_at!(world, 1, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 0, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_rapid_focus_not_swallowed() {
    let mut harness = TestHarness::new().with_windows(5);

    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Last)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ]);

    assert_focused!(harness.world(), 4);

    let focus_west = Event::Command {
        command: Command::Window(Operation::Focus(Direction::West)),
    };
    for _ in 0..3 {
        harness
            .app
            .world_mut()
            .write_message::<Event>(focus_west.clone());
        harness.app.update();
    }

    assert_focused!(harness.world(), 1);
}

#[test]
fn test_stale_focus_event_ignored() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::WindowFocused { window_id: 4 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 1);
        })
        .on_iteration(2, |world, _state| {
            assert_focused!(world, 1);
        })
        .run(commands);
}

#[test]
fn test_repeated_external_focus_reshuffles_already_focused_window() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 0);

            let mut query = world.query::<(Entity, &LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let (entity, _, _) = query
                .iter(world)
                .find(|(_, _, active)| *active)
                .expect("active strip");
            world.commands().entity(entity).insert((
                Position(Origin::new(0, 0)),
                RepositionMarker(Origin::new(-TEST_DISPLAY_WIDTH, 0)),
            ));
        })
        .on_iteration(2, |_world, state| {
            state.focus_window(0);
        })
        .on_iteration(4, |world, _state| {
            assert_focused!(world, 0);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
        })
        .run(commands);
}

#[test]
fn test_external_focus_reactivates_hidden_virtual_strip_when_marker_is_stale() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::WindowFocused { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 1);
            assert_focused!(world, 0);
        })
        .on_iteration(3, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 0);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
        })
        .run(commands);
}

// When the focused window leaves the active strip (e.g. it just became
// floating, or the OS handed focus to an off-strip window), window_focus
// east/west must enter the strip from the appropriate side rather than
// silently doing nothing.
fn focused_window_id(world: &mut World) -> i32 {
    let mut q = world.query::<(&Window, Has<crate::ecs::FocusedMarker>)>();
    q.iter(world)
        .find_map(|(w, f)| f.then_some(w.id()))
        .expect("a focused window")
}

fn entity_to_window_id(world: &mut World, entity: Entity) -> i32 {
    let mut q = world.query::<(&Window, Entity)>();
    q.iter(world)
        .find_map(|(w, e)| (e == entity).then_some(w.id()))
        .expect("entity must be a Window")
}

fn active_strip_first_id(world: &mut World) -> i32 {
    let entity = {
        let mut q = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
        let strip = q.single(world).expect("a single active strip");
        strip
            .first()
            .expect("strip should have a column")
            .top()
            .expect("column should have a top entity")
    };
    entity_to_window_id(world, entity)
}

fn active_strip_last_id(world: &mut World) -> i32 {
    let entity = {
        let mut q = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
        let strip = q.single(world).expect("a single active strip");
        strip
            .last()
            .expect("strip should have a column")
            .top()
            .expect("column should have a top entity")
    };
    entity_to_window_id(world, entity)
}

// Strip the currently focused entity out of every LayoutStrip so the
// "focused window not in active strip" condition is reproduced regardless
// of how the harness happened to populate the strip. Without this, the
// init-time duplicate-insertion in the test scheduler keeps the entity in
// the strip and the bug is masked.
fn remove_focused_from_all_strips(world: &mut World) {
    let entity = {
        let mut q = world.query_filtered::<Entity, With<crate::ecs::FocusedMarker>>();
        q.single(world).expect("a single focused entity")
    };
    let mut q = world.query::<&mut LayoutStrip>();
    for mut strip in q.iter_mut(world) {
        while strip.contains(entity) {
            strip.remove(entity);
        }
    }
}

#[test]
fn test_focus_recovers_when_focused_window_is_outside_strip() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            // Make the focused entity genuinely live outside any strip,
            // mirroring the state the user reported: the OS handed focus
            // to a window Paneru doesn't track on its active strip.
            remove_focused_from_all_strips(world);
        })
        .on_iteration(1, |world, _state| {
            // Before the fix: get_window_in_direction returns None because
            // active_strip.index_of(focused) fails for a window that's not
            // in the strip, so East is a silent no-op and focus stays on 0.
            let focused = focused_window_id(world);
            assert_ne!(
                focused, 0,
                "focus must leave the off-strip window 0 when pressing East",
            );
            let expected = active_strip_first_id(world);
            assert_eq!(
                focused, expected,
                "East from outside the strip enters at the first (leftmost) column",
            );
        })
        .run(commands);
}

/// A background native tab that ended up with a column of its own is folded
/// back into the column of the tab that is showing, so the strip stops holding
/// a slot nothing can ever appear in.
#[test]
fn test_stray_background_tab_is_folded_into_the_visible_tab() {
    use bevy::ecs::system::RunSystemOnce as _;

    use crate::ecs::{Bounds, Position};

    let mut harness = TestHarness::new().with_windows(2);
    for _ in 0..3 {
        harness.app.update();
    }

    // Window 1 is a background tab of window 0: same app, same frame, and the
    // window server does not report it on screen.
    harness.mock_state.update_window(1, |window| {
        window.visible = false;
    });

    let world = harness.app.world_mut();
    let leader = find_window_entity(0, world);
    let background = find_window_entity(1, world);
    let position = world.get::<Position>(leader).expect("a position").clone();
    let bounds = world.get::<Bounds>(leader).expect("bounds").clone();
    world.entity_mut(background).insert((position, bounds));

    {
        let mut strips = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
        let strip = strips.single(world).expect("one active strip");
        assert_eq!(strip.len(), 2, "the tabs start out in columns of their own");
    }

    world
        .run_system_once(crate::ecs::systems::regroup_stray_native_tabs)
        .expect("the regrouping system runs");

    let mut strips = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
    let strip = strips.single(world).expect("one active strip");
    assert_eq!(strip.len(), 1, "the stray column is gone");
    assert!(strip.tabbed(background), "the background tab is a tab now");
    assert!(strip.tabbed(leader));
}

/// An app with native tabs answers "which window is focused?" with whichever
/// member of the tab group it decided to show, so the id on a focus event can
/// already be out of date. Paneru has to follow the app to that window; drop
/// the event and the strip stays parked where it was, which is what makes
/// Cmd-Tab into a tabbed terminal look like nothing happened.
#[test]
fn test_focus_event_follows_the_window_the_app_says_is_focused() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::WindowFocused { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(1, |world, state| {
            assert_eq!(focused_window_id(world), 0);
            // The app has moved on to its other window without telling us.
            state.set_focused_window(1);
        })
        .on_iteration(3, |world, _state| {
            assert_eq!(
                focused_window_id(world),
                1,
                "the focus event must follow the app to the window it actually focused",
            );
        })
        .run(commands);
}

#[test]
fn test_focus_west_from_outside_strip_enters_at_last_column() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(0, |world, _state| {
            remove_focused_from_all_strips(world);
        })
        .on_iteration(1, |world, _state| {
            let focused = focused_window_id(world);
            let expected = active_strip_last_id(world);
            assert_ne!(focused, 0);
            assert_eq!(
                focused, expected,
                "West from outside the strip enters at the last (rightmost) column",
            );
        })
        .run(commands);
}

#[test]
fn test_external_focus_restores_app_hidden_window_to_original_virtual_strip() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::ApplicationHidden {
            pid: TEST_PROCESS_ID,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::ApplicationVisible {
            pid: TEST_PROCESS_ID,
        },
        Event::WindowFocused { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(2, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 1);
        })
        .on_iteration(5, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 0);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn test_external_focus_restores_hidden_window_without_visible_event() {
    let ignored_repositions = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::ApplicationHidden {
            pid: TEST_PROCESS_ID,
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::WindowFocused { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, move |world, _state| {
            let mut query = world.query::<&mut Window>();
            let mut window = query
                .iter_mut(world)
                .find(|window| window.id() == 0)
                .expect("window 0");
            window.reposition(Origin::new(0, TEST_DISPLAY_HEIGHT));
            ignored_repositions.store(1, std::sync::atomic::Ordering::SeqCst);
        })
        .on_iteration(4, |world, _state| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, 0);
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn mouse_in_bottom_right_corner_does_not_change_focus() {
    // Focus window 2 explicitly, then move cursor into the bottom-right 30x30
    // dead zone. The corner gate should suppress the focus-follow-mouse event,
    // so focus stays on window 2.
    //
    // Test display is 1024x768 with no Dock, so the dead zone is
    // x >= 994, y >= 738. Cursor at (1010, 750) is inside it. The mock's
    // find_window_at_point always returns window 0, so without the gate the
    // FFM event would shift focus to window 0; with the gate it should not.
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
        Event::MouseMoved {
            point: CGPoint {
                x: 1010.0,
                y: 750.0,
            },
            modifiers: Modifiers::empty(),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(2, |world, _state| {
            // After MouseMoved into corner dead zone: focus should remain on window 2
            // because the corner gate suppressed the focus-follow-mouse event.
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn mouse_outside_corner_still_changes_focus() {
    use crate::events::Event;
    use crate::platform::Modifiers;
    use objc2_core_foundation::CGPoint;

    // Cursor at (500, 400), middle of the display, outside the dead zone.
    // FFM should fire normally and switch focus.
    //
    // Focus window 2 first, then move cursor away from the corner. The mock's
    // find_window_at_point always returns window 0, so FFM lands focus on
    // window 0.
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::West)),
        },
        Event::MouseMoved {
            point: CGPoint { x: 500.0, y: 400.0 },
            modifiers: Modifiers::empty(),
        },
    ];

    TestHarness::new()
        .with_windows(3)
        .on_iteration(2, |world, _state| {
            // After MouseMoved outside corner: FFM should have fired and changed focus.
            assert_focused!(world, 1);
        })
        .run(commands);
}

fn current_floating_layer(world: &mut World) -> FloatingLayer {
    let mut query = world.query::<&FloatingLayer>();
    *query
        .query(world)
        .iter()
        .find(|layer| layer.workspace_id == TEST_WORKSPACE_ID)
        .expect("active workspace has FloatingLayer")
}

#[test]
fn toggle_floating_layer_tracks_focused_tier() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToggleFloatingLayer),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleFloatingLayer),
        },
    ];

    TestHarness::new()
        .with_config(Config::default())
        .with_windows(2)
        .on_iteration(0, |world, _state| {
            let floating = find_window_entity(1, world);
            world.entity_mut(floating).insert(Unmanaged::Floating);
            assert!(!current_floating_layer(world).front);
        })
        .on_iteration(1, |world, _state| {
            assert!(current_floating_layer(world).front);
            assert_focused!(world, 1);
        })
        .on_iteration(2, |world, _state| {
            assert!(!current_floating_layer(world).front);
            assert_focused!(world, 0);
        })
        .run(commands);
}

#[test]
fn toggle_floating_layer_uses_nearest_tiled_window_after_floating_focus() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Nth(2))),
        },
        Event::Command {
            command: Command::Window(Operation::Manage),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleFloatingLayer),
        },
    ];

    TestHarness::new()
        .with_config(Config::default())
        .with_windows(5)
        .on_iteration(1, |world, _state| assert_focused!(world, 2))
        .on_iteration(2, |world, _state| {
            let floating = find_window_entity(2, world);
            assert!(matches!(
                world.get::<Unmanaged>(floating),
                Some(Unmanaged::Floating)
            ));
            assert_focused!(world, 2);
        })
        .on_iteration(3, |world, _state| {
            assert!(!current_floating_layer(world).front);
            assert_focused!(world, 3);
        })
        .run(commands);
}

#[test]
fn toggle_floating_layer_uses_mouse_refocused_tiled_window() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToggleFloatingLayer),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::ToggleFloatingLayer),
        },
    ];

    TestHarness::new()
        .with_config(Config::default())
        .with_windows(2)
        .on_iteration(0, |world, _state| {
            let floating = find_window_entity(1, world);
            world.entity_mut(floating).insert(Unmanaged::Floating);
        })
        .on_iteration(1, |world, state| {
            assert_focused!(world, 1);
            assert!(current_floating_layer(world).front);

            // Simulate clicking the tiled window after the command focused the
            // floating tier. The queued OS focus event is processed during the
            // following no-op command.
            state.focus_window(0);
        })
        .on_iteration(2, |world, _state| {
            assert_focused!(world, 0);
            assert!(current_floating_layer(world).front);
        })
        .on_iteration(3, |world, _state| {
            // One toggle is enough to return to the floating tier even though
            // the stored layer state was already `front`.
            assert_focused!(world, 1);
            assert!(current_floating_layer(world).front);
        })
        .run(commands);
}

#[test]
fn test_unfloat_after_virtual_switch_uses_active_workspace() {
    let commands = vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Manage),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Command {
            command: Command::Window(Operation::Manage),
        },
    ];

    TestHarness::new()
        .with_windows(2)
        .on_iteration(3, |world, _state| {
            let entity = find_window_entity(0, world);
            let mut query = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
            let strip = query.single(world).expect("an active virtual workspace");

            assert_eq!(strip.virtual_index, 1);
            assert!(strip.contains(entity));
        })
        .run(commands);
}

#[test]
fn ordinary_float_remains_available_across_virtual_rows() {
    let mut harness = TestHarness::new().with_windows(3).with_focused_window(0);
    harness.run(vec![
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::Nth(2))),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::First)),
        },
        Event::Command {
            command: Command::Window(Operation::Manage),
        },
    ]);
    let original_frame = window_frame(harness.world(), 0);

    // Both rows have a tiled window. The ordinary float is shared by the native
    // Space: switching rows must neither move it nor exclude it from commands.
    for (virtual_index, tiled_window) in [(1, 2), (0, 1)] {
        harness.run(vec![Event::Command {
            command: Command::Window(Operation::VirtualNumber(virtual_index)),
        }]);
        assert_eq!(window_frame(harness.world(), 0), original_frame);

        for operation in [
            Operation::CycleFloating(false),
            Operation::CycleFloating(true),
            Operation::FocusUnmanaged,
            Operation::RaiseFloating,
            Operation::ToggleFloatingLayer,
        ] {
            harness.run(vec![Event::Command {
                command: Command::Window(Operation::FocusManaged),
            }]);
            assert_focused!(harness.world(), tiled_window);
            harness.run(vec![Event::Command {
                command: Command::Window(operation),
            }]);
            assert_focused!(harness.world(), 0);
            assert_eq!(window_frame(harness.world(), 0), original_frame);
            let world = harness.world();
            let mut strips = world.query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>();
            assert_eq!(strips.single(world).unwrap().virtual_index, virtual_index);
        }
    }
}

#[test]
fn focus_unmanaged_ignores_floats_from_other_workspaces() {
    let workspaces = vec![TEST_WORKSPACE_ID, TEST_WORKSPACE_ID + 1];
    let harness = TestHarness::new()
        .with_display(
            TEST_DISPLAY_ID,
            IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT),
            workspaces,
        )
        .with_workspace_window(0, TEST_WORKSPACE_ID, |_| {})
        .with_workspace_window(99, TEST_WORKSPACE_ID + 1, |w| {
            w.frame = IRect::new(600, 0, 600 + TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT);
        });

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::FocusUnmanaged),
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    harness
        .on_iteration(2, |world, _state| {
            let off_workspace_float = find_window_entity(99, world);
            world
                .entity_mut(off_workspace_float)
                .insert(Unmanaged::Floating);
            assert_focused!(world, 0);
        })
        .on_iteration(3, |world, _state| {
            let active_float = find_window_entity(0, world);
            world.entity_mut(active_float).insert(Unmanaged::Floating);
            assert_focused!(world, 0);
        })
        .on_iteration(4, |world, _state| {
            assert_focused!(world, 0);
        })
        .run(commands);
}

/// `window_swap_*` on a focused floating window moves it by `float_move_step`
/// of the viewport (`command_move_floating`) and clamps it to the viewport
/// edges. The tiled strip is untouched.
fn window_frame(world: &mut World, id: i32) -> IRect {
    let mut query = world.query::<&Window>();
    query
        .iter(world)
        .find(|w| w.id() == id)
        .expect("window not found")
        .frame()
}

#[test]
fn test_swap_moves_focused_floating_window() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        }, // 1: float window 0
        Event::Command {
            command: Command::PrintState,
        }, // 2: capture settled frames
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::East)),
        }, // 3
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::West)),
        }, // 4
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::West)),
        }, // 5: clamped at the left edge
        Event::Command {
            command: Command::Window(Operation::Swap(Direction::North)),
        }, // 6: clamped below the menubar
    ];

    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            float_move_step: Some(0.25),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let dx = TEST_DISPLAY_WIDTH / 4;
    let float_start = Rc::new(Cell::new(IRect::default()));
    let tiled_start = Rc::new(Cell::new(IRect::default()));

    let capture_float = float_start.clone();
    let capture_tiled = tiled_start.clone();
    let east_float = float_start.clone();
    let west_float = float_start.clone();
    let clamp_float = float_start.clone();
    let final_float = float_start.clone();
    let final_tiled = tiled_start.clone();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(1, |world, _state| {
            let entity = find_window_entity(0, world);
            world.entity_mut(entity).insert(Unmanaged::Floating);
        })
        .on_iteration(2, move |world, _state| {
            let frame = window_frame(world, 0);
            // The moves below assume the float starts near the top-left
            // corner (where the unmanaged trigger settles it).
            assert!(frame.min.x < dx, "float starts left of one step");
            capture_float.set(frame);
            capture_tiled.set(window_frame(world, 1));
        })
        .on_iteration(3, move |world, _state| {
            let start = east_float.get();
            assert_window_at!(world, 0, start.min.x + dx, start.min.y);
        })
        .on_iteration(4, move |world, _state| {
            let start = west_float.get();
            assert_window_at!(world, 0, start.min.x, start.min.y);
        })
        .on_iteration(5, move |world, _state| {
            let start = clamp_float.get();
            assert_window_at!(world, 0, 0, start.min.y);
        })
        .on_iteration(6, move |world, _state| {
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            let tiled = final_tiled.get();
            assert_window_at!(world, 1, tiled.min.x, tiled.min.y);
            let float = final_float.get();
            assert_window_size!(world, 0, float.width(), float.height());
            assert_focused!(world, 0);
        })
        .run(commands);
}

/// The dedicated `window_movefloat_*` keybinds only act on floating windows:
/// on a tiled window nothing moves (no swap either), on a floating window
/// they move it by `float_move_step` like `window_swap_*` does.
#[test]
fn test_movefloat_dedicated_keybind() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::MoveFloating(Direction::East)),
        }, // 1: tiled focused window — no-op
        Event::Command {
            command: Command::PrintState,
        }, // 2: float window 0
        Event::Command {
            command: Command::Window(Operation::MoveFloating(Direction::East)),
        }, // 3: floating window moves one step
    ];

    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            float_move_step: Some(0.25),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let dx = TEST_DISPLAY_WIDTH / 4;
    let float_start = Rc::new(Cell::new(IRect::default()));
    let tiled_start = Rc::new(Cell::new(IRect::default()));
    let capture_float = float_start.clone();
    let capture_tiled = tiled_start.clone();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(1, move |world, _state| {
            // Tiled windows are untouched: neither moved nor swapped.
            assert_window_at!(world, 0, 0, TEST_MENUBAR_HEIGHT);
            assert_window_at!(world, 1, TEST_WINDOW_WIDTH, TEST_MENUBAR_HEIGHT);

            let entity = find_window_entity(0, world);
            world.entity_mut(entity).insert(Unmanaged::Floating);
        })
        .on_iteration(2, move |world, _state| {
            // Floating window 0 pops off the corner and the strip reshuffles
            // around the departure; capture wherever both settled.
            capture_float.set(window_frame(world, 0));
            capture_tiled.set(window_frame(world, 1));
        })
        .on_iteration(3, move |world, _state| {
            let start = float_start.get();
            assert_window_at!(world, 0, start.min.x + dx, start.min.y);
            let tiled = tiled_start.get();
            assert_window_at!(world, 1, tiled.min.x, tiled.min.y);
            assert_focused!(world, 0);
        })
        .run(commands);
}

/// `window_cyclefloat` rotates focus through the visible floating windows in
/// window-ID order, wrapping around; the reverse variant rotates backwards.
/// Tiled windows are never part of the rotation.
#[test]
fn test_cyclefloat_rotates_through_floating_windows() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        }, // 1: float windows 0 and 1; window 2 stays tiled
        Event::Command {
            command: Command::Window(Operation::CycleFloating(false)),
        }, // 2: 0 -> 1
        Event::Command {
            command: Command::Window(Operation::CycleFloating(false)),
        }, // 3: 1 -> wraps to 0
        Event::Command {
            command: Command::Window(Operation::CycleFloating(true)),
        }, // 4: 0 -> wraps back to 1
    ];

    TestHarness::new()
        .with_config(Config::default())
        .with_windows(3)
        .on_iteration(1, |world, _state| {
            for id in [0, 1] {
                let entity = find_window_entity(id, world);
                world.entity_mut(entity).insert(Unmanaged::Floating);
            }
            assert_focused!(world, 0);
        })
        .on_iteration(2, |world, _state| {
            assert_focused!(world, 1);
        })
        .on_iteration(3, |world, _state| {
            assert_focused!(world, 0);
        })
        .on_iteration(4, |world, _state| {
            assert_focused!(world, 1);
        })
        .run(commands);
}

/// With `insert_windows_mid_strip` enabled, following a window into another
/// virtual workspace keeps it at its exact on-screen x — even when the
/// destination strip is scrolled and not grid-aligned. The rest of the strip
/// shifts to make room.
#[test]
fn test_mid_strip_insertion_preserves_window_x() {
    let config: Config = (
        MainOptions {
            insert_windows_mid_strip: Some(true),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let harness = TestHarness::new().with_config(config).with_windows(8);

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // Build VW1 with four windows (scrollable), leaving four on VW0.
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        // Scroll VW0 slightly to randomize the positions. "Slightly" is the
        // point: a single event's delta is viewport-fractions travelled in one
        // frame, so the velocity it yields is `delta / dt`. Anything much
        // larger throws the strip into its clamp bound, which parks the
        // focused window at an edge sliver and makes the offset compared below
        // that fixed sliver rather than a real layout position.
        Event::Swipe {
            delta: 0.06,
            fingers: 3,
        },
        // Used as a noop to let the scroll settle.
        Event::MenuOpened { window_id: 0 },
        // Change to VW1 and scroll it slightly as well.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        Event::Swipe {
            delta: 0.04,
            fingers: 3,
        },
        Event::MenuOpened { window_id: 0 },
        // Change back to VW0 and send one window over to VW1.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let previous_offset = std::rc::Rc::new(std::cell::RefCell::new(0));
    let previous_offset2 = previous_offset.clone();
    harness
        .on_iteration(10, move |world, _state| {
            let mut q =
                world.query_filtered::<(&Window, &Position), With<crate::ecs::FocusedMarker>>();
            let (_, position) = q.single(world).expect("a focused window");

            previous_offset.replace(position.x);
            assert_ne!(position.x, 0);
        })
        .on_iteration(11, move |world, _state| {
            let mut q =
                world.query_filtered::<(&Window, &Position), With<crate::ecs::FocusedMarker>>();
            let (_, position) = q.single(world).expect("a focused window");

            assert_eq!(position.x, previous_offset2.take());
        })
        .run(commands);
}

/// Without the flag (the default), a moved window is appended to the end of the
/// destination strip, preserving arrival order.
#[test]
fn test_move_appends_to_end_by_default() {
    let mut h = TestHarness::new().with_windows(3);

    let pump = |h: &mut TestHarness, cmd: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: cmd });
        for _ in 0..6 {
            h.app.update();
            for event in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(event);
            }
        }
    };

    // Seed VW1 with one window, keeping us on VW0.
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );

    // Whatever window is focused now is the one the follow-move will carry.
    let mover = focused_window_id(h.app.world_mut());

    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
    );

    // Default behaviour: the moved window is appended, i.e. it is the last
    // column of the (now active) destination strip.
    let last = {
        let world = h.app.world_mut();
        let mut q = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
        let entity = q
            .iter(world)
            .find_map(|(s, a)| a.then(|| s.all_windows()))
            .and_then(|windows| windows.last().copied())
            .expect("active strip with windows");
        let mut wq = world.query::<(Entity, &Window)>();
        wq.iter(world)
            .find_map(|(ent, w)| (ent == entity).then_some(w.id()))
            .expect("window id")
    };
    assert_eq!(
        last, mover,
        "default move should append to the end of the strip"
    );
}

/// A follow-move that appends the window to an already-populated destination
/// strip must bring it fully on-screen. Regression test: the moved window
/// keeps focus, so no `Added<FocusedMarker>` fires to trigger the reshuffle,
/// and it used to land off the right edge until manually centered.
#[test]
fn test_follow_move_brings_appended_window_on_screen() {
    // Enough windows that the destination strip overflows the display width
    // (each window is 400px wide, display is 1024px), so an appended window
    // lands off the right edge unless the strip scrolls to expose it.
    let mut h = TestHarness::new().with_windows(5);

    let pump = |h: &mut TestHarness, cmd: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: cmd });
        for _ in 0..8 {
            h.app.update();
            for event in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(event);
            }
        }
    };

    // Seed VW1 with three windows (Stay keeps us on VW0), making the
    // destination strip wider than the display before the follow-move appends
    // to it.
    for _ in 0..3 {
        pump(
            &mut h,
            Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        );
    }

    let mover = focused_window_id(h.app.world_mut());

    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
    );

    assert_focused!(h.app.world_mut(), mover);
    let frame = {
        let world = h.app.world_mut();
        let mut q = world.query::<&Window>();
        q.iter(world)
            .find(|w| w.id() == mover)
            .expect("moved window")
            .frame()
    };
    assert!(
        frame.min.x >= 0 && frame.max.x <= TEST_DISPLAY_WIDTH,
        "moved window must be fully on-screen, got frame x {}..{} (display width {})",
        frame.min.x,
        frame.max.x,
        TEST_DISPLAY_WIDTH,
    );
}

/// With `insert_windows_mid_strip` enabled and a smooth `animation_speed`, moving
/// a window to another virtual workspace must not animate: every window snaps to
/// its final spot. Checked per-update, since markers created and consumed
/// mid-move would be invisible to a settle-then-check.
#[test]
fn test_mid_strip_move_does_not_animate() {
    let config: Config = (
        MainOptions {
            insert_windows_mid_strip: Some(true),
            animation_speed: Some(12.0),
            virtual_workspace_animations: Some(false),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(8);
    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    // Build a scrolled VW1 and scroll VW0 too, so the move is off the grid.
    for _ in 0..4 {
        pump(
            &mut h,
            Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        );
    }
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));
    h.app.world_mut().write_message::<Event>(Event::Swipe {
        delta: 0.3,
        fingers: 3,
    });
    for _ in 0..6 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }
    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));
    h.app.world_mut().write_message::<Event>(Event::Swipe {
        delta: 0.3,
        fingers: 3,
    });
    for _ in 0..6 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    // Follow-move into the existing VW1, checking every update for animation.
    h.app.world_mut().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
    });
    for step in 0..10 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Window, With<RepositionMarker>>();
        let animating: Vec<i32> = q.iter(world).map(|w| w.id()).collect();
        assert!(
            animating.is_empty(),
            "step {step}: no window should animate during a mid-strip move, got {animating:?}",
        );
    }
}

/// Switching virtual workspaces with `virtual_workspace_animations = false` must
/// never cause the active strip to slide left or right. The strip position must
/// snap directly to its saved scroll position without any `RepositionMarker`
/// animating it further. Regression test: after VW2 → VW1, a stale
/// `reshuffle_layout_strip` was computing an incorrect strip target from
/// un-updated window positions, inserting a `RepositionMarker` that animated
/// the strip sideways.
///
/// Setup: VW0 has 5 windows (scrollable), scrolled so the focused window sits
/// at a non-zero strip offset. We then switch to VW1 and back to VW0, and
/// verify that no `RepositionMarker` is ever placed on the strip (which would
/// cause horizontal sliding).
#[test]
fn test_virtual_workspace_switch_no_horizontal_slide_no_animations() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows: strip width = 5 * 400 = 2000, display = 1024 → scrollable.
    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump_event = |h: &mut TestHarness, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    let pump = |h: &mut TestHarness, c: Command| pump_event(h, Event::Command { command: c });

    // Boot the strip and focus window 0.
    pump(&mut h, Command::PrintState);

    // Scroll the strip so it sits at a non-zero x offset.
    pump_event(
        &mut h,
        Event::Swipe {
            delta: 0.3,
            fingers: 3,
        },
    );

    // Remember the settled strip x after scrolling.
    let strip_x_after_scroll = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world)
            .expect("exactly one active strip after scroll")
            .0
            .x
    };
    assert_ne!(
        strip_x_after_scroll, 0,
        "test setup: strip should be scrolled to a non-zero position, got 0"
    );

    // Switch to VW1 (spawned on the fly, empty).
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    // Switch back to VW0. This is where the bug triggers: the strip should
    // snap to `strip_x_after_scroll` with no RepositionMarker causing further
    // horizontal motion.
    h.app.world_mut().write_message::<Event>(Event::Command {
        command: Command::Window(Operation::VirtualNumber(0)),
    });

    // After the VW switch back to VW0, pump frames and assert the active strip's
    // x position settles exactly at the pre-switch scroll value. Any deviation
    // means a stale reshuffle slid the strip sideways.
    for _ in 0..10 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    let strip_x_final = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world)
            .expect("exactly one active strip after switch-back")
            .0
            .x
    };

    assert_eq!(
        strip_x_final, strip_x_after_scroll,
        "strip x must equal the pre-switch scroll position after VW switch-back (no sideways slide). \
         Expected {strip_x_after_scroll}, got {strip_x_final}"
    );
}

/// Regression: `show_active_workspace` defers the "expose the arriving
/// focus window" correction to `ensure_visible_in_strip` because that
/// system's own `is_added(ActiveWorkspaceMarker)` guard would otherwise skip
/// it on the very tick it's needed. By the time it actually runs (one tick
/// later), `is_added` is no longer true, so without the `snap` flag it fell
/// back to always animating — sliding the whole strip (everything in it,
/// stacked or not) into place even with `virtual_workspace_animations =
/// false`. This exercises `ensure_visible_in_strip` directly (via the same
/// `EnsureVisibleMarker { snap }` `show_active_workspace` inserts) rather
/// than reproducing the full VW-restore choreography, since only that one
/// system's snap-vs-animate decision is under test here.
#[test]
fn test_ensure_visible_snap_does_not_animate_with_animations_off() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows @ 400px = 2000px strip on a 1024px display → scrollable, so
    // window 4 sits off the right edge at scroll position 0.
    let mut h = TestHarness::new().with_config(config).with_windows(5);
    h.app.world_mut().write_message::<Event>(Event::Command {
        command: Command::PrintState,
    });
    for _ in 0..8 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    let off_screen_window = find_window_entity(4, h.app.world_mut());
    h.app
        .world_mut()
        .entity_mut(off_screen_window)
        .insert(crate::ecs::EnsureVisibleMarker { snap: true });

    for step in 0..10 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<Entity, (With<LayoutStrip>, With<RepositionMarker>)>();
        assert!(
            q.iter(world).next().is_none(),
            "step {step}: strip must never animate when EnsureVisibleMarker::snap is true \
             and virtual_workspace_animations is false"
        );
    }

    let world = h.app.world_mut();
    let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
    let strip_x = q.single(world).expect("exactly one active strip").0.x;
    assert_ne!(
        strip_x, 0,
        "test setup: the strip must actually have scrolled to expose window 4"
    );
}

/// Companion regression: the *ordinary* (non-restore) `ensure_visible` path
/// — the one every other caller uses — must keep animating exactly as
/// before. This is the guard against a fix for the case above accidentally
/// making every scroll-to-reveal instant.
#[test]
fn test_ensure_visible_without_snap_still_animates() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            animation_speed: Some(0.5),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(5);
    h.app.world_mut().write_message::<Event>(Event::Command {
        command: Command::PrintState,
    });
    for _ in 0..8 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    let off_screen_window = find_window_entity(4, h.app.world_mut());
    h.app
        .world_mut()
        .entity_mut(off_screen_window)
        .insert(crate::ecs::EnsureVisibleMarker { snap: false });

    h.app.update();
    for e in h.mock_state.drain_events() {
        h.app.world_mut().write_message::<Event>(e);
    }

    let world = h.app.world_mut();
    let mut q = world.query_filtered::<Entity, (With<LayoutStrip>, With<RepositionMarker>)>();
    assert!(
        q.iter(world).next().is_some(),
        "an ordinary (non-restore) ensure_visible correction must still animate, \
         regardless of virtual_workspace_animations"
    );
}

/// Regression: `position_layout_windows`'s offscreen/parking magnitude
/// heuristic has no way to know a virtual-workspace restore is in progress.
/// A member window whose last position differs from its recomputed target
/// by less than the "offscreen" distance (and isn't at the parked corner
/// either) gets animated by the ordinary layout-change path even with
/// `virtual_workspace_animations = false`, because nothing about the move
/// looks large enough to be restore-driven. This happens for real: a lower
/// stack member parked while its strip was hidden can land at a Y just
/// under both thresholds. `SnapStripMarker` closes the gap by naming the
/// strip explicitly, rather than inferring "was this restore-driven?" from
/// move magnitude. Reproduces the mechanism directly (perturb + retrigger),
/// since replicating the exact real-world "parked just under threshold"
/// numbers through natural VW-switch parking isn't reliable in the mock
/// harness.
#[test]
fn test_snap_strip_marker_forces_snap_for_under_threshold_move() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(3);
    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    pump(&mut h, Command::PrintState);
    pump(&mut h, Command::Window(Operation::Focus(Direction::East)));
    pump(&mut h, Command::Window(Operation::Stack(true)));
    // Let the stack's own build-out animation fully settle before
    // perturbing anything, so the "before" position is a true resting
    // state, not a value still mid-transit.
    for _ in 0..20 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    let stack_member = find_window_entity(1, h.app.world_mut());
    let strip_entity = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<Entity, With<ActiveWorkspaceMarker>>();
        q.single(world).expect("exactly one active strip")
    };
    assert!(
        h.app
            .world_mut()
            .get::<RepositionMarker>(stack_member)
            .is_none(),
        "test setup: the stack must have fully settled before perturbing it"
    );

    // Perturb the stack member well under both the parking threshold and
    // the 80%-of-viewport "offscreen" distance (748 * 0.8 ~= 598 in this
    // harness), spawn the guard, then re-touch the strip's own Position -
    // the same trigger `show_active_workspace` uses on a restore.
    h.app
        .world_mut()
        .get_mut::<Position>(stack_member)
        .expect("stack member has a Position")
        .0
        .y -= 300;
    h.app
        .world_mut()
        .spawn(crate::ecs::workspace::SnapStripMarker {
            strip: strip_entity,
        });
    h.app
        .world_mut()
        .get_mut::<Position>(strip_entity)
        .expect("strip has a Position")
        .set_changed();

    for _ in 0..5 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    assert!(
        h.app
            .world_mut()
            .get::<RepositionMarker>(stack_member)
            .is_none(),
        "a strip named by a live SnapStripMarker must snap its members directly, not animate"
    );
}

/// Companion regression: the same under-threshold perturbation, without a
/// `SnapStripMarker`, must still animate exactly as before — the guard from
/// the test above is name-scoped to the strip, not a blanket behavior
/// change to `position_layout_windows`.
#[test]
fn test_under_threshold_move_animates_without_snap_strip_marker() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(3);
    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    pump(&mut h, Command::PrintState);
    pump(&mut h, Command::Window(Operation::Focus(Direction::East)));
    pump(&mut h, Command::Window(Operation::Stack(true)));
    // Let the stack's own build-out animation fully settle before
    // perturbing anything, so the "before" position is a true resting
    // state, not a value still mid-transit.
    for _ in 0..20 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    let stack_member = find_window_entity(1, h.app.world_mut());
    let strip_entity = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<Entity, With<ActiveWorkspaceMarker>>();
        q.single(world).expect("exactly one active strip")
    };
    assert!(
        h.app
            .world_mut()
            .get::<RepositionMarker>(stack_member)
            .is_none(),
        "test setup: the stack must have fully settled before perturbing it"
    );

    h.app
        .world_mut()
        .get_mut::<Position>(stack_member)
        .expect("stack member has a Position")
        .0
        .y -= 300;
    h.app
        .world_mut()
        .get_mut::<Position>(strip_entity)
        .expect("strip has a Position")
        .set_changed();

    let mut saw_reposition_marker = false;
    for _ in 0..5 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
        if h.app
            .world_mut()
            .get::<RepositionMarker>(stack_member)
            .is_some()
        {
            saw_reposition_marker = true;
            break;
        }
    }

    assert!(
        saw_reposition_marker,
        "without a SnapStripMarker, the under-threshold move must still animate"
    );
}

/// Switching virtual workspaces with `virtual_workspace_animations = false`
/// must switch focus to the focused window of the destination workspace.
#[test]
fn test_virtual_workspace_switch_restores_focus_without_animations() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(3);

    let pump_event = |h: &mut TestHarness, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    let pump = |h: &mut TestHarness, c: Command| pump_event(h, Event::Command { command: c });

    // Boot: Window 0 is focused on VW0 (workspace_virtual_num = 0).
    pump(&mut h, Command::PrintState);

    // Move focused window (Window 0) to VW1 with MoveFocus::Stay.
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );

    // Focus on VW0 should have shifted to Window 1.
    let focused_on_vw0 = {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<FocusedMarker>>();
        query.iter(world).next().map(|w| w.id())
    };
    assert_eq!(
        focused_on_vw0,
        Some(1),
        "focus should remain on VW0 (shifting to Window 1) after MoveFocus::Stay"
    );

    // Switch to VW1: Window 0 (the window on VW1) should now be focused.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    let focused_on_vw1 = {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<FocusedMarker>>();
        query.iter(world).next().map(|w| w.id())
    };
    assert_eq!(
        focused_on_vw1,
        Some(0),
        "focus should switch to Window 0 when activating VW1 with animations disabled"
    );

    // Switch back to VW0: Window 1 (the window on VW0) should be focused again.
    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));

    let focused_back_on_vw0 = {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<FocusedMarker>>();
        query.iter(world).next().map(|w| w.id())
    };
    assert_eq!(
        focused_back_on_vw0,
        Some(1),
        "focus should switch back to Window 1 when activating VW0 with animations disabled"
    );
}

/// Switching virtual workspaces with `virtual_workspace_animations = true`
/// must switch focus to the remembered window of the destination workspace
/// and spawn exactly one focus guard.
#[test]
fn test_virtual_workspace_switch_restores_focus_with_animations() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(true),
            animation_speed: Some(30.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(3);

    let pump_event = |h: &mut TestHarness, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    let pump = |h: &mut TestHarness, c: Command| pump_event(h, Event::Command { command: c });

    // Boot: Window 0 is focused on VW0.
    pump(&mut h, Command::PrintState);

    // Move Window 0 to VW1 with MoveFocus::Stay.
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );

    // Focus on VW0 shifted to Window 1.
    let focused_on_vw0 = {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<FocusedMarker>>();
        query.iter(world).next().map(|w| w.id())
    };
    assert_eq!(focused_on_vw0, Some(1));

    // Switch to VW1: Window 0 should be focused and exactly one guard spawned.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    let (focused_on_vw1, guard_count) = {
        let world = h.app.world_mut();
        let mut query = world.query_filtered::<&crate::manager::Window, With<FocusedMarker>>();
        let focused = query.iter(world).next().map(|w| w.id());
        let mut guards = world.query::<&crate::ecs::workspace::RestoreFocusMarker>();
        let count = guards.iter(world).count();
        (focused, count)
    };
    assert_eq!(
        focused_on_vw1,
        Some(0),
        "focus should switch to Window 0 when activating VW1 with animations enabled"
    );
    assert_eq!(
        guard_count, 1,
        "exactly one restore focus guard should be spawned during workspace switch"
    );
}

/// When a strip is mid-animation (has a `RepositionMarker`) at the moment the
/// user switches to another virtual workspace, the animation must stop
/// immediately. Previously the `RepositionMarker` was left on the hidden strip
/// so `animate_entities` kept updating its position while it was off-screen,
/// making the two strips briefly visible at the same time (the hidden one still
/// sliding) and corrupting the saved restore position.
#[test]
fn test_virtual_workspace_switch_stops_in_flight_strip_animation() {
    let config: Config = (
        MainOptions {
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump_n = |h: &mut TestHarness, n: usize, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..n {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    let pump = |h: &mut TestHarness, c: Command| {
        pump_n(h, 8, Event::Command { command: c });
    };

    pump(&mut h, Command::PrintState);

    // Scroll and then switch to VW1 mid-animation (only 1 frame so animation
    // is still in progress when the switch fires).
    h.app.world_mut().write_message::<Event>(Event::Swipe {
        delta: 0.3,
        fingers: 3,
    });
    // One frame to start the animation.
    h.app.update();
    for e in h.mock_state.drain_events() {
        h.app.world_mut().write_message::<Event>(e);
    }

    // Switch to VW1 while the strip may still have a RepositionMarker.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    // VW0's strip must have no RepositionMarker (animation stopped on hide).
    {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<Entity, (
            With<crate::ecs::layout::LayoutStrip>,
            Without<ActiveWorkspaceMarker>,
        )>();
        for entity in q.iter(world) {
            assert!(
                world.get::<RepositionMarker>(entity).is_none(),
                "hidden strip {entity:?} must not have RepositionMarker after VW switch"
            );
        }
    }

    // Switch back to VW0. The strip should restore to the saved position,
    // not to wherever the mid-flight animation would have taken it.
    let saved_x = {
        // The saved position is snapped to what it was at switch time; just
        // record where VW0's strip ends up after restoring.
        h.app.world_mut().write_message::<Event>(Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        });
        for _ in 0..10 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world)
            .expect("exactly one active strip after restore")
            .0
            .x
    };

    // After restoring the strip must also have no RepositionMarker — it
    // should have snapped directly, not started a new animation.
    {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<Entity, (
            With<crate::ecs::layout::LayoutStrip>,
            With<ActiveWorkspaceMarker>,
        )>();
        for entity in q.iter(world) {
            assert!(
                world.get::<RepositionMarker>(entity).is_none(),
                "restored strip {entity:?} must not have RepositionMarker (no animation after no-anim VW switch)"
            );
        }
    }

    // The final position must be stable (no further drift).
    for _ in 0..5 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }
    let world = h.app.world_mut();
    let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
    let final_x = q.single(world).expect("exactly one active strip").0.x;
    assert_eq!(
        final_x, saved_x,
        "strip x drifted after restore: was {saved_x}, now {final_x}"
    );
}

/// A virtual-workspace switch restores the strip's saved scroll position and
/// refocuses the remembered window. macOS acknowledges that focus several
/// ticks later, after `reshuffle_layout_strip`'s guards have expired; with
/// `auto_center` on, that late acknowledgment used to re-center the strip and
/// undo the restore — a "wiggle" on every switch.
#[test]
fn test_virtual_workspace_switch_focus_echo_does_not_recenter_strip() {
    let config: Config = (
        MainOptions {
            auto_center: Some(true),
            virtual_workspace_animations: Some(false),
            animation_speed: Some(12.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(6);

    // A second of simulated time, which is what the auto-centre animation
    // needs to reach its target. Expressed as a duration rather than a frame
    // count so it stays a second whatever the harness tick is: a settle that
    // stops short leaves an animation still in flight, and the displacement
    // below then gets overwritten by the tail of it.
    let settle = |h: &mut TestHarness| h.advance(Duration::from_secs(1));
    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        settle(h);
    };
    let strip_x = |h: &mut TestHarness| -> i32 {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world).expect("exactly one active strip").0.x
    };

    pump(&mut h, Command::PrintState);

    // Seed VW1 with one window so focus genuinely moves across the switch.
    h.mock_state.focus_window(5);
    settle(&mut h);
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );

    // Focus window 0 and let auto-center settle the strip on it.
    h.mock_state.focus_window(0);
    settle(&mut h);
    let centered_x = strip_x(&mut h);

    // Displace the strip directly instead of swiping: the swipe pipeline's
    // finger-lift threshold is wall-clock based, which makes event order
    // load-dependent and the test flaky.
    let saved_x = centered_x - 250;
    {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&mut Position, With<ActiveWorkspaceMarker>>();
        q.single_mut(world).expect("exactly one active strip").0.x = saved_x;
    }
    settle(&mut h);
    assert_eq!(
        strip_x(&mut h),
        saved_x,
        "test setup: the displaced strip position must stick"
    );

    // Switch to VW1; deliver the OS focus acknowledgment for its window.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));
    h.mock_state.focus_window(5);
    settle(&mut h);

    // Switch back to VW0.
    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));
    assert_eq!(
        strip_x(&mut h),
        saved_x,
        "strip must restore to its saved position on switch-back"
    );

    // Delayed focus acknowledgment for the remembered window.
    h.mock_state.focus_window(0);
    settle(&mut h);
    let final_x = strip_x(&mut h);
    assert_eq!(
        final_x, saved_x,
        "late focus acknowledgment re-centered the strip (wiggle): \
         restored to {saved_x}, ended at {final_x} (centered would be {centered_x})"
    );
}

/// With `auto_center` off, a reshuffle around the leftmost window of a
/// scrollable strip must pin the strip to the left edge — the leftmost
/// window's left edge must touch the display's left edge, never leaving empty
/// space to its left.
///
/// Regression: after a virtual-workspace switch parks the inactive strip at
/// `bounds.max - 10`, every window's on-screen frame is momentarily stale at
/// the right-edge sliver. A focus-driven `reshuffle_layout_strip` that read
/// that stale frame computed a large positive strip offset and pushed column 0
/// away from the left edge (leftmost window ended up right-aligned). This test
/// injects the stale right-edge frame directly (the real trigger is a delayed
/// duplicate OS focus event that the mock platform doesn't emit) and asserts
/// the reshuffle clamps the strip back to the left edge.
#[test]
fn test_reshuffle_leftmost_pins_strip_to_left_edge_with_stale_frame() {
    use crate::ecs::{Position, ReshuffleAroundMarker};

    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            animation_speed: Some(30.0),
            continuous_swipe: Some(false),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows @ 400px = 2000px strip on a 1024px display → scrollable.
    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..10 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    // Boot the strip; column 0 (window id 0) sits at layout x 0.
    pump(&mut h, Command::PrintState);

    let leftmost = find_window_entity(0, h.app.world_mut());

    // Simulate the stale post-VW-switch state: the leftmost window's on-screen
    // frame is parked at the right-edge sliver while its layout position is
    // still 0. Clear any in-flight animation so moving_frame reads the origin.
    {
        let world = h.app.world_mut();
        if let Ok(mut e) = world.get_entity_mut(leftmost) {
            e.insert(Position(Origin::new(
                TEST_DISPLAY_WIDTH - 5,
                TEST_MENUBAR_HEIGHT,
            )));
            e.remove::<RepositionMarker>();
            // Trigger a reshuffle around the leftmost window, as focus would.
            e.insert(ReshuffleAroundMarker);
        }
    }

    for _ in 0..15 {
        h.app.update();
        for e in h.mock_state.drain_events() {
            h.app.world_mut().write_message::<Event>(e);
        }
    }

    // The strip must be pinned to the left edge (offset 0): column 0 has
    // layout x 0, so its on-screen left edge lands at the display's left edge.
    let world = h.app.world_mut();
    let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
    let strip_x = q.single(world).expect("exactly one active strip").0.x;
    assert_eq!(
        strip_x, 0,
        "reshuffle around leftmost window must pin strip to left edge (offset 0), got {strip_x}"
    );
}

/// With `virtual_workspace_animations = true`, switching away from a scrolled
/// strip and back must restore its saved scroll position, not reset it.
///
/// Regression: the animated restore branch of `show_active_workspace` called
/// `reshuffle_around(focus)` in addition to animating the strip to its saved
/// origin. That reshuffle read stale mid-animation window frames a frame later
/// and overwrote the restore target with a different offset, discarding the
/// saved scroll (the strip jumped back to 0). The animated branch now restores
/// the origin without reshuffling, mirroring the non-animated branch.
#[test]
fn test_virtual_workspace_switch_preserves_scroll_with_animations() {
    use Position;

    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            animation_speed: Some(30.0),
            swipe_gesture_fingers: Some(3),
            virtual_workspace_animations: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows @ 400px = 2000px strip on a 1024px display → scrollable.
    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump_event = |h: &mut TestHarness, ev: Event| {
        h.app.world_mut().write_message::<Event>(ev);
        for _ in 0..14 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };
    let pump = |h: &mut TestHarness, c: Command| pump_event(h, Event::Command { command: c });

    // Boot and scroll the strip off the left edge to a non-zero offset.
    pump(&mut h, Command::PrintState);
    pump_event(
        &mut h,
        Event::Swipe {
            delta: 0.4,
            fingers: 3,
        },
    );

    let strip_x_after_scroll = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world).expect("active strip after scroll").0.x
    };
    assert_ne!(
        strip_x_after_scroll, 0,
        "test setup: strip should be scrolled off the left edge, got 0"
    );

    // Switch to an empty VW and back.
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));
    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));

    let strip_x_restored = {
        let world = h.app.world_mut();
        let mut q = world.query_filtered::<&Position, With<ActiveWorkspaceMarker>>();
        q.single(world).expect("active strip after restore").0.x
    };
    assert_eq!(
        strip_x_restored, strip_x_after_scroll,
        "animated VW restore must preserve the saved scroll position. \
         Expected {strip_x_after_scroll}, got {strip_x_restored}"
    );
}

/// With `virtual_workspace_animations = true`, switching to another virtual
/// workspace must move the previously-active strip (and therefore its windows)
/// off-screen. Regression: `show_active_workspace` queued a `RepositionMarker`
/// to animate the old strip to `bounds.max - 10`, but a cleanup block right
/// after removed the very same marker in the same command flush — so the strip
/// never moved and all of the old workspace's windows stayed visible on top of
/// the workspace we switched to. The cleanup now runs before the hide
/// reposition is queued.
#[test]
fn test_virtual_workspace_switch_hides_old_strip_with_animations() {
    use crate::ecs::{Position, layout::LayoutStrip};

    let config: Config = (
        MainOptions {
            auto_center: Some(false),
            animation_speed: Some(30.0),
            swipe_gesture_fingers: Some(3),
            virtual_workspace_animations: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let mut h = TestHarness::new().with_config(config).with_windows(5);

    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..14 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    pump(&mut h, Command::PrintState);
    // Leave a window on VW0 (Stay) and spawn VW1 with one window, then switch.
    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
    );
    pump(&mut h, Command::Window(Operation::VirtualNumber(1)));

    // The inactive VW0 strip must have moved off-screen (its origin parked at
    // bounds.max - 10 = 1014), taking its windows with it.
    let world = h.app.world_mut();
    let mut q = world.query::<(&LayoutStrip, &Position, Has<ActiveWorkspaceMarker>)>();
    let inactive_x = q
        .iter(world)
        .find_map(|(_, pos, active)| (!active).then_some(pos.0.x))
        .expect("an inactive strip exists after switching workspaces");
    assert!(
        inactive_x >= TEST_DISPLAY_WIDTH - 10,
        "inactive strip must be parked off-screen (>= {}), got {inactive_x}",
        TEST_DISPLAY_WIDTH - 10
    );
}

/// Stacking or unstacking the focused window must bring it fully back into
/// view. Regression: `stack_windows_handler` mutated the strip but never
/// reshuffled, so when the strip was scrolled such that the focused window's
/// new column slot fell off-screen, the window stayed partially or fully
/// invisible even though it kept focus. It now reshuffles around the focused
/// window; the edge-clamp in `reshuffle_layout_strip` keeps the strip pinned to
/// the edges.
#[test]
fn test_stack_unstack_brings_focused_window_into_view() {
    fn check_if_offscreen(world: &mut World) {
        let mut q = world.query_filtered::<(&Window, &Position), With<crate::ecs::FocusedMarker>>();
        let (_, position) = q.single(world).expect("a focused window");

        assert!(
            position.x < -(TEST_WINDOW_WIDTH / 4),
            "focused window should be somewhat offscreen after the scroll."
        );
    }

    let config: Config = (
        MainOptions {
            focus_follows_mouse: Some(false),
            auto_center: Some(false),
            animation_speed: Some(10000.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // 5 windows @ 400px = 2000px strip on a 1024px display → scrollable.
    let mut harness = TestHarness::new().with_config(config).with_windows(5);
    harness.run(vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ]);

    // Swipe windows 0 and 1 off screen.
    perform_trackpad_swipe(&mut harness, 0.3);
    check_if_offscreen(harness.world());

    harness.run(vec![Event::Command {
        command: Command::Window(Operation::Stack(true)),
    }]);
    {
        let world = harness.world();
        assert_window_at!(world, 0, 0, 20);
        assert_window_at!(world, 1, 0, 394);
    }

    // Swipe the stacked windows off screen again.
    perform_trackpad_swipe(&mut harness, 0.1);
    check_if_offscreen(harness.world());

    harness.run(vec![Event::Command {
        command: Command::Window(Operation::Stack(false)),
    }]);
    assert_window_at!(harness.world(), 1, 0, 20);
}

/// A window parked on a hidden virtual row must stay parked when its app
/// hides and re-shows itself (e.g. 1Password self-activating periodically),
/// which runs the whole unmanage/remanage cycle unprompted. Regression: the
/// remanage path used to reshuffle around the window's popped frame, dragging
/// the hidden strip back on screen and making the window unreachable to
/// commands that only act on the active strip.
#[test]
fn test_app_self_activation_keeps_window_parked_on_hidden_virtual_row() {
    /// Position of the parked window and of the hidden strip holding it.
    fn parked_state(world: &mut World) -> (Origin, Origin) {
        let entity = find_window_entity(0, world);
        let mut strips = world.query::<(&LayoutStrip, &Position, Has<ActiveWorkspaceMarker>)>();
        let (strip_position, active) = strips
            .iter(world)
            .find_map(|(strip, position, active)| {
                (strip.virtual_index == 1 && strip.contains(entity)).then_some((position.0, active))
            })
            .expect("window 0 parked on the hidden virtual row");
        assert!(!active, "virtual row 1 must not be the active one");

        let mut windows = world.query_filtered::<&Position, With<Window>>();
        let window_position = windows.get(world, entity).expect("window 0 position").0;
        (window_position, strip_position)
    }

    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // Park the focused window on VW1 while VW0 stays on screen.
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        Event::Command {
            command: Command::PrintState,
        },
        // The app hides and re-shows itself, unmanaging and remanaging the
        // parked window.
        Event::ApplicationHidden {
            pid: TEST_PROCESS_ID,
        },
        Event::ApplicationVisible {
            pid: TEST_PROCESS_ID,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let parked = std::rc::Rc::new(std::cell::RefCell::new(None));
    let parked_after = parked.clone();

    TestHarness::new()
        .with_windows(2)
        .on_iteration(2, move |world, _state| {
            parked.replace(Some(parked_state(world)));
        })
        .on_iteration(5, move |world, _state| {
            let (window_before, strip_before) =
                parked_after.borrow().expect("parked state was captured");
            let (window_after, strip_after) = parked_state(world);

            assert_eq!(
                window_after, window_before,
                "parked window must keep its off-screen frame across the hide/show cycle"
            );
            assert_eq!(
                strip_after, strip_before,
                "hidden virtual row must not be dragged back on screen"
            );
        })
        .run(commands);
}

/// A `WindowMoved` notification for a window paneru is not currently moving is
/// the app (or the user) moving it, and the layout must take that new origin on
/// board.
#[test]
fn test_foreign_window_move_is_adopted() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let config: Config = (
        MainOptions {
            // Snappy, so no `RepositionMarker` is still in flight when the
            // notification below arrives.
            animation_speed: Some(10000.0),
            ..Default::default()
        },
        vec![],
    )
        .into();

    TestHarness::new()
        .with_config(config)
        .with_windows(2)
        .on_iteration(0, |_world, state| {
            state.os_move_window(0, Origin::new(77, 88));
        })
        .on_iteration(2, |world, _state| {
            let entity = find_window_entity(0, world);
            let position = world.get::<Position>(entity).expect("window position");
            assert_eq!(
                position.0,
                Origin::new(77, 88),
                "a move paneru did not make must be read back into the layout"
            );
        })
        .run(commands);
}

/// A `WindowMoved` echo of a move paneru itself just made must not perturb the
/// in-flight animation — reading it back naively made the animation and the
/// echo chase each other, causing jitter on every reflow.
///
/// Driven directly at the system instead of through the harness loop: the mock
/// applies its reposition synchronously, so a normal frame would resolve the
/// move before the notification could ever be read back.
#[test]
fn test_own_window_move_echo_is_ignored() {
    use bevy::ecs::system::RunSystemOnce as _;

    let mut harness = TestHarness::new().with_windows(2);
    harness.app.update();

    let state = harness.mock_state.clone();
    let world = harness.world();
    let entity = find_window_entity(0, world);
    let before = world.get::<Position>(entity).expect("window position").0;

    // A move of ours is in flight, and the app reports a frame we didn't ask
    // for. Displaced on the axis the animation leaves alone, so the assertion
    // can't be confused by how far the lerp has run.
    world
        .entity_mut(entity)
        .insert(RepositionMarker(Origin::new(5000, before.y)));
    state.os_move_window(0, Origin::new(before.x, before.y + 888));
    world.write_message(Event::WindowMoved { window_id: 0 });

    world
        .run_system_once(crate::ecs::systems::window_moved_update_frame)
        .expect("running window_moved_update_frame");

    assert_eq!(
        world.get::<Position>(entity).expect("window position").0,
        before,
        "the echo of our own move must not be read back over the animation"
    );
}

#[test]
fn test_virtual_directions_first_last_east_west() {
    use crate::config::{Config, MainOptions};

    let config: Config = (
        MainOptions {
            reap_empty_workspaces: Some(false),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let commands = vec![
        // iteration 0: Create VW1
        Event::Command {
            command: Command::Window(Operation::VirtualAdd),
        },
        // iteration 1: Create VW2
        Event::Command {
            command: Command::Window(Operation::VirtualAdd),
        },
        // iteration 2: Switch First -> VW0
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::First)),
        },
        // iteration 3: Switch East (alias for South/next) -> VW1
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::East)),
        },
        // iteration 4: Switch Last -> VW2
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::Last)),
        },
        // iteration 5: Switch West (alias for North/prev) -> VW1
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::West)),
        },
        // iteration 6: Switch First -> VW0
        Event::Command {
            command: Command::Window(Operation::Virtual(Direction::First)),
        },
        // iteration 7: Move focused window to Last with Follow -> moves to VW2 & follows to VW2
        Event::Command {
            command: Command::Window(Operation::VirtualMove(Direction::Last, MoveFocus::Follow)),
        },
        // iteration 8: Move focused window to First with Follow -> moves to VW0 & follows to VW0
        Event::Command {
            command: Command::Window(Operation::VirtualMove(Direction::First, MoveFocus::Follow)),
        },
    ];

    let assert_active_vw = |expected: u32| {
        move |world: &mut World, _state: MockState| {
            let mut query = world.query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>();
            let active = query
                .iter(world)
                .find_map(|(strip, active)| active.then_some(strip.virtual_index))
                .expect("an active virtual strip");
            assert_eq!(active, expected);
        }
    };

    TestHarness::new()
        .with_config(config)
        .with_windows(6)
        .on_iteration(2, assert_active_vw(0))
        .on_iteration(3, assert_active_vw(1))
        .on_iteration(4, assert_active_vw(2))
        .on_iteration(5, assert_active_vw(1))
        .on_iteration(6, assert_active_vw(0))
        .on_iteration(7, assert_active_vw(2))
        .on_iteration(8, assert_active_vw(0))
        .run(commands);
}

/// Focusing an unknown window (e.g. clicking an unmanaged tab or window)
/// must trigger auto-discovery and manage it on the fly into ECS.
#[test]
fn test_auto_discover_unmanaged_focused_window() {
    let events = vec![
        // Iteration 0: Boot with 1 managed window (window 0).
        Event::Command {
            command: Command::PrintState,
        },
        // Iteration 1: Send focus event for unmanaged window 1.
        Event::WindowFocused { window_id: 1 },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(0, |world, state| {
            // Verify window 1 is not in ECS yet.
            let mut query = world.query::<&crate::manager::Window>();
            let is_managed = query.iter(world).any(|w| w.id() == 1);
            assert!(!is_managed, "Window 1 should not be managed yet");

            // Spawn window 1 in mock state without sending AX notification to Paneru (unmanaged tab).
            state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                1,
                bevy::math::IRect::from_corners(
                    bevy::math::IVec2::new(0, 0),
                    bevy::math::IVec2::new(400, 400),
                ),
            );
            state.focus_window(1);
        })
        .on_iteration(1, |world, _state| {
            // Verify window 1 is now auto-discovered and managed in ECS.
            let mut query = world.query::<&crate::manager::Window>();
            let is_managed = query.iter(world).any(|w| w.id() == 1);
            assert!(
                is_managed,
                "Window 1 should be auto-discovered and managed in ECS after receiving focus"
            );
        })
        .run(events);
}

/// The centering config the manual-offset tests share: `auto_center` off and
/// `continuous_swipe` off is the combination that arms the edge invariant in
/// `reshuffle_layout_strip`, which is what used to snap a centered strip back
/// to the display's left edge.
fn manual_offset_config() -> Config {
    (
        MainOptions {
            auto_center: Some(false),
            continuous_swipe: Some(false),
            animation_speed: Some(10000.0),
            swipe_gesture_fingers: Some(3),
            ..Default::default()
        },
        vec![],
    )
        .into()
}

/// Where window 0 sits once `Operation::Center` has placed it.
const CENTERED_X: i32 = (TEST_DISPLAY_WIDTH - TEST_WINDOW_WIDTH) / 2;

fn active_strip_entity(world: &mut World) -> Entity {
    let mut query =
        world.query_filtered::<Entity, (With<LayoutStrip>, With<ActiveWorkspaceMarker>)>();
    query.single(world).expect("exactly one active strip")
}

fn has_manual_offset(world: &mut World) -> bool {
    let entity = active_strip_entity(world);
    world.get::<ManualStripOffset>(entity).is_some()
}

fn window_x(world: &mut World, id: WinID) -> i32 {
    let mut query = world.query::<&Window>();
    query
        .iter(world)
        .find(|window| window.id() == id)
        .expect("window not found")
        .frame()
        .min
        .x
}

/// A repeated OS focus event for the window that already holds focus — what a
/// browser emits when a new tab opens — is not new layout information. It used
/// to run a full reshuffle, which re-derived the strip offset and threw away a
/// manual centering.
#[test]
fn test_center_survives_repeated_focus_event() {
    let commands = vec![
        // 0: boot with focus on window 0.
        Event::MenuOpened { window_id: 0 },
        // 1: center it.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: the repeated focus event queued below plays out here.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, state| {
            assert_eq!(window_x(world, 0), CENTERED_X, "window 0 must be centered");
            // The app re-announces the window that already holds focus.
            state.focus_window(0);
        })
        .on_iteration(2, |world, _state| {
            assert_eq!(
                window_x(world, 0),
                CENTERED_X,
                "a repeated focus event must not undo the centering"
            );
            assert!(
                has_manual_offset(world),
                "the manual offset must survive a repeated focus event"
            );
        })
        .run(commands);
}

#[test]
fn manual_center_survives_an_unrelated_touchpad_release() {
    let mut harness = TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(4)
        .with_focused_window(0);
    harness.run(vec![Event::Command {
        command: Command::PrintState,
    }]);
    let float = find_window_entity(3, harness.world());
    harness
        .world()
        .entity_mut(float)
        .insert(Unmanaged::Floating);
    harness.advance(Duration::from_millis(100));
    harness.run(vec![Event::Command {
        command: Command::Window(Operation::Center),
    }]);
    assert_eq!(window_x(harness.world(), 0), CENTERED_X);

    harness.mock_state.focus_window(3);
    harness.advance(Duration::from_millis(100));
    // macOS emits a release even for gestures Paneru did not use for panning.
    harness.world().write_message(Event::TouchpadUp);
    harness.advance(Duration::from_millis(20));
    harness.mock_state.focus_window(0);
    harness.advance(Duration::from_millis(100));

    assert_eq!(
        window_x(harness.world(), 0),
        CENTERED_X,
        "returning focus must retain the deliberate placement after a non-pan release"
    );
}

/// The offset is only a claim about the layout it was taken on. Adding a window
/// changes that layout, so the next reshuffle derives a fresh offset and the
/// strip goes back to the edge invariant.
#[test]
fn test_center_dropped_when_window_added() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // 1: center window 0.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: the sixth window spawned below joins the strip.
        Event::Command {
            command: Command::PrintState,
        },
        // 3: a focus change asks for a reshuffle on the new layout.
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, state| {
            assert_eq!(window_x(world, 0), CENTERED_X, "window 0 must be centered");

            let window = state.spawn_window(
                TEST_PROCESS_ID,
                TEST_WORKSPACE_ID,
                5,
                IRect::new(0, 0, TEST_WINDOW_WIDTH, TEST_WINDOW_HEIGHT),
            );
            world.trigger(SpawnWindowTrigger(vec![window]));
        })
        .on_iteration(3, |world, _state| {
            assert_eq!(
                window_x(world, 0),
                0,
                "a reshuffle on a changed layout must re-derive the offset"
            );
            assert!(
                !has_manual_offset(world),
                "adding a window must invalidate the manual offset"
            );
        })
        .run(commands);
}

/// Skipping the reshuffle must not strand a window off-screen: if the strip has
/// scrolled the focused window past an edge, a focus event still has to bring
/// it back — and the manual placement it contradicts is dropped with it.
#[test]
fn test_partly_hidden_window_still_scrolled_into_view_after_center() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // 1: center window 0.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: the strip is dragged left below, hiding half of window 0.
        Event::Command {
            command: Command::PrintState,
        },
        // 3: the focus event queued below plays out here.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert_eq!(window_x(world, 0), CENTERED_X, "window 0 must be centered");

            // Push the strip a full window width further left than the
            // centering did, which drags window 0 off the left edge.
            let strip_entity = active_strip_entity(world);
            let displaced = {
                let position = world.get::<Position>(strip_entity).expect("strip position");
                Origin::new(position.0.x - TEST_WINDOW_WIDTH, position.0.y)
            };
            world.entity_mut(strip_entity).insert(Position(displaced));
        })
        .on_iteration(2, |_world, state| {
            state.focus_window(0);
        })
        .on_iteration(3, |world, _state| {
            assert!(
                window_x(world, 0) >= 0,
                "a window scrolled off the left edge must be brought back into view, got {}",
                window_x(world, 0)
            );
            assert!(
                !has_manual_offset(world),
                "a manual offset that hides its own window is stale"
            );
        })
        .run(commands);
}

/// Switching virtual workspaces re-places the strip from its saved origin, so
/// the manual claim on the old offset goes away with the switch.
#[test]
fn test_center_dropped_on_virtual_workspace_switch() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // 1: center window 0.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: move a window to VW1 and follow it there.
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
        },
        // 3: back to VW0.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert!(has_manual_offset(world), "center must mark the strip");
        })
        .on_iteration(3, |world, _state| {
            assert!(
                !has_manual_offset(world),
                "a workspace switch must invalidate the manual offset"
            );
        })
        .run(commands);
}

/// Once the user drives the strip by hand, the earlier placement no longer
/// describes where they want it.
#[test]
fn test_center_dropped_on_user_swipe() {
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        // 1: center window 0.
        Event::Command {
            command: Command::Window(Operation::Center),
        },
        // 2: the user swipes the strip.
        Event::Swipe {
            delta: -0.2,
            fingers: 3,
        },
    ];

    TestHarness::new()
        .with_config(manual_offset_config())
        .with_windows(5)
        .on_iteration(1, |world, _state| {
            assert!(has_manual_offset(world), "center must mark the strip");
        })
        .on_iteration(2, |world, _state| {
            assert!(
                !has_manual_offset(world),
                "a user swipe must invalidate the manual offset"
            );
        })
        .run(commands);
}

/// Cmd-Tab into an app parked on another virtual workspace must land focus on
/// that app's window. Regression: with `virtual_workspace_animations = true`,
/// the animated branch of `show_active_workspace` restored the strip's
/// remembered focus unconditionally, so activating the strip through a focus
/// event immediately yanked focus onto whatever was focused there last.
#[test]
fn test_focus_into_hidden_virtual_workspace_keeps_target_window() {
    let config: Config = (
        MainOptions {
            animation_speed: Some(30.0),
            virtual_workspace_animations: Some(true),
            ..Default::default()
        },
        vec![],
    )
        .into();

    let commands = vec![
        // 0: boot.
        Event::MenuOpened { window_id: 0 },
        // 1: window 2 takes focus.
        Event::Command {
            command: Command::PrintState,
        },
        // 2: park it on VW1.
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        // 3: window 3 takes focus.
        Event::Command {
            command: Command::PrintState,
        },
        // 4: park it on VW1 too, leaving VW1 = [2, 3].
        Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        },
        // 5: visit VW1.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        // 6: focus window 3 there, so that is what VW1 remembers.
        Event::Command {
            command: Command::PrintState,
        },
        // 7: back to VW0, parking VW1.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        // 8: the Cmd-Tab into window 2 queued below plays out here.
        Event::Command {
            command: Command::PrintState,
        },
    ];

    TestHarness::new()
        .with_config(config)
        .with_windows(4)
        .on_iteration(0, |_world, state| {
            state.focus_window(2);
        })
        .on_iteration(1, |world, _state| {
            assert_focused!(world, 2);
        })
        .on_iteration(2, |_world, state| {
            state.focus_window(3);
        })
        .on_iteration(3, |world, _state| {
            assert_focused!(world, 3);
        })
        .on_iteration(5, |_world, state| {
            state.focus_window(3);
        })
        .on_iteration(6, |world, _state| {
            assert_focused!(world, 3);
        })
        .on_iteration(7, |_world, state| {
            // Cmd-Tab straight into window 2, the other window on VW1.
            state.focus_window(2);
        })
        .on_iteration(8, |world, _state| {
            assert_focused!(world, 2);
        })
        .run(commands);
}

/// Cmd-Tab into a window left hanging off the edge of a hidden virtual
/// workspace must scroll that workspace's strip far enough to show all of it.
/// Regression: `show_active_workspace` restored the strip to the offset it had
/// when it was parked, which says nothing about the window that just took
/// focus, and `window_hidden_ratio` lets the reshuffle leave a window that far
/// over an edge alone - so nothing brought it back.
#[test]
fn test_focus_into_hidden_virtual_workspace_exposes_target_window() {
    /// How far window 2 hangs off the left edge while VW1 is parked: a
    /// quarter of it, which `window_hidden_ratio` below tolerates.
    const OVERHANG: i32 = TEST_WINDOW_WIDTH / 4;

    let config: Config = (
        MainOptions {
            animation_speed: Some(10000.0),
            virtual_workspace_animations: Some(true),
            auto_center: Some(false),
            // A reshuffle may leave a window up to half hidden where it is,
            // so exposing this one is down to the workspace restore itself.
            window_hidden_ratio: Some(0.5),
            ..Default::default()
        },
        vec![],
    )
        .into();

    // Park windows 2 to 5 on VW1: four 400px columns on a 1024px display, so
    // the strip is wider than the screen and has somewhere to scroll to.
    let mut commands = vec![Event::MenuOpened { window_id: 0 }];
    for _ in 0..4 {
        commands.push(Event::Command {
            command: Command::PrintState,
        });
        commands.push(Event::Command {
            command: Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Stay)),
        });
    }
    commands.extend([
        // 9: visit VW1.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(1)),
        },
        // 10: the strip is dragged left below, leaving window 2 hanging off.
        Event::Command {
            command: Command::PrintState,
        },
        // 11: park VW1 at that offset.
        Event::Command {
            command: Command::Window(Operation::VirtualNumber(0)),
        },
        // 12: the Cmd-Tab into window 2 queued below plays out here.
        Event::Command {
            command: Command::PrintState,
        },
    ]);

    let focus_next = |id: WinID| move |_world: &mut World, state: MockState| state.focus_window(id);
    TestHarness::new()
        .with_config(config)
        .with_windows(6)
        .on_iteration(0, focus_next(2))
        .on_iteration(2, focus_next(3))
        .on_iteration(4, focus_next(4))
        .on_iteration(6, focus_next(5))
        .on_iteration(8, focus_next(5))
        .on_iteration(9, move |world, _state| {
            // Slide VW1 so window 2 hangs off the left edge by a quarter.
            let strip_entity = active_strip_entity(world);
            world
                .entity_mut(strip_entity)
                .insert(Position(Origin::new(-OVERHANG, TEST_MENUBAR_HEIGHT)));
        })
        .on_iteration(10, move |world, _state| {
            assert_eq!(
                window_x(world, 2),
                -OVERHANG,
                "test setup: window 2 must hang off the left edge before parking"
            );
        })
        // Cmd-Tab straight into window 2, now that VW1 is parked.
        .on_iteration(11, focus_next(2))
        .on_iteration(12, |world, _state| {
            assert_focused!(world, 2);
            assert_eq!(
                window_x(world, 2),
                0,
                "the workspace restore must show all of the window it was activated for"
            );
        })
        .run(commands);
}

/// Invoking `Operation::CopyRule` copies a valid window rule snippet
/// for the focused window to the clipboard.
#[test]
fn test_copy_window_rule_command() {
    let commands = vec![
        // 0: boot with focus on window 0.
        Event::MenuOpened { window_id: 0 },
        // 1: copy window rule for the focused window.
        Event::Command {
            command: Command::Window(Operation::CopyRule),
        },
    ];

    TestHarness::new()
        .with_windows(1)
        .on_iteration(1, |_world, _state| {
            let copied = crate::pasteboard::get_test_clipboard()
                .expect("clipboard should have been populated");
            assert!(
                copied.contains("[windows.testapp]") || copied.contains("windows = {"),
                "copied snippet should contain window rule, got: {copied}"
            );
            assert!(
                copied.contains("bundle_id = \"test\""),
                "copied snippet should contain bundle id, got: {copied}"
            );
            assert!(
                copied.contains("title = \"^Window 0$\""),
                "copied snippet should contain exact anchored window title, got: {copied}"
            );
        })
        .run(commands);
}

/// `virtualnum` on a missing row spawns it — row 0 included. Row 0 used to be
/// the one index that bailed out instead, so a space that had lost its row 0
/// could never switch back to workspace "1".
#[test]
fn test_virtual_number_recreates_missing_baseline_row() {
    let mut h = TestHarness::new().with_windows(2);
    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    // Move both windows onto row 1 and switch there, then drop row 0 the way a
    // display change does, leaving the space numbered from "2".
    for _ in 0..2 {
        pump(
            &mut h,
            Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
        );
    }
    let world = h.world();
    let row_zero = world
        .query::<(Entity, &LayoutStrip)>()
        .iter(world)
        .find(|(_, strip)| strip.virtual_index == 0)
        .map(|(entity, _)| entity)
        .expect("row 0 should still exist before it is dropped");
    world.entity_mut(row_zero).despawn();

    let world = h.world();
    let indexes = world
        .query::<&LayoutStrip>()
        .iter(world)
        .map(|strip| strip.virtual_index)
        .collect::<Vec<_>>();
    assert_eq!(indexes, vec![1], "only row 1 should be left");

    pump(&mut h, Command::Window(Operation::VirtualNumber(0)));

    let world = h.world();
    let recreated = world
        .query::<(&LayoutStrip, Has<ActiveWorkspaceMarker>)>()
        .iter(world)
        .filter(|(strip, _)| strip.virtual_index == 0)
        .map(|(_, active)| active)
        .collect::<Vec<_>>();
    assert_eq!(
        recreated,
        vec![true],
        "virtualnum 0 should recreate row 0 and make it active"
    );
}

/// Moving a window to a missing row spawns it — row 0 included. Row 0 used to
/// be refused here too, so a space that had lost its row 0 could not even send
/// a window back to workspace "1".
#[test]
fn test_virtual_move_number_recreates_missing_baseline_row() {
    let mut h = TestHarness::new().with_windows(2);
    let pump = |h: &mut TestHarness, c: Command| {
        h.app
            .world_mut()
            .write_message::<Event>(Event::Command { command: c });
        for _ in 0..8 {
            h.app.update();
            for e in h.mock_state.drain_events() {
                h.app.world_mut().write_message::<Event>(e);
            }
        }
    };

    // Park both windows on row 1 and drop the emptied row 0 the way a display
    // change does, leaving the space numbered from "2".
    for _ in 0..2 {
        pump(
            &mut h,
            Command::Window(Operation::VirtualMoveNumber(1, MoveFocus::Follow)),
        );
    }
    let world = h.world();
    let row_zero = world
        .query::<(Entity, &LayoutStrip)>()
        .iter(world)
        .find(|(_, strip)| strip.virtual_index == 0)
        .map(|(entity, _)| entity)
        .expect("row 0 should still exist before it is dropped");
    world.entity_mut(row_zero).despawn();

    pump(
        &mut h,
        Command::Window(Operation::VirtualMoveNumber(0, MoveFocus::Follow)),
    );

    let world = h.world();
    let focused = world
        .query_filtered::<Entity, With<FocusedMarker>>()
        .single(world)
        .expect("the followed window should be focused");
    let recreated = world
        .query::<&LayoutStrip>()
        .iter(world)
        .filter(|strip| strip.virtual_index == 0)
        .map(|strip| strip.contains(focused))
        .collect::<Vec<_>>();
    assert_eq!(
        recreated,
        vec![true],
        "the moved window should land on a single recreated row 0"
    );
}

//! Native Desktop creation and destruction, and explicit window moves between
//! native Spaces. Every native write is asynchronous: the window server may
//! apply it later, refuse it, or lose the reply, and the ECS may only act on
//! what it has observed. These tests drive that boundary with the virtual
//! window server's outcome controls and the manual clock.

use std::time::Duration;

use bevy::ecs::system::SystemState;
use bevy::prelude::*;

use crate::commands::{Command, Direction, MoveFocus, Operation, SpaceOperation, SpaceSelector};
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::native_spaces::{
    DestroyedSpaceMarker, NativeSpaceCreatePending, NativeSpaceDestroyPending,
    NativeSpacePlacementPending, SpaceMovePending,
};
use crate::ecs::state::QueryStateParams;
use crate::ecs::{
    FloatingMarker, FocusedMarker, FollowCurrentWorkspaceMarker, MinimizedMarker, Position,
    SelectedVirtualMarker, SpawnWindowTrigger,
};
use crate::events::Event;
use crate::manager::{Display, Window};
use crate::platform::WinID;
use crate::{assert_focused, assert_not_on_workspace, assert_on_workspace};

use super::*;

/// The Space the test display shows at startup.
const FIRST: WorkspaceId = TEST_WORKSPACE_ID;
const SECOND: WorkspaceId = TEST_WORKSPACE_ID + 1;
const THIRD: WorkspaceId = TEST_WORKSPACE_ID + 2;
/// The second Space of the external display.
const EXT_SECOND: WorkspaceId = EXT_WORKSPACE_ID + 1;

/// A native type that is neither a Desktop nor a fullscreen Space.
const SYSTEM_SPACE_KIND: i64 = 2;

/// A sheet attached to window 0.
const SHEET: WinID = 10;
/// A window that starts out on a Space that is not showing.
const HIDDEN: WinID = 5;
/// A Desktop the user adds in Mission Control, on the external display.
const EXT_ADDED: WorkspaceId = EXT_WORKSPACE_ID + 5;

fn test_display_bounds() -> IRect {
    IRect::new(0, 0, TEST_DISPLAY_WIDTH, TEST_DISPLAY_HEIGHT)
}

/// A harness whose test display offers `workspaces`, the first one showing.
fn display_with(workspaces: Vec<WorkspaceId>) -> TestHarness {
    TestHarness::new().with_display(TEST_DISPLAY_ID, test_display_bounds(), workspaces)
}

/// Config that gives every native Space three virtual rows from the start.
fn three_rows_config() -> Config {
    Config::try_from("default_workspaces = 3\n[options]\n[bindings]\n")
        .expect("a config with three virtual rows")
}

/// Runs startup to completion, so the first native request is the test's.
fn boot(harness: &mut TestHarness) {
    harness.run(vec![print_state()]);
}

fn print_state() -> Event {
    Event::Command {
        command: Command::PrintState,
    }
}

fn create() -> Event {
    Event::Command {
        command: Command::Space(SpaceOperation::Create),
    }
}

fn destroy(selector: SpaceSelector, migrate: bool) -> Event {
    Event::Command {
        command: Command::Space(SpaceOperation::Destroy { selector, migrate }),
    }
}

fn space_move(selector: SpaceSelector, focus: MoveFocus) -> Event {
    Event::Command {
        command: Command::Window(Operation::SpaceMove(selector, focus)),
    }
}

/// Toggles the focused window between tiled and floating.
fn toggle_float() -> Event {
    Event::Command {
        command: Command::Window(Operation::Manage),
    }
}

fn follow(enable: bool) -> Event {
    Event::Command {
        command: Command::Window(Operation::Follow(Some(enable))),
    }
}

fn window_entity(world: &mut World, id: WinID) -> Option<Entity> {
    world
        .query::<(Entity, &Window)>()
        .iter(world)
        .find_map(|(entity, window)| (window.id() == id).then_some(entity))
}

/// Every virtual row of native Space `workspace_id`, lowest index first:
/// the row entity, its virtual index and the windows it holds.
fn rows_of(world: &mut World, workspace_id: WorkspaceId) -> Vec<(Entity, u32, Vec<Entity>)> {
    let mut rows = world
        .query::<(Entity, &LayoutStrip)>()
        .iter(world)
        .filter(|(_, strip)| strip.id() == workspace_id)
        .map(|(entity, strip)| (entity, strip.virtual_index, strip.all_windows()))
        .collect::<Vec<_>>();
    rows.sort_unstable_by_key(|(_, index, _)| *index);
    rows
}

/// The one display that owns every row of `workspace_id`.
fn rows_display(world: &mut World, workspace_id: WorkspaceId) -> u32 {
    let mut owners = rows_of(world, workspace_id)
        .into_iter()
        .map(|(row, _, _)| {
            let parent = world
                .get::<ChildOf>(row)
                .expect("a row is parented to its display")
                .parent();
            world
                .get::<Display>(parent)
                .expect("a display owns the row")
                .id()
        })
        .collect::<Vec<_>>();
    owners.sort_unstable();
    owners.dedup();
    match owners.as_slice() {
        [owner] => *owner,
        many => panic!("native Space {workspace_id} rows are owned by displays {many:?}"),
    }
}

/// The origin the one row of `workspace_id` lays its windows out from.
fn row_position(world: &mut World, workspace_id: WorkspaceId) -> IVec2 {
    match rows_of(world, workspace_id).as_slice() {
        [(row, _, _)] => world.get::<Position>(*row).expect("a row has a position").0,
        rows => panic!("expected one row of native Space {workspace_id}, got {rows:?}"),
    }
}

/// Whether `frame` sits entirely on the external display.
fn on_ext_display(frame: IRect) -> bool {
    let bounds = ext_display_bounds();
    bounds.contains(frame.min) && frame.max.x <= bounds.max.x && frame.max.y <= bounds.max.y
}

/// The virtual row of `workspace_id` a moved window is meant to join.
fn selected_row(world: &mut World, workspace_id: WorkspaceId) -> u32 {
    world
        .query_filtered::<&LayoutStrip, With<SelectedVirtualMarker>>()
        .iter(world)
        .find_map(|strip| (strip.id() == workspace_id).then_some(strip.virtual_index))
        .expect("a selected row")
}

/// The native Space and virtual row whose strip holds window `id`.
fn window_row(world: &mut World, id: WinID) -> Option<(WorkspaceId, u32)> {
    let entity = window_entity(world, id)?;
    world.query::<&LayoutStrip>().iter(world).find_map(|strip| {
        strip
            .contains(entity)
            .then_some((strip.id(), strip.virtual_index))
    })
}

fn focused_window(world: &mut World) -> Option<WinID> {
    let mut query = world.query_filtered::<&Window, With<FocusedMarker>>();
    let window = query.iter(world).next()?;
    Some(window.id())
}

/// Every window carrying the focus marker, by id.
fn focused_windows(world: &mut World) -> Vec<WinID> {
    let mut ids = world
        .query_filtered::<&Window, With<FocusedMarker>>()
        .iter(world)
        .map(|window| window.id())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

/// Every virtual row of every native Space.
fn row_count(world: &mut World) -> usize {
    world.query::<&LayoutStrip>().iter(world).count()
}

fn is_floating(world: &mut World, id: WinID) -> bool {
    let entity = find_window_entity(id, world);
    world.entity(entity).contains::<FloatingMarker>()
}

fn is_follower(world: &mut World, id: WinID) -> bool {
    let entity = find_window_entity(id, world);
    world
        .entity(entity)
        .contains::<FollowCurrentWorkspaceMarker>()
}

/// Whether the windows `ids` sit together in one native tab column.
fn share_a_tab_column(world: &mut World, ids: &[WinID]) -> bool {
    let entities = ids
        .iter()
        .map(|&id| find_window_entity(id, world))
        .collect::<Vec<_>>();
    world.query::<&LayoutStrip>().iter(world).any(|strip| {
        strip.tab_group(entities[0]).is_some_and(|group| {
            group.len() == entities.len() && entities.iter().all(|entity| group.contains(entity))
        })
    })
}

fn creations_pending(world: &mut World) -> usize {
    world
        .query::<&NativeSpaceCreatePending>()
        .iter(world)
        .count()
}

fn destructions_pending(world: &mut World) -> usize {
    world
        .query::<&NativeSpaceDestroyPending>()
        .iter(world)
        .count()
}

fn placements_pending(world: &mut World) -> usize {
    world
        .query::<&NativeSpacePlacementPending>()
        .iter(world)
        .count()
}

/// Rows kept for a native Space the census no longer lists, whose windows
/// have not yet been observed elsewhere.
fn destroyed_rows(world: &mut World) -> usize {
    world
        .query_filtered::<&LayoutStrip, With<DestroyedSpaceMarker>>()
        .iter(world)
        .count()
}

fn space_move_pending(world: &mut World, id: WinID) -> bool {
    window_entity(world, id)
        .is_some_and(|entity| world.entity(entity).contains::<SpaceMovePending>())
}

fn any_space_move_pending(world: &mut World) -> bool {
    world
        .query::<&SpaceMovePending>()
        .iter(world)
        .next()
        .is_some()
}

/// The one Desktop the window server has been asked to create so far.
fn created_space(harness: &TestHarness) -> WorkspaceId {
    match harness.mock_state.native_space_creations().as_slice() {
        [id] => *id,
        creations => panic!("expected exactly one creation, got {creations:?}"),
    }
}

/// The batch a single submitted move carried, in window order.
fn submitted_batch(harness: &TestHarness) -> (Vec<WinID>, WorkspaceId) {
    match harness.mock_state.workspace_moves().as_slice() {
        [(windows, target)] => {
            let mut windows = windows.clone();
            windows.sort_unstable();
            (windows, *target)
        }
        moves => panic!("expected exactly one move, got {moves:?}"),
    }
}

/// Configuration whose windows all float and follow the current Space.
fn follower_config() -> Config {
    let mut params = WindowParams::new(".*", None);
    params.follow = Some(true);
    (MainOptions::default(), vec![params]).into()
}

/// Configuration whose windows all float.
fn floating_config() -> Config {
    let mut params = WindowParams::new(".*", None);
    params.floating = Some(true);
    (MainOptions::default(), vec![params]).into()
}

/// Configuration whose windows all float, with instant layout animation so a
/// confirmed placement shows on the next frame.
fn instant_floating_config() -> Config {
    let mut params = WindowParams::new(".*", None);
    params.floating = Some(true);
    (
        MainOptions {
            animation_speed: Some(10000.0),
            ..default()
        },
        vec![params],
    )
        .into()
}

/// A two-display harness whose window server announces topology changes
/// before its readers show them.
fn lagging_two_display_harness() -> TestHarness {
    let harness = display_with(vec![FIRST])
        .with_display(EXT_DISPLAY_ID, ext_display_bounds(), vec![EXT_WORKSPACE_ID])
        .with_windows(1);
    harness.mock_state.set_native_topology_lagging(true);
    harness
}

/// The row `workspace_id` ends up with once its owner is known: one, under
/// the external display, laid out from where that display's rows start —
/// the origin its existing row uses.
fn assert_placed_on_ext_display(world: &mut World, workspace_id: WorkspaceId) {
    assert_eq!(rows_of(world, workspace_id).len(), 1);
    assert_eq!(rows_display(world, workspace_id), EXT_DISPLAY_ID);
    assert_eq!(
        row_position(world, workspace_id),
        row_position(world, EXT_WORKSPACE_ID),
        "the row lays out from its actual display, not the one it was guessed onto"
    );
    assert_eq!(placements_pending(world), 0);
    assert_eq!(creations_pending(world), 0);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(ecs_active_display(world), TEST_DISPLAY_ID);
}

// --- Creation ---

#[test]
fn create_adds_a_desktop_under_its_owning_display_without_switching() {
    let mut harness = display_with(vec![FIRST])
        .with_display(EXT_DISPLAY_ID, ext_display_bounds(), vec![EXT_WORKSPACE_ID])
        .with_windows(1);
    // The bridge decides where a new Desktop goes; here it is not the
    // active display.
    harness.mock_state.set_native_create_display(EXT_DISPLAY_ID);
    boot(&mut harness);

    harness.run(vec![create()]);

    let created = created_space(&harness);
    assert_eq!(
        harness.mock_state.display_workspaces(EXT_DISPLAY_ID),
        vec![EXT_WORKSPACE_ID, created]
    );
    let world = harness.world();
    let rows = rows_of(world, created);
    assert_eq!(rows.len(), 1, "one fresh row for the new Desktop: {rows:?}");
    assert_eq!(
        rows_display(world, created),
        EXT_DISPLAY_ID,
        "the row belongs to the display that actually owns the Desktop"
    );
    assert_eq!(creations_pending(world), 0);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(ecs_active_display(world), TEST_DISPLAY_ID);
    assert_focused!(world, 0);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert_eq!(
        harness.mock_state.active_workspace(EXT_DISPLAY_ID),
        EXT_WORKSPACE_ID,
        "plain creation switches nothing, not even the owning display"
    );
}

#[test]
fn deferred_creation_is_adopted_only_once_the_census_shows_it() {
    let mut harness = display_with(vec![FIRST]).with_windows(1);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![create()]);
    let created = created_space(&harness);
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST]
    );
    assert!(
        rows_of(harness.world(), created).is_empty(),
        "an id the census does not show yet is a receipt, not a Desktop"
    );
    assert_eq!(creations_pending(harness.world()), 1);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(rows_of(harness.world(), created).len(), 1);
    assert_eq!(rows_display(harness.world(), created), TEST_DISPLAY_ID);
    assert_eq!(creations_pending(harness.world()), 0);
    assert_eq!(harness.mock_state.native_space_creations().len(), 1);
    assert_eq!(ecs_active_workspaces(harness.world()), vec![FIRST]);
}

#[test]
fn eventless_creation_is_confirmed_by_the_census() {
    let mut harness = display_with(vec![FIRST])
        .with_display(EXT_DISPLAY_ID, ext_display_bounds(), vec![EXT_WORKSPACE_ID])
        .with_windows(1);
    harness.mock_state.set_native_create_display(EXT_DISPLAY_ID);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Deferred);
    // The window server applies the creation without announcing it.
    harness.mock_state.set_native_lifecycle_notifies(false);
    boot(&mut harness);

    harness.run(vec![create()]);
    let created = created_space(&harness);
    assert!(rows_of(harness.world(), created).is_empty());

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        rows_of(harness.world(), created).len(),
        1,
        "the Desktop is adopted from the census alone"
    );
    assert_eq!(rows_display(harness.world(), created), EXT_DISPLAY_ID);
    assert_eq!(creations_pending(harness.world()), 0);
}

#[test]
fn creation_that_never_shows_up_is_abandoned_without_retry() {
    let mut harness = display_with(vec![FIRST]).with_windows(1);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![create()]);
    let created = created_space(&harness);

    harness.advance(NATIVE_DEADLINE);
    assert_eq!(
        harness.mock_state.native_space_creations().len(),
        1,
        "a creation the census never confirms is not resubmitted"
    );
    assert!(rows_of(harness.world(), created).is_empty());
    assert_eq!(
        creations_pending(harness.world()),
        0,
        "the wait is bounded by the deadline"
    );

    // A fresh command is a fresh request.
    harness.run(vec![create()]);
    assert_eq!(harness.mock_state.native_space_creations().len(), 2);
}

#[test]
fn refused_creation_leaves_nothing_pending() {
    let mut harness = display_with(vec![FIRST]).with_windows(1);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Rejected);
    boot(&mut harness);
    let rows_before = row_count(harness.world());

    harness.run(vec![create()]);
    assert!(harness.mock_state.native_space_creations().is_empty());
    assert_eq!(
        row_count(harness.world()),
        rows_before,
        "a refusal creates no row"
    );
    assert_eq!(creations_pending(harness.world()), 0);

    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Immediate);
    harness.run(vec![create()]);
    let created = created_space(&harness);
    assert_eq!(rows_of(harness.world(), created).len(), 1);
}

#[test]
fn uncertain_creation_is_neither_claimed_nor_repeated() {
    let mut harness = display_with(vec![FIRST]).with_windows(1);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Uncertain { applies: true });
    boot(&mut harness);

    // The reply is lost: the bridge reports failure and no id.
    harness.run(vec![create()]);
    let created = created_space(&harness);
    assert!(rows_of(harness.world(), created).is_empty());

    // The window server carries the creation out anyway and announces it.
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_DEADLINE);
    assert_eq!(
        harness.mock_state.native_space_creations().len(),
        1,
        "an uncertain creation is observed, never blindly retried"
    );
    assert_eq!(
        rows_of(harness.world(), created).len(),
        1,
        "the Desktop the OS announced gets one row, not one per observer"
    );
    assert_eq!(rows_display(harness.world(), created), TEST_DISPLAY_ID);
    assert_eq!(creations_pending(harness.world()), 0);
}

#[test]
fn repeated_create_commands_submit_once_per_confirmed_desktop() {
    let mut harness = display_with(vec![FIRST]).with_windows(1);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    // Two commands in the same frame.
    harness.world().write_message(create());
    harness.world().write_message(create());
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.native_space_creations().len(),
        1,
        "a burst of commands does not submit a burst of creations"
    );

    // Another while the first is still unconfirmed.
    harness.run(vec![create()]);
    assert_eq!(harness.mock_state.native_space_creations().len(), 1);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    let created = created_space(&harness);
    assert_eq!(rows_of(harness.world(), created).len(), 1);
    assert_eq!(creations_pending(harness.world()), 0);

    harness.run(vec![create()]);
    assert_eq!(
        harness.mock_state.native_space_creations().len(),
        2,
        "once the first Desktop is confirmed a new command is served"
    );
}

#[test]
fn repeated_created_notifications_do_not_duplicate_rows() {
    let mut harness = display_with(vec![FIRST]).with_windows(1);
    boot(&mut harness);

    harness.run(vec![create()]);
    let created = created_space(&harness);
    assert_eq!(rows_of(harness.world(), created).len(), 1);

    harness
        .world()
        .write_message(Event::SpaceCreated { space_id: created });
    harness
        .world()
        .write_message(Event::SpaceCreated { space_id: created });
    harness.advance(NATIVE_REACTION);
    assert_eq!(rows_of(harness.world(), created).len(), 1);
    assert_eq!(ecs_active_workspaces(harness.world()), vec![FIRST]);
}

#[test]
fn desktop_announced_before_the_census_lists_it_waits_for_its_owner() {
    let mut harness = lagging_two_display_harness();
    boot(&mut harness);
    let rows_before = row_count(harness.world());

    // The user adds a Desktop on the external display; the notification
    // arrives while no display lists it yet.
    harness.mock_state.add_workspace(EXT_DISPLAY_ID, EXT_ADDED);
    harness.advance(NATIVE_REACTION);
    let world = harness.world();
    assert!(
        rows_of(world, EXT_ADDED).is_empty(),
        "a Desktop nobody owns yet is not guessed onto the active display"
    );
    assert_eq!(row_count(world), rows_before);
    assert_eq!(placements_pending(world), 1);

    harness.mock_state.settle_native_topology();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.display_workspaces(EXT_DISPLAY_ID),
        vec![EXT_WORKSPACE_ID, EXT_ADDED]
    );
    assert_placed_on_ext_display(harness.world(), EXT_ADDED);
    assert_eq!(row_count(harness.world()), rows_before + 1);
}

#[test]
fn desktop_the_census_never_lists_gets_no_row_and_a_bounded_wait() {
    let mut harness = lagging_two_display_harness();
    boot(&mut harness);
    let rows_before = row_count(harness.world());

    harness.mock_state.add_workspace(EXT_DISPLAY_ID, EXT_ADDED);
    harness.advance(NATIVE_DEADLINE);

    let world = harness.world();
    assert!(
        rows_of(world, EXT_ADDED).is_empty(),
        "the deadline is not a reason to invent an owner"
    );
    assert_eq!(row_count(world), rows_before);
    assert_eq!(placements_pending(world), 0, "the wait is bounded");
    assert!(!pump_awake(world), "nothing keeps polling for it");
}

#[test]
fn created_desktop_is_placed_once_the_census_recovers_and_lists_it() {
    let mut harness = lagging_two_display_harness();
    harness.mock_state.set_native_create_display(EXT_DISPLAY_ID);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);
    let rows_before = row_count(harness.world());

    harness.run(vec![create()]);
    let created = created_space(&harness);

    // The window server announces the Desktop while its census cannot be
    // read at all, and lists it only later.
    harness.mock_state.set_native_census_failing(true);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    let world = harness.world();
    assert!(
        rows_of(world, created).is_empty(),
        "neither the notification nor a failed census places the row"
    );
    assert_eq!(row_count(world), rows_before);

    harness.mock_state.set_native_census_failing(false);
    harness.mock_state.settle_native_topology();
    harness.advance(NATIVE_REACTION);
    assert_placed_on_ext_display(harness.world(), created);
    assert_eq!(harness.mock_state.native_space_creations().len(), 1);
}

#[test]
fn uncertain_creation_announced_before_the_census_is_placed_by_its_owner() {
    let mut harness = lagging_two_display_harness();
    harness.mock_state.set_native_create_display(EXT_DISPLAY_ID);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Uncertain { applies: true });
    boot(&mut harness);
    let rows_before = row_count(harness.world());

    // The reply was lost, so the ECS holds no ID to match the notification
    // against; the notification then precedes the census listing.
    harness.run(vec![create()]);
    let created = created_space(&harness);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    let world = harness.world();
    assert!(rows_of(world, created).is_empty());
    assert_eq!(row_count(world), rows_before);

    harness.mock_state.settle_native_topology();
    harness.advance(NATIVE_REACTION);
    assert_placed_on_ext_display(harness.world(), created);
    assert_eq!(
        harness.mock_state.native_space_creations().len(),
        1,
        "an uncertain creation is observed, never blindly retried"
    );
}

#[test]
fn startup_native_membership_assigns_each_tiled_window_to_one_virtual_row() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_config(three_rows_config())
        .with_windows(2);
    boot(&mut harness);
    for id in [0, 1] {
        let entity = find_window_entity(id, harness.world());
        let occupied = rows_of(harness.world(), FIRST)
            .into_iter()
            .filter_map(|(_, index, members)| members.contains(&entity).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(
            occupied,
            vec![0],
            "native membership does not imply every virtual row"
        );
    }
}

// --- Destruction ---

#[test]
fn destroy_removes_every_virtual_row_of_the_empty_desktop() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD])
        .with_config(three_rows_config())
        .with_windows(2);
    boot(&mut harness);
    assert_eq!(rows_of(harness.world(), SECOND).len(), 3);
    let frames_before = (
        window_frame(harness.world(), 0),
        window_frame(harness.world(), 1),
    );

    harness.run(vec![destroy(SpaceSelector::Next, false)]);

    assert_eq!(
        harness.mock_state.native_space_destroys(),
        vec![(SECOND, false)]
    );
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, THIRD]
    );
    let world = harness.world();
    assert!(
        rows_of(world, SECOND).is_empty(),
        "every virtual row of the destroyed Desktop goes, not just the first"
    );
    assert_eq!(rows_of(world, FIRST).len(), 3);
    assert_eq!(rows_of(world, THIRD).len(), 3);
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(
        (window_frame(world, 0), window_frame(world, 1)),
        frames_before,
        "windows on the surviving Space are not touched"
    );
    assert_focused!(world, 0);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(destructions_pending(world), 0);
}

#[test]
fn destroy_refuses_a_space_that_is_current_on_any_display() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_display(
            EXT_DISPLAY_ID,
            ext_display_bounds(),
            vec![EXT_WORKSPACE_ID, EXT_SECOND],
        )
        .with_windows(1);
    boot(&mut harness);

    harness.run(vec![
        // The Space showing on the display without the menu bar.
        destroy(SpaceSelector::Number(3), false),
        // The Space showing on the active display.
        destroy(SpaceSelector::Number(1), false),
    ]);

    assert!(harness.mock_state.native_space_destroys().is_empty());
    assert_eq!(
        harness.mock_state.display_workspaces(EXT_DISPLAY_ID),
        vec![EXT_WORKSPACE_ID, EXT_SECOND]
    );
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND]
    );
    let world = harness.world();
    assert_eq!(rows_of(world, EXT_WORKSPACE_ID).len(), 1);
    assert_eq!(rows_of(world, FIRST).len(), 1);
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_eq!(destructions_pending(world), 0);
}

#[test]
fn destroy_refuses_the_last_desktop_and_non_desktop_spaces() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD])
        .with_display(
            EXT_DISPLAY_ID,
            ext_display_bounds(),
            vec![EXT_WORKSPACE_ID, EXT_SECOND],
        )
        .with_windows(1);
    harness
        .mock_state
        .set_native_space_kind(THIRD, SYSTEM_SPACE_KIND);
    // The external display shows a fullscreen Space, leaving one Desktop.
    harness
        .mock_state
        .activate_workspace(EXT_DISPLAY_ID, EXT_SECOND, true);
    boot(&mut harness);

    harness.run(vec![
        destroy(SpaceSelector::Number(3), false),
        destroy(SpaceSelector::Number(4), false),
    ]);

    assert!(harness.mock_state.native_space_destroys().is_empty());
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND, THIRD],
        "a Space that is not a Desktop is left alone"
    );
    assert_eq!(
        harness.mock_state.display_workspaces(EXT_DISPLAY_ID),
        vec![EXT_WORKSPACE_ID, EXT_SECOND],
        "the last Desktop of a display is left alone"
    );
    let world = harness.world();
    assert_eq!(rows_of(world, THIRD).len(), 1);
    assert_eq!(rows_of(world, EXT_WORKSPACE_ID).len(), 1);
    assert_eq!(destructions_pending(world), 0);
}

#[test]
fn destroy_refuses_an_occupied_desktop_until_migration_is_requested() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(2)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    boot(&mut harness);
    assert_eq!(window_row(harness.world(), HIDDEN), Some((SECOND, 0)));

    harness.run(vec![destroy(SpaceSelector::Next, false)]);
    assert!(harness.mock_state.native_space_destroys().is_empty());
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND]
    );
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), SECOND);
    assert_eq!(window_row(harness.world(), HIDDEN), Some((SECOND, 0)));
    assert_eq!(destructions_pending(harness.world()), 0);

    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    assert_eq!(
        harness.mock_state.native_space_destroys(),
        vec![(SECOND, true)]
    );
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST]
    );
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), FIRST);
    let world = harness.world();
    assert!(rows_of(world, SECOND).is_empty());
    assert_on_workspace!(world, HIDDEN, FIRST);
    assert_eq!(
        rows_of(world, FIRST).len(),
        1,
        "the survivor joins the showing Space's row; no row is invented"
    );
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_focused!(world, 0);
    assert_eq!(destructions_pending(world), 0);
}

#[test]
fn migration_lands_windows_on_the_display_current_space() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD])
        .with_workspace_window(0, SECOND, |_| {})
        .with_workspace_window(HIDDEN, THIRD, |_| {});
    // The display shows its second Space, so "current" and "first" differ.
    harness
        .mock_state
        .activate_workspace(TEST_DISPLAY_ID, SECOND, false);
    boot(&mut harness);
    assert_eq!(ecs_active_workspaces(harness.world()), vec![SECOND]);

    harness.run(vec![destroy(SpaceSelector::Next, true)]);

    assert_eq!(
        harness.mock_state.native_space_destroys(),
        vec![(THIRD, true)]
    );
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), SECOND);
    let world = harness.world();
    assert!(rows_of(world, THIRD).is_empty());
    assert_on_workspace!(world, HIDDEN, SECOND);
    assert_eq!(rows_of(world, SECOND).len(), 1);
    assert_not_on_workspace!(world, HIDDEN, FIRST);
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(ecs_active_workspaces(world), vec![SECOND]);
}

#[test]
fn deferred_destruction_keeps_rows_and_windows_until_confirmed() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness
        .mock_state
        .set_native_destroy_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);
    let frame_before = window_frame(harness.world(), HIDDEN);

    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    assert_eq!(
        harness.mock_state.native_space_destroys(),
        vec![(SECOND, true)]
    );
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND]
    );
    let world = harness.world();
    assert_eq!(
        window_row(world, HIDDEN),
        Some((SECOND, 0)),
        "nothing is re-homed before the destruction is observed"
    );
    assert_eq!(rows_of(world, SECOND).len(), 1);
    assert_eq!(window_frame(world, HIDDEN), frame_before);
    assert_eq!(destructions_pending(world), 1);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), FIRST);
    let world = harness.world();
    assert!(rows_of(world, SECOND).is_empty());
    assert_on_workspace!(world, HIDDEN, FIRST);
    assert_eq!(rows_of(world, FIRST).len(), 1);
    assert_eq!(destructions_pending(world), 0);
    assert_eq!(harness.mock_state.native_space_destroys().len(), 1);
}

#[test]
fn destruction_that_never_lands_is_abandoned_with_rows_intact() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(1);
    harness
        .mock_state
        .set_native_destroy_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![destroy(SpaceSelector::Next, false)]);
    harness.advance(NATIVE_DEADLINE);
    assert_eq!(
        harness.mock_state.native_space_destroys().len(),
        1,
        "a destruction the window server never applies is not resubmitted"
    );
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND]
    );
    assert_eq!(
        rows_of(harness.world(), SECOND).len(),
        1,
        "a Space still in the census keeps its rows"
    );
    assert_eq!(destructions_pending(harness.world()), 0);

    // Giving up released the guard: a fresh command is served.
    harness
        .mock_state
        .set_native_destroy_outcome(NativeRequestOutcome::Immediate);
    harness.run(vec![destroy(SpaceSelector::Next, false)]);
    assert_eq!(harness.mock_state.native_space_destroys().len(), 2);
    assert!(rows_of(harness.world(), SECOND).is_empty());
}

#[test]
fn uncertain_destruction_reconciles_whatever_the_os_did() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness
        .mock_state
        .set_native_destroy_outcome(NativeRequestOutcome::Uncertain { applies: true });
    boot(&mut harness);

    // The bridge reports failure after submission, without its sample.
    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    assert_eq!(
        harness.mock_state.native_space_destroys(),
        vec![(SECOND, true)]
    );
    assert_eq!(
        window_row(harness.world(), HIDDEN),
        Some((SECOND, 0)),
        "an unobserved outcome changes nothing yet"
    );

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), FIRST);
    let world = harness.world();
    assert!(rows_of(world, SECOND).is_empty());
    assert_on_workspace!(world, HIDDEN, FIRST);
    assert_eq!(destructions_pending(world), 0);
    assert_eq!(
        harness.mock_state.native_space_destroys().len(),
        1,
        "a destruction that may have applied is observed, not repeated"
    );
}

#[test]
fn uncertain_destruction_that_did_not_apply_leaves_the_desktop_alone() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness
        .mock_state
        .set_native_destroy_outcome(NativeRequestOutcome::Uncertain { applies: false });
    boot(&mut harness);

    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    harness.advance(NATIVE_DEADLINE);
    assert_eq!(harness.mock_state.native_space_destroys().len(), 1);
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND]
    );
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), SECOND);
    let world = harness.world();
    assert_eq!(window_row(world, HIDDEN), Some((SECOND, 0)));
    assert_eq!(rows_of(world, SECOND).len(), 1);
    assert_eq!(destructions_pending(world), 0);
}

#[test]
fn eventless_destruction_is_reconciled_by_the_census() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness
        .mock_state
        .set_native_destroy_outcome(NativeRequestOutcome::Deferred);
    harness.mock_state.set_native_lifecycle_notifies(false);
    boot(&mut harness);

    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);

    assert_eq!(harness.mock_state.window_workspace(HIDDEN), FIRST);
    let world = harness.world();
    assert!(
        rows_of(world, SECOND).is_empty(),
        "the census alone tells that the Desktop is gone"
    );
    assert_on_workspace!(world, HIDDEN, FIRST);
    assert_eq!(destructions_pending(world), 0);
}

#[test]
fn a_failed_census_is_not_absence() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness
        .mock_state
        .set_native_destroy_outcome(NativeRequestOutcome::Deferred);
    harness.mock_state.set_native_lifecycle_notifies(false);
    boot(&mut harness);

    // The window server stops answering census queries while a destruction
    // it never applies is in flight.
    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    assert_eq!(
        harness.mock_state.native_space_destroys(),
        vec![(SECOND, true)]
    );
    harness.mock_state.set_native_census_failing(true);
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND]
    );
    let world = harness.world();
    assert_eq!(
        rows_of(world, SECOND).len(),
        1,
        "an unanswered census never counts as the Desktop being gone"
    );
    assert_eq!(window_row(world, HIDDEN), Some((SECOND, 0)));
    assert_eq!(destructions_pending(world), 0, "the wait stays bounded");
    assert_eq!(harness.mock_state.native_space_destroys().len(), 1);
}

#[test]
fn a_failed_census_is_not_presence() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(1);
    harness
        .mock_state
        .set_native_create_outcome(NativeRequestOutcome::Deferred);
    harness.mock_state.set_native_lifecycle_notifies(false);
    boot(&mut harness);

    // The window server applies the creation but stops answering census
    // queries before the ECS can see it.
    harness.run(vec![create()]);
    let created = created_space(&harness);
    harness.mock_state.set_native_census_failing(true);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND, created]
    );
    let world = harness.world();
    assert!(
        rows_of(world, created).is_empty(),
        "an unanswered census never counts as the Desktop having appeared"
    );
    assert_eq!(creations_pending(world), 0, "the wait stays bounded");
    assert_eq!(harness.mock_state.native_space_creations().len(), 1);
}

#[test]
fn destroy_selectors_do_not_wrap_or_accept_zero() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(1);
    boot(&mut harness);

    harness.run(vec![
        destroy(SpaceSelector::Previous, false),
        destroy(SpaceSelector::Number(0), false),
        destroy(SpaceSelector::Number(3), false),
    ]);

    assert!(harness.mock_state.native_space_destroys().is_empty());
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND]
    );
    let world = harness.world();
    assert_eq!(rows_of(world, FIRST).len(), 1);
    assert_eq!(rows_of(world, SECOND).len(), 1);
    assert_eq!(destructions_pending(world), 0);
}

#[test]
fn repeated_destroyed_notifications_are_harmless() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    boot(&mut harness);

    harness.run(vec![destroy(SpaceSelector::Next, false)]);
    assert!(rows_of(harness.world(), SECOND).is_empty());

    harness
        .world()
        .write_message(Event::SpaceDestroyed { space_id: SECOND });
    harness
        .world()
        .write_message(Event::SpaceDestroyed { space_id: SECOND });
    harness.advance(NATIVE_REACTION);

    let world = harness.world();
    assert_eq!(rows_of(world, FIRST).len(), 1);
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_focused!(world, 0);
}

#[test]
fn migration_announced_before_windows_report_it_keeps_them_tiled() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness.mock_state.set_native_topology_lagging(true);
    boot(&mut harness);
    let frame_before = window_frame(harness.world(), HIDDEN);

    // The census drops the Desktop and the notification arrives while the
    // window it held still reports the destroyed Space.
    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST]
    );
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), SECOND);
    let world = harness.world();
    assert_eq!(
        window_row(world, HIDDEN),
        Some((SECOND, 0)),
        "a window whose new Space is not known yet keeps its row"
    );
    assert!(
        !is_floating(world, HIDDEN),
        "a stale membership is not a reason to float a tiled window"
    );
    assert_eq!(window_frame(world, HIDDEN), frame_before);
    assert_eq!(
        destructions_pending(world),
        1,
        "the notification does not cut the command's survivor wait short"
    );

    harness.mock_state.settle_native_topology();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), FIRST);
    let world = harness.world();
    assert!(rows_of(world, SECOND).is_empty());
    assert_eq!(window_row(world, HIDDEN), Some((FIRST, 0)));
    assert!(!is_floating(world, HIDDEN));
    assert_eq!(rows_of(world, FIRST).len(), 1, "no row is invented");
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_focused!(world, 0);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(destructions_pending(world), 0);
    assert_eq!(harness.mock_state.native_space_destroys().len(), 1);
}

#[test]
fn windows_whose_migration_is_never_reported_keep_their_rows() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness.mock_state.set_native_topology_lagging(true);
    boot(&mut harness);

    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST]
    );
    let world = harness.world();
    assert_eq!(
        window_row(world, HIDDEN),
        Some((SECOND, 0)),
        "the last known layout outlives a wait that ran out"
    );
    assert!(!is_floating(world, HIDDEN));
    assert_eq!(destructions_pending(world), 0, "the wait is bounded");
    assert!(
        !pump_awake(world),
        "nothing keeps polling for the migration"
    );
    assert_eq!(harness.mock_state.native_space_destroys().len(), 1);

    // The window server eventually reports the migration, and the OS
    // repeats the notification: the row is reconciled from that evidence.
    harness.mock_state.settle_native_topology();
    harness
        .world()
        .write_message(Event::SpaceDestroyed { space_id: SECOND });
    harness.advance(NATIVE_REACTION);
    let world = harness.world();
    assert!(rows_of(world, SECOND).is_empty());
    assert_eq!(window_row(world, HIDDEN), Some((FIRST, 0)));
    assert!(!is_floating(world, HIDDEN));
    assert_eq!(destructions_pending(world), 0);
    assert_eq!(harness.mock_state.native_space_destroys().len(), 1);
}

#[test]
fn desktop_closed_by_the_user_rehomes_its_windows_once_they_are_reported() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness.mock_state.set_native_topology_lagging(true);
    boot(&mut harness);

    // The user closes the Desktop in Mission Control; the window server
    // reports its window on the dead Space for a while.
    harness.mock_state.remove_workspace(TEST_DISPLAY_ID, SECOND);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), SECOND);
    let world = harness.world();
    assert_eq!(
        window_row(world, HIDDEN),
        Some((SECOND, 0)),
        "an external destruction waits for the same evidence a command does"
    );
    assert!(!is_floating(world, HIDDEN));
    assert_eq!(destructions_pending(world), 1);

    harness.mock_state.settle_native_topology();
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), FIRST);
    harness.advance(NATIVE_REACTION);
    let world = harness.world();
    assert!(rows_of(world, SECOND).is_empty());
    assert_eq!(window_row(world, HIDDEN), Some((FIRST, 0)));
    assert!(!is_floating(world, HIDDEN));
    assert_eq!(rows_of(world, FIRST).len(), 1);
    assert_focused!(world, 0);
    assert_eq!(destructions_pending(world), 0);
    assert!(
        harness.mock_state.native_space_destroys().is_empty(),
        "observing what the user did submits nothing"
    );
}

/// Regression: a `SpaceDestroyed` for a Desktop the census keeps listing —
/// and whose window keeps reporting it — is not evidence of anything. Its
/// ordinary rows must not be reconciled or re-homed on the OS's word alone;
/// only a fullscreen row's saved-slot restoration may act before the
/// census agrees, and this is not one.
#[test]
fn destroyed_notification_for_a_desktop_the_census_still_lists_moves_nothing() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    boot(&mut harness);

    harness
        .world()
        .write_message(Event::SpaceDestroyed { space_id: SECOND });
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, SECOND]
    );
    let world = harness.world();
    assert_eq!(
        window_row(world, HIDDEN),
        Some((SECOND, 0)),
        "a notification the census contradicts moves no ordinary row"
    );
    assert_eq!(rows_of(world, SECOND).len(), 1);
    assert_eq!(destroyed_rows(world), 0);
    assert!(!is_floating(world, HIDDEN));
    assert_eq!(
        destructions_pending(world),
        1,
        "the Space is watched for the census to agree, not written off"
    );

    harness.advance(NATIVE_DEADLINE);
    assert!(
        harness.mock_state.native_space_destroys().is_empty(),
        "nothing is destroyed on the OS's word alone"
    );
    let world = harness.world();
    assert_eq!(window_row(world, HIDDEN), Some((SECOND, 0)));
    assert_eq!(rows_of(world, SECOND).len(), 1);
    assert_eq!(
        destroyed_rows(world),
        0,
        "a row of a Space still listed is not marked destroyed"
    );
    assert!(!is_floating(world, HIDDEN));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(destructions_pending(world), 0, "the wait is bounded");
    assert!(!pump_awake(world), "nothing keeps polling for it");
}

#[test]
fn rows_kept_past_the_deadline_go_once_an_ordinary_refresh_empties_them() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    harness.mock_state.set_native_topology_lagging(true);
    boot(&mut harness);

    harness.run(vec![destroy(SpaceSelector::Next, true)]);
    harness.advance(NATIVE_DEADLINE);
    assert_eq!(
        harness.mock_state.display_workspaces(TEST_DISPLAY_ID),
        vec![FIRST, THIRD]
    );
    let world = harness.world();
    assert_eq!(window_row(world, HIDDEN), Some((SECOND, 0)));
    assert_eq!(
        destroyed_rows(world),
        1,
        "the row kept for the vanished Space is known for what it is"
    );
    assert_eq!(destructions_pending(world), 0);
    assert!(
        !pump_awake(world),
        "a row kept for later evidence is not polled for"
    );

    // The window server catches up but the OS never repeats itself. The
    // user leaves the Space and comes back; that ordinary refresh finds the
    // carried window on the Space on screen and takes it out of the row.
    harness.mock_state.settle_native_topology();
    assert_eq!(harness.mock_state.window_workspace(HIDDEN), FIRST);
    user_switches_native_space(&mut harness, THIRD);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        window_row(harness.world(), HIDDEN),
        Some((SECOND, 0)),
        "nothing scans for the migration on its own"
    );
    user_switches_native_space(&mut harness, FIRST);
    harness.advance(NATIVE_REACTION);

    let world = harness.world();
    assert_eq!(window_row(world, HIDDEN), Some((FIRST, 0)));
    assert!(!is_floating(world, HIDDEN));
    assert!(
        rows_of(world, SECOND).is_empty(),
        "the emptied row of the destroyed Space goes without a second SpaceDestroyed"
    );
    assert_eq!(destroyed_rows(world), 0);
    assert_eq!(
        rows_of(world, THIRD).len(),
        1,
        "the empty row of a live Space is left alone"
    );
    assert_eq!(rows_of(world, FIRST).len(), 1);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(destructions_pending(world), 0);
    assert!(!pump_awake(world));
    assert_eq!(harness.mock_state.native_space_destroys().len(), 1);
}

#[test]
fn query_state_keeps_unresolved_survivors_without_enumerating_the_destroyed_space() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD])
        .with_windows(1)
        .with_workspace_window(HIDDEN, SECOND, |_| {});
    boot(&mut harness);

    let mut query = SystemState::<QueryStateParams>::new(harness.world());
    let before = query
        .get(harness.world())
        .expect("query parameters")
        .extract()
        .expect("state before destruction");
    let frame_before = before
        .virtual_workspaces
        .iter()
        .flat_map(|row| &row.windows)
        .find(|window| window.window_id == HIDDEN)
        .expect("window on the second Desktop")
        .frame
        .expect("last-known frame");

    // The user closes the Desktop, but its native group cannot report where
    // it landed. Once the bounded observation ends, its last-known row stays.
    harness.mock_state.set_window_queries_failing(HIDDEN, true);
    harness.mock_state.remove_workspace(TEST_DISPLAY_ID, SECOND);
    harness
        .mock_state
        .set_workspace_queries_failing(SECOND, true);
    harness.advance(NATIVE_DEADLINE);
    assert_eq!(destroyed_rows(harness.world()), 1);
    assert_eq!(destructions_pending(harness.world()), 0);

    let params = query.get(harness.world()).expect("query parameters");
    let state = params
        .extract()
        .expect("a deleted Space must not make the state query unavailable");
    let retained = state
        .virtual_workspaces
        .iter()
        .find(|row| row.native_workspace_id == SECOND)
        .expect("last-known row retained");
    assert!(!retained.active);
    let survivor = retained
        .windows
        .iter()
        .find(|window| window.window_id == HIDDEN)
        .expect("unresolved survivor retained");
    assert_eq!(survivor.frame, Some(frame_before));
    assert!(!survivor.floating);
    assert!(!survivor.visible);
    assert!(!survivor.focused);
    let live = state
        .virtual_workspaces
        .iter()
        .find(|row| row.native_workspace_id == FIRST)
        .expect("unrelated live row available");
    assert!(live.active);
    assert!(live.windows.iter().any(|window| window.window_id == 0));
    assert_eq!(state.active.native_workspace_id, Some(FIRST));
    assert_ne!(state.active.focused_window_id, Some(HIDDEN));

    let window_set = params
        .extract_window_set()
        .expect("the scripting view also remains available");
    let retained = window_set.workspace_of(HIDDEN).expect("survivor's row");
    assert_eq!(retained.native_id, SECOND);
    assert!(!retained.active);
    let survivor = window_set.window(HIDDEN).expect("last-known survivor");
    assert_eq!(survivor.frame, Some(frame_before));
    assert!(!survivor.visible);
    assert!(!survivor.focused);
    assert_eq!(window_set.current().map(|row| row.native_id), Some(FIRST));
    assert_eq!(
        window_set.workspace_of(0).map(|row| row.native_id),
        Some(FIRST)
    );
    assert_ne!(window_set.focused(), Some(HIDDEN));

    // With the destroyed row still retained, both the active Space and the
    // selected row of another live Space must continue reporting real errors.
    for workspace_id in [FIRST, THIRD] {
        harness
            .mock_state
            .set_workspace_queries_failing(workspace_id, true);
        let params = query.get(harness.world()).expect("query parameters");
        assert!(matches!(
            params.extract(),
            Err(crate::errors::Error::Generic(_))
        ));
        assert!(matches!(
            params.extract_window_set(),
            Err(crate::errors::Error::Generic(_))
        ));
        harness
            .mock_state
            .set_workspace_queries_failing(workspace_id, false);
    }
}

// --- Moving and sending windows ---

#[test]
fn spacesend_moves_a_tiled_window_and_keeps_the_source_space_showing() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    boot(&mut harness);
    assert_focused!(harness.world(), 0);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.window_workspace(1), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert!(!is_floating(world, 0), "a tiled window stays tiled");
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(
        focused_window(world),
        Some(1),
        "focus stays on the source Space with the window that remains"
    );
    assert!(!space_move_pending(world, 0));
}

#[test]
fn spacesend_of_the_last_window_stays_put_and_clears_focus() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(1);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert!(
        harness.mock_state.native_space_focuses().is_empty(),
        "sending the last window is not a reason to follow it"
    );
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    let world = harness.world();
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(
        focused_window(world),
        None,
        "nothing on the source Space is left to focus"
    );
    assert!(!space_move_pending(world, 0));
}

#[test]
fn spacesend_lands_in_the_destination_selected_row() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_config(three_rows_config())
        .with_windows(2);
    boot(&mut harness);
    let selected = selected_row(harness.world(), SECOND);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, selected)));
    assert_eq!(rows_of(world, SECOND).len(), 3, "no row is invented");
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
}

#[test]
fn spacemove_switches_and_focuses_only_after_move_and_activation_confirm() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SECOND);
    let world = harness.world();
    assert_eq!(ecs_active_workspaces(world), vec![SECOND]);
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_focused!(world, 0);
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
}

#[test]
fn spacemove_does_not_fake_the_switch_before_activation_is_observed() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);
    assert_eq!(harness.mock_state.pending_native_requests(), 1);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(
        ecs_active_workspaces(world),
        vec![FIRST],
        "the ECS does not declare a switch the window server has not made"
    );
    assert!(native_switch_pending(world));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SECOND);
    let world = harness.world();
    assert_eq!(ecs_active_workspaces(world), vec![SECOND]);
    assert_focused!(world, 0);
    assert!(!native_switch_pending(world));
    assert!(!space_move_pending(world, 0));
    assert_eq!(
        harness.mock_state.native_space_focuses().len(),
        1,
        "the activation is confirmed, not resubmitted"
    );
}

#[test]
fn spacemove_follow_gives_up_when_activation_never_confirms() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    assert!(native_switch_pending(harness.world()));

    harness.advance(NATIVE_DEADLINE);
    assert_eq!(harness.mock_state.native_space_focuses().len(), 1);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    let world = harness.world();
    assert!(!native_switch_pending(world));
    assert!(!space_move_pending(world, 0));
    assert_eq!(
        ecs_active_workspaces(world),
        vec![FIRST],
        "an unconfirmed switch is never assumed"
    );
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(
        focused_windows(world),
        vec![1],
        "the window hidden behind the source Space lets go of focus; its neighbour takes it"
    );
}

/// Regression: a whole move whose destination the window server refuses to
/// activate leaves the moved window hidden behind the source Space, which
/// stays on screen. The window must not keep the focus marker there, or
/// every command keeps targeting a window nobody can see and focus recovery
/// finds nothing to repair; the source's neighbour is focused instead.
#[test]
fn spacemove_whose_activation_is_refused_settles_focus_on_the_source() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(2)
        .with_focused_window(0);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Rejected);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert_eq!(harness.mock_state.pending_native_requests(), 0);
    let world = harness.world();
    assert_eq!(
        ecs_active_workspaces(world),
        vec![FIRST],
        "a refused activation switches nothing"
    );
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(
        focused_windows(world),
        vec![1],
        "the source keeps a visible window focused; the hidden one lets go"
    );
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
}

/// Regression: settling the source's focus after a failed activation must
/// not override what the user focused meanwhile. The moved window lost the
/// marker to the user's choice already; nothing hands it to the neighbour.
#[test]
fn failed_activation_respects_the_focus_the_user_chose_meanwhile() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_windows(3)
        .with_focused_window(0);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert!(native_switch_pending(harness.world()));

    // Still on the source Space, the user focuses a window that is not the
    // moved one's neighbour.
    harness.mock_state.focus_window(2);
    harness
        .world()
        .write_message(Event::WindowFocused { window_id: 2 });
    harness.advance(NATIVE_REACTION);
    assert_eq!(focused_windows(harness.world()), vec![2]);

    harness.advance(NATIVE_DEADLINE);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.native_space_focuses().len(), 1);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    let world = harness.world();
    assert!(!native_switch_pending(world));
    assert!(!space_move_pending(world, 0));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(window_row(world, 2), Some((FIRST, 0)));
    assert_eq!(
        focused_windows(world),
        vec![2],
        "the user's focus stands; the failed activation does not reassign it"
    );
}

#[test]
fn spacesend_leaves_layout_and_focus_alone_until_membership_is_confirmed() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);
    let frame_before = window_frame(harness.world(), 0);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    let world = harness.world();
    assert!(space_move_pending(world, 0));
    assert_eq!(
        window_row(world, 0),
        Some((FIRST, 0)),
        "the strip is not rewritten on a mere submission"
    );
    assert_eq!(window_frame(world, 0), frame_before);
    assert_focused!(world, 0);

    harness.advance(Duration::from_millis(500));
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "an unobserved move is not resubmitted"
    );
    assert_eq!(window_row(harness.world(), 0), Some((FIRST, 0)));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(focused_window(world), Some(1));
    assert!(!space_move_pending(world, 0));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert!(harness.mock_state.native_space_focuses().is_empty());
}

#[test]
fn closing_a_display_tab_does_not_focus_a_sibling_being_sent_to_another_space() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(3);
    boot(&mut harness);
    harness.run(vec![
        Event::Command {
            command: Command::Window(Operation::Focus(Direction::East)),
        },
        Event::Command {
            command: Command::Window(Operation::Stack(true)),
        },
        Event::Command {
            command: Command::Window(Operation::ToggleTabbedDisplay),
        },
    ]);
    assert_focused!(harness.world(), 1);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    // The user returns to the remaining display tab before the send settles.
    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 0);
    harness.mock_state.os_close_window(0);
    harness.advance(NATIVE_REACTION);

    assert!(space_move_pending(harness.world(), 1));
    assert_focused!(harness.world(), 2);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(1), SECOND);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert_focused!(harness.world(), 2);
}

#[test]
fn spacemove_same_target_and_out_of_range_selectors_move_nothing() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD]).with_windows(2);
    boot(&mut harness);

    harness.run(vec![
        space_move(SpaceSelector::Previous, MoveFocus::Follow),
        space_move(SpaceSelector::Number(1), MoveFocus::Follow),
        space_move(SpaceSelector::Number(0), MoveFocus::Stay),
        space_move(SpaceSelector::Number(4), MoveFocus::Stay),
    ]);

    assert!(harness.mock_state.workspace_moves().is_empty());
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_focused!(world, 0);
    assert!(!any_space_move_pending(world));

    // The command path itself is alive: a valid target still moves.
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(window_row(harness.world(), 0), Some((SECOND, 0)));
}

#[test]
fn spacemove_number_targets_another_display_in_global_order() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_display(
            EXT_DISPLAY_ID,
            ext_display_bounds(),
            vec![EXT_WORKSPACE_ID, EXT_SECOND],
        )
        .with_windows(2);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Number(4), MoveFocus::Stay)]);

    assert_eq!(submitted_batch(&harness), (vec![0], EXT_SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), EXT_SECOND);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.active_display(), TEST_DISPLAY_ID);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((EXT_SECOND, 0)));
    assert_eq!(rows_display(world, EXT_SECOND), EXT_DISPLAY_ID);
    assert_eq!(ecs_active_display(world), TEST_DISPLAY_ID);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(focused_window(world), Some(1));
}

#[test]
fn spacemove_relative_selector_counts_from_the_window_own_space() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_display(
            EXT_DISPLAY_ID,
            ext_display_bounds(),
            vec![EXT_WORKSPACE_ID, EXT_SECOND],
        )
        .with_windows(1)
        .with_workspace_window(100, EXT_WORKSPACE_ID, |window| {
            window.frame.min.x += TEST_DISPLAY_WIDTH;
            window.frame.max.x += TEST_DISPLAY_WIDTH;
        });
    boot(&mut harness);

    // The user focuses the window on the other display; the menu bar, and
    // with it the "current" Space, stays on the test display.
    harness.mock_state.focus_window(100);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 100);
    assert_eq!(harness.mock_state.active_display(), TEST_DISPLAY_ID);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    assert_eq!(
        submitted_batch(&harness),
        (vec![100], EXT_SECOND),
        "next counts from the Space the window is on, not the one under the menu bar"
    );
    assert_eq!(harness.mock_state.window_workspace(100), EXT_SECOND);
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert_eq!(window_row(harness.world(), 100), Some((EXT_SECOND, 0)));
}

/// Whether the ECS knows display `id`.
fn ecs_has_display(world: &mut World, id: u32) -> bool {
    world
        .query::<&Display>()
        .iter(world)
        .any(|display| display.id() == id)
}

/// Sends window 0, tiled, to the one Space of a display plugged in after
/// startup, while the window server already routes Spaces to that display
/// but does not list it yet: the ECS has no display to row the window
/// under. The window server applies the move at once.
fn move_to_an_unlisted_display() -> TestHarness {
    let mut harness = display_with(vec![FIRST]).with_windows(2);
    boot(&mut harness);
    harness
        .mock_state
        .add_display(EXT_DISPLAY_ID, ext_display_bounds(), vec![EXT_WORKSPACE_ID]);
    harness
        .mock_state
        .set_display_listing_lagging(EXT_DISPLAY_ID, true);

    harness.run(vec![space_move(SpaceSelector::Number(2), MoveFocus::Stay)]);
    assert_eq!(submitted_batch(&harness), (vec![0], EXT_WORKSPACE_ID));
    assert_eq!(harness.mock_state.window_workspace(0), EXT_WORKSPACE_ID);
    harness
}

#[test]
fn tiled_move_to_a_display_the_ecs_cannot_see_yet_waits_to_row_it_there() {
    let mut harness = move_to_an_unlisted_display();

    let world = harness.world();
    assert!(
        !is_floating(world, 0),
        "a tiled window is not floated for want of a display to row it under"
    );
    assert_eq!(
        window_row(world, 0),
        Some((FIRST, 0)),
        "the source row is not rewritten before the destination can take the window"
    );
    assert!(
        rows_of(world, EXT_WORKSPACE_ID).is_empty(),
        "no row is invented under a display the ECS has not seen"
    );
    assert!(!ecs_has_display(world, EXT_DISPLAY_ID));
    assert!(space_move_pending(world, 0));
    assert!(
        pump_awake(world),
        "the wait for the display keeps the pump awake"
    );

    // The display list catches up; the ECS learns the display, then the row.
    harness
        .mock_state
        .set_display_listing_lagging(EXT_DISPLAY_ID, false);
    harness.advance(NATIVE_REACTION * 2);
    assert_eq!(harness.mock_state.window_workspace(0), EXT_WORKSPACE_ID);
    let world = harness.world();
    assert!(ecs_has_display(world, EXT_DISPLAY_ID));
    assert_eq!(window_row(world, 0), Some((EXT_WORKSPACE_ID, 0)));
    assert_eq!(rows_display(world, EXT_WORKSPACE_ID), EXT_DISPLAY_ID);
    assert!(!is_floating(world, 0));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(focused_window(world), Some(1));
    assert!(!space_move_pending(world, 0));
    assert_eq!(ecs_active_display(world), TEST_DISPLAY_ID);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "the window is not moved again while its display is awaited"
    );
    assert!(harness.mock_state.native_space_focuses().is_empty());
}

#[test]
fn tiled_move_whose_display_is_never_listed_keeps_its_last_known_row() {
    let mut harness = move_to_an_unlisted_display();

    harness.advance(NATIVE_DEADLINE);

    assert_eq!(harness.mock_state.window_workspace(0), EXT_WORKSPACE_ID);
    let world = harness.world();
    assert_eq!(
        window_row(world, 0),
        Some((FIRST, 0)),
        "the last known layout outlives a wait that ran out"
    );
    assert!(
        !is_floating(world, 0),
        "a display that never turns up is not a reason to float a tiled window"
    );
    assert!(rows_of(world, EXT_WORKSPACE_ID).is_empty());
    assert!(!ecs_has_display(world, EXT_DISPLAY_ID));
    assert!(!space_move_pending(world, 0), "the wait is bounded");
    assert!(!pump_awake(world), "nothing keeps polling for the display");
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "the move is not resubmitted"
    );
    assert!(harness.mock_state.native_space_focuses().is_empty());
}

#[test]
fn spacemove_carries_the_native_tab_group_as_one_column() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(1);
    boot(&mut harness);

    // A second window of the same app opens as a native tab of window 0.
    let frame = window_frame(harness.world(), 0);
    let tab = harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, FIRST, 1, frame);
    harness.mock_state.merge_native_tabs(&[0, 1], 1);
    harness.world().trigger(SpawnWindowTrigger(vec![tab]));
    harness.run(vec![print_state()]);
    assert!(share_a_tab_column(harness.world(), &[0, 1]));
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 1);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    assert_eq!(harness.mock_state.window_memberships(0), vec![SECOND]);
    assert_eq!(harness.mock_state.window_memberships(1), vec![SECOND]);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), Some((SECOND, 0)));
    assert!(
        share_a_tab_column(world, &[0, 1]),
        "the tabs stay one column on the destination"
    );
    assert!(rows_of(world, FIRST)[0].2.is_empty());
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_memberships(0), vec![SECOND]);
    assert!(harness.mock_state.window_memberships(1).is_empty());
    assert_eq!(window_row(harness.world(), 0), Some((SECOND, 0)));
}

#[test]
fn spacesend_reconciles_managed_children_without_tab_or_source_focus_aliasing() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(3);
    boot(&mut harness);
    harness.mock_state.associate_window(0, 1);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(window_row(harness.world(), 1), Some((FIRST, 0)));
    // A child may take focus while the native request is still pending.
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 1);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.window_workspace(1), SECOND);
    assert_eq!(harness.mock_state.window_workspace(2), FIRST);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), Some((SECOND, 0)));
    assert_eq!(window_row(world, 2), Some((FIRST, 0)));
    assert!(!share_a_tab_column(world, &[0, 1]));
    assert_eq!(focused_windows(world), vec![2]);
    assert!(!any_space_move_pending(world));
}

#[test]
fn spacesend_of_parent_and_last_managed_child_stays_on_empty_source() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    boot(&mut harness);
    harness.mock_state.associate_window(0, 1);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.window_workspace(1), SECOND);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), Some((SECOND, 0)));
    assert!(!share_a_tab_column(world, &[0, 1]));
    assert!(rows_of(world, FIRST)[0].2.is_empty());
    assert!(focused_windows(world).is_empty());
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
}

#[test]
fn spacesend_keeps_native_tabs_separate_from_their_managed_child() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    boot(&mut harness);
    let frame = window_frame(harness.world(), 0);
    let tab = harness
        .mock_state
        .spawn_window(TEST_PROCESS_ID, FIRST, 2, frame);
    harness.mock_state.merge_native_tabs(&[0, 2], 2);
    harness.world().trigger(SpawnWindowTrigger(vec![tab]));
    harness.run(vec![print_state()]);
    assert!(share_a_tab_column(harness.world(), &[0, 2]));
    harness.mock_state.associate_window(2, 1);
    harness.mock_state.focus_window(2);
    harness.advance(NATIVE_REACTION);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    for id in [0, 1, 2] {
        assert_eq!(window_row(harness.world(), id), Some((SECOND, 0)));
    }
    assert_eq!(harness.mock_state.window_memberships(0), vec![SECOND]);
    assert_eq!(harness.mock_state.window_memberships(1), vec![SECOND]);
    assert_eq!(harness.mock_state.window_memberships(2), vec![SECOND]);
    let world = harness.world();
    assert!(share_a_tab_column(world, &[0, 2]));
    assert!(!share_a_tab_column(world, &[0, 1, 2]));
    assert!(rows_of(world, FIRST)[0].2.is_empty());
    assert!(focused_windows(world).is_empty());
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
}

#[test]
fn partial_move_reconciles_only_the_managed_child_that_landed_without_following() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(3);
    boot(&mut harness);
    harness.mock_state.associate_window(0, 1);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);

    // Only the independently movable child reaches the target. The parent
    // remains live on the source through the native confirmation deadline.
    harness
        .mock_state
        .update_window(1, |window| window.workspace_id = SECOND);
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert_eq!(harness.mock_state.window_workspace(1), SECOND);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_eq!(window_row(world, 1), Some((SECOND, 0)));
    assert_eq!(window_row(world, 2), Some((FIRST, 0)));
    assert_eq!(focused_windows(world), vec![2]);
    assert!(!any_space_move_pending(world));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
}

#[test]
fn closing_move_leader_reconciles_surviving_child_without_following_or_focusing_it() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(3);
    boot(&mut harness);
    harness.mock_state.associate_window(0, 1);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);

    harness.mock_state.os_close_window(0);
    harness.advance(NATIVE_REACTION);
    assert!(window_entity(harness.world(), 0).is_none());
    assert_eq!(window_row(harness.world(), 1), Some((FIRST, 0)));
    assert_eq!(focused_windows(harness.world()), vec![2]);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);

    assert_eq!(harness.mock_state.window_workspace(1), SECOND);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    let world = harness.world();
    assert_eq!(window_row(world, 1), Some((SECOND, 0)));
    assert_eq!(window_row(world, 2), Some((FIRST, 0)));
    assert_eq!(focused_windows(world), vec![2]);
    assert!(!any_space_move_pending(world));
}

#[test]
fn closing_move_leader_does_not_restart_the_surviving_child_deadline() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(3);
    boot(&mut harness);
    harness.mock_state.associate_window(0, 1);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    let submitted_at = harness.world().resource::<Time>().elapsed();
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    harness.advance(Duration::from_secs(1));
    harness.mock_state.os_close_window(0);
    harness.advance(NATIVE_REACTION);

    let remaining = (submitted_at + NATIVE_DEADLINE)
        .saturating_sub(harness.world().resource::<Time>().elapsed());
    harness.advance(remaining);
    assert_eq!(harness.mock_state.window_workspace(1), FIRST);
    assert_eq!(window_row(harness.world(), 1), Some((FIRST, 0)));
    assert!(!any_space_move_pending(harness.world()));
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert!(harness.mock_state.native_space_focuses().is_empty());

    // The expired handoff no longer blocks independent native commands.
    harness.run(vec![create()]);
    let created = created_space(&harness);
    assert_eq!(rows_of(harness.world(), created).len(), 1);
}

#[test]
fn closing_move_leader_preserves_submitted_activation_without_focusing_its_child() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(3);
    boot(&mut harness);
    harness.mock_state.associate_window(0, 1);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    assert_eq!(window_row(harness.world(), 1), Some((SECOND, 0)));
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);

    harness.mock_state.focus_window(2);
    harness.advance(NATIVE_REACTION);
    harness.mock_state.os_close_window(0);
    harness.advance(NATIVE_REACTION);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);

    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SECOND);
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    let world = harness.world();
    assert_eq!(window_row(world, 1), Some((SECOND, 0)));
    assert_eq!(focused_windows(world), vec![2]);
    assert!(!any_space_move_pending(world));
    assert!(!native_switch_pending(world));
}

#[test]
fn managed_child_closing_in_flight_leaves_no_slot_or_focus_after_parent_lands() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(3);
    boot(&mut harness);
    harness.mock_state.associate_window(0, 1);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    let child = find_window_entity(1, harness.world());

    harness.mock_state.os_close_window(1);
    harness.advance(NATIVE_REACTION);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);

    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    let world = harness.world();
    assert!(window_entity(world, 1).is_none());
    assert!(
        world
            .query::<&LayoutStrip>()
            .iter(world)
            .all(|strip| !strip.contains(child))
    );
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 2), Some((FIRST, 0)));
    assert_eq!(focused_windows(world), vec![2]);
    assert!(!any_space_move_pending(world));
}

#[test]
fn managed_children_keep_in_flight_mode_changes_and_resume_on_destination() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(4);
    boot(&mut harness);
    harness.mock_state.associate_window(0, 1);
    harness.mock_state.associate_window(0, 2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    harness.run(vec![toggle_float()]);
    harness.mock_state.os_minimize_window(1, true);
    harness.mock_state.os_minimize_window(2, true);
    harness.advance(NATIVE_REACTION);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);

    for id in [0, 1, 2] {
        assert_eq!(harness.mock_state.window_workspace(id), SECOND);
    }
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), None);
    assert_eq!(window_row(world, 2), None);
    assert!(is_floating(world, 1));
    assert!(!is_floating(world, 2));
    for id in [1, 2] {
        let entity = find_window_entity(id, world);
        assert!(world.entity(entity).contains::<MinimizedMarker>());
    }

    harness.mock_state.os_minimize_window(1, false);
    harness.mock_state.os_minimize_window(2, false);
    harness.advance(NATIVE_REACTION);

    let world = harness.world();
    assert!(is_floating(world, 1));
    assert_eq!(window_row(world, 1), None);
    assert!(!is_floating(world, 2));
    assert_eq!(window_row(world, 2), Some((SECOND, 0)));
    assert_eq!(window_row(world, 3), Some((FIRST, 0)));
    assert!(!share_a_tab_column(world, &[0, 2]));
}

#[test]
fn spacemove_carries_attached_windows_and_lands_when_a_closed_one_is_dropped() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness.mock_state.attach_window(0, SHEET);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(
        submitted_batch(&harness),
        (vec![0, SHEET], SECOND),
        "the sheet travels with its parent"
    );

    // The sheet is dismissed while the move is in flight.
    harness.mock_state.os_close_window(SHEET);
    harness.advance(NATIVE_REACTION);
    assert!(
        space_move_pending(harness.world(), 0),
        "the parent itself is still unconfirmed"
    );
    assert_eq!(window_row(harness.world(), 0), Some((FIRST, 0)));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert!(!space_move_pending(world, 0));
    assert_eq!(focused_window(world), Some(1));
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

/// Sets up a parent with a sheet whose move the window server applies but
/// can then no longer answer for: neither the sheet's Spaces nor whether it
/// still exists. The parent is reported on the target; the batch never is.
fn unanswerable_sheet_move(focus: MoveFocus) -> TestHarness {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness.mock_state.attach_window(0, SHEET);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, focus)]);
    assert_eq!(submitted_batch(&harness), (vec![0, SHEET], SECOND));

    harness.mock_state.set_window_queries_failing(SHEET, true);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    harness
}

#[test]
fn spacesend_on_an_unanswerable_sheet_rehomes_the_parent_without_completing() {
    let mut harness = unanswerable_sheet_move(MoveFocus::Stay);

    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    let world = harness.world();
    assert_eq!(
        window_row(world, 0),
        Some((SECOND, 0)),
        "the parent the window server reports on the target is not left ghosting its old row"
    );
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert!(
        !space_move_pending(world, 0),
        "a member that cannot be asked about ends the wait; the outcome is not success"
    );
    assert_eq!(
        focused_window(world),
        Some(1),
        "focus stays on the source Space with the window that remains"
    );
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert!(harness.mock_state.native_space_focuses().is_empty());

    harness.advance(NATIVE_DEADLINE);
    assert!(!any_space_move_pending(harness.world()));
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "an unconfirmable move is not resubmitted"
    );
}

#[test]
fn spacemove_on_an_unanswerable_sheet_never_activates_the_destination() {
    let mut harness = unanswerable_sheet_move(MoveFocus::Follow);

    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert!(
        harness.mock_state.native_space_focuses().is_empty(),
        "a batch that was not confirmed as a whole never switches Spaces"
    );
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_ne!(
        focused_window(world),
        Some(0),
        "a window that left the Space unconfirmed is not focused across it"
    );

    harness.advance(NATIVE_DEADLINE);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert_eq!(ecs_active_workspaces(harness.world()), vec![FIRST]);
}

#[test]
fn floating_window_stays_floating_across_a_spacesend() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    boot(&mut harness);
    let entity = find_window_entity(0, harness.world());
    harness.world().entity_mut(entity).insert(FloatingMarker);
    harness.advance(NATIVE_REACTION);
    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 0);
    assert!(is_floating(harness.world(), 0));
    let frame_before = window_frame(harness.world(), 0);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    let world = harness.world();
    assert!(is_floating(world, 0), "a float is not tiled by the move");
    assert_eq!(window_row(world, 0), None, "a float joins no strip");
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_ne!(
        focused_window(world),
        Some(0),
        "focus does not stay on a window that left the Space"
    );
    assert!(!space_move_pending(world, 0));
    assert_eq!(
        window_frame(world, 0),
        frame_before,
        "a float sent to a Space on the same display is not moved on screen"
    );
}

#[test]
fn floating_window_sent_to_another_display_is_reframed_only_once_confirmed() {
    let offset = IVec2::new(100, 50);
    let size = IVec2::new(TEST_WINDOW_WIDTH, 700);
    let mut harness = display_with(vec![FIRST])
        .with_display(
            EXT_DISPLAY_ID,
            ext_display_bounds(),
            vec![EXT_WORKSPACE_ID, EXT_SECOND],
        )
        .with_config(instant_floating_config())
        .with_window(0, |window| {
            window.frame = IRect::from_corners(offset, offset + size);
        })
        .with_focused_window(0);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);
    assert!(is_floating(harness.world(), 0));
    let frame_before = window_frame(harness.world(), 0);
    assert!(on_test_display(frame_before));

    harness.run(vec![space_move(SpaceSelector::Number(3), MoveFocus::Stay)]);
    assert_eq!(submitted_batch(&harness), (vec![0], EXT_SECOND));
    assert_eq!(
        window_frame(harness.world(), 0),
        frame_before,
        "nothing is moved on screen before the membership is confirmed"
    );
    assert!(space_move_pending(harness.world(), 0));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), EXT_SECOND);
    let world = harness.world();
    assert!(!space_move_pending(world, 0));
    assert!(is_floating(world, 0));
    let frame = window_frame(world, 0);
    assert!(
        on_ext_display(frame),
        "float at {frame:?} is framed on the display that owns its new Space"
    );
    assert_eq!(
        frame.min,
        ext_display_bounds().min + offset,
        "the float keeps its offset within the display"
    );
    assert_eq!(frame.size(), size);
    assert_eq!(ecs_active_display(world), TEST_DISPLAY_ID);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert!(harness.mock_state.native_space_focuses().is_empty());
}

#[test]
fn floating_window_stays_floating_across_a_spacemove() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_config(floating_config())
        .with_windows(1)
        .with_focused_window(0);
    boot(&mut harness);
    assert!(is_floating(harness.world(), 0));
    assert_focused!(harness.world(), 0);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SECOND);
    let world = harness.world();
    assert!(is_floating(world, 0));
    assert_eq!(window_row(world, 0), None);
    assert_eq!(ecs_active_workspaces(world), vec![SECOND]);
    assert_focused!(world, 0);
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
}

#[test]
fn spacesend_refuses_a_continuous_follower_without_toggling_follow() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_config(follower_config())
        .with_windows(1)
        .with_focused_window(0);
    boot(&mut harness);
    assert!(is_follower(harness.world(), 0));

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    harness.advance(NATIVE_DEADLINE);

    assert!(
        harness.mock_state.workspace_moves().is_empty(),
        "a one-shot send would race the follower's own carry-back"
    );
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    let world = harness.world();
    assert!(is_follower(world, 0), "follow is not silently switched off");
    assert!(is_floating(world, 0));
    assert!(!follower_move_pending(world, 0));
    assert!(!space_move_pending(world, 0));
    assert_focused!(world, 0);
}

/// A started single-display harness whose one window, focused, floats and
/// follows the current Space; `workspaces` are on offer.
fn follower_on_first(workspaces: Vec<WorkspaceId>) -> TestHarness {
    let mut harness = display_with(workspaces)
        .with_config(follower_config())
        .with_windows(1)
        .with_focused_window(0);
    boot(&mut harness);
    assert!(is_follower(harness.world(), 0));
    assert_focused!(harness.world(), 0);
    harness
}

#[test]
fn spacemove_of_a_follower_moves_it_before_switching() {
    let mut harness = follower_on_first(vec![FIRST, SECOND]);
    harness.mock_state.attach_window(0, SHEET);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    let frame_before = window_frame(harness.world(), 0);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);

    assert_eq!(
        submitted_batch(&harness),
        (vec![0, SHEET], SECOND),
        "a follower and its sheet are moved by the command's own batch, like any other window"
    );
    assert!(
        harness.mock_state.native_space_focuses().is_empty(),
        "the destination is not activated before the move is confirmed"
    );
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    let world = harness.world();
    assert!(space_move_pending(world, 0));
    assert!(
        !follower_move_pending(world, 0),
        "the follow machinery does not queue a competing carry"
    );
    assert_eq!(window_frame(world, 0), frame_before);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_focused!(world, 0);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SECOND);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.window_workspace(SHEET), SECOND);
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "exactly one move carries the follower: no competing submission"
    );
    let world = harness.world();
    assert!(is_follower(world, 0), "follow is preserved across the move");
    assert!(is_floating(world, 0));
    assert!(!follower_move_pending(world, 0));
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
    assert_eq!(ecs_active_workspaces(world), vec![SECOND]);
    assert_focused!(world, 0);
}

#[test]
fn spacemove_of_a_follower_with_an_unmovable_sheet_switches_nothing() {
    let mut harness = follower_on_first(vec![FIRST, SECOND]);
    // A sheet on every Space at once opens on the follower; the window
    // server refuses any batch that carries it.
    harness.mock_state.attach_window(0, SHEET);
    harness.mock_state.set_window_sticky(SHEET, true);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(harness.mock_state.pending_native_requests(), 0);
    assert!(
        harness.mock_state.native_space_focuses().is_empty(),
        "a move that was refused switches nothing"
    );
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), FIRST);
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert_eq!(harness.mock_state.window_workspace(SHEET), FIRST);
    let world = harness.world();
    assert!(is_follower(world, 0));
    assert!(!follower_move_pending(world, 0));
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_focused!(world, 0);
}

#[test]
fn spacemove_owns_a_managed_follower_child_while_the_user_switches_away() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD]).with_windows(2);
    boot(&mut harness);
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    harness.run(vec![toggle_float(), follow(true)]);
    harness.mock_state.focus_window(0);
    harness.advance(NATIVE_REACTION);
    harness.mock_state.associate_window(0, 1);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    user_switches_native_space(&mut harness, THIRD);
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert_eq!(harness.mock_state.window_workspace(1), FIRST);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert_eq!(ecs_active_workspaces(harness.world()), vec![THIRD]);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);

    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.window_workspace(1), SECOND);
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SECOND);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((SECOND, 0)));
    assert!(is_floating(world, 1));
    assert!(is_follower(world, 1));
    assert_eq!(window_row(world, 1), None);
    assert_eq!(focused_windows(world), vec![0]);
    assert!(!any_space_move_pending(world));
}

#[test]
fn spacemove_of_a_follower_owns_it_while_the_user_switches_away() {
    let mut harness = follower_on_first(vec![FIRST, SECOND, THIRD]);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));

    // The user switches to a third Space while the batch is in flight.
    user_switches_native_space(&mut harness, THIRD);
    harness.advance(NATIVE_REACTION);
    assert_eq!(ecs_active_workspaces(harness.world()), vec![THIRD]);
    assert!(
        !follower_move_pending(harness.world(), 0),
        "the Space change does not queue a carry under the explicit move"
    );
    assert!(space_move_pending(harness.world(), 0));
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "one write per window in flight"
    );

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(
        harness.mock_state.native_space_focuses(),
        vec![SECOND],
        "the command's destination is activated once the move is confirmed"
    );
    assert_eq!(harness.mock_state.active_workspace(TEST_DISPLAY_ID), SECOND);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    let world = harness.world();
    assert!(is_follower(world, 0));
    assert!(!follower_move_pending(world, 0));
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
    assert_eq!(ecs_active_workspaces(world), vec![SECOND]);
    assert_focused!(world, 0);
}

#[test]
fn spacemove_of_a_follower_carries_it_back_once_when_the_switch_never_confirms() {
    let mut harness = follower_on_first(vec![FIRST, SECOND]);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);
    let world = harness.world();
    assert!(native_switch_pending(world));
    assert!(space_move_pending(world, 0));
    assert!(!follower_move_pending(world, 0));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);

    // The activation never lands; the follower belongs on the Space the
    // user is actually on, and is carried there once the command lets go.
    harness.advance(NATIVE_DEADLINE);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], SECOND), (vec![0], FIRST)],
        "one carry back, after the explicit move released the window"
    );
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert_eq!(
        harness.mock_state.native_space_focuses().len(),
        1,
        "the activation is not resubmitted"
    );
    let world = harness.world();
    assert!(is_follower(world, 0));
    assert!(!space_move_pending(world, 0));
    assert!(!follower_move_pending(world, 0));
    assert!(!native_switch_pending(world));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
}

/// Regression: a follower whose explicit move never lands is handed back to
/// the follow machinery by where the user actually is, even when that is the
/// move's own target. The carry the user's switch would have queued was
/// withheld while the move owned the window, so unless the release queues
/// it the follower is stranded on the Space it left.
#[test]
fn spacemove_of_a_follower_that_never_lands_carries_it_to_the_target_the_user_reached() {
    let mut harness = follower_on_first(vec![FIRST, SECOND]);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);

    // The user reaches the destination on their own while the batch is in
    // flight.
    user_switches_native_space(&mut harness, SECOND);
    harness.advance(NATIVE_REACTION);
    assert_eq!(ecs_active_workspaces(harness.world()), vec![SECOND]);
    assert!(space_move_pending(harness.world(), 0));
    assert!(
        !follower_move_pending(harness.world(), 0),
        "the Space change does not queue a carry under the explicit move"
    );
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "no competing write while the explicit move is still owed"
    );

    // The window server never applies that move; whatever is asked next it
    // applies at once.
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Immediate);
    harness.advance(NATIVE_DEADLINE);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], SECOND), (vec![0], SECOND)],
        "one carry to the Space the user is on, after the explicit move let go"
    );
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert!(
        harness.mock_state.native_space_focuses().is_empty(),
        "a move that never landed activates nothing"
    );
    let world = harness.world();
    assert!(is_follower(world, 0));
    assert!(is_floating(world, 0));
    assert!(!space_move_pending(world, 0));
    assert!(!follower_move_pending(world, 0));
    assert!(!native_switch_pending(world));
    assert_eq!(ecs_active_workspaces(world), vec![SECOND]);
}

#[test]
fn follow_turned_off_in_flight_leaves_the_window_where_it_was_sent() {
    let mut harness = follower_on_first(vec![FIRST, SECOND]);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.native_space_focuses(), vec![SECOND]);
    assert!(native_switch_pending(harness.world()));
    assert!(space_move_pending(harness.world(), 0));

    // The user turns follow off while the switch is still unconfirmed.
    harness.run(vec![follow(false)]);
    let world = harness.world();
    assert!(!is_follower(world, 0));
    assert!(is_floating(world, 0));
    assert!(space_move_pending(world, 0));

    // The activation never lands. A window that stopped following belongs
    // where it was sent; nothing carries it back to the Space on screen.
    harness.advance(NATIVE_DEADLINE);
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], SECOND)],
        "no carry is queued for a window that no longer follows"
    );
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.native_space_focuses().len(), 1);
    let world = harness.world();
    assert!(!is_follower(world, 0));
    assert!(!follower_move_pending(world, 0));
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
    assert!(
        !pump_awake(world),
        "nothing is left pending for a window nobody carries"
    );
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
}

#[test]
fn a_second_move_waits_for_the_first_to_be_observed() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    // Two commands in the same frame, then another while still in flight.
    harness
        .world()
        .write_message(space_move(SpaceSelector::Next, MoveFocus::Stay));
    harness
        .world()
        .write_message(space_move(SpaceSelector::Next, MoveFocus::Stay));
    harness.advance(NATIVE_REACTION);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(
        submitted_batch(&harness),
        (vec![0], SECOND),
        "one write per window in flight"
    );

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(window_row(harness.world(), 0), Some((SECOND, 0)));
    assert!(!space_move_pending(harness.world(), 0));
    assert_eq!(focused_window(harness.world()), Some(1));

    // Once the first move is observed, the next command is served.
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], SECOND), (vec![1], SECOND)]
    );
    assert_eq!(window_row(harness.world(), 1), Some((SECOND, 0)));
    assert_eq!(focused_window(harness.world()), None);
}

#[test]
fn lifecycle_and_move_requests_do_not_overlap() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness
        .mock_state
        .set_native_destroy_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    // A destruction while a move is in flight.
    harness.run(vec![
        space_move(SpaceSelector::Next, MoveFocus::Stay),
        destroy(SpaceSelector::Number(3), false),
    ]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert!(
        harness.mock_state.native_space_destroys().is_empty(),
        "the layout cannot be rewritten under a move that is still landing"
    );
    assert_eq!(rows_of(harness.world(), THIRD).len(), 1);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert!(!space_move_pending(harness.world(), 0));

    // A creation while a destruction is in flight.
    harness.run(vec![destroy(SpaceSelector::Number(3), false), create()]);
    assert_eq!(
        harness.mock_state.native_space_destroys(),
        vec![(THIRD, false)]
    );
    assert!(harness.mock_state.native_space_creations().is_empty());
    assert_eq!(destructions_pending(harness.world()), 1);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert!(rows_of(harness.world(), THIRD).is_empty());
    assert_eq!(destructions_pending(harness.world()), 0);

    harness.run(vec![create()]);
    assert_eq!(harness.mock_state.native_space_creations().len(), 1);
}

#[test]
fn a_move_is_refused_while_a_native_switch_is_pending() {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD]).with_windows(2);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![
        Event::Command {
            command: Command::Space(SpaceOperation::Focus(SpaceSelector::Number(3))),
        },
        space_move(SpaceSelector::Next, MoveFocus::Stay),
    ]);
    assert!(native_switch_pending(harness.world()));
    assert!(
        harness.mock_state.workspace_moves().is_empty(),
        "a move is not resolved against a Space that is still switching"
    );
    assert_eq!(window_row(harness.world(), 0), Some((FIRST, 0)));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert!(!native_switch_pending(harness.world()));
    assert_eq!(ecs_active_workspaces(harness.world()), vec![THIRD]);
}

/// Writes `first` and `second` into the same frame and lets it play out
/// with every native request held in flight, so neither can settle and make
/// room for the other.
fn one_native_write_for(first: Event, second: Event) -> TestHarness {
    let mut harness = display_with(vec![FIRST, SECOND, THIRD]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    harness
        .mock_state
        .set_native_activation_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.world().write_message(first);
    harness.world().write_message(second);
    harness.advance(NATIVE_REACTION);
    harness
}

#[test]
fn same_frame_move_and_switch_leave_exactly_one_native_write() {
    let switch = || Event::Command {
        command: Command::Space(SpaceOperation::Focus(SpaceSelector::Number(3))),
    };
    let send = || space_move(SpaceSelector::Next, MoveFocus::Stay);

    for harness in [
        one_native_write_for(send(), switch()),
        one_native_write_for(switch(), send()),
    ] {
        let moves = harness.mock_state.workspace_moves().len();
        let switches = harness.mock_state.native_space_focuses().len();
        assert_eq!(
            moves + switches,
            1,
            "a move and a switch in one frame must not both reach the window server: \
             {moves} move(s), {switches} switch(es)"
        );
        assert_eq!(harness.mock_state.pending_native_requests(), 1);
    }
}

#[test]
fn refused_move_leaves_layout_and_focus_alone_and_clears_at_once() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Rejected);
    boot(&mut harness);
    let frame_before = window_frame(harness.world(), 0);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);

    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    assert_eq!(harness.mock_state.pending_native_requests(), 0);
    let world = harness.world();
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_eq!(window_frame(world, 0), frame_before);
    assert_focused!(world, 0);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert!(
        !space_move_pending(world, 0),
        "a request refused before submission has nothing to wait for"
    );

    harness.advance(NATIVE_DEADLINE);
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "a refused request is not retried"
    );

    // The next command is served without waiting out any deadline.
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Immediate);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(harness.mock_state.workspace_moves().len(), 2);
    assert_eq!(window_row(harness.world(), 0), Some((SECOND, 0)));
}

#[test]
fn move_that_never_lands_is_abandoned_without_resubmission() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    let world = harness.world();
    assert!(!space_move_pending(world, 0));
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_focused!(world, 0);
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
}

#[test]
fn uncertain_move_is_observed_rather_than_repeated() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Uncertain { applies: true });
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert_eq!(window_row(harness.world(), 0), Some((FIRST, 0)));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    let world = harness.world();
    assert_eq!(
        window_row(world, 0),
        Some((SECOND, 0)),
        "a move that may have applied is observed to completion"
    );
    assert!(!space_move_pending(world, 0));
    assert_eq!(focused_window(world), Some(1));
    assert_eq!(
        harness.mock_state.workspace_moves().len(),
        1,
        "the lost reply does not cause a second write"
    );
}

#[test]
fn move_whose_destination_vanishes_recovers_without_a_blind_rollback() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));

    // The user closes the destination Desktop; the window server drops the
    // in-flight move.
    harness.mock_state.remove_workspace(TEST_DISPLAY_ID, SECOND);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    let world = harness.world();
    assert!(rows_of(world, SECOND).is_empty());
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert!(!space_move_pending(world, 0));
    assert!(!native_switch_pending(world));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_focused!(world, 0);
}

#[test]
fn moved_window_closing_in_flight_leaves_nothing_pending() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));

    harness.mock_state.os_close_window(0);
    harness.advance(NATIVE_REACTION);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_DEADLINE);

    let world = harness.world();
    assert!(window_entity(world, 0).is_none());
    assert!(!any_space_move_pending(world));
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);

    // Nothing stale blocks the next request.
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(
        harness.mock_state.workspace_moves(),
        vec![(vec![0], SECOND), (vec![1], SECOND)]
    );
    assert_eq!(window_row(harness.world(), 1), Some((SECOND, 0)));
}

#[test]
fn user_focus_change_during_flight_is_respected() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));

    // The user clicks the other window while the move is in flight.
    harness.mock_state.focus_window(1);
    harness.advance(NATIVE_REACTION);
    assert_focused!(harness.world(), 1);

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    assert_eq!(harness.mock_state.window_workspace(1), FIRST);
    let world = harness.world();
    assert_eq!(
        window_row(world, 0),
        Some((SECOND, 0)),
        "the move lands on the window it was submitted for"
    );
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert_focused!(world, 1);
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
}

#[test]
fn window_floated_in_flight_lands_as_a_float() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));
    assert!(!is_floating(harness.world(), 0));

    // The user floats the window while its move is in flight.
    harness.run(vec![toggle_float()]);
    let world = harness.world();
    assert!(is_floating(world, 0));
    assert_eq!(window_row(world, 0), None);
    assert!(space_move_pending(world, 0));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    let world = harness.world();
    assert!(
        is_floating(world, 0),
        "the mode the window has when it lands is the one it keeps"
    );
    assert_eq!(
        window_row(world, 0),
        None,
        "a window floated in flight is not tiled into the destination"
    );
    assert_eq!(window_row(world, 1), Some((FIRST, 0)));
    assert!(!space_move_pending(world, 0));
    assert_ne!(focused_window(world), Some(0));
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert!(harness.mock_state.native_space_focuses().is_empty());
}

#[test]
fn window_tiled_in_flight_lands_in_the_destination_row() {
    let mut harness = display_with(vec![FIRST, SECOND])
        .with_config(floating_config())
        .with_windows(2)
        .with_focused_window(0);
    harness
        .mock_state
        .set_native_move_outcome(NativeRequestOutcome::Deferred);
    boot(&mut harness);
    assert!(is_floating(harness.world(), 0));
    assert_focused!(harness.world(), 0);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Stay)]);
    assert_eq!(submitted_batch(&harness), (vec![0], SECOND));

    // The user tiles the window while its move is in flight.
    harness.run(vec![toggle_float()]);
    let world = harness.world();
    assert!(!is_floating(world, 0));
    assert_eq!(window_row(world, 0), Some((FIRST, 0)));
    assert!(space_move_pending(world, 0));

    harness.mock_state.settle_native_requests();
    harness.advance(NATIVE_REACTION);
    assert_eq!(harness.mock_state.window_workspace(0), SECOND);
    let world = harness.world();
    assert!(!is_floating(world, 0));
    assert_eq!(
        window_row(world, 0),
        Some((SECOND, 0)),
        "a window tiled in flight joins the destination row rather than being left out of every row"
    );
    assert_eq!(window_row(world, 1), None, "the other float is untouched");
    assert!(!space_move_pending(world, 0));
    assert_ne!(focused_window(world), Some(0));
    assert_eq!(harness.mock_state.workspace_moves().len(), 1);
    assert!(harness.mock_state.native_space_focuses().is_empty());
}

#[test]
fn sticky_window_is_not_moved() {
    let mut harness = display_with(vec![FIRST, SECOND]).with_windows(2);
    boot(&mut harness);
    harness.mock_state.set_window_sticky(0, true);

    harness.run(vec![space_move(SpaceSelector::Next, MoveFocus::Follow)]);
    harness.advance(NATIVE_DEADLINE);

    assert_eq!(harness.mock_state.pending_native_requests(), 0);
    assert_eq!(harness.mock_state.window_workspace(0), FIRST);
    assert!(harness.mock_state.native_space_focuses().is_empty());
    let world = harness.world();
    assert_eq!(
        window_row(world, 0),
        Some((FIRST, 0)),
        "a window on every Space has no single source to move from"
    );
    assert!(!space_move_pending(world, 0));
    assert_eq!(ecs_active_workspaces(world), vec![FIRST]);
    assert_focused!(world, 0);
}

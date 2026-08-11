use std::time::Duration;

use bevy::app::PreUpdate;
use bevy::ecs::entity::{Entity, EntityHashSet};
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::system::{Commands, Query, Res, ResMut, Single};
use bevy::math::IRect;
use tracing::{Level, instrument};
use tracing::{debug, error, info, warn};

mod query;

use crate::config::Config;
use crate::ecs::display::FloatingLayer;
use crate::ecs::focus::FocusHistory;
use crate::ecs::layout::{Column, LayoutStrip, StackItem, clamp_origin_to_viewport};
use crate::ecs::params::{ActiveDisplay, ActiveDisplayMut, Windows};
use crate::ecs::workspace::FollowSpacePending;
use crate::ecs::{
    ActiveDisplayMarker, ActiveWorkspaceMarker, Bounds, DockPosition, FocusedMarker,
    FollowCurrentWorkspaceMarker, FullWidthMarker, InstantSpaceSwitch, MissionControlActive,
    NativeFullscreenMarker, RaiseWindow, SelectedVirtualMarker, SendMessageTrigger,
    SpawnCommandsExt, Timeout, Unmanaged,
};
use crate::errors::Error;
use crate::events::Event;
use crate::manager::{Application, Display, Origin, Size, Window, WindowManager, origin_from};
use crate::platform::WorkspaceId;
use crate::util::round_px;

// The command vocabulary itself lives in the `paneru-command` crate, shared with
// the Lua API in `crates/lua-api` so every host speaks the same types.
pub use paneru_shared_types::commands::{
    Command, Direction, MouseMove, MoveFocus, Operation, ResizeDirection, SpaceOperation,
    SpaceSelector,
};

/// The strips that are selected on their display but not the one on screen —
/// the parked virtual workspaces a window can be handed off to.
type OffscreenStrips<'w, 's> = Query<
    'w,
    's,
    (&'static mut LayoutStrip, &'static ChildOf),
    (With<SelectedVirtualMarker>, Without<ActiveWorkspaceMarker>),
>;

/// Every strip alongside the two flags that say where it sits: whether it is the
/// one currently on screen, and whether it is its display's selected virtual
/// workspace.
type StripsWithVisibility<'w, 's> = Query<
    'w,
    's,
    (
        &'static ChildOf,
        &'static LayoutStrip,
        Entity,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    ),
>;

pub fn register_commands(app: &mut bevy::app::App) {
    // Registered here (not with the Lua systems) so it's exercised by the mock
    // harness without a running interpreter.
    #[cfg(feature = "lua")]
    app.add_systems(PreUpdate, crate::ecs::layout_ops::apply_layout_ops);

    query::register_query_commands(app);
    // Empty store so the mock harness and saveless runs still have one to
    // answer from; the real app overwrites it from disk.
    app.init_resource::<crate::ecs::script_state::ScriptStateStore>();
    app.add_systems(PreUpdate, crate::ecs::script_state::script_state_handler);
    app.add_systems(
        PreUpdate,
        (
            command_quit_handler,
            command_restart_handler,
            print_internal_state_handler,
            mouse_to_next_display,
            resize_window,
            command_center_window,
            full_width_window,
            to_next_display,
            equalize_column,
            balance_strip,
            manage_window,
            stack_windows_handler,
            command_move_focus,
            command_focus_unmanaged,
            command_focus_managed,
            command_raise_floating,
            command_cycle_floating,
            command_toggle_floating_layer,
            command_swap_focus,
            snap_window,
        ),
    );
    app.add_systems(
        PreUpdate,
        (
            command_move_floating,
            follow_window,
            command_focus_native_space,
        ),
    );
}

pub fn filter_window_operations<'a, F: Fn(&Operation) -> bool>(
    messages: &'a mut MessageReader<Event>,
    filter: F,
) -> impl Iterator<Item = &'a Operation> {
    messages.read().filter_map(move |event| {
        if let Event::Command {
            command: Command::Window(op),
        } = event
            && filter(op)
        {
            Some(op)
        } else {
            None
        }
    })
}

fn resolve_native_space(
    spaces: &[WorkspaceId],
    current_workspace: WorkspaceId,
    selector: SpaceSelector,
) -> Option<WorkspaceId> {
    let current_index = spaces
        .iter()
        .position(|workspace| *workspace == current_workspace)?;
    match selector {
        SpaceSelector::Next => spaces.get(current_index + 1).copied(),
        SpaceSelector::Previous => current_index
            .checked_sub(1)
            .and_then(|index| spaces.get(index))
            .copied(),
        SpaceSelector::Number(number) => number
            .checked_sub(1)
            .and_then(|index| spaces.get(index))
            .copied(),
    }
}

#[allow(clippy::needless_pass_by_value)]
#[instrument(level = Level::DEBUG, skip_all)]
fn command_focus_native_space(
    mut messages: MessageReader<Event>,
    window_manager: Res<WindowManager>,
    mission_control: Res<MissionControlActive>,
    mut instant_space_switch: ResMut<InstantSpaceSwitch>,
) {
    for selector in messages.read().filter_map(|event| {
        let Event::Command {
            command: Command::Space(SpaceOperation::Focus(selector)),
        } = event
        else {
            return None;
        };
        Some(*selector)
    }) {
        if mission_control.0 {
            warn!("native Space focus is unavailable during Mission Control");
            continue;
        }
        if !instant_space_switch.should_begin_command() {
            warn!("native Space focus is already in progress");
            continue;
        }

        let target = window_manager
            .native_spaces()
            .and_then(|spaces| {
                let display_id = window_manager.active_display_id()?;
                let current_workspace = window_manager.active_display_space(display_id)?;
                resolve_native_space(&spaces, current_workspace, selector).ok_or_else(|| {
                    Error::InvalidInput(format!(
                        "native Space selector {selector:?} is out of range"
                    ))
                })
            })
            .and_then(|workspace_id| {
                window_manager
                    .focus_native_space(workspace_id)
                    .map(|switched| (workspace_id, switched))
            });

        match target {
            Ok((workspace_id, true)) => {
                instant_space_switch.begin_command();
                debug!(workspace_id, "focused native Space");
            }
            Ok((workspace_id, false)) => {
                debug!(workspace_id, "native Space is already focused");
            }
            Err(err) => warn!("could not focus native Space: {err}"),
        }
    }
}

/// Retrieves a window `Entity` in a specified direction relative to a `current_window_id` within a `LayoutStrip`.
///
/// # Arguments
///
/// * `direction` - The direction (e.g., `West`, `East`, `First`, `Last`, `North`, `South`).
/// * `current_window_id` - The `Entity` of the current window.
/// * `strip` - A reference to the `LayoutStrip` to search within.
///
/// # Returns
///
/// `Some(Entity)` with the found window's entity, otherwise `None`.
#[instrument(level = Level::DEBUG, ret)]
fn get_window_in_direction(
    direction: &Direction,
    entity: Entity,
    strip: &LayoutStrip,
) -> Option<Entity> {
    let index = strip.index_of(entity).ok()?;

    match direction {
        Direction::West => strip.left_neighbour(entity),
        Direction::East => strip.right_neighbour(entity),

        Direction::First => strip.first().ok().and_then(|column| column.top()),

        Direction::Last => strip.last().ok().and_then(|column| column.top()),

        Direction::Nth(index) => strip.get(*index).ok().and_then(|column| column.top()),

        Direction::North => match strip.get(index).ok()? {
            Column::Single(_) | Column::Tabs(_) | Column::Fullscren(_) => None,
            Column::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, item)| item.contains(entity))
                .and_then(|(index, _)| (index > 0).then(|| stack.get(index - 1)).flatten())
                .and_then(StackItem::top),
        },

        Direction::South => match strip.get(index).ok()? {
            Column::Single(_) | Column::Tabs(_) | Column::Fullscren(_) => None,
            Column::Stack(stack) => stack
                .iter()
                .enumerate()
                .find(|(_, item)| item.contains(entity))
                .and_then(|(index, _)| {
                    (index < stack.len() - 1)
                        .then(|| stack.get(index + 1))
                        .flatten()
                })
                .and_then(StackItem::top),
        },
    }
}

/// 45° direction cone, closest by squared Euclidean distance.
/// `First` / `Last` are strip-only and return `None`.
fn pick_nearest_in_direction(
    direction: &Direction,
    focused_center: bevy::math::IVec2,
    candidates: impl IntoIterator<Item = (Entity, bevy::math::IVec2)>,
) -> Option<Entity> {
    candidates
        .into_iter()
        .filter_map(|(entity, center)| {
            let dx = center.x - focused_center.x;
            let dy = center.y - focused_center.y;
            let in_direction = match direction {
                Direction::East => dx > 0 && dy.abs() <= dx.abs(),
                Direction::West => dx < 0 && dy.abs() <= dx.abs(),
                Direction::North => dy < 0 && dx.abs() <= dy.abs(),
                Direction::South => dy > 0 && dx.abs() <= dy.abs(),
                Direction::First | Direction::Last | Direction::Nth(_) => return None,
            };
            in_direction.then_some((entity, dx * dx + dy * dy))
        })
        .min_by_key(|(_, dist_sq)| *dist_sq)
        .map(|(entity, _)| entity)
}

fn visible_floating_entities(
    windows: &Windows,
    window_manager: &WindowManager,
    workspace_id: WorkspaceId,
    display_bounds: IRect,
) -> Vec<Entity> {
    let workspace_window_ids: std::collections::HashSet<_> = window_manager
        .windows_in_workspace(workspace_id)
        .ok()
        .map(|ids| ids.into_iter().collect())
        .unwrap_or_default();

    windows
        .iter()
        .filter_map(|(_, entity)| {
            let (window, _, Some(Unmanaged::Floating)) = windows.get_managed(entity)? else {
                return None;
            };
            if !workspace_window_ids.contains(&window.id()) {
                return None;
            }
            let frame = windows.frame(entity)?;
            (!display_bounds.intersect(frame).is_empty()).then_some(entity)
        })
        .collect()
}

fn nearest_float_in_direction(
    direction: &Direction,
    focused_entity: Entity,
    windows: &Windows,
    window_manager: &WindowManager,
    workspace_id: WorkspaceId,
    display_bounds: IRect,
) -> Option<Entity> {
    let focused_center = windows.frame(focused_entity)?.center();

    let candidates =
        visible_floating_entities(windows, window_manager, workspace_id, display_bounds)
            .into_iter()
            .filter(|entity| *entity != focused_entity)
            .filter_map(|entity| windows.frame(entity).map(|frame| (entity, frame.center())));

    pick_nearest_in_direction(direction, focused_center, candidates)
}

/// Handles the "focus" command, moving focus to a window in a specified direction.
///
/// # Arguments
///
/// * `direction` - The `Direction` to move focus (e.g., `Direction::East`).
/// * `current_window` - The `Entity` of the currently focused `Window`.
/// * `strip` - A reference to the active `LayoutStrip`.
/// * `windows` - A query for all `Window` components.
///
/// # Returns
///
/// `Some(Entity)` with the entity of the newly focused window, otherwise `None`.
fn command_move_focus(
    mut messages: MessageReader<Event>,
    windows: Windows,
    workspaces: Query<(&LayoutStrip, Entity, Option<&NativeFullscreenMarker>)>,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    mut focus_history: ResMut<FocusHistory>,
    mut commands: Commands,
) {
    let Some(Operation::Focus(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Focus(_))).next()
    else {
        return;
    };

    let active_strip = active_display.active_strip();

    // On a fullscreen space, swap to the last column in the workspace.
    if let Some(NativeFullscreenMarker {
        layout_strip,
        workspace_id,
        index: _,
    }) = active_display.fullscreen()
        && matches!(direction, Direction::West)
    {
        let mut strip = workspaces
            .into_iter()
            .find_map(|(strip, entity, _)| (entity == *layout_strip).then_some(strip));
        if strip.is_none() {
            strip = workspaces
                .into_iter()
                .find_map(|(strip, _, _)| (strip.id() == *workspace_id).then_some(strip));
        }

        if let Some(entity) = strip.and_then(|strip| strip.last().ok().and_then(|col| col.top())) {
            debug!("fullscreen: swap raising {entity}");
            focus_history.pending_focus = Some(entity);
            commands.focus_entity(entity, true);
        }
        return;
    }

    let Some((_, focused_entity)) = windows.focused() else {
        return;
    };

    if let Some((_, _, Some(Unmanaged::Floating))) = windows.get_managed(focused_entity)
        && !matches!(direction, Direction::Nth(_))
    {
        if let Some(entity) = nearest_float_in_direction(
            direction,
            focused_entity,
            &windows,
            &window_manager,
            active_strip.id(),
            active_display.bounds(),
        ) {
            focus_history.pending_focus = Some(entity);
            commands.focus_entity(entity, true);
        }
        return;
    }

    // If focus is on a window that no longer lives in the active strip
    // (e.g. it just became floating, was minimised on another row, or
    // the OS handed focus to a window we don't track on this strip),
    // `get_window_in_direction` would return None and the user would
    // be unable to leave that window. Enter the active strip from the
    // appropriate side so subsequent presses behave normally.
    let candidate = if active_strip.contains(focused_entity) {
        get_window_in_direction(direction, focused_entity, active_strip).or_else(|| {
            // At the right edge going East, enter the fullscreen workspaces.
            (matches!(direction, Direction::East)
                && active_strip.right_neighbour(focused_entity).is_none())
            .then(|| {
                workspaces
                    .iter()
                    .find(|(strip, _, fullscreen)| {
                        fullscreen.is_some() && strip.id() != active_strip.id()
                    })
                    .and_then(|(strip, _, _)| strip.get(0).ok().and_then(|col| col.top()))
            })
            .flatten()
        })
    } else {
        match direction {
            Direction::East | Direction::First => {
                active_strip.first().ok().and_then(|col| col.top())
            }
            Direction::West | Direction::Last => active_strip.last().ok().and_then(|col| col.top()),
            Direction::Nth(index) => active_strip
                .get(*index)
                .ok()
                .and_then(|column| column.top()),
            Direction::North | Direction::South => None,
        }
    };

    if let Some(entity) = candidate {
        focus_history.pending_focus = Some(entity);
        commands.focus_entity(entity, true);
        // Explicitly reshuffle so the target window is brought into view.
        // This avoids a race where focus-follows-mouse leaves skip_reshuffle
        // set, causing the WindowFocused handler to skip the reshuffle.
        commands.reshuffle_around(entity);
        return;
    }

    // Check if the movement can switch to another display.
    let Some(other_display) = active_display.other().next() else {
        return;
    };
    let change_display = match direction {
        Direction::North => active_display.bounds().min.y > other_display.bounds().min.y,
        Direction::South => active_display.bounds().min.y < other_display.bounds().min.y,
        _ => false,
    };
    debug!("moving focus to another display: {change_display}");
    if change_display {
        commands.trigger(SendMessageTrigger(Event::Command {
            command: Command::Mouse(MouseMove::ToNextDisplay),
        }));
    }
}

fn command_focus_unmanaged(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    focus_history: Res<FocusHistory>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::FocusUnmanaged))
        .next()
        .is_none()
    {
        return;
    }

    let display_bounds = active_display.bounds();
    let workspace_id = active_display.active_strip().id();
    let visible_floats =
        visible_floating_entities(&windows, &window_manager, workspace_id, display_bounds);
    let is_visible_float = |entity: Entity| -> bool { visible_floats.contains(&entity) };

    let target = focus_history
        .last_floating(workspace_id)
        .filter(|entity| is_visible_float(*entity))
        .or_else(|| visible_floats.into_iter().next());

    if let Some(entity) = target {
        commands.focus_entity(entity, true);
    }
}

fn command_focus_managed(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    focus_history: Res<FocusHistory>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::FocusManaged))
        .next()
        .is_none()
    {
        return;
    }

    let active_strip = active_display.active_strip();
    let workspace_id = active_strip.id();

    let target = focus_history
        .last_managed(workspace_id)
        .filter(|entity| active_strip.contains(*entity))
        .or_else(|| active_strip.all_columns().into_iter().next());

    if let Some(entity) = target {
        commands.focus_entity(entity, true);
        commands.reshuffle_around(entity);
    }
}

fn command_raise_floating(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    focus_history: Res<FocusHistory>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::RaiseFloating))
        .next()
        .is_none()
    {
        return;
    }

    let display_bounds = active_display.bounds();
    let workspace_id = active_display.active_strip().id();
    let visible_floats =
        visible_floating_entities(&windows, &window_manager, workspace_id, display_bounds);
    let is_visible_float = |entity: Entity| -> bool { visible_floats.contains(&entity) };

    let target = focus_history
        .last_floating(workspace_id)
        .filter(|entity| is_visible_float(*entity))
        .or_else(|| visible_floats.first().copied());

    for (_, entity) in windows.iter() {
        if is_visible_float(entity) && Some(entity) != target {
            commands.trigger(RaiseWindow {
                entity,
                with_strip: false,
            });
        }
    }

    if let Some(entity) = target {
        commands.focus_entity(entity, true);
    }
}

/// Cycles focus through the visible floating windows on the active workspace.
///
/// The floats are ordered by window ID, so repeated presses visit every
/// visible float in a stable rotation regardless of position or focus
/// history, wrapping around at the ends. When the focused window is not a
/// visible float, this enters the floating tier at the workspace's
/// last-focused float (like `FocusUnmanaged`).
#[allow(clippy::needless_pass_by_value)]
fn command_cycle_floating(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    focus_history: Res<FocusHistory>,
    mut commands: Commands,
) {
    let Some(Operation::CycleFloating(reverse)) = filter_window_operations(&mut messages, |op| {
        matches!(op, Operation::CycleFloating(_))
    })
    .next() else {
        return;
    };

    let display_bounds = active_display.bounds();
    let workspace_id = active_display.active_strip().id();
    let mut floats =
        visible_floating_entities(&windows, &window_manager, workspace_id, display_bounds)
            .into_iter()
            .filter_map(|entity| windows.get(entity).map(|window| (window.id(), entity)))
            .collect::<Vec<_>>();
    floats.sort_unstable_by_key(|(id, _)| *id);

    if floats.is_empty() {
        return;
    }

    let focused_index = windows
        .focused()
        .and_then(|(_, focused)| floats.iter().position(|(_, entity)| *entity == focused));

    let target = match focused_index {
        Some(index) => {
            let len = floats.len();
            let next = if *reverse {
                (index + len - 1) % len
            } else {
                (index + 1) % len
            };
            floats[next].1
        }
        None => focus_history
            .last_floating(workspace_id)
            .filter(|entity| floats.iter().any(|(_, float)| float == entity))
            .unwrap_or(floats[0].1),
    };

    commands.focus_entity(target, true);
}

/// Focus-and-raise are deliberately coupled here: macOS AX raise can't lift a
/// window above another app's frontmost window, so the target's app must be
/// made frontmost. Other windows in the new top tier are raised within their
/// own apps' stacks as a best-effort.
fn command_toggle_floating_layer(
    mut messages: MessageReader<Event>,
    active_display: ActiveDisplay,
    mut floating_layers: Query<&mut FloatingLayer>,
    focus_history: Res<FocusHistory>,
    window_manager: Res<WindowManager>,
    windows: Windows,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| {
        matches!(op, Operation::ToggleFloatingLayer)
    })
    .next()
    .is_none()
    {
        return;
    }

    let display_bounds = active_display.bounds();
    let active_strip = active_display.active_strip();
    let workspace_id = active_strip.id();

    let visible_floats =
        visible_floating_entities(&windows, &window_manager, workspace_id, display_bounds);
    let visible_float = |entity: Entity| -> bool {
        visible_floats.contains(&entity) && !active_strip.contains(entity)
    };
    let focused_entity = windows.focused().map(|(_, entity)| entity);
    let focused_is_floating = focused_entity.and_then(|entity| {
        if visible_float(entity) {
            Some(true)
        } else if active_strip.contains(entity) {
            Some(false)
        } else {
            None
        }
    });

    let floating_front = floating_layers
        .iter_mut()
        .find_map(|mut layer| {
            if layer.workspace_id == workspace_id {
                layer.front = focused_is_floating.map_or(!layer.front, |front| !front);
                Some(layer.front)
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            let mut layer = FloatingLayer::new(workspace_id);
            if let Some(front) = focused_is_floating {
                layer.front = !front;
            }
            let front = layer.front;
            commands.spawn((layer, ChildOf(active_display.entity())));
            front
        });

    let tiled_columns = active_strip.all_columns();
    let nearest_tiled_to_focused_float = || {
        let focused_center = focused_entity
            .filter(|entity| visible_float(*entity))
            .and_then(|entity| windows.moving_frame(entity))
            .map(|frame| frame.center())?;

        tiled_columns
            .iter()
            .copied()
            .filter_map(|entity| {
                let center = windows.moving_frame(entity)?.center();
                let dx = i64::from(center.x) - i64::from(focused_center.x);
                let dy = i64::from(center.y) - i64::from(focused_center.y);
                Some((entity, dx * dx + dy * dy))
            })
            .min_by_key(|(_, distance_squared)| *distance_squared)
            .map(|(entity, _)| entity)
    };

    let target = if floating_front {
        focus_history
            .last_floating(workspace_id)
            .filter(|entity| visible_float(*entity))
            .or_else(|| visible_floats.iter().copied().find(|e| visible_float(*e)))
    } else {
        focus_history
            .last_managed(workspace_id)
            .filter(|entity| active_strip.contains(*entity))
            .or_else(nearest_tiled_to_focused_float)
            .or_else(|| tiled_columns.first().copied())
    };

    if floating_front {
        windows
            .iter()
            .filter_map(|(_, entity)| {
                (visible_float(entity) && Some(entity) != target).then_some(entity)
            })
            .for_each(|entity| {
                commands.trigger(RaiseWindow {
                    entity,
                    with_strip: false,
                });
            });
    } else if let Some(entity) = target {
        commands.trigger(RaiseWindow {
            entity,
            with_strip: true,
        });
    }

    if let Some(entity) = target {
        commands.focus_entity(entity, true);
    }

    debug!("floating layer -> front: {floating_front}");
}

/// Handles the "swap" command, swapping the positions of the current window with another window in a specified direction.
///
/// # Arguments
///
/// * `direction` - The `Direction` to swap the window (e.g., `Direction::West`).
/// * `current` - The `Entity` of the currently focused `Window`.
/// * `active_display` - A mutable reference to the `ActiveDisplayMut` representing the active display.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
///
/// # Returns
///
/// `Some(Entity)` with the entity that was swapped with, otherwise `None`.
#[instrument(level = Level::DEBUG, skip_all)]
fn command_swap_focus(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    mut commands: Commands,
) {
    let Some(Operation::Swap(direction)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Swap(_))).next()
    else {
        return;
    };

    let active_strip = active_display.active_strip();
    let mut handler = || {
        let (_, current) = windows.focused()?;
        let index = active_strip.index_of(current).ok()?;
        let other_window = get_window_in_direction(direction, current, active_strip)?;
        let new_index = active_strip.index_of(other_window).ok()?;
        debug!(
            "swap {direction:?}: current={current} idx={index}, other={other_window} idx={new_index}, strip_len={}",
            active_strip.len()
        );

        if index == new_index
            && let Some(Column::Stack(stack)) = active_strip.get_column_mut(index)
        {
            let pos_a = stack.iter().position(|i| i.contains(current))?;
            let pos_b = stack.iter().position(|i| i.contains(other_window))?;
            stack.swap(pos_a, pos_b);
        } else if index < new_index {
            (index..new_index).for_each(|idx| active_strip.swap(idx, idx + 1));
        } else {
            (new_index..index)
                .rev()
                .for_each(|idx| active_strip.swap(idx, idx + 1));
        }
        Some(current)
    };

    // Keep the focused window on-screen, but don't anchor it: if its new
    // layout slot is already visible with the strip where it is, the strip
    // stays put and per-window animation slides the window into the slot.
    // Only when the slot would fall off the edge does the strip scroll —
    // and only by the shortfall.
    if let Some(window) = handler() {
        commands.ensure_visible(window);
    } else {
        debug!(
            "swap {direction:?}: handler returned None (focused={:?}, strip_len={})",
            windows.focused().map(|(_, e)| e),
            active_strip.len()
        );
    }

    // Floating windows are handled by command_move_floating; without the
    // strip-membership check every swap on a floating window would dispatch
    // a spurious ToNextDisplay.
    if windows.focused().is_some_and(|(_, current)| {
        active_strip.contains(current)
            && get_window_in_direction(direction, current, active_strip).is_none()
    }) {
        // Check if the movement can swap to another display.
        let bounds = active_display.bounds();
        let Some(other_display) = active_display.other().next() else {
            return;
        };
        let change_display = match direction {
            Direction::North => bounds.min.y > other_display.bounds().min.y,
            Direction::South => bounds.min.y < other_display.bounds().min.y,
            _ => false,
        };
        debug!("swapping window to another display: {change_display}");
        if change_display {
            commands.trigger(SendMessageTrigger(Event::Command {
                command: Command::Window(Operation::ToNextDisplay(MoveFocus::Follow)),
            }));
        }
    }
}

/// Moves the focused floating window one step in the specified direction.
///
/// Handles the dedicated `Operation::MoveFloating` as well as the same
/// `Operation::Swap` messages as `command_swap_focus`, so the `window_swap_*`
/// keybinds serve both tiers. The two handlers are disjoint by window state:
/// swap requires strip membership, while this one requires
/// `Unmanaged::Floating` — and adding that component removes the window from
/// every strip (see `window_unmanaged_trigger`).
///
/// The step is `float_move_step` of the viewport width (east/west) or height
/// (north/south); `First`/`Last` snap to the left/right viewport edge. The
/// window is kept inside the viewport.
#[instrument(level = Level::DEBUG, skip_all)]
#[allow(clippy::needless_pass_by_value, clippy::cast_possible_truncation)]
fn command_move_floating(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some(Operation::Swap(direction) | Operation::MoveFloating(direction)) =
        filter_window_operations(&mut messages, |op| {
            matches!(op, Operation::Swap(_) | Operation::MoveFloating(_))
        })
        .next()
    else {
        return;
    };

    let Some((_, entity, Some(Unmanaged::Floating))) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        return;
    };
    let Some(frame) = windows.moving_frame(entity) else {
        return;
    };

    let bounds = active_display.actual_bounds(&config);
    let step = config.float_move_step();
    let dx = (f64::from(bounds.width()) * step) as i32;
    let dy = (f64::from(bounds.height()) * step) as i32;

    let mut origin = frame.min;
    match direction {
        Direction::West => origin.x -= dx,
        Direction::East => origin.x += dx,
        Direction::North => origin.y -= dy,
        Direction::South => origin.y += dy,
        Direction::First => origin.x = bounds.min.x,
        Direction::Last => origin.x = bounds.max.x - frame.width(),
        Direction::Nth(_) => return,
    }
    // Keep the window inside the viewport. Applying `.min` before `.max`
    // pins windows larger than the viewport to its top/left edge.
    origin.x = origin.x.min(bounds.max.x - frame.width()).max(bounds.min.x);
    origin.y = origin
        .y
        .min(bounds.max.y - frame.height())
        .max(bounds.min.y);

    debug!("moving floating window {entity} {direction:?} to {origin:?}");
    commands.reposition_entity(entity, origin);
}

/// Centers the focused window on the active display.
fn command_center_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Center))
        .next()
        .is_none()
    {
        return;
    }

    if let Some((_, entity)) = windows.focused()
        && let Some(size) = windows.size(entity)
        && let Some(mut origin) = windows.origin(entity)
    {
        let center = active_display.bounds().center().x;
        origin.x = center - size.x / 2;

        if active_display.active_strip().contains(entity)
            && let Some(layout_position) = windows.layout_position(entity)
        {
            // Directly reposition the strip (bypasses hidden_ratio check).
            let strip_position = origin - layout_position.0;
            commands.reposition_entity(active_display.active_strip_entity(), strip_position);
        } else {
            commands.reposition_entity(entity, origin);
        }

        window_manager.warp_mouse(active_display.bounds().center());
    }
}

/// Resizes the focused window based on preset column widths.
///
/// # Arguments
///
/// * `active_display` - A mutable reference to the `Display` resource.
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for all `Window` components.
/// * `commands` - Bevy commands to trigger events.
/// * `config` - The `Config` resource.
fn resize_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some(operation) = filter_window_operations(&mut messages, |op| {
        matches!(op, Operation::Resize(_) | Operation::SetWidth(_))
    })
    .next() else {
        return;
    };

    let Some((frame, entity)) = windows
        .focused()
        .and_then(|(_, entity)| windows.frame(entity).zip(Some(entity)))
    else {
        return;
    };
    if windows.full_width(entity).is_some()
        && let Ok(mut cmds) = commands.get_entity(entity)
    {
        cmds.try_remove::<FullWidthMarker>();
    }

    let viewport = active_display.actual_bounds(&config);
    let current_ratio = f64::from(frame.width()) / f64::from(viewport.width());
    let widths = config.preset_column_widths();
    let fallback = *widths.first().unwrap_or(&0.5);
    let cycle = config.window_resize_cycle();
    let next_ratio = match operation {
        Operation::SetWidth(ratio) if ratio.is_finite() && *ratio > 0.0 => *ratio,
        Operation::Resize(ResizeDirection::Grow) => widths
            .iter()
            .copied()
            .find(|&r| r > current_ratio + 0.05)
            .unwrap_or_else(|| {
                if cycle {
                    fallback
                } else {
                    *widths.last().unwrap_or(&fallback)
                }
            }),
        Operation::Resize(ResizeDirection::Shrink) => widths
            .iter()
            .rev()
            .copied()
            .find(|&r| r < current_ratio - 0.05)
            .unwrap_or_else(|| {
                if cycle {
                    *widths.last().unwrap_or(&fallback)
                } else {
                    fallback
                }
            }),
        _ => return,
    };

    let new_width = round_px(next_ratio * f64::from(viewport.width()));
    let size = Size::new(new_width, frame.height());

    let origin = clamp_origin_to_viewport(
        IRect::from_center_size(frame.center(), size).min,
        size,
        viewport,
    );
    commands.reposition_entity(entity, origin);

    // Resize all windows in the column so stacked siblings share the new width.
    let strip = active_display.active_strip();
    if let Some(Column::Stack(stack)) = strip
        .index_of(entity)
        .ok()
        .and_then(|idx| strip.get(idx).ok())
    {
        for sibling in stack.iter().flat_map(StackItem::window_iter) {
            if sibling != entity
                && let Some(size) = windows.size(sibling)
            {
                commands.resize_entity(sibling, size.with_x(new_width));
            }
        }
    }

    commands.resize_entity(entity, size);
    commands.reshuffle_around(entity);
}

fn full_width_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::FullWidth))
        .next()
        .is_none()
    {
        return;
    }

    let Some((_, entity)) = windows.focused() else {
        return;
    };

    let viewport = active_display.actual_bounds(&config);

    if let Some(marker) = windows.full_width(entity) {
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_remove::<FullWidthMarker>();
        }
        let w = round_px(marker.width_ratio * f64::from(viewport.width()));
        let bounds = active_display.actual_bounds(&config).size().with_x(w);
        commands.resize_entity(entity, bounds);
    } else {
        let strip = active_display.active_strip();
        if strip
            .index_of(entity)
            .ok()
            .and_then(|idx| strip.get(idx).ok())
            .is_some_and(|col| matches!(col, Column::Stack(_)))
        {
            _ = strip.unstack(entity);
        }
        let width_ratio = windows.width_ratio(entity).unwrap_or(0.5);
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_insert(FullWidthMarker { width_ratio });
        }
        commands.reposition_entity(entity, Origin::new(viewport.min.x, viewport.min.y));
        commands.resize_entity(entity, Size::new(viewport.width(), viewport.height()));
        commands.reshuffle_around(entity);
    }
}

/// Toggles the managed state of the focused window.
/// If the window is currently unmanaged, it becomes managed. If managed, it becomes unmanaged (floating).
///
/// # Arguments
///
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `commands` - Bevy commands to modify entities.
fn manage_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    followed: Query<(), With<FollowCurrentWorkspaceMarker>>,
    mut workspaces: Query<(&mut LayoutStrip, Has<ActiveWorkspaceMarker>)>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Manage))
        .next()
        .is_none()
    {
        return;
    }

    let Some((window, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        return;
    };
    debug!(
        "window: {} {entity} unmanaged: {}.",
        window.id(),
        unmanaged.is_some()
    );
    let was_unmanaged = unmanaged.is_some();
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        if was_unmanaged {
            entity_commands
                .try_remove::<FollowCurrentWorkspaceMarker>()
                .try_remove::<FollowSpacePending>()
                .try_remove::<Unmanaged>();
        } else {
            entity_commands.try_insert(Unmanaged::Floating);
        }
    }

    if was_unmanaged && followed.contains(entity) {
        debug!("disabled follow while managing window {}", window.id());
    }

    // Going floating -> managed only flips the component. Nothing else in
    // the pipeline reinserts the window into a strip, so if it had been
    // stripped of membership (spawn-floating path in window_unmanaged_trigger
    // strip.removes; orphan rescue in find_orphaned_workspaces despawns the
    // strip) the toggle is invisible — the window stays where it floated
    // and the user thinks the keybind is broken. Append to the active
    // strip and reshuffle so the layout pipeline tiles it.
    if was_unmanaged
        && !workspaces.iter().any(|(strip, _)| strip.contains(entity))
        && let Some(mut strip) = workspaces
            .iter_mut()
            .find_map(|(strip, active)| active.then_some(strip))
    {
        strip.append(entity);
        commands.reshuffle_around(entity);
    }
}

/// Toggles sticky-style following for the focused window. A followed window
/// is necessarily floating; disabling follow leaves it floating on its current
/// native Space so the command never unexpectedly changes its layout.
#[allow(clippy::needless_pass_by_value)]
fn follow_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    followed: Query<(), With<FollowCurrentWorkspaceMarker>>,
    mut commands: Commands,
) {
    let Some(Operation::Follow(requested)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Follow(_))).next()
    else {
        return;
    };

    let Some((window, entity)) = windows.focused() else {
        return;
    };
    if window.is_full_screen() {
        warn!("cannot follow a native-fullscreen window");
        return;
    }

    let currently_following = followed.contains(entity);
    let enable = requested.unwrap_or(!currently_following);
    let Ok(mut entity_commands) = commands.get_entity(entity) else {
        return;
    };
    if enable {
        if !currently_following {
            debug!("window {} will follow the current workspace", window.id());
            entity_commands.try_insert((FollowCurrentWorkspaceMarker, Unmanaged::Floating));
        }
    } else if currently_following {
        debug!(
            "window {} will stop following the current workspace",
            window.id()
        );
        entity_commands
            .try_remove::<FollowCurrentWorkspaceMarker>()
            .try_remove::<FollowSpacePending>();
    }
}

/// Moves the focused window to the next available display.
/// The window will be repositioned to the center of the new display.
///
/// # Arguments
///
/// * `focused_entity` - The `Entity` of the currently focused window.
/// * `windows` - A mutable query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `active_display` - A mutable reference to the `ActiveDisplayMut` resource.
/// * `commands` - Bevy commands to modify entities and trigger events.
fn to_next_display(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    mut other_workspaces: OffscreenStrips,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut commands: Commands,
) {
    let Some(Operation::ToNextDisplay(move_focus)) =
        filter_window_operations(&mut messages, |op| {
            matches!(op, Operation::ToNextDisplay(_))
        })
        .next()
    else {
        return;
    };

    let Some((window, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        return;
    };
    if unmanaged.is_some() {
        return;
    }

    // Width relative to the source display's usable viewport (dock- and
    // padding-adjusted). Captured before `other()` mutably borrows
    // `active_display`. This matches how `resize_window` computes the ratio
    // against `actual_bounds`, so a fixed (non-auto-hiding) dock is accounted
    // for on both the source and target displays.
    let source_viewport_width = active_display.actual_bounds(&config).width();

    let Some(other) = active_display.other().next() else {
        debug!("no other display to move window to.");
        return;
    };

    debug!(
        "moving window (id {}, {entity}) to display {}: {}.",
        window.id(),
        other.id(),
        other.width() / 2,
    );
    let center = other.bounds().center().x;
    let target_display_id = other.id();

    let Some(size) = windows.size(entity) else {
        return;
    };
    let width_ratio =
        (source_viewport_width > 0).then(|| f64::from(size.x) / f64::from(source_viewport_width));
    let dest = other.bounds().min.with_x(center - size.x / 2);
    commands.reposition_entity(entity, dest);

    if matches!(move_focus, MoveFocus::Follow) {
        window_manager.warp_mouse(other.bounds().center());
    }

    // Remove the window from the source strip.
    let source_neighbour = active_display
        .active_strip()
        .left_neighbour(entity)
        .or_else(|| active_display.active_strip().right_neighbour(entity));
    active_display.active_strip().remove(entity);
    if let Some(neighbour) = source_neighbour {
        commands.reshuffle_around(neighbour);
    }

    if matches!(move_focus, MoveFocus::Stay)
        && let Some(neighbour) = source_neighbour
    {
        commands.focus_entity(neighbour, false);
    }

    // Insert into the target display's selected strip.
    if let Ok(target_space_id) = window_manager.active_display_space(target_display_id)
        && let Some((mut target_strip, child)) = other_workspaces
            .iter_mut()
            .find(|(strip, _)| strip.id() == target_space_id)
    {
        target_strip.append(entity);
        commands.reshuffle_around(entity);

        // Add a delayed refresh of the window size - because the other display can have different bounds.
        let display_entity = child.parent();
        let moved_window = entity;
        let refresh_size = move |windows: Query<&Bounds, With<Window>>,
                                 displays: Query<(&Display, Option<&DockPosition>)>,
                                 mut commands: Commands,
                                 config: Res<Config>| {
            let Ok((display, dock)) = displays.get(display_entity) else {
                return;
            };
            let viewport_bounds = display.actual_display_bounds(dock, &config);
            if let Ok(Bounds(bounds)) = windows.get(moved_window) {
                debug!("Refreshing size of window {entity}");
                // Preserve the window's width ratio relative to the target
                // display's usable viewport (dock- and padding-adjusted), so a
                // fixed dock is accounted for consistently with the source.
                let width = width_ratio.map_or(bounds.x, |ratio| {
                    round_px(ratio * f64::from(viewport_bounds.width()))
                });
                let size = Size::new(width, viewport_bounds.height());
                commands.resize_entity(moved_window, size);
                commands.reshuffle_around(moved_window);
            }
        };
        let system_id = commands.register_system(refresh_size);
        Timeout::callback(Duration::from_millis(150), system_id, &mut commands);
    }
}

/// Moves the mouse pointer to the next available display.
#[instrument(level = Level::DEBUG, skip_all)]
fn mouse_to_next_display(
    mut messages: MessageReader<Event>,
    windows: Windows,
    layout_strips: Query<(&LayoutStrip, Entity)>,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    if !messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::Mouse(MouseMove::ToNextDisplay),
            }
        )
    }) {
        return;
    }

    let Some(cursor_position) = window_manager.cursor_position().map(origin_from) else {
        return;
    };
    let Some(other) = displays
        .into_iter()
        .find(|display| !display.bounds().contains(cursor_position))
    else {
        debug!("no other display to move mouse to.");
        return;
    };
    let Some((other_strip, _)) = window_manager
        .active_display_space(other.id())
        .ok()
        .and_then(|id| layout_strips.iter().find(|(strip, _)| strip.id() == id))
    else {
        return;
    };

    let visible_width = |frame: IRect| other.bounds().intersect(frame).width();
    let Some((frame, entity)) = other_strip
        .all_windows()
        .iter()
        .filter_map(|entity| windows.frame(*entity).zip(Some(*entity)))
        .max_by(|left, right| {
            if visible_width(left.0) < visible_width(right.0) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        })
    else {
        debug!("no suitable windows on the other display to move the mouse.");
        window_manager.warp_mouse(other.bounds().center());
        return;
    };

    let visible_frame = other.bounds().intersect(frame);
    debug!("warping mouse to {visible_frame:?}",);
    window_manager.warp_mouse(visible_frame.center());

    commands.focus_entity(entity, true);
}

/// Distributes heights equally among all windows in the currently focused stack.
fn equalize_column(
    mut messages: MessageReader<Event>,
    current_focus: Single<(&Window, Entity), With<FocusedMarker>>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Equalize))
        .next()
        .is_none()
    {
        return;
    }

    let (_, entity) = *current_focus;
    let active_strip = active_display.active_strip();
    let Ok(column) = active_strip
        .index_of(entity)
        .and_then(|index| active_strip.get(index))
    else {
        return;
    };

    if let Column::Stack(stack) = column {
        #[allow(clippy::cast_precision_loss)]
        let equal_height =
            active_display.actual_bounds(&config).height() / i32::try_from(stack.len()).unwrap();

        for item in &stack {
            for entity in item.window_iter() {
                if let Some(size) = windows.size(entity) {
                    commands.resize_entity(entity, size.with_y(equal_height));
                }
            }
        }
    }
}

/// Makes all columns in the active strip the same width as the focused window.
fn balance_strip(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Balance))
        .next()
        .is_none()
    {
        return;
    }

    let Some((_, focused_entity)) = windows.focused() else {
        return;
    };
    let Some(focused_width) = windows.size(focused_entity).map(|s| s.x) else {
        return;
    };

    let strip = active_display.active_strip();

    for column in strip.columns() {
        if matches!(column, Column::Fullscren(_)) {
            continue;
        }

        for entity in column.window_iter() {
            if windows.full_width(entity).is_some()
                && let Ok(mut cmds) = commands.get_entity(entity)
            {
                cmds.try_remove::<FullWidthMarker>();
            }

            if let Some(size) = windows.size(entity) {
                commands.resize_entity(entity, size.with_x(focused_width));
            }
        }
    }

    commands.reshuffle_around(focused_entity);
}

/// Slides the strip so the focused window is fully visible, snapping to the
/// nearest edge: left-aligned when the window overflows left, right-aligned
/// when it overflows right. No resize — the window keeps its current size.
/// Bypasses the lazy-viewport check since the user explicitly asked to reveal.
fn snap_window(
    mut messages: MessageReader<Event>,
    windows: Windows,
    active_display: ActiveDisplay,
    config: Res<Config>,
    mut commands: Commands,
) {
    if filter_window_operations(&mut messages, |op| matches!(op, Operation::Snap))
        .next()
        .is_none()
    {
        return;
    }

    let Some((_, entity)) = windows.focused() else {
        return;
    };
    let Some(layout_position) = windows.layout_position(entity) else {
        return;
    };
    let Some(mut frame) = windows.moving_frame(entity) else {
        return;
    };

    let display_bounds = active_display.actual_bounds(&config);

    // Clamp the frame into the display and reposition the *strip* (not the
    // window) so the layout stays consistent.
    let size = frame.size();
    frame.min = clamp_origin_to_viewport(frame.min, size, display_bounds);
    frame.max = frame.min + size;

    let strip_position = frame.min - layout_position.0;
    commands.reposition_entity(active_display.active_strip_entity(), strip_position);
}

#[instrument(level = Level::DEBUG, skip_all)]
pub fn stack_windows_handler(
    mut messages: MessageReader<Event>,
    windows: Windows,
    mut active_display: ActiveDisplayMut,
    // config: Res<Config>,
    mut commands: Commands,
) {
    let Some(Operation::Stack(stack)) =
        filter_window_operations(&mut messages, |op| matches!(op, Operation::Stack(_))).next()
    else {
        return;
    };

    if let Some((_, entity, unmanaged)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
        && unmanaged.is_none()
    {
        if windows.full_width(entity).is_some()
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            entity_commands.try_remove::<FullWidthMarker>();
        }
        let strip = active_display.active_strip();
        if *stack {
            _ = strip.stack(entity);
        } else {
            _ = strip.unstack(entity);
        }

        // Stacking/unstacking moves the focused window to a new column slot
        // (onto the left master, or out to its own column on the right).
        // Reshuffle around it so it is brought fully back into view; the
        // edge-clamp in reshuffle_layout_strip keeps the strip pinned so the
        // leftmost window touches the left edge and the rightmost the right.
        commands.reshuffle_around(entity);
    }
}

/// Dispatches a command based on the `CommandTrigger` event.
/// This function is a Bevy system that reacts to `CommandTrigger` events and executes the corresponding window manager command.
///
/// # Arguments
///
/// * `trigger` - The `On<CommandTrigger>` event trigger containing the command to process.
/// * `windows` - A query for `Window` components, their `Entity`, and whether they have the `Unmanaged` marker.
/// * `active_display` - A mutable reference to the `ActiveDisplayMut` resource.
/// * `window_manager` - The `WindowManager` resource for interacting with the window management logic.
/// * `commands` - Bevy commands to trigger events and modify entities.
/// * `config` - The `Config` resource, containing application settings.
#[instrument(level = Level::DEBUG, skip_all)]
pub fn command_quit_handler(
    mut messages: MessageReader<Event>,
    window_manager: Res<WindowManager>,
) {
    if messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::Quit
            }
        )
    }) {
        _ = window_manager.quit();
    }
}

#[instrument(level = Level::DEBUG, skip_all)]
pub fn command_restart_handler(mut messages: MessageReader<Event>) {
    if messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::Restart
            }
        )
    }) && let Err(err) = crate::platform::service::Service::request_restart()
    {
        error!("failed to restart service: {err}");
    }
}

#[instrument(level = Level::DEBUG, skip_all)]
fn print_internal_state_handler(
    mut messages: MessageReader<Event>,
    focused: Query<(&Window, Entity), With<FocusedMarker>>,
    windows: Query<(&Window, Entity, &ChildOf, Option<&Unmanaged>)>,
    apps: Query<&Application>,
    workspaces: StripsWithVisibility,
    displays: Query<(&Display, Entity, Has<ActiveDisplayMarker>)>,
) {
    if !messages.read().any(|event| {
        matches!(
            event,
            Event::Command {
                command: Command::PrintState,
            }
        )
    }) {
        return;
    }

    let focused = focused.single().ok();
    let print_window = |(window, entity, child, unmanaged): (
        &Window,
        Entity,
        &ChildOf,
        Option<_>,
    )| {
        let bundle_id = apps
            .get(child.parent())
            .ok()
            .and_then(|app| app.bundle_id())
            .unwrap_or_default();
        format!(
            "\tid: {}, {entity}, {}:{}, {}x{}{}{}, bundle: {}, role: {}, subrole: {}, title: '{:.70}'",
            window.id(),
            window.frame().min.x,
            window.frame().min.y,
            window.frame().width(),
            window.frame().height(),
            if focused.is_some_and(|(_, focus)| focus == entity) {
                ", focused"
            } else {
                ""
            },
            unmanaged.map(|m| format!(", {m:?}")).unwrap_or_default(),
            bundle_id,
            window.role().unwrap_or_default(),
            window.subrole().unwrap_or_default(),
            window.title().unwrap_or_default()
        )
    };

    let mut seen = EntityHashSet::new();

    for (display, display_entity, active) in displays {
        for (_, strip, strip_entity, active_workspace, selected) in workspaces
            .iter()
            .filter(|child| child.0.parent() == display_entity)
        {
            let windows = strip
                .all_windows()
                .iter()
                .filter_map(|entity| windows.get(*entity).ok())
                .inspect(|(_, entity, _, _)| {
                    seen.insert(*entity);
                })
                .map(print_window)
                .collect::<Vec<_>>();

            let display_id = display.id();
            info!(
                "Display {display_id}{}, workspace id {} ({strip_entity}){}{}: {strip}:\n{}",
                if active { ", active" } else { "" },
                strip.id(),
                if active_workspace { ", active" } else { "" },
                if selected { ", selected" } else { "" },
                windows.join("\n")
            );
        }
    }

    let remaining = windows
        .iter()
        .filter(|entity| !seen.contains(&entity.1))
        .map(print_window)
        .collect::<Vec<_>>();
    info!("Remaining:\n{}", remaining.join("\n"));

    if let Some(pool) = bevy::tasks::ComputeTaskPool::try_get() {
        info!("Running with {} threads", pool.thread_num());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    fn setup_world_with_layout() -> (World, LayoutStrip, Vec<Entity>) {
        let mut world = World::new();
        // e0, e1 are stacked, e2 is single, e3 is single
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]); // This will become a stack
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);
        strip.stack(entities[1]).unwrap(); // Stack e1 onto e0

        (world, strip, entities)
    }

    #[test]
    fn test_get_window_in_direction_simple() {
        let (_world, strip, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e2 = entities[2];
        let e3 = entities[3];
        let east = Direction::East;
        let west = Direction::West;

        // From e2, east should be e3, west should be e0 (top of stack)
        assert_eq!(get_window_in_direction(&east, e2, &strip), Some(e3));
        assert_eq!(get_window_in_direction(&west, e2, &strip), Some(e0));

        // From e3, west is e2, east is None
        assert_eq!(get_window_in_direction(&west, e3, &strip), Some(e2));
        assert_eq!(get_window_in_direction(&east, e3, &strip), None);

        // From e0, east is e2, west is None
        assert_eq!(get_window_in_direction(&east, e0, &strip), Some(e2));
        assert_eq!(get_window_in_direction(&west, e0, &strip), None);
    }

    #[test]
    fn test_get_window_in_direction_stacked() {
        let (_world, strip, entities) = setup_world_with_layout();
        let e0 = entities[0];
        let e1 = entities[1];
        let north = Direction::North;
        let south = Direction::South;

        // From e0 (top of stack), south should be e1, north is None
        assert_eq!(get_window_in_direction(&south, e0, &strip), Some(e1));
        assert_eq!(get_window_in_direction(&north, e0, &strip), None);

        // From e1 (bottom of stack), north should be e0, south is None
        assert_eq!(get_window_in_direction(&north, e1, &strip), Some(e0));
        assert_eq!(get_window_in_direction(&south, e1, &strip), None);
    }

    #[test]
    fn test_get_window_in_direction_adjacent_stacks() {
        // Layout: [Stack(e0, e1), Stack(e2, e3)]
        let mut world = World::new();
        let entities = world
            .spawn_batch(vec![(), (), (), ()])
            .collect::<Vec<Entity>>();

        let mut strip = LayoutStrip::default();
        strip.append(entities[0]);
        strip.append(entities[1]);
        strip.append(entities[2]);
        strip.append(entities[3]);
        strip.stack(entities[1]).unwrap(); // Stack e1 onto e0: [Stack(e0, e1), e2, e3]
        strip.stack(entities[3]).unwrap(); // Stack e3 onto e2: [Stack(e0, e1), Stack(e2, e3)]

        let east = Direction::East;
        let west = Direction::West;

        // From e0 (top-left), east should go to e2 (top-right)
        assert_eq!(
            get_window_in_direction(&east, entities[0], &strip),
            Some(entities[2])
        );
        // From e1 (bottom-left), east should go to e3 (bottom-right)
        assert_eq!(
            get_window_in_direction(&east, entities[1], &strip),
            Some(entities[3])
        );
        // From e2 (top-right), west should go to e0 (top-left)
        assert_eq!(
            get_window_in_direction(&west, entities[2], &strip),
            Some(entities[0])
        );
        // From e3 (bottom-right), west should go to e1 (bottom-left)
        assert_eq!(
            get_window_in_direction(&west, entities[3], &strip),
            Some(entities[1])
        );
    }

    #[test]
    fn pick_nearest_in_direction_east_picks_closer() {
        let mut world = World::new();
        let near = world.spawn(()).id();
        let far = world.spawn(()).id();
        let focused = bevy::math::IVec2::new(0, 0);
        let candidates = vec![
            (near, bevy::math::IVec2::new(10, 0)),
            (far, bevy::math::IVec2::new(50, 0)),
        ];
        assert_eq!(
            pick_nearest_in_direction(&Direction::East, focused, candidates),
            Some(near),
        );
    }

    #[test]
    fn pick_nearest_in_direction_respects_cone() {
        let mut world = World::new();
        let candidate = world.spawn(()).id();
        let focused = bevy::math::IVec2::new(0, 0);
        // y/x ratio > 1 → outside the 45° east cone.
        let candidates = vec![(candidate, bevy::math::IVec2::new(10, 20))];
        assert_eq!(
            pick_nearest_in_direction(&Direction::East, focused, candidates),
            None,
        );
    }

    #[test]
    fn pick_nearest_in_direction_ignores_wrong_side() {
        let mut world = World::new();
        let west_one = world.spawn(()).id();
        let focused = bevy::math::IVec2::new(0, 0);
        let candidates = vec![(west_one, bevy::math::IVec2::new(-10, 0))];
        assert_eq!(
            pick_nearest_in_direction(&Direction::East, focused, candidates),
            None,
        );
    }

    #[test]
    fn pick_nearest_in_direction_first_last_return_none() {
        let mut world = World::new();
        let any = world.spawn(()).id();
        let focused = bevy::math::IVec2::new(0, 0);
        let candidates = vec![(any, bevy::math::IVec2::new(10, 0))];
        assert_eq!(
            pick_nearest_in_direction(&Direction::First, focused, candidates.clone()),
            None,
        );
        assert_eq!(
            pick_nearest_in_direction(&Direction::Last, focused, candidates),
            None,
        );
    }
}

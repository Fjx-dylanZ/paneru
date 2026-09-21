//! Fresh Cocoa titlebar observations, never geometry, establish native tab identity.

use std::collections::HashSet;
use std::pin::Pin;
use std::time::Duration;

use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use super::layout::LayoutStrip;
use super::native_spaces::{NATIVE_CHECK_INTERVAL, NATIVE_REQUEST_TIMEOUT, SpaceMovePending};
use super::params::Windows;
use super::{
    ActiveWorkspaceMarker, MissionControlActive, SelectedVirtualMarker, SpawnCommandsExt,
    SpawnWindowTrigger,
};
use crate::config::Config;
use crate::errors::{Error, Result};
use crate::events::Event;
use crate::manager::{Application, Window, WindowManager};
use crate::platform::{Pid, PlatformCallbacks, WinID, WorkspaceId};

/// Positive identity evidence, separate from the layout's tab-shaped columns.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NativeTabGroup {
    pub members: Vec<(Entity, WinID)>,
    pub selected: Entity,
    pub workspace: WorkspaceId,
    /// Raw logical order, including tabs never exposed as an `AXWindow`. An
    /// absent ID is titlebar evidence, not permission to invent an ECS member.
    pub identity: Vec<(String, Option<WinID>)>,
}

#[derive(Component, Default)]
pub(crate) struct NativeTabGroups(pub Vec<NativeTabGroup>);

impl NativeTabGroups {
    pub(crate) fn is_inactive(&self, entity: Entity) -> bool {
        self.0.iter().any(|group| {
            group.selected != entity && group.members.iter().any(|(member, _)| *member == entity)
        })
    }
}

/// Native ordering can change before the cached titlebar selection settles.
/// Keep these reads and the guarded AX writes on the main thread.
#[derive(SystemParam)]
pub(crate) struct NativeTabSurfaces<'w, 's> {
    groups: Query<'w, 's, &'static NativeTabGroups>,
    manager: Res<'w, WindowManager>,
    _platform: Option<NonSend<'w, Pin<Box<PlatformCallbacks>>>>,
}

impl NativeTabSurfaces<'_, '_> {
    pub(crate) fn can_write(&self, entity: Entity, id: WinID) -> bool {
        !self.groups.iter().any(|groups| groups.is_inactive(entity))
            && self
                .manager
                .window_is_unordered(id)
                .is_ok_and(|unordered| !unordered)
    }

    pub(crate) fn can_raise_alongside(&self, entity: Entity, focus: Entity, id: WinID) -> bool {
        self.can_write(entity, id)
            && !self.groups.iter().any(|groups| {
                groups.0.iter().any(|group| {
                    group.members.iter().any(|(member, _)| *member == entity)
                        && group.members.iter().any(|(member, _)| *member == focus)
                })
            })
    }
}

fn sync_selected_geometry(
    changed: Populated<&NativeTabGroups, Changed<NativeTabGroups>>,
    mut windows: Query<(&mut super::Position, &mut super::Bounds), Without<super::FloatingMarker>>,
) {
    for groups in changed {
        for group in &groups.0 {
            if let Ok((mut position, mut bounds)) = windows.get_mut(group.selected) {
                position.set_changed();
                bounds.set_changed();
            }
        }
    }
}

/// Geometry echoes settle independently of prompt lifecycle observations.
/// Neither moves nor our own reads extend a lifecycle retry's deadline.
#[derive(Component, Default)]
pub(crate) struct NativeTabsDirty {
    until: Duration,
    next_check: Option<Duration>,
    geometry_after: Option<Duration>,
    discovery_pending: bool,
}

const GEOMETRY_SETTLE: Duration = Duration::from_millis(150);

impl NativeTabsDirty {
    fn is_due(&self, now: Duration) -> bool {
        self.next_check.is_some_and(|check| now >= check)
            || self.geometry_after.is_some_and(|settled| now >= settled)
    }

    fn is_complete(&self) -> bool {
        self.next_check.is_none() && self.geometry_after.is_none()
    }
}

pub(crate) struct NativeTabsPlugin;

impl Plugin for NativeTabsPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(reconcile_after_move);
        app.add_systems(
            PreUpdate,
            (
                dirty_native_tabs,
                discover_exposed_native_tabs,
                reconcile_native_tabs,
                sync_selected_geometry,
            )
                .chain()
                .after(super::systems::pump_events)
                .after(super::triggers::mission_control_trigger),
        );
        app.add_systems(
            Update,
            (
                discover_native_tabs,
                reconcile_native_tabs,
                sync_selected_geometry,
            )
                .chain()
                .after(super::triggers::apply_window_defaults)
                .after(super::systems::auto_discover_unmanaged_focused_windows)
                .before(super::triggers::window_focused_trigger)
                .before(super::triggers::apply_window_positions),
        );
    }
}

fn enabled(config: &Config) -> bool {
    config.native_tabs_enabled()
}

fn known_native_sibling(
    windows: &Windows,
    owner: (Entity, WinID),
    member: (Entity, WinID),
    title: &str,
) -> bool {
    windows.native_tab_groups(owner.0).is_some_and(|groups| {
        groups.0.iter().any(|group| {
            group.members.contains(&owner)
                && group.members.contains(&member)
                && group
                    .identity
                    .iter()
                    .any(|(known_title, id)| known_title == title && *id == Some(member.1))
        })
    })
}

type ObservedTabWindow<'a> = (Entity, &'a Window, String, Vec<WorkspaceId>);

fn observe_app_windows<'a>(
    windows: &'a Windows,
    manager: &WindowManager,
    pid: Pid,
    live: &HashSet<WinID>,
) -> Result<Vec<ObservedTabWindow<'a>>> {
    let mut observed = Vec::new();
    for (window, entity) in windows.iter() {
        if !live.contains(&window.id()) {
            continue;
        }
        let owner = window.pid()?;
        if owner != pid {
            continue;
        }
        // Inactive native tabs can remain live with no memberships and no
        // accessible AX metadata. Title-change notifications, not observations,
        // own invalidation of their last usable title.
        let memberships = manager.window_workspaces(window.id())?;
        let title = window
            .title()
            .map_err(|err| Error::Generic(format!("window {} AXTitle: {err}", window.id())))?;
        observed.push((entity, window, title, memberships));
    }
    Ok(observed)
}

/// A complete application observation. Every known live title participates in
/// uniqueness checks, including ordinary windows that happen to have the same title.
/// An unexposed startup tab retains its titlebar identity without a synthetic ID.
pub(crate) fn observe_app_groups(
    windows: &Windows,
    manager: &WindowManager,
    pid: Pid,
) -> Result<Vec<NativeTabGroup>> {
    observe_live_app_groups(windows, manager, pid, &manager.window_census()?)
}

fn observe_live_app_groups(
    windows: &Windows,
    manager: &WindowManager,
    pid: Pid,
    live: &HashSet<WinID>,
) -> Result<Vec<NativeTabGroup>> {
    let observed = observe_app_windows(windows, manager, pid, live)?;
    let mut groups: Vec<NativeTabGroup> = Vec::new();
    for (owner, window, _, spaces) in &observed {
        // Only an ordered-in physical window supplies a selected titlebar.
        // Explicit moves can assign a Space to still-ordered-out native tabs.
        let [workspace] = spaces.as_slice() else {
            continue;
        };
        if manager.window_is_unordered(window.id())? {
            continue;
        }
        let Some(snapshot) = window.native_tab_snapshot().map_err(|err| {
            Error::Generic(format!("window {} native titlebar: {err}", window.id()))
        })?
        else {
            continue;
        };
        if snapshot.titles.is_empty() || snapshot.selected >= snapshot.titles.len() {
            return Err(Error::Generic("invalid native tab selection".into()));
        }
        let mut members = Vec::with_capacity(snapshot.titles.len());
        let mut identity = Vec::with_capacity(snapshot.titles.len());
        for (index, title) in snapshot.titles.into_iter().enumerate() {
            if title.is_empty() || identity.iter().any(|(seen, _)| seen == &title) {
                return Err(Error::Generic(
                    "empty or duplicate native tab title is ambiguous".into(),
                ));
            }
            if groups
                .iter()
                .any(|group| group.identity.iter().any(|(seen, _)| seen == &title))
            {
                return Err(Error::Generic(
                    "overlapping native titlebar identities".into(),
                ));
            }
            let mut matches = observed
                .iter()
                .filter(|(_, _, candidate, _)| candidate == &title);
            let Some((entity, candidate, _, memberships)) = matches.next() else {
                // All live tracked titles were readable above. An empty label
                // still cannot be ruled out as this supposedly unknown sibling.
                if index == snapshot.selected
                    || observed.iter().any(|(_, _, title, _)| title.is_empty())
                {
                    return Err(Error::Generic(
                        "native tab title has no unambiguous tracked selection".into(),
                    ));
                }
                identity.push((title, None));
                continue;
            };
            if matches.next().is_some() {
                return Err(Error::Generic(
                    "native tab title does not uniquely identify a window".into(),
                ));
            }
            if index == snapshot.selected {
                if entity != owner {
                    return Err(Error::Generic(
                        "native tab selection disagrees with its AXWindow".into(),
                    ));
                }
            } else if memberships.len() > 1
                || !manager.window_is_unordered(candidate.id())?
                    && !known_native_sibling(
                        windows,
                        (*owner, window.id()),
                        (*entity, candidate.id()),
                        &title,
                    )
            {
                return Err(Error::Generic(
                    "native inactive tab is independently present".into(),
                ));
            }
            if groups
                .iter()
                .any(|group| group.members.iter().any(|(member, _)| member == entity))
            {
                return Err(Error::Generic("overlapping native titlebar groups".into()));
            }
            members.push((*entity, candidate.id()));
            identity.push((title, Some(candidate.id())));
        }
        groups.push(NativeTabGroup {
            members,
            selected: *owner,
            workspace: *workspace,
            identity,
        });
    }
    Ok(groups)
}

fn mark(app: Entity, now: Duration, geometry_only: bool, commands: &mut Commands) {
    // Merge at command application so notifications in the same frame compose,
    // and an application destroyed before then cannot be recreated by an echo.
    commands.queue(move |world: &mut World| {
        let Ok(mut entity) = world.get_entity_mut(app) else {
            return;
        };
        let mut dirty = entity.entry::<NativeTabsDirty>().or_default().into_mut();
        if geometry_only {
            dirty.geometry_after = Some(now + GEOMETRY_SETTLE);
        } else {
            dirty.until = now + NATIVE_REQUEST_TIMEOUT;
            dirty.next_check = Some(now);
        }
    });
}

pub(crate) fn dirty_native_tabs(
    mut messages: MessageReader<Event>,
    windows: Windows,
    apps: Query<(Entity, &Application)>,
    config: Res<Config>,
    time: Res<Time>,
    mut commands: Commands,
) {
    for event in messages.read() {
        if !enabled(&config) {
            continue;
        }
        let geometry_only = matches!(
            event,
            Event::WindowMoved { .. } | Event::WindowResized { .. }
        );
        let id = match event {
            Event::WindowFocused { window_id }
            | Event::WindowMoved { window_id }
            | Event::WindowResized { window_id }
            | Event::WindowTitleChanged { window_id }
            | Event::WindowDestroyed { window_id, .. }
            | Event::WindowDeminimized { window_id }
            | Event::WindowMinimized { window_id }
            | Event::MenuClosed { window_id } => Some(*window_id),
            Event::WindowSpawned { pid, .. }
            | Event::ApplicationVisible { pid }
            | Event::ApplicationActivated { pid } => {
                for (entity, app) in &apps {
                    if app.pid() == *pid {
                        mark(entity, time.elapsed(), false, &mut commands);
                    }
                }
                continue;
            }
            Event::ApplicationFrontSwitched { psn } => {
                for (entity, app) in &apps {
                    if app.psn() == *psn {
                        mark(entity, time.elapsed(), false, &mut commands);
                    }
                }
                continue;
            }
            Event::SpaceChanged | Event::MissionControlExit | Event::WindowCreated { .. } => None,
            _ => continue,
        };
        if let Some((_, _, app)) = id.and_then(|id| windows.find_parent(id)) {
            mark(app, time.elapsed(), geometry_only, &mut commands);
        } else {
            for (app, _) in &apps {
                mark(app, time.elapsed(), geometry_only, &mut commands);
            }
        }
    }
}

fn discover_native_tabs(
    added: Populated<&ChildOf, Added<Window>>,
    config: Res<Config>,
    time: Res<Time>,
    mut commands: Commands,
) {
    if enabled(&config) {
        for parent in added {
            mark(parent.parent(), time.elapsed(), false, &mut commands);
        }
    }
}

/// A tab selected while AX was settling may have missed the original focus
/// discovery attempt. Retry only actual exposed `AXWindows`, within the dirty
/// application's settling window; titlebar button IDs are never invented.
fn discover_exposed_native_tabs(
    apps: Populated<(&Application, &mut NativeTabsDirty)>,
    windows: Windows,
    config: Res<Config>,
    mission_control: Res<MissionControlActive>,
    time: Res<Time>,
    _platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    mut commands: Commands,
) {
    if !enabled(&config) || mission_control.blocks_mutations() {
        return;
    }
    for (app, mut dirty) in apps {
        if !dirty.is_due(time.elapsed()) {
            continue;
        }
        dirty.discovery_pending = false;
        let mut discovered = Vec::new();
        for window in app.window_list(&config) {
            if windows.find(window.id()).is_some() {
                continue;
            }
            match window.native_tab_snapshot() {
                Ok(Some(_)) => discovered.push(window),
                Ok(None) => {}
                Err(_) => dirty.discovery_pending = true,
            }
        }
        if !discovered.is_empty() {
            commands.trigger(SpawnWindowTrigger(discovered));
        }
    }
}

fn reconcile_after_move(
    removed: On<Remove, SpaceMovePending>,
    parents: Query<&ChildOf, With<Window>>,
    moves: Query<&SpaceMovePending>,
    config: Res<Config>,
    time: Res<Time>,
    mut commands: Commands,
) {
    if enabled(&config)
        && let Ok(parent) = parents.get(removed.event().entity)
        && let Ok(pending) = moves.get(removed.event().entity)
        && pending.has_native_tabs()
    {
        mark(parent.parent(), time.elapsed(), false, &mut commands);
    }
}

type TabRows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut LayoutStrip,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    ),
>;

type DirtyApps<'w, 's> = Populated<
    'w,
    's,
    (
        Entity,
        &'static Application,
        &'static mut NativeTabsDirty,
        Option<&'static NativeTabGroups>,
    ),
>;

fn destination(
    rows: &TabRows,
    members: &[(Entity, WinID)],
    workspace: WorkspaceId,
) -> Option<Entity> {
    rows.iter()
        .filter(|(_, strip, _, _)| strip.id() == workspace)
        .min_by_key(|(_, strip, active, selected)| {
            (
                !members.iter().any(|(member, _)| strip.contains(*member)),
                !*active,
                !*selected,
                strip.virtual_index,
            )
        })
        .map(|(entity, _, _, _)| entity)
}

#[derive(SystemParam)]
pub(crate) struct NativeTabContext<'w, 's> {
    windows: Windows<'w, 's>,
    manager: Res<'w, WindowManager>,
    rows: TabRows<'w, 's>,
    moves: Query<'w, 's, &'static SpaceMovePending>,
    commands: Commands<'w, 's>,
}

/// Retains unresolved old identities without granting them move authority.
fn reconcile_detached(
    previous: &NativeTabGroups,
    groups: &mut Vec<NativeTabGroup>,
    ctx: &mut NativeTabContext,
    live: &HashSet<WinID>,
) -> bool {
    let observed_count = groups.len();
    let mut unresolved = false;
    for old in &previous.0 {
        let mut pending = false;
        for &(entity, id) in &old.members {
            if groups[..observed_count]
                .iter()
                .any(|group| group.members.contains(&(entity, id)))
            {
                continue;
            }
            if ctx.moves.iter().any(|movement| movement.travels(entity)) {
                pending = true;
                continue;
            }
            if !live.contains(&id) {
                continue;
            }
            let Ok(spaces) = ctx.manager.window_workspaces(id) else {
                pending = true;
                continue;
            };
            let [workspace] = spaces.as_slice() else {
                pending = true;
                continue;
            };
            if ctx
                .windows
                .get_managed(entity)
                .is_none_or(|(_, _, flags)| !flags.is_tiled())
            {
                continue;
            }
            let Some(target) = destination(&ctx.rows, &[(entity, id)], *workspace) else {
                pending = true;
                continue;
            };
            let mut changed = false;
            for (row, mut strip, _, _) in &mut ctx.rows {
                if strip.contains(entity) && (row != target || strip.tabbed(entity)) {
                    strip.remove(entity);
                    changed = true;
                }
            }
            if let Ok((_, mut strip, _, _)) = ctx.rows.get_mut(target)
                && !strip.contains(entity)
            {
                strip.append_tab_group(&[entity]);
                changed = true;
            }
            if changed {
                ctx.commands.reshuffle_around(entity);
            }
        }
        if pending {
            groups.push(old.clone());
            unresolved = true;
        }
    }
    unresolved
}

/// Returns whether every tiled member has a destination and a shared slot.
fn place_group(group: &NativeTabGroup, ctx: &mut NativeTabContext) -> bool {
    if group
        .members
        .iter()
        .any(|(entity, _)| ctx.moves.iter().any(|movement| movement.travels(*entity)))
    {
        return false;
    }
    let tiled = group
        .members
        .iter()
        .copied()
        .filter(|(entity, _)| {
            ctx.windows
                .get_managed(*entity)
                .is_some_and(|(_, _, flags)| flags.is_tiled())
        })
        .collect::<Vec<_>>();
    if tiled.is_empty() {
        return true;
    }
    let Some(target) = destination(&ctx.rows, &tiled, group.workspace) else {
        return false;
    };
    let entities = tiled.iter().map(|(entity, _)| *entity).collect::<Vec<_>>();
    let mut changed = false;
    let mut complete = true;
    for (row, mut strip, _, _) in &mut ctx.rows {
        if row == target {
            continue;
        }
        for entity in &entities {
            if strip.contains(*entity) {
                strip.remove(*entity);
                changed = true;
            }
        }
    }
    if let Ok((_, mut strip, _, _)) = ctx.rows.get_mut(target) {
        let existing = strip.tab_group(entities[0]).unwrap_or_else(|| {
            if strip.contains(entities[0]) {
                vec![entities[0]]
            } else {
                vec![]
            }
        });
        if existing.len() != entities.len()
            || entities.iter().any(|entity| !existing.contains(entity))
        {
            let anchor = entities
                .iter()
                .copied()
                .find(|entity| strip.contains(*entity));
            if let Some(anchor) = anchor.filter(|anchor| {
                strip
                    .tab_group(*anchor)
                    .is_none_or(|old| old.iter().all(|entity| entities.contains(entity)))
            }) {
                // Preserve a validated group's existing stack item and column.
                for entity in &entities {
                    if *entity != anchor
                        && !strip
                            .tab_group(anchor)
                            .is_some_and(|tabs| tabs.contains(entity))
                    {
                        match strip.convert_to_tabs(anchor, *entity) {
                            Ok(()) => changed = true,
                            Err(_) => complete = false,
                        }
                    }
                }
            } else {
                strip.append_tab_group(&entities);
                changed = true;
            }
        }
    }
    if changed {
        ctx.commands.reshuffle_around(group.selected);
    }
    complete
}

/// Repairs row ownership when Cocoa transfers selection to another `NSWindow`.
pub(crate) fn reconcile_native_tabs(
    apps: DirtyApps,
    mut ctx: NativeTabContext,
    mission_control: Res<MissionControlActive>,
    config: Res<Config>,
    time: Res<Time>,
    _platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
) {
    let now = time.elapsed();
    for (app_entity, app, mut dirty, previous) in apps {
        if !enabled(&config) {
            ctx.commands
                .entity(app_entity)
                .try_remove::<NativeTabsDirty>();
            continue;
        }
        if !dirty.is_due(now) {
            continue;
        }
        if let Some(settled) = dirty.geometry_after.filter(|settled| now >= *settled) {
            dirty.geometry_after = None;
            dirty.until = dirty.until.max(settled + NATIVE_REQUEST_TIMEOUT);
        }
        dirty.next_check = (now < dirty.until).then_some(now + NATIVE_CHECK_INTERVAL);
        if dirty.is_complete() {
            ctx.commands
                .entity(app_entity)
                .try_remove::<NativeTabsDirty>();
        }
        if mission_control.blocks_mutations() {
            continue;
        }
        let observation = ctx.manager.window_census().and_then(|live| {
            observe_live_app_groups(&ctx.windows, &ctx.manager, app.pid(), &live)
                .map(|groups| (groups, live))
        });
        let (mut groups, live) = match observation {
            Ok(observation) => observation,
            Err(err) => {
                tracing::debug!(
                    pid = app.pid(),
                    "native tab observation is unsettled: {err}"
                );
                continue;
            }
        };
        let observed_count = groups.len();
        let mut unresolved = dirty.discovery_pending
            || groups
                .iter()
                .any(|group| group.identity.iter().any(|(_, id)| id.is_none()));
        if let Some(previous) = previous {
            unresolved |= reconcile_detached(previous, &mut groups, &mut ctx, &live);
        }
        for group in &groups[..observed_count] {
            unresolved |= !place_group(group, &mut ctx);
        }
        if previous.is_none_or(|previous| previous.0 != groups) {
            ctx.commands
                .entity(app_entity)
                .try_insert(NativeTabGroups(groups));
        }
        if !unresolved {
            dirty.next_check = None;
        }
        if dirty.is_complete() {
            ctx.commands
                .entity(app_entity)
                .try_remove::<NativeTabsDirty>();
        }
    }
}

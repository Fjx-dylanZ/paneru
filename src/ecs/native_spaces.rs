//! Native macOS Space lifecycle and explicit window moves between Spaces.
//!
//! Every request here is a submission, never a confirmation: the window
//! server creates and destroys Desktops and reassigns windows asynchronously,
//! so each accepted request leaves a small pending component behind that is
//! observed on [`Time`] every [`NATIVE_CHECK_INTERVAL`] until the native state
//! shows it applied or [`NATIVE_REQUEST_TIMEOUT`] passes. The ECS layout is
//! touched only once the observed native state says so; what could not be
//! observed keeps its last known layout rather than being guessed at, a
//! request that failed after submission is observed exactly like a clean one,
//! and nothing is ever resubmitted on the strength of a stale read.
//!
//! The OS notifications for the same changes go through the same observers.
//! A Desktop gets its row 0 only once the detailed census vouches for it —
//! lists it as an ordinary Desktop — and a present display the ECS knows
//! owns it: the display list alone never places a row, a census that could
//! not be read is asked again until the deadline rather than worked around,
//! and a `SpaceCreated` the census cannot vouch for yet waits here instead
//! of guessing a display. A `SpaceDestroyed` for a Desktop joins the bounded
//! survivor wait instead of pre-empting it; one for the fullscreen Space of
//! a window this side put in a fullscreen row restores that window to its
//! remembered slot at once, as the OS has already taken the Space away. A
//! row a destroyed Space keeps for windows whose new Space was never
//! observed is marked as such and goes as soon as whatever later places
//! those windows has emptied it.
//!
//! One native request is in flight at a time. Creation, destruction, a window
//! move and a native Space switch all reshape the census the others resolve
//! against, so a new command is refused while any of them is still being
//! confirmed, and repeats within one tick never reach the window server.

use std::pin::Pin;
use std::time::Duration;

use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::lifecycle::Remove;
use bevy::ecs::message::MessageReader;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Added, Changed, Has, Or, With};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, NonSend, Populated, Query, Res, ResMut, SystemParam};
use bevy::math::IRect;
use bevy::time::Time;
use objc2_core_graphics::CGDirectDisplayID;
use paneru_shared_types::state::NativeSpaceState;
use tracing::{Level, debug, instrument, warn};

use super::focus::FocusHistory;
use super::layout::{Column, LayoutStrip, StackItem, clamp_origin_to_viewport};
use super::native_tabs::{NativeTabGroup, observe_app_groups};
use super::params::Windows;
use super::workspace::{FollowSpacePending, workspace_created_handler};
use super::{
    ActiveWorkspaceMarker, DockPosition, FocusedMarker, FollowCurrentWorkspaceMarker,
    InstantSpaceSwitch, MissionControlActive, NativeFullscreenMarker, Position,
    PreviousManagedStrip, RepositionMarker, SelectedVirtualMarker, SendMessageTrigger,
    SpawnCommandsExt,
};
use crate::commands::{
    Command, MoveFocus, Operation, SpaceOperation, SpaceSelector, command_focus_native_space,
    resolve_native_space,
};
use crate::config::Config;
use crate::errors::{Error, Result};
use crate::events::Event;
use crate::manager::{Display, Origin, Window, WindowManager};
use crate::platform::{PlatformCallbacks, WinID, WorkspaceId};

/// Bound on how long one submitted native request may go unobserved before
/// it is given up on.
pub(crate) const NATIVE_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Cadence of the non-blocking confirmation queries while a request is in
/// flight.
pub(crate) const NATIVE_CHECK_INTERVAL: Duration = Duration::from_millis(50);
/// The native Space type of an ordinary Desktop, as the census reports it.
const DESKTOP_KIND: i64 = 0;

pub struct NativeSpacesPlugin;

impl Plugin for NativeSpacesPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(handoff_space_move);
        // Ordered after the `space focus` handler so a switch and a lifecycle
        // or move request arriving in the same frame serialize: the switch is
        // recorded on `InstantSpaceSwitch` at once and refuses the request
        // here, instead of both writes leaving because the request's pending
        // component was still a deferred command when the switch looked.
        app.add_systems(
            PreUpdate,
            command_native_space_request
                .after(command_focus_native_space)
                .after(super::native_tabs::reconcile_native_tabs),
        );
        app.add_systems(
            Update,
            (
                // Ordered after the OS notification handler so a row 0 a
                // `SpaceCreated` spawned in the same tick is visible here and
                // the Desktop is never given a second one; the placement
                // observer runs last so a wait the notification left for the
                // same Desktop sees the row confirmation gave this tick and
                // never spawns beside it.
                confirm_native_space_creation.after(workspace_created_handler),
                place_native_desktops.after(confirm_native_space_creation),
                confirm_native_space_destruction,
                reap_destroyed_space_rows,
                observe_space_moves.after(super::native_tabs::reconcile_native_tabs),
            ),
        );
    }
}

/// Every strip with the flags that pick a destination row (the one on screen,
/// or the one selected on its display) and the fullscreen restore target a
/// destroyed Space's reconciliation reads.
type NativeStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut LayoutStrip,
        Option<&'static NativeFullscreenMarker>,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    ),
>;

/// Every display with what its actual viewport depends on.
pub(super) type Displays<'w, 's> =
    Query<'w, 's, (Entity, &'static Display, Option<&'static DockPosition>)>;

/// The flags of a move's leader that decide whether it may travel now.
type LeaderFlags<'w, 's> = Query<
    'w,
    's,
    (
        Has<FollowCurrentWorkspaceMarker>,
        Has<FollowSpacePending>,
        Has<SpaceMovePending>,
    ),
    With<Window>,
>;

/// Every explicit move in flight, carried by its leader.
type MovingLeaders<'w, 's> =
    Populated<'w, 's, (Entity, &'static Window, &'static mut SpaceMovePending)>;

/// Current follow intent and frame of every tracked move member.
type MemberFlags<'w, 's> = Query<
    'w,
    's,
    (
        Has<FollowCurrentWorkspaceMarker>,
        Option<&'static RepositionMarker>,
    ),
    With<Window>,
>;

/// The rows of destroyed Spaces that were just marked as such or rewritten
/// since the last look, so [`reap_destroyed_space_rows`] runs only then.
type RewrittenDestroyedRows<'w, 's> = Populated<
    'w,
    's,
    (Entity, &'static LayoutStrip),
    (
        With<DestroyedSpaceMarker>,
        Or<(Added<DestroyedSpaceMarker>, Changed<LayoutStrip>)>,
    ),
>;

/// One submitted `move_windows_to_workspace` batch awaiting confirmation, or
/// a move somebody else submitted (a follower's own carry) that is only
/// observed here.
#[derive(Debug)]
pub(super) struct NativeMoveRequest {
    target: WorkspaceId,
    /// The windows still owed confirmation, each of which must report
    /// exactly `target`. A member that closes mid-flight leaves the list; the
    /// anchor window itself never does.
    windows: Vec<WinID>,
    requested_at: Duration,
    next_check: Duration,
}

impl NativeMoveRequest {
    /// The pre-submission membership read just showed the old state, so the
    /// first confirmation waits a full interval rather than re-asking at once.
    pub(super) fn new(target: WorkspaceId, windows: Vec<WinID>, now: Duration) -> Self {
        Self {
            target,
            windows,
            requested_at: now,
            next_check: now + NATIVE_CHECK_INTERVAL,
        }
    }

    pub(super) fn target(&self) -> WorkspaceId {
        self.target
    }

    /// Whether the next confirmation query is due.
    pub(super) fn due(&self, now: Duration) -> bool {
        now >= self.next_check
    }

    fn expired(&self, now: Duration) -> bool {
        past_deadline(self.requested_at, now)
    }

    /// Checks the batch. `Some(true)` once every live member reports the
    /// target, `Some(false)` when the deadline passed or the query failed,
    /// and `None` while it is still worth waiting on, with the next check
    /// scheduled. Old membership on an early check is never a reason to
    /// resend the write.
    pub(super) fn observe(
        &mut self,
        window_manager: &WindowManager,
        anchor: WinID,
        now: Duration,
    ) -> Option<bool> {
        let result = batch_landed(window_manager, &mut self.windows, anchor, self.target);
        self.observe_result(result, anchor, now)
    }

    fn observe_result(
        &mut self,
        result: Result<bool>,
        anchor: WinID,
        now: Duration,
    ) -> Option<bool> {
        let target = self.target;
        match result {
            Ok(true) => {
                debug!(
                    window_id = anchor,
                    workspace_id = target,
                    waited = ?now.saturating_sub(self.requested_at),
                    "window batch confirmed on requested native Space"
                );
                Some(true)
            }
            Ok(false) if !self.expired(now) => {
                self.next_check = now + NATIVE_CHECK_INTERVAL;
                None
            }
            Ok(false) => {
                warn!(
                    window_id = anchor,
                    workspace_id = target,
                    "window batch not observed on requested native Space before the deadline; giving up"
                );
                Some(false)
            }
            Err(err) => {
                warn!(
                    window_id = anchor,
                    workspace_id = target,
                    "unable to confirm window batch on requested native Space: {err}"
                );
                Some(false)
            }
        }
    }
}

/// Whether every window of `windows` now reports exactly `target`, read fresh
/// from the window server. Inclusion of the anchor alone is not enough: its
/// tab siblings and associated windows travel in the same batch and must
/// have landed too before the anchor is treated as moved. A member the window
/// server no longer knows at all — closed before or during the move — is
/// dropped from `windows` rather than waited on, so the anchor is not
/// stranded on the far side of a move that did land; the anchor itself never
/// is dropped, and a failed query is neither landing nor absence.
pub(super) fn batch_landed(
    window_manager: &WindowManager,
    windows: &mut Vec<WinID>,
    anchor: WinID,
    target: WorkspaceId,
) -> Result<bool> {
    let mut landed = true;
    let mut index = 0;
    while let Some(&window_id) = windows.get(index) {
        let workspaces = window_manager.window_workspaces(window_id);
        if !matches!(workspaces.as_deref(), Ok([workspace]) if *workspace == target) {
            if window_id != anchor && !window_manager.window_exists(window_id)? {
                debug!(
                    anchor,
                    window_id,
                    workspace_id = target,
                    "batch member closed; no longer part of the native move"
                );
                windows.remove(index);
                continue;
            }
            // A live window that has not landed keeps the batch waiting; one
            // whose Spaces could not be read at all fails the check outright.
            workspaces?;
            landed = false;
        }
        index += 1;
    }
    Ok(landed)
}

/// Submits one batch for `desired`. Returns the Space to observe: `desired`
/// on success, the reported one when the request may have partially applied,
/// and nothing when it was refused before anything happened.
pub(super) fn submit_native_move(
    window_manager: &WindowManager,
    batch: &[WinID],
    desired: WorkspaceId,
    anchor: WinID,
    inactive_tabs: &[WinID],
) -> Option<WorkspaceId> {
    match window_manager.move_windows_to_workspace(batch, desired, inactive_tabs) {
        Ok(()) => {
            debug!(
                window_id = anchor,
                workspace_id = desired,
                ?batch,
                "requested native window move"
            );
            Some(desired)
        }
        Err(Error::NativeSpaceRequest {
            workspace_id: reported,
            request_may_have_applied: true,
            message,
            ..
        }) => {
            warn!(
                window_id = anchor,
                workspace_id = reported,
                "native window move may have partially applied, observing it: {message}"
            );
            Some(reported)
        }
        Err(err) => {
            warn!(
                window_id = anchor,
                workspace_id = desired,
                "unable to move windows to native Space: {err}"
            );
            None
        }
    }
}

/// Whether a request made at `requested_at` has gone unobserved past the
/// deadline.
fn past_deadline(requested_at: Duration, now: Duration) -> bool {
    now >= requested_at + NATIVE_REQUEST_TIMEOUT
}

/// A submitted native Desktop creation awaiting its appearance in the census.
/// Lives on its own entity.
#[derive(Component, Debug)]
pub(crate) struct NativeSpaceCreatePending {
    /// The ID the bridge handed back. `None` when the request failed after
    /// submission without a usable new ID: the outcome is then only ever
    /// reported, never claimed.
    workspace_id: Option<WorkspaceId>,
    /// The Spaces known before submission, so an uncertain outcome can at
    /// least say which Desktops appeared since.
    baseline: Vec<WorkspaceId>,
    requested_at: Duration,
    next_check: Duration,
}

impl NativeSpaceCreatePending {
    fn new(workspace_id: Option<WorkspaceId>, baseline: Vec<WorkspaceId>, now: Duration) -> Self {
        Self {
            workspace_id,
            baseline,
            requested_at: now,
            next_check: now + NATIVE_CHECK_INTERVAL,
        }
    }
}

/// A native Space the OS announced whose row 0 cannot be placed yet: the
/// detailed census could not be read, does not list it as an ordinary
/// Desktop yet, or no present display the ECS knows owns it — every reader
/// can lag the `SpaceCreated` notification. The row is spawned under the
/// actual owner once the census vouches for the Desktop and a display lists
/// it, never under the active display by assumption; a Space the census has
/// not vouched for by the deadline gets no row at all. Lives on its own
/// entity.
#[derive(Component, Debug)]
pub(crate) struct NativeSpacePlacementPending {
    workspace_id: WorkspaceId,
    requested_at: Duration,
    next_check: Duration,
}

impl NativeSpacePlacementPending {
    /// The window server was just asked and could not vouch for the Desktop,
    /// so the first attempt waits a full interval rather than re-asking at
    /// once.
    fn new(workspace_id: WorkspaceId, now: Duration) -> Self {
        Self {
            workspace_id,
            requested_at: now,
            next_check: now + NATIVE_CHECK_INTERVAL,
        }
    }
}

/// What the windows sampled on a destroyed Desktop report once it is gone.
/// Only `unresolved` is ever re-asked: a membership observed once stands, so
/// a transient query failure later cannot unsay it.
#[derive(Debug, Default)]
struct Survivors {
    /// Members of another Space now.
    landed: Vec<WinID>,
    /// Gone from the window server; nothing here closed them.
    lost: Vec<WinID>,
    /// Still without a membership elsewhere, or unanswerable.
    unresolved: Vec<WinID>,
}

impl Survivors {
    fn sampled(sampled: Vec<WinID>) -> Self {
        Self {
            unresolved: sampled,
            ..Self::default()
        }
    }

    /// Re-asks the window server about every window still unresolved.
    fn observe(&mut self, window_manager: &WindowManager, destroyed: WorkspaceId) {
        for window_id in std::mem::take(&mut self.unresolved) {
            match window_manager.window_workspaces(window_id) {
                Ok(spaces) if !spaces.is_empty() && !spaces.contains(&destroyed) => {
                    self.landed.push(window_id);
                }
                Ok(spaces) if spaces.is_empty() => match window_manager.window_exists(window_id) {
                    Ok(false) => self.lost.push(window_id),
                    Ok(true) => self.unresolved.push(window_id),
                    Err(err) => {
                        debug!(
                            window_id,
                            "unable to tell whether a sampled window still exists: {err}"
                        );
                        self.unresolved.push(window_id);
                    }
                },
                Ok(_) => self.unresolved.push(window_id),
                Err(err) => {
                    debug!(
                        window_id,
                        "unable to read a sampled window's native Spaces: {err}"
                    );
                    self.unresolved.push(window_id);
                }
            }
        }
    }
}

/// How a Desktop's destruction came to be observed, for the record.
#[derive(Clone, Copy, Debug)]
enum DestroyOrigin {
    /// A `space destroy` command. `receipt` is whether the preflight's
    /// sampled window list survived the submission.
    Command { migrate: bool, receipt: bool },
    /// An OS `SpaceDestroyed` notification for a Space nothing here asked to
    /// destroy, or whose request had already been given up on.
    Notification,
}

/// A native Desktop destruction awaiting its disappearance from the census
/// and the migration of what it held: submitted here, or reported by the OS.
/// Lives on its own entity and is the only path that reconciles the Space's
/// rows, so the notification can never pre-empt the survivor wait.
#[derive(Component, Debug)]
pub(crate) struct NativeSpaceDestroyPending {
    workspace_id: WorkspaceId,
    origin: DestroyOrigin,
    /// The application windows the destruction preflight sampled on the
    /// Space, each owed a surviving membership elsewhere.
    survivors: Survivors,
    /// The tab groups of the Space's rows already observed on exactly one
    /// other Space, with that Space. Kept across checks so a later stale or
    /// failed read cannot turn an observed migration into a lost group.
    migrated: Vec<(Vec<Entity>, WorkspaceId)>,
    requested_at: Duration,
    next_check: Duration,
}

impl NativeSpaceDestroyPending {
    fn command(
        workspace_id: WorkspaceId,
        sampled: Option<Vec<WinID>>,
        migrate: bool,
        now: Duration,
    ) -> Self {
        Self {
            workspace_id,
            origin: DestroyOrigin::Command {
                migrate,
                receipt: sampled.is_some(),
            },
            survivors: Survivors::sampled(sampled.unwrap_or_default()),
            migrated: Vec::new(),
            requested_at: now,
            next_check: now + NATIVE_CHECK_INTERVAL,
        }
    }

    /// The OS just said the Space is gone, so the first observation is due at
    /// once.
    fn notification(workspace_id: WorkspaceId, now: Duration) -> Self {
        Self {
            workspace_id,
            origin: DestroyOrigin::Notification,
            survivors: Survivors::default(),
            migrated: Vec::new(),
            requested_at: now,
            next_check: now,
        }
    }
}

/// A landed batch whose tiled members have no row to join yet: the target
/// has no row, and no present display the ECS knows lists it. The layout is
/// left as it is and looked at again every [`NATIVE_CHECK_INTERVAL`] until a
/// row can be resolved or [`NATIVE_REQUEST_TIMEOUT`] passes; nothing is
/// floated or rowed by guess meanwhile.
#[derive(Debug)]
struct Placing {
    /// The members confirmed on the target.
    landed: Vec<Entity>,
    /// Whether the batch was confirmed whole.
    whole: bool,
    since: Duration,
    next_check: Duration,
    /// Whether the display reconciliation has been asked, once, to catch up
    /// on an owner the census lists but the ECS has no display for.
    reconciliation_requested: bool,
}

impl Placing {
    fn new(landed: Vec<Entity>, whole: bool, now: Duration) -> Self {
        Self {
            landed,
            whole,
            since: now,
            next_check: now,
            reconciliation_requested: false,
        }
    }
}

/// Where an explicit move stands. The layout is only rewritten when
/// `Landing` settles and a row for what landed tiled could be resolved.
#[derive(Debug)]
enum MovePhase {
    /// A batch is on its way; every live member owes exact membership of
    /// the target before anything in the ECS changes.
    Landing(NativeMoveRequest),
    /// The batch landed, but the row its tiled members join could not be
    /// resolved yet.
    Placing(Placing),
    /// The destination's activation is with [`InstantSpaceSwitch`]; the
    /// moved window is focused once that has been confirmed. `neighbour` is
    /// the source row's window next to where the leader was, the one the
    /// source keeps focused should the activation fail while the leader
    /// still holds the focus marker.
    Activating { neighbour: Option<Entity> },
}

/// An explicit `spacemove`/`spacesend` in flight for the window entity that
/// carries it — the leader of the native batch. Every tracked tab or associated
/// child belongs to this move until it settles; none may be carried by follow
/// machinery or chosen as the source's replacement focus. A closing leader
/// hands bounded reconciliation to a survivor, but never its activation or
/// focus intent. Current mode is read at landing, so in-flight toggles survive.
#[derive(Component, Debug)]
pub(crate) struct SpaceMovePending {
    source: WorkspaceId,
    target: WorkspaceId,
    focus: MoveFocus,
    /// All submitted native IDs that resolve to existing window entities,
    /// including independently managed associated children.
    members: Vec<(Entity, WinID)>,
    /// Logical placement groups, preserving actual tab order without turning
    /// independently tracked children into tabs of their parent.
    groups: Vec<Vec<Entity>>,
    /// Native groups are revalidated after the initial all-member assignment,
    /// so later selection changes do not split their logical placement.
    native_groups: Vec<NativeTabGroup>,
    phase: MovePhase,
}

impl SpaceMovePending {
    pub(crate) fn travels(&self, entity: Entity) -> bool {
        self.members.iter().any(|(member, _)| *member == entity)
    }

    pub(crate) fn has_native_tabs(&self) -> bool {
        !self.native_groups.is_empty()
    }

    /// Transfers buffers without cloning them or restarting either deadline.
    /// Called only while this component's window is being removed.
    fn take_for_survivor(&mut self) -> Self {
        let phase = match &mut self.phase {
            MovePhase::Landing(request) => MovePhase::Landing(NativeMoveRequest {
                target: request.target,
                windows: std::mem::take(&mut request.windows),
                requested_at: request.requested_at,
                next_check: request.next_check,
            }),
            MovePhase::Placing(placing) => MovePhase::Placing(Placing {
                landed: std::mem::take(&mut placing.landed),
                whole: placing.whole,
                since: placing.since,
                next_check: placing.next_check,
                reconciliation_requested: placing.reconciliation_requested,
            }),
            MovePhase::Activating { neighbour } => MovePhase::Activating {
                neighbour: *neighbour,
            },
        };
        Self {
            source: self.source,
            target: self.target,
            focus: MoveFocus::Stay,
            members: std::mem::take(&mut self.members),
            groups: std::mem::take(&mut self.groups),
            native_groups: std::mem::take(&mut self.native_groups),
            phase,
        }
    }
}

/// Keeps observing surviving tracked members when the original leader closes.
/// Native effects already submitted remain owned; nothing is resubmitted and
/// a surviving child is never promoted into the command's focus intent.
fn handoff_space_move(
    trigger: On<Remove, Window>,
    mut moves: Query<&mut SpaceMovePending>,
    windows: Windows,
    window_manager: Res<WindowManager>,
    mut commands: Commands,
) {
    let closing = trigger.event().entity;
    let Ok(mut pending) = moves.get_mut(closing) else {
        return;
    };
    let survivor = pending.members.iter().find_map(|(member, id)| {
        (*member != closing
            && windows
                .get(*member)
                .is_some_and(|window| window.id() == *id)
            && !matches!(window_manager.window_exists(*id), Ok(false)))
        .then_some(*member)
    });
    if let Some(survivor) = survivor
        && let Ok(mut entity_commands) = commands.get_entity(survivor)
    {
        debug!("window {closing} closed; move reconciliation handed to {survivor}");
        entity_commands.try_insert(pending.take_for_survivor());
    }
}

/// A row of a native Space the census no longer lists, kept only because the
/// new Space of what it held was not observed. Whatever later takes those
/// windows out — the Space they turned up on going on screen, or their
/// closing — leaves nothing to keep the row, and it goes then.
#[derive(Component, Debug)]
pub(crate) struct DestroyedSpaceMarker;

/// The native requests still being confirmed, any of which refuses a new
/// lifecycle or move command until it has settled.
#[derive(SystemParam)]
pub(crate) struct NativeInFlight<'w, 's> {
    creating: Query<'w, 's, (), With<NativeSpaceCreatePending>>,
    placing: Query<'w, 's, (), With<NativeSpacePlacementPending>>,
    destroying: Query<'w, 's, (), With<NativeSpaceDestroyPending>>,
    moving: Query<'w, 's, (), With<SpaceMovePending>>,
}

impl NativeInFlight<'_, '_> {
    /// Why a new native request must wait, if it must.
    pub(crate) fn blocker(&self, switching: &InstantSpaceSwitch) -> Option<&'static str> {
        if !self.creating.is_empty() {
            Some("a native Desktop creation is still being confirmed")
        } else if !self.placing.is_empty() {
            Some("a native Desktop's owning display is still being confirmed")
        } else if !self.destroying.is_empty() {
            Some("a native Desktop destruction is still being confirmed")
        } else if !self.moving.is_empty() {
            Some("a window move between native Spaces is still being confirmed")
        } else if switching.is_pending() {
            Some("a native Space switch is still being confirmed")
        } else {
            None
        }
    }
}

/// The one native request a tick may submit.
#[derive(Clone, Copy, Debug)]
enum NativeRequest {
    Create,
    Destroy {
        selector: SpaceSelector,
        migrate: bool,
    },
    Move {
        selector: SpaceSelector,
        focus: MoveFocus,
    },
}

fn native_request(event: &Event) -> Option<NativeRequest> {
    let Event::Command { command } = event else {
        return None;
    };
    match command {
        Command::Space(SpaceOperation::Create) => Some(NativeRequest::Create),
        Command::Space(SpaceOperation::Destroy { selector, migrate }) => {
            Some(NativeRequest::Destroy {
                selector: *selector,
                migrate: *migrate,
            })
        }
        Command::Window(Operation::SpaceMove(selector, focus)) => Some(NativeRequest::Move {
            selector: *selector,
            focus: *focus,
        }),
        _ => None,
    }
}

/// Submits the one native lifecycle or move request of this tick. Each is a
/// submission recorded on a pending component and confirmed later; repeats in
/// the same tick, and any request while another is still being confirmed,
/// are refused here so the deferred component can never let two writes out.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
#[instrument(level = Level::DEBUG, skip_all)]
fn command_native_space_request(
    mut messages: MessageReader<Event>,
    windows: Windows,
    strips: Query<&LayoutStrip>,
    leaders: LeaderFlags,
    window_manager: Res<WindowManager>,
    mission_control: Res<MissionControlActive>,
    time: Res<Time>,
    in_flight: NativeInFlight,
    instant_space_switch: Res<InstantSpaceSwitch>,
    _platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    mut commands: Commands,
) {
    let mut requests = messages.read().filter_map(native_request);
    let Some(request) = requests.next() else {
        return;
    };
    let repeats = requests.count();
    if repeats > 0 {
        warn!(
            ?request,
            repeats, "only one native Space request is submitted per tick; ignoring the repeats"
        );
    }
    if mission_control.blocks_mutations() {
        warn!(
            ?request,
            "native Space requests are unavailable during Mission Control"
        );
        return;
    }
    if let Some(reason) = in_flight.blocker(&instant_space_switch) {
        warn!(?request, "native Space request refused: {reason}");
        return;
    }

    let now = time.elapsed();
    match request {
        NativeRequest::Create => submit_create(&window_manager, now, &mut commands),
        NativeRequest::Destroy { selector, migrate } => {
            submit_destroy(&window_manager, selector, migrate, now, &mut commands);
        }
        NativeRequest::Move { selector, focus } => submit_move(
            &windows,
            &strips,
            &leaders,
            &window_manager,
            selector,
            focus,
            now,
            &mut commands,
        ),
    }
}

fn submit_create(window_manager: &WindowManager, now: Duration, commands: &mut Commands) {
    let baseline = match window_manager.native_spaces() {
        Ok(spaces) => spaces,
        Err(err) => {
            warn!("unable to read the native Space census before creating a Desktop: {err}");
            return;
        }
    };
    match window_manager.create_native_space() {
        Ok(workspace_id) => {
            debug!(workspace_id, "requested native Desktop creation");
            commands.spawn(NativeSpaceCreatePending::new(
                Some(workspace_id),
                baseline,
                now,
            ));
        }
        Err(Error::NativeSpaceRequest {
            workspace_id,
            request_may_have_applied: true,
            message,
        }) => {
            // Only a nonzero ID unknown before the call names the new Desktop;
            // anything else is observed without ever being called created.
            let known =
                (workspace_id != 0 && !baseline.contains(&workspace_id)).then_some(workspace_id);
            warn!(
                workspace_id,
                "native Desktop creation may have partially applied, observing the census: {message}"
            );
            commands.spawn(NativeSpaceCreatePending::new(known, baseline, now));
        }
        Err(err) => warn!("unable to create a native Desktop: {err}"),
    }
}

fn submit_destroy(
    window_manager: &WindowManager,
    selector: SpaceSelector,
    migrate: bool,
    now: Duration,
    commands: &mut Commands,
) {
    // Resolved once, in the current global order, relative to the active
    // display's current Space, exactly as `space focus` counts.
    let target = window_manager.native_spaces().and_then(|spaces| {
        let display_id = window_manager.active_display_id()?;
        let current = window_manager.active_display_space(display_id)?;
        resolve_native_space(&spaces, current, selector).ok_or_else(|| {
            Error::InvalidInput(format!(
                "native Space selector {selector:?} is out of range"
            ))
        })
    });
    let workspace_id = match target {
        Ok(workspace_id) => workspace_id,
        Err(err) => {
            warn!("could not resolve the native Space to destroy: {err}");
            return;
        }
    };

    match window_manager.destroy_native_space(workspace_id, migrate) {
        Ok(sampled) => {
            debug!(
                workspace_id,
                migrate,
                ?sampled,
                "requested native Desktop destruction"
            );
            commands.spawn(NativeSpaceDestroyPending::command(
                workspace_id,
                Some(sampled),
                migrate,
                now,
            ));
        }
        Err(Error::NativeSpaceRequest {
            request_may_have_applied: true,
            message,
            ..
        }) => {
            warn!(
                workspace_id,
                migrate,
                "native Desktop destruction may have partially applied, observing the census: {message}"
            );
            commands.spawn(NativeSpaceDestroyPending::command(
                workspace_id,
                None,
                migrate,
                now,
            ));
        }
        Err(err) => warn!(workspace_id, "unable to destroy native Desktop: {err}"),
    }
}

/// Where an activation request stands right after submission.
enum Activation {
    /// Submitted, or already current elsewhere and owed the display focus
    /// policy; [`InstantSpaceSwitch`] observes it.
    Pending,
    /// Already the current Space of the active display; nothing was sent.
    Current,
    /// Refused before anything happened.
    Refused,
}

/// Submits the destination's activation the way `space focus` does, so
/// `confirm_native_space_focus` confirms it and finishes the display focus.
fn submit_activation(
    window_manager: &WindowManager,
    target: WorkspaceId,
    instant_space_switch: &mut InstantSpaceSwitch,
    now: Duration,
) -> Activation {
    match window_manager.focus_native_space(target) {
        Ok(true) => {
            instant_space_switch.begin_command(target, now);
            debug!(
                workspace_id = target,
                "requested native Space focus after move"
            );
            Activation::Pending
        }
        Ok(false) => Activation::Current,
        Err(Error::NativeSpaceRequest {
            workspace_id: reported,
            request_may_have_applied: true,
            message,
            ..
        }) => {
            instant_space_switch.begin_command(reported, now);
            warn!(
                workspace_id = reported,
                "native Space focus after move may have partially applied, observing it: {message}"
            );
            Activation::Pending
        }
        Err(err) => {
            warn!(
                workspace_id = target,
                "unable to focus native Space after move: {err}"
            );
            Activation::Refused
        }
    }
}

/// Submits one explicit move for the focused window. A continuous follower
/// travels exactly like any other leader — batch first, activation only once
/// the batch is confirmed — and is this move's alone while it is in flight:
/// the follow machinery neither queues nor carries a window that has a
/// [`SpaceMovePending`], and one whose follow carry is still being confirmed
/// is refused here so no two batches are ever out for it.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn submit_move(
    windows: &Windows,
    strips: &Query<&LayoutStrip>,
    leaders: &LeaderFlags,
    window_manager: &WindowManager,
    selector: SpaceSelector,
    focus: MoveFocus,
    now: Duration,
    commands: &mut Commands,
) {
    let Some((window, entity, flags)) = windows
        .focused()
        .and_then(|(_, entity)| windows.get_managed(entity))
    else {
        debug!("no focused window to move between native Spaces");
        return;
    };
    let window_id = window.id();
    if window.is_full_screen() {
        warn!(
            window_id,
            "a native-fullscreen window cannot be moved between Spaces"
        );
        return;
    }
    if flags.is_suspended() {
        warn!(
            window_id,
            "a minimized or hidden window is not moved between Spaces"
        );
        return;
    }

    let observed_groups = match window
        .pid()
        .and_then(|pid| observe_app_groups(windows, window_manager, pid))
    {
        Ok(groups) => groups,
        Err(err) => {
            warn!(
                window_id,
                "native tab identity could not be established before move: {err}"
            );
            return;
        }
    };
    let native_group = observed_groups
        .iter()
        .find(|group| group.members.iter().any(|(member, _)| *member == entity));
    let representative = native_group.map_or(entity, |group| group.selected);
    let Some(representative_window) = windows.get(representative) else {
        return;
    };
    let representative_id = representative_window.id();

    // The actual source, read fresh: relative selectors count from where the
    // window really is, not from the Space on screen.
    let source = match window_manager.window_workspaces(representative_id) {
        Ok(spaces) => match spaces.as_slice() {
            [source] => *source,
            [] => {
                warn!(
                    window_id,
                    "window has no native Space membership; not moving it"
                );
                return;
            }
            spaces => {
                warn!(
                    window_id,
                    count = spaces.len(),
                    "window is on several native Spaces; not moving it"
                );
                return;
            }
        },
        Err(err) => {
            warn!(window_id, "unable to read the window's native Space: {err}");
            return;
        }
    };
    let target = match window_manager.native_spaces().and_then(|spaces| {
        resolve_native_space(&spaces, source, selector).ok_or_else(|| {
            Error::InvalidInput(format!(
                "native Space selector {selector:?} is out of range from Space {source}"
            ))
        })
    }) {
        Ok(target) => target,
        Err(err) => {
            warn!(window_id, "could not resolve native Space: {err}");
            return;
        }
    };
    if target == source {
        debug!(
            window_id,
            workspace_id = source,
            "window is already on the selected native Space"
        );
        return;
    }

    // Every native tab needs an explicit assignment, including ordered-out
    // windows with no membership. Moving the selected tab alone is insufficient.
    let group = native_group.map_or_else(
        || {
            strips
                .iter()
                .find_map(|strip| strip.tab_group(entity))
                .unwrap_or_else(|| vec![entity])
                .into_iter()
                .filter_map(|member| windows.get(member).map(|window| (member, window.id())))
                .collect::<Vec<_>>()
        },
        |group| group.members.clone(),
    );

    let mut batch = Vec::with_capacity(group.len());
    for (_, member_id) in &group {
        batch.push(*member_id);
        batch.extend(window_manager.get_associated_windows(*member_id));
    }
    let native_groups = observed_groups
        .into_iter()
        .filter(|group| group.members.iter().any(|(_, id)| batch.contains(id)))
        .collect::<Vec<_>>();
    if native_groups
        .iter()
        .any(|group| group.identity.iter().any(|(_, id)| id.is_none()))
    {
        warn!(
            window_id,
            "native tab group contains unresolved windows; select each tab before moving it"
        );
        return;
    }
    let inactive_tabs = native_groups
        .iter()
        .flat_map(|group| {
            group
                .members
                .iter()
                .filter_map(|(member, id)| (*member != group.selected).then_some(*id))
        })
        .collect::<Vec<_>>();
    let mut logical_members = Vec::new();
    for group in &native_groups {
        logical_members.extend_from_slice(&group.members);
        // Associations of every logical member are independent unless they are
        // themselves positively identified as Cocoa tabs.
        for (_, id) in &group.members {
            batch.push(*id);
            batch.extend(window_manager.get_associated_windows(*id));
        }
    }
    batch.sort_unstable();
    batch.dedup();
    // Associated windows that have since closed leave the batch; any live
    // member the window server cannot answer for fails the request closed
    // before anything is submitted.
    if let Err(err) = batch_landed(window_manager, &mut batch, representative_id, target) {
        warn!(
            window_id,
            workspace_id = target,
            "unable to read the native Spaces of the windows to move: {err}"
        );
        return;
    }

    let mut members = batch
        .iter()
        .filter_map(|id| windows.find(*id).map(|(_, entity)| (entity, *id)))
        .collect::<Vec<_>>();
    for member in logical_members {
        if !members.contains(&member) {
            members.push(member);
        }
    }
    for (member, member_id) in &members {
        let (following, carrying, moving) = leaders.get(*member).unwrap_or((false, false, false));
        if carrying || moving {
            warn!(
                window_id = member_id,
                "a batch member's native move is still being confirmed"
            );
            return;
        }
        if following && matches!(focus, MoveFocus::Stay) {
            warn!(
                window_id = member_id,
                "a batch member follows the current Space; turn follow off before sending it"
            );
            return;
        }
    }

    let mut groups = native_groups
        .iter()
        .map(|group| {
            group
                .members
                .iter()
                .map(|(entity, _)| *entity)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for (member, _) in &members {
        if groups.iter().any(|group| group.contains(member)) {
            continue;
        }
        let mut group = strips
            .iter()
            .find_map(|strip| strip.tab_group(*member))
            .unwrap_or_else(|| vec![*member]);
        group.retain(|entity| members.iter().any(|(member, _)| member == entity));
        groups.push(group);
    }
    let Some(observed) = submit_native_move(
        window_manager,
        &batch,
        target,
        representative_id,
        &inactive_tabs,
    ) else {
        return;
    };

    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_insert(SpaceMovePending {
            source,
            target,
            focus,
            members,
            groups,
            native_groups,
            phase: MovePhase::Landing(NativeMoveRequest::new(observed, batch, now)),
        });
    }
}

/// The display the window server says owns `workspace_id` right now, whether
/// or not the ECS has it.
fn census_owner(
    workspace_id: WorkspaceId,
    window_manager: &WindowManager,
) -> Option<CGDirectDisplayID> {
    window_manager
        .present_displays()
        .into_iter()
        .find_map(|(display, spaces)| spaces.contains(&workspace_id).then_some(display.id()))
}

/// The ECS display the window server says owns `workspace_id` right now.
fn owning_display<'a>(
    workspace_id: WorkspaceId,
    window_manager: &WindowManager,
    displays: &'a Displays,
) -> Option<(Entity, &'a Display, Option<&'a DockPosition>)> {
    let owner = census_owner(workspace_id, window_manager)?;
    displays
        .iter()
        .find(|(_, display, _)| display.id() == owner)
}

/// Every row with the display it hangs under.
pub(super) type ParentedStrips<'w, 's> =
    Query<'w, 's, (Entity, &'static LayoutStrip, &'static ChildOf)>;

/// One detailed census per system run, read the first time a Space is asked
/// about — so a run with nothing to place asks the window server nothing —
/// and shared by every Space asked about in that run. It is the only
/// evidence that a Space exists as an ordinary Desktop: the display list is
/// a weaker reader that can name a Space the census has not validated, and
/// a census that could not be read says nothing either way. A read that
/// failed stays failed for the run; the next check asks again.
pub(super) struct CensusRead<'a> {
    window_manager: &'a WindowManager,
    census: Census,
}

enum Census {
    Unread,
    Unreadable,
    Read(Vec<NativeSpaceState>),
}

impl<'a> CensusRead<'a> {
    pub(super) fn new(window_manager: &'a WindowManager) -> Self {
        Self {
            window_manager,
            census: Census::Unread,
        }
    }

    /// The census, or nothing when it could not be read.
    fn spaces(&mut self) -> Option<&[NativeSpaceState]> {
        if matches!(self.census, Census::Unread) {
            self.census = match self.window_manager.native_space_info() {
                Ok(spaces) => Census::Read(spaces),
                Err(err) => {
                    warn!("unable to read the native Space census: {err}");
                    Census::Unreadable
                }
            };
        }
        match &self.census {
            Census::Read(spaces) => Some(spaces),
            Census::Unread | Census::Unreadable => None,
        }
    }
}

/// Where a Desktop's row 0 stands after one look at the window server.
#[derive(Clone, Copy, Debug)]
pub(super) enum RowPlacement {
    /// The Desktop has its row under the display that owns it.
    Placed,
    /// The census could not be read; nothing is known either way.
    Unread,
    /// The census does not list the Space.
    Unlisted,
    /// The census lists the Desktop, but no present display the ECS knows
    /// owns it yet.
    Unowned,
    /// The census lists the Space with another native type. It is not an
    /// ordinary Desktop and gets no row here, now or later; a fullscreen
    /// Space is rowed by the fullscreen transition alone.
    NotADesktop(i64),
}

/// Gives `workspace_id` its row 0 once the detailed census lists it as an
/// ordinary Desktop and a present display the ECS knows owns it, and says
/// where it stands otherwise. The census alone is evidence that the Desktop
/// exists; the display list decides which display, never whether.
pub(super) fn place_desktop_row(
    workspace_id: WorkspaceId,
    census: &mut CensusRead<'_>,
    strips: &ParentedStrips,
    displays: &Displays,
    commands: &mut Commands,
) -> RowPlacement {
    let window_manager = census.window_manager;
    let Some(spaces) = census.spaces() else {
        return RowPlacement::Unread;
    };
    match spaces.iter().find(|space| space.id == workspace_id) {
        None => RowPlacement::Unlisted,
        Some(space) if space.kind != DESKTOP_KIND => RowPlacement::NotADesktop(space.kind),
        Some(_) => {
            if ensure_desktop_strip(workspace_id, window_manager, strips, displays, commands) {
                RowPlacement::Placed
            } else {
                RowPlacement::Unowned
            }
        }
    }
}

/// Gives `workspace_id` its row 0 under the display that actually owns it.
/// Rows that already exist are kept; one that sat under another display is
/// re-parented and its origin reset to the owner's, as a fresh row would get.
/// Returns `false` while no present display the ECS knows lists the Space.
/// Called only with the census vouching for the Desktop: the display list
/// picks the display, it never stands in for the census.
fn ensure_desktop_strip(
    workspace_id: WorkspaceId,
    window_manager: &WindowManager,
    strips: &ParentedStrips,
    displays: &Displays,
    commands: &mut Commands,
) -> bool {
    let Some((display_entity, display, _)) = owning_display(workspace_id, window_manager, displays)
    else {
        return false;
    };
    let origin = display.bounds().min;
    let mut found = false;
    for (entity, strip, child_of) in strips {
        if strip.id() != workspace_id {
            continue;
        }
        found = true;
        if child_of.parent() != display_entity
            && let Ok(mut entity_commands) = commands.get_entity(entity)
        {
            debug!(
                workspace_id,
                virtual_index = strip.virtual_index,
                "re-parenting the Desktop's row to its owning display {display_entity}"
            );
            entity_commands
                .try_remove::<ChildOf>()
                .try_insert((ChildOf(display_entity), Position(origin)));
        }
    }
    if !found {
        debug!(
            workspace_id,
            "new Desktop row 0 on display {display_entity}"
        );
        commands.spawn_layout_strip(
            LayoutStrip::new(workspace_id, 0),
            origin,
            display_entity,
            false,
        );
        // The Space may have gone on screen while its row was still owed, with
        // nothing for that notification to mark active; the Space refresh
        // picks the row up now and changes nothing when the marker is right.
        commands.trigger(SendMessageTrigger(Event::SpaceChanged));
    }
    true
}

/// Whether a placement for `workspace_id` is already waiting on the census.
fn placement_pending(
    workspace_id: WorkspaceId,
    placements: &Query<&NativeSpacePlacementPending>,
) -> bool {
    placements
        .iter()
        .any(|pending| pending.workspace_id == workspace_id)
}

/// Leaves `workspace_id`'s row 0 to the bounded wait for the census to vouch
/// for it under a present display, unless one is already waiting. The caller
/// has established that the Space has no row and could not be placed now.
pub(super) fn defer_desktop_placement(
    workspace_id: WorkspaceId,
    placements: &Query<&NativeSpacePlacementPending>,
    now: Duration,
    commands: &mut Commands,
) {
    if placement_pending(workspace_id, placements) {
        return;
    }
    debug!(
        workspace_id,
        "the census does not vouch for the native Desktop yet; its row waits for it"
    );
    commands.spawn(NativeSpacePlacementPending::new(workspace_id, now));
}

/// Places the rows of Spaces the OS announced before the census could vouch
/// for them, once it lists them as ordinary Desktops under a present display
/// the ECS knows. A Space the census has not vouched for by the deadline is
/// left without a row rather than given one by guess — the display refresh
/// path spawns it if the owner turns up later — and one it lists with
/// another native type is not a Desktop and gets none at all. A second
/// placement for the same Space is dropped so one tick spawns one row.
#[allow(clippy::needless_pass_by_value)]
fn place_native_desktops(
    pending: Populated<(Entity, &mut NativeSpacePlacementPending)>,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    strips: ParentedStrips,
    displays: Displays,
    _platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    mut commands: Commands,
) {
    let now = time.elapsed();
    let mut census = CensusRead::new(&window_manager);
    let mut handled = Vec::new();
    for (entity, mut pending) in pending {
        let workspace_id = pending.workspace_id;
        let settled = if handled.contains(&workspace_id) {
            true
        } else {
            handled.push(workspace_id);
            if now < pending.next_check {
                continue;
            }
            let waited = now.saturating_sub(pending.requested_at);
            match place_desktop_row(workspace_id, &mut census, &strips, &displays, &mut commands) {
                RowPlacement::Placed => {
                    debug!(workspace_id, ?waited, "native Desktop's row placed");
                    true
                }
                RowPlacement::NotADesktop(kind) => {
                    debug!(
                        workspace_id,
                        kind, "native Space is not an ordinary Desktop; it gets no row here"
                    );
                    true
                }
                placement if past_deadline(pending.requested_at, now) => {
                    warn!(
                        workspace_id,
                        ?placement,
                        ?waited,
                        "the census did not vouch for the native Desktop before the deadline; leaving it without a row until a display refresh lists it"
                    );
                    true
                }
                _ => false,
            }
        };
        if settled {
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
        } else {
            pending.next_check = now + NATIVE_CHECK_INTERVAL;
        }
    }
}

/// Observes a submitted Desktop creation: once the census lists the known ID
/// as an ordinary Desktop and a present display the ECS knows owns it, its
/// row is placed under that display — never under the active one by
/// assumption. Until then the census is asked again every check, a read
/// that failed included: nothing else is evidence, and the wait is bounded
/// by the deadline, at which the Desktop is left without a row. An
/// uncertain outcome is only reported: the Desktops that appeared since are
/// logged and left to the notification path, never adopted as the one
/// requested.
#[allow(clippy::needless_pass_by_value)]
fn confirm_native_space_creation(
    pending: Populated<(Entity, &mut NativeSpaceCreatePending)>,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    strips: ParentedStrips,
    displays: Displays,
    _platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    mut commands: Commands,
) {
    let now = time.elapsed();
    let mut census = CensusRead::new(&window_manager);
    for (entity, mut pending) in pending {
        if now < pending.next_check {
            continue;
        }
        let waited = now.saturating_sub(pending.requested_at);
        let expired = past_deadline(pending.requested_at, now);

        let settled = match pending.workspace_id {
            Some(workspace_id) => {
                match place_desktop_row(
                    workspace_id,
                    &mut census,
                    &strips,
                    &displays,
                    &mut commands,
                ) {
                    RowPlacement::Placed => {
                        debug!(workspace_id, ?waited, "native Desktop creation confirmed");
                        true
                    }
                    RowPlacement::NotADesktop(kind) => {
                        warn!(
                            workspace_id,
                            kind,
                            "created native Space is not an ordinary Desktop; not adopting it"
                        );
                        true
                    }
                    placement if expired => {
                        warn!(
                            workspace_id,
                            ?placement,
                            ?waited,
                            "native Desktop creation not confirmed by the census before the deadline; giving up"
                        );
                        true
                    }
                    _ => false,
                }
            }
            None => match census.spaces() {
                Some(spaces) => {
                    let appeared = spaces
                        .iter()
                        .filter(|space| {
                            space.kind == DESKTOP_KIND && !pending.baseline.contains(&space.id)
                        })
                        .map(|space| space.id)
                        .collect::<Vec<_>>();
                    if appeared.is_empty() && !expired {
                        false
                    } else {
                        warn!(
                            ?appeared,
                            ?waited,
                            "native Desktop creation outcome uncertain; Desktops that appeared since are reconciled from the OS notification, not claimed"
                        );
                        true
                    }
                }
                None if expired => {
                    warn!(
                        ?waited,
                        "native Desktop creation outcome uncertain and the census could not be read before the deadline; giving up"
                    );
                    true
                }
                None => false,
            },
        };

        if settled {
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
        } else {
            pending.next_check = now + NATIVE_CHECK_INTERVAL;
        }
    }
}

/// Applies an OS `SpaceDestroyed` notification: a Space whose destruction
/// nothing here is already observing, and that still has rows, gets the same
/// bounded observation a `space destroy` command does — or, for the
/// fullscreen Space of a window in a fullscreen row, the immediate restore
/// [`confirm_native_space_destruction`] makes of it. The pending owns the
/// reconciliation, so the notification never pre-empts a survivor wait with
/// a one-shot membership read, and a repeat for a Space already reconciled
/// changes nothing.
pub(super) fn observe_destroyed_space(
    workspace_id: WorkspaceId,
    destructions: &Query<&NativeSpaceDestroyPending>,
    strips: &Query<&LayoutStrip>,
    now: Duration,
    commands: &mut Commands,
) {
    if destructions
        .iter()
        .any(|destruction| destruction.workspace_id == workspace_id)
    {
        debug!(
            workspace_id,
            "native Space destruction is already being observed"
        );
        return;
    }
    if !strips.iter().any(|strip| strip.id() == workspace_id) {
        debug!(workspace_id, "destroyed native Space has no rows left");
        return;
    }
    debug!(
        workspace_id,
        "native Space destroyed; observing where its rows' windows went"
    );
    commands.spawn(NativeSpaceDestroyPending::notification(workspace_id, now));
}

/// Reads where the tab groups of the Space's rows not yet observed elsewhere
/// have gone, remembering every group seen on exactly one other Space.
/// Returns how many groups still report the destroyed Space, no single
/// Space, or could not be asked.
fn observe_groups(
    pending: &mut NativeSpaceDestroyPending,
    strips: &NativeStrips,
    windows: &Windows,
    window_manager: &WindowManager,
) -> usize {
    let workspace_id = pending.workspace_id;
    let mut unresolved = 0;
    for (_, strip, fullscreen, _, _) in strips {
        if strip.id() != workspace_id || fullscreen.is_some() {
            continue;
        }
        for group in strip_groups(strip) {
            if pending
                .migrated
                .iter()
                .any(|(members, _)| members.iter().any(|member| group.contains(member)))
            {
                continue;
            }
            // Tab siblings share a Space; the first live member answers for
            // the group. A group with no live member has nothing to place.
            let Some(window) = group.iter().find_map(|member| windows.get(*member)) else {
                continue;
            };
            match window_manager.window_workspaces(window.id()).as_deref() {
                Ok([destination]) if *destination != workspace_id => {
                    debug!(
                        workspace_id,
                        destination,
                        ?group,
                        "windows of the destroyed native Space observed on another Space"
                    );
                    pending.migrated.push((group, *destination));
                }
                Ok(spaces) => {
                    debug!(
                        workspace_id,
                        ?group,
                        ?spaces,
                        "windows of the destroyed native Space have no single new Space yet"
                    );
                    unresolved += 1;
                }
                Err(err) => {
                    debug!(
                        workspace_id,
                        ?group,
                        "unable to read where the destroyed native Space's windows went: {err}"
                    );
                    unresolved += 1;
                }
            }
        }
    }
    unresolved
}

/// Observes a Desktop destruction, submitted here or reported by the OS:
/// once the census no longer lists the Space, every sampled window is owed a
/// membership elsewhere (macOS migrates them; a window that vanished is
/// reported, not explained away) and every tab group of its rows is owed a
/// single new Space. Each is asked about until it answers or the deadline
/// passes, and an answer once observed stands. Then the ECS is reconciled
/// with what was observed — verified or not, the truth is logged — and
/// whatever was not observed keeps its last known layout. A Space still
/// listed at the deadline, or a census that cannot be read, ends the wait
/// without touching its rows; nothing is retried. Two observations of one
/// Space collapse into the first.
///
/// The one exception is the OS reporting the fullscreen Space of a window
/// this side put in a fullscreen row gone: nothing here asked for that, no
/// window was sampled, and the Space is the window server's own to take
/// away when the window leaves fullscreen, so the notification is the word
/// for it and the window goes back to its remembered slot at once — the
/// fullscreen restore as it always was, with no census wait imposed on it.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
fn confirm_native_space_destruction(
    pending: Populated<(Entity, &mut NativeSpaceDestroyPending)>,
    placements: Query<(Entity, &NativeSpacePlacementPending)>,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    mut strips: NativeStrips,
    windows: Windows,
    displays: Displays,
    mut focus_history: ResMut<FocusHistory>,
    _platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    mut commands: Commands,
) {
    let now = time.elapsed();
    let mut handled = Vec::new();
    for (entity, mut pending) in pending {
        let workspace_id = pending.workspace_id;
        if handled.contains(&workspace_id) {
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
            continue;
        }
        handled.push(workspace_id);
        if now < pending.next_check {
            continue;
        }
        let waited = now.saturating_sub(pending.requested_at);
        let expired = past_deadline(pending.requested_at, now);

        if matches!(pending.origin, DestroyOrigin::Notification)
            && strips.iter().any(|(_, strip, fullscreen, _, _)| {
                strip.id() == workspace_id && fullscreen.is_some()
            })
        {
            debug!(
                workspace_id,
                "fullscreen native Space destroyed by the OS; its window goes back to its remembered slot"
            );
            drop_placements(workspace_id, &placements, &mut commands);
            reconcile_destroyed_space(
                workspace_id,
                &pending.migrated,
                &window_manager,
                &mut strips,
                &windows,
                &displays,
                &mut focus_history,
                &mut commands,
            );
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
            continue;
        }

        let present = match window_manager.native_space_info() {
            Ok(census) => census.iter().any(|space| space.id == workspace_id),
            Err(err) => {
                warn!(
                    workspace_id,
                    ?waited,
                    "unable to confirm native Desktop destruction: {err}; its rows keep their layout until a later notification"
                );
                if let Ok(mut entity_commands) = commands.get_entity(entity) {
                    entity_commands.try_despawn();
                }
                continue;
            }
        };
        if present {
            if !expired {
                pending.next_check = now + NATIVE_CHECK_INTERVAL;
                continue;
            }
            warn!(
                workspace_id,
                ?waited,
                "native Desktop still in the census after the deadline; destruction not observed and not retried"
            );
            if let Ok(mut entity_commands) = commands.get_entity(entity) {
                entity_commands.try_despawn();
            }
            continue;
        }

        // The Space is gone; only what has not answered yet is asked again.
        pending.survivors.observe(&window_manager, workspace_id);
        let unresolved_groups = observe_groups(&mut pending, &strips, &windows, &window_manager);
        let unresolved_windows = pending.survivors.unresolved.len();
        if (unresolved_windows > 0 || unresolved_groups > 0) && !expired {
            pending.next_check = now + NATIVE_CHECK_INTERVAL;
            continue;
        }

        let survivors = &pending.survivors;
        let complete =
            unresolved_windows == 0 && unresolved_groups == 0 && survivors.lost.is_empty();
        match pending.origin {
            DestroyOrigin::Command { migrate, receipt } if complete && receipt => debug!(
                workspace_id,
                migrate,
                landed = ?survivors.landed,
                migrated = ?pending.migrated,
                ?waited,
                "native Desktop destruction confirmed"
            ),
            DestroyOrigin::Command {
                migrate,
                receipt: true,
            } => warn!(
                workspace_id,
                migrate,
                landed = ?survivors.landed,
                lost = ?survivors.lost,
                unresolved = ?survivors.unresolved,
                unresolved_groups,
                ?waited,
                "native Desktop is gone, but not every sampled window has a confirmed membership elsewhere"
            ),
            DestroyOrigin::Command {
                migrate,
                receipt: false,
            } => warn!(
                workspace_id,
                migrate,
                unresolved_groups,
                ?waited,
                "native Desktop is gone after an uncertain request; its windows were not sampled, so their migration is unverified"
            ),
            DestroyOrigin::Notification if complete => debug!(
                workspace_id,
                migrated = ?pending.migrated,
                ?waited,
                "native Space destroyed by the OS; its windows' new Spaces observed"
            ),
            DestroyOrigin::Notification => warn!(
                workspace_id,
                unresolved_groups,
                ?waited,
                "native Space destroyed by the OS, but not every window of its rows has a single new Space; those keep their last layout"
            ),
        }

        drop_placements(workspace_id, &placements, &mut commands);

        reconcile_destroyed_space(
            workspace_id,
            &pending.migrated,
            &window_manager,
            &mut strips,
            &windows,
            &displays,
            &mut focus_history,
            &mut commands,
        );
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_despawn();
        }
    }
}

/// Drops the row placements still waiting for `workspace_id`: a Space that
/// is gone has nothing left to place.
fn drop_placements(
    workspace_id: WorkspaceId,
    placements: &Query<(Entity, &NativeSpacePlacementPending)>,
    commands: &mut Commands,
) {
    for (placement, pending) in placements {
        if pending.workspace_id == workspace_id
            && let Ok(mut entity_commands) = commands.get_entity(placement)
        {
            entity_commands.try_despawn();
        }
    }
}

/// The tab-preserving groups of a strip, column by column.
fn strip_groups(strip: &LayoutStrip) -> Vec<Vec<Entity>> {
    let mut groups = Vec::new();
    for column in strip.columns() {
        match column {
            Column::Single(entity) | Column::Fullscren(entity) => groups.push(vec![*entity]),
            Column::Tabs(tabs) => groups.push(tabs.clone()),
            Column::Stack(items) => {
                for item in items {
                    match item {
                        StackItem::Single(entity) => groups.push(vec![*entity]),
                        StackItem::Tabs(tabs) => groups.push(tabs.clone()),
                    }
                }
            }
        }
    }
    groups
}

/// The row of `target` a landing tab group joins: the one on screen, else the
/// one selected on its display, else its lowest row — never one of
/// `excluded`, the rows about to go. Read only, so a row can be resolved
/// before anything is rewritten.
fn destination_row(
    strips: &NativeStrips,
    target: WorkspaceId,
    excluded: &[Entity],
) -> Option<Entity> {
    strips
        .iter()
        .filter(|(entity, strip, _, _, _)| strip.id() == target && !excluded.contains(entity))
        .min_by_key(|(_, strip, _, active, selected)| {
            (!(*active || *selected), strip.virtual_index)
        })
        .map(|(entity, _, _, _, _)| entity)
}

/// Reconciles the rows of the destroyed Space `workspace_id` with what was
/// observed: a window that left native fullscreen goes back to its remembered
/// slot, a tab group observed on another Space joins that Space's destination
/// row (or a row 0 spawned under the display that owns it), and a group whose
/// whereabouts were not established keeps its row and tiled state — its last
/// known layout — for a later native event to settle, rather than being
/// floated or given a destination by guess. A row goes once nothing is left
/// in it.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn reconcile_destroyed_space(
    workspace_id: WorkspaceId,
    migrated: &[(Vec<Entity>, WorkspaceId)],
    window_manager: &WindowManager,
    strips: &mut NativeStrips,
    windows: &Windows,
    displays: &Displays,
    focus_history: &mut FocusHistory,
    commands: &mut Commands,
) {
    focus_history.forget_workspace(workspace_id);

    // Read everything first: the rows to empty, and where their groups go.
    let mut doomed = Vec::new();
    let mut fullscreen_restore = None;
    let mut landing: Vec<(WorkspaceId, Vec<Entity>)> = Vec::new();
    let mut leaving: Vec<Entity> = Vec::new();
    let mut retained: Vec<Vec<Entity>> = Vec::new();
    for (entity, strip, fullscreen, _, _) in &*strips {
        if strip.id() != workspace_id {
            continue;
        }
        doomed.push(entity);
        if let Some(marker) = fullscreen {
            if let Some(window) = strip.first().ok().and_then(|column| column.top()) {
                fullscreen_restore = Some((window, marker.clone()));
                leaving.push(window);
            }
            continue;
        }
        for group in strip_groups(strip) {
            if !group.iter().any(|member| windows.get(*member).is_some()) {
                // Closed windows leave with their row.
                leaving.extend(group);
                continue;
            }
            match migrated
                .iter()
                .find(|(members, _)| members.iter().any(|member| group.contains(member)))
            {
                Some((_, destination)) => {
                    leaving.extend(group.iter().copied());
                    landing.push((*destination, group));
                }
                None => retained.push(group),
            }
        }
    }
    if doomed.is_empty() {
        return;
    }

    if let Some((window, marker)) = fullscreen_restore {
        let mut home = strips
            .iter_mut()
            .find_map(|(entity, strip, _, _, _)| (entity == marker.layout_strip).then_some(strip));
        if home.is_none() {
            home = strips.iter_mut().find_map(|(_, strip, _, _, _)| {
                (strip.id() == marker.workspace_id).then_some(strip)
            });
        }
        debug!(
            "previously fullscreened window {window} inserted at {}",
            marker.index
        );
        if let Some(mut strip) = home {
            strip.insert_at(marker.index, window);
            commands.reshuffle_around(window);
        }
    }

    // Groups observed elsewhere join that Space's destination row; a Space
    // without a row gets one under the display that owns it. One nobody owns
    // yet keeps the group where it was.
    let mut new_rows: Vec<(WorkspaceId, Entity, Origin, LayoutStrip)> = Vec::new();
    for (destination, group) in landing {
        if let Some(row) = destination_row(strips, destination, &doomed)
            && let Ok((_, mut row, _, _, _)) = strips.get_mut(row)
        {
            debug!(
                workspace_id,
                destination,
                ?group,
                "windows migrated off the destroyed native Space"
            );
            row.append_tab_group(&group);
            if let Some(leader) = group.first() {
                commands.reshuffle_around(*leader);
            }
        } else if let Some((_, _, _, strip)) = new_rows
            .iter_mut()
            .find(|(pending_row, _, _, _)| *pending_row == destination)
        {
            strip.append_tab_group(&group);
        } else if let Some((display_entity, display, _)) =
            owning_display(destination, window_manager, displays)
        {
            debug!(
                workspace_id,
                destination,
                ?group,
                "windows migrated to a native Space without a row; row 0 on display {display_entity}"
            );
            let mut strip = LayoutStrip::new(destination, 0);
            strip.append_tab_group(&group);
            new_rows.push((destination, display_entity, display.bounds().min, strip));
        } else {
            warn!(
                workspace_id,
                destination,
                ?group,
                "windows migrated to a native Space no present display owns; keeping their row until one does"
            );
            leaving.retain(|member| !group.contains(member));
            retained.push(group);
        }
    }
    for (_, display_entity, origin, strip) in new_rows {
        commands.spawn_layout_strip(strip, origin, display_entity, false);
    }

    // Empty the doomed rows of what left. A row with nothing left goes; one
    // still holding a group whose new Space was not observed keeps it, as the
    // last known layout, and is marked so it goes once whatever later places
    // those windows has emptied it.
    for (entity, mut strip, _, _, _) in &mut *strips {
        if !doomed.contains(&entity) {
            continue;
        }
        for member in &leaving {
            if strip.contains(*member) {
                strip.remove(*member);
            }
        }
        let Ok(mut entity_commands) = commands.get_entity(entity) else {
            continue;
        };
        if strip.len() == 0 {
            debug!(workspace_id, "workspace destroyed, dropping row {entity}");
            entity_commands.try_despawn();
        } else {
            warn!(
                workspace_id,
                remaining = ?strip.all_windows(),
                "row {entity} of the destroyed native Space keeps windows whose new Space was not observed; left as last known until a native event places them"
            );
            entity_commands.try_insert(DestroyedSpaceMarker);
        }
    }
    if !retained.is_empty() {
        debug!(workspace_id, ?retained, "groups left in place unreconciled");
    }
}

/// Drops the rows of destroyed Spaces once they are empty. The windows a
/// destruction's observation could not place are taken out of their old row
/// by whatever later reconciles them — the Space they turned up on going on
/// screen, or their closing — and nothing is left to keep the row. Runs only
/// when such a row was marked or rewritten, never as a scan, and touches no
/// row of a Space the census still lists.
#[allow(clippy::needless_pass_by_value)]
fn reap_destroyed_space_rows(rows: RewrittenDestroyedRows, mut commands: Commands) {
    for (entity, strip) in rows {
        if strip.len() != 0 {
            continue;
        }
        debug!(
            workspace_id = strip.id(),
            "row {entity} of a destroyed native Space emptied; dropping it"
        );
        if let Ok(mut entity_commands) = commands.get_entity(entity) {
            entity_commands.try_despawn();
        }
    }
}

/// Where a window framed at `frame` belongs once it is confirmed on a Space
/// of the display with `bounds` and `viewport`: nowhere new while the frame
/// already touches that display; otherwise at the offset it had on whichever
/// of `others` showed it (or from the display's own origin when none did),
/// clamped into the viewport. Shared by followers carried to the Space on
/// screen and floats moved to a Space on another display.
pub(super) fn origin_on_display(
    frame: IRect,
    bounds: IRect,
    viewport: IRect,
    mut others: impl Iterator<Item = IRect>,
) -> Option<Origin> {
    if !bounds.intersect(frame).is_empty() {
        return None;
    }
    let relative_origin = others
        .find(|other| !other.intersect(frame).is_empty())
        .map_or(frame.min - bounds.min, |other| frame.min - other.min);
    let destination =
        clamp_origin_to_viewport(viewport.min + relative_origin, frame.size(), viewport);
    (destination != frame.min).then_some(destination)
}

/// The row a landed tiled group joins on its target, resolved before any row
/// is rewritten.
#[derive(Clone, Copy, Debug)]
enum Placement {
    /// An existing row of the target.
    Row(Entity),
    /// A row 0 to spawn under the display that owns the target, at its
    /// origin.
    NewRow { display: Entity, origin: Origin },
}

/// Where a landed tiled group goes on `target`: the target's destination row
/// — the one on screen, else the one selected on its display, else its
/// lowest — or a row 0 under the display that owns the target. Nothing while
/// the target has no row and no present display the ECS knows lists it.
fn resolve_placement(
    target: WorkspaceId,
    strips: &NativeStrips,
    displays: &Displays,
    window_manager: &WindowManager,
) -> Option<Placement> {
    if let Some(row) = destination_row(strips, target, &[]) {
        return Some(Placement::Row(row));
    }
    owning_display(target, window_manager, displays).map(|(display_entity, display, _)| {
        Placement::NewRow {
            display: display_entity,
            origin: display.bounds().min,
        }
    })
}

/// Keeps a landed batch waiting for its row, without touching the layout.
/// When the census names an owner the ECS has no display for, the display
/// reconciliation is asked once to catch up — the same path a display
/// arriving takes — so the row turns up through it; a census that lists no
/// owner yet is simply looked at again at the next check.
fn keep_placing(
    placing: &mut Placing,
    target: WorkspaceId,
    window_manager: &WindowManager,
    displays: &Displays,
    now: Duration,
    commands: &mut Commands,
) {
    if !placing.reconciliation_requested
        && let Some(display_id) = census_owner(target, window_manager)
        && !displays
            .iter()
            .any(|(_, display, _)| display.id() == display_id)
    {
        debug!(
            workspace_id = target,
            display_id,
            "the native Space's owning display is not in the ECS; asking the display reconciliation to catch up"
        );
        commands.trigger(SendMessageTrigger(Event::DisplayConfigured { display_id }));
        placing.reconciliation_requested = true;
    }
    placing.next_check = now + NATIVE_CHECK_INTERVAL;
}

/// Takes the landed members out of every row but the target's and forgets
/// every traveling member's source focus history. Replacement focus must not
/// be another member of the same move, even when that member did not land.
fn leave_source_rows(
    leader: Entity,
    pending: &SpaceMovePending,
    moved: &[Entity],
    strips: &mut NativeStrips,
    focus_history: &mut FocusHistory,
) -> Option<Entity> {
    let mut source_focus = None;
    for (_, mut strip, _, _, _) in &mut *strips {
        if strip.id() == pending.target || !moved.iter().any(|entity| strip.contains(*entity)) {
            continue;
        }
        let held_leader = strip.contains(leader);
        if held_leader {
            source_focus = strip
                .left_neighbour(leader)
                .filter(|entity| !pending.travels(*entity))
                .or_else(|| {
                    strip
                        .right_neighbour(leader)
                        .filter(|entity| !pending.travels(*entity))
                });
        }
        for entity in moved {
            strip.remove(*entity);
        }
        if held_leader && source_focus.is_none() {
            source_focus = strip
                .all_columns()
                .into_iter()
                .find(|entity| !pending.travels(*entity));
        }
    }
    for (entity, _) in &pending.members {
        focus_history.forget(*entity);
    }
    source_focus
}

/// Rows the landed members that are tiled now on the target, preserving
/// each logical tab group and keeping independent associated children separate.
fn join_target_row(
    target: WorkspaceId,
    rowed: &[Vec<Entity>],
    placement: Placement,
    strips: &mut NativeStrips,
    commands: &mut Commands,
) {
    match placement {
        Placement::Row(row) => {
            if let Ok((_, mut strip, _, _, _)) = strips.get_mut(row) {
                for group in rowed {
                    strip.append_tab_group(group);
                }
            }
        }
        Placement::NewRow {
            display: display_entity,
            origin,
        } => {
            debug!(
                workspace_id = target,
                "row 0 for the move's destination on display {display_entity}"
            );
            let mut strip = LayoutStrip::new(target, 0);
            for group in rowed {
                strip.append_tab_group(group);
            }
            commands.spawn_layout_strip(strip, origin, display_entity, false);
        }
    }
}

/// Frames a floating leader confirmed on a Space of another display for that
/// display's viewport; one already on the target's display is left alone,
/// as is one whose display the ECS does not know.
fn reframe_landed_float(
    leader: Entity,
    target: WorkspaceId,
    frame: IRect,
    window_manager: &WindowManager,
    displays: &Displays,
    config: &Config,
    commands: &mut Commands,
) {
    if let Some((display_entity, display, dock)) = owning_display(target, window_manager, displays)
        && let Some(destination) = origin_on_display(
            frame,
            display.bounds(),
            display.actual_display_bounds(dock, config),
            displays
                .iter()
                .filter(|(other, _, _)| *other != display_entity)
                .map(|(_, other, _)| other.bounds()),
        )
    {
        debug!(
            workspace_id = target,
            "moving floating window {leader} onto display {display_entity} at {destination}"
        );
        commands.reposition_entity(leader, destination);
    }
}

/// Picks what the source Space keeps focused once the leader has left it
/// with the focus marker — after a `Stay` move, or a `Follow` whose
/// destination could not be brought on screen: the leader's old neighbour,
/// else the Space's last focused tiled or floating window that did not
/// travel, else nothing at all. A source the user has meanwhile switched
/// away from only loses the leader's focus marker: focusing a window there
/// would raise it and pull the OS back.
fn keep_source_focus(
    leader: Entity,
    pending: &SpaceMovePending,
    neighbour: Option<Entity>,
    strips: &NativeStrips,
    windows: &Windows,
    focus_history: &FocusHistory,
    commands: &mut Commands,
) {
    let source_on_screen = strips
        .iter()
        .any(|(_, strip, _, active, _)| active && strip.id() == pending.source);
    let on_source = |entity: Entity| {
        !pending.travels(entity)
            && windows
                .get_managed(entity)
                .is_some_and(|(_, _, flags)| flags.is_tiled())
            && strips
                .iter()
                .any(|(_, strip, _, _, _)| strip.id() == pending.source && strip.contains(entity))
    };
    let remembered = focus_history
        .last_managed(pending.source)
        .filter(|entity| on_source(*entity))
        .or_else(|| {
            focus_history
                .last_floating(pending.source)
                .filter(|entity| {
                    !pending.travels(*entity)
                        && windows
                            .get_managed(*entity)
                            .is_some_and(|(_, _, flags)| flags.is_floating())
                })
        });
    if let Some(entity) = neighbour
        .filter(|entity| on_source(*entity))
        .or(remembered)
        .filter(|_| source_on_screen)
    {
        debug!(
            workspace_id = pending.source,
            "focus stays on {entity} after sending {leader} away"
        );
        commands.focus_entity(entity, false);
    } else {
        debug!(
            workspace_id = pending.source,
            source_on_screen, "nothing to focus on the source Space after sending {leader} away"
        );
        for (entity, _) in &pending.members {
            if let Ok(mut entity_commands) = commands.get_entity(*entity) {
                entity_commands.try_remove::<FocusedMarker>();
            }
        }
    }
}

/// The members of `group` the window server now reports on `target`: the
/// truth to reconcile against once a batch could not be confirmed as a whole,
/// since a native write can land despite an error or after the deadline.
fn members_on_target(
    group: &[(Entity, WinID)],
    target: WorkspaceId,
    window_manager: &WindowManager,
) -> Vec<Entity> {
    group
        .iter()
        .filter(|(_, window_id)| {
            matches!(
                window_manager.window_workspaces(*window_id).as_deref(),
                Ok([workspace]) if *workspace == target
            )
        })
        .map(|(entity, _)| *entity)
        .collect()
}

fn live_native_members(
    group: &NativeTabGroup,
    windows: &Windows,
    manager: &WindowManager,
) -> Result<Vec<(Entity, WinID)>> {
    group
        .members
        .iter()
        .copied()
        .filter_map(|(entity, id)| match manager.window_exists(id) {
            Ok(false) => None,
            Ok(true) if windows.get(entity).is_some_and(|window| window.id() == id) => {
                Some(Ok((entity, id)))
            }
            Ok(true) => Some(Err(Error::Generic("live native tab is not tracked".into()))),
            Err(err) => Some(Err(err)),
        })
        .collect()
}

/// Only authoritative closure of a known ID removes an identity obligation.
/// An unexposed title has no ID with which closure could be proved.
fn live_native_identity<'a>(
    group: &'a NativeTabGroup,
    live: &'a [(Entity, WinID)],
) -> impl Iterator<Item = &'a (String, Option<WinID>)> {
    group
        .identity
        .iter()
        .filter(move |(_, id)| id.is_none_or(|id| live.iter().any(|(_, live_id)| *live_id == id)))
}

/// Resolves only the Cocoa members of the submitted move. Detaching a member
/// restores strict per-window confirmation; merging with a new, unsubmitted
/// window is not evidence that the original logical group traveled intact.
fn logical_landing(
    known: &[NativeTabGroup],
    windows: &Windows,
    manager: &WindowManager,
    target: WorkspaceId,
) -> Result<(Vec<Entity>, Vec<Vec<Entity>>, bool)> {
    let mut landed = Vec::new();
    let mut placement = Vec::new();
    let mut whole = true;
    for old in known {
        let live = live_native_members(old, windows, manager)?;
        let Some(&(first, _)) = live.first() else {
            whole &= old.identity.iter().all(|(_, id)| id.is_some());
            continue;
        };
        let pid = windows.get(first).ok_or(Error::InvalidWindow)?.pid()?;
        let observed = observe_app_groups(windows, manager, pid)?;
        let mut accounted = Vec::new();
        for group in observed
            .iter()
            .filter(|group| group.members.iter().any(|member| live.contains(member)))
        {
            if group.members.iter().any(|member| !live.contains(member)) {
                return Err(Error::Generic(
                    "native tab group merged with an unsubmitted window during move".into(),
                ));
            }
            // Known members alone are insufficient: a startup titlebar can
            // include tabs that have never had an AXWindow or an ECS entity.
            // Fresh selection was validated against the current representative;
            // every logical title and resolved ID must still match submission.
            if !group.identity.iter().eq(live_native_identity(old, &live)) {
                return Err(Error::Generic(
                    "native tab identity changed during move".into(),
                ));
            }
            accounted.extend(group.members.iter().map(|(entity, _)| *entity));
            if group.workspace == target {
                let entities = group
                    .members
                    .iter()
                    .map(|(entity, _)| *entity)
                    .collect::<Vec<_>>();
                landed.extend_from_slice(&entities);
                placement.push(entities);
            } else {
                whole = false;
            }
        }
        // Closing the other known tabs can leave one ordinary AXWindow with
        // no titlebar at all. Its exact native membership is sufficient;
        // unknown startup titles cannot take this path.
        let single_known_survivor =
            live.len() == 1 && live_native_identity(old, &live).count() == 1;
        for (entity, id) in live {
            if accounted.contains(&entity) {
                continue;
            }
            let spaces = manager.window_workspaces(id)?;
            let on_target = matches!(spaces.as_slice(), [space] if *space == target);
            whole &= single_known_survivor && on_target;
            if on_target {
                landed.push(entity);
                placement.push(vec![entity]);
            }
        }
    }
    Ok((landed, placement, whole))
}

fn reconcile_move_groups(
    groups: &mut Vec<Vec<Entity>>,
    known: &[NativeTabGroup],
    observed: Vec<Vec<Entity>>,
    manager: &WindowManager,
    target: WorkspaceId,
) {
    if known.is_empty() {
        return;
    }
    for group in &mut *groups {
        group.retain(|member| {
            !known
                .iter()
                .any(|native| native.members.iter().any(|(entity, _)| entity == member))
        });
    }
    groups.retain(|group| !group.is_empty());
    groups.extend(observed);
    // Unreadable titlebars revoke grouping/follow authority, not a physical
    // landing already proved by exact native membership. Keep those rows.
    for &(entity, id) in known.iter().flat_map(|group| &group.members) {
        if !groups.iter().any(|group| group.contains(&entity))
            && matches!(manager.window_workspaces(id).as_deref(), Ok([space]) if *space == target)
        {
            groups.push(vec![entity]);
        }
    }
}

fn move_focus_target(
    leader: Entity,
    pending: &SpaceMovePending,
    windows: &Windows,
    manager: &WindowManager,
) -> Option<Entity> {
    let target = if let Some(group) = pending
        .native_groups
        .iter()
        .find(|group| group.members.iter().any(|(member, _)| *member == leader))
    {
        let pid = group
            .members
            .iter()
            .find_map(|(entity, _)| windows.get(*entity).and_then(|window| window.pid().ok()))?;
        let live = live_native_members(group, windows, manager).ok()?;
        let observed = observe_app_groups(windows, manager, pid).ok()?;
        if let Some(current) = observed.iter().find(|current| {
            current
                .members
                .iter()
                .any(|member| group.members.contains(member))
        }) {
            if current.workspace != pending.target
                || !current
                    .identity
                    .iter()
                    .eq(live_native_identity(group, &live))
                || current
                    .members
                    .iter()
                    .any(|member| !group.members.contains(member))
            {
                return None;
            }
            current.selected
        } else if live.len() == 1 && live_native_identity(group, &live).count() == 1 {
            live[0].0
        } else {
            return None;
        }
    } else {
        leader
    };
    let (window, _, flags) = windows.get_managed(target)?;
    (!flags.is_suspended()
        && matches!(manager.window_workspaces(window.id()).as_deref(), Ok([space]) if *space == pending.target))
        .then_some(target)
}

/// Drops the move and hands every current floating follower back to its own
/// carry, including associated children whose carry was withheld in flight.
/// The active native Space is read once and the follower machinery confirms
/// membership before writing, so already-landed members need no extra write.
fn release_move(
    entity: Entity,
    pending: &SpaceMovePending,
    member_flags: &MemberFlags,
    windows: &Windows,
    window_manager: &WindowManager,
    commands: &mut Commands,
) {
    if let Ok(mut entity_commands) = commands.get_entity(entity) {
        entity_commands.try_remove::<SpaceMovePending>();
    }
    let mut current = None;
    for (member, _) in &pending.members {
        let carried = member_flags.get(*member).is_ok_and(|(follows, _)| follows)
            && windows
                .get_managed(*member)
                .is_some_and(|(window, _, flags)| flags.is_floating() && !window.is_full_screen());
        if !carried {
            continue;
        }
        let workspace = current.get_or_insert_with(|| {
            window_manager
                .active_display_id()
                .and_then(|display_id| window_manager.active_display_space(display_id))
        });
        match workspace {
            Ok(current) => {
                if let Ok(mut entity_commands) = commands.get_entity(*member) {
                    entity_commands.try_insert(FollowSpacePending::new(*current));
                }
            }
            Err(err) => warn!(
                "unable to read the current native Space to hand followed window {member} back: {err}"
            ),
        }
    }
}

/// Drives every explicit move in flight. A landed batch is the only thing
/// that rewrites the layout, and only once the row its tiled members join
/// has been resolved: a target with no row and no display the ECS knows is
/// waited on, boundedly, with the layout left as it was. `Stay` then settles
/// the source's focus, `Follow` hands the destination's activation to
/// [`InstantSpaceSwitch`] and focuses the moved window once that has been
/// confirmed; an activation refused, not observed or not current once it
/// settled leaves the moved window unfocused and settles the source's focus
/// the way `Stay` does while the leader still holds the focus marker, so
/// no window off screen keeps it. A batch given up on is reconciled against
/// what actually landed rather than assumed unmoved, but is never treated
/// as complete: it neither activates nor focuses anything, and nothing is
/// ever resubmitted.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
fn observe_space_moves(
    pending: MovingLeaders,
    time: Res<Time>,
    window_manager: Res<WindowManager>,
    mut strips: NativeStrips,
    displays: Displays,
    windows: Windows,
    member_flags: MemberFlags,
    config: Res<Config>,
    mut focus_history: ResMut<FocusHistory>,
    mut instant_space_switch: ResMut<InstantSpaceSwitch>,
    _platform: Option<NonSend<Pin<Box<PlatformCallbacks>>>>,
    mut commands: Commands,
) {
    let now = time.elapsed();
    for (entity, window, mut pending) in pending {
        let window_id = window.id();
        let pending = &mut *pending;
        let target = pending.target;
        let follow_focus = matches!(pending.focus, MoveFocus::Follow);
        let focused_member = windows
            .focused()
            .map(|(_, member)| member)
            .filter(|member| pending.travels(*member));

        let (landed, whole) = match &mut pending.phase {
            MovePhase::Activating { neighbour } => {
                // The native switch observer owns activation confirmation.
                if instant_space_switch.pending_target() == Some(target) {
                    continue;
                }
                let neighbour = *neighbour;
                let activated = match window_manager.native_space_is_active(target) {
                    Ok(true) => {
                        if follow_focus
                            && let Some(focus) =
                                move_focus_target(entity, pending, &windows, &window_manager)
                        {
                            commands.focus_entity(focus, true);
                        }
                        true
                    }
                    Ok(false) => {
                        warn!(
                            window_id,
                            workspace_id = target,
                            "native Space not current after the move's activation settled; not focusing the moved window"
                        );
                        false
                    }
                    Err(err) => {
                        warn!(
                            window_id,
                            workspace_id = target,
                            "unable to tell whether the move's native Space is current: {err}"
                        );
                        false
                    }
                };
                // Respect a user focus change to a non-traveling window, but
                // never strand focus on a child that left with the leader.
                if !activated && focused_member.is_some() {
                    keep_source_focus(
                        entity,
                        pending,
                        neighbour,
                        &strips,
                        &windows,
                        &focus_history,
                        &mut commands,
                    );
                }
                release_move(
                    entity,
                    pending,
                    &member_flags,
                    &windows,
                    &window_manager,
                    &mut commands,
                );
                continue;
            }
            MovePhase::Landing(request) => {
                if !request.due(now) {
                    continue;
                }
                let logical =
                    logical_landing(&pending.native_groups, &windows, &window_manager, target);
                let strict = batch_landed(
                    &window_manager,
                    &mut request.windows,
                    window_id,
                    request.target,
                );
                let result = match &logical {
                    Ok((_, _, whole)) => strict.map(|landed| landed && *whole),
                    Err(err) => Err(Error::Generic(err.to_string())),
                };
                let Some(confirmed) = request.observe_result(result, window_id, now) else {
                    continue;
                };
                let (logical_landed, logical_groups, _) = logical.unwrap_or_default();
                reconcile_move_groups(
                    &mut pending.groups,
                    &pending.native_groups,
                    logical_groups,
                    &window_manager,
                    target,
                );
                // Native closure can be observed before the ECS removal. A
                // member pruned from the request must not be placed as landed.
                let live = pending
                    .members
                    .iter()
                    .filter(|(member, member_id)| {
                        (request.windows.contains(member_id) || logical_landed.contains(member))
                            && windows
                                .get(*member)
                                .is_some_and(|window| window.id() == *member_id)
                    })
                    .map(|(member, _)| *member)
                    .collect::<Vec<_>>();
                let landed = if confirmed {
                    live
                } else {
                    let mut landed = members_on_target(&pending.members, target, &window_manager)
                        .into_iter()
                        .filter(|member| live.contains(member))
                        .collect::<Vec<_>>();
                    for member in logical_landed {
                        if !landed.contains(&member) {
                            landed.push(member);
                        }
                    }
                    warn!(
                        window_id,
                        workspace_id = target,
                        ?landed,
                        members = ?live,
                        "reconciling the members that did reach the native Space"
                    );
                    landed
                };
                // All native IDs, including untracked associated windows,
                // must confirm before following is allowed.
                let whole = confirmed && landed.contains(&entity);
                (landed, whole)
            }
            MovePhase::Placing(placing) => {
                if now < placing.next_check {
                    continue;
                }
                let logical =
                    logical_landing(&pending.native_groups, &windows, &window_manager, target);
                let (logical_landed, logical_groups, logical_whole) = logical.unwrap_or_default();
                reconcile_move_groups(
                    &mut pending.groups,
                    &pending.native_groups,
                    logical_groups,
                    &window_manager,
                    target,
                );
                let landed = pending
                    .members
                    .iter()
                    .filter(|(member, member_id)| {
                        placing.landed.contains(member)
                            && (!pending.native_groups.iter().any(|native| {
                                native.members.iter().any(|(entity, _)| entity == member)
                            }) || logical_landed.contains(member)
                                || matches!(window_manager.window_workspaces(*member_id).as_deref(), Ok([space]) if *space == target))
                            && windows
                                .get(*member)
                                .is_some_and(|window| window.id() == *member_id)
                    })
                    .map(|(member, _)| *member)
                    .collect::<Vec<_>>();
                (landed, placing.whole && logical_whole)
            }
        };

        // Current visibility and persistent mode decide participation at
        // landing, independently for every member and logical group.
        let rowed = pending
            .groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .copied()
                    .filter(|member| {
                        landed.contains(member)
                            && windows
                                .get_managed(*member)
                                .is_some_and(|(_, _, flags)| flags.is_tiled())
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|group| !group.is_empty())
            .collect::<Vec<_>>();
        let placement = if rowed.is_empty() {
            None
        } else {
            resolve_placement(target, &strips, &displays, &window_manager)
        };
        if !rowed.is_empty() && placement.is_none() {
            // Keep the last known layout until the destination row is known.
            if let MovePhase::Placing(placing) = &mut pending.phase {
                if past_deadline(placing.since, now) {
                    warn!(
                        window_id,
                        workspace_id = target,
                        ?rowed,
                        waited = ?now.saturating_sub(placing.since),
                        "no row or display the ECS knows for the native Space before the deadline; the moved windows keep their last known layout"
                    );
                    release_move(
                        entity,
                        pending,
                        &member_flags,
                        &windows,
                        &window_manager,
                        &mut commands,
                    );
                } else {
                    keep_placing(
                        placing,
                        target,
                        &window_manager,
                        &displays,
                        now,
                        &mut commands,
                    );
                }
            } else {
                debug!(
                    window_id,
                    workspace_id = target,
                    ?rowed,
                    "windows confirmed on a native Space with no row and no display the ECS knows; their layout waits for one"
                );
                let mut placing = Placing::new(landed, whole, now);
                keep_placing(
                    &mut placing,
                    target,
                    &window_manager,
                    &displays,
                    now,
                    &mut commands,
                );
                pending.phase = MovePhase::Placing(placing);
            }
            continue;
        }

        let neighbour = if landed.is_empty() {
            None
        } else {
            let neighbour =
                leave_source_rows(entity, pending, &landed, &mut strips, &mut focus_history);
            if let Some(placement) = placement {
                join_target_row(target, &rowed, placement, &mut strips, &mut commands);
            }
            let (virtual_index, index) = destination_row(&strips, target, &[])
                .and_then(|row| strips.get(row).ok())
                .map_or((0, 0), |(_, strip, _, _, _)| {
                    (strip.virtual_index, strip.len())
                });
            for member in &landed {
                let Some((window, _, flags)) = windows.get_managed(*member) else {
                    continue;
                };
                if !flags.is_tiled()
                    && let Ok(mut entity_commands) = commands.get_entity(*member)
                {
                    // A later unsuspend or tile toggle must not restore the
                    // pre-move slot on the source Space.
                    entity_commands.try_insert(PreviousManagedStrip {
                        workspace_id: target,
                        virtual_index,
                        index,
                    });
                }
                if !flags.is_floating() {
                    continue;
                }
                let moving = member_flags
                    .get(*member)
                    .ok()
                    .and_then(|(_, moving)| moving);
                let frame = moving.map_or_else(
                    || window.frame(),
                    |RepositionMarker(origin)| {
                        IRect::from_corners(*origin, *origin + window.frame().size())
                    },
                );
                reframe_landed_float(
                    *member,
                    target,
                    frame,
                    &window_manager,
                    &displays,
                    &config,
                    &mut commands,
                );
            }
            neighbour
        };
        let stay = matches!(pending.focus, MoveFocus::Stay);
        if focused_member
            .is_some_and(|member| landed.contains(&member) || !pending.native_groups.is_empty())
            && (stay || !whole)
        {
            keep_source_focus(
                entity,
                pending,
                neighbour,
                &strips,
                &windows,
                &focus_history,
                &mut commands,
            );
        }
        if stay || !whole {
            // Partial outcomes reconcile truth, but never activate the target.
            release_move(
                entity,
                pending,
                &member_flags,
                &windows,
                &window_manager,
                &mut commands,
            );
            continue;
        }

        match submit_activation(&window_manager, target, &mut instant_space_switch, now) {
            Activation::Pending => pending.phase = MovePhase::Activating { neighbour },
            Activation::Current => {
                if let Some(focus) = move_focus_target(entity, pending, &windows, &window_manager) {
                    commands.focus_entity(focus, true);
                }
                release_move(
                    entity,
                    pending,
                    &member_flags,
                    &windows,
                    &window_manager,
                    &mut commands,
                );
            }
            Activation::Refused => {
                warn!(
                    window_id,
                    workspace_id = target,
                    "window moved but its native Space could not be activated; it is not focused there"
                );
                if focused_member.is_some() {
                    keep_source_focus(
                        entity,
                        pending,
                        neighbour,
                        &strips,
                        &windows,
                        &focus_history,
                        &mut commands,
                    );
                }
                release_move(
                    entity,
                    pending,
                    &member_flags,
                    &windows,
                    &window_manager,
                    &mut commands,
                );
            }
        }
    }
}

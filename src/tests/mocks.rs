use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, RwLock};

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;
use objc2_core_graphics::CGDirectDisplayID;
use stdext::prelude::RwLockExt;

use crate::errors::Error;
use crate::events::{DestroySource, Event};
use crate::manager::app::MockApplicationApi;
use crate::manager::{
    Application, Display, MockProcessApi, MockWindowApi, MockWindowManagerApi, Origin, Size,
    Window, origin_from, origin_to,
};
use crate::platform::{Modifiers, Pid, ProcessSerialNumber, WinID, WorkspaceId};

use super::*;

/// Data for a mocked application.
pub(crate) struct MockAppData {
    pub(crate) psn: ProcessSerialNumber,
    pub(crate) bundle_id: String,
    pub(crate) name: String,
    pub(crate) focused_window_id: Option<WinID>,
    pub(crate) is_frontmost: bool,
    pub(crate) connection: Option<crate::platform::ConnID>,
}

/// Data for a mocked window.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct MockWindowData {
    pub(crate) id: WinID,
    pub(crate) pid: Pid,
    pub(crate) frame: IRect,
    pub(crate) title: String,
    pub(crate) minimized: bool,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) visible: bool,
    pub(crate) role: String,
    pub(crate) subrole: String,
    pub(crate) identifier: String,
    pub(crate) is_full_screen: bool,
    pub(crate) border_radius: Option<f64>,
    pub(crate) horizontal_padding: i32,
    pub(crate) vertical_padding: i32,
    pub(crate) child_role: bool,
}

impl Default for MockWindowData {
    fn default() -> Self {
        Self {
            id: 0,
            pid: 0,
            frame: IRect::default(),
            title: String::new(),
            minimized: false,
            workspace_id: 0,
            visible: true,
            role: "AXWindow".to_string(),
            subrole: "AXStandardWindow".to_string(),
            identifier: "testid".to_string(),
            is_full_screen: false,
            border_radius: None,
            horizontal_padding: 0,
            vertical_padding: 0,
            child_role: false,
        }
    }
}

/// Data for a mocked display.
struct MockDisplayData {
    id: u32,
    bounds: IRect,
    workspaces: Vec<WorkspaceId>,
    active_workspace: WorkspaceId,
}

/// What the virtual window server does with a native Space request after the
/// manager has submitted it. The default mirrors a window server that keeps
/// up; the others model the asynchronous bridge honestly: a request that is
/// not yet applied is not reported as applied by any query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum NativeRequestOutcome {
    /// Applied before the submitting call returns.
    #[default]
    Immediate,
    /// Accepted, but held in flight until [`MockState::settle_native_requests`].
    Deferred,
    /// Refused before anything is submitted.
    Rejected,
    /// Reported as failed after submission; `applies` decides whether the
    /// window server nevertheless carries it out on settle.
    Uncertain { applies: bool },
}

/// A submitted native request the virtual window server has not applied yet.
enum PendingNativeRequest {
    Move {
        windows: Vec<WinID>,
        workspace_id: WorkspaceId,
    },
    Activate {
        workspace_id: WorkspaceId,
    },
}

/// The internal state of our "Virtual macOS".
struct MockStateInner {
    apps: HashMap<Pid, MockAppData>,
    windows: HashMap<WinID, MockWindowData>,
    displays: HashMap<u32, MockDisplayData>,
    fullscreen_spaces: HashSet<WorkspaceId>,
    active_display_id: u32,
    cursor_position: Origin,
    event_queue: VecDeque<Event>,
    /// Windows that are gone but which the app's AX window list still reports,
    /// modelling the lag real apps show right after a window closes.
    stale_window_ids: HashMap<WinID, Pid>,
    unordered_windows: HashSet<WinID>,
    /// Child windows the window server reports for a parent. Reported as
    /// recorded, like an app's window list: a closed child stays associated
    /// until it is explicitly disassociated, so a stale association can be
    /// handed to the manager after the child is already gone.
    associated_windows: HashMap<WinID, Vec<WinID>>,
    /// Windows on every Space at once; the window server refuses to move
    /// them, and reports every Space as their membership.
    sticky_windows: HashSet<WinID>,
    /// Windows whose per-window queries fail outright, independent of
    /// whether the window exists: the answer is an error, not absence.
    failing_window_queries: HashSet<WinID>,
    /// Every move the manager requested, whether the window server accepted,
    /// deferred or refused it.
    workspace_moves: Vec<(Vec<WinID>, WorkspaceId)>,
    workspace_focuses: Vec<WinID>,
    native_space_focuses: Vec<WorkspaceId>,
    /// Spaces whose cross-display focus policy was completed after
    /// confirmation.
    native_space_focus_completions: Vec<WorkspaceId>,
    native_move_outcome: NativeRequestOutcome,
    native_activation_outcome: NativeRequestOutcome,
    /// Whether an applied activation is announced with `SpaceChanged`, as
    /// the OS normally does. Off, the manager only learns of the switch by
    /// asking.
    activation_notifies: bool,
    pending_native_requests: Vec<PendingNativeRequest>,
}

impl MockStateInner {
    fn owning_display(&self, workspace_id: WorkspaceId) -> Option<u32> {
        self.displays.values().find_map(|display| {
            display
                .workspaces
                .contains(&workspace_id)
                .then_some(display.id)
        })
    }

    fn apply(&mut self, request: PendingNativeRequest) {
        match request {
            PendingNativeRequest::Move {
                windows,
                workspace_id,
            } => {
                // A destination that vanished while the request was in flight
                // cannot receive anything; the window server drops the move.
                if self.owning_display(workspace_id).is_none() {
                    return;
                }
                for window_id in windows {
                    if let Some(window) = self.windows.get_mut(&window_id) {
                        window.workspace_id = workspace_id;
                    }
                }
            }
            PendingNativeRequest::Activate { workspace_id } => {
                let Some(display_id) = self.owning_display(workspace_id) else {
                    return;
                };
                self.displays
                    .get_mut(&display_id)
                    .expect("finding owning display")
                    .active_workspace = workspace_id;
                if self.activation_notifies {
                    self.event_queue.push_back(Event::SpaceChanged);
                }
            }
        }
    }

    /// Routes a submitted request through the configured outcome.
    fn submit(
        &mut self,
        outcome: NativeRequestOutcome,
        workspace_id: WorkspaceId,
        request: PendingNativeRequest,
    ) -> crate::errors::Result<()> {
        match outcome {
            NativeRequestOutcome::Immediate => {
                self.apply(request);
                Ok(())
            }
            NativeRequestOutcome::Deferred => {
                self.pending_native_requests.push(request);
                Ok(())
            }
            NativeRequestOutcome::Rejected => Err(Error::NativeSpaceRequest {
                workspace_id,
                request_may_have_applied: false,
                message: "the virtual window server refused the request".to_string(),
            }),
            NativeRequestOutcome::Uncertain { applies } => {
                if applies {
                    self.pending_native_requests.push(request);
                }
                Err(Error::NativeSpaceRequest {
                    workspace_id,
                    request_may_have_applied: true,
                    message: "the virtual window server lost the reply".to_string(),
                })
            }
        }
    }

    /// The Spaces the window server reports for `window_id`, read the way
    /// `SLSCopySpacesForWindows` answers: an empty list for a window it no
    /// longer knows, every Space for a sticky one.
    fn window_memberships(&self, window_id: WinID) -> crate::errors::Result<Vec<WorkspaceId>> {
        self.check_window_query(window_id)?;
        let Some(window) = self.windows.get(&window_id) else {
            return Ok(vec![]);
        };
        if self.sticky_windows.contains(&window_id) {
            let mut displays = self.displays.values().collect::<Vec<_>>();
            displays.sort_unstable_by_key(|display| display.id);
            return Ok(displays
                .into_iter()
                .flat_map(|display| display.workspaces.iter().copied())
                .collect());
        }
        Ok(vec![window.workspace_id])
    }

    fn check_window_query(&self, window_id: WinID) -> crate::errors::Result<()> {
        if self.failing_window_queries.contains(&window_id) {
            return Err(Error::Generic(format!(
                "the virtual window server could not answer for window {window_id}"
            )));
        }
        Ok(())
    }

    /// Validates a move batch the way the bridge does before it submits
    /// anything: every member must exist and sit on exactly one Space. The
    /// first offender fails the whole batch; nothing is submitted.
    fn check_move_batch(&self, windows: &[WinID]) -> crate::errors::Result<()> {
        for &window_id in windows {
            match self.window_memberships(window_id)?.as_slice() {
                [] => {
                    return Err(Error::NotFound(format!(
                        "window {window_id} has no native Space membership; it may not exist"
                    )));
                }
                [_] => {}
                memberships => {
                    return Err(Error::InvalidInput(format!(
                        "window {window_id} belongs to {} native Spaces and cannot be moved",
                        memberships.len()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Whether `window_id` is attached to a parent. Attached windows are not
    /// standalone application windows: the window server leaves them out of
    /// its Space window lists and the app never offers them for management,
    /// so they only reach the manager through the parent's association.
    fn is_associated_child(&self, window_id: WinID) -> bool {
        self.associated_windows
            .values()
            .any(|children| children.contains(&window_id))
    }
}

#[derive(Clone)]
pub struct MockState {
    inner: Arc<RwLock<MockStateInner>>,
}

impl MockState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(MockStateInner {
                apps: HashMap::new(),
                windows: HashMap::new(),
                displays: HashMap::new(),
                fullscreen_spaces: HashSet::new(),
                active_display_id: 0,
                cursor_position: Origin::ZERO,
                event_queue: VecDeque::new(),
                stale_window_ids: HashMap::new(),
                unordered_windows: HashSet::new(),
                associated_windows: HashMap::new(),
                sticky_windows: HashSet::new(),
                failing_window_queries: HashSet::new(),
                workspace_moves: Vec::new(),
                workspace_focuses: Vec::new(),
                native_space_focuses: Vec::new(),
                native_space_focus_completions: Vec::new(),
                native_move_outcome: NativeRequestOutcome::default(),
                native_activation_outcome: NativeRequestOutcome::default(),
                activation_notifies: true,
                pending_native_requests: Vec::new(),
            })),
        }
    }

    #[allow(dead_code)]
    pub fn set_window_unordered(&self, window_id: WinID, unordered: bool) {
        let mut inner = self.inner.force_write();
        if unordered {
            inner.unordered_windows.insert(window_id);
        } else {
            inner.unordered_windows.remove(&window_id);
        }
    }

    pub(crate) fn window_visible(&self, window_id: WinID, visible: bool) {
        let mut state = self.inner.force_write();
        let window = state.windows.get_mut(&window_id).expect("finding window");
        window.visible = visible;
    }

    // --- OS Behavior Methods ---

    pub fn spawn_app(&self, pid: Pid, bundle_id: &str, name: &str) {
        let mut inner = self.inner.force_write();
        inner.apps.insert(
            pid,
            MockAppData {
                psn: ProcessSerialNumber {
                    high: 0,
                    low: pid.cast_unsigned(),
                },
                bundle_id: bundle_id.to_string(),
                name: name.to_string(),
                focused_window_id: None,
                is_frontmost: true,
                connection: Some(0),
            },
        );
    }

    pub fn spawn_window(
        &self,
        pid: Pid,
        workspace_id: WorkspaceId,
        id: WinID,
        frame: IRect,
    ) -> Window {
        let mut inner = self.inner.force_write();
        inner.windows.insert(
            id,
            MockWindowData {
                id,
                pid,
                frame,
                title: format!("Window {id}"),
                workspace_id,
                ..default()
            },
        );
        self.create_window(id)
    }

    /// Moves the app's focused window without emitting the notifications a
    /// real focus change would produce.
    pub fn set_focused_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        let Some(pid) = inner.windows.get(&id).map(|window| window.pid) else {
            return;
        };
        if let Some(app) = inner.apps.get_mut(&pid) {
            app.focused_window_id = Some(id);
        }
    }

    pub fn focus_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        if let Some(win) = inner.windows.get(&id) {
            let pid = win.pid;
            if let Some(app) = inner.apps.get_mut(&pid) {
                app.focused_window_id = Some(id);
                let psn = app.psn;
                inner
                    .event_queue
                    .push_back(Event::ApplicationFrontSwitched { psn });
                inner
                    .event_queue
                    .push_back(Event::WindowFocused { window_id: id });
            }
        }
    }

    pub fn add_display(&mut self, id: u32, bounds: IRect, workspaces: Vec<WorkspaceId>) {
        let active_workspace = workspaces.first().copied().unwrap_or_default();
        let mut inner = self.inner.force_write();
        if inner.displays.is_empty() {
            inner.active_display_id = id;
        }
        inner.displays.insert(
            id,
            MockDisplayData {
                id,
                bounds,
                workspaces,
                active_workspace,
            },
        );
    }

    #[allow(unused)]
    pub fn remove_display(&self, id: u32) {
        let mut inner = self.inner.force_write();
        inner.displays.remove(&id);
        if inner.active_display_id == id {
            inner.active_display_id = inner.displays.keys().copied().next().unwrap_or(0);
        }
    }

    pub fn active_display(&self) -> CGDirectDisplayID {
        self.inner.force_read().active_display_id
    }

    pub(crate) fn activate_workspace(
        &self,
        display_id: u32,
        workspace_id: WorkspaceId,
        fullscreen: bool,
    ) {
        let mut inner = self.inner.force_write();
        {
            let display = inner
                .displays
                .get_mut(&display_id)
                .expect("finding display");
            if !display.workspaces.contains(&workspace_id) {
                display.workspaces.push(workspace_id);
            }
            display.active_workspace = workspace_id;
        }
        if fullscreen {
            inner.fullscreen_spaces.insert(workspace_id);
        } else {
            inner.fullscreen_spaces.remove(&workspace_id);
        }
    }

    pub fn drain_events(&self) -> Vec<Event> {
        let mut inner = self.inner.force_write();
        inner.event_queue.drain(..).collect()
    }

    pub(crate) fn workspace_focuses(&self) -> Vec<WinID> {
        self.inner.force_read().workspace_focuses.clone()
    }

    pub(crate) fn active_workspace(&self, display_id: u32) -> WorkspaceId {
        self.inner
            .force_read()
            .displays
            .get(&display_id)
            .map(|display| display.active_workspace)
            .expect("finding active workspace")
    }

    pub(crate) fn native_space_focuses(&self) -> Vec<WorkspaceId> {
        self.inner.force_read().native_space_focuses.clone()
    }

    pub(crate) fn native_space_focus_completions(&self) -> Vec<WorkspaceId> {
        self.inner
            .force_read()
            .native_space_focus_completions
            .clone()
    }

    // --- Native Request Controls ---

    pub(crate) fn set_native_move_outcome(&self, outcome: NativeRequestOutcome) {
        self.inner.force_write().native_move_outcome = outcome;
    }

    pub(crate) fn set_native_activation_outcome(&self, outcome: NativeRequestOutcome) {
        self.inner.force_write().native_activation_outcome = outcome;
    }

    /// Whether the window server announces an applied activation with
    /// `SpaceChanged`. Off, a confirmed switch is only visible to whoever
    /// asks the window server.
    pub(crate) fn set_native_activation_notifies(&self, notifies: bool) {
        self.inner.force_write().activation_notifies = notifies;
    }

    /// Spawns `child` attached to `parent` the way a sheet or an attached
    /// panel is: owned by the parent's app, on the parent's Space, and
    /// reported by the window server as one of the parent's associated
    /// windows. The association is reported as recorded even after the child
    /// closes; it is the caller's job to check whether the child still exists.
    pub(crate) fn attach_window(&self, parent: WinID, child: WinID) {
        let mut inner = self.inner.force_write();
        let parent_window = inner.windows.get(&parent).expect("finding parent window");
        let (pid, workspace_id, frame) = (
            parent_window.pid,
            parent_window.workspace_id,
            parent_window.frame,
        );
        inner.windows.insert(
            child,
            MockWindowData {
                id: child,
                pid,
                frame,
                title: format!("Sheet {child}"),
                workspace_id,
                ..default()
            },
        );
        inner
            .associated_windows
            .entry(parent)
            .or_default()
            .push(child);
    }

    /// Puts `window_id` on every Space at once. The window server reports
    /// all of them as its membership and refuses to move it, failing any
    /// batch that contains it before anything is submitted.
    pub(crate) fn set_window_sticky(&self, window_id: WinID, sticky: bool) {
        let mut inner = self.inner.force_write();
        if sticky {
            inner.sticky_windows.insert(window_id);
        } else {
            inner.sticky_windows.remove(&window_id);
        }
    }

    /// Makes every per-window query about `window_id` fail, whether or not
    /// the window exists. A failed query is never an answer about existence
    /// or membership.
    pub(crate) fn set_window_queries_failing(&self, window_id: WinID, failing: bool) {
        let mut inner = self.inner.force_write();
        if failing {
            inner.failing_window_queries.insert(window_id);
        } else {
            inner.failing_window_queries.remove(&window_id);
        }
    }

    /// Number of submitted requests the window server has not applied yet.
    pub(crate) fn pending_native_requests(&self) -> usize {
        self.inner.force_read().pending_native_requests.len()
    }

    /// Lets the window server carry out every in-flight request, in
    /// submission order, emitting the notifications a real apply would.
    pub(crate) fn settle_native_requests(&self) {
        let mut inner = self.inner.force_write();
        for request in std::mem::take(&mut inner.pending_native_requests) {
            inner.apply(request);
        }
    }

    /// Removes a Space from its display, as when the user closes a Desktop
    /// in Mission Control, reporting it the way macOS does: the Space change
    /// first when the current Space vanished, then the destruction. Windows
    /// keep their stale membership until an explicit move, which is what the
    /// window server reports for a Space that no longer exists.
    pub(crate) fn remove_workspace(&self, display_id: u32, workspace_id: WorkspaceId) {
        let mut inner = self.inner.force_write();
        let display = inner
            .displays
            .get_mut(&display_id)
            .expect("finding display");
        display.workspaces.retain(|id| *id != workspace_id);
        let fell_back = display.active_workspace == workspace_id;
        if fell_back {
            display.active_workspace = display.workspaces.first().copied().unwrap_or_default();
        }
        if fell_back {
            inner.event_queue.push_back(Event::SpaceChanged);
        }
        inner.event_queue.push_back(Event::SpaceDestroyed {
            space_id: workspace_id,
        });
    }

    // --- State Mutation Methods ---

    pub fn update_window<F>(&self, id: WinID, f: F)
    where
        F: FnOnce(&mut MockWindowData),
    {
        let mut inner = self.inner.force_write();
        if let Some(w) = inner.windows.get_mut(&id) {
            f(w);
        }
    }

    #[allow(unused)]
    pub fn update_app(&self, pid: Pid, f: impl FnOnce(&mut MockAppData)) {
        let mut inner = self.inner.force_write();
        if let Some(a) = inner.apps.get_mut(&pid) {
            f(a);
        }
    }

    // --- OS Behavior Methods ---

    #[allow(unused)]
    pub fn os_move_window(&self, id: WinID, origin: Origin) {
        let mut inner = self.inner.force_write();
        if let Some(w) = inner.windows.get_mut(&id) {
            let size = w.frame.size();
            w.frame.min = origin;
            w.frame.max = origin + size;
            inner
                .event_queue
                .push_back(Event::WindowMoved { window_id: id });
        }
    }

    #[allow(unused)]
    pub fn os_resize_window(&self, id: WinID, size: Size) {
        let mut inner = self.inner.force_write();
        if let Some(w) = inner.windows.get_mut(&id) {
            w.frame.max = w.frame.min + size;
            inner
                .event_queue
                .push_back(Event::WindowResized { window_id: id });
        }
    }

    #[allow(unused)]
    pub fn os_minimize_window(&self, id: WinID, minimized: bool) {
        let mut inner = self.inner.force_write();
        if let Some(w) = inner.windows.get_mut(&id) {
            w.minimized = minimized;
            if minimized {
                inner
                    .event_queue
                    .push_back(Event::WindowMinimized { window_id: id });
            } else {
                inner
                    .event_queue
                    .push_back(Event::WindowDeminimized { window_id: id });
            }
        }
    }

    /// Closes a window while its application keeps running, the way macOS
    /// actually reports it: the SLS space notification first, then the AX
    /// element teardown, with the AX element and the app's window list still
    /// reporting the window for a while after, as they do on real apps.
    #[allow(unused)]
    pub fn os_close_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        let Some(window) = inner.windows.remove(&id) else {
            return;
        };
        inner.stale_window_ids.insert(id, window.pid);
        inner.event_queue.push_back(Event::WindowDestroyed {
            window_id: id,
            source: DestroySource::SpaceNotification,
        });
        inner.event_queue.push_back(Event::WindowDestroyed {
            window_id: id,
            source: DestroySource::Accessibility,
        });
    }

    /// Makes a window disappear with no notification at all, modelling a
    /// destroy event that never arrived — a lost notification, or a window
    /// closed while paneru was not running.
    #[allow(unused)]
    pub fn os_vanish_window(&self, id: WinID) {
        let mut inner = self.inner.force_write();
        inner.windows.remove(&id);
        inner.unordered_windows.insert(id);
    }

    /// Lets the app's window list catch up with reality after a close.
    #[allow(unused)]
    pub fn os_settle_window_list(&self) {
        self.inner.force_write().stale_window_ids.clear();
    }

    // --- Interaction Helpers ---

    #[allow(unused)]
    pub fn simulate_click(&self, point: Origin) {
        let mut inner = self.inner.force_write();
        let point = CGPoint::new(point.x.into(), point.y.into());
        inner.event_queue.push_back(Event::MouseDown {
            point,
            modifiers: Modifiers::empty(),
        });
        inner.event_queue.push_back(Event::MouseUp {
            point,
            modifiers: Modifiers::empty(),
        });
    }

    #[allow(unused)]
    pub fn simulate_window_click(&self, id: WinID) {
        let inner = self.inner.force_read();
        if let Some(w) = inner.windows.get(&id) {
            let center = w.frame.center();
            drop(inner);
            self.simulate_click(center);
        }
    }

    #[allow(unused)]
    pub fn simulate_drag(&self, start: Origin, end: Origin) {
        let mut inner = self.inner.force_write();
        let start_p = CGPoint::new(start.x.into(), start.y.into());
        let end_p = CGPoint::new(end.x.into(), end.y.into());
        inner.event_queue.push_back(Event::MouseDown {
            point: start_p,
            modifiers: Modifiers::empty(),
        });
        inner.event_queue.push_back(Event::MouseDragged {
            point: end_p,
            modifiers: Modifiers::empty(),
        });
        inner.event_queue.push_back(Event::MouseUp {
            point: end_p,
            modifiers: Modifiers::empty(),
        });
    }

    pub fn cursor_position(&self) -> IVec2 {
        self.inner.force_read().cursor_position
    }

    pub(crate) fn window_workspace(&self, window_id: WinID) -> WorkspaceId {
        self.inner
            .force_read()
            .windows
            .get(&window_id)
            .expect("finding window")
            .workspace_id
    }

    pub(crate) fn workspace_moves(&self) -> Vec<(Vec<WinID>, WorkspaceId)> {
        self.inner.force_read().workspace_moves.clone()
    }

    // --- Mock Factory Methods ---

    #[allow(clippy::too_many_lines)]
    pub fn create_window(&self, id: WinID) -> Window {
        let mut mw = MockWindowApi::new();

        mw.expect_id().return_const(id);

        let s = self.clone();
        mw.expect_pid().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.pid)
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        mw.expect_frame().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.frame)
                .unwrap_or_default()
        });

        let s = self.clone();
        mw.expect_resize().returning(move |size| {
            let mut inner = s.inner.force_write();
            if let Some(w) = inner.windows.get_mut(&id) {
                w.frame.max = w.frame.min + size;
            }
        });

        let s_move = self.clone();
        mw.expect_reposition().returning(move |origin| {
            let mut inner = s_move.inner.force_write();
            if let Some(w) = inner.windows.get_mut(&id) {
                let size = w.frame.size();
                w.frame.min = origin;
                w.frame.max = origin + size;
            }
        });

        let s = self.clone();
        mw.expect_focus_with_raise().returning(move |_psn| {
            s.focus_window(id);
        });

        let s = self.clone();
        mw.expect_title().returning(move || {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.title.clone())
                .unwrap_or_default())
        });
        // The mock reads its title from shared state every time, so there's
        // nothing to invalidate — but the call still needs an expectation.
        mw.expect_invalidate_title().return_const(());

        let s = self.clone();
        mw.expect_is_minimized().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|w| w.minimized)
        });

        let s = self.clone();
        mw.expect_update_frame().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.frame)
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        mw.expect_identifier().returning(move || {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.identifier.clone())
                .unwrap_or_default())
        });

        let s = self.clone();
        mw.expect_role().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.role.clone())
                .ok_or_else(|| crate::errors::Error::Generic(format!("window {id} not found")))
        });

        let s = self.clone();
        mw.expect_subrole().returning(move || {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.subrole.clone())
                .unwrap_or_default())
        });

        let s = self.clone();
        mw.expect_child_role().returning(move || {
            Ok(s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|w| w.child_role))
        });

        let s = self.clone();
        mw.expect_horizontal_padding().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.horizontal_padding)
                .unwrap_or_default()
        });

        let s = self.clone();
        mw.expect_vertical_padding().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .map(|w| w.vertical_padding)
                .unwrap_or_default()
        });

        let s = self.clone();
        mw.expect_is_full_screen().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .is_some_and(|w| w.is_full_screen)
        });

        let s = self.clone();
        mw.expect_border_radius().returning(move || {
            s.inner
                .force_read()
                .windows
                .get(&id)
                .and_then(|w| w.border_radius)
        });

        // Fill in remaining defaults
        mw.expect_element().return_const(None);
        mw.expect_raise_without_focus().return_const(());

        // Focusing without raising still moves the OS focus, it just leaves the
        // window order alone. Only the app's idea of its focused window changes
        // here: the notification that follows in the real thing is what
        // `focus_window` models, and callers of this already act on their own
        // request. Dropping even this much left the app reporting the previous
        // window, which made `window_focused_trigger` discard events for the
        // window that had actually been focused.
        let s = self.clone();
        mw.expect_focus_without_raise()
            .returning(move |_psn, _focused, _focused_psn| {
                s.set_focused_window(id);
            });
        mw.expect_set_padding().return_const(());

        Window::new(Box::new(mw))
    }

    pub fn create_application(&self, pid: Pid) -> Application {
        let mut ma = MockApplicationApi::new();
        let s = self.clone();

        ma.expect_pid().return_const(pid);
        ma.expect_psn()
            .returning(move || s.inner.force_read().apps.get(&pid).map(|a| a.psn).unwrap());

        let s = self.clone();
        ma.expect_focused_window_id().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .and_then(|a| a.focused_window_id)
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        ma.expect_focused_window().returning(move |_config| {
            let inner = s.inner.force_read();
            let focused_id = inner.apps.get(&pid).and_then(|a| a.focused_window_id)?;
            if inner.windows.contains_key(&focused_id) {
                Some(s.create_window(focused_id))
            } else {
                None
            }
        });

        let s = self.clone();
        ma.expect_bundle_id().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .map(|a| a.bundle_id.clone())
        });

        let name = self
            .inner
            .force_read()
            .apps
            .get(&pid)
            .map(|a| a.name.clone())
            .unwrap();
        ma.expect_name().return_const(name);

        let s = self.clone();
        ma.expect_is_frontmost().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .is_some_and(|a| a.is_frontmost)
        });

        let s = self.clone();
        ma.expect_connection().returning(move || {
            s.inner
                .force_read()
                .apps
                .get(&pid)
                .and_then(|a| a.connection)
        });

        ma.expect_observe().returning(|| Ok(true));
        ma.expect_observe_window().returning(|_| Ok(true));
        ma.expect_unobserve_window().return_const(());
        let s = self.clone();
        let window_ids = move || {
            let inner = s.inner.force_read();
            inner
                .windows
                .values()
                .filter(|w| w.pid == pid)
                .map(|w| w.id)
                .chain(
                    inner
                        .stale_window_ids
                        .iter()
                        .filter(|&(_, &owner)| owner == pid)
                        .map(|(&id, _)| id),
                )
                .filter(|&id| !inner.is_associated_child(id))
                .collect::<Vec<_>>()
        };

        let (s, ids) = (self.clone(), window_ids.clone());
        ma.expect_window_list()
            .returning(move |_| ids().into_iter().map(|id| s.create_window(id)).collect());

        Application::new(Box::new(ma))
    }

    #[allow(clippy::too_many_lines)]
    pub fn create_window_manager(&self) -> MockWindowManagerApi {
        let mut wm = MockWindowManagerApi::new();

        let s = self.clone();
        wm.expect_window_is_unordered().returning(move |window_id| {
            let inner = s.inner.force_read();
            inner.unordered_windows.contains(&window_id) || !inner.windows.contains_key(&window_id)
        });

        let s = self.clone();
        wm.expect_active_display_id()
            .returning(move || Ok(s.inner.force_read().active_display_id));

        let s = self.clone();
        wm.expect_active_display_space().returning(move |id| {
            s.inner
                .force_read()
                .displays
                .get(&id)
                .map(|display| display.active_workspace)
                .ok_or(Error::InvalidWindow)
        });

        let s = self.clone();
        wm.expect_is_fullscreen_space()
            .returning(move |display_id| {
                let inner = s.inner.force_read();
                inner.displays.get(&display_id).is_some_and(|display| {
                    inner.fullscreen_spaces.contains(&display.active_workspace)
                })
            });

        let s = self.clone();
        wm.expect_native_spaces().returning(move || {
            let inner = s.inner.force_read();
            let mut displays = inner.displays.values().collect::<Vec<_>>();
            displays.sort_unstable_by_key(|display| display.id);
            Ok(displays
                .into_iter()
                .flat_map(|display| display.workspaces.iter().copied())
                .collect())
        });

        let s = self.clone();
        wm.expect_focus_native_space()
            .returning(move |workspace_id| {
                let mut inner = s.inner.force_write();
                let display_id = inner.owning_display(workspace_id).ok_or_else(|| {
                    Error::NotFound(format!("no display owns native Space {workspace_id}"))
                })?;
                let current = inner.displays[&display_id].active_workspace == workspace_id;
                if current && inner.active_display_id == display_id {
                    return Ok(false);
                }
                inner.native_space_focuses.push(workspace_id);
                if !current {
                    let outcome = inner.native_activation_outcome;
                    inner.submit(
                        outcome,
                        workspace_id,
                        PendingNativeRequest::Activate { workspace_id },
                    )?;
                }
                Ok(true)
            });

        let s = self.clone();
        wm.expect_focus_window_workspace()
            .returning(move |window_id, _psn| {
                let mut inner = s.inner.force_write();
                let target_workspace = inner
                    .windows
                    .get(&window_id)
                    .map(|window| window.workspace_id)
                    .ok_or(Error::InvalidWindow)?;
                inner
                    .owning_display(target_workspace)
                    .ok_or(Error::InvalidWindow)?;
                if inner
                    .displays
                    .values()
                    .any(|display| display.active_workspace == target_workspace)
                {
                    return Ok(None);
                }
                inner.workspace_focuses.push(window_id);
                let outcome = inner.native_activation_outcome;
                inner.submit(
                    outcome,
                    target_workspace,
                    PendingNativeRequest::Activate {
                        workspace_id: target_workspace,
                    },
                )?;
                Ok(Some(target_workspace))
            });

        let s = self.clone();
        wm.expect_window_workspaces()
            .returning(move |window_id| s.inner.force_read().window_memberships(window_id));

        let s = self.clone();
        wm.expect_window_exists().returning(move |window_id| {
            let inner = s.inner.force_read();
            inner.check_window_query(window_id)?;
            Ok(inner.windows.contains_key(&window_id))
        });

        let s = self.clone();
        wm.expect_native_space_is_active()
            .returning(move |workspace_id| {
                let inner = s.inner.force_read();
                let display_id = inner.owning_display(workspace_id).ok_or_else(|| {
                    Error::NotFound(format!("no display owns native Space {workspace_id}"))
                })?;
                Ok(inner.displays[&display_id].active_workspace == workspace_id)
            });

        let s = self.clone();
        wm.expect_complete_native_space_focus()
            .returning(move |workspace_id| {
                let mut inner = s.inner.force_write();
                let display_id = inner.owning_display(workspace_id).ok_or_else(|| {
                    Error::NotFound(format!("no display owns native Space {workspace_id}"))
                })?;
                let (current, bounds) = {
                    let display = &inner.displays[&display_id];
                    (display.active_workspace, display.bounds)
                };
                if current != workspace_id {
                    return Err(Error::Generic(format!(
                        "native Space {workspace_id} is not current on display {display_id} yet"
                    )));
                }
                if !bounds.contains(inner.cursor_position) {
                    inner.cursor_position = bounds.center();
                }
                inner.active_display_id = display_id;
                inner.native_space_focus_completions.push(workspace_id);
                Ok(())
            });

        let s = self.clone();
        wm.expect_present_displays().returning(move || {
            s.inner
                .force_read()
                .displays
                .values()
                .map(|d| {
                    (
                        Display::new(d.id, d.bounds, TEST_MENUBAR_HEIGHT),
                        d.workspaces.clone(),
                    )
                })
                .collect()
        });

        let s = self.clone();
        wm.expect_find_existing_application_windows()
            .returning(move |app, spaces, _config| {
                let pid = app.pid();
                let inner = s.inner.force_read();
                let mut windows = inner
                    .windows
                    .values()
                    .filter_map(|w| {
                        (w.pid == pid
                            && spaces.contains(&w.workspace_id)
                            && !inner.is_associated_child(w.id))
                        .then_some(s.create_window(w.id))
                    })
                    .collect::<Vec<_>>();
                windows.sort_unstable_by_key(|window| window.id());
                Ok((windows, vec![]))
            });

        let s = self.clone();
        wm.expect_windows_in_workspace()
            .returning(move |workspace_id| {
                let inner = s.inner.force_read();
                let mut windows = inner
                    .windows
                    .values()
                    .filter_map(|w| {
                        (w.workspace_id == workspace_id && !inner.is_associated_child(w.id))
                            .then_some(w.id)
                    })
                    .collect::<Vec<_>>();
                // Sort the windows to keep the tests consistent
                windows.sort_unstable();
                Ok(windows)
            });

        let s = self.clone();
        wm.expect_move_windows_to_workspace()
            .returning(move |window_ids, workspace_id| {
                let mut inner = s.inner.force_write();
                let outcome = inner.native_move_outcome;
                inner
                    .workspace_moves
                    .push((window_ids.to_vec(), workspace_id));
                inner.check_move_batch(window_ids)?;
                inner.submit(
                    outcome,
                    workspace_id,
                    PendingNativeRequest::Move {
                        windows: window_ids.to_vec(),
                        workspace_id,
                    },
                )
            });

        let s = self.clone();
        wm.expect_windows_on_screen().returning(move || {
            let windows = s
                .inner
                .force_read()
                .windows
                .iter()
                .filter_map(|(id, window)| window.visible.then_some(id))
                .copied()
                .collect::<Vec<_>>();
            Some(windows)
        });

        let s = self.clone();
        wm.expect_warp_mouse()
            .returning(move |origin| s.inner.force_write().cursor_position = origin);

        let s = self.clone();
        wm.expect_cursor_position()
            .returning(move || Some(origin_to(s.inner.force_read().cursor_position)));

        let s = self.clone();
        wm.expect_get_associated_windows()
            .returning(move |window_id| {
                s.inner
                    .force_read()
                    .associated_windows
                    .get(&window_id)
                    .cloned()
                    .unwrap_or_default()
            });

        let s = self.clone();
        wm.expect_find_window_at_point().returning(move |at_point| {
            let point = origin_from(*at_point);
            s.inner
                .force_read()
                .windows
                .iter()
                .find_map(|(id, window)| window.frame.contains(point).then_some(id))
                .ok_or(Error::NotFound(format!("no window found at point {point}")))
                .copied()
        });

        wm
    }

    pub fn create_process(&self, pid: Pid) -> MockProcessApi {
        let mut mp = MockProcessApi::new();
        let s = self.clone();

        let name = self
            .inner
            .force_read()
            .apps
            .get(&pid)
            .map(|a| a.name.clone())
            .unwrap();
        mp.expect_name().return_const(name);

        mp.expect_pid().return_const(pid);
        mp.expect_psn()
            .returning(move || s.inner.force_read().apps.get(&pid).map(|a| a.psn).unwrap());
        mp.expect_is_observable().returning(|| true);
        mp.expect_application().return_const(None);
        mp.expect_ready().return_const(true);
        mp.expect_force_manage().return_const(());

        mp
    }
}

use accessibility_sys::{
    AXError, AXObserverRef, AXUIElementCreateApplication, AXUIElementGetTypeID, AXUIElementRef,
    kAXErrorAttributeUnsupported, kAXErrorNoValue, kAXErrorNotificationAlreadyRegistered,
    kAXErrorNotificationNotRegistered, kAXErrorSuccess,
};
use objc2::MainThreadMarker;
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSRunningApplication;
use objc2_core_foundation::{
    CFArray, CFDictionary, CFGetTypeID, CFNumber, CFRetained, CFString, CFType,
    kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowLayer,
    kCGWindowOwnerPID,
};
use objc2_foundation::NSString;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use super::{
    AXObserverAddNotification, AXObserverCreate, AXObserverRemoveNotification, CFStringRef, Pid,
};
use crate::errors::{Error, Result};
use crate::events::{Event, EventSender};
use crate::manager::AXUIElementCopyAttributeValue;
use crate::util::{AXUIWrapper, add_run_loop, remove_run_loop};

const ACTIVE_CHECK: Duration = Duration::from_millis(100);
const UNKNOWN_CHECK: Duration = Duration::from_millis(500);
const SUBSCRIPTION_RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Dock,
    WindowManager,
}

impl Owner {
    const ALL: [Self; 2] = [Self::Dock, Self::WindowManager];

    fn bundle(self) -> &'static str {
        match self {
            Self::Dock => "com.apple.dock",
            Self::WindowManager => "com.apple.WindowManager",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Dock => 0,
            Self::WindowManager => 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct LegacyActivity {
    pid: Pid,
    /// Once this active cycle has appeared in fresh AX/CG owner data, its
    /// disappearance can end the cycle even if the exit notification is lost.
    appeared_in_tree: bool,
}

thread_local! {
    // Main-thread-only positive evidence, never an inactive authorization.
    static LEGACY_ACTIVE: Cell<[Option<LegacyActivity>; 2]> = const { Cell::new([None; 2]) };
    static WINDOW_MANAGER_SEEN: Cell<bool> = const { Cell::new(false) };
}

fn owner_pid(owner: Owner) -> Result<Option<Pid>> {
    let applications = NSRunningApplication::runningApplicationsWithBundleIdentifier(
        &NSString::from_str(owner.bundle()),
    );
    let mut applications = applications.iter();
    let pid = applications
        .next()
        .map(|application| application.processIdentifier());
    if applications.next().is_some() || pid.is_some_and(|pid| pid <= 0) {
        return Err(Error::Generic(format!(
            "ambiguous Mission Control owner {}",
            owner.bundle()
        )));
    }
    Ok(pid)
}

fn owner_pids() -> Result<[Option<Pid>; 2]> {
    let dock = owner_pid(Owner::Dock)?;
    let window_manager = owner_pid(Owner::WindowManager)?;
    if dock.is_none() {
        return Err(Error::NotFound(
            "Mission Control Dock owner is unavailable".to_string(),
        ));
    }
    WINDOW_MANAGER_SEEN.with(|seen| {
        if window_manager.is_some() {
            seen.set(true);
        } else if seen.get() {
            return Err(Error::NotFound(
                "Mission Control WindowManager owner is restarting".to_string(),
            ));
        }
        Ok([dock, window_manager])
    })
}

/// Only optional identity metadata may be absent. In particular, `NoValue` for
/// `AXChildren` is NOT an empty tree: the observed closed `WindowManager` returns
/// success with an actual empty array.
fn attribute_is_absent(status: AXError, optional: bool) -> Result<bool> {
    if status == kAXErrorSuccess {
        Ok(false)
    } else if optional && (status == kAXErrorNoValue || status == kAXErrorAttributeUnsupported) {
        Ok(true)
    } else {
        Err(Error::Generic(format!(
            "Mission Control AX query failed ({status})"
        )))
    }
}

fn attribute(
    element: AXUIElementRef,
    name: &'static str,
    optional: bool,
) -> Result<Option<CFRetained<CFType>>> {
    let mut value = null_mut();
    let status = unsafe {
        AXUIElementCopyAttributeValue(element, &CFString::from_static_str(name), &mut value)
    };
    // Balance even a surprising nonnull result accompanying an AX error.
    let value = NonNull::new(value).map(|value| unsafe { CFRetained::from_raw(value) });
    if attribute_is_absent(status, optional)? {
        return Ok(None);
    }
    value
        .map(Some)
        .ok_or_else(|| Error::Generic(format!("Mission Control {name} returned null")))
}

fn string(value: &CFType) -> Result<&CFString> {
    value.downcast_ref::<CFString>().ok_or_else(|| {
        Error::Generic("Mission Control AX string has an unexpected type".to_string())
    })
}

fn equals(value: &CFString, expected: &'static str) -> bool {
    value == &*CFString::from_static_str(expected)
}

/// The complete negative shapes seen at runtime are an empty `WindowManager`
/// application and the Dock's ordinary `AXList`. Other UI is unknown, not an
/// invented inactive state. Legacy owners can still positively identify the
/// same Mission Control groups or deliver their original `AXExpose` events.
fn classify_child(owner: Owner, role: &CFString, identifier: Option<&CFString>) -> Result<bool> {
    if identifier.is_some_and(|id| {
        equals(id, "mc.display") || equals(id, "mc.spaces") || equals(id, "appexpose.display")
    }) {
        return if equals(role, "AXGroup") {
            Ok(true)
        } else {
            Err(Error::Generic(
                "Mission Control identifier has a non-group role".to_string(),
            ))
        };
    }
    if owner == Owner::Dock && equals(role, "AXList") {
        return Ok(false);
    }
    Err(Error::Generic(format!(
        "unrecognized Mission Control UI for {}",
        owner.bundle()
    )))
}

fn observe_owner(owner: Owner, pid: Pid) -> Result<bool> {
    let element = AXUIWrapper::from_retained(unsafe { AXUIElementCreateApplication(pid) })?;
    let role = attribute(element.as_ptr(), "AXRole", false)?
        .ok_or_else(|| Error::Generic("Mission Control application role is missing".to_string()))?;
    if !equals(string(&role)?, "AXApplication") {
        return Err(Error::Generic(
            "Mission Control owner is not an AXApplication".to_string(),
        ));
    }
    let children = attribute(element.as_ptr(), "AXChildren", false)?.ok_or_else(|| {
        Error::Generic("Mission Control application children are missing".to_string())
    })?;
    let children = children
        .downcast_ref::<CFArray>()
        .ok_or_else(|| Error::Generic("Mission Control AXChildren is not an array".to_string()))?;
    // AX arrays contain CF objects, but every child must separately prove it
    // is an AXUIElement before being passed to an AX function.
    let children = unsafe { children.cast_unchecked::<CFType>() };
    if owner == Owner::Dock && children.is_empty() {
        return Err(Error::Generic(
            "Mission Control Dock tree is empty".to_string(),
        ));
    }
    let mut result = Ok(false);
    for child in children.iter() {
        let observed = (|| {
            if CFGetTypeID(Some(&child)) != unsafe { AXUIElementGetTypeID() } {
                return Err(Error::Generic(
                    "Mission Control AX child is not an element".to_string(),
                ));
            }
            let child = NonNull::from(&*child).cast().as_ptr();
            let role = attribute(child, "AXRole", false)?.ok_or_else(|| {
                Error::Generic("Mission Control child role is missing".to_string())
            })?;
            let identifier = attribute(child, "AXIdentifier", true)?;
            classify_child(
                owner,
                string(&role)?,
                identifier.as_deref().map(string).transpose()?,
            )
        })();
        match observed {
            Ok(true) => return Ok(true),
            Err(error) => result = Err(error),
            Ok(false) => {}
        }
    }
    result
}

fn combine_observations(first: Result<bool>, second: Result<bool>) -> Result<bool> {
    match (first, second) {
        (Ok(true), _) | (_, Ok(true)) => Ok(true),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(false), Ok(false)) => Ok(false),
    }
}

/// Legacy Dock Mission Control and `WindowManager` Show Desktop use an
/// on-screen layer-18 surface. Match the current owner PID, not localized
/// names or titles that screen-recording permissions may omit.
fn expose_surface(value: &CFType, owner_pid: Pid) -> Result<bool> {
    let entry = value.downcast_ref::<CFDictionary>().ok_or_else(|| {
        Error::Generic("Mission Control window census entry is not a dictionary".to_string())
    })?;
    // Every CoreGraphics window-info key and value is a CF object.
    let entry = unsafe { entry.cast_unchecked::<CFType, CFType>() };
    let owner = entry
        .get(unsafe { kCGWindowOwnerPID })
        .and_then(|value| value.downcast_ref::<CFNumber>().and_then(CFNumber::as_i64))
        .ok_or_else(|| {
            Error::Generic("Mission Control window owner PID is missing or malformed".to_string())
        })?;
    if owner != i64::from(owner_pid) {
        return Ok(false);
    }
    let layer = entry
        .get(unsafe { kCGWindowLayer })
        .and_then(|value| value.downcast_ref::<CFNumber>().and_then(CFNumber::as_i64))
        .ok_or_else(|| {
            Error::Generic("Mission Control surface layer is missing or malformed".to_string())
        })?;
    Ok(layer == 18)
}

fn read_expose_surfaces() -> Result<CFRetained<CFArray<CFType>>> {
    let windows =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, kCGNullWindowID)
            .ok_or_else(|| {
                Error::Generic("Mission Control window census is unavailable".to_string())
            })?;
    // Every CoreGraphics array entry is a CF object, validated when consumed.
    Ok(unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(windows) })
}

fn observe_expose_surfaces(windows: &CFArray<CFType>, owner_pid: Pid) -> Result<bool> {
    let mut observation = Ok(false);
    for window in windows.iter() {
        observation = combine_observations(observation, expose_surface(&window, owner_pid));
        if matches!(observation, Ok(true)) {
            return observation;
        }
    }
    observation
}

fn apply_legacy_observation(
    state: &mut Option<LegacyActivity>,
    pid: Pid,
    observation: Result<bool>,
) -> Result<bool> {
    if let Some(legacy) = state.as_mut() {
        if legacy.pid != pid {
            *state = None;
        } else if matches!(observation, Ok(true)) {
            legacy.appeared_in_tree = true;
        } else if matches!(observation, Ok(false)) && legacy.appeared_in_tree {
            *state = None;
        }
    }
    if state.is_some() {
        Ok(true)
    } else {
        observation
    }
}

fn apply_legacy_activity(owner: Owner, pid: Pid, observation: Result<bool>) -> Result<bool> {
    LEGACY_ACTIVE.with(|active| {
        let mut states = active.get();
        let observation = apply_legacy_observation(&mut states[owner.index()], pid, observation);
        active.set(states);
        observation
    })
}

fn observe_current_owners(pids: [Option<Pid>; 2]) -> Result<bool> {
    let mut observation = Ok(false);
    let mut surfaces = None;
    for owner in Owner::ALL {
        if let Some(pid) = pids[owner.index()] {
            let mut owner_state = observe_owner(owner, pid);
            // Both owners have a verified layer-18 mode with no interactive
            // AX children. Share one fresh census across their observations.
            if !matches!(owner_state, Ok(true)) {
                let surface_state = surfaces
                    .get_or_insert_with(read_expose_surfaces)
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|windows| observe_expose_surfaces(windows, pid));
                owner_state = combine_observations(owner_state, surface_state);
            }
            observation =
                combine_observations(observation, apply_legacy_activity(owner, pid, owner_state));
        }
    }
    // A restart between process discovery and the read cannot authorize a
    // write with the previous owner's tree.
    if owner_pids()? != pids {
        return Err(Error::Generic(
            "Mission Control owner changed during observation".to_string(),
        ));
    }
    observation
}

/// Fresh main-thread authorization for native mutations. Query failures,
/// missing owners, nulls and malformed/unsupported required AX data are errors;
/// only an observed inactive tree returns false. No ECS cache is consulted.
pub fn mission_control_is_active() -> Result<bool> {
    if MainThreadMarker::new().is_none() {
        return Err(Error::Generic(
            "Mission Control observation must run on the process main thread".to_string(),
        ));
    }
    autoreleasepool(|_| observe_current_owners(owner_pids()?))
}

#[derive(Debug)]
struct Subscription {
    owner: Owner,
    pid: Pid,
    element: CFRetained<AXUIWrapper>,
    observer: CFRetained<AXUIWrapper>,
    registered: Vec<&'static str>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        for name in &self.registered {
            let status = unsafe {
                AXObserverRemoveNotification(
                    self.observer.as_ptr(),
                    self.element.as_ptr(),
                    &CFString::from_static_str(name),
                )
            };
            if status != kAXErrorSuccess && status != kAXErrorNotificationNotRegistered {
                debug!(status, name, "removing Mission Control subscription");
            }
        }
        remove_run_loop(&self.observer);
    }
}

/// `AXExpose` remains useful on older owners, but a successful subscription is
/// not state evidence. Fresh AX observations drive the cache even on owners
/// whose subscription returns success and never delivers a callback.
#[derive(Debug)]
pub(super) struct MissionControlHandler {
    events: EventSender,
    subscriptions: Vec<Subscription>,
    bound_pids: Option<[Option<Pid>; 2]>,
    last_subscription: Option<Instant>,
    last_observation: Option<Instant>,
    status: Option<bool>,
    pending: RefCell<Option<Event>>,
    enabled: bool,
}

fn transition_event(previous: Option<bool>, next: Option<bool>) -> Option<Event> {
    if previous == next {
        return None;
    }
    Some(match next {
        Some(true) => Event::MissionControlShowAllWindows,
        Some(false) => Event::MissionControlExit,
        None => Event::MissionControlStateUnknown,
    })
}

fn observation_interval(status: Option<bool>) -> Option<Duration> {
    match status {
        Some(true) => Some(ACTIVE_CHECK),
        None => Some(UNKNOWN_CHECK),
        Some(false) => None,
    }
}

impl MissionControlHandler {
    pub(super) fn new(events: EventSender) -> Self {
        Self {
            events,
            subscriptions: Vec::new(),
            bound_pids: None,
            last_subscription: None,
            last_observation: None,
            status: None,
            pending: RefCell::new(None),
            enabled: false,
        }
    }

    const EVENTS: [&'static str; 7] = [
        "AXExposeShowAllWindows",
        "AXExposeShowFrontWindows",
        "AXExposeShowDesktop",
        "AXExposeExit",
        "AXChildrenChanged",
        "AXLayoutChanged",
        "AXFocusedUIElementChanged",
    ];

    /// Subscription failure is nonfatal: fresh reads remain the authority.
    /// No observer retains a callback context unless setup completed, and Drop
    /// removes only notifications that were actually registered.
    pub(super) fn observe(&mut self) {
        self.enabled = true;
        if let Ok(pids) = owner_pids() {
            self.rebind(pids, Instant::now());
        }
    }

    fn subscribe(&self, owner: Owner, pid: Pid) -> Result<Subscription> {
        let element = AXUIWrapper::from_retained(unsafe { AXUIElementCreateApplication(pid) })?;
        let mut observer = null_mut();
        let status = unsafe { AXObserverCreate(pid, Self::callback, &mut observer) };
        if status != kAXErrorSuccess {
            // AXObserverCreate owns nothing on failure.
            return Err(Error::Generic(format!(
                "could not create Mission Control observer for {pid}: {status}"
            )));
        }
        let observer = AXUIWrapper::from_retained(observer)?;
        let mut subscription = Subscription {
            owner,
            pid,
            element,
            observer,
            registered: Vec::new(),
        };
        for name in Self::EVENTS {
            let status = unsafe {
                AXObserverAddNotification(
                    subscription.observer.as_ptr(),
                    subscription.element.as_ptr(),
                    &CFString::from_static_str(name),
                    std::ptr::from_ref(self).cast_mut().cast(),
                )
            };
            if status == kAXErrorSuccess || status == kAXErrorNotificationAlreadyRegistered {
                subscription.registered.push(name);
            } else {
                debug!(
                    pid,
                    name,
                    status,
                    "Mission Control notification unavailable; using fresh AX observations"
                );
            }
        }
        if subscription.registered.is_empty() {
            return Err(Error::Generic(format!(
                "Mission Control owner {pid} supports no observed notifications"
            )));
        }
        unsafe { add_run_loop(&subscription.observer, kCFRunLoopDefaultMode)? };
        Ok(subscription)
    }

    fn rebind(&mut self, pids: [Option<Pid>; 2], now: Instant) {
        let changed = self.bound_pids != Some(pids);
        if changed {
            self.subscriptions
                .retain(|subscription| pids[subscription.owner.index()] == Some(subscription.pid));
            self.bound_pids = Some(pids);
            self.pending.borrow_mut().take();
        } else if self
            .last_subscription
            .is_some_and(|last| now.duration_since(last) < SUBSCRIPTION_RETRY)
        {
            return;
        }
        self.last_subscription = Some(now);
        for owner in Owner::ALL {
            let Some(pid) = pids[owner.index()] else {
                continue;
            };
            if self
                .subscriptions
                .iter()
                .any(|subscription| subscription.owner == owner)
            {
                continue;
            }
            match self.subscribe(owner, pid) {
                Ok(subscription) => self.subscriptions.push(subscription),
                Err(error) => debug!(%error, "Mission Control subscription not established"),
            }
        }
    }

    pub(super) fn refresh(&mut self, activity: bool) -> Option<Event> {
        if !self.enabled {
            return None;
        }
        let now = Instant::now();
        let pending = self.pending.borrow_mut().take();
        let due = self.last_observation.is_none_or(|last| {
            observation_interval(self.status)
                .is_some_and(|interval| now.duration_since(last) >= interval)
        });
        if !activity && pending.is_none() && !due {
            return None;
        }
        let observation = autoreleasepool(|_| {
            let pids = owner_pids()?;
            self.rebind(pids, now);
            observe_current_owners(pids)
        });
        let next = match observation {
            Ok(active) => Some(active),
            Err(error) => {
                if self.last_observation.is_none() || self.status.is_some() {
                    warn!(%error, "Mission Control state is unknown; native mutations suspended");
                }
                None
            }
        };
        let event = transition_event(self.status, next);
        self.status = next;
        self.last_observation = Some(Instant::now());
        if next == Some(true)
            && let Some(
                event @ (Event::MissionControlShowAllWindows
                | Event::MissionControlShowFrontWindows
                | Event::MissionControlShowDesktop),
            ) = pending
        {
            return Some(event);
        }
        event
    }

    /// Cap only active/unknown sleeps. Known-inactive idle has no MC polling;
    /// meaningful input, process events and animation work trigger refresh.
    pub(super) fn limit_wait(&self, timeout: f64) -> f64 {
        if !self.enabled || self.status == Some(false) {
            return timeout;
        }
        let remaining = self.last_observation.map_or(Duration::ZERO, |last| {
            observation_interval(self.status)
                .unwrap_or_default()
                .saturating_sub(last.elapsed())
        });
        timeout.min(remaining.as_secs_f64())
    }

    extern "C" fn callback(
        observer: AXObserverRef,
        _element: AXUIElementRef,
        notification: CFStringRef,
        context: *mut c_void,
    ) {
        let Some(this) = NonNull::new(context).map(|this| unsafe { this.cast::<Self>().as_ref() })
        else {
            return;
        };
        let Some(notification) = NonNull::new(notification.cast_mut()) else {
            return;
        };
        // The callback argument is declared CFStringRef by AXObserver's ABI.
        let notification = unsafe { notification.as_ref() };
        let Some(subscription) = this
            .subscriptions
            .iter()
            .find(|subscription| subscription.observer.as_ptr() == observer)
        else {
            return;
        };
        if owner_pid(subscription.owner).ok().flatten() != Some(subscription.pid) {
            this.pending
                .borrow_mut()
                .get_or_insert(Event::MissionControlStateUnknown);
            this.events.waker().wake();
            return;
        }
        let event = if equals(notification, "AXExposeShowAllWindows") {
            Some(Event::MissionControlShowAllWindows)
        } else if equals(notification, "AXExposeShowFrontWindows") {
            Some(Event::MissionControlShowFrontWindows)
        } else if equals(notification, "AXExposeShowDesktop") {
            Some(Event::MissionControlShowDesktop)
        } else if equals(notification, "AXExposeExit") {
            Some(Event::MissionControlExit)
        } else {
            None
        };
        if let Some(event) = event {
            LEGACY_ACTIVE.with(|active| {
                let mut states = active.get();
                states[subscription.owner.index()] = (!matches!(event, Event::MissionControlExit))
                    .then_some(LegacyActivity {
                        pid: subscription.pid,
                        appeared_in_tree: false,
                    });
                active.set(states);
            });
            *this.pending.borrow_mut() = Some(event);
        } else {
            // A wake without a channel event must still force an observation.
            this.pending
                .borrow_mut()
                .get_or_insert(Event::MissionControlStateUnknown);
        }
        // Never enqueue a raw Exit: a subsequent fresh read may fail, and an
        // old queued Exit must not overwrite the authoritative unknown event.
        this.events.waker().wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_groups_and_optional_dock_identifiers_are_classified_safely() {
        let group = CFString::from_static_str("AXGroup");
        let list = CFString::from_static_str("AXList");
        let display = CFString::from_static_str("mc.display");
        let spaces = CFString::from_static_str("mc.spaces");
        let app_expose = CFString::from_static_str("appexpose.display");
        assert!(classify_child(Owner::WindowManager, &group, Some(&app_expose)).unwrap());
        assert!(classify_child(Owner::WindowManager, &group, Some(&display)).unwrap());
        assert!(classify_child(Owner::Dock, &group, Some(&spaces)).unwrap());
        assert!(!classify_child(Owner::Dock, &list, None).unwrap());
        assert!(classify_child(Owner::WindowManager, &group, None).is_err());
        assert!(classify_child(Owner::Dock, &list, Some(&display)).is_err());
    }

    #[test]
    fn missing_optional_identifier_is_not_missing_required_children() {
        assert!(attribute_is_absent(kAXErrorNoValue, true).unwrap());
        assert!(attribute_is_absent(kAXErrorAttributeUnsupported, true).unwrap());
        assert!(attribute_is_absent(kAXErrorNoValue, false).is_err());
        assert!(attribute_is_absent(kAXErrorAttributeUnsupported, false).is_err());
        assert!(attribute_is_absent(accessibility_sys::kAXErrorCannotComplete, true).is_err());
    }

    #[test]
    fn one_active_owner_blocks_but_one_failed_owner_cannot_authorize_inactivity() {
        let failed = || Err(Error::Generic("owner restarting".to_string()));
        assert!(combine_observations(Ok(true), failed()).unwrap());
        assert!(combine_observations(failed(), Ok(false)).is_err());
        assert!(!combine_observations(Ok(false), Ok(false)).unwrap());
    }

    #[test]
    fn legacy_surface_requires_the_current_dock_pid_and_typed_layer() {
        let pid = CFNumber::new_i32(42);
        let layer = CFNumber::new_i32(18);
        let entry = CFDictionary::<CFString, CFType>::from_slices(
            &[unsafe { kCGWindowOwnerPID }, unsafe { kCGWindowLayer }],
            &[&*pid, &*layer],
        );
        assert!(expose_surface(&entry, 42).unwrap());
        assert!(!expose_surface(&entry, 43).unwrap());
        let missing_layer = CFDictionary::<CFString, CFType>::from_slices(
            &[unsafe { kCGWindowOwnerPID }],
            &[&*pid],
        );
        assert!(expose_surface(&missing_layer, 42).is_err());
        let malformed = CFString::from_static_str("18");
        let wrong_type = CFDictionary::<CFString, CFType>::from_slices(
            &[unsafe { kCGWindowOwnerPID }, unsafe { kCGWindowLayer }],
            &[&*pid, &*malformed],
        );
        assert!(expose_surface(&wrong_type, 42).is_err());
    }

    #[test]
    fn legacy_positive_evidence_survives_unknown_but_not_observed_exit_or_pid_change() {
        let mut state = Some(LegacyActivity {
            pid: 42,
            appeared_in_tree: false,
        });
        assert!(apply_legacy_observation(&mut state, 42, Ok(false)).unwrap());
        assert!(apply_legacy_observation(&mut state, 42, Ok(true)).unwrap());
        assert!(
            apply_legacy_observation(
                &mut state,
                42,
                Err(Error::Generic("unrecognized active layout".to_string())),
            )
            .unwrap()
        );
        assert!(!apply_legacy_observation(&mut state, 42, Ok(false)).unwrap());
        state = Some(LegacyActivity {
            pid: 42,
            appeared_in_tree: true,
        });
        assert!(
            apply_legacy_observation(
                &mut state,
                43,
                Err(Error::Generic("new owner is unavailable".to_string())),
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_recovery_and_idle_cadence_do_not_manufacture_exit() {
        assert!(matches!(
            transition_event(Some(false), None),
            Some(Event::MissionControlStateUnknown)
        ));
        assert!(transition_event(None, None).is_none());
        assert!(matches!(
            transition_event(None, Some(true)),
            Some(Event::MissionControlShowAllWindows)
        ));
        assert!(matches!(
            transition_event(Some(true), Some(false)),
            Some(Event::MissionControlExit)
        ));
        assert!(observation_interval(Some(false)).is_none());
        assert_eq!(observation_interval(Some(true)), Some(ACTIVE_CHECK));
        assert_eq!(observation_interval(None), Some(UNKNOWN_CHECK));
    }
}

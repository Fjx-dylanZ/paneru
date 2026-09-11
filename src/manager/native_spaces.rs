//! Native macOS Space operations through the private `SkyLight` `WMBridge`
//! operation objects (`SLSBridged…Operation`), dispatched with the no-colon
//! `performWithWMBridgeDelegate` selector.
//!
//! Ported from native-space-kit's `src/native_space_kit.m`
//! (<https://github.com/Fjx-dylanZ/native-space-kit>), whose behaviour was
//! verified on ordinary single-display desktops only. The port keeps that
//! project's notice:
//!
//! > MIT License
//! >
//! > Copyright (c) 2026 native-space-kit contributors
//! >
//! > Permission is hereby granted, free of charge, to any person obtaining a
//! > copy of this software and associated documentation files (the
//! > "Software"), to deal in the Software without restriction, including
//! > without limitation the rights to use, copy, modify, merge, publish,
//! > distribute, sublicense, and/or sell copies of the Software, and to
//! > permit persons to whom the Software is furnished to do so, subject to
//! > the following conditions:
//! >
//! > The above copyright notice and this permission notice shall be included
//! > in all copies or substantial portions of the Software.
//! >
//! > THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
//! > OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//! > MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
//! > IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
//! > CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
//! > TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
//! > SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//!
//! Guarantees kept from the original:
//!
//! * every private ABI is validated from the Objective-C method type
//!   encodings (class, initializer arguments, `void` perform) before a call;
//! * census reads fail closed on any structural surprise;
//! * mutations run on the process main thread in an unlocked, logged-in
//!   console session, prepare every operation object before the first write,
//!   and only *submit* asynchronous requests. Nothing here polls or blocks:
//!   confirmation belongs to the caller, which observes the census later;
//! * native exceptions are caught at this boundary and reported as errors:
//!   private class lookups (which may run `+resolveInstanceMethod:`),
//!   initializers and performs all run under it.

use std::ffi::CStr;
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;

use objc2::MainThreadMarker;
use objc2::exception::{Exception, catch};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CFType, CGPoint,
};
use objc2_core_graphics::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, CGSessionCopyCurrentDictionary,
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowLayer,
    kCGWindowNumber,
};
use tracing::trace;

use crate::errors::{Error, Result};
use crate::platform::{ConnID, WinID, WorkspaceId};
use crate::util::create_array;

use super::skylight::{SLSCopyManagedDisplaySpaces, SLSCopySpacesForWindows};

/// The dispatch selector shared by every bridged operation. The asynchronous
/// operations used here return `void` from it.
const PERFORM: &CStr = c"performWithWMBridgeDelegate";

/// Native Space type of an ordinary Desktop.
const DESKTOP: i64 = 0;

/// `SLSCopySpacesForWindows` selector including parked and minimized windows.
const WINDOW_SPACES_SELECTOR: i32 = 0x7;

const DISPLAY_IDENTIFIER_KEY: &str = "Display Identifier";
const SPACES_KEY: &str = "Spaces";
const CURRENT_SPACE_KEY: &str = "Current Space";
const SPACE_ID_KEY: &str = "id64";
const SPACE_TYPE_KEY: &str = "type";
const SPACE_UUID_KEY: &str = "uuid";

const SESSION_ON_CONSOLE_KEY: &str = "kCGSSessionOnConsoleKey";
const SESSION_LOGIN_DONE_KEY: &str = "kCGSessionLoginDoneKey";
const SESSION_SCREEN_LOCKED_KEY: &str = "CGSSessionScreenIsLocked";

/// A verified `WMBridge` operation ABI. `arguments` holds one Objective-C type
/// code per initializer argument; the initializer returns `@` and the
/// perform selector returns `v`.
#[derive(Clone, Copy)]
struct OperationAbi {
    class: &'static CStr,
    init: &'static CStr,
    arguments: &'static [u8],
}

const SHOW_SPACES: OperationAbi = OperationAbi {
    class: c"SLSBridgedShowSpacesOperation",
    init: c"initWithSpaces:",
    arguments: b"@",
};
const HIDE_SPACES: OperationAbi = OperationAbi {
    class: c"SLSBridgedHideSpacesOperation",
    init: c"initWithSpaces:",
    arguments: b"@",
};
const SET_CURRENT_SPACE: OperationAbi = OperationAbi {
    class: c"SLSBridgedManagedDisplaySetCurrentSpaceOperation",
    init: c"initWithDisplayIdentifier:spaceID:",
    arguments: b"@Q",
};
const MOVE_WINDOWS: OperationAbi = OperationAbi {
    class: c"SLSBridgedMoveWindowsToManagedSpaceOperation",
    init: c"initWithWindows:spaceID:",
    arguments: b"@Q",
};

/// One managed Space from the `SLSCopyManagedDisplaySpaces` census.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SpaceRecord {
    /// Native Space ID, never an ordinal.
    id: WorkspaceId,
    /// Owning display identifier exactly as the window server reports it.
    display: String,
    /// Native Space type; [`DESKTOP`] is an ordinary Desktop.
    kind: i64,
    /// Whether this is the current Space of its own display.
    active: bool,
}

/// Initializer arguments for the verified operation shapes. Objects are
/// toll-free bridged `CoreFoundation` arrays or strings.
#[derive(Clone, Copy)]
enum InitArguments<'a> {
    Object(&'a CFType),
    ObjectAndSpace(&'a CFType, WorkspaceId),
}

impl InitArguments<'_> {
    fn encodings(&self) -> &'static [u8] {
        match self {
            InitArguments::Object(_) => b"@",
            InitArguments::ObjectAndSpace(..) => b"@Q",
        }
    }
}

type InitWithObject =
    unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *const AnyObject) -> *mut AnyObject;
type InitWithObjectAndSpace = unsafe extern "C-unwind" fn(
    *mut AnyObject,
    Sel,
    *const AnyObject,
    WorkspaceId,
) -> *mut AnyObject;
type Perform = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);

// MARK: - Runtime ABI validation and dispatch

/// The instance method must exist, take exactly `arguments.len()` explicit
/// arguments, return `result`, and every argument encoding must be exactly
/// the listed single type code. Unknown or changed encodings are refused,
/// never guessed.
fn method_matches(class: &AnyClass, selector: Sel, result: &[u8], arguments: &[u8]) -> bool {
    let Some(method) = class.instance_method(selector) else {
        return false;
    };
    method.arguments_count() == arguments.len() + 2
        && method.return_type().to_bytes() == result
        && arguments.iter().enumerate().all(|(index, code)| {
            method
                .argument_type(index + 2)
                .is_some_and(|encoding| encoding.to_bytes() == std::slice::from_ref(code))
        })
}

/// Resolves the operation class only when both its initializer and the
/// asynchronous perform selector match the verified ABI. Looking the methods
/// up may run the private class's `+resolveInstanceMethod:`, so callers run
/// this inside [`preflight`].
fn resolve_operation(abi: OperationAbi) -> Option<&'static AnyClass> {
    let class = AnyClass::get(abi.class)?;
    (method_matches(class, Sel::register(abi.init), b"@", abi.arguments)
        && method_matches(class, Sel::register(PERFORM), b"v", b""))
    .then_some(class)
}

/// Allocates and initializes one operation object with its verified
/// initializer. Ownership follows Objective-C: `init` consumes the allocation
/// and may return nil. Plain `objc_msgSend` casts express that and let a
/// thrown exception reach the surrounding [`preflight`] boundary, which
/// `msg_send!` (built with `catch-all`) would turn into a panic instead.
///
/// # Safety
///
/// `class` must have been returned by [`resolve_operation`] for `abi`.
unsafe fn init_operation(
    class: &AnyClass,
    abi: OperationAbi,
    arguments: InitArguments<'_>,
) -> Option<Retained<AnyObject>> {
    if arguments.encodings() != abi.arguments {
        return None;
    }
    let selector = Sel::register(abi.init);
    // SAFETY: the class was resolved from the runtime and is instantiable.
    let allocated = unsafe { objc2::ffi::objc_alloc(class) };
    if allocated.is_null() {
        return None;
    }
    // SAFETY: the initializer encodings were validated against `abi`, so the
    // casts match the method's real signature; `allocated` is a +1 allocation
    // that the initializer consumes.
    let initialized = match arguments {
        InitArguments::Object(object) => {
            let init =
                unsafe { std::mem::transmute::<Imp, InitWithObject>(objc2::ffi::objc_msgSend) };
            unsafe { init(allocated, selector, object_ptr(object)) }
        }
        InitArguments::ObjectAndSpace(object, space) => {
            let init = unsafe {
                std::mem::transmute::<Imp, InitWithObjectAndSpace>(objc2::ffi::objc_msgSend)
            };
            unsafe { init(allocated, selector, object_ptr(object), space) }
        }
    };
    // SAFETY: `init` returns a +1 reference or nil.
    unsafe { Retained::from_raw(initialized) }
}

fn object_ptr(object: &CFType) -> *const AnyObject {
    let object: *const CFType = object;
    object.cast()
}

/// Runs one pre-submission step of a request (class resolution or operation
/// initialization) in its own autorelease pool under the exception boundary.
/// Nothing has been written yet, so both a native exception and a `None`
/// outcome are reported as not submitted, the latter with `refusal`.
fn preflight<T>(
    workspace_id: WorkspaceId,
    refusal: &str,
    step: impl FnOnce() -> Option<T>,
) -> Result<T> {
    autoreleasepool(|_| catch(AssertUnwindSafe(step)))
        .map_err(|exception| request_error(workspace_id, false, exception_message(exception)))?
        .ok_or_else(|| request_error(workspace_id, false, refusal.to_string()))
}

/// Performs the prepared operations in order. From the first perform on, the
/// request may have reached the window server, so every later failure is
/// reported with `request_may_have_applied`.
fn submit(workspace_id: WorkspaceId, operations: &[Retained<AnyObject>]) -> Result<()> {
    let selector = Sel::register(PERFORM);
    // SAFETY: `resolve_operation` verified the perform selector returns void
    // and takes no arguments on every prepared operation's class.
    let perform = unsafe { std::mem::transmute::<Imp, Perform>(objc2::ffi::objc_msgSend) };
    autoreleasepool(|_| {
        catch(AssertUnwindSafe(|| {
            for operation in operations {
                unsafe { perform(Retained::as_ptr(operation).cast_mut(), selector) };
            }
        }))
    })
    .map_err(|exception| request_error(workspace_id, true, exception_message(exception)))
}

fn request_error(
    workspace_id: WorkspaceId,
    request_may_have_applied: bool,
    message: String,
) -> Error {
    Error::NativeSpaceRequest {
        workspace_id,
        request_may_have_applied,
        message,
    }
}

fn exception_message(exception: Option<Retained<Exception>>) -> String {
    exception.map_or_else(
        || "unexpected native exception (nil)".to_string(),
        |exception| format!("unexpected native {exception:?}"),
    )
}

// MARK: - Preconditions

/// Every `WMBridge` and census call must run on the process main thread, not
/// merely on a main-queue label.
fn require_main_thread(what: &str) -> Result<()> {
    if MainThreadMarker::new().is_none() {
        return Err(Error::Generic(format!(
            "{what} must run on the process main thread"
        )));
    }
    Ok(())
}

/// Reads a session flag stored as either a `CFBoolean` or an integer
/// `CFNumber`. `Ok(None)` when the key is absent; an error when it is present
/// in an unexpected representation.
fn session_flag(session: &CFDictionary<CFType, CFType>, key: &'static str) -> Result<Option<bool>> {
    let Some(value) = session.get(&CFString::from_static_str(key)) else {
        return Ok(None);
    };
    if let Some(flag) = value.downcast_ref::<CFBoolean>() {
        return Ok(Some(flag.as_bool()));
    }
    if let Some(number) = value.downcast_ref::<CFNumber>() {
        return Ok(Some(number.as_i64().is_some_and(|value| value != 0)));
    }
    Err(Error::Generic(format!(
        "session key {key} has an unexpected representation"
    )))
}

/// Refuses mutations, and existence queries that may license omitting a
/// window from a mutation, outside an unlocked, logged-in console GUI
/// session, where window metadata may be restricted.
fn require_unlocked_session() -> Result<()> {
    let session = CGSessionCopyCurrentDictionary().ok_or_else(|| {
        Error::PermissionDenied("no graphical login session is available".to_string())
    })?;
    // SAFETY: keys and values of any CF dictionary are `CFType`s.
    let session = unsafe { session.cast_unchecked::<CFType, CFType>() };
    let logged_in = session_flag(session, SESSION_ON_CONSOLE_KEY)? == Some(true)
        && session_flag(session, SESSION_LOGIN_DONE_KEY)? == Some(true);
    if !logged_in {
        return Err(Error::PermissionDenied(
            "the current session is not a logged-in console GUI session".to_string(),
        ));
    }
    if session_flag(session, SESSION_SCREEN_LOCKED_KEY)? == Some(true) {
        return Err(Error::PermissionDenied(
            "the screen is locked; unlock the session before changing native Spaces".to_string(),
        ));
    }
    Ok(())
}

// MARK: - Census model

/// Views any `CoreFoundation` array as untyped objects; every element of a CF
/// collection is a `CFType`, so the element cast is always sound.
fn untyped_array(value: &CFType) -> Option<&CFArray<CFType>> {
    value
        .downcast_ref::<CFArray>()
        .map(|array| unsafe { array.cast_unchecked::<CFType>() })
}

/// Views any `CoreFoundation` dictionary as untyped keys and values.
fn untyped_dictionary(value: &CFType) -> Option<&CFDictionary<CFType, CFType>> {
    value
        .downcast_ref::<CFDictionary>()
        .map(|dictionary| unsafe { dictionary.cast_unchecked::<CFType, CFType>() })
}

fn integer(value: &CFType) -> Option<i64> {
    let number = value.downcast_ref::<CFNumber>()?;
    if number.is_float_type() {
        return None;
    }
    number.as_i64()
}

fn space_id(value: &CFType) -> Option<WorkspaceId> {
    WorkspaceId::try_from(integer(value)?)
        .ok()
        .filter(|id| *id != 0)
}

/// Flattens the `SLSCopyManagedDisplaySpaces` census. Any structural surprise
/// yields `None` so callers fail closed instead of acting on a partial model.
fn parse_census(displays: &CFArray<CFType>) -> Option<Vec<SpaceRecord>> {
    if displays.is_empty() {
        return None;
    }
    let identifier_key = CFString::from_static_str(DISPLAY_IDENTIFIER_KEY);
    let spaces_key = CFString::from_static_str(SPACES_KEY);
    let current_key = CFString::from_static_str(CURRENT_SPACE_KEY);
    let id_key = CFString::from_static_str(SPACE_ID_KEY);
    let type_key = CFString::from_static_str(SPACE_TYPE_KEY);
    let uuid_key = CFString::from_static_str(SPACE_UUID_KEY);

    let mut spaces = Vec::new();
    for display in displays.iter() {
        let display = untyped_dictionary(&display)?;
        let identifier = display.get(&identifier_key)?;
        let identifier = identifier.downcast_ref::<CFString>()?.to_string();
        if identifier.is_empty() {
            return None;
        }
        let members = display.get(&spaces_key)?;
        let members = untyped_array(&members)?;
        if members.is_empty() {
            return None;
        }
        let current = display.get(&current_key)?;
        let current = untyped_dictionary(&current)?;
        let current_id = space_id(&*current.get(&id_key)?)?;

        let mut found_current = false;
        for space in members.iter() {
            let space = untyped_dictionary(&space)?;
            let id = space_id(&*space.get(&id_key)?)?;
            let kind = integer(&*space.get(&type_key)?)?;
            space.get(&uuid_key)?.downcast_ref::<CFString>()?;
            if spaces.iter().any(|record: &SpaceRecord| record.id == id) {
                return None;
            }
            let active = id == current_id;
            found_current |= active;
            spaces.push(SpaceRecord {
                id,
                display: identifier.clone(),
                kind,
                active,
            });
        }
        if !found_current {
            return None;
        }
    }
    Some(spaces)
}

/// Reads a fresh, validated census. Never cached: topology changes at will.
fn read_census(connection: ConnID) -> Result<Vec<SpaceRecord>> {
    let displays = NonNull::new(unsafe { SLSCopyManagedDisplaySpaces(connection) })
        .map(|displays| unsafe { CFRetained::from_raw(displays) })
        .ok_or_else(|| Error::Generic("could not read the managed Space census".to_string()))?;
    // SAFETY: the element types are only a hint; every element is checked
    // against its real CoreFoundation type while parsing.
    parse_census(unsafe { displays.cast_unchecked::<CFType>() })
        .ok_or_else(|| Error::Generic("the managed Space census is malformed".to_string()))
}

fn find_space(census: &[SpaceRecord], workspace_id: WorkspaceId) -> Option<&SpaceRecord> {
    census.iter().find(|space| space.id == workspace_id)
}

fn missing_space(workspace_id: WorkspaceId) -> Error {
    Error::NotFound(format!(
        "no managed Space has native ID {workspace_id}; Desktop numbers are not IDs"
    ))
}

// MARK: - Window model

/// Native Space IDs the window belongs to. Fails closed on a null list or on
/// any member that is not a positive integer.
fn window_memberships(connection: ConnID, window_id: WinID) -> Result<Vec<WorkspaceId>> {
    let windows = create_array(&[window_id], CFNumberType::SInt32Type)?;
    let spaces = NonNull::new(unsafe {
        SLSCopySpacesForWindows(connection, WINDOW_SPACES_SELECTOR, &windows)
    })
    .map(|spaces| unsafe { CFRetained::from_raw(spaces) })
    .ok_or_else(|| {
        Error::NotFound(format!(
            "could not read the native Space memberships of window {window_id}"
        ))
    })?;
    // SAFETY: the element type is only a hint; every member is type-checked.
    unsafe { spaces.cast_unchecked::<CFType>() }
        .iter()
        .map(|space| {
            space_id(&space).ok_or_else(|| {
                Error::Generic(format!(
                    "the native Space memberships of window {window_id} are malformed"
                ))
            })
        })
        .collect()
}

/// The `kCGWindowNumber` of one `CoreGraphics` window list entry, when it is
/// an integer.
fn window_number(entry: &CFDictionary<CFType, CFType>) -> Option<i64> {
    let number_key: &CFType = unsafe { kCGWindowNumber };
    entry.get(number_key).and_then(|number| integer(&number))
}

/// Fresh `CoreGraphics` metadata of every window, on and off screen.
fn window_list() -> Result<CFRetained<CFArray<CFType>>> {
    let entries = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, kCGNullWindowID)
        .ok_or_else(|| Error::Generic("window metadata is unavailable".to_string()))?;
    // SAFETY: elements of any CF array are `CFType`s.
    Ok(unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(entries) })
}

/// Whether the window list contains `window_id`. `None` when any entry is
/// not a dictionary numbered by an integer, so a broken census is never
/// mistaken for absence; unrelated well-formed windows are simply skipped.
fn window_listed(entries: &CFArray<CFType>, window_id: WinID) -> Option<bool> {
    let window = i64::from(window_id);
    for entry in entries.iter() {
        if untyped_dictionary(&entry).and_then(window_number)? == window {
            return Some(true);
        }
    }
    Some(false)
}

/// Window levels of `windows` from the `CoreGraphics` window list, in the same
/// order; `None` where the window has no metadata.
fn window_layers(windows: &[WinID]) -> Result<Vec<Option<i64>>> {
    let entries = window_list()?;
    let layer_key: &CFType = unsafe { kCGWindowLayer };
    let mut layers = vec![None; windows.len()];
    for entry in entries.iter() {
        let Some(entry) = untyped_dictionary(&entry) else {
            continue;
        };
        let Some(number) = window_number(entry) else {
            continue;
        };
        let layer = entry.get(layer_key).and_then(|layer| integer(&layer));
        for (slot, window) in layers.iter_mut().zip(windows) {
            if i64::from(*window) == number {
                *slot = layer;
            }
        }
    }
    Ok(layers)
}

/// Normal, floating, and modal application window levels.
fn application_layer(layer: i64) -> bool {
    matches!(layer, 0 | 3 | 8)
}

// MARK: - Operations

/// Submits the verified activation sequence for `workspace_id` on the display
/// that actually owns it: show the target, hide its display siblings, then set
/// the display's current Space. Returns `Ok(false)` when the target already is
/// its display's current Space and nothing was submitted; `Ok(true)` means the
/// request was submitted, never that it applied.
pub(super) fn activate_workspace(connection: ConnID, workspace_id: WorkspaceId) -> Result<bool> {
    require_main_thread("native Space activation")?;
    if workspace_id == 0 {
        return Err(Error::InvalidInput(
            "native Space ID must be nonzero".to_string(),
        ));
    }
    require_unlocked_session()?;
    let census = read_census(connection)?;
    let target = find_space(&census, workspace_id).ok_or_else(|| missing_space(workspace_id))?;
    if target.active {
        return Ok(false);
    }
    let [show_class, hide_class, current_class] = preflight(
        workspace_id,
        "native Space activation is unavailable or its ABI changed",
        || {
            Some([
                resolve_operation(SHOW_SPACES)?,
                resolve_operation(HIDE_SPACES)?,
                resolve_operation(SET_CURRENT_SPACE)?,
            ])
        },
    )?;

    let siblings = census
        .iter()
        .filter(|space| space.display == target.display && space.id != workspace_id)
        .map(|space| space.id)
        .collect::<Vec<_>>();
    let shown = create_array(&[workspace_id], CFNumberType::SInt64Type)?;
    let hidden = create_array(&siblings, CFNumberType::SInt64Type)?;
    let display = CFString::from_str(&target.display);

    let operations = preflight(
        workspace_id,
        "could not initialize the native Space activation operations",
        || unsafe {
            Some([
                init_operation(show_class, SHOW_SPACES, InitArguments::Object(&shown))?,
                init_operation(hide_class, HIDE_SPACES, InitArguments::Object(&hidden))?,
                init_operation(
                    current_class,
                    SET_CURRENT_SPACE,
                    InitArguments::ObjectAndSpace(&display, workspace_id),
                )?,
            ])
        },
    )?;
    submit(workspace_id, &operations)?;
    trace!(
        workspace_id,
        display = %target.display,
        ?siblings,
        "submitted native Space activation"
    );
    Ok(true)
}

/// Whether `workspace_id` is the current Space of the display that owns it,
/// read from a fresh census. Errors when the Space no longer exists.
pub(super) fn workspace_is_active(connection: ConnID, workspace_id: WorkspaceId) -> Result<bool> {
    require_main_thread("native Space queries")?;
    let census = read_census(connection)?;
    find_space(&census, workspace_id)
        .map(|space| space.active)
        .ok_or_else(|| missing_space(workspace_id))
}

/// The single ordinary Desktop hosting `window_id`, once the window has proven
/// to be a movable application window.
fn movable_source(
    connection: ConnID,
    census: &[SpaceRecord],
    window_id: WinID,
    layer: Option<i64>,
) -> Result<WorkspaceId> {
    let layer = layer.ok_or_else(|| {
        Error::Generic(format!(
            "window {window_id} has no window metadata; refusing to move an unclassified window"
        ))
    })?;
    if !application_layer(layer) {
        return Err(Error::InvalidInput(format!(
            "window {window_id} is at level {layer}; only normal, floating and modal application windows can be moved"
        )));
    }
    let memberships = window_memberships(connection, window_id)?;
    let source = match memberships.as_slice() {
        [] => {
            return Err(Error::NotFound(format!(
                "window {window_id} has no native Space membership; it may not exist"
            )));
        }
        [source] => *source,
        _ => {
            return Err(Error::InvalidInput(format!(
                "window {window_id} belongs to {} native Spaces; sticky and multi-Space windows cannot be moved",
                memberships.len()
            )));
        }
    };
    if find_space(census, source).is_none_or(|space| space.kind != DESKTOP) {
        return Err(Error::InvalidInput(format!(
            "window {window_id} is on native Space {source}, which is not an ordinary Desktop"
        )));
    }
    Ok(source)
}

/// Submits one asynchronous request assigning `windows` to exactly one native
/// Space. This is movement, not stickiness. The whole batch is validated
/// before anything is submitted: every window must be an ordinary application
/// window (levels 0, 3, 8) on exactly one ordinary Desktop, and the destination
/// must be an ordinary Desktop. Windows already on the destination are left
/// out; when nothing remains, nothing is submitted.
pub(super) fn move_windows_to_workspace(
    connection: ConnID,
    windows: &[WinID],
    workspace_id: WorkspaceId,
) -> Result<()> {
    if windows.is_empty() {
        return Ok(());
    }
    require_main_thread("native window moves")?;
    if workspace_id == 0 || windows.contains(&0) {
        return Err(Error::InvalidInput(
            "window and native Space IDs must be nonzero".to_string(),
        ));
    }
    require_unlocked_session()?;
    let class = preflight(
        workspace_id,
        "native window moves are unavailable or their ABI changed",
        || resolve_operation(MOVE_WINDOWS),
    )?;
    let census = read_census(connection)?;
    let target = find_space(&census, workspace_id).ok_or_else(|| missing_space(workspace_id))?;
    if target.kind != DESKTOP {
        return Err(Error::InvalidInput(format!(
            "native Space {workspace_id} is not an ordinary Desktop; windows can only be moved to Desktops"
        )));
    }

    let layers = window_layers(windows)?;
    let mut pending = Vec::with_capacity(windows.len());
    for (window_id, layer) in windows.iter().copied().zip(layers) {
        if movable_source(connection, &census, window_id, layer)? != workspace_id {
            pending.push(window_id);
        }
    }
    if pending.is_empty() {
        trace!(
            ?windows,
            workspace_id, "windows already on the native Space"
        );
        return Ok(());
    }

    let window_list = create_array(&pending, CFNumberType::SInt32Type)?;
    let operations = preflight(
        workspace_id,
        "could not initialize the native window move operation",
        || unsafe {
            Some([init_operation(
                class,
                MOVE_WINDOWS,
                InitArguments::ObjectAndSpace(&window_list, workspace_id),
            )?])
        },
    )?;
    submit(workspace_id, &operations)?;
    trace!(windows = ?pending, workspace_id, "submitted native window move");
    Ok(())
}

/// Returns the native Spaces containing `window_id`, failing closed on null or
/// malformed data.
pub(super) fn window_workspaces(connection: ConnID, window_id: WinID) -> Result<Vec<WorkspaceId>> {
    require_main_thread("native Space queries")?;
    if window_id == 0 {
        return Err(Error::InvalidInput("window ID must be nonzero".to_string()));
    }
    window_memberships(connection, window_id)
}

/// Whether the window server still lists `window_id` in a fresh census of
/// every window, on and off screen. `Ok(false)` only after that census was
/// read, in an unlocked logged-in console session, and parsed completely
/// without the window, so a window that vanished can be told apart from a
/// lookup that failed: a locked or foreign session (where metadata may be
/// restricted), a null list or a malformed entry is an error, never absence.
/// (A targeted `kCGWindowListOptionIncludingWindow` query is not used: it was
/// observed to return an empty list for a live hidden system window.)
pub(super) fn window_exists(window_id: WinID) -> Result<bool> {
    require_main_thread("native window queries")?;
    if window_id <= 0 {
        return Err(Error::InvalidInput(
            "window ID must be positive".to_string(),
        ));
    }
    require_unlocked_session()?;
    let windows = window_list()?;
    window_listed(&windows, window_id).ok_or_else(|| {
        Error::Generic(format!(
            "window metadata is malformed; cannot tell whether window {window_id} exists"
        ))
    })
}

/// Posts a left click at `point` to complete display focus once a native
/// Space switch has been observed on another display.
pub(super) fn post_left_click(point: CGPoint) -> Result<()> {
    require_main_thread("display focus")?;
    for event_type in [CGEventType::LeftMouseDown, CGEventType::LeftMouseUp] {
        let event = CGEvent::new_mouse_event(None, event_type, point, CGMouseButton::Left)
            .ok_or_else(|| Error::Generic("could not create display focus event".to_string()))?;
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use objc2::runtime::NSObject;
    use objc2::sel;

    use super::*;

    /// Sends a selector `NSObject` does not implement through the same raw
    /// `objc_msgSend` cast the dispatch path uses, so the runtime throws a
    /// genuine `NSInvalidArgumentException` out of a `C-unwind` call.
    fn send_unrecognized_selector(object: &AnyObject) {
        let send = unsafe { std::mem::transmute::<Imp, Perform>(objc2::ffi::objc_msgSend) };
        // SAFETY: the selector takes no arguments and the receiver does not
        // implement it, so the runtime forwards to `doesNotRecognizeSelector:`
        // and throws before any call with a mismatched signature could run.
        unsafe {
            send(
                std::ptr::from_ref(object).cast_mut(),
                sel!(paneruMissingSelector),
            );
        }
    }

    #[test]
    fn preflight_reports_native_exceptions_and_refusals_as_not_submitted() {
        let object = NSObject::new();
        let thrown = preflight(7, "unused", || {
            send_unrecognized_selector(&object);
            Some(())
        })
        .expect_err("the exception must be caught at the boundary");
        assert!(matches!(
            thrown,
            Error::NativeSpaceRequest {
                workspace_id: 7,
                request_may_have_applied: false,
                ..
            }
        ));

        let refused = preflight(7, "unsupported", || None::<()>).expect_err("refusal");
        assert!(matches!(
            refused,
            Error::NativeSpaceRequest {
                workspace_id: 7,
                request_may_have_applied: false,
                ..
            }
        ));
    }

    #[test]
    fn submit_reports_native_exceptions_as_possibly_applied() {
        // `NSObject` does not implement the perform selector, so the first
        // perform throws; from then on the request may have reached the
        // window server.
        let stranger = NSObject::new().into_super();
        let error = submit(7, &[stranger]).expect_err("the exception must be caught");
        assert!(matches!(
            error,
            Error::NativeSpaceRequest {
                workspace_id: 7,
                request_may_have_applied: true,
                ..
            }
        ));
    }

    #[test]
    fn method_matches_requires_exact_count_return_and_argument_encodings() {
        let object = AnyClass::get(c"NSObject").expect("NSObject");
        assert!(method_matches(object, sel!(init), b"@", b""));
        assert!(method_matches(object, sel!(performSelector:), b"@", b":"));
        assert!(!method_matches(object, sel!(performSelector:), b"v", b":"));
        assert!(!method_matches(object, sel!(performSelector:), b"@", b""));
        assert!(!method_matches(object, sel!(performSelector:), b"@", b"@"));
        assert!(!method_matches(
            object,
            sel!(paneruMissingSelector),
            b"@",
            b""
        ));
        assert!(
            resolve_operation(OperationAbi {
                class: c"PaneruMissingOperation",
                init: c"initWithSpaces:",
                arguments: b"@",
            })
            .is_none()
        );
    }

    fn dictionary(entries: &[(&str, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
        let keys = entries
            .iter()
            .map(|(key, _)| CFString::from_str(key))
            .collect::<Vec<_>>();
        let keys = keys.iter().map(|key| &**key).collect::<Vec<_>>();
        let values = entries.iter().map(|(_, value)| *value).collect::<Vec<_>>();
        CFDictionary::from_slices(&keys, &values)
    }

    fn space(id: &CFType, kind: i64) -> CFRetained<CFDictionary<CFString, CFType>> {
        let kind = CFNumber::new_i64(kind);
        let uuid = CFString::from_static_str("uuid");
        dictionary(&[
            (SPACE_ID_KEY, id),
            (SPACE_TYPE_KEY, &kind),
            (SPACE_UUID_KEY, &uuid),
        ])
    }

    fn display(
        identifier: &str,
        current: i64,
        spaces: &[CFRetained<CFDictionary<CFString, CFType>>],
    ) -> CFRetained<CFDictionary<CFString, CFType>> {
        let identifier = CFString::from_str(identifier);
        let members = spaces.iter().map(|space| &***space).collect::<Vec<_>>();
        let members = CFArray::<CFType>::from_objects(&members);
        let current_id = CFNumber::new_i64(current);
        let current = dictionary(&[(SPACE_ID_KEY, &current_id)]);
        dictionary(&[
            (DISPLAY_IDENTIFIER_KEY, &identifier),
            (SPACES_KEY, &members),
            (CURRENT_SPACE_KEY, &current),
        ])
    }

    fn census(displays: &[CFRetained<CFDictionary<CFString, CFType>>]) -> Option<Vec<SpaceRecord>> {
        let displays = displays
            .iter()
            .map(|display| &***display)
            .collect::<Vec<_>>();
        parse_census(&CFArray::<CFType>::from_objects(&displays))
    }

    fn desktop(id: i64) -> CFRetained<CFDictionary<CFString, CFType>> {
        space(&CFNumber::new_i64(id), DESKTOP)
    }

    #[test]
    fn census_records_each_display_current_space_by_native_id() {
        let fullscreen = space(&CFNumber::new_i64(30), 4);
        let parsed = census(&[
            display("A", 20, &[desktop(10), desktop(20)]),
            display("B", 30, &[fullscreen, desktop(40)]),
        ])
        .expect("well-formed census");
        assert_eq!(
            parsed,
            vec![
                SpaceRecord {
                    id: 10,
                    display: "A".into(),
                    kind: DESKTOP,
                    active: false
                },
                SpaceRecord {
                    id: 20,
                    display: "A".into(),
                    kind: DESKTOP,
                    active: true
                },
                SpaceRecord {
                    id: 30,
                    display: "B".into(),
                    kind: 4,
                    active: true
                },
                SpaceRecord {
                    id: 40,
                    display: "B".into(),
                    kind: DESKTOP,
                    active: false
                },
            ]
        );
    }

    #[test]
    fn census_fails_closed_on_structural_surprises() {
        assert!(census(&[]).is_none(), "no displays");
        assert!(
            census(&[display("A", 30, &[desktop(10), desktop(20)])]).is_none(),
            "current Space missing from its display"
        );
        assert!(
            census(&[
                display("A", 10, &[desktop(10)]),
                display("B", 10, &[desktop(10)]),
            ])
            .is_none(),
            "duplicate native ID"
        );
        assert!(
            census(&[display("", 10, &[desktop(10)])]).is_none(),
            "empty display identifier"
        );
        assert!(
            census(&[display("A", 0, &[space(&CFNumber::new_i64(0), DESKTOP)])]).is_none(),
            "zero native ID"
        );
        assert!(
            census(&[display(
                "A",
                10,
                &[space(&CFNumber::new_f64(10.0), DESKTOP)]
            )])
            .is_none(),
            "floating-point native ID"
        );
        let no_spaces = {
            let identifier = CFString::from_static_str("A");
            let current_id = CFNumber::new_i64(10);
            let current = dictionary(&[(SPACE_ID_KEY, &current_id)]);
            dictionary(&[
                (DISPLAY_IDENTIFIER_KEY, &identifier),
                (CURRENT_SPACE_KEY, &current),
            ])
        };
        assert!(census(&[no_spaces]).is_none(), "missing Spaces list");
    }

    fn window_entry(number: &CFType) -> CFRetained<CFDictionary<CFString, CFType>> {
        let key = unsafe { kCGWindowNumber }.to_string();
        dictionary(&[(&key, number)])
    }

    fn listed(entries: &[&CFType], window_id: WinID) -> Option<bool> {
        window_listed(&CFArray::<CFType>::from_objects(entries), window_id)
    }

    #[test]
    fn window_presence_scans_whole_census_and_fails_closed_on_malformed_entries() {
        let other = window_entry(&CFNumber::new_i64(5));
        let target = window_entry(&CFNumber::new_i64(8));
        let other: &CFType = &other;
        let target: &CFType = &target;
        assert_eq!(listed(&[], 8), Some(false), "empty census");
        assert_eq!(listed(&[other], 8), Some(false), "only other windows");
        assert_eq!(
            listed(&[other, target], 8),
            Some(true),
            "target after unrelated entries"
        );
        let unnumbered = dictionary(&[]);
        let unnumbered: &CFType = &unnumbered;
        assert!(
            listed(&[unnumbered, target], 8).is_none(),
            "entry without a window number"
        );
        let fractional = window_entry(&CFNumber::new_f64(8.0));
        let fractional: &CFType = &fractional;
        assert!(
            listed(&[fractional], 8).is_none(),
            "floating-point window number"
        );
        let stray = CFString::from_static_str("not a window");
        let stray: &CFType = &stray;
        assert!(
            listed(&[other, stray, target], 8).is_none(),
            "non-dictionary entry before the target"
        );
    }
}

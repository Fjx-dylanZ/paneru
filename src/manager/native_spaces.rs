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
//!   encodings (class, initializer arguments, perform return, and the
//!   `spaceID` getter of a synchronous result) before a call;
//! * census reads fail closed on any structural surprise;
//! * mutations run on the process main thread in an unlocked, logged-in
//!   console session, prepare every operation object before the first write,
//!   and only *submit* requests; callers observe the census later. There is
//!   no confirmation polling or run-loop pumping here. Desktop creation's
//!   verified synchronous perform returns an object carrying the new ID, but
//!   the Desktop may appear in the census later: that ID is a receipt to
//!   confirm, not a confirmation.
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

use paneru_shared_types::state::NativeSpaceState;

use crate::errors::{Error, Result};
use crate::platform::{ConnID, WinID, WorkspaceId};
use crate::util::create_array;

use super::skylight::{
    SLSCopyManagedDisplaySpaces, SLSCopySpacesForWindows, SLSCopyWindowsWithOptionsAndTags,
};

/// The dispatch selector shared by every bridged operation. Asynchronous
/// operations return `void` from it; the synchronous create returns its
/// result object.
const PERFORM: &CStr = c"performWithWMBridgeDelegate";

/// Getter of the new Space ID on the object a synchronous create returns.
const SPACE_ID_GETTER: &CStr = c"spaceID";

/// Perform encodings: asynchronous (`void`) and synchronous (result object).
const ASYNC: &[u8] = b"v";
const SYNC: &[u8] = b"@";

/// Native Space type of an ordinary Desktop.
const DESKTOP: i64 = 0;

/// `SLSCopySpacesForWindows` selector including parked and minimized windows.
const WINDOW_SPACES_SELECTOR: i32 = 0x7;

/// `SLSCopyWindowsWithOptionsAndTags` options listing every window on a
/// Space, minimized and parked included.
const SPACE_WINDOWS_OPTIONS: i32 = 0x7;

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
/// perform selector returns `perform` ([`ASYNC`] or [`SYNC`]).
#[derive(Clone, Copy)]
struct OperationAbi {
    class: &'static CStr,
    init: &'static CStr,
    arguments: &'static [u8],
    perform: &'static [u8],
}

const CREATE_SPACE: OperationAbi = OperationAbi {
    class: c"SLSBridgedSpaceCreateOperation",
    init: c"initWithOptions:values:",
    arguments: b"I@",
    perform: SYNC,
};
const DESTROY_SPACE: OperationAbi = OperationAbi {
    class: c"SLSBridgedSpaceDestroyOperation",
    init: c"initWithSpaceID:",
    arguments: b"Q",
    perform: ASYNC,
};
const SHOW_SPACES: OperationAbi = OperationAbi {
    class: c"SLSBridgedShowSpacesOperation",
    init: c"initWithSpaces:",
    arguments: b"@",
    perform: ASYNC,
};
const HIDE_SPACES: OperationAbi = OperationAbi {
    class: c"SLSBridgedHideSpacesOperation",
    init: c"initWithSpaces:",
    arguments: b"@",
    perform: ASYNC,
};
const SET_CURRENT_SPACE: OperationAbi = OperationAbi {
    class: c"SLSBridgedManagedDisplaySetCurrentSpaceOperation",
    init: c"initWithDisplayIdentifier:spaceID:",
    arguments: b"@Q",
    perform: ASYNC,
};
const MOVE_WINDOWS: OperationAbi = OperationAbi {
    class: c"SLSBridgedMoveWindowsToManagedSpaceOperation",
    init: c"initWithWindows:spaceID:",
    arguments: b"@Q",
    perform: ASYNC,
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
/// toll-free bridged `CoreFoundation` arrays, strings or dictionaries.
#[derive(Clone, Copy)]
enum InitArguments<'a> {
    Object(&'a CFType),
    ObjectAndSpace(&'a CFType, WorkspaceId),
    OptionsAndValues(u32, &'a CFType),
    Space(WorkspaceId),
}

impl InitArguments<'_> {
    fn encodings(&self) -> &'static [u8] {
        match self {
            InitArguments::Object(_) => b"@",
            InitArguments::ObjectAndSpace(..) => b"@Q",
            InitArguments::OptionsAndValues(..) => b"I@",
            InitArguments::Space(_) => b"Q",
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
type InitWithOptionsAndValues =
    unsafe extern "C-unwind" fn(*mut AnyObject, Sel, u32, *const AnyObject) -> *mut AnyObject;
type InitWithSpace =
    unsafe extern "C-unwind" fn(*mut AnyObject, Sel, WorkspaceId) -> *mut AnyObject;
type Perform = unsafe extern "C-unwind" fn(*mut AnyObject, Sel);
type PerformSync = unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut AnyObject;
type SpaceIdGetter = unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> WorkspaceId;

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

/// Resolves the operation class only when both its initializer and its
/// perform selector match the verified ABI. Looking the methods up may run
/// the private class's `+resolveInstanceMethod:`, so callers run this inside
/// [`preflight`].
fn resolve_operation(abi: OperationAbi) -> Option<&'static AnyClass> {
    let class = AnyClass::get(abi.class)?;
    (method_matches(class, Sel::register(abi.init), b"@", abi.arguments)
        && method_matches(class, Sel::register(PERFORM), abi.perform, b""))
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
        InitArguments::OptionsAndValues(options, values) => {
            let init = unsafe {
                std::mem::transmute::<Imp, InitWithOptionsAndValues>(objc2::ffi::objc_msgSend)
            };
            unsafe { init(allocated, selector, options, object_ptr(values)) }
        }
        InitArguments::Space(space) => {
            let init =
                unsafe { std::mem::transmute::<Imp, InitWithSpace>(objc2::ffi::objc_msgSend) };
            unsafe { init(allocated, selector, space) }
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

/// Performs the prepared asynchronous operations in order. From the first
/// perform on, the request may have reached the window server, so every later
/// failure is reported with `request_may_have_applied`.
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

/// The `spaceID` of a synchronous create result, read only when the result's
/// class implements the verified `Q` getter; anything else is no usable ID.
///
/// # Safety
///
/// `result` must be null or point to a live object.
unsafe fn result_space_id(result: *mut AnyObject) -> Option<WorkspaceId> {
    // SAFETY: the caller guarantees a null or live object pointer.
    let class = unsafe { result.as_ref() }?.class();
    let getter = Sel::register(SPACE_ID_GETTER);
    if !method_matches(class, getter, b"Q", b"") {
        return None;
    }
    // SAFETY: the getter encoding was validated as `Q` with no arguments.
    let read = unsafe { std::mem::transmute::<Imp, SpaceIdGetter>(objc2::ffi::objc_msgSend) };
    Some(unsafe { read(result, getter) })
}

/// Performs the prepared synchronous create and reads its result's Space ID
/// inside the same autorelease pool: the result is a +0 return that the pool
/// owns, so nothing is retained past this call. From the perform on, a
/// Desktop may have been created; a thrown exception or an unusable result
/// is reported as possibly applied with no known ID.
fn submit_create(operation: &Retained<AnyObject>) -> Result<WorkspaceId> {
    let selector = Sel::register(PERFORM);
    // SAFETY: `resolve_operation` verified the perform selector returns an
    // object and takes no arguments on the create operation's class.
    let perform = unsafe { std::mem::transmute::<Imp, PerformSync>(objc2::ffi::objc_msgSend) };
    autoreleasepool(|_| {
        catch(AssertUnwindSafe(|| {
            let result = unsafe { perform(Retained::as_ptr(operation).cast_mut(), selector) };
            // SAFETY: the perform returned null or an autoreleased object that
            // stays alive until this pool drains.
            unsafe { result_space_id(result) }
        }))
    })
    .map_err(|exception| request_error(0, true, exception_message(exception)))?
    .ok_or_else(|| {
        request_error(
            0,
            true,
            "the native create operation returned no usable Space ID".to_string(),
        )
    })
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

/// Numbers a validated census for callers: 1-based global Mission Control
/// index and 1-based index within the owning display, in census order.
fn census_states(census: Vec<SpaceRecord>) -> Vec<NativeSpaceState> {
    let mut states: Vec<NativeSpaceState> = Vec::with_capacity(census.len());
    for space in census {
        let index = states.len() + 1;
        let display_index = states
            .iter()
            .filter(|state| state.display == space.display)
            .count()
            + 1;
        states.push(NativeSpaceState {
            id: space.id,
            index,
            display: space.display,
            display_index,
            kind: space.kind,
            active: space.active,
        });
    }
    states
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

/// Window IDs from a `SkyLight` window list. `None` on any member that is
/// not a positive 32-bit integer, so a broken list is never mistaken for an
/// empty Space.
fn parse_window_ids(windows: &CFArray<CFType>) -> Option<Vec<WinID>> {
    windows
        .iter()
        .map(|window| {
            WinID::try_from(integer(&window)?)
                .ok()
                .filter(|window| *window > 0)
        })
        .collect()
}

/// Every window the window server lists on `workspace_id`, minimized and
/// parked included. A null list is an error, not an empty Space.
fn space_windows(connection: ConnID, workspace_id: WorkspaceId) -> Result<Vec<WinID>> {
    let spaces = create_array(&[workspace_id], CFNumberType::SInt64Type)?;
    let mut set_tags = 0i64;
    let mut clear_tags = 0i64;
    let windows = NonNull::new(unsafe {
        SLSCopyWindowsWithOptionsAndTags(
            connection,
            0,
            &raw const *spaces,
            SPACE_WINDOWS_OPTIONS,
            &mut set_tags,
            &mut clear_tags,
        )
    })
    .map(|windows| unsafe { CFRetained::from_raw(windows) })
    .ok_or_else(|| {
        Error::Generic(format!(
            "could not inspect the windows on native Space {workspace_id}"
        ))
    })?;
    // SAFETY: elements of any CF array are `CFType`s; each is type-checked.
    parse_window_ids(unsafe { windows.cast_unchecked::<CFType>() }).ok_or_else(|| {
        Error::Generic(format!(
            "the window list of native Space {workspace_id} is malformed"
        ))
    })
}

/// The application windows (levels 0, 3, 8) among `windows`, given their
/// levels in the same order. Missing metadata fails closed: a window that
/// cannot be classified is never treated as absent.
fn application_windows(windows: &[WinID], layers: &[Option<i64>]) -> Result<Vec<WinID>> {
    let mut sampled = Vec::with_capacity(windows.len());
    for (window_id, layer) in windows.iter().copied().zip(layers) {
        let layer = layer.ok_or_else(|| {
            Error::Generic(format!(
                "window {window_id} has no window metadata; refusing to classify its native Space"
            ))
        })?;
        if application_layer(layer) {
            sampled.push(window_id);
        }
    }
    Ok(sampled)
}

/// Fresh sample of the normal, floating and modal windows on `workspace_id`,
/// minimized included.
fn space_application_windows(connection: ConnID, workspace_id: WorkspaceId) -> Result<Vec<WinID>> {
    let windows = space_windows(connection, workspace_id)?;
    let layers = window_layers(&windows)?;
    application_windows(&windows, &layers)
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

/// Fresh, validated census of every native Space in global Mission Control
/// order, empty and fullscreen Spaces included. Never cached.
pub(super) fn native_space_info(connection: ConnID) -> Result<Vec<NativeSpaceState>> {
    require_main_thread("native Space queries")?;
    read_census(connection).map(census_states)
}

/// Creates one ordinary Desktop on the bridge's default display without
/// switching to it, through the verified synchronous create operation.
///
/// `Ok(id)` is the receipt the window server handed back: a nonzero ID absent
/// from the census read before the call. The Desktop itself appears in the
/// census later, so the caller confirms it from a later census, along with
/// which display actually owns it. Errors after the perform report
/// `request_may_have_applied` and never label a pre-existing ID as created:
/// their `workspace_id` is `0` unless the returned ID was nonzero and new.
pub(super) fn create_workspace(connection: ConnID) -> Result<WorkspaceId> {
    require_main_thread("native Space creation")?;
    require_unlocked_session()?;
    let before = read_census(connection)?;
    let class = preflight(
        0,
        "native Space creation is unavailable or its ABI changed",
        || resolve_operation(CREATE_SPACE),
    )?;
    let values = CFDictionary::<CFString, CFType>::empty();
    let operation = preflight(
        0,
        "could not initialize the native create operation",
        || unsafe {
            init_operation(
                class,
                CREATE_SPACE,
                InitArguments::OptionsAndValues(0, &values),
            )
        },
    )?;
    let created = submit_create(&operation)?;
    if created == 0 {
        return Err(request_error(
            0,
            true,
            "the native create operation returned Space ID 0".to_string(),
        ));
    }
    if find_space(&before, created).is_some() {
        return Err(request_error(
            0,
            true,
            format!(
                "the native create operation returned Space ID {created}, which already existed"
            ),
        ));
    }
    trace!(workspace_id = created, "submitted native Space creation");
    Ok(created)
}

/// Submits the asynchronous destruction of the ordinary Desktop
/// `workspace_id`. Refused before anything is submitted when the Space is
/// current on its display, the last ordinary Desktop of its display, not a
/// Desktop, or, unless `migrate`, hosts any normal, floating or modal window
/// (minimized included; a window without metadata counts as present).
/// `migrate` lifts only that occupancy guard: macOS moves the windows to the
/// current Desktop itself; nothing here moves or closes a window.
///
/// `Ok(windows)` is the sample of application windows taken by the preflight,
/// which the caller confirms later: the Space gone from the census and every
/// sampled window a member of some other Space. It is a submission, never a
/// confirmation. An error after the perform reports
/// `request_may_have_applied` and carries no sample.
pub(super) fn destroy_workspace(
    connection: ConnID,
    workspace_id: WorkspaceId,
    migrate: bool,
) -> Result<Vec<WinID>> {
    require_main_thread("native Space destruction")?;
    if workspace_id == 0 {
        return Err(Error::InvalidInput(
            "native Space ID must be nonzero".to_string(),
        ));
    }
    require_unlocked_session()?;
    let census = read_census(connection)?;
    let target = find_space(&census, workspace_id).ok_or_else(|| missing_space(workspace_id))?;
    if target.kind != DESKTOP {
        return Err(Error::InvalidInput(format!(
            "native Space {workspace_id} is not an ordinary Desktop; only Desktops may be destroyed"
        )));
    }
    let desktops = census
        .iter()
        .filter(|space| space.display == target.display && space.kind == DESKTOP)
        .count();
    if desktops < 2 {
        return Err(Error::InvalidInput(format!(
            "native Space {workspace_id} is the last ordinary Desktop of its display; refusing to remove it"
        )));
    }
    if target.active {
        return Err(Error::InvalidInput(format!(
            "native Space {workspace_id} is current on its display; switch to another Desktop before destroying it"
        )));
    }
    let class = preflight(
        workspace_id,
        "native Space destruction is unavailable or its ABI changed",
        || resolve_operation(DESTROY_SPACE),
    )?;
    let windows = space_application_windows(connection, workspace_id)?;
    if !migrate && !windows.is_empty() {
        return Err(Error::InvalidInput(format!(
            "native Space {workspace_id} hosts {} application window(s), e.g. window {}; move them elsewhere or request migration",
            windows.len(),
            windows[0]
        )));
    }
    let operations = preflight(
        workspace_id,
        "could not initialize the native destroy operation",
        || unsafe {
            Some([init_operation(
                class,
                DESTROY_SPACE,
                InitArguments::Space(workspace_id),
            )?])
        },
    )?;
    submit(workspace_id, &operations)?;
    trace!(
        workspace_id,
        display = %target.display,
        migrate,
        ?windows,
        "submitted native Space destruction"
    );
    Ok(windows)
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
    fn create_submission_never_reports_an_id_it_did_not_read() {
        // `NSObject` does not implement the perform selector: the perform
        // throws, so a Desktop may have been requested but no ID is known.
        let stranger = NSObject::new().into_super();
        let error = submit_create(&stranger).expect_err("the exception must be caught");
        assert!(matches!(
            error,
            Error::NativeSpaceRequest {
                workspace_id: 0,
                request_may_have_applied: true,
                ..
            }
        ));

        // A result without the verified `spaceID` getter, or no result at
        // all, yields no ID rather than a garbage read.
        let plain = NSObject::new();
        assert!(unsafe { result_space_id(Retained::as_ptr(&plain).cast_mut().cast()) }.is_none());
        assert!(unsafe { result_space_id(std::ptr::null_mut()) }.is_none());
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
                perform: ASYNC,
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

    #[test]
    fn census_states_number_spaces_globally_and_per_owning_display() {
        let fullscreen = space(&CFNumber::new_i64(30), 4);
        let parsed = census(&[
            display("A", 20, &[desktop(10), desktop(20)]),
            display("B", 30, &[fullscreen, desktop(40)]),
        ])
        .expect("well-formed census");
        let states = census_states(parsed);
        let numbered = states
            .iter()
            .map(|state| {
                (
                    state.id,
                    state.index,
                    state.display.as_str(),
                    state.display_index,
                    state.kind,
                    state.active,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            numbered,
            vec![
                (10, 1, "A", 1, DESKTOP, false),
                (20, 2, "A", 2, DESKTOP, true),
                (30, 3, "B", 1, 4, true),
                (40, 4, "B", 2, DESKTOP, false),
            ]
        );
    }

    #[test]
    fn space_window_sample_keeps_application_levels_and_fails_closed_without_metadata() {
        assert_eq!(
            application_windows(&[5, 6, 7, 8], &[Some(0), Some(25), Some(3), Some(8)])
                .expect("classified"),
            vec![5, 7, 8],
            "only normal, floating and modal levels are sampled"
        );
        assert!(
            application_windows(&[], &[]).expect("empty").is_empty(),
            "an empty Space samples nothing"
        );
        assert!(
            application_windows(&[5, 6], &[Some(0), None]).is_err(),
            "a window without metadata is never treated as absent"
        );

        let numbers = [CFNumber::new_i64(5), CFNumber::new_i64(6)];
        let numbers = numbers.iter().map(|n| &***n).collect::<Vec<&CFType>>();
        assert_eq!(
            parse_window_ids(&CFArray::<CFType>::from_objects(&numbers)),
            Some(vec![5, 6])
        );
        let zero = CFNumber::new_i64(0);
        let zero: &CFType = &zero;
        assert!(
            parse_window_ids(&CFArray::<CFType>::from_objects(&[zero])).is_none(),
            "a zero window ID is malformed, not an empty Space"
        );
        let text = CFString::from_static_str("5");
        let text: &CFType = &text;
        assert!(
            parse_window_ids(&CFArray::<CFType>::from_objects(&[text])).is_none(),
            "a non-numeric member is malformed"
        );
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

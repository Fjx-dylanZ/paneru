use std::cmp::Ordering;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_core_foundation::{CFNumberType, CFRetained, CGPoint};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventTapLocation, CGEventType, CGMouseButton};
use tracing::{debug, trace};

use crate::errors::{Error, Result};
use crate::platform::{ConnID, WinID, WorkspaceId};
use crate::util::create_array;

use super::macho::find_loaded_symbol;

const SKYLIGHT_IMAGE: &std::ffi::CStr =
    c"/System/Library/PrivateFrameworks/SkyLight.framework/Versions/A/SkyLight";
const BRIDGED_OPERATION_SYMBOL: &std::ffi::CStr = c"__ZL54SLSPerformAsynchronousBridgedWindowManagementOperationP47SLSAsynchronousBridgedWindowManagementOperation";
const MOVE_OPERATION_CLASS: &std::ffi::CStr = c"SLSBridgedMoveWindowsToManagedSpaceOperation";

const GESTURE_EVENT_TYPE: CGEventField = CGEventField(55);
const GESTURE_HID_TYPE: CGEventField = CGEventField(110);
const GESTURE_SWIPE_MOTION: CGEventField = CGEventField(123);
const GESTURE_SWIPE_PROGRESS: CGEventField = CGEventField(124);
const GESTURE_PHASE: CGEventField = CGEventField(132);
const GESTURE_VELOCITY_X: CGEventField = CGEventField(129);
const GESTURE_EVENT_TYPE_VALUE: i64 = 30;
const GESTURE_HID_TYPE_DOCK_SWIPE: i64 = 23;
const GESTURE_SWIPE_MOTION_HORIZONTAL: i64 = 1;
const GESTURE_PHASE_BEGAN: i64 = 1;
const GESTURE_PHASE_ENDED: i64 = 4;
const GESTURE_INSTANT_VELOCITY: f64 = 9999.0;
type PerformOperation = unsafe extern "C" fn(*mut c_void) -> i64;

fn perform_operation() -> Option<PerformOperation> {
    static OPERATION: OnceLock<Option<PerformOperation>> = OnceLock::new();
    *OPERATION.get_or_init(|| {
        let address = unsafe { find_loaded_symbol(SKYLIGHT_IMAGE, BRIDGED_OPERATION_SYMBOL) }?;
        debug!("SIP-safe native workspace movement is available");
        Some(unsafe { std::mem::transmute::<*mut c_void, PerformOperation>(address.as_ptr()) })
    })
}

/// Submit an asynchronous `WindowServer` operation that assigns the windows to
/// exactly one native macOS Space. This is movement, not true all-Spaces
/// stickiness.
pub(super) fn move_windows_to_workspace(
    windows: &[WinID],
    workspace_id: WorkspaceId,
) -> Result<()> {
    if windows.is_empty() {
        return Ok(());
    }
    MainThreadMarker::new().ok_or_else(|| {
        Error::Generic("native workspace moves must run on the main thread".to_string())
    })?;

    let perform = perform_operation().ok_or_else(|| {
        Error::NotFound(
            "the SIP-safe SkyLight native-workspace operation is unavailable on this macOS build"
                .to_string(),
        )
    })?;
    let class = AnyClass::get(MOVE_OPERATION_CLASS).ok_or_else(|| {
        Error::NotFound(format!(
            "Objective-C class {} is unavailable",
            MOVE_OPERATION_CLASS.to_string_lossy()
        ))
    })?;
    if !class.responds_to(sel!(initWithWindows:spaceID:)) {
        return Err(Error::NotFound(
            "the native-workspace move initializer is unavailable".to_string(),
        ));
    }
    let window_numbers = create_array(windows, CFNumberType::SInt32Type)?;

    let operation: Option<Retained<AnyObject>> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        msg_send![allocated, initWithWindows: &*window_numbers, spaceID: workspace_id]
    };
    let operation = operation.ok_or_else(|| {
        Error::Generic("the native-workspace move operation failed to initialize".to_string())
    })?;
    let result = unsafe { perform(Retained::as_ptr(&operation).cast_mut().cast()) };
    trace!(
        ?windows,
        workspace_id, result, "submitted native workspace move"
    );
    Ok(())
}

/// Returns the native Spaces containing `window_id`.
pub(super) fn window_workspaces(connection: ConnID, window_id: WinID) -> Result<Vec<WorkspaceId>> {
    MainThreadMarker::new().ok_or_else(|| {
        Error::Generic("native workspace queries must run on the main thread".to_string())
    })?;
    let windows = create_array(&[window_id], CFNumberType::SInt32Type)?;
    let spaces = NonNull::new(unsafe {
        super::skylight::SLSCopySpacesForWindows(connection, 0x7, &windows)
    })
    .map(|spaces| unsafe { CFRetained::from_raw(spaces) })
    .ok_or_else(|| {
        Error::NotFound(format!(
            "could not find a native workspace for window {window_id}"
        ))
    })?;

    Ok(spaces
        .iter()
        .filter_map(|space| {
            space
                .as_i64()
                .and_then(|value| WorkspaceId::try_from(value).ok())
        })
        .collect())
}

fn workspace_switch_gesture(current_index: usize, target_index: usize) -> Option<(f64, usize)> {
    let direction = match target_index.cmp(&current_index) {
        Ordering::Equal => return None,
        Ordering::Greater => 1.0,
        Ordering::Less => -1.0,
    };
    Some((direction, current_index.abs_diff(target_index)))
}
/// Posts the private Dock swipe gesture used by macOS to switch Spaces.
///
/// Derived from yabai's SIP-enabled `space --focus` implementation and the
/// `InstantSpaceSwitcher` technique that it credits:
/// <https://github.com/asmvik/yabai/blob/master/src/space_manager.c>
/// <https://github.com/jurplel/InstantSpaceSwitcher>
pub(super) fn post_workspace_switch_gesture(
    current_index: usize,
    target_index: usize,
) -> Result<bool> {
    MainThreadMarker::new().ok_or_else(|| {
        Error::Generic("native workspace focus must run on the main thread".to_string())
    })?;

    let Some((direction, count)) = workspace_switch_gesture(current_index, target_index) else {
        return Ok(false);
    };
    let gesture = CGEvent::new(None)
        .ok_or_else(|| Error::Generic("could not create Dock gesture event".to_string()))?;

    CGEvent::set_integer_value_field(Some(&gesture), GESTURE_EVENT_TYPE, GESTURE_EVENT_TYPE_VALUE);
    CGEvent::set_integer_value_field(
        Some(&gesture),
        GESTURE_HID_TYPE,
        GESTURE_HID_TYPE_DOCK_SWIPE,
    );
    CGEvent::set_integer_value_field(
        Some(&gesture),
        GESTURE_SWIPE_MOTION,
        GESTURE_SWIPE_MOTION_HORIZONTAL,
    );
    CGEvent::set_double_value_field(Some(&gesture), GESTURE_SWIPE_PROGRESS, direction);
    CGEvent::set_double_value_field(
        Some(&gesture),
        GESTURE_VELOCITY_X,
        direction * GESTURE_INSTANT_VELOCITY,
    );

    for _ in 0..count {
        CGEvent::set_integer_value_field(Some(&gesture), GESTURE_PHASE, GESTURE_PHASE_BEGAN);
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&gesture));
        CGEvent::set_integer_value_field(Some(&gesture), GESTURE_PHASE, GESTURE_PHASE_ENDED);
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&gesture));
    }

    trace!(current_index, target_index, "posted instant Space switch");
    Ok(true)
}

/// Posts a left click at `point` to complete activation of an empty Space on
/// another display.
pub(super) fn post_left_click(point: CGPoint) -> Result<()> {
    MainThreadMarker::new()
        .ok_or_else(|| Error::Generic("display focus must run on the main thread".to_string()))?;
    for event_type in [CGEventType::LeftMouseDown, CGEventType::LeftMouseUp] {
        let event = CGEvent::new_mouse_event(None, event_type, point, CGMouseButton::Left)
            .ok_or_else(|| Error::Generic("could not create display focus event".to_string()))?;
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_parameters_cover_direction_and_distance() {
        assert_eq!(workspace_switch_gesture(1, 4), Some((1.0, 3)));
        assert_eq!(workspace_switch_gesture(4, 1), Some((-1.0, 3)));
        assert_eq!(workspace_switch_gesture(2, 2), None);
    }

    #[test]
    #[ignore = "depends on the private SkyLight implementation in the host macOS build"]
    fn sip_safe_workspace_move_capability_is_available() {
        assert!(perform_operation().is_some());
        let class = AnyClass::get(MOVE_OPERATION_CLASS).expect("move operation class");
        assert!(class.responds_to(sel!(initWithWindows:spaceID:)));
    }
}

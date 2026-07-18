use std::ffi::c_void;
use std::sync::OnceLock;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_core_foundation::CFNumberType;
use tracing::{debug, trace};

use crate::errors::{Error, Result};
use crate::platform::{WinID, WorkspaceId};
use crate::util::create_array;

use super::macho::find_loaded_symbol;

const SKYLIGHT_IMAGE: &std::ffi::CStr =
    c"/System/Library/PrivateFrameworks/SkyLight.framework/Versions/A/SkyLight";
const BRIDGED_OPERATION_SYMBOL: &std::ffi::CStr = c"__ZL54SLSPerformAsynchronousBridgedWindowManagementOperationP47SLSAsynchronousBridgedWindowManagementOperation";
const MOVE_OPERATION_CLASS: &std::ffi::CStr = c"SLSBridgedMoveWindowsToManagedSpaceOperation";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "depends on the private SkyLight implementation in the host macOS build"]
    fn sip_safe_workspace_move_capability_is_available() {
        assert!(perform_operation().is_some());
        let class = AnyClass::get(MOVE_OPERATION_CLASS).expect("move operation class");
        assert!(class.responds_to(sel!(initWithWindows:spaceID:)));
    }
}

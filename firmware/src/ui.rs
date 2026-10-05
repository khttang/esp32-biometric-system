//! Safe wrappers around the control panel in `biometrics_wrapper.cpp` (status line, admin view).
//!
//! The C++ side owns the LVGL widgets and only reports touches as events; what they mean is
//! decided by the state machine in `biometrics.rs`, which calls these from the main thread.

use std::ffi::{CStr, CString};
use std::sync::Arc;

use biometric_core::matching::GroupMember;

use crate::ffi;

/// Wait for the LVGL lock as long as it takes: panel updates are rare and must not be lost.
const LOCK_WAIT_FOREVER: u32 = 0;

#[derive(Debug, PartialEq, Eq)]
pub enum UiEvent {
    /// "Admin" pressed in the idle view.
    Admin,
    /// "Enroll" pressed; `name` is the (possibly empty) content of the name field.
    Enroll { name: String },
    /// "Delete" pressed; `index` refers to the list last passed to [`set_members`].
    Delete { index: usize },
    /// "Done" pressed.
    Exit,
}

/// Returns the oldest pending touch event, if any. Never blocks.
pub fn poll_event() -> Option<UiEvent> {
    let mut raw = ffi::p4_ui_event_t::default();
    // Safety: `raw` is a valid out-parameter; the call does not block.
    if !unsafe { ffi::p4_ui_poll_event(&mut raw) } {
        return None;
    }
    match u32::from(raw.kind) {
        ffi::P4_UI_EVENT_ADMIN => Some(UiEvent::Admin),
        ffi::P4_UI_EVENT_ENROLL => {
            // The C side always NUL-terminates `name`; fall back to "" if it somehow didn't.
            let name = CStr::from_bytes_until_nul(&raw.name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            Some(UiEvent::Enroll { name })
        }
        ffi::P4_UI_EVENT_DELETE => Some(UiEvent::Delete {
            index: usize::from(raw.selected),
        }),
        ffi::P4_UI_EVENT_EXIT => Some(UiEvent::Exit),
        _ => None,
    }
}

pub fn set_status(text: &str) {
    let text = to_c_text(text);
    // Safety: `text` is a valid C string for the duration of the call; LVGL copies it.
    unsafe { ffi::p4_ui_set_status(text.as_ptr(), LOCK_WAIT_FOREVER) };
}

pub fn set_admin_mode(enabled: bool) {
    // Safety: plain call; takes the LVGL lock internally.
    unsafe { ffi::p4_ui_set_admin_mode(enabled, LOCK_WAIT_FOREVER) };
}

/// Replaces the member list shown in the admin view; "Delete" reports an index into `members`.
pub fn set_members(members: &[Arc<GroupMember>]) {
    let mut list = String::new();
    for (index, member) in members.iter().enumerate() {
        if index > 0 {
            list.push('\n');
        }
        // One line per member: a line break inside a name would shift every later index.
        list.extend(
            member
                .name
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c }),
        );
    }
    let list = to_c_text(&list);
    // Safety: `list` is a valid C string for the duration of the call; LVGL copies it.
    unsafe { ffi::p4_ui_set_members(list.as_ptr(), members.len(), LOCK_WAIT_FOREVER) };
}

/// `text` as a C string, cut at the first NUL (which our own strings never contain).
fn to_c_text(text: &str) -> CString {
    let end = text.find('\0').unwrap_or(text.len());
    CString::new(&text[..end]).expect("no interior NUL before `end`")
}

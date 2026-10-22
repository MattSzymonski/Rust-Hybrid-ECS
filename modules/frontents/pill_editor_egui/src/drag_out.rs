//! Dragging a project asset out to the OS (W-0038 Stage 19).
//!
//! # Responsibilities
//!
//! - [`start`]: hand one project asset to OLE as a file drag, so it can be
//!   dropped into Explorer, another editor, or any `CF_HDROP` target.
//! - [`EditorDropSource`]: the drop source that ends the drag when the mouse
//!   button comes up and cancels it on Escape.
//!
//! # Design
//!
//! The shell cannot do this: a drag is `DoDragDrop`, which runs a modal loop on
//! the thread that starts it, and only this executable names the `windows`
//! crate (the shared-dylib rule). The shell therefore reports the asset's path
//! through the frame it already hands back, and this module starts the drag
//! from the frame boundary.
//!
//! The data object is the shell's, not one written here:
//! `SHCreateDataObject` wraps a PIDL into an `IDataObject` that offers the
//! file under `CF_HDROP`, which is the format every drop target understands.
//! That keeps this module to the parts only the editor can supply - the file
//! list and the drop source's lifetime rules.
//!
//! The drop source's policy is "when in doubt, end the drag": it reports a
//! drop as soon as the left button is not held, and a cancel on Escape. A drag
//! source that answered "continue" on a button it could not read would leave
//! the modal loop spinning with the editor's frame frozen.
//!
//! The same rule applies before the loop: `DoDragDrop` only asks its drop
//! source for a verdict when input arrives, so a drag started with no button
//! held would wait forever. [`start`] therefore reads the button itself first
//! and reports [`DragOutOutcome::NoButtonHeld`] instead of entering the loop.

// External crates
use windows::core::implement;
use windows::Win32::Foundation::{BOOL, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, S_OK};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, DROPEFFECT, DROPEFFECT_COPY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::{SHCreateDataObject, SHParseDisplayName};

/// The drop source OLE calls while the drag runs.
#[implement(IDropSource)]
pub struct EditorDropSource;

impl IDropSource_Impl for EditorDropSource_Impl {
    /// Whether the drag goes on, ends in a drop, or is cancelled.
    ///
    /// `fescapepressed` is OLE's own reading of the keyboard; the button state
    /// is this implementation's to read, and an unreadable button ends the
    /// drag rather than spinning the loop.
    fn QueryContinueDrag(
        &self,
        fescapepressed: BOOL,
        _grfkeystate: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        // SAFETY: `GetAsyncKeyState` takes a virtual-key code and cannot fail;
        // the high bit of the result is the key's held state.
        let held = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & 0x8000 != 0;
        if held {
            S_OK
        } else {
            DRAGDROP_S_DROP
        }
    }

    /// The pointer feedback OLE draws from; the target's own effect is right.
    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> windows::core::HRESULT {
        S_OK
    }
}

/// How a drag out ended, for the caller's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragOutOutcome {
    /// The drop target took the file under this effect.
    Dropped(DROPEFFECT),
    /// The drag ended with no target taking the file.
    Cancelled,
    /// The pointer button was not held, so no drag was started.
    NoButtonHeld,
    /// The drag could not be prepared, or OLE refused to run it.
    Failed,
}

/// Whether the left button is held right now.
///
/// # Safety
///
/// `GetAsyncKeyState` takes a virtual-key code and cannot fail; the top bit of
/// its result is the key's held state.
fn left_button_held() -> bool {
    // SAFETY: see the function's contract - a virtual-key code, no pointers.
    (unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16) & 0x8000 != 0
}

/// Hand `absolute_path` to OLE as a file drag; blocks until the drag ends.
///
/// Logs `[editor] drag out prepared` once the data object exists, which is the
/// point after which the drag is the OS's; the caller logs the outcome's line.
/// A drag started with no button held is refused rather than run, because the
/// modal loop would never see the input that ends it.
pub fn start(absolute_path: &str) -> DragOutOutcome {
    if !initialize_ole() {
        return DragOutOutcome::Failed;
    }
    let wide: Vec<u16> = absolute_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut pidl = std::ptr::null_mut();
    // SAFETY: the name is a NUL-terminated UTF-16 buffer that outlives the
    // call and the PIDL is written into `pidl`, which is freed below.
    if let Err(error) = unsafe {
        SHParseDisplayName(
            windows::core::PCWSTR(wide.as_ptr()),
            None,
            &mut pidl,
            0,
            None,
        )
    } {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            path = absolute_path,
            reason = %error,
            "[editor] drag out target not resolvable"
        );
        return DragOutOutcome::Failed;
    }
    // The shell's data object over that one PIDL: `CF_HDROP` for every target
    // that takes files, which is what a drag out of a project panel needs.
    let data_object: windows::core::Result<windows::Win32::System::Com::IDataObject> =
        // SAFETY: `pidl` is the item list just parsed and stays alive for the
        // whole call; the data object copies what it needs.
        unsafe { SHCreateDataObject(None, Some(&[pidl as *const _]), None) };
    let outcome = match data_object {
        Ok(data_object) => {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                path = absolute_path,
                "[editor] drag out prepared"
            );
            // `DoDragDrop` only asks its drop source for a verdict when input
            // arrives; with no button held, none ever would.
            if !left_button_held() {
                DragOutOutcome::NoButtonHeld
            } else {
                drag(data_object)
            }
        }
        Err(error) => {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                path = absolute_path,
                reason = %error,
                "[editor] drag out data object refused"
            );
            DragOutOutcome::Failed
        }
    };
    // SAFETY: `pidl` came from `SHParseDisplayName`, which allocates it with
    // the COM task allocator; it is freed exactly once here.
    unsafe { CoTaskMemFree(Some(pidl as *const core::ffi::c_void)) };
    outcome
}

/// Run the modal drag over one prepared data object.
///
/// Blocks on OLE's loop until the drop source ends it; the effect OLE reports
/// is the target's, and zero means nothing took the file.
fn drag(data_object: windows::Win32::System::Com::IDataObject) -> DragOutOutcome {
    let source: IDropSource = EditorDropSource.into();
    let mut effect = DROPEFFECT(0);
    // SAFETY: both interfaces are live for the call, and `effect` is a valid
    // out-pointer; `DoDragDrop` runs its own modal loop and returns when the
    // drag ends.
    let result = unsafe {
        DoDragDrop(
            &data_object,
            &source,
            DROPEFFECT_COPY,
            &mut effect as *mut DROPEFFECT,
        )
    };
    if result.is_err() {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            reason = %windows::core::Error::from_hresult(result),
            "[editor] drag out refused by OLE"
        );
        DragOutOutcome::Failed
    } else if effect.0 == 0 {
        // A zero effect is a cancelled drag: the target refused, or the
        // pointer came up over nothing that takes files.
        DragOutOutcome::Cancelled
    } else {
        DragOutOutcome::Dropped(effect)
    }
}

/// Initialise OLE on the calling thread, once.
///
/// `DoDragDrop` needs an OLE-initialised thread; the editor's main thread is
/// the one that owns the window, so that is where this runs. A failure leaves
/// drag-out unavailable and is logged once.
fn initialize_ole() -> bool {
    use std::sync::OnceLock;
    static INITIALISED: OnceLock<bool> = OnceLock::new();
    *INITIALISED.get_or_init(|| {
        // SAFETY: `OleInitialize` takes a reserved pointer that must be null.
        match unsafe { OleInitialize(None) } {
            Ok(()) => true,
            Err(error) => {
                // S_FALSE (already initialised) arrives as Ok; a real failure
                // is what lands here.
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    reason = %error,
                    "[editor] drag out needs OLE, which this thread refused"
                );
                false
            }
        }
    })
}

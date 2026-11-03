//! Writes the editor's payload to the OS clipboard and reads text back from it.
//!
//! # Responsibilities
//!
//! - [`write`]: put text on the clipboard twice, once in the editor's own
//!   clipboard format and once as plain text, so another editor instance or a
//!   text editor can read it.
//! - [`read`]: return the text the clipboard carries, preferring the editor's
//!   own format and falling back to plain text.
//!
//! # Design
//!
//! The Win32 code lives here because only this executable names the `windows`
//! crate. A shared dylib must never carry that crate's large `raw-dylib` import
//! set. The shell does not touch the OS clipboard. It queues a
//! `ClipboardRequest` on its `EditorClipboard` resource, and the runner carries
//! the request out here at the frame boundary. That way no OS clipboard handle
//! crosses the reload boundary.
//!
//! The write path and the read path both close the clipboard on every path,
//! because a clipboard left open blocks every other process.
//!
//! A read is bounded by the size the clipboard reports for the block
//! (`GlobalSize`), never by the terminator the producer was supposed to write.
//! Any process can set the registered format. A block with no NUL inside its own
//! size is therefore refused and logged, instead of being read past its end into
//! this process's heap.

// External crates
use pill_core::telemetry::telemetry_target;
use pill_core::warn;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Threading::GetCurrentProcessId;

// Current crate
use pill_editor_core::clipboard::FORMAT_NAME;

// =============================================================================
// Constants
// =============================================================================

/// `CF_UNICODETEXT`: the plain-text clipboard format that any application can
/// read.
const CF_UNICODETEXT: u32 = 13;

/// How many attempts a read makes to open a busy clipboard before it reports
/// failure.
const READ_ATTEMPTS: usize = 10;

/// The pause between clipboard read attempts.
const READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

// =============================================================================
// Free Functions
// =============================================================================

/// Returns the clipboard format id the editor registers its copied text under.
///
/// Registering the name is idempotent, so every call returns the same id for
/// the life of the process and nothing needs caching. `None` means Windows
/// refused the name.
fn payload_format_id() -> Option<u32> {
    let name: Vec<u16> = FORMAT_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the call,
    // which is what `RegisterClipboardFormatW` requires. A zero return means
    // Windows refused the name, reported here as `None`.
    let id = unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) };
    (id != 0).then_some(id)
}

/// Puts `text` on the clipboard, in the editor's own format and as plain text.
///
/// Once the clipboard is open it is emptied before the first allocation, so a
/// later failure leaves it empty rather than holding what it held before.
///
/// # Errors
///
/// Returns a message naming the step that failed: registering the editor's
/// format, opening the clipboard, emptying it, allocating the clipboard block,
/// or handing that block to the clipboard.
pub fn write(text: &str) -> Result<(), String> {
    // Step 1: find the editor's own clipboard format and open the clipboard
    // for this process.
    let format = payload_format_id().ok_or_else(|| "the payload format was refused".to_string())?;
    let owner_window = HWND::default();
    // SAFETY: `owner_window` is a null handle, which the API documents as
    // associating the clipboard with the calling process rather than a window.
    unsafe { OpenClipboard(owner_window) }.map_err(|error| format!("open clipboard: {error}"))?;
    // Step 2: write both copies while it is open, then close it whatever the
    // write did.
    let write_open_result = write_open(text, format);
    // SAFETY: the clipboard is open exactly once here, after the write path
    // above opened it; closing it is unconditional so a failure above cannot
    // leave it locked.
    let _ = unsafe { CloseClipboard() };
    write_open_result
}

/// The write path with the clipboard already open.
///
/// # Errors
///
/// Returns a message naming the failed step: emptying the clipboard, or one of
/// the two copies [`set_one`] makes.
fn write_open(text: &str, format: u32) -> Result<(), String> {
    // SAFETY: the clipboard is open (the caller holds it), which is the
    // documented precondition for emptying it.
    unsafe { EmptyClipboard() }.map_err(|error| format!("empty clipboard: {error}"))?;
    // The registered format goes first, so a reader that understands it gets
    // the editor's payload. A reader that does not falls back to the plain text
    // written below.
    set_one(text, format)?;
    set_one(text, CF_UNICODETEXT)?;
    Ok(())
}

/// Puts one UTF-16 copy of `text` on the clipboard under `format`.
///
/// The memory is allocated with `GMEM_MOVEABLE` and handed to the clipboard,
/// which takes ownership on success. A failed `SetClipboardData` leaves the
/// block with this function, which frees it with `GlobalFree`.
///
/// # Errors
///
/// Returns a message when the block cannot be allocated, when it cannot be
/// locked, or when the clipboard refuses it.
fn set_one(text: &str, format: u32) -> Result<(), String> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);
    let bytes = units.len() * std::mem::size_of::<u16>();
    // SAFETY: `GMEM_MOVEABLE` with a byte count is the documented allocation
    // for clipboard data; a null return means the allocation failed.
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
        .map_err(|error| format!("clipboard allocation: {error}"))?;
    // SAFETY: `handle` is the allocation just made, and the lock is taken once
    // so the buffer can be filled before the data is handed over.
    let target = unsafe { GlobalLock(HGLOBAL(handle.0)) };
    if target.is_null() {
        // SAFETY: `handle` is an allocation this call owns and no other
        // reference to it exists.
        let _ = unsafe { GlobalFree(handle) };
        return Err("clipboard allocation could not be locked".to_string());
    }
    // SAFETY: the destination is the locked allocation, `bytes` long, and the
    // source is `units`, a live buffer of exactly that size.
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), target.cast::<u16>(), units.len()) };
    // SAFETY: the lock was taken above on this handle and is released once.
    let _ = unsafe { GlobalUnlock(HGLOBAL(handle.0)) };
    // SAFETY: the clipboard is open (the caller holds it); on success it takes
    // ownership of `handle`, so this call must not free it afterwards.
    match unsafe { SetClipboardData(format, windows::Win32::Foundation::HANDLE(handle.0)) } {
        Ok(_) => Ok(()),
        Err(error) => {
            // SAFETY: `handle` is still this call's allocation because the
            // clipboard refused it.
            let _ = unsafe { GlobalFree(handle) };
            Err(format!("clipboard data: {error}"))
        }
    }
}

/// Returns the text the clipboard carries, preferring the editor's own format.
///
/// `Ok(None)` means the clipboard held nothing usable; the runner then shows an
/// empty-clipboard notice instead of pasting.
///
/// The clipboard is a lock shared by every process, so `OpenClipboard` can
/// fail simply because another application holds it at that moment. The
/// attempt is retried up to `READ_ATTEMPTS` times, `READ_RETRY_DELAY` apart,
/// and a clipboard that never frees is an error rather than an empty paste.
///
/// # Errors
///
/// Returns the message from the last failed attempt when the clipboard cannot
/// be opened within the retries.
pub fn read() -> Result<Option<String>, String> {
    // Step 1: try to open the clipboard, pausing a little between attempts.
    let owner_window = HWND::default();
    let mut last_error = String::new();
    for attempt in 0..READ_ATTEMPTS {
        // SAFETY: `owner_window` is a null handle, the documented form for
        // reading the clipboard from a process without a window that owns the
        // data.
        match unsafe { OpenClipboard(owner_window) } {
            Ok(()) => {
                // Step 2: read while it is open, then close it on every path.
                let read_open_result = read_open();
                // SAFETY: the clipboard is open exactly once here and is closed
                // unconditionally, so a read failure cannot leave it locked.
                let _ = unsafe { CloseClipboard() };
                return read_open_result;
            }
            Err(error) => {
                last_error = format!("open clipboard: {error}");
                if attempt + 1 < READ_ATTEMPTS {
                    std::thread::sleep(READ_RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error)
}

/// The read path with the clipboard already open.
///
/// The formats are tried in order: the editor's own format first, then plain
/// text. A format that is unavailable, a block that cannot be locked and a
/// block with no terminator all move on to the next format. The result is the
/// first text that could be read, or `None`.
///
/// # Errors
///
/// No branch returns an error: the signature carries the type [`read`] passes
/// on, but every failure here ends in `Ok`.
fn read_open() -> Result<Option<String>, String> {
    let mut format_ids = Vec::new();
    if let Some(format) = payload_format_id() {
        format_ids.push(format);
    }
    format_ids.push(CF_UNICODETEXT);
    for format in format_ids {
        // SAFETY: the clipboard is open (the caller holds it); the query only
        // reads the format list.
        if unsafe { IsClipboardFormatAvailable(format) }.is_err() {
            continue;
        }
        // SAFETY: the format was just reported available while the clipboard
        // is open, which is the documented precondition for fetching it.
        let handle = match unsafe { GetClipboardData(format) } {
            Ok(handle) => handle,
            Err(_) => continue,
        };
        if handle.is_invalid() {
            continue;
        }
        // SAFETY: `handle` is the clipboard's data block for an available
        // format; it stays valid until the clipboard changes, and it is not
        // freed here - the clipboard owns it.
        let block_pointer = unsafe { GlobalLock(HGLOBAL(handle.0)) }.cast::<u16>();
        if block_pointer.is_null() {
            continue;
        }
        // Read only as far as the block's own size allows. The terminator is
        // something the producer is supposed to write, not a property the block
        // guarantees. Any process can set the registered format, so a block with
        // no NUL inside its size must not be followed out of the allocation.
        // SAFETY: `handle` is the clipboard's data block; the query only reads
        // the allocation's size, and neither frees nor unlocks it.
        let bytes = unsafe { GlobalSize(HGLOBAL(handle.0)) };
        let unit_capacity = bytes / std::mem::size_of::<u16>();
        let mut unit_count = 0usize;
        // SAFETY: `block_pointer` addresses the locked block of `unit_capacity`
        // UTF-16 units, so every index below `unit_capacity` is inside the
        // allocation.
        while unit_count < unit_capacity && unsafe { *block_pointer.add(unit_count) } != 0 {
            unit_count += 1;
        }
        let unterminated = unit_count == unit_capacity;
        // SAFETY: `block_pointer` is the locked block and `unit_count` counts
        // the UTF-16 units before its terminator, so the slice stays inside the
        // block.
        let slice = unsafe { std::slice::from_raw_parts(block_pointer, unit_count) };
        let text = String::from_utf16_lossy(slice);
        // SAFETY: the lock was taken above on this handle and is released once.
        let _ = unsafe { GlobalUnlock(HGLOBAL(handle.0)) };
        if unterminated {
            warn!(
                target: telemetry_target::ENGINE,
                bytes = bytes,
                "[editor] clipboard refused: the block carries no terminator"
            );
            continue;
        }
        if !text.is_empty() {
            return Ok(Some(text));
        }
    }
    Ok(None)
}

/// Returns the id of this process, which the clipboard log lines carry.
///
/// The write and paste lines name it as `writer =` and `reader =`, so a test
/// that runs more than one editor can tell which process did each step.
pub fn process_id() -> u32 {
    // SAFETY: `GetCurrentProcessId` takes no arguments and cannot fail.
    unsafe { GetCurrentProcessId() }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The clipboard is one resource for the whole process, so the tests below
    /// take turns. A `CloseClipboard` on one thread ends an `OpenClipboard` that
    /// another thread made.
    static CLIPBOARD_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Waits until no other test is using the clipboard, and keeps it until the
    /// returned guard drops.
    fn clipboard_turn() -> std::sync::MutexGuard<'static, ()> {
        CLIPBOARD_TURN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Puts `units` UTF-16 `A`s on the clipboard under the editor's format,
    /// with no terminator - the block shape a foreign application can hand
    /// over.
    fn set_unterminated_block(units: usize) {
        let format = payload_format_id().expect("the payload format registers");
        // SAFETY: `HWND::default()` is the null owner, the documented form for
        // a process without a clipboard-owning window.
        unsafe { OpenClipboard(HWND::default()) }.expect("the clipboard opens");
        // SAFETY: the clipboard is open and this call owns it.
        unsafe { EmptyClipboard() }.expect("the clipboard empties");
        let bytes = units * std::mem::size_of::<u16>();
        // SAFETY: `GMEM_MOVEABLE` with a byte count is the documented
        // allocation for clipboard data; a null return fails the test.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.expect("the block allocates");
        // SAFETY: `handle` is the allocation just made, locked once so it can
        // be filled.
        let target = unsafe { GlobalLock(HGLOBAL(handle.0)) }.cast::<u16>();
        assert!(!target.is_null(), "the block locks");
        for unit in 0..units {
            // SAFETY: `unit` is inside the block just allocated and locked.
            unsafe { *target.add(unit) = u16::from(b'A') };
        }
        // SAFETY: the lock was taken above on this handle and is released once.
        let _ = unsafe { GlobalUnlock(HGLOBAL(handle.0)) };
        // SAFETY: the clipboard is open and takes ownership of the block.
        unsafe { SetClipboardData(format, windows::Win32::Foundation::HANDLE(handle.0)) }
            .expect("the block is accepted");
        // SAFETY: the clipboard is open exactly once here.
        let _ = unsafe { CloseClipboard() };
    }

    #[test]
    fn an_unterminated_block_is_refused_rather_than_walked_past() {
        let _turn = clipboard_turn();
        const UNITS: usize = 4096;
        set_unterminated_block(UNITS);
        match read() {
            Ok(None) => {}
            Ok(Some(text)) => panic!(
                "a block of {UNITS} units with no terminator was read as {} units",
                text.chars().count()
            ),
            Err(message) => panic!("the read reported '{message}' instead of refusing the block"),
        }
    }

    #[test]
    fn a_terminated_block_is_read_back_whole() {
        let _turn = clipboard_turn();
        write("a payload the editor wrote itself").expect("the clipboard write succeeds");
        match read() {
            Ok(Some(text)) => assert_eq!(text, "a payload the editor wrote itself"),
            Ok(None) => panic!("the block just written was not found"),
            Err(message) => panic!("the read reported '{message}'"),
        }
    }
}

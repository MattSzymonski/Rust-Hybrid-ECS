//! The OS clipboard bridge.
//!
//! # Responsibilities
//!
//! - [`write`]: put one payload text on the clipboard under the editor's
//!   registered format and as plain text, so another editor instance (or a
//!   text editor) can read it.
//! - [`read`]: the text the clipboard carries, preferring the editor's
//!   payload format and falling back to plain text.
//!
//! # Design
//!
//! The Win32 half lives here because only this executable names the `windows`
//! crate: a shared dylib must never carry its large `raw-dylib` import set.
//! The shell queues a `ClipboardRequest` on its `EditorClipboard` resource and
//! the runner carries it out here at the frame boundary, so no OS clipboard
//! handle crosses the reload boundary. Both sides close the clipboard on every
//! path, because an open clipboard blocks every other process.
//!
//! A read is bounded by the block the clipboard reports (`GlobalSize`), never
//! by the producer's terminator: any process can set the registered format,
//! and a block with no NUL inside its own size is refused with a log line
//! rather than walked past into this process's heap.

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

/// `CF_UNICODETEXT`: the plain-text format the readable copy uses.
const CF_UNICODETEXT: u32 = 13;

/// The clipboard format id the editor's payloads are registered under.
///
/// Registering is idempotent per name, so every call returns the same id for
/// the life of the process and nothing needs caching.
fn payload_format() -> Option<u32> {
    let name: Vec<u16> = FORMAT_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the call,
    // which is what `RegisterClipboardFormatW` requires. A zero return means
    // the format could not be registered, reported as `None` by the caller.
    let id = unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) };
    (id != 0).then_some(id)
}

/// Put `text` on the clipboard, under the editor's format and as plain text.
///
/// Returns an error naming the failed step; once the clipboard is open it is
/// emptied before the first allocation, so a later failure leaves it empty
/// rather than holding what it held before.
pub fn write(text: &str) -> Result<(), String> {
    let format = payload_format().ok_or_else(|| "the payload format was refused".to_string())?;
    let owner = HWND::default();
    // SAFETY: `owner` is a null handle, which asks the API to associate the
    // clipboard with the calling process - the documented form for a window
    // that does not own the data.
    unsafe { OpenClipboard(owner) }.map_err(|error| format!("open clipboard: {error}"))?;
    let outcome = write_open(text, format);
    // SAFETY: the clipboard is open exactly once here, after the write path
    // above opened it; closing it is unconditional so a failure above cannot
    // leave it locked.
    let _ = unsafe { CloseClipboard() };
    outcome
}

/// The write path with the clipboard already open.
fn write_open(text: &str, format: u32) -> Result<(), String> {
    // SAFETY: the clipboard is open (the caller holds it), which is the
    // documented precondition for emptying it.
    unsafe { EmptyClipboard() }.map_err(|error| format!("empty clipboard: {error}"))?;
    // The registered format first: a reader that understands it gets the
    // structured payload, one that does not falls back to the plain text below.
    set_one(text, format)?;
    set_one(text, CF_UNICODETEXT)?;
    Ok(())
}

/// Put one UTF-16 copy of `text` on the clipboard under `format`.
///
/// The memory is allocated with `GMEM_MOVEABLE` and handed to the clipboard,
/// which takes ownership on success; a failed `SetClipboardData` leaves the
/// block ours, and it is freed with `GlobalFree`.
fn set_one(text: &str, format: u32) -> Result<(), String> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();
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
    // source is `wide`'s live buffer of exactly that size.
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), target.cast::<u16>(), wide.len()) };
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

/// The text the clipboard carries, preferring the editor's payload format.
///
/// `Ok(None)` means the clipboard held nothing usable; the runner reports it
/// as an empty paste.
///
/// The clipboard is a process-global lock, so `OpenClipboard` can fail simply
/// because another application held it at that moment: the attempt is retried
/// a few times before the failure is reported, and a clipboard that never
/// frees is an error rather than an empty paste.
pub fn read() -> Result<Option<String>, String> {
    let owner = HWND::default();
    let mut last = String::new();
    for attempt in 0..READ_ATTEMPTS {
        // SAFETY: `owner` is a null handle, the documented form for reading the
        // clipboard from a process without a window that owns the data.
        match unsafe { OpenClipboard(owner) } {
            Ok(()) => {
                let outcome = read_open();
                // SAFETY: the clipboard is open exactly once here and is closed
                // unconditionally, so a read failure cannot leave it locked.
                let _ = unsafe { CloseClipboard() };
                return outcome;
            }
            Err(error) => {
                last = format!("open clipboard: {error}");
                if attempt + 1 < READ_ATTEMPTS {
                    std::thread::sleep(READ_RETRY_DELAY);
                }
            }
        }
    }
    Err(last)
}

/// How many times a busy clipboard is retried before the read reports failure.
const READ_ATTEMPTS: usize = 10;

/// The pause between clipboard read attempts.
const READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// The read path with the clipboard already open.
fn read_open() -> Result<Option<String>, String> {
    let mut formats = Vec::new();
    if let Some(format) = payload_format() {
        formats.push(format);
    }
    formats.push(CF_UNICODETEXT);
    for format in formats {
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
        let pointer = unsafe { GlobalLock(HGLOBAL(handle.0)) }.cast::<u16>();
        if pointer.is_null() {
            continue;
        }
        // The block's own size is the only bound the read can trust: the
        // terminator is a convention of the producer, not a property of the
        // block, so a foreign block with no NUL in it must not walk the read
        // out of the allocation.
        // SAFETY: `handle` is the clipboard's data block; the query only reads
        // the allocation's size, and neither frees nor unlocks it.
        let bytes = unsafe { GlobalSize(HGLOBAL(handle.0)) };
        let capacity = bytes / std::mem::size_of::<u16>();
        let mut length = 0usize;
        // SAFETY: `pointer` addresses the locked block of `capacity` UTF-16
        // units, so every index below `capacity` is inside the allocation.
        while length < capacity && unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        let unterminated = length == capacity;
        // SAFETY: `pointer` is the locked block and `length` counts the UTF-16
        // units before its terminator, so the slice stays inside the block.
        let slice = unsafe { std::slice::from_raw_parts(pointer, length) };
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

/// The clipboard owner tag the logs name, so a suite can tell which process
/// wrote what it read.
pub fn process_id() -> u32 {
    // SAFETY: `GetCurrentProcessId` takes no arguments and cannot fail.
    unsafe { GetCurrentProcessId() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clipboard is one process-wide resource, so the tests below take
    /// turns: an `OpenClipboard` on one thread is closed by a `CloseClipboard`
    /// on another.
    static CLIPBOARD_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serialise one test's use of the clipboard for its whole body.
    fn clipboard_turn() -> std::sync::MutexGuard<'static, ()> {
        CLIPBOARD_TURN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Put `units` UTF-16 `A`s on the clipboard under the payload format with
    /// no terminator - the block shape a foreign application can hand over.
    fn set_unterminated_block(units: usize) {
        let format = payload_format().expect("the payload format registers");
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

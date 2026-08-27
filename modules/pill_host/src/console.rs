//! ANSI console helpers for the host's hot-reload log.
//
//   Colors are opt-in and disabled automatically when stdout is not a
//   terminal, so the benchmark harness (which pipes the host's stdout and
//   parses the `[analytics]` lines) always sees plain text. `PILL_ANSI=1`
//   forces colors on (useful for `cargo run 2>&1 | Tee-Object`); `NO_COLOR`
//   always wins.
//
//   The host deliberately does not depend on the `colored` crate: pulling a
//   Windows console-mode crate in here could change feature unification with
//   `pill_core` and split the shared DLL. Instead ANSI escape codes are
//   emitted directly, and VT processing is enabled with a tiny raw `kernel32`
//   FFI (no new crate, no feature changes).
//
// --- SCRIPT ---

use std::io::IsTerminal;

/// Whether ANSI colors should be emitted. The Windows console's VT mode is
/// enabled on the first time this returns true, so legacy conhost renders
/// the codes as well as Windows Terminal.
fn ansi_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    let enabled = std::env::var_os("PILL_ANSI").is_some_and(|value| value == "1")
        || std::io::stdout().is_terminal();
    if enabled {
        enable_windows_vt();
    }
    enabled
}

/// Turn on ENABLE_VIRTUAL_TERMINAL_PROCESSING for the console once. A no-op
/// when stdout is a pipe or the mode query fails, so it never affects piped
/// (harness) output.
#[cfg(windows)]
fn enable_windows_vt() {
    use std::os::raw::c_void;
    use std::sync::Once;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(handle: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    }

    // STD_OUTPUT_HANDLE is -11; ENABLE_VIRTUAL_TERMINAL_PROCESSING is 0x0004.
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    static ENABLED: Once = Once::new();
    // SAFETY: The `kernel32` functions declared above are part of the stable
    // Win32 ABI and are called with matching argument types; the mode buffer
    // is a stack value that outlives the call. The call is best-effort - a
    // failure leaves the console in its previous mode.
    ENABLED.call_once(|| unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    });
}

#[cfg(not(windows))]
fn enable_windows_vt() {}

/// Wrap `text` in an ANSI SGR sequence, or return it unchanged when colors
/// are disabled. Either way the console is left reset afterwards.
pub(crate) fn paint(code: &str, text: &str) -> String {
    if ansi_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Bold white (the section tags and labels).
pub(crate) fn bold(text: &str) -> String {
    paint("1", text)
}

/// Bold cyan (reload/module names and the analytics tag).
pub(crate) fn bold_cyan(text: &str) -> String {
    paint("1;36", text)
}

/// Cyan (inline crate/module names).
pub(crate) fn cyan(text: &str) -> String {
    paint("36", text)
}

/// Dim (reload counters, separators, the crates pipe).
pub(crate) fn dim(text: &str) -> String {
    paint("2", text)
}

/// Yellow (measured values).
pub(crate) fn yellow(text: &str) -> String {
    paint("33", text)
}

/// Green (crate names in the rebuild list).
pub(crate) fn green(text: &str) -> String {
    paint("32", text)
}

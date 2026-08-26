//! Stage the toolchain's standard-library dylib next to `pill_standalone.exe`.
//!
//! The engine workspace links with `-C prefer-dynamic`, so this executable
//! imports `std-<hash>.dll` at process load. That dylib lives in the
//! toolchain's `lib/rustlib/<host>/lib` directory, which Windows does not
//! search; without it on `PATH` the process fails with
//! `STATUS_DLL_NOT_FOUND`. This build script copies it beside the executable
//! so the host runs without any `PATH` adjustment in development. All the
//! logic lives in the shared `pill_hot_scan` crate; this script only
//! calls it.

fn main() {
    // Copy `std-*.dll` next to the built executable (no-op off Windows).
    pill_hot_scan::stage_std_dylib();
}

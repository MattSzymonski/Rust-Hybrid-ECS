//! Stage the toolchain's standard-library dylib next to `pill_editor.exe`.
//!
//! # Responsibilities
//!
//! - Copy the toolchain's `std` dylib beside the executable.
//! - Include the shared staging code from `modules/build_support`; this script
//!   only wires it up.
//!
//! The engine workspace links with `-C prefer-dynamic`, so this executable
//! imports `std-<hash>.dll` at process load, and Windows does not search the
//! toolchain directory that holds it. Without this script the editor starts
//! only when another executable's build has already copied the dylib into the
//! shared target directory.

// The staging code shared by every executable's build script.
#[path = "../../../build_support/stage_std_dylib.rs"]
mod stage_std_dylib;

fn main() {
    // Copy `std-*.dll` next to the built executable (no-op off Windows).
    stage_std_dylib::stage_std_dylib();
}

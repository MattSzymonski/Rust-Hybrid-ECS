//! Stage the toolchain's standard-library dylib next to `pill_editor.exe`.
//!
//! # Responsibilities
//!
//! - Copy the toolchain's `std` dylib beside the executable.
//! - Delegate the staging logic to `pill_std_staging`; this script only wires it up.
//!
//! The engine workspace links with `-C prefer-dynamic`, so this executable
//! imports `std-<hash>.dll` at process load, and Windows does not search the
//! toolchain directory that holds it. Without this script the editor starts
//! only when another executable's build has already copied the dylib into the
//! shared target directory.

fn main() {
    // Copy `std-*.dll` next to the built executable (no-op off Windows).
    pill_std_staging::stage_std_dylib();
}

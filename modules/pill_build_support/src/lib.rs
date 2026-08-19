//! Shared build-script helpers for the Pill engine workspace.
//!
//! # Responsibilities
//!
//! - Stages the toolchain's standard-library dylib (`std-*.dll`) next to any
//!   binary that links with `-C prefer-dynamic`, so those executables run
//!   without the toolchain directory on `PATH`.
//!
//! # Design
//!
//! The engine workspace links its binaries with `-C prefer-dynamic` (see
//! `modules/.cargo/config.toml`), which makes them import `std-<hash>.dll` at
//! process load. That dylib lives in the toolchain's
//! `lib/rustlib/<host>/lib` directory, which Windows does not search, so the
//! process would otherwise fail with `STATUS_DLL_NOT_FOUND`. Copying it beside
//! the executable removes the `PATH` requirement for development. It is a
//! build-time staging step only: the dylib is a toolchain artifact, not a
//! checked-in file.

use std::path::{Path, PathBuf};
use std::process::Command;

// =============================================================================
// Public Entry Point
// =============================================================================

/// Stage the toolchain's std dylib next to the binary being built.
///
/// Call this from a `build.rs` `main`. It resolves the output directory from
/// `OUT_DIR`, so it must run as a build script. Failures degrade to
/// `cargo:warning` messages and never fail the build, and the copy is
/// idempotent. On non-Windows platforms this is a no-op.
pub fn stage_std_dylib() {
    // The std dylib is a Windows artifact; other platforms either find the
    // dynamic library through the executable's own directory or have no
    // dylib std at all.
    #[cfg(windows)]
    stage_std_dylib_windows();
}

// =============================================================================
// Windows Staging
// =============================================================================

/// Copy every `std-*.dll` from the active toolchain into the output directory.
///
/// Warnings instead of hard errors: a missing or moved toolchain should not
/// break the build, it should only leave the old `PATH` requirement in place.
#[cfg(windows)]
fn stage_std_dylib_windows() {
    // Step 1: Resolve the toolchain sysroot through the compiler cargo used,
    // so the copy follows toolchain switches instead of a hardcoded path.
    let sysroot = match rust_sysroot() {
        Ok(sysroot) => sysroot,
        Err(reason) => {
            println!("cargo:warning=std dylib not staged: {reason}");
            return;
        }
    };

    // Step 2: Build the sysroot path that holds the host's std dylib.
    let host = std::env::var("HOST").unwrap_or_else(|_| "x86_64-pc-windows-msvc".to_string());
    let std_lib_directory = PathBuf::from(&sysroot)
        .join("lib")
        .join("rustlib")
        .join(host)
        .join("lib");

    // Step 3: Resolve the output directory that will hold the executable.
    // `OUT_DIR` is `<target>/<profile>/build/<package>-<hash>/out`, so three
    // parents up is the same directory the built executable lands in.
    let Some(output_directory) =
        std::env::var_os("OUT_DIR")
            .map(PathBuf::from)
            .and_then(|out_directory| {
                out_directory
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .map(PathBuf::from)
            })
    else {
        println!("cargo:warning=std dylib not staged: cannot locate the output directory");
        return;
    };

    // Step 4: Copy each matching dylib beside the executable. A locked file
    // (the binary is running) simply skips with a warning; the next build
    // retries.
    let entries = match std::fs::read_dir(&std_lib_directory) {
        Ok(entries) => entries,
        Err(error) => {
            println!(
                "cargo:warning=std dylib not staged: cannot read {} ({error})",
                std_lib_directory.display()
            );
            return;
        }
    };

    let mut staged_count = 0usize;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with("std-") && file_name.ends_with(".dll") {
            let destination = output_directory.join(file_name);
            if std::fs::copy(entry.path(), &destination).is_ok() {
                staged_count += 1;
            }
        }
    }

    if staged_count == 0 {
        println!(
            "cargo:warning=std dylib not staged: no std-*.dll found in {}",
            std_lib_directory.display()
        );
    }
}

/// Run `rustc --print sysroot` and return the toolchain's root directory.
#[cfg(windows)]
fn rust_sysroot() -> Result<String, String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(&rustc)
        .arg("--print")
        .arg("sysroot")
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "`{rustc} --print sysroot` exited with {:?}",
            output.status
        ));
    }
    String::from_utf8(output.stdout)
        .map(|sysroot| sysroot.trim().to_string())
        .map_err(|error| error.to_string())
}

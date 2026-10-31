//! Stage the toolchain's standard-library dylib beside an executable.
//!
//! # Responsibilities
//!
//! - Copy every `std-*.dll` from the active toolchain into the directory the
//!   calling package's executable is built into.
//! - Degrade every failure to a `cargo:warning`, never a failed build.
//!
//! # Design
//!
//! The workspace links with `-C prefer-dynamic`, so every executable built from
//! it imports `std-<hash>.dll` at process load. That dylib lives in the
//! toolchain's `lib/rustlib/<host>/lib` directory, which Windows does not
//! search; without a copy beside the executable the process fails with
//! `STATUS_DLL_NOT_FOUND`. Each executable's build script calls
//! [`stage_std_dylib`], so no executable relies on another one having been
//! built first.
//!
//! The crate has no dependencies: build scripts are compiled before everything
//! else, in every posture, so anything pulled in here is paid on every clean
//! build - and a shipping build must not compile development code just to run
//! its build script.

// Standard library
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

// =============================================================================
// Staging
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
/// break the build, it should only leave the binary depending on the toolchain
/// being on `PATH`.
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

    // Counted separately so the warning can say which of the two things went
    // wrong. They are not the same problem: no candidates means the toolchain
    // layout is not what this expects, while a failed copy is almost always a
    // destination file locked by a running binary, which the next build fixes
    // by itself. Reporting the second as the first sends a reader looking for
    // a missing file that was there all along.
    let mut candidate_count = 0usize;
    let mut staged_count = 0usize;
    let mut last_error = String::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with("std-") && file_name.ends_with(".dll") {
            candidate_count += 1;
            let destination = output_directory.join(file_name);
            // An up-to-date copy is left alone. The running host maps this
            // file, so copying over it fails with a sharing violation on every
            // build even though there is nothing to update.
            if staged_copy_is_current(&entry.path(), &destination) {
                staged_count += 1;
                continue;
            }
            match std::fs::copy(entry.path(), &destination) {
                Ok(_) => staged_count += 1,
                Err(error) => last_error = error.to_string(),
            }
        }
    }

    if candidate_count == 0 {
        println!(
            "cargo:warning=std dylib not staged: no std-*.dll found in {}",
            std_lib_directory.display()
        );
    } else if staged_count == 0 {
        println!(
            "cargo:warning=std dylib not staged: found {candidate_count} in {}, but no copy into {} succeeded ({last_error}); a running binary usually holds the lock",
            std_lib_directory.display(),
            output_directory.display()
        );
    }
}

/// Whether `destination` already holds the same file as `source`.
///
/// `std::fs::copy` keeps the source's modification time on Windows, so a
/// staged copy of the same toolchain file has the same size and is no older.
/// A toolchain update changes the source's time, which makes the copy stale.
#[cfg(windows)]
fn staged_copy_is_current(source: &Path, destination: &Path) -> bool {
    let (Ok(source_metadata), Ok(destination_metadata)) =
        (std::fs::metadata(source), std::fs::metadata(destination))
    else {
        return false;
    };
    match (source_metadata.modified(), destination_metadata.modified()) {
        (Ok(source_time), Ok(destination_time)) => {
            source_metadata.len() == destination_metadata.len() && destination_time >= source_time
        }
        _ => false,
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

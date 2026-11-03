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
//! This is shared source for build scripts, not a crate of its own. Each
//! executable's `build.rs` includes it with
//! `#[path = "..."] mod stage_std_dylib;`, so the code exists once without a
//! package. It must therefore stay self-contained: the standard library only,
//! nothing from the crate that includes it. Cargo reruns each including build
//! script when this file changes, because rustc lists the file as an input.
//!
//! The workspace links with `-C prefer-dynamic`, so every executable built from
//! it imports `std-<hash>.dll` when the process starts. That dylib lives in the
//! toolchain's `lib/rustlib/<host>/lib` directory, which Windows does not
//! search. Without a copy beside the executable, the process fails with
//! `STATUS_DLL_NOT_FOUND`. Each executable stages its own copy, so none relies
//! on another one having been built first.

// Standard library
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

// =============================================================================
// Staging
// =============================================================================

/// Stages the toolchain's std dylib next to the binary being built.
///
/// Call this from a `build.rs` `main`: it finds the output directory through
/// `OUT_DIR`, so it only works inside a build script. A failure is reported as
/// a `cargo:warning`, never as a failed build, and running it twice is safe.
/// On platforms other than Windows it does nothing.
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

/// Copies every `std-*.dll` from the active toolchain into the output
/// directory.
///
/// A missing or moved toolchain produces a warning instead of a failed build.
/// The binary then needs the std dylib to be somewhere the loader searches;
/// the next build can still copy it into place.
#[cfg(windows)]
fn stage_std_dylib_windows() {
    // Step 1: Ask the compiler cargo used for its sysroot. This follows
    // toolchain switches instead of using a hardcoded path.
    let sysroot_path = match rust_sysroot() {
        Ok(sysroot_path) => sysroot_path,
        Err(error) => {
            println!("cargo:warning=std dylib not staged: {error}");
            return;
        }
    };

    // Step 2: Build the path to the directory that holds the host's std dylib.
    let host_triple =
        std::env::var("HOST").unwrap_or_else(|_| "x86_64-pc-windows-msvc".to_string());
    let std_library_directory = PathBuf::from(&sysroot_path)
        .join("lib")
        .join("rustlib")
        .join(host_triple)
        .join("lib");

    // Step 3: Find the output directory that will hold the executable.
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

    // Step 4: Copy each matching dylib beside the executable. A failed copy
    // is remembered rather than reported on the spot; if no copy succeeds at
    // all, the warning below names the last error. A running binary usually
    // holds the destination lock, and the next build tries again.
    let directory_entries = match std::fs::read_dir(&std_library_directory) {
        Ok(directory_entries) => directory_entries,
        Err(error) => {
            println!(
                "cargo:warning=std dylib not staged: cannot read {} ({error})",
                std_library_directory.display()
            );
            return;
        }
    };

    // The two counts stay separate so the warning can name the right problem.
    // No candidates means the toolchain layout is not what this expects. A
    // failed copy is almost always a file held open by a running binary, and
    // the next build fixes it. Reporting that as a missing file would send
    // the reader looking for something that was there all along.
    let mut candidate_count = 0usize;
    let mut staged_count = 0usize;
    let mut last_error = String::new();
    for entry in directory_entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if file_name.starts_with("std-") && file_name.ends_with(".dll") {
            candidate_count += 1;
            let destination_path = output_directory.join(file_name);
            // An up-to-date copy is left alone. The running host maps this
            // file, so copying over it fails with a sharing violation on every
            // build even though there is nothing to update.
            if staged_copy_is_current(&entry.path(), &destination_path) {
                staged_count += 1;
                continue;
            }
            match std::fs::copy(entry.path(), &destination_path) {
                Ok(_) => staged_count += 1,
                Err(error) => last_error = error.to_string(),
            }
        }
    }

    if candidate_count == 0 {
        println!(
            "cargo:warning=std dylib not staged: no std-*.dll found in {}",
            std_library_directory.display()
        );
    } else if staged_count == 0 {
        println!(
            "cargo:warning=std dylib not staged: found {candidate_count} in {}, but no copy into {} succeeded ({last_error}); a running binary usually holds the lock",
            std_library_directory.display(),
            output_directory.display()
        );
    }
}

/// Whether `destination_path` already holds the same file as `source_path`.
///
/// `std::fs::copy` keeps the source's modification time on Windows, so a
/// staged copy of the same toolchain file has the same size and is no older.
/// A toolchain update changes the source's time, which makes the copy stale.
#[cfg(windows)]
fn staged_copy_is_current(source_path: &Path, destination_path: &Path) -> bool {
    let (Ok(source_metadata), Ok(destination_metadata)) = (
        std::fs::metadata(source_path),
        std::fs::metadata(destination_path),
    ) else {
        return false;
    };
    match (source_metadata.modified(), destination_metadata.modified()) {
        (Ok(source_time), Ok(destination_time)) => {
            source_metadata.len() == destination_metadata.len() && destination_time >= source_time
        }
        _ => false,
    }
}

/// Runs `rustc --print sysroot` and returns the toolchain's root directory.
///
/// Uses the compiler named by `RUSTC` when the build script sets it, and
/// otherwise the first `rustc` on `PATH`.
///
/// # Errors
///
/// Returns the reason when the compiler cannot be started, exits with a
/// non-zero status, or writes output that is not UTF-8.
#[cfg(windows)]
fn rust_sysroot() -> Result<String, String> {
    let rustc_path = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let sysroot_output = Command::new(&rustc_path)
        .arg("--print")
        .arg("sysroot")
        .output()
        .map_err(|error| error.to_string())?;
    if !sysroot_output.status.success() {
        return Err(format!(
            "`{rustc_path} --print sysroot` exited with {:?}",
            sysroot_output.status
        ));
    }
    String::from_utf8(sysroot_output.stdout)
        .map(|sysroot_path| sysroot_path.trim().to_string())
        .map_err(|error| error.to_string())
}

//! Record the cargo profile this host is being compiled into.
//!
//! # Responsibilities
//!
//! - Emits `PILL_HOST_PROFILE_DIRECTORY` so the host can build the project and
//!   its optional modules into the same profile it was compiled into.
//!
//! # Design
//!
//! The host shells out to cargo to build the project and every optional
//! module, and those builds must land in the same profile the host itself was
//! built with. A release host that loads debug-profile modules fails at
//! `LoadLibrary` with "The specified procedure could not be found" (os error
//! 127), because the two sides resolve different crate-metadata hashes.
//!
//! `cfg!(debug_assertions)` cannot answer this: the `release-with-debug`
//! profile keeps debug assertions on while building into `target/release-with-
//! debug`. The only thing that knows the real profile directory is `OUT_DIR`,
//! which cargo shapes as `<target>/<profile-directory>/build/<pkg>-<hash>/out`.

use std::path::{Path, PathBuf};

/// Profile directory assumed when `OUT_DIR` cannot be parsed.
///
/// `debug` is the safe fallback: it is what an ordinary `cargo build`
/// produces, so a host that guesses wrong here behaves exactly as it did
/// before this script existed.
const FALLBACK_PROFILE_DIRECTORY: &str = "debug";

fn main() {
    // Only `OUT_DIR` changes the answer, so nothing else needs to re-run this.
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=PILL_HOST_PROFILE_DIRECTORY={}",
        profile_directory().unwrap_or_else(|| FALLBACK_PROFILE_DIRECTORY.to_string())
    );
}

/// Extract the profile directory name from `OUT_DIR`.
///
/// `OUT_DIR` is `<target>/<profile-directory>/build/<package>-<hash>/out`, so
/// the profile directory is three levels up. Returns `None` when the variable
/// is missing or shorter than that shape, which is not an error worth failing
/// a build over - the caller falls back.
fn profile_directory() -> Option<String> {
    let out_directory = PathBuf::from(std::env::var_os("OUT_DIR")?);
    out_directory
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
}

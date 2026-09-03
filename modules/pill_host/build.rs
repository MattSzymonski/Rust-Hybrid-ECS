//! Record the cargo profile this host is being compiled into.
//!
//! # Responsibilities
//!
//! - Emits `PILL_HOST_PROFILE_DIRECTORY` so the host can build the project and
//!   its optional modules into the same profile it was compiled into.
//! - Emits `PILL_HOST_TARGET_TRIPLE` (empty for a native build) so host-spawned
//!   module builds can pass the same `--target` when a launcher such as the
//!   dioxus CLI built the host with one. Cargo folds the target into every
//!   crate's metadata hash, so a module built natively against a `--target`
//!   host cannot resolve its dynamic imports (os error 127 at load).
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
    // Only `OUT_DIR` and the workspace manifest change the answer.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../Cargo.toml");
    let (profile_directory, target_triple) = out_directory_layout();
    let profile_directory =
        profile_directory.unwrap_or_else(|| FALLBACK_PROFILE_DIRECTORY.to_string());
    println!("cargo:rustc-env=PILL_HOST_PROFILE_DIRECTORY={profile_directory}");
    // Empty when the host was built natively (no `--target`), which is the
    // common `cargo run` case; set to the triple when a tool such as the
    // dioxus CLI passed `--target <host>` explicitly.
    println!("cargo:rustc-env=PILL_HOST_TARGET_TRIPLE={target_triple}");
}

/// Parse `OUT_DIR` into `(profile directory, target triple)`.
///
/// `OUT_DIR` is `<target-root>/[<triple>/]<profile>/build/<package>-<hash>/out`
/// for an explicit-`--target` build, or `<target-root>/<profile>/build/
/// <package>-<hash>/out` for a native one. Three parents up is the profile
/// directory; the parent of THAT is the target root when native, or the
/// target triple when cargo was invoked with `--target`. Distinguishing the
/// two matters because cargo folds the target into each crate's metadata
/// hash, so a module built natively against a `--target` host resolves
/// different symbol names than the host exports - visible only as a silent
/// `LoadLibrary` "procedure not found" at the DLL boundary.
fn out_directory_layout() -> (Option<String>, String) {
    let Some(out_directory) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return (None, String::new());
    };
    let package_build = out_directory.parent();
    let build = package_build.and_then(Path::parent);
    let profile = build.and_then(Path::parent);
    let target_root = profile.and_then(Path::parent);
    let profile_name = profile
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned());
    // A rustc target triple always contains at least one hyphen
    // (`x86_64-pc-windows-msvc`, `aarch64-apple-darwin`, `wasm32-unknown-
    // unknown`); a bare target root such as `target` never does. The one
    // false positive - a CARGO_TARGET_DIR whose own name contains a hyphen -
    // cannot occur here because every host build in this repository uses the
    // default `target` directory.
    let triple = target_root
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.contains('-'))
        .unwrap_or_default();
    (profile_name, triple)
}

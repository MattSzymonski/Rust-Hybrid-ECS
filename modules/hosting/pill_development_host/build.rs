//! Record the cargo profile and target triple this host is compiled into.
//!
//! # Responsibilities
//!
//! - Emit `PILL_HOST_PROFILE_DIRECTORY`, the profile directory the host was
//!   built into, so the project and extension builds it spawns land in the
//!   same one.
//! - Emit `PILL_HOST_TARGET_TRIPLE`, empty for a native build, so
//!   host-spawned module builds can pass the `--target` a launcher gave the
//!   host.
//!
//! # Design
//!
//! The host shells out to cargo for the project and every extension, and those
//! builds must land in the profile the host itself was built with. A release
//! host that loads debug-profile modules fails in `LoadLibrary` with "The
//! specified procedure could not be found" (os error 127), because the two
//! sides resolve different crate-metadata hashes. The same hashing makes a
//! natively built module unloadable by a host cargo built with `--target`.
//!
//! `cfg!(debug_assertions)` cannot answer this: `release-with-debug` inherits
//! `release`, so its assertions stay off, yet it builds into
//! `target/release-with-debug`. The only thing that knows the real profile
//! directory is `OUT_DIR`, which cargo shapes as
//! `<target>/<profile-directory>/build/<package>-<hash>/out`.

// Standard library
use std::path::{Path, PathBuf};

/// Profile directory assumed when `OUT_DIR` cannot be parsed.
///
/// `debug` is the safe fallback: it is what an ordinary `cargo build`
/// produces, so a host that guesses wrong still matches the default build.
const FALLBACK_PROFILE_DIRECTORY: &str = "debug";

/// Emits the host's profile directory and target triple as `rustc-env` values.
fn main() {
    // Step 1: Declare the inputs. The workspace manifest carries the
    // `[profile.*]` sections, and its path must name a file that exists: cargo
    // treats a missing one as changed on every build, recompiling the host and
    // every binary that links it.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../Cargo.toml");

    // Step 2: Resolve the layout, falling back to the default build profile.
    let (profile_directory, target_triple) = out_directory_layout();
    let profile_directory =
        profile_directory.unwrap_or_else(|| FALLBACK_PROFILE_DIRECTORY.to_string());
    println!("cargo:rustc-env=PILL_HOST_PROFILE_DIRECTORY={profile_directory}");

    // Empty when the host was built natively (no `--target`), which is the
    // common `cargo run` case; set to the triple when a tool such as the
    // dioxus CLI passed `--target <host>` explicitly.
    println!("cargo:rustc-env=PILL_HOST_TARGET_TRIPLE={target_triple}");
}

/// Parses `OUT_DIR` into `(profile directory, target triple)`.
///
/// `OUT_DIR` is `<target-root>/[<triple>/]<profile>/build/<package>-<hash>/out`,
/// so three parents up is the profile directory, and that directory's parent
/// is the target root when native or the triple cargo was invoked with. The
/// parent names are what tell the two apart; a natively built module loaded
/// into a `--target` host fails only at the DLL boundary with a silent
/// `LoadLibrary` "procedure not found".
fn out_directory_layout() -> (Option<String>, String) {
    // Step 1: Read `OUT_DIR`; without it neither value can be derived.
    let Some(out_directory) = std::env::var_os("OUT_DIR").map(PathBuf::from) else {
        return (None, String::new());
    };

    // Step 2: Walk out of `build/<package>-<hash>/out` to the profile
    // directory and its parent.
    let package_build = out_directory.parent();
    let build = package_build.and_then(Path::parent);
    let profile = build.and_then(Path::parent);
    let target_root = profile.and_then(Path::parent);
    let profile_name = profile
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned());

    // Step 3: A rustc target triple always contains at least one hyphen
    // (`x86_64-pc-windows-msvc`, `aarch64-apple-darwin`,
    // `wasm32-unknown-unknown`); a bare target root such as `target` never
    // does. The one false positive - a `CARGO_TARGET_DIR` whose own name
    // contains a hyphen - cannot occur here: every build that compiles this
    // host writes under the workspace's `target` directory.
    let triple = target_root
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.contains('-'))
        .unwrap_or_default();
    (profile_name, triple)
}

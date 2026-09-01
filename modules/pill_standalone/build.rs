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
//!
//! It also enforces the release posture: a release build of the host is always
//! the shipping build, so `hot_reload` (and its `hot_patch` superset) are
//! refused in any non-debug profile.

fn main() {
    // A release build of the host is always the shipping posture: the reloading
    // machinery is a development tool and must not ship. cargo exposes the
    // active profile to build scripts as `PROFILE` and each enabled feature as
    // `CARGO_FEATURE_*`, so the combination is refused here - before any crate
    // in the dependency graph is compiled - for every invocation path (script,
    // CI, or a bare `cargo build --release`). `main.rs` also carries a
    // `compile_error!` for the same combination; this fires first with the
    // same guidance.
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let hot_reload_enabled = std::env::var_os("CARGO_FEATURE_HOT_RELOAD").is_some()
        || std::env::var_os("CARGO_FEATURE_HOT_PATCH").is_some();
    if profile != "debug" && hot_reload_enabled {
        panic!(
            "pill_standalone cannot be built with `hot_reload` in the `{profile}` profile: \
             a release build is always the shipping posture. Build it as \
             `--no-default-features --features static_project`."
        );
    }

    // Copy `std-*.dll` next to the built executable (no-op off Windows).
    pill_hot_scan::stage_std_dylib();
}

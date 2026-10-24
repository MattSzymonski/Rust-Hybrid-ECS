//! The platform layer: the one place that knows which target the engine runs on.
//!
//! # Responsibilities
//!
//! - Provide a monotonic clock ([`Instant`]) and the wall-clock time that work
//!   on native and on the web.
//! - Provide the log outputs: terminal and file on native, the browser
//!   console on the web.
//! - Provide the way to run a future: block on native, spawn on the web.
//!
//! # Design
//!
//! Plain functions and types with a per-platform body, not trait objects: the
//! target is fixed at compile time, so there is nothing to choose at runtime.
//! The `cfg(target_arch = "wasm32")` gates live inside these submodules and
//! nowhere else, so code above this layer reads the same on every target.
//!
//! `std::time::Instant` is banned outside [`clock`] by the workspace
//! `clippy.toml` (`disallowed-types`): `Instant::now()` panics on
//! `wasm32-unknown-unknown`, and a use that compiles fine natively would only
//! fail once the game runs in a browser.

/// Monotonic time and wall-clock time.
pub mod clock;
/// Running futures: block on native, spawn on the web.
pub mod futures;
/// Where log output goes: terminal and file on native, the console on the web.
pub mod log_output;

/// The monotonic clock, re-exported so callers write `platform::Instant`.
pub use clock::Instant;

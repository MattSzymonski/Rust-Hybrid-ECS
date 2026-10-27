//! This crate's build script: emits its function-address inventory.
//!
//! # Responsibilities
//!
//! - Emit the crate's function-address inventory so the host can redirect any
//!   of its functions with nothing annotated in the source.
//! - Keep the work in `pill_hot_scan`, which the host also uses to decide what
//!   is patchable; the two must agree byte for byte, so there is exactly one
//!   implementation.
//
// REQUIREMENTS
//   Rust (stable). Run by Cargo as this crate's build script.
//
// DESCRIPTION
//   Generates the crate's function-address inventory into
//   `$OUT_DIR/function_inventory.rs`.
//
// USAGE
//   cargo build -p pill_audio
//
// --- SCRIPT ---

fn main() {
    pill_hot_scan::generate_function_inventory();
}

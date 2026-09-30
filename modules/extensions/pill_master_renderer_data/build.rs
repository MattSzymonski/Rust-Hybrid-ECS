//! Cooks the renderer's shaders before the crate itself compiles.
//!
//! # Responsibilities
//!
//! - Cook every HLSL tree under `shaders/` to the WGSL the crate embeds,
//!   through the same routine the development shader reload runs.
//! - Report every source directory, header and discovered input to cargo so
//!   that a source edit rebuilds.
//!
//! # Design
//!
//! Cargo runs build scripts before the crate's own compilation, so the cooked
//! files are on disk by the time `include_str!` looks for them. The roots and
//! the cooking itself live in `src/config/shader_roots.rs` (Rust, so it stays
//! under `src/`), compiled here as a
//! `#[path]` module, so a runtime cook and this one cannot disagree about which
//! files exist; see that file for the tree layout.
//!
//! Cargo cannot watch a glob, so a *new* `.hlsl` is invisible to a per-file
//! list written before it existed; the directory's timestamp carries the
//! addition and the file list carries the edits.

// Standard library
use std::path::PathBuf;

/// The shader trees and the routine that cooks them, shared with the crate.
///
/// Each of the two compilations reads only part of the report - this one the
/// inputs, the crate the rebuilt outputs - so the rest is dead code here.
#[allow(dead_code)]
#[path = "src/config/shader_roots.rs"]
mod shader_roots;

fn main() {
    // The shader trees live at the crate root rather than under `src/`, so a
    // shader edit is not a Rust source edit: a module watcher on `src/` never
    // rebuilds the crate for one.
    let config_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("shaders");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/config/shader_roots.rs");

    let report = shader_roots::cook_config_shaders(&config_directory)
        .unwrap_or_else(|error| panic!("shader cooking failed: {error}"));

    // Directories carry additions, files carry edits; headers are inputs of
    // every rule through the include path, and no glob matches them. A missing
    // directory is left out: cargo treats a path that does not exist as changed
    // and would rerun this script on every build.
    for directory in shader_roots::shader_source_directories(&config_directory) {
        if directory.is_dir() {
            println!("cargo:rerun-if-changed={}", directory.display());
        }
    }
    for header in &report.header_files {
        println!("cargo:rerun-if-changed={}", header.display());
    }
    for input in &report.discovered_sources {
        println!("cargo:rerun-if-changed={}", input.display());
    }
}

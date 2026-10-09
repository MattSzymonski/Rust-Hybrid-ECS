//! Cooks the renderer's shaders before the crate itself compiles.
//!
//! # Responsibilities
//!
//! - Cook every HLSL source under `shaders/` to the WGSL the crate embeds,
//!   through the routine the development host's shader watcher also runs.
//! - Report every source directory, header and discovered input to cargo so
//!   that a source edit rebuilds.
//!
//! # Design
//!
//! Cargo runs build scripts before the crate's own compilation, so the cooked
//! files are on disk by the time `include_str!` looks for them. Which
//! directories hold sources and which hold headers is not listed here: it is
//! `pill_assets`' shader-tree convention ([`pill_assets::cook_shader_tree`]), so
//! the build and a runtime cook cannot disagree about which files exist.
//!
//! Cargo cannot watch a glob, so a *new* `.hlsl` is invisible to a per-file
//! list written before it existed; the directory's timestamp carries the
//! addition and the file list carries the edits.

// Standard library
use std::path::PathBuf;

fn main() {
    // The shader trees live at the crate root rather than under `src/`, so a
    // shader edit is not a Rust source edit: a module watcher on `src/` never
    // rebuilds the crate for one.
    let shaders_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("shaders");

    println!("cargo:rerun-if-changed=build.rs");

    let report = pill_assets::cook_shader_tree(&shaders_directory)
        .unwrap_or_else(|error| panic!("shader cooking failed: {error}"));

    // Directories carry additions, files carry edits; headers are inputs of
    // every source through the include path, and no glob matches them.
    for directory in report.layout.watched_directories() {
        println!("cargo:rerun-if-changed={}", directory.display());
    }
    for header in &report.header_files {
        println!("cargo:rerun-if-changed={}", header.display());
    }
    for input in &report.discovered_sources {
        println!("cargo:rerun-if-changed={}", input.display());
    }
}

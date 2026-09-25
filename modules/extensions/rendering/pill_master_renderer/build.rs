//! Cooks the renderer's shaders before the crate itself compiles.
//!
//! # Responsibilities
//!
//! - Run the [`pill_assets`] pipeline over `src/`, turning `src/shaders/*.hlsl`
//!   into the WGSL the crate `include_str!`s.
//! - Report every discovered input, and every shader header, to cargo so that a
//!   source edit rebuilds.
//!
//! # Design
//!
//! Cargo runs build scripts before the crate's own compilation, so the cooked
//! files are on disk by the time `include_str!` looks for them.
//!
//! The watched set is the shader directory as well as the files in it. Cargo
//! cannot watch a glob, so a *new* `.hlsl` is invisible to a per-file list
//! written before it existed; the directory's timestamp carries the addition and
//! the file list carries the edits.

use std::path::PathBuf;

use pill_assets::{walk_files, Pipeline};

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let stats = Pipeline::new(root.clone())
        .run()
        .unwrap_or_else(|error| panic!("shader cooking failed: {error}"));

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", root.join("shaders").display());
    // The sources `#include` this directory while the rule's glob matches only
    // top-level `shaders/*.hlsl`, so the headers are reported here by hand.
    let headers = walk_files(&root.join("shaders/include"))
        .unwrap_or_else(|error| panic!("shader headers: {error}"));
    for header in headers {
        println!("cargo:rerun-if-changed={}", header.display());
    }
    for input in &stats.discovered {
        println!("cargo:rerun-if-changed={}", input.display());
    }
}

//! Cooks this project's shaders before the crate compiles.
//!
//! # Responsibilities
//!
//! - Run the [`pill_assets`] pipeline over `res/`, turning `res/shaders/*.hlsl`
//!   into the WGSL the project loads at runtime.
//! - Report every discovered input, and every shared shader header, to cargo so
//!   that editing a source rebuilds.
//!
//! # Design
//!
//! Shader cooking is as mandatory for a project as it is for the renderer: the
//! runtime loads the `.wgsl` beside each `.hlsl`, so a build that skipped the
//! cook would load a stale or missing shader. Cooking meshes and textures stays
//! opt-in through the pipeline's manifest, and this project does not ask for it -
//! the committed helmet asset is loaded as it is.
//!
//! The watched set is the shader directory as well as the files in it. Cargo
//! cannot watch a glob, so a *new* `.hlsl` is invisible to a per-file list that
//! was written before it existed; the directory's own timestamp carries the
//! addition and the file list carries the edits.

use std::path::PathBuf;

use pill_assets::{walk_files, Pipeline};

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("res");
    let stats = Pipeline::new(root.clone())
        .run()
        .unwrap_or_else(|error| panic!("shader cooking failed: {error}"));

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", root.join("shaders").display());
    let headers = walk_files(&root.join("shaders/include"))
        .unwrap_or_else(|error| panic!("shader headers: {error}"));
    for header in headers {
        println!("cargo:rerun-if-changed={}", header.display());
    }
    for input in &stats.discovered {
        println!("cargo:rerun-if-changed={}", input.display());
    }
}

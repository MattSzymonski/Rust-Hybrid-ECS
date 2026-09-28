//! Cooks the renderer's shaders before the crate itself compiles.
//!
//! # Responsibilities
//!
//! - Run the [`pill_assets`] pipeline over each pipeline's `shaders/`
//!   directory, turning `*.hlsl` into the WGSL the crate `include_str!`s.
//! - Report every discovered input, each tree's `include/`, and the shared
//!   header, to cargo so that a source edit rebuilds.
//!
//! # Design
//!
//! Cargo runs build scripts before the crate's own compilation, so the cooked
//! files are on disk by the time `include_str!` looks for them.
//!
//! # Roots
//!
//! The shader rule matches `<root>/shaders/*.hlsl`: one level, not a search. So
//! every `shaders` directory is a root of its own, and one ships per pipeline,
//! each beside the passes that draw through it:
//!
//! - `src/config/simple_pipeline/` - the built-in lit pair.
//! - `src/config/pbr_pipeline/` - the fragment stage the geometry pass draws
//!   through.
//! - `src/config/post_processing/` - the fullscreen vertex stage and the four
//!   fragment stages the post-processing passes draw through.
//!
//! None of the three sits inside another, so one call each covers them. A rule
//! that walked the tree would need a single root; it does not, and the cost of
//! being explicit is one line per pipeline that keeps its stages beside the
//! passes rather than in a crate-wide pile.
//!
//! # The shared header
//!
//! Three sources `#include` `config/common_shaders/common.hlsl`, which belongs
//! to no one pipeline. It is not in a `shaders/` directory at all, and rather
//! than each tree carrying a copy of it the rule is handed the directory as an
//! include path. The name is deliberate: `common_shaders` is not `shaders`, so
//! no rule's glob reaches a `.hlsl` there, and the name still says what the
//! directory is for.
//!
//! The watched set is each shader directory, the files in it, and the shared
//! header. Cargo cannot watch a glob, so a *new* `.hlsl` is invisible to a
//! per-file list written before it existed; the directory's timestamp carries
//! the addition and the file list carries the edits.

use std::path::PathBuf;

use pill_assets::{walk_files, HlslToWgsl, Pipeline, Rule};

fn main() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let shared = src.join("config").join("common_shaders");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", shared.display());
    // Cargo cannot watch a glob, and the shared header is not a rule input in
    // any root, so every file in the directory is reported by name.
    for header in walk_files(&shared).unwrap_or_else(|error| panic!("shared headers: {error}")) {
        println!("cargo:rerun-if-changed={}", header.display());
    }

    for root in [
        src.join("config").join("simple_pipeline"),
        src.join("config").join("pbr_pipeline"),
        src.join("config").join("post_processing"),
    ] {
        // A fresh rule per root: the rule set is not `Clone`, and rebuilding it
        // is cheaper to read than hoisting something out of the loop.
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(HlslToWgsl::new().with_include(&shared))];
        let stats = Pipeline::with_rules(root.clone(), rules)
            .run()
            .unwrap_or_else(|error| panic!("shader cooking failed: {error}"));

        println!("cargo:rerun-if-changed={}", root.join("shaders").display());
        // No tree has an `include/` since the header became shared, but a tree
        // that grows one is still watched: the rule's glob is top level only, so
        // nothing else would notice.
        let headers = walk_files(&root.join("shaders/include"))
            .unwrap_or_else(|error| panic!("shader headers: {error}"));
        for header in headers {
            println!("cargo:rerun-if-changed={}", header.display());
        }
        for input in &stats.discovered {
            println!("cargo:rerun-if-changed={}", input.display());
        }
    }
}

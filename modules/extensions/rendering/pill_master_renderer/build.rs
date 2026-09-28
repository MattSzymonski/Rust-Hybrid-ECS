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
//! The shader rule matches its sources one level below the directory it is
//! given: `shaders/*.hlsl` for [`HlslToWgsl::new`], or `*.hlsl` for
//! [`HlslToWgsl::flat`], where the root is itself the shaders directory. Four
//! trees ship, and one call each covers them:
//!
//! - `src/config/common_shaders/` - flat: the vertex stage every lit pass in the
//!   crate starts from. It has no `shaders/` level because it is not one
//!   pipeline's tree, and a shared one holding a handful of stages does not
//!   need the extra directory to keep them apart.
//! - `src/config/simple_pipeline/` - the built-in lit fragment stage.
//! - `src/config/pbr_pipeline/` - the fragment stage the geometry pass draws
//!   through.
//! - `src/config/post_processing/` - the fullscreen vertex stage and the four
//!   fragment stages the post-processing passes draw through.
//!
//! # Headers
//!
//! A header cannot sit where a glob will match it: the rule refuses a name that
//! declares no stage rather than guessing one, so `common.hlsl` would fail the
//! build rather than be skipped. It lives in `common_shaders/include/` - the one
//! directory beside the sources that no glob reaches - and the rule is handed
//! that directory as an include path, so `#include "common.hlsl"` resolves from
//! every tree.
//!
//! The watched set is each source directory, each tree's own `include/`, the
//! shared `include/`, and every file the rule discovered. Cargo cannot watch a
//! glob, so a *new* `.hlsl` is invisible to a per-file list written before it
//! existed; the directory's timestamp carries the addition and the file list
//! carries the edits.

use std::path::PathBuf;

use pill_assets::{walk_files, HlslToWgsl, Pipeline, Rule};

fn main() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let config = src.join("config");
    let shared = config.join("common_shaders");
    let shared_include = shared.join("include");

    println!("cargo:rerun-if-changed=build.rs");
    // The header is an input of every rule, through the include path, and no
    // glob matches it: it is reported by hand, directory and files both.
    println!("cargo:rerun-if-changed={}", shared_include.display());
    for header in walk_files(&shared_include)
        .unwrap_or_else(|error| panic!("shared headers: {error}"))
    {
        println!("cargo:rerun-if-changed={}", header.display());
    }

    // The flag is set where the root is itself the shaders directory.
    let roots = [
        (shared.clone(), true),
        (config.join("simple_pipeline"), false),
        (config.join("pbr_pipeline"), false),
        (config.join("post_processing"), false),
    ];

    for (root, flat) in roots {
        // A fresh rule per root: the rule set is not `Clone`, and rebuilding it
        // is cheaper to read than hoisting something out of the loop.
        let rule = if flat {
            HlslToWgsl::flat()
        } else {
            HlslToWgsl::new()
        };
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(rule.with_include(&shared_include))];
        let stats = Pipeline::with_rules(root.clone(), rules)
            .run()
            .unwrap_or_else(|error| panic!("shader cooking failed: {error}"));

        let sources = if flat { root } else { root.join("shaders") };
        println!("cargo:rerun-if-changed={}", sources.display());
        // A tree may keep headers of its own in `include/`; the rule's glob does
        // not reach them, so they are listed here. A directory that is not there
        // walks to an empty list rather than an error.
        for header in walk_files(&sources.join("include"))
            .unwrap_or_else(|error| panic!("shader headers: {error}"))
        {
            println!("cargo:rerun-if-changed={}", header.display());
        }
        for input in &stats.discovered {
            println!("cargo:rerun-if-changed={}", input.display());
        }
    }
}

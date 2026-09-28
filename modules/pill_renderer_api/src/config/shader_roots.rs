//! Where the renderer's shader sources live, and how one pass cooks them all.
//!
//! # Responsibilities
//!
//! - Name the four HLSL source trees under `src/config/` and the shared header
//!   directory they all include from.
//! - Cook every tree with the [`pill_assets`] shader rule in one call
//!   ([`cook_config_shaders`]), and list the directories an edit can land in
//!   ([`shader_source_directories`]).
//!
//! # Design
//!
//! Compiled twice from this one file: by `build.rs` through a `#[path]` module,
//! which cooks before the crate builds and reports the inputs to cargo, and by
//! the crate itself behind the `shader-hot-reload` feature, where the
//! development shader reload cooks the same trees at runtime. Keeping the roots
//! here rather than in either caller is what stops a runtime reload cooking a
//! different set of files than the build did. It depends on nothing but `std`
//! and `pill_assets`, which both callers have.
//!
//! # Roots
//!
//! The shader rule matches its sources one level below the directory it is
//! given: `shaders/*.hlsl` for [`HlslToWgsl::new`], or `*.hlsl` for
//! [`HlslToWgsl::flat`], where the root is itself the shaders directory:
//!
//! - `common_shaders/` - flat: the vertex stage every lit pass in the crate
//!   starts from. It has no `shaders/` level because it is not one pipeline's
//!   tree.
//! - `simple_pipeline/` - the built-in lit fragment stage.
//! - `pbr_pipeline/` - the fragment stage the geometry pass draws through.
//! - `post_processing/` - the fullscreen vertex stage and the four fragment
//!   stages the post-processing passes draw through.
//!
//! # Headers
//!
//! A header cannot sit where a glob will match it: the rule refuses a name that
//! declares no stage rather than guessing one. Shared headers live in
//! `common_shaders/include/`, which every rule is handed as an include path; a
//! tree may also keep its own headers in an `include/` beside its sources.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_assets::{walk_files, CookError, HlslToWgsl, Pipeline, Rule};

/// Directory under `src/config/` holding the headers every tree includes.
const SHARED_INCLUDE_DIRECTORY: &str = "common_shaders/include";

/// Each source tree under `src/config/`, and whether its root is itself the
/// shaders directory (`true`) or holds one named `shaders/` (`false`).
const SHADER_ROOTS: [(&str, bool); 4] = [
    ("common_shaders", true),
    ("simple_pipeline", false),
    ("pbr_pipeline", false),
    ("post_processing", false),
];

/// What one cook of every tree touched and found.
#[derive(Debug, Default)]
pub struct ShaderCookReport {
    /// Every HLSL source the rules matched.
    pub discovered_sources: Vec<PathBuf>,
    /// Every header in a shared or per-tree `include/` directory.
    pub header_files: Vec<PathBuf>,
    /// Cooked outputs this run rebuilt, whether or not their text changed.
    pub rebuilt_outputs: Vec<PathBuf>,
}

/// The directories an edit to a shader source or header can land in.
///
/// Each tree's sources directory, each tree's own `include/`, and the shared
/// `include/`. A directory that does not exist is listed anyway; callers skip
/// it, because a watcher cannot register it and cargo would treat it as
/// permanently changed.
pub fn shader_source_directories(config_directory: &Path) -> Vec<PathBuf> {
    let mut directories = vec![config_directory.join(SHARED_INCLUDE_DIRECTORY)];
    for (root, flat) in SHADER_ROOTS {
        let sources = sources_directory(config_directory, root, flat);
        directories.push(sources.join("include"));
        directories.push(sources);
    }
    directories
}

/// Cook every stale shader under `config_directory` (the crate's `src/config/`).
///
/// # Errors
///
/// Returns the first [`CookError`]: a source `slangc` rejects, a missing
/// `slangc`, or an I/O failure. Outputs cooked before the failure stay in place.
pub fn cook_config_shaders(config_directory: &Path) -> Result<ShaderCookReport, CookError> {
    let shared_include = config_directory.join(SHARED_INCLUDE_DIRECTORY);
    let mut report = ShaderCookReport {
        header_files: walk_files(&shared_include)?,
        ..ShaderCookReport::default()
    };

    for (root, flat) in SHADER_ROOTS {
        // A fresh rule per root: the rule set is not `Clone`, and building it
        // again is cheaper to read than hoisting it out of the loop.
        let rule = if flat {
            HlslToWgsl::flat()
        } else {
            HlslToWgsl::new()
        };
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(rule.with_include(&shared_include))];
        let stats = Pipeline::with_rules(config_directory.join(root), rules).run()?;

        // A tree's own headers: the rule's glob does not reach them, and a
        // directory that is not there walks to an empty list.
        let sources = sources_directory(config_directory, root, flat);
        report
            .header_files
            .extend(walk_files(&sources.join("include"))?);
        report.discovered_sources.extend(stats.discovered);
        report.rebuilt_outputs.extend(stats.rebuilt);
    }
    Ok(report)
}

/// The directory a tree's stage sources sit in.
fn sources_directory(config_directory: &Path, root: &str, flat: bool) -> PathBuf {
    let root = config_directory.join(root);
    if flat {
        root
    } else {
        root.join("shaders")
    }
}

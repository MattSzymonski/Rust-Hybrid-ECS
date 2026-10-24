//! A crate's whole shader directory, cooked by convention.
//!
//! # Responsibilities
//!
//! - Find the directories under a `shaders/` root that hold stage sources, and
//!   the `include/` directories that hold headers ([`shader_tree_layout`]).
//! - Cook every stale source in all of them in one call ([`cook_shader_tree`]),
//!   with every header directory on the include path.
//!
//! # Design
//!
//! One routine for the two places a crate's shaders are cooked: its build
//! script, before the crate compiles, and the development host's shader
//! watcher, while the crate runs. Both call this with the same root, so a
//! runtime cook can never compile a different set of files, or resolve a header
//! differently, than the build did - and neither has to name the crate's
//! directories.
//!
//! # Convention
//!
//! - A directory named `include` holds headers. Every one of them in the tree
//!   is on every source's include path: a source's own `include/` first (the
//!   rule searches it anyway), then the rest in path order.
//! - Any other directory that directly holds `.hlsl` files is a source
//!   directory. Its sources are cooked to `.wgsl` beside them, with the stage
//!   named by the file (see [`HlslToWgsl`]).
//! - Every other directory only groups; nothing in it is cooked.

// Standard library
use std::fs;
use std::path::{Path, PathBuf};

// Current crate
use crate::{walk_files, CookError, HlslToWgsl, Pipeline, Rule};

/// Name of a directory holding headers rather than stage sources.
const INCLUDE_DIRECTORY_NAME: &str = "include";

/// Extension of the stage sources and headers the shader rule reads.
const SOURCE_EXTENSION: &str = "hlsl";

/// Where a shader tree keeps its sources and its headers.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ShaderTreeLayout {
    /// Directories that directly hold stage sources, sorted.
    pub source_directories: Vec<PathBuf>,
    /// Directories named `include`, sorted.
    pub include_directories: Vec<PathBuf>,
}

impl ShaderTreeLayout {
    /// Every directory an edit to a source or a header can land in: the ones a
    /// watcher registers and a build script reports to cargo.
    pub fn watched_directories(&self) -> Vec<PathBuf> {
        let mut directories = self.source_directories.clone();
        directories.extend(self.include_directories.iter().cloned());
        directories.sort();
        directories
    }
}

/// What one [`cook_shader_tree`] found and did.
#[derive(Debug, Default)]
pub struct ShaderTreeReport {
    /// The layout the cook ran against.
    pub layout: ShaderTreeLayout,
    /// Every stage source the cook matched.
    pub discovered_sources: Vec<PathBuf>,
    /// Every file in an `include/` directory.
    pub header_files: Vec<PathBuf>,
    /// Outputs this run rebuilt, whether or not their text changed.
    pub rebuilt_outputs: Vec<PathBuf>,
}

/// Find the source and header directories under `root`.
///
/// A `root` that does not exist has neither; that is not an error, so a crate
/// without shaders can still run its build script.
///
/// # Errors
///
/// Returns [`CookError::Io`] when a directory cannot be read.
pub fn shader_tree_layout(root: &Path) -> Result<ShaderTreeLayout, CookError> {
    let mut layout = ShaderTreeLayout::default();
    collect_layout(root, &mut layout)?;
    layout.source_directories.sort();
    layout.include_directories.sort();
    Ok(layout)
}

/// Cook every stale stage source under `root` (a crate's `shaders/`).
///
/// A source is stale when its `.wgsl` is missing or older than the source or
/// than any header, so a header edit re-cooks every source that could include
/// it, and an unchanged tree cooks nothing.
///
/// # Errors
///
/// Returns the first [`CookError`]: a source `slangc` rejects, a missing
/// `slangc`, or an I/O failure. Outputs cooked before the failure stay in
/// place, so a caller that keeps the previous shaders on failure still finds
/// them on disk.
pub fn cook_shader_tree(root: &Path) -> Result<ShaderTreeReport, CookError> {
    let layout = shader_tree_layout(root)?;
    let mut report = ShaderTreeReport::default();
    for include_directory in &layout.include_directories {
        report.header_files.extend(walk_files(include_directory)?);
    }

    for source_directory in &layout.source_directories {
        // A source's own `include/` is searched by the rule first; the rest of
        // the tree's header directories follow, in path order.
        let mut rule = HlslToWgsl::flat();
        for include_directory in &layout.include_directories {
            rule = rule.with_include(include_directory);
        }
        let rules: Vec<Box<dyn Rule>> = vec![Box::new(rule)];
        let stats = Pipeline::with_rules(source_directory, rules).run()?;
        report.discovered_sources.extend(stats.discovered);
        report.rebuilt_outputs.extend(stats.rebuilt);
    }
    report.layout = layout;
    Ok(report)
}

/// Walk `directory`, sorting each subdirectory into `layout`.
fn collect_layout(directory: &Path, layout: &mut ShaderTreeLayout) -> Result<(), CookError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(CookError::Io {
                path: directory.to_owned(),
                source,
            })
        }
    };

    let mut holds_sources = false;
    let mut subdirectories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CookError::Io {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            subdirectories.push(path);
        } else if path
            .extension()
            .is_some_and(|extension| extension == SOURCE_EXTENSION)
        {
            holds_sources = true;
        }
    }

    // An `include/` is a header directory whatever it holds, and is not
    // descended into: headers are found by `walk_files`, never cooked.
    if directory
        .file_name()
        .is_some_and(|name| name == INCLUDE_DIRECTORY_NAME)
    {
        layout.include_directories.push(directory.to_owned());
        return Ok(());
    }
    if holds_sources {
        layout.source_directories.push(directory.to_owned());
    }
    for subdirectory in subdirectories {
        collect_layout(&subdirectory, layout)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree shaped like a renderer's `shaders/`: a flat group, a
    /// group with a `shaders/` level, and a shared `include/`.
    fn scratch_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("pill_assets_shader_tree")
            .join(format!("{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for directory in ["common/include", "pipeline/shaders", "empty_group"] {
            fs::create_dir_all(root.join(directory)).expect("scratch directory");
        }
        fs::write(root.join("common/default_vertex.hlsl"), "").expect("source");
        fs::write(root.join("common/include/shared.hlsl"), "").expect("header");
        fs::write(root.join("pipeline/shaders/lit_fragment.hlsl"), "").expect("source");
        fs::write(root.join("pipeline/readme.txt"), "").expect("other file");
        root
    }

    #[test]
    fn sources_and_headers_are_found_by_convention() {
        let root = scratch_tree("layout");

        let layout = shader_tree_layout(&root).expect("readable");

        assert_eq!(
            layout.source_directories,
            vec![root.join("common"), root.join("pipeline/shaders")]
        );
        assert_eq!(
            layout.include_directories,
            vec![root.join("common/include")]
        );
        assert_eq!(
            layout.watched_directories(),
            vec![
                root.join("common"),
                root.join("common/include"),
                root.join("pipeline/shaders"),
            ]
        );
    }

    #[test]
    fn a_missing_root_has_an_empty_layout() {
        let root = std::env::temp_dir().join("pill_assets_shader_tree_that_does_not_exist");

        assert_eq!(
            shader_tree_layout(&root).expect("not an error"),
            ShaderTreeLayout::default()
        );
    }
}

//! Rule-based asset cooking, driven from build scripts.
//!
//! # Responsibilities
//!
//! - Discover each rule's inputs by glob under a pipeline root, rebuild the
//!   stale outputs, and report what was discovered, rebuilt and skipped.
//! - Stamp an output a rule left untouched with its newest input's time, so an
//!   unchanged rewrite still counts as fresh.
//! - Write a project's `res` and the roots its modules ship as one asset pack
//!   ([`write_asset_pack`]), which a shipping build embeds and the engine's
//!   asset store reads.
//! - Move a source asset together with its `.meta` file ([`move_asset`]).
//!
//! # Design
//!
//! No dependencies beyond `pill_asset_format`'s constants, on purpose, like
//! `pill_hot_scan`: this crate runs from build scripts, where a dependency is
//! paid on every clean build. Globbing and timestamps come from `std::fs`.
//!
//! Shader cooking is not optional: [`default_rules`] always carries
//! [`HlslToWgsl`], because a shader that was never cooked is a shader that does
//! not exist. Cooking meshes and textures is opt-in through
//! [`Pipeline::with_manifest`], so a project that decodes those at runtime pays
//! neither the cook time nor the decoder's dependencies.

// The one exception to the workspace ban on `std::time::Instant`: this crate
// cooks assets in build scripts and the development host, never in a game, and
// it does not depend on `pill_core` (whose platform clock the ban points to).
#![allow(clippy::disallowed_types)]

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pill_asset_format::{ASSET_PACK_MAGIC, ASSET_PACK_VERSION};

/// The cooking rules this crate ships: the always-on shader rule, and whatever
/// a caller resolves to add to it.
pub mod rules;

pub use rules::{default_rules, HlslToWgsl};

/// A crate's whole `shaders/` directory, cooked by convention: the routine a
/// build script and the development shader watcher share.
pub mod shader_tree;

/// Moving a source asset and its metadata file as one operation.
pub mod asset_move;

pub use asset_move::{metadata_path_of, move_asset, MoveAssetError};

pub use shader_tree::{cook_shader_tree, shader_tree_layout, ShaderTreeLayout, ShaderTreeReport};

// =============================================================================
// Errors
// =============================================================================

/// Failure to discover inputs, cook an output, or read a manifest.
#[derive(Debug)]
pub enum CookError {
    /// The rule's input glob is not a shape this pipeline supports.
    BadGlob {
        /// The pattern that could not be expanded.
        pattern: String,
    },
    /// A filesystem operation failed.
    Io {
        /// Path the operation touched.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A rule could not turn its input into its output.
    Rule {
        /// Rule that failed.
        rule: &'static str,
        /// Input it was working on.
        input: PathBuf,
        /// What the rule reported.
        detail: String,
    },
    /// The manifest names a rule the caller did not supply.
    UnknownRule {
        /// The name found in the manifest.
        name: String,
    },
}

impl fmt::Display for CookError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadGlob { pattern } => {
                write!(formatter, "unsupported input glob: {pattern}")
            }
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Rule {
                rule,
                input,
                detail,
            } => {
                write!(
                    formatter,
                    "rule {rule} failed on {}: {detail}",
                    input.display()
                )
            }
            Self::UnknownRule { name } => write!(
                formatter,
                "the manifest names a rule this pipeline does not have: {name}"
            ),
        }
    }
}

impl std::error::Error for CookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

// =============================================================================
// Rule
// =============================================================================

/// One transformation applied to every file its glob matches.
pub trait Rule {
    /// Name used in logs, in errors and in manifests.
    fn name(&self) -> &'static str;

    /// Glob, relative to the pipeline root, selecting this rule's inputs.
    ///
    /// The shape is `directory/*.extension`, or `*.extension` alone for a root
    /// whose inputs sit directly under it. Nothing more general is supported:
    /// that is all the rules use, and a real glob engine would be a dependency
    /// every build script pays for.
    fn input_glob(&self) -> &'static str;

    /// The output `input` produces. Its timestamp decides staleness.
    fn output_for(&self, input: &Path) -> PathBuf;

    /// Side outputs written in addition to [`Rule::output_for`].
    fn extra_outputs(&self, _input: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Inputs besides `input` the rule's build reads, if any.
    ///
    /// Used for staleness only: an output is stale when any of these is newer
    /// than it. The shader rule's `#include` headers are the reason this
    /// exists - a build script can report them to cargo, but without this the
    /// per-file check would see the output as fresh and silently serve a
    /// stale cook.
    ///
    /// # Errors
    ///
    /// Returns [`CookError::Io`] when the extra inputs cannot be listed.
    fn extra_inputs(&self, _input: &Path) -> Result<Vec<PathBuf>, CookError> {
        Ok(Vec::new())
    }

    /// Turn `input` into `output`, called only while `output` is stale.
    ///
    /// # Errors
    ///
    /// Returns [`CookError::Rule`] when the underlying tool fails, including the
    /// tool's own output in `detail` so a build script's failure is readable
    /// without rerunning the tool by hand.
    fn build(&self, input: &Path, output: &Path) -> Result<(), CookError>;
}

// =============================================================================
// Pipeline
// =============================================================================

/// What one [`Pipeline::run`] found and did.
#[derive(Debug, Default)]
pub struct Stats {
    /// Every input any rule matched. Build scripts turn these into
    /// `cargo:rerun-if-changed` lines.
    pub discovered: Vec<PathBuf>,
    /// Outputs written this run.
    pub rebuilt: Vec<PathBuf>,
    /// Outputs already newer than their input and any extra inputs.
    pub skipped: Vec<PathBuf>,
    /// One `(input, output, rule)` per output a rule would produce, whether
    /// planned, rebuilt or skipped.
    pub edges: Vec<(PathBuf, PathBuf, &'static str)>,
    /// How long each rebuilt output took.
    pub cook_times: Vec<(PathBuf, Duration)>,
}

/// A set of rules applied under one root directory.
pub struct Pipeline {
    /// Directory the rules' globs resolve against.
    pub root: PathBuf,
    /// Rules to apply, in order.
    pub rules: Vec<Box<dyn Rule>>,
}

impl Pipeline {
    /// A pipeline over `root` holding the always-on rule set.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            rules: default_rules(),
        }
    }

    /// A pipeline over `root` holding `rules` instead of [`default_rules`].
    ///
    /// For a caller that has to configure a rule - the shader rule's include
    /// path, say - rather than take the always-on set as it comes.
    pub fn with_rules(root: impl Into<PathBuf>, rules: Vec<Box<dyn Rule>>) -> Self {
        Self {
            root: root.into(),
            rules,
        }
    }

    /// Add the optional rules `manifest` names, resolved through `resolve`.
    ///
    /// The manifest is plain text: one rule name per line, with `#` comments and
    /// blank lines ignored. This is where a project opts into cooking meshes and
    /// textures; an absent manifest leaves the optional rules off. `resolve` is
    /// the caller's registry, so this crate never carries the decoders.
    ///
    /// # Errors
    ///
    /// Returns [`CookError::Io`] when the manifest cannot be read, and
    /// [`CookError::UnknownRule`] when it names a rule `resolve` does not supply.
    pub fn with_manifest(
        mut self,
        manifest: &Path,
        resolve: impl Fn(&str) -> Option<Box<dyn Rule>>,
    ) -> Result<Self, CookError> {
        let text = match fs::read_to_string(manifest) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(self),
            Err(source) => {
                return Err(CookError::Io {
                    path: manifest.to_owned(),
                    source,
                })
            }
        };
        for line in text.lines() {
            let name = line.split('#').next().unwrap_or_default().trim();
            if name.is_empty() {
                continue;
            }
            match resolve(name) {
                Some(rule) => self.rules.push(rule),
                None => {
                    return Err(CookError::UnknownRule {
                        name: name.to_owned(),
                    })
                }
            }
        }
        Ok(self)
    }

    /// Cook every stale output.
    ///
    /// # Errors
    ///
    /// Returns the first failure, leaving outputs already cooked by this run in
    /// place: an output is only replaced by a successful build of its own rule.
    ///
    /// # Examples
    ///
    /// A build script reports every discovered input back to cargo:
    ///
    /// ```no_run
    /// let stats = pill_assets::Pipeline::new("src").run()?;
    /// for input in &stats.discovered {
    ///     println!("cargo:rerun-if-changed={}", input.display());
    /// }
    /// # Ok::<(), pill_assets::CookError>(())
    /// ```
    pub fn run(&self) -> Result<Stats, CookError> {
        self.walk(true)
    }

    /// List what a run would discover, without touching the filesystem.
    ///
    /// `discovered` and `edges` are filled in; `rebuilt`, `skipped` and
    /// `cook_times` stay empty because nothing is measured.
    ///
    /// # Errors
    ///
    /// Returns [`CookError::BadGlob`] or [`CookError::Io`] as for [`Pipeline::run`].
    pub fn plan(&self) -> Result<Stats, CookError> {
        self.walk(false)
    }

    /// One pass over every rule, cooking only when `execute` is set.
    fn walk(&self, execute: bool) -> Result<Stats, CookError> {
        let mut stats = Stats::default();
        for rule in &self.rules {
            for input in expand(&self.root, rule.input_glob())? {
                let output = rule.output_for(&input);
                stats.discovered.push(input.clone());
                stats
                    .edges
                    .push((input.clone(), output.clone(), rule.name()));
                for extra_output in rule.extra_outputs(&input) {
                    stats.edges.push((input.clone(), extra_output, rule.name()));
                }
                if !execute {
                    continue;
                }
                let extra_inputs = rule.extra_inputs(&input)?;
                let mut stale = is_stale(&input, &output)?;
                for extra_input in &extra_inputs {
                    if is_stale(extra_input, &output)? {
                        stale = true;
                        break;
                    }
                }
                if !stale {
                    stats.skipped.push(output);
                    continue;
                }
                let started = Instant::now();
                rule.build(&input, &output)?;
                stamp_output(&output, std::iter::once(&input).chain(&extra_inputs))?;
                stats.cook_times.push((output.clone(), started.elapsed()));
                stats.rebuilt.push(output);
            }
        }
        Ok(stats)
    }
}

/// Every file below `directory`, recursively, sorted by path.
///
/// For build scripts: a shader `#include`s headers that the rule's own glob does
/// not match, so those have to be reported to cargo by hand.
///
/// # Errors
///
/// Returns [`CookError::Io`] when a directory cannot be read. A directory that
/// does not exist is not an error and yields nothing.
pub fn walk_files(directory: &Path) -> Result<Vec<PathBuf>, CookError> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(source) => {
            return Err(CookError::Io {
                path: directory.to_owned(),
                source,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| CookError::Io {
            path: directory.to_owned(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path)?);
        } else if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Write every file below `source` into one asset pack at `output`, returning
/// the packed files for a build script to report to cargo.
///
/// Shorthand for [`write_asset_pack_with_roots`] with no module roots: a
/// project with no module resources beside it packs only its own `res`.
///
/// # Errors
///
/// As [`write_asset_pack_with_roots`].
pub fn write_asset_pack(source: &Path, output: &Path) -> Result<Vec<PathBuf>, CookError> {
    write_asset_pack_with_roots(source, &[], output)
}

/// Write every file below `source` and below each of `module_roots` into one
/// asset pack at `output`, returning the packed files for a build script to
/// report to cargo.
///
/// The format is `pill_engine::asset_store`'s, and the two change together:
/// `PILLPACK`, a `u32` version and entry count, then per entry a `u32` path
/// length, the `/`-separated path below its root, and a `u64` offset and
/// length from the pack's start, all little-endian; the bytes follow the
/// index, sorted by path. A missing `source` or root contributes no files.
///
/// A module's resources are named relative to its own root, so they pack under
/// that same name: a shipped build mounts the pack and reads them where a
/// development build finds them beside the module, with no directory to
/// declare. Names resolve the way `pill_engine::asset_store::read` resolves
/// them - the project's files first, then each root in order - because the
/// index is flat and a duplicate would replace the earlier entry silently.
/// `res/editor/` is the editor's own content and stays out of the project's
/// part; a module root is read-only content and keeps everything it ships.
///
/// # Errors
///
/// [`CookError::Io`] when a file cannot be read or the pack written, and
/// [`CookError::Rule`] for a path that is not UTF-8.
pub fn write_asset_pack_with_roots(
    source: &Path,
    module_roots: &[PathBuf],
    output: &Path,
) -> Result<Vec<PathBuf>, CookError> {
    // Name every file by its `/`-separated path below its root.
    let mut files = Vec::new();
    for file in walk_files(source)? {
        let Some(key) = pack_key(source, &file)? else {
            continue;
        };
        // `res/editor/` is the editor's own content - its theme layers and the
        // fonts they name. A game never loads it, so it never ships.
        if key.split('/').next() == Some("editor") {
            continue;
        }
        files.push((key, file));
    }
    // A project file of a module resource's name is the project's, exactly as
    // the store orders the two, so a key it claimed is not offered to a root.
    let mut claimed: BTreeSet<String> = files.iter().map(|(key, _)| key.clone()).collect();
    for root in module_roots {
        for file in walk_files(root)? {
            let Some(key) = pack_key(root, &file)? else {
                continue;
            };
            if claimed.insert(key.clone()) {
                files.push((key, file));
            }
        }
    }
    files.sort();

    // The index comes first, so every offset is known before any bytes are
    // written: it starts after the header and the whole index.
    let index_length: usize = files.iter().map(|(key, _)| 4 + key.len() + 16).sum();
    let mut offset = (ASSET_PACK_MAGIC.len() + 4 + 4 + index_length) as u64;
    let mut contents = Vec::with_capacity(files.len());
    let mut pack = Vec::new();
    pack.extend_from_slice(ASSET_PACK_MAGIC);
    pack.extend_from_slice(&ASSET_PACK_VERSION.to_le_bytes());
    pack.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (key, file) in &files {
        let bytes = fs::read(file).map_err(|source| CookError::Io {
            path: file.clone(),
            source,
        })?;
        pack.extend_from_slice(&(key.len() as u32).to_le_bytes());
        pack.extend_from_slice(key.as_bytes());
        pack.extend_from_slice(&offset.to_le_bytes());
        pack.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        offset += bytes.len() as u64;
        contents.push(bytes);
    }
    for bytes in contents {
        pack.extend_from_slice(&bytes);
    }

    // Only rewrite a pack whose bytes changed, so an unchanged `res` does not
    // relink the binary that embeds it.
    if fs::read(output).ok().as_deref() != Some(pack.as_slice()) {
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).map_err(|source| CookError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        fs::write(output, &pack).map_err(|source| CookError::Io {
            path: output.to_owned(),
            source,
        })?;
    }
    Ok(files.into_iter().map(|(_, file)| file).collect())
}

/// The `/`-separated key `file` packs under below `root`, or `None` for a
/// dot-file.
///
/// A dot-file is never an asset: `write_beside` writes a metadata update as
/// `.<file>.meta.<pid>-<seq>.tmp` and a crash can leave one behind, and the
/// runtime's own walk of `res` skips dot-files. Packing one would let it stand
/// in for the asset it was written for.
///
/// # Errors
///
/// [`CookError::Rule`] when the path is not UTF-8.
fn pack_key(root: &Path, file: &Path) -> Result<Option<String>, CookError> {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let parts: Option<Vec<&str>> = relative.iter().map(|part| part.to_str()).collect();
    let key = parts.ok_or_else(|| CookError::Rule {
        rule: "asset_pack",
        input: file.to_owned(),
        detail: "the path is not UTF-8".to_owned(),
    })?;
    if key.iter().any(|part| part.starts_with('.')) {
        return Ok(None);
    }
    Ok(Some(key.join("/")))
}

/// Files matching one `directory/*.extension` pattern below `root`.
fn expand(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, CookError> {
    let unsupported = || CookError::BadGlob {
        pattern: pattern.to_owned(),
    };
    let (directory, file_pattern) = match pattern.rsplit_once('/') {
        Some((directory, file_pattern)) => (root.join(directory), file_pattern),
        // No directory part: the inputs sit directly under the root, which is
        // how a tree that is itself the shaders directory is cooked.
        None => (root.to_owned(), pattern),
    };
    let (prefix, suffix) = file_pattern.split_once('*').ok_or_else(unsupported)?;

    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        // A root without this directory has no inputs for the rule. A crate that
        // cooks shaders but ships no shaders of its own is not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(CookError::Io {
                path: directory,
                source,
            })
        }
    };

    let mut matched = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CookError::Io {
            path: directory.clone(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(prefix) && name.ends_with(suffix) && entry.path().is_file() {
            matched.push(entry.path());
        }
    }
    matched.sort();
    Ok(matched)
}

/// Mark `output` as cooked against `inputs`, whether or not the rule rewrote
/// it; an output the rule never produced is left for the next run to find
/// missing.
///
/// A tool may leave an output untouched when what it would write is
/// identical - `slangc` does - so without a stamp an edit that changes nothing
/// in the output would leave it older than its input and re-cooked every run.
/// The stamp is the newest input's own timestamp, and only moves the output
/// forward to it: a clock stamp would be wrong, because file times are coarser
/// than `SystemTime::now()`, so an input saved just after the cook can carry
/// an earlier time and never be cooked.
fn stamp_output<'a>(
    output: &Path,
    inputs: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<(), CookError> {
    let modified = |path: &Path| {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .map_err(|source| CookError::Io {
                path: path.to_owned(),
                source,
            })
    };
    let output_modified = match fs::metadata(output).and_then(|meta| meta.modified()) {
        Ok(time) => time,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(CookError::Io {
                path: output.to_owned(),
                source,
            })
        }
    };
    let mut newest_input = None;
    for input in inputs {
        let time = modified(input)?;
        newest_input =
            Some(newest_input.map_or(time, |newest: std::time::SystemTime| newest.max(time)));
    }
    let Some(newest_input) = newest_input.filter(|newest| *newest > output_modified) else {
        return Ok(());
    };
    fs::File::options()
        .write(true)
        .open(output)
        .and_then(|file| file.set_modified(newest_input))
        .map_err(|source| CookError::Io {
            path: output.to_owned(),
            source,
        })
}

/// Whether `output` is missing or older than `input`.
///
/// An output that exists but cannot be read is treated as stale: the rule
/// overwrites it, which is the only way a corrupt output ever recovers.
fn is_stale(input: &Path, output: &Path) -> Result<bool, CookError> {
    let Ok(output_modified) = fs::metadata(output).and_then(|meta| meta.modified()) else {
        return Ok(true);
    };
    let input_modified = fs::metadata(input)
        .and_then(|meta| meta.modified())
        .map_err(|source| CookError::Io {
            path: input.to_owned(),
            source,
        })?;
    Ok(input_modified > output_modified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_asset_pack_indexes_every_file_by_its_relative_path() {
        let directory = std::env::temp_dir()
            .join("pill_assets_tests")
            .join(format!("{}_asset_pack", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let source = directory.join("res");
        fs::create_dir_all(source.join("models")).expect("scratch directory");
        fs::write(source.join("models").join("a.obj"), b"mesh").expect("input");
        fs::write(source.join("b.png"), b"image").expect("input");
        // A metadata update that crashed part-way through leaves this behind.
        fs::write(
            source.join(".b.png.meta.4242-0.tmp"),
            b"{\"format_version\":1}",
        )
        .expect("input");
        let output = directory.join("out").join("assets.pillpack");

        let packed = write_asset_pack(&source, &output).expect("pack");
        let pack = fs::read(&output).expect("written");

        assert_eq!(packed.len(), 2);
        assert_eq!(&pack[..8], ASSET_PACK_MAGIC);
        assert_eq!(&pack[8..12], &ASSET_PACK_VERSION.to_le_bytes());
        assert_eq!(&pack[12..16], &2u32.to_le_bytes());
        // Sorted: `b.png` before `models/a.obj`; the bytes follow the index.
        assert_eq!(&pack[16..20], &5u32.to_le_bytes());
        assert_eq!(&pack[20..25], b"b.png");
        assert!(pack.ends_with(b"imagemesh"));
        assert!(
            !pack.windows(7).any(|window| window == b"b.png.m"),
            "the leftover metadata temp file is not packed"
        );
        let _ = fs::remove_dir_all(&directory);
    }

    /// `res/editor/` is the editor's own content: a game pack never
    /// carries it, and a game file that merely starts with the same letters
    /// ("editorial.txt") still packs.
    #[test]
    fn an_asset_pack_skips_the_editor_directory() {
        let directory = scratch("editor_directory");
        let source = directory.join("res");
        fs::create_dir_all(source.join("editor").join("fonts")).expect("scratch directory");
        fs::create_dir_all(source.join("ui")).expect("scratch directory");
        fs::write(source.join("editor").join("editor.theme"), b"{}").expect("input");
        fs::write(source.join("editor").join("fonts").join("a.ttf"), b"font").expect("input");
        fs::write(source.join("ui").join("accent.theme"), b"{}").expect("input");
        fs::write(source.join("editorial.txt"), b"prose").expect("input");
        let output = directory.join("assets.pillpack");

        let packed = write_asset_pack(&source, &output).expect("pack");
        let pack = fs::read(&output).expect("written");

        let keys: Vec<String> = packed
            .iter()
            .map(|path| {
                path.strip_prefix(&source)
                    .expect("below res")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(keys, ["editorial.txt", "ui/accent.theme"]);
        assert!(!pack.windows(12).any(|window| window == b"editor.theme"));
        let _ = fs::remove_dir_all(&directory);
    }

    /// A module ships its resources beside its code, not in the project's
    /// `res`: a packed build has no module directory left to resolve them
    /// from, so the pack carries them under their own root's relative names.
    #[test]
    fn a_module_roots_files_are_packed_beside_the_projects() {
        let directory = scratch("module_roots");
        let source = directory.join("res");
        fs::create_dir_all(&source).expect("scratch directory");
        fs::write(source.join("project.txt"), b"project").expect("input");
        let resources = directory.join("module").join("res");
        fs::create_dir_all(resources.join("defaults")).expect("scratch directory");
        fs::write(
            resources.join("defaults").join("pill.pbr.material"),
            b"material",
        )
        .expect("input");
        let shaders = directory.join("module").join("shader_sources");
        fs::create_dir_all(shaders.join("pbr").join("shaders")).expect("scratch directory");
        fs::write(shaders.join("pbr").join("shaders").join("x.wgsl"), b"stage").expect("input");
        let output = directory.join("assets.pillpack");

        let packed =
            write_asset_pack_with_roots(&source, &[resources.clone(), shaders.clone()], &output)
                .expect("pack");
        let pack = fs::read(&output).expect("written");

        assert_eq!(packed.len(), 3);
        assert_eq!(&pack[12..16], &3u32.to_le_bytes());
        for key in [
            "project.txt",
            "defaults/pill.pbr.material",
            "pbr/shaders/x.wgsl",
        ] {
            assert!(
                pack.windows(key.len())
                    .any(|window| window == key.as_bytes()),
                "{key} is indexed"
            );
        }
        let _ = fs::remove_dir_all(&directory);
    }

    /// A project file of a module resource's name is the project's - the store
    /// tries the project's directory before a module root - and the flat index
    /// cannot hold both.
    #[test]
    fn a_project_file_shadows_a_module_resource_of_the_same_name() {
        let directory = scratch("shadowed_module_resource");
        let source = directory.join("res");
        fs::create_dir_all(source.join("defaults")).expect("scratch directory");
        fs::write(source.join("defaults").join("x.material"), b"project").expect("input");
        let resources = directory.join("module").join("res");
        fs::create_dir_all(resources.join("defaults")).expect("scratch directory");
        fs::write(resources.join("defaults").join("x.material"), b"module").expect("input");
        let output = directory.join("assets.pillpack");

        let packed = write_asset_pack_with_roots(&source, &[resources], &output).expect("pack");
        let pack = fs::read(&output).expect("written");

        assert_eq!(packed, [source.join("defaults").join("x.material")]);
        assert!(pack.windows(7).any(|window| window == b"project"));
        assert!(!pack.windows(6).any(|window| window == b"module"));
        let _ = fs::remove_dir_all(&directory);
    }

    /// Two roots shipping one name resolve like their module's registration
    /// order: the first root keeps it, as the first declared root answers a
    /// lookup first.
    #[test]
    fn the_first_module_root_wins_a_name_two_roots_ship() {
        let directory = scratch("overlapping_module_roots");
        let first = directory.join("first");
        let second = directory.join("second");
        fs::create_dir_all(&first).expect("scratch directory");
        fs::create_dir_all(&second).expect("scratch directory");
        fs::write(first.join("shared.txt"), b"first").expect("input");
        fs::write(second.join("shared.txt"), b"second").expect("input");
        let output = directory.join("assets.pillpack");

        let packed = write_asset_pack_with_roots(
            &directory.join("missing_res"),
            &[first.clone(), second],
            &output,
        )
        .expect("pack");
        let pack = fs::read(&output).expect("written");

        assert_eq!(packed, [first.join("shared.txt")]);
        assert!(pack.windows(5).any(|window| window == b"first"));
        assert!(!pack.windows(6).any(|window| window == b"second"));
        let _ = fs::remove_dir_all(&directory);
    }

    /// A project with no `res` still ships what its modules declare.
    #[test]
    fn a_missing_project_directory_still_packs_module_roots() {
        let directory = scratch("module_only_pack");
        let resources = directory.join("module").join("res");
        fs::create_dir_all(&resources).expect("scratch directory");
        fs::write(resources.join("defaults.material"), b"material").expect("input");
        let output = directory.join("assets.pillpack");

        let packed = write_asset_pack_with_roots(
            &directory.join("res"),
            std::slice::from_ref(&resources),
            &output,
        )
        .expect("pack");
        let pack = fs::read(&output).expect("written");

        assert_eq!(packed, [resources.join("defaults.material")]);
        assert_eq!(&pack[12..16], &1u32.to_le_bytes());
        let _ = fs::remove_dir_all(&directory);
    }

    /// Writes `body` to `<temp>/pill_assets_tests/<name>` and returns the directory.
    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir()
            .join("pill_assets_tests")
            .join(format!("{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    /// A rule that records what it was asked to build.
    struct Recording {
        built: std::cell::RefCell<Vec<PathBuf>>,
    }

    impl Rule for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn input_glob(&self) -> &'static str {
            "shaders/*.in"
        }

        fn output_for(&self, input: &Path) -> PathBuf {
            input.with_extension("out")
        }

        fn build(&self, input: &Path, output: &Path) -> Result<(), CookError> {
            fs::write(output, b"cooked").map_err(|source| CookError::Io {
                path: output.to_owned(),
                source,
            })?;
            self.built.borrow_mut().push(input.to_owned());
            Ok(())
        }
    }

    #[test]
    fn expand_matches_only_the_extension_and_the_named_directory() {
        let root = scratch("expand");
        fs::create_dir_all(root.join("shaders")).expect("shaders");
        fs::create_dir_all(root.join("elsewhere")).expect("elsewhere");
        fs::write(root.join("shaders/a.hlsl"), b"").expect("a");
        fs::write(root.join("shaders/b.hlsl"), b"").expect("b");
        fs::write(root.join("shaders/note.txt"), b"").expect("note");
        fs::write(root.join("elsewhere/c.hlsl"), b"").expect("c");

        let matched = expand(&root, "shaders/*.hlsl").expect("expansion");

        let names: Vec<_> = matched
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.hlsl", "b.hlsl"]);
    }

    #[test]
    fn a_pattern_without_a_directory_matches_the_root_itself() {
        let root = scratch("flat");
        fs::create_dir_all(root.join("include")).expect("include");
        fs::write(root.join("a.hlsl"), b"").expect("a");
        fs::write(root.join("include/header.hlsl"), b"").expect("header");

        let matched = expand(&root, "*.hlsl").expect("expansion");

        let names: Vec<_> = matched
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // The nested header is not a match: the pattern is one level deep even
        // when that level is the root.
        assert_eq!(names, ["a.hlsl"]);
    }

    #[test]
    fn a_missing_directory_is_not_a_failure() {
        let root = scratch("missing");

        assert!(expand(&root, "shaders/*.hlsl")
            .expect("expansion")
            .is_empty());
    }

    #[test]
    fn run_rebuilds_only_stale_outputs() {
        let root = scratch("stale");
        fs::create_dir_all(root.join("shaders")).expect("shaders");
        fs::write(root.join("shaders/triangle.in"), b"source").expect("input");
        let rule = Recording {
            built: std::cell::RefCell::new(Vec::new()),
        };
        let pipeline = Pipeline {
            root: root.clone(),
            rules: vec![Box::new(rule)],
        };

        let first = pipeline.run().expect("first run");
        assert_eq!(first.rebuilt.len(), 1);
        assert!(first.skipped.is_empty());

        // Nothing changed, so the second run leaves the output alone.
        let second = pipeline.run().expect("second run");
        assert!(second.rebuilt.is_empty());
        assert_eq!(second.skipped.len(), 1);
        assert_eq!(second.discovered, first.discovered);
    }

    /// A rule whose tool leaves an identical output untouched must not leave it
    /// stale: the next run has to skip it rather than cook it again.
    #[test]
    fn an_output_the_rule_left_untouched_is_fresh_afterwards() {
        /// Cooks `shaders/*.in`, writing the output only when it is missing.
        struct WritesOnlyWhenMissing;

        impl Rule for WritesOnlyWhenMissing {
            fn name(&self) -> &'static str {
                "writes_only_when_missing"
            }

            fn input_glob(&self) -> &'static str {
                "shaders/*.in"
            }

            fn output_for(&self, input: &Path) -> PathBuf {
                input.with_extension("out")
            }

            fn build(&self, _input: &Path, output: &Path) -> Result<(), CookError> {
                if !output.exists() {
                    fs::write(output, b"cooked").map_err(|source| CookError::Io {
                        path: output.to_owned(),
                        source,
                    })?;
                }
                Ok(())
            }
        }

        let root = scratch("untouched");
        fs::create_dir_all(root.join("shaders")).expect("shaders");
        let input = root.join("shaders/triangle.in");
        fs::write(&input, b"source").expect("input");
        let pipeline = Pipeline {
            root: root.clone(),
            rules: vec![Box::new(WritesOnlyWhenMissing)],
        };
        pipeline.run().expect("first run");

        // Age the output past the input, as an edit to the input would; the
        // rule will see an existing output and not rewrite it.
        let output = input.with_extension("out");
        let earlier = std::time::SystemTime::now() - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&output)
            .and_then(|file| file.set_modified(earlier))
            .expect("age the output");
        let second = pipeline.run().expect("second run");
        assert_eq!(second.rebuilt.len(), 1);

        // Without the stamp the output would still be older than the input and
        // this run would cook it again.
        let third = pipeline.run().expect("third run");
        assert!(third.rebuilt.is_empty());
        assert_eq!(third.skipped.len(), 1);
    }

    /// A rule can declare inputs its glob does not match - the shader rule's
    /// `#include` headers - and a newer one has to make the output stale.
    #[test]
    fn a_newer_extra_input_rebuilds_the_output() {
        /// Cooks `shaders/*.in`, re-cooked when `include/` changes.
        struct WithIncludes;

        impl Rule for WithIncludes {
            fn name(&self) -> &'static str {
                "with_includes"
            }

            fn input_glob(&self) -> &'static str {
                "shaders/*.in"
            }

            fn output_for(&self, input: &Path) -> PathBuf {
                input.with_extension("out")
            }

            fn extra_inputs(&self, input: &Path) -> Result<Vec<PathBuf>, CookError> {
                let parent = input.parent().expect("matched inputs live in a directory");
                walk_files(&parent.join("include"))
            }

            fn build(&self, input: &Path, output: &Path) -> Result<(), CookError> {
                fs::copy(input, output).map_err(|source| CookError::Io {
                    path: output.to_owned(),
                    source,
                })?;
                Ok(())
            }
        }

        let root = scratch("extra_inputs");
        fs::create_dir_all(root.join("shaders/include")).expect("shaders");
        fs::write(root.join("shaders/triangle.in"), b"source").expect("input");
        let header = root.join("shaders/include/common.h");
        fs::write(&header, b"header").expect("header");
        let pipeline = Pipeline {
            root: root.clone(),
            rules: vec![Box::new(WithIncludes)],
        };

        let first = pipeline.run().expect("first run");
        assert_eq!(first.rebuilt.len(), 1);

        // Nothing moved: the matched input is untouched, so the output stays.
        let second = pipeline.run().expect("second run");
        assert!(second.rebuilt.is_empty());
        assert_eq!(second.skipped.len(), 1);

        // The header the rule reads is newer than the output, so the next run
        // re-cooks even though the matched input never moved. The timestamp is
        // set by hand: a file written a moment later can still carry the same
        // mtime as the output (file timestamps are quantized), and the
        // staleness check is strict.
        fs::write(&header, b"header v2").expect("header edit");
        let newer = std::time::SystemTime::now() + Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&header)
            .and_then(|file| file.set_modified(newer))
            .expect("age the header");
        let third = pipeline.run().expect("third run");
        assert_eq!(third.rebuilt.len(), 1);
    }

    #[test]
    fn the_manifest_adds_only_the_rules_it_names() {
        let root = scratch("manifest");
        let manifest = root.join("cook.rules");
        fs::write(
            &manifest,
            "# meshes stay uncooked here\n\nobj_to_cooked_mesh  # and so does this one\n",
        )
        .expect("manifest");

        let pipeline = Pipeline::new(root.clone())
            .with_manifest(&manifest, |name| {
                (name == "obj_to_cooked_mesh").then(|| {
                    Box::new(Recording {
                        built: std::cell::RefCell::new(Vec::new()),
                    }) as Box<dyn Rule>
                })
            })
            .expect("manifest resolves");

        let names: Vec<_> = pipeline.rules.iter().map(|rule| rule.name()).collect();
        assert_eq!(names, ["hlsl_to_wgsl", "recording"]);

        // An unknown name is a hard error rather than a silently skipped rule.
        fs::write(&manifest, "nonsense\n").expect("manifest");
        assert!(matches!(
            Pipeline::new(root).with_manifest(&manifest, |_| None),
            Err(CookError::UnknownRule { .. })
        ));
    }

    #[test]
    fn a_manifest_that_is_not_there_leaves_the_rules_alone() {
        let root = scratch("no_manifest");

        let pipeline = Pipeline::new(root.clone())
            .with_manifest(&root.join("absent.rules"), |_| None)
            .expect("absent manifest");

        assert_eq!(pipeline.rules.len(), default_rules().len());
    }
}

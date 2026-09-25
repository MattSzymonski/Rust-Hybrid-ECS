//! Rule-based asset cooking, driven from build scripts.
//!
//! # Responsibilities
//!
//! - Discover each rule's inputs by glob, relative to a pipeline root.
//! - Rebuild an output only when it is missing or older than its input.
//! - Report what was discovered, rebuilt and skipped, and how long each rebuilt
//!   output took.
//!
//! # Design
//!
//! Dependency-free on purpose, like `pill_hot_scan`: this crate runs from build
//! scripts, so a dependency here is paid on every clean build of every crate
//! that cooks. Globbing covers the `directory/*.extension` shape the rules use,
//! and timestamps come from `std::fs`.
//!
//! Shader cooking is not optional. [`default_rules`] always carries
//! [`HlslToWgsl`], because a shader that was never cooked is a shader that does
//! not exist, and a source of truth in HLSL only means anything if the build
//! runs the compiler. Cooking meshes and textures is opt-in instead:
//! [`Pipeline::with_manifest`] adds such a rule only when a manifest names it
//! and the caller can supply it, so a project that decodes those at runtime
//! pays neither the cook time nor the decoder's dependencies.
//!
//! # Examples
//!
//! A build script cooks the shaders under `src/shaders` and reports every
//! discovered input back to cargo:
//!
//! ```no_run
//! let root = std::path::PathBuf::from("src");
//! let stats = pill_assets::Pipeline::new(root).run()?;
//! for input in &stats.discovered {
//!     println!("cargo:rerun-if-changed={}", input.display());
//! }
//! # Ok::<(), pill_assets::CookError>(())
//! ```

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The cooking rules this crate ships: the always-on shader rule, and whatever
/// a caller resolves to add to it.
pub mod rules;

pub use rules::{default_rules, HlslToWgsl};

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
    /// Only the `directory/*.extension` shape is supported; that is all the
    /// rules use, and a general glob engine would be a dependency every build
    /// script pays for.
    fn input_glob(&self) -> &'static str;

    /// The output `input` produces. Its timestamp decides staleness.
    fn output_for(&self, input: &Path) -> PathBuf;

    /// Side outputs written in addition to [`Rule::output_for`].
    fn extra_outputs(&self, _input: &Path) -> Vec<PathBuf> {
        Vec::new()
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
    /// Outputs that were already newer than their input.
    pub skipped: Vec<PathBuf>,
    /// One `(input, output, rule)` per produced file, rebuilt or skipped.
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

    /// Add the optional rules `manifest` names, resolved through `resolve`.
    ///
    /// The manifest is plain text: one rule name per line, with `#` comments and
    /// blank lines ignored. This is where a project opts into cooking meshes and
    /// textures rather than decoding them at runtime; a manifest that is not
    /// there is not an error, it just leaves the optional rules off, which is
    /// the posture for a project that loads its assets as they are. `resolve` is
    /// the caller's registry, so this crate never has to carry the decoders the
    /// optional rules need.
    ///
    /// # Errors
    ///
    /// Returns [`CookError::Io`] when the manifest exists but cannot be read, and
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
                if !is_stale(&input, &output)? {
                    stats.skipped.push(output);
                    continue;
                }
                let started = Instant::now();
                rule.build(&input, &output)?;
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

/// Files matching one `directory/*.extension` pattern below `root`.
fn expand(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, CookError> {
    let unsupported = || CookError::BadGlob {
        pattern: pattern.to_owned(),
    };
    let (directory, file_pattern) = pattern.rsplit_once('/').ok_or_else(unsupported)?;
    let (prefix, suffix) = file_pattern.split_once('*').ok_or_else(unsupported)?;
    let directory = root.join(directory);

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

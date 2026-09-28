//! HLSL/Slang source in, WGSL out, through the `slangc` CLI.
//!
//! # Responsibilities
//!
//! - Compile every `shaders/*.hlsl` the pipeline root holds.
//! - Infer the stage and entry point from the file name, refusing any name that
//!   does not declare one.
//! - Surface `slangc`'s own diagnostics when a compile fails.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{walk_files, CookError, Rule};

/// Compiles `shaders/*.hlsl` to WGSL beside the source.
///
/// The stage and entry point come from the file name: `*_vertex.hlsl` is a
/// vertex stage entered at `vs_main`, `*_fragment.hlsl` a fragment stage entered
/// at `fs_main`. Any other name fails instead of guessing, so adding a stage is
/// a decision made here rather than something inferred from a typo.
///
/// Sources `#include` an `include/` beside them, which costs nothing to set up
/// and needs no configuration. [`HlslToWgsl::with_include`] adds directories for
/// a header that several trees share: without it the header has to be copied
/// into each `include/`, and copies drift.
pub struct HlslToWgsl {
    /// Directories handed to `slangc -I`, searched after the source's own
    /// directory and `include/`.
    include_dirs: Vec<PathBuf>,
}

impl HlslToWgsl {
    /// The rule with no include path: sources resolve against their own tree.
    pub fn new() -> Self {
        Self {
            include_dirs: Vec::new(),
        }
    }

    /// Also search `directory` for the headers the sources `#include`.
    ///
    /// Repeatable; directories are searched in the order given.
    #[must_use]
    pub fn with_include(mut self, directory: impl Into<PathBuf>) -> Self {
        self.include_dirs.push(directory.into());
        self
    }
}

impl Default for HlslToWgsl {
    fn default() -> Self {
        Self::new()
    }
}

impl Rule for HlslToWgsl {
    fn name(&self) -> &'static str {
        "hlsl_to_wgsl"
    }

    fn input_glob(&self) -> &'static str {
        // Top level only: `shaders/include/*.hlsl` are headers the sources
        // `#include`, and a build script reports those as inputs itself.
        "shaders/*.hlsl"
    }

    fn output_for(&self, input: &Path) -> PathBuf {
        input.with_extension("wgsl")
    }

    fn extra_inputs(&self, input: &Path) -> Result<Vec<PathBuf>, CookError> {
        // Everything the sources `#include` is an input, whether it sits in the
        // tree's own `include/` or in one shared with other trees. None of it
        // matches the rule's glob, so without this a header edit would leave
        // every output looking fresh.
        let mut headers = Vec::new();
        if let Some(parent) = input.parent() {
            headers.extend(walk_files(&parent.join("include"))?);
        }
        for directory in &self.include_dirs {
            headers.extend(walk_files(directory)?);
        }
        Ok(headers)
    }

    fn build(&self, input: &Path, output: &Path) -> Result<(), CookError> {
        let (entry, stage) = stage_for(input)?;
        let mut slangc = Command::new("slangc");
        slangc
            .arg(input)
            .args(["-target", "wgsl", "-entry", entry, "-stage", stage]);
        for directory in &self.include_dirs {
            slangc.arg("-I").arg(directory);
        }
        // `-O0 -g` keep the intermediate variable names, so the emitted WGSL
        // still reads like the HLSL it came from.
        let cooked = slangc.args(["-O0", "-g", "-o"]).arg(output).output();

        let cooked = match cooked {
            Ok(cooked) => cooked,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(CookError::Rule {
                    rule: self.name(),
                    input: input.to_owned(),
                    detail: "slangc is not on PATH; install Slang from \
                             https://github.com/shader-slang/slang/releases"
                        .to_owned(),
                })
            }
            Err(error) => {
                return Err(CookError::Rule {
                    rule: self.name(),
                    input: input.to_owned(),
                    detail: error.to_string(),
                })
            }
        };
        if !cooked.status.success() {
            return Err(CookError::Rule {
                rule: self.name(),
                input: input.to_owned(),
                detail: format!(
                    "slangc exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    cooked.status.code(),
                    String::from_utf8_lossy(&cooked.stdout).trim_end(),
                    String::from_utf8_lossy(&cooked.stderr).trim_end(),
                ),
            });
        }
        Ok(())
    }
}

/// The entry point and stage `file` declares through its name.
///
/// # Errors
///
/// Returns [`CookError::Rule`] when the name carries neither `_vertex` nor
/// `_fragment`, because compiling with a guessed stage would produce a shader
/// that fails at pipeline creation instead of here.
fn stage_for(file: &Path) -> Result<(&'static str, &'static str), CookError> {
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.ends_with("_vertex.hlsl") {
        return Ok(("vs_main", "vertex"));
    }
    if name.ends_with("_fragment.hlsl") {
        return Ok(("fs_main", "fragment"));
    }
    Err(CookError::Rule {
        rule: "hlsl_to_wgsl",
        input: file.to_owned(),
        detail: format!(
            "cannot infer a stage from {name}: name it *_vertex.hlsl or *_fragment.hlsl"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_sits_beside_the_source() {
        let rule = HlslToWgsl::new();

        assert_eq!(
            rule.output_for(Path::new("shaders/default_vertex.hlsl")),
            PathBuf::from("shaders/default_vertex.wgsl")
        );
        assert_eq!(rule.input_glob(), "shaders/*.hlsl");
    }

    #[test]
    fn an_include_path_is_searched_after_the_trees_own() {
        let shared = Path::new("../common_shaders");
        let configured = HlslToWgsl::new().with_include(shared);

        assert_eq!(configured.include_dirs, vec![PathBuf::from(shared)]);
        assert!(HlslToWgsl::new().include_dirs.is_empty());
    }

    #[test]
    fn the_stage_comes_from_the_name_suffix() {
        assert_eq!(
            stage_for(Path::new("shaders/unlit_fragment.hlsl")).expect("fragment"),
            ("fs_main", "fragment")
        );
        assert_eq!(
            stage_for(Path::new("shaders/default_vertex.hlsl")).expect("vertex"),
            ("vs_main", "vertex")
        );
    }

    #[test]
    fn a_name_without_a_stage_is_refused() {
        let refused = stage_for(Path::new("shaders/helpers.hlsl"));

        assert!(matches!(refused, Err(CookError::Rule { .. })));
    }
}

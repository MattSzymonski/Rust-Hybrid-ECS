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

use crate::{CookError, Rule};

/// Compiles `shaders/*.hlsl` to WGSL beside the source.
///
/// The stage and entry point come from the file name: `*_vertex.hlsl` is a
/// vertex stage entered at `vs_main`, `*_fragment.hlsl` a fragment stage entered
/// at `fs_main`. Any other name fails instead of guessing, so adding a stage is
/// a decision made here rather than something inferred from a typo.
pub struct HlslToWgsl;

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

    fn build(&self, input: &Path, output: &Path) -> Result<(), CookError> {
        let (entry, stage) = stage_for(input)?;
        let cooked = Command::new("slangc")
            .arg(input)
            .args(["-target", "wgsl", "-entry", entry, "-stage", stage])
            // `-O0 -g` keep the intermediate variable names, so the emitted WGSL
            // still reads like the HLSL it came from.
            .args(["-O0", "-g", "-o"])
            .arg(output)
            .output();

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
        let rule = HlslToWgsl;

        assert_eq!(
            rule.output_for(Path::new("shaders/default_vertex.hlsl")),
            PathBuf::from("shaders/default_vertex.wgsl")
        );
        assert_eq!(rule.input_glob(), "shaders/*.hlsl");
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

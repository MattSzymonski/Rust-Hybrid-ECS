//! Native HLSL-to-WGSL conversion through the Slang compiler.
//!
//! # Responsibilities
//!
//! - Infers vertex or fragment entry points from source filename suffixes.
//! - Runs `slangc` and preserves useful compiler diagnostics on failure.
//!
//! # Design
//!
//! Only direct children of the shaders directory are entry-point inputs; nested
//! include files remain dependencies captured by the outer generation snapshot.
//! The compiler is a native cooking dependency, never a runtime requirement.

// Standard library
use std::path::{Path, PathBuf};
use std::process::Command;

// External crates
use anyhow::{bail, Context, Result};

use crate::pill_assets::Rule;

// =============================================================================
// Conversion Rule
// =============================================================================

/// HLSL → WGSL via the `slangc` CLI. Stage and entry-point inferred from
/// filename suffix:
///   `*_vertex.hlsl`   → `-stage vertex   -entry vs_main`
///   `*_fragment.hlsl` → `-stage fragment -entry fs_main`
///
/// Other suffixes fail fast with a clear message; add a new naming convention
/// (or a richer rule) before introducing compute / mesh stages.
pub struct HlslToWgsl;

impl Rule for HlslToWgsl {
    fn name(&self) -> &'static str {
        "hlsl_to_wgsl"
    }

    fn input_glob(&self) -> &'static str {
        // Top-level only; `shaders/include/*.hlsl` are header files (#include'd).
        "shaders/*.hlsl"
    }

    fn output_for(&self, input: &Path) -> PathBuf {
        input.with_extension("wgsl")
    }

    fn build(&self, input: &Path, output: &Path) -> Result<()> {
        Self::build_with_compiler(input, output, std::ffi::OsStr::new("slangc"))
    }
}

impl HlslToWgsl {
    /// Invoke the selected compiler and attach stdout/stderr to failed builds.
    ///
    /// Keeping the executable injectable lets tests cover a missing compiler without
    /// changing the process-wide PATH.
    fn build_with_compiler(input: &Path, output: &Path, compiler: &std::ffi::OsStr) -> Result<()> {
        let (entry, stage) = infer_stage(input)?;

        let result = Command::new(compiler)
            .args([
                input.to_str().context("input path is not UTF-8")?,
                "-target",
                "wgsl",
                "-entry",
                entry,
                "-stage",
                stage,
                // -O0 + -g preserve intermediate variable names so the emitted
                // WGSL is debuggable in browser devtools.
                "-O0",
                "-g",
                "-o",
                output.to_str().context("output path is not UTF-8")?,
            ])
            .output();

        let out = match result {
            Ok(out) => out,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "slangc not found on PATH. Install Slang from https://github.com/shader-slang/slang/releases and add slangc to PATH."
            ),
            Err(e) => return Err(e).context("failed to spawn slangc"),
        };

        if !out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "slangc exited {:?} for {input:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
                out.status.code()
            );
        }

        Ok(())
    }
}

// =============================================================================
// Stage Selection
// =============================================================================

/// Map `_vertex` and `_fragment` stems to Slang entry/stage pairs.
///
/// Unknown suffixes fail rather than guessing an incompatible shader stage.
fn infer_stage(input: &Path) -> Result<(&'static str, &'static str)> {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("can't read filename stem of {input:?}"))?;

    if stem.ends_with("_vertex") {
        Ok(("vs_main", "vertex"))
    } else if stem.ends_with("_fragment") {
        Ok(("fs_main", "fragment"))
    } else {
        bail!(
            "{input:?}: can't infer shader stage from filename. Expected suffix `_vertex` or `_fragment`."
        )
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    // Current crate
use super::*;
    /// A missing compiler must produce the installation hint expected by the cooker UI.
    #[test]
    fn missing_compiler_is_actionable() {
        let missing = std::env::temp_dir().join("pill-definitely-missing-slangc.exe");
        let error = HlslToWgsl::build_with_compiler(
            Path::new("test_vertex.hlsl"),
            Path::new("unused.wgsl"),
            missing.as_os_str(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("slangc not found"));
    }
}

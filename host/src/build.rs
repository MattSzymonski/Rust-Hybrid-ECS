//! Game-module build execution and output-path resolution.
//!
//! Build processes inherit the host's standard streams so compiler progress
//! and diagnostics remain visible in the terminal that launched the host.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{GameModuleBackend, GameModuleConfig};

/// Build the selected game module and return its expected output artifact.
pub(crate) fn build_game_module(
    workspace_root: &Path,
    config: &GameModuleConfig,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    println!("[host] Building {} module...", config.name);

    // GameModuleConfig stores commands as static slices so callers can define
    // both Cargo and dotnet builds without shell-specific quoting. The first
    // item is always the executable; every remaining item is passed verbatim.
    let program = config.build_command[0];
    let arguments = &config.build_command[1..];

    // Run from the workspace root because configured paths and Cargo package
    // selection are workspace-relative. `status` deliberately inherits the
    // host's stdout and stderr instead of capturing them, which keeps compiler
    // progress, warnings, and errors visible during startup and hot reload.
    let status = Command::new(program)
        .args(arguments)
        .current_dir(workspace_root)
        .status()?;

    // A failed compiler must stop the load transaction. During hot reload the
    // caller handles this error by leaving the current game module untouched.
    if !status.success() {
        return Err(format!(
            "Build command failed for '{}' with status {}",
            config.name, status
        )
        .into());
    }

    // The build command itself is backend-agnostic, but each backend names and
    // locates its loadable artifact differently. Native outputs use platform
    // naming conventions; managed outputs always use an assembly `.dll`.
    let output_path = match &config.backend {
        GameModuleBackend::NativeLibrary {
            library_name,
            output_subdirectory,
        } => workspace_root
            .join(output_subdirectory)
            .join(native_library_filename(library_name)),
        GameModuleBackend::CSharp(config) => workspace_root
            .join(config.game_output_subdirectory)
            .join(format!("{}.dll", config.game_assembly_name)),
    };

    // A successful process exit does not guarantee that configuration points
    // at the artifact it produced. Validate the resolved path here so loading
    // errors identify an output-directory mismatch rather than an opaque DLL
    // or managed-runtime failure later in the startup sequence.
    if !output_path.exists() {
        return Err(format!(
            "Shared library not found at expected path: {}\n\
             Build succeeded but the output was not where we expected. \
             Check the selected backend output directory in GameModuleConfig.",
            output_path.display()
        )
        .into());
    }

    Ok(output_path)
}

/// Return the platform-specific filename produced for a native library.
fn native_library_filename(library_name: &str) -> String {
    // Cargo follows each platform's conventional dynamic-library prefix and
    // extension. Keeping this mapping here prevents backend orchestration from
    // accumulating operating-system-specific branches.
    if cfg!(target_os = "windows") {
        format!("{library_name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{library_name}.dylib")
    } else {
        format!("lib{library_name}.so")
    }
}

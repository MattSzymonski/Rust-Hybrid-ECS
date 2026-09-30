//! The master renderer's data, registered by the host in every posture.
//!
//! Interim (renderer data split, stages 4-5): stage 6 makes the data crate an
//! extension the host loads, and removes this module.
//!
//! # Responsibilities
//!
//! - Register the master renderer's data crate (`pill_master_renderer_data`):
//!   its components, asset types, resources and
//!   default pipeline with the engine before any extension, project or C#
//!   runtime loads, and report which component types that registered.
//! - Expose those components to a managed project exactly as an extension's
//!   are: a generated mirror file and a native binding for each.
//!
//! # Design
//!
//! A project that declares cameras, meshes and lights has to load whether or
//! not anything draws them. Registering the data here - headless included - is
//! what makes that true: a Rust project finds its components already
//! registered, and the C# bridge binds the renderer's shared components
//! natively in both postures, through the same generated mirrors and
//! `ModuleNative` bindings an extension's components get - nothing about the
//! renderer's types is hand-written in the bridge. Drawing stays the renderer's: this registers no
//! system, and a windowed frontend adds the `rendering` system when it attaches
//! the renderer to a window.
//!
//! The host never unloads, so registering first also leaves the asset types'
//! function tables pointing at code that is always mapped until an artifact
//! that registers them itself re-points them at its own image.

// Standard library
#[cfg(feature = "hot_reload")]
use std::path::Path;

// External crates
#[cfg(feature = "hot_reload")]
use pill_core::error::CSharpError;
use pill_engine::Engine;

// Current crate
#[cfg(feature = "hot_reload")]
use crate::csharp::ModuleExposedComponent;

/// Workspace-relative directory of the renderer data crate; its generated C#
/// mirror lands in `generated/` inside it.
#[cfg(feature = "hot_reload")]
const RENDERER_DATA_DIRECTORY: &str = "extensions/pill_master_renderer_data";

/// Name the generated mirror file is derived from.
#[cfg(feature = "hot_reload")]
const RENDERER_DATA_CRATE_NAME: &str = "pill_master_renderer_data";

/// Register the renderer's plain data with `engine`, and return the names of
/// the component types that registration added.
///
/// Idempotent, like [`pill_master_renderer_data::register`] itself: a project that
/// registers the same data again during its own `init` changes nothing. The
/// names are what a managed project is handed bindings for; a second call
/// registers nothing new and returns none.
pub(crate) fn register_renderer_data(engine: &mut Engine) -> Vec<String> {
    let sequence = engine.world().component_registration_sequence();
    pill_master_renderer_data::register(engine);
    engine.world().registered_component_names_since(sequence)
}

/// Write the renderer data's C# mirror and return the components to bind.
///
/// The mirror goes to `extensions/pill_master_renderer_data/generated/`, which managed projects
/// that draw compile in. Like an extension's, it is written only when its
/// content changed, so a committed copy stays clean.
///
/// # Errors
///
/// Returns [`CSharpError::CodegenFailed`] when a component cannot be mirrored
/// or the file cannot be written.
#[cfg(feature = "hot_reload")]
pub(crate) fn expose_renderer_data_to_csharp(
    workspace_root: &Path,
    engine: &Engine,
    component_names: &[String],
) -> Result<Vec<ModuleExposedComponent>, CSharpError> {
    let exposed = crate::csharp::exposed_components_from_names(engine.world(), component_names);
    crate::csharp::generate_components_csharp(
        &workspace_root.join(RENDERER_DATA_DIRECTORY),
        RENDERER_DATA_CRATE_NAME,
        &exposed,
        &[],
        &[],
        &[],
    )
    .map_err(|message| CSharpError::CodegenFailed { message })?;
    Ok(exposed)
}

//! The startup scene: the one `.scene` a project's settings name, loaded
//! before the project's own `init` adds anything.
//!
//! # Responsibilities
//!
//! - [`StartupScene`]: the request, a world resource the front end queues from
//!   the project's settings before any artifact loads.
//! - [`apply_startup_scene`]: the load itself, run between component
//!   registration and the project's `init` - the only window in which the
//!   component types exist and the project has not yet seeded its own world.
//!
//! # Design
//!
//! A project's `init` fills its world up to the counts it expects, so a scene
//! loaded after it would double every entity; loaded before registration it
//! would find no component types to attach. The generated entry points
//! therefore drain this request right after registering the artifact's
//! components, and a shipping build does the same between its modules and its
//! project.
//!
//! A failure is logged and dropped rather than propagated: a missing or
//! unreadable scene must not stop a project from starting with its own scene.

// Standard library

// Current crate
use crate::engine::Engine;
use crate::resource::Resource;
use crate::scene::SceneLoad;
use crate::world::World;

/// A scene the front end asked the engine to load before the project starts.
///
/// Inserted as a world resource; the load consumes it.
pub struct StartupScene {
    /// The scene's path below the project's asset directory, as
    /// `project_settings.yaml` names it.
    pub path: String,
}

impl Resource for StartupScene {}

/// Ask for `path` to be loaded before the project's `init` runs.
pub fn queue_startup_scene(world: &mut World, path: impl Into<String>) {
    world.insert_resource(StartupScene { path: path.into() });
}

/// Load the queued startup scene, if one was queued.
///
/// `None` when nothing was queued; otherwise what the load made of the
/// document, or the reason it could not be read or instantiated.
pub fn apply_startup_scene(engine: &mut Engine) -> Option<Result<SceneLoad, String>> {
    let queued = match engine.world_mut().remove_resource::<StartupScene>() {
        Ok(queued) => queued?,
        Err(error) => {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                error = %error,
                "[engine] startup scene request could not be taken"
            );
            return None;
        }
    };
    Some(load_startup_scene(engine, &queued.path))
}

/// Load the scene at `path` into `engine`, through the one scene loader every
/// entry point shares.
///
/// References resolve through the world's asset database when the host built
/// one - a shipped pack's index, or a host that indexed the project - so a
/// guid-only reference resolves in a pack build, where no directory is mounted.
fn load_startup_scene(engine: &mut Engine, path: &str) -> Result<SceneLoad, String> {
    let report = crate::asset_loading::load_scene_asset(engine, path)?;
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        path,
        entities = report.entities,
        components = report.components,
        unknown_components = report.unknown_components.len(),
        unresolved_references = report.unresolved_references,
        "[engine] startup scene loaded"
    );
    Ok(report)
}

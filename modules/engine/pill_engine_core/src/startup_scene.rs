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
//! Order is the whole point of this module. A project's `init` fills its world
//! up to the counts it expects, so a scene loaded after it would double every
//! entity; loaded before registration it would find no component types to
//! attach. The generated entry points therefore drain this request right after
//! registering the artifact's components, and a shipping build does the same
//! between its modules and its project.
//!
//! A failure is logged and dropped rather than propagated: a missing or
//! unreadable scene must not stop a project from starting with its own scene.

// Standard library
use std::path::Path;

// Current crate
use crate::asset::AssetManager;
use crate::asset_database::AssetDatabase;
use crate::asset_import_registry::ImportRegistry;
use crate::engine::Engine;
use crate::resource::Resource;
use crate::scene::{load_scene, Scene, SceneLoad};
use crate::world::World;

/// A scene the front end asked the engine to load before the project starts.
///
/// Inserted as a world resource, so it survives nothing: the load consumes it.
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

/// Load the scene at `path` into `engine`, resolving it below the mounted asset
/// directory.
fn load_startup_scene(engine: &mut Engine, path: &str) -> Result<SceneLoad, String> {
    let registry = engine
        .world()
        .get_resource::<ImportRegistry>()
        .cloned()
        .ok_or_else(|| "no asset types are registered".to_string())?;
    if !registry.is_standalone_extension(extension_of(path)) {
        return Err(format!("`{path}` is not a scene file"));
    }
    let document = {
        let assets = engine
            .world_mut()
            .get_resource_mut::<AssetManager>()
            .ok_or_else(|| "the engine has no AssetManager".to_string())?;
        let imported = assets
            .import_standalone::<Scene>(Path::new(path))
            .map_err(|error| format!("{error}"))?;
        assets
            .get(imported.handle)
            .map(|scene| scene.document().clone())
            .ok_or_else(|| "the scene did not stay loaded".to_string())?
    };
    let database = start_database(&registry);
    let report = load_scene(engine, &database, &document);
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

/// The database a startup load resolves references through: the project's asset
/// directory, when one is mounted.
fn start_database(registry: &ImportRegistry) -> AssetDatabase {
    match crate::asset_store::mounted_directory() {
        Some(root) => AssetDatabase::from_directory(&root, registry),
        None => AssetDatabase::new(),
    }
}

/// The extension of `path`, lowercase and without the dot.
fn extension_of(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
}

//! The editor's open scene: which `.scene` the world came from, whether it is
//! dirty, and the file operations behind New, Open, Save and Save As.
//!
//! # Responsibilities
//!
//! - [`SceneSession`]: the open scene's path and guid, and the unsaved marker
//!   every recorded edit raises. A world resource, so a shell or executable
//!   reload keeps it.
//! - [`new_scene`], [`open_scene`], [`write_scene`]: the three file operations,
//!   over the engine's [`save_scene`]/[`load_scene`].
//!
//! # Design
//!
//! The engine owns serialization; this module owns the document's *identity* -
//! the path it was read from and the guid written into it - because that is
//! what Save needs and what the title shows. Opening replaces the world, so the
//! command history and selection go with it: their steps name entities the
//! previous document owned.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_engine::{
    load_scene, render_standalone, save_scene, AssetDatabase, AssetGuid, AssetManager, Engine,
    ImportRegistry, Resource, Scene, SceneLoad, World,
};

// Current crate
use crate::editor_state::{EditorCommand, EditorSelection};

/// The `.scene` the editor's world currently is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenScene {
    /// The scene's path below the asset directory, `/`-separated.
    pub path: String,
    /// The guid written into the file; Save keeps it, so references to the
    /// scene from other data stay valid.
    pub guid: AssetGuid,
}

/// The editor's document state: what is open, and whether it has unsaved
/// edits.
#[derive(Default)]
pub struct SceneSession {
    /// The open scene, or `None` for a world that was never saved.
    pub open: Option<OpenScene>,
    /// Whether a recorded edit happened since the last save or open.
    pub dirty: bool,
}

impl Resource for SceneSession {}

impl SceneSession {
    /// Re-claim the resource across a reload, keeping the live document state.
    pub fn ensure(world: &mut World) {
        world.register_resource::<SceneSession>();
        if world.get_resource::<SceneSession>().is_none() {
            world.insert_resource(Self::default());
        }
    }

    /// What the title bar shows: the scene's file name, or `untitled`, with a
    /// trailing `*` while there are unsaved edits.
    #[must_use]
    pub fn title(&self) -> String {
        let name = match &self.open {
            Some(open) => open
                .path
                .rsplit('/')
                .next()
                .unwrap_or(open.path.as_str())
                .to_string(),
            None => "untitled".to_string(),
        };
        if self.dirty {
            format!("{name} *")
        } else {
            name
        }
    }
}

/// Note that an edit touched the open document.
///
/// Called once per recorded step that applied; the document commands reset
/// the marker themselves.
pub fn mark_dirty(world: &mut World) {
    if let Some(session) = world.get_resource_mut::<SceneSession>() {
        session.dirty = true;
    }
}

/// Start an empty scene: every entity goes, and no file is open.
///
/// Returns how many entities were removed.
pub fn new_scene(engine: &mut Engine) -> usize {
    let removed = clear_world(engine.world_mut());
    let world = engine.world_mut();
    if let Some(session) = world.get_resource_mut::<SceneSession>() {
        session.open = None;
        session.dirty = false;
    }
    reset_document_state(world);
    removed
}

/// Open the scene at `path`, replacing the world's entities.
///
/// The path is below the asset directory; the file's own guid is kept, so
/// saving writes it back unchanged.
///
/// # Errors
///
/// Returns why the file could not be imported, when it carries no guid, or
/// when no asset types are registered to instantiate it.
pub fn open_scene(engine: &mut Engine, path: &str) -> Result<SceneLoad, String> {
    let registry = engine
        .world()
        .get_resource::<ImportRegistry>()
        .cloned()
        .ok_or_else(|| "no asset types are registered".to_string())?;
    let (document, guid) = {
        let assets = engine
            .world_mut()
            .get_resource_mut::<AssetManager>()
            .ok_or_else(|| "the engine has no AssetManager".to_string())?;
        let imported = assets
            .import_standalone::<Scene>(Path::new(path))
            .map_err(|error| format!("{error}"))?;
        let document = assets
            .get(imported.handle)
            .map(|scene| scene.document().clone())
            .ok_or_else(|| "the scene did not stay loaded".to_string())?;
        // A standalone scene carries its guid in the file's own header.
        let guid = imported
            .guid
            .ok_or_else(|| format!("the scene file `{path}` carries no guid"))?;
        (document, guid)
    };
    let database = asset_database(&registry);
    clear_world(engine.world_mut());
    let report = load_scene(engine, &database, &document);
    let world = engine.world_mut();
    if let Some(session) = world.get_resource_mut::<SceneSession>() {
        session.open = Some(OpenScene {
            path: path.to_string(),
            guid,
        });
        session.dirty = false;
    }
    reset_document_state(world);
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        path,
        guid = %guid,
        entities = report.entities,
        components = report.components,
        unknown_components = report.unknown_components.len(),
        unresolved_references = report.unresolved_references,
        "[editor] scene opened"
    );
    Ok(report)
}

/// Write the world to the open scene, or to `path` for a save-as.
///
/// Saving to the open scene keeps its guid; a save-as writes a fresh one unless
/// the target file already exists, in which case its guid is kept so anything
/// referencing it still resolves.
///
/// # Errors
///
/// Returns why the scene could not be rendered, written or loaded back.
pub fn write_scene(engine: &mut Engine, path: Option<&str>) -> Result<OpenScene, String> {
    let registry = engine
        .world()
        .get_resource::<ImportRegistry>()
        .cloned()
        .ok_or_else(|| "no asset types are registered".to_string())?;
    let open = engine
        .world()
        .get_resource::<SceneSession>()
        .and_then(|session| session.open.clone());
    let target = path
        .map(str::to_string)
        .or_else(|| open.as_ref().map(|open| open.path.clone()))
        .ok_or_else(|| "no scene is open; use Save Scene As".to_string())?;
    let root = pill_engine::asset_store::mounted_directory()
        .ok_or("no asset directory is mounted".to_owned())?;
    let file = root.join(&target);

    let (document, report) = {
        let world = engine.world();
        let assets = world
            .get_resource::<AssetManager>()
            .ok_or_else(|| "the engine has no AssetManager".to_string())?;
        save_scene(world, assets, &registry, None)
    };
    // A field the writer cannot express - a handle to an asset that has no
    // guid, because game code created it instead of a file - is a loss the
    // author has to see. The save goes ahead: refusing would make every
    // project that builds its meshes in code unable to save a scene at all.
    if !report.skipped_fields.is_empty() {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            count = report.skipped_fields.len(),
            fields = report.skipped_fields.join(", ").as_str(),
            "[editor] scene fields not written"
        );
    }
    // A component the field layer cannot reach is not in the file at all, so
    // reopening the scene brings the entity back without it. Same bargain: the
    // save goes ahead, and the author is told what stayed behind.
    if !report.skipped_components.is_empty() {
        pill_core::warn!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            count = report.skipped_components.len(),
            components = report.skipped_components.join(", ").as_str(),
            "[editor] scene components not written"
        );
    }
    let guid = match (open.as_ref(), path) {
        // Save to the open scene: keep its guid.
        (Some(open), None) => open.guid,
        // Save As on top of an existing scene: keep the guid in that file.
        _ if file.is_file() => match existing_guid(engine, &target) {
            Some(guid) => guid,
            None => fresh_guid()?,
        },
        _ => fresh_guid()?,
    };
    let bytes = render_standalone::<Scene>(guid, &document)
        .map_err(|error| format!("the scene cannot be rendered: {error}"))?;
    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create `{}`: {error}", parent.display()))?;
        }
    }
    std::fs::write(&file, &bytes)
        .map_err(|error| format!("cannot write `{}`: {error}", file.display()))?;

    // Re-read the file: the world and the loaded asset then agree, and a file
    // the engine cannot read back is caught here rather than at the next open.
    let reopened = {
        let assets = engine
            .world_mut()
            .get_resource_mut::<AssetManager>()
            .ok_or_else(|| "the engine has no AssetManager".to_string())?;
        assets
            .reimport_standalone::<Scene>(Path::new(&target))
            .map_err(|error| format!("`{target}` was written but did not load back: {error}"))?
    };
    if reopened.guid != Some(guid) {
        return Err(format!(
            "`{target}` was written with guid {guid} but reads back as {}",
            reopened
                .guid
                .map_or_else(|| "none".to_owned(), |guid| guid.to_string())
        ));
    }
    let open = OpenScene { path: target, guid };
    let world = engine.world_mut();
    if let Some(session) = world.get_resource_mut::<SceneSession>() {
        session.open = Some(open.clone());
        session.dirty = false;
    }
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        path = open.path.as_str(),
        guid = %open.guid,
        entities = document.entities.len(),
        components = report.components,
        "[editor] scene saved"
    );
    Ok(open)
}

/// The guid the scene at `path` already carries, when it can be read.
fn existing_guid(engine: &mut Engine, path: &str) -> Option<AssetGuid> {
    let assets = engine.world_mut().get_resource_mut::<AssetManager>()?;
    assets
        .import_standalone::<Scene>(Path::new(path))
        .ok()
        .and_then(|imported| imported.guid)
}

/// A fresh guid for a scene file that does not exist yet.
fn fresh_guid() -> Result<AssetGuid, String> {
    AssetGuid::random().map_err(|error| format!("no random source for a scene guid: {error}"))
}

/// The project's asset directory as the database a load resolves references
/// through.
fn asset_database(registry: &ImportRegistry) -> AssetDatabase {
    match pill_engine::asset_store::mounted_directory() {
        Some(root) => AssetDatabase::from_directory(&root, registry),
        None => AssetDatabase::new(),
    }
}

/// Destroy every entity, returning how many there were.
fn clear_world(world: &mut World) -> usize {
    let entities: Vec<_> = world
        .entity_rows()
        .into_iter()
        .map(|row| row.entity)
        .collect();
    for entity in &entities {
        let _ = world.destroy_entity(*entity);
    }
    entities.len()
}

/// Forget the selection and the recorded history: both name entities the
/// document that just replaced this one owned.
fn reset_document_state(world: &mut World) {
    if let Some(selection) = world.get_resource_mut::<EditorSelection>() {
        *selection = EditorSelection::default();
    }
    if let Some(history) = world.get_resource_mut::<crate::history::CommandHistory>() {
        history.clear();
    }
}

/// The commands that change which document is open; they are not undoable
/// edits and never mark the document dirty.
#[must_use]
pub fn is_document_command(command: &EditorCommand) -> bool {
    matches!(
        command,
        EditorCommand::SceneNew
            | EditorCommand::SceneOpen { .. }
            | EditorCommand::SceneSave
            | EditorCommand::SceneSaveAs { .. }
    )
}

/// The file name `name` gets, with the scene extension when it lacks one.
#[must_use]
pub fn scene_file_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.to_ascii_lowercase().ends_with(".scene") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.scene")
    }
}

/// The path a Save As defaults to: the scene that is open, or `scenes/untitled.scene`
/// for a world that was never saved.
#[must_use]
pub fn default_save_path(session: &SceneSession) -> String {
    match &session.open {
        Some(open) => open.path.clone(),
        None => PathBuf::from("scenes")
            .join(scene_file_name("untitled"))
            .to_string_lossy()
            .replace('\\', "/"),
    }
}

//! Loading project assets and scenes from game code, by path.
//!
//! # Responsibilities
//!
//! - [`World::load_asset`]: the one call a project makes to get a typed handle
//!   for a file under `res`, with its guid recorded so references can save.
//! - [`Engine::load_scene_file`]: a `.scene` file instantiated into the world
//!   from code, through the same [`load_scene`] pass the editor's Open and the
//!   startup scene run.
//!
//! # Design
//!
//! Assets are named by path: a project writes `"models/helmet.obj"` and gets a
//! `Handle<Mesh>`, instead of decoding the bytes itself and inventing a name for
//! the result. The path is resolved through the mounted asset directory, the
//! type through the file's extension, and the guid is written to the file's
//! `.meta` when it has none - so the handle a game holds is the same identity a
//! scene reference saves.

// Standard library
use std::path::Path;

// Current crate
use crate::asset::{AssetGuid, Handle};
use crate::asset_database::AssetDatabase;
use crate::asset_import_registry::ImportRegistry;
use crate::asset_metadata::{ImportedAsset, MetadataPolicy};
use crate::asset_store;
use crate::engine::Engine;
use crate::scene::{load_scene, Scene, SceneDocument, SceneLoad};
use crate::world::World;

impl World {
    /// Load the asset at `path` as `T` and return its handle.
    ///
    /// `path` is below the project's asset directory (`res` in a checkout, the
    /// embedded pack in a shipping build). A file that has no `.meta` gets one
    /// with a fresh guid, so the handle can be saved as a reference later; a
    /// path that is already loaded returns its existing handle and decodes
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns why the file could not be read, did not decode as `T`, or named
    /// an asset of another type.
    pub fn load_asset<T: ImportedAsset>(&mut self, path: &str) -> Result<Handle<T>, String> {
        let registry = self
            .get_resource::<ImportRegistry>()
            .cloned()
            .ok_or_else(|| format!("no asset types are registered (loading `{path}`)"))?;
        let assets = self
            .get_resource_mut::<crate::asset::AssetManager>()
            .ok_or_else(|| "the engine has no AssetManager".to_string())?;
        registry
            .load_handle_by_path::<T>(assets, Path::new(path), MetadataPolicy::CreateIfMissing)
            .map_err(|error| format!("`{path}` did not load: {error}"))
    }
}

impl Engine {
    /// [`World::load_asset`] through the engine, for code that owns the engine
    /// rather than the world.
    ///
    /// # Errors
    ///
    /// As [`World::load_asset`].
    pub fn load_asset<T: ImportedAsset>(&mut self, path: &str) -> Result<Handle<T>, String> {
        self.world_mut().load_asset::<T>(path)
    }

    /// Instantiate the scene at `path` into this engine's world.
    ///
    /// See [`load_scene_asset`], the one scene loader this and every other
    /// scene entry point share. Entities are created under the stable ids the
    /// file carries, so references between them resolve.
    ///
    /// # Errors
    ///
    /// Returns why the file could not be read or named another asset type.
    pub fn load_scene_file(&mut self, path: &str) -> Result<SceneLoad, String> {
        load_scene_asset(self, path)
    }
}

/// Reads the `.scene` at `path` and returns its document and the guid the file
/// carries.
///
/// The one import step behind every scene launcher - the engine's
/// [`Engine::load_scene_file`], the startup scene and the editor's Open - so
/// they cannot disagree about what a scene document is or which asset types it
/// needs.
///
/// # Errors
///
/// Returns why no asset types are registered, why `path` is not a scene file,
/// or why it could not be imported and kept loaded.
pub fn import_scene_document(
    engine: &mut Engine,
    path: &str,
) -> Result<(SceneDocument, Option<AssetGuid>), String> {
    let registry = engine
        .world()
        .get_resource::<ImportRegistry>()
        .cloned()
        .ok_or_else(|| format!("no asset types are registered (loading `{path}`)"))?;
    if !registry.is_standalone_extension(extension_of(path)) {
        return Err(format!("`{path}` is not a scene file"));
    }
    let assets = engine
        .world_mut()
        .get_resource_mut::<crate::asset::AssetManager>()
        .ok_or_else(|| "the engine has no AssetManager".to_string())?;
    let imported = assets
        .import_standalone::<Scene>(Path::new(path))
        .map_err(|error| format!("`{path}` did not load: {error}"))?;
    // A standalone scene carries its guid in the file's own header; a file with
    // no header yet has none.
    let guid = imported.guid;
    let document = assets
        .get(imported.handle)
        .map(|scene| scene.document().clone())
        .ok_or_else(|| format!("`{path}` did not stay loaded"))?;
    Ok((document, guid))
}

/// The asset index a scene load resolves its references through.
///
/// The world's [`AssetDatabase`] when the host built one - a shipped pack's
/// index, or a host that indexed the project - otherwise an index built from
/// the mounted asset directory, otherwise an empty one. Preferring the resource
/// is what lets a guid-only reference resolve in a pack build, where no
/// directory is mounted.
pub fn scene_database(engine: &Engine) -> AssetDatabase {
    let world = engine.world();
    if let Some(database) = world.get_resource::<AssetDatabase>() {
        return database.clone();
    }
    let Some(root) = asset_store::mounted_directory() else {
        return AssetDatabase::new();
    };
    match world.get_resource::<ImportRegistry>() {
        Some(registry) => AssetDatabase::from_directory(&root, registry),
        None => AssetDatabase::new(),
    }
}

/// Instantiates the `.scene` at `path` into the engine's world.
///
/// The one scene loader: the document comes from [`import_scene_document`] and
/// the references resolve through [`scene_database`], so every entry point -
/// the engine's own, the startup scene and the editor's Open - resolves an
/// asset reference by guid under the same rules.
///
/// # Errors
///
/// As [`import_scene_document`].
pub fn load_scene_asset(engine: &mut Engine, path: &str) -> Result<SceneLoad, String> {
    let (document, _guid) = import_scene_document(engine, path)?;
    let database = scene_database(engine);
    Ok(load_scene(engine, &database, &document))
}

/// The extension of `path`, lowercase and without the dot.
fn extension_of(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{Asset, AssetGuid, AssetLoadResult};
    use crate::asset_metadata::ImportedAsset;
    use crate::asset_standalone::render_standalone;
    use crate::asset_store::{mount_directory, mounted_directory_test_lock};
    use crate::entity::EntityId;
    use crate::scene::save_scene;
    use trait_type_map::impl_trait_accessible;

    /// The test asset: a prop's note.
    #[derive(Debug)]
    struct Prop(String);
    impl Asset for Prop {}
    impl_trait_accessible!(dyn Asset; Prop);

    impl ImportedAsset for Prop {
        type ImportSettings = ();
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["prop"];

        fn metadata_type_name() -> &'static str {
            "pill_engine_core::asset_loading::tests::Prop"
        }

        fn import(_name: &str, bytes: &[u8], _settings: &()) -> AssetLoadResult<Self> {
            Ok(Self(String::from_utf8_lossy(bytes).into_owned()))
        }
    }

    /// A scratch asset directory, mounted while the test holds the lock.
    struct Scratch {
        root: std::path::PathBuf,
        _mounted: std::sync::MutexGuard<'static, ()>,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let mounted = mounted_directory_test_lock();
            let root = std::env::temp_dir()
                .join(format!("pill-load-asset-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            mount_directory(&root);
            Self {
                root,
                _mounted: mounted,
            }
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn write_meta(&self, relative: &str, guid: AssetGuid) {
            self.write(
                &format!("{relative}.meta"),
                &format!(
                    r#"{{"format_version": 1, "asset_type": "pill_engine_core::asset_loading::tests::Prop", "guid": "{guid}", "settings": null}}"#
                ),
            );
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A project loads an asset by path, the guid it was given is the one the
    /// file's sidecar carries, and loading it again returns the same handle.
    #[test]
    fn a_project_loads_an_asset_by_path_and_keeps_its_guid() {
        let scratch = Scratch::new("by-path");
        let guid = AssetGuid::random().expect("a fresh guid");
        scratch.write("props/a.prop", "a note");
        scratch.write_meta("props/a.prop", guid);

        let mut world = World::new();
        world.register_imported_asset::<Prop>();
        let first = world
            .load_asset::<Prop>("props/a.prop")
            .expect("the prop loads");
        let assets = world.get_resource::<crate::asset::AssetManager>().unwrap();
        assert_eq!(assets.guid_of(first), Some(guid), "the sidecar's guid");
        assert_eq!(
            assets.get(first).map(|prop| prop.0.as_str()),
            Some("a note")
        );

        let again = world
            .load_asset::<Prop>("props/a.prop")
            .expect("the second load reuses the asset");
        assert_eq!(again, first, "one path is one handle");
    }

    /// A scene load resolves its references through the world's asset database
    /// when the host built one, even with a directory mounted.
    ///
    /// A pack build mounts nothing, so its index exists only as the world
    /// resource; preferring the mounted directory would leave every guid in a
    /// shipped startup scene unresolved.
    #[test]
    fn the_scene_database_prefers_the_worlds_index() {
        // The mounted directory maps the guid to one path...
        let scratch = Scratch::new("database-preference");
        let guid = AssetGuid::random().expect("a fresh guid");
        scratch.write("props/a.prop", "a note");
        scratch.write_meta("props/a.prop", guid);

        let mut engine = Engine::new();
        engine.world_mut().register_imported_asset::<Prop>();
        let registry = engine
            .world()
            .get_resource::<ImportRegistry>()
            .expect("an engine owns an import registry")
            .clone();

        // ...and the index the host would insert maps it to another, so which
        // one a load used is visible in the record it finds.
        let other_root =
            std::env::temp_dir().join(format!("pill-load-asset-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&other_root);
        std::fs::create_dir_all(other_root.join("props")).unwrap();
        std::fs::write(other_root.join("props/other.prop"), "a note").unwrap();
        std::fs::write(
            other_root.join("props/other.prop.meta"),
            format!(
                r#"{{"format_version": 1, "asset_type": "pill_engine_core::asset_loading::tests::Prop", "guid": "{guid}", "settings": null}}"#
            ),
        )
        .unwrap();
        let index = AssetDatabase::from_directory(&other_root, &registry);

        engine.world_mut().insert_resource(index);
        let database = scene_database(&engine);
        assert_eq!(
            database.by_guid(guid).map(|record| record.path.as_str()),
            Some("props/other.prop"),
            "the world's index is the one a scene load uses"
        );

        let _ = std::fs::remove_dir_all(&other_root);
    }

    /// A file with no sidecar gets one, so the handle it returns can be saved
    /// as a reference later.
    #[test]
    fn a_source_without_a_sidecar_is_given_a_guid() {
        let scratch = Scratch::new("no-meta");
        scratch.write("props/b.prop", "another note");

        let mut world = World::new();
        world.register_imported_asset::<Prop>();
        let handle = world
            .load_asset::<Prop>("props/b.prop")
            .expect("the prop loads");
        assert!(
            world
                .get_resource::<crate::asset::AssetManager>()
                .unwrap()
                .guid_of(handle)
                .is_some(),
            "the load recorded a guid"
        );
        assert!(
            scratch.root.join("props/b.prop.meta").is_file(),
            "the sidecar was written beside the source"
        );
    }

    /// A scene file loads from code, entities and all, under the ids it
    /// carries.
    #[test]
    fn a_scene_loads_from_code() {
        let scratch = Scratch::new("scene");
        let mut writer = Engine::new();
        let id = EntityId::from_name("crate");
        writer
            .world_mut()
            .create_entity()
            .with_id(id)
            .build()
            .unwrap();
        let registry = writer
            .world()
            .get_resource::<ImportRegistry>()
            .cloned()
            .unwrap();
        let (document, report) = {
            let world = writer.world();
            let assets = world.get_resource::<crate::asset::AssetManager>().unwrap();
            save_scene(world, assets, &registry, None)
        };
        assert!(
            report.skipped_fields.is_empty(),
            "{:?}",
            report.skipped_fields
        );
        let bytes =
            render_standalone::<Scene>(AssetGuid::random().expect("a fresh guid"), &document)
                .expect("the scene renders");
        std::fs::create_dir_all(scratch.root.join("scenes")).unwrap();
        std::fs::write(scratch.root.join("scenes/a.scene"), bytes).unwrap();

        let mut engine = Engine::new();
        let loaded = engine
            .load_scene_file("scenes/a.scene")
            .expect("the scene loads");
        assert_eq!(loaded.entities, 1);
        assert!(
            engine.world().entity_by_id(id).is_some(),
            "the entity kept the id the document gave it"
        );
    }

    /// The engine owns its scene type: a fresh engine lists it as a standalone
    /// type and loads a `.scene` file with nothing else registered.
    #[test]
    fn the_engine_registers_its_own_scene_type() {
        let scratch = Scratch::new("engine-scene");
        let bytes = render_standalone::<Scene>(
            AssetGuid::random().expect("a fresh guid"),
            &crate::scene::SceneDocument::default(),
        )
        .expect("the scene renders");
        std::fs::create_dir_all(scratch.root.join("scenes")).unwrap();
        std::fs::write(scratch.root.join("scenes/empty.scene"), bytes).unwrap();

        let mut engine = Engine::new();
        let registry = engine
            .world()
            .get_resource::<ImportRegistry>()
            .expect("the engine registers its import registry")
            .clone();
        assert!(
            registry
                .standalone_types()
                .iter()
                .any(|(type_name, extension)| {
                    type_name
                        == <Scene as crate::asset_standalone::StandaloneAsset>::metadata_type_name()
                        && extension == "scene"
                }),
            "a fresh engine lists the scene type: {:?}",
            registry.standalone_types()
        );
        let loaded = engine
            .load_scene_file("scenes/empty.scene")
            .expect("the scene loads without a caller registering it");
        assert_eq!(loaded.entities, 0);
    }
}

//! A game's world saves to a `.scene` file and loads back into an empty one.
//!
//! # Responsibilities
//!
//! - Prove the whole scene path through the API a project uses: the derive's
//!   field layouts, [`save_scene`], the `.scene` file, and [`load_scene`],
//!   with an entity reference and a `Handle<Scene>` reference surviving the
//!   trip.
//!
//! # Design
//!
//! The components come from `#[derive(PillComponent)]` and are registered
//! through the artifact-wide loop, exactly as a module's are, so the layout the
//! scene reads and writes is the generated one rather than a hand-written copy.
//! The referenced asset is itself a scene file, which is what makes a scene
//! nameable by other data.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_engine::asset_store::mount_directory;
use pill_engine::component_registry::register_all_components;
use pill_engine::{
    load_scene, render_standalone, save_scene, AssetDatabase, AssetGuid, AssetManager, Engine,
    Entity, EntityId, Handle, ImportRegistry, PillComponent, Scene, SceneDocument, StandaloneAsset,
};

/// A rider: the entity it rides, the prop scene it carries, and a scalar, which
/// is one field of each kind a scene has to carry.
#[repr(C)]
#[derive(Debug, Clone, Copy, PillComponent)]
pub struct Rider {
    /// The entity this rider sits on.
    mount: Entity,
    /// The prop scene drawn with the rider.
    prop: Handle<Scene>,
    /// How much the rider weighs, in the physics.
    weight: f32,
}

/// A mounted project world: both engines of the test register the same
/// components and asset types, as two runs of one project would.
fn engine_with_project() -> Engine {
    let mut engine = Engine::new();
    register_all_components(engine.world_mut()).expect("registration succeeds");
    engine.world_mut().register_standalone_asset::<Scene>();
    engine
}

/// The project's asset directory, removed when the test ends.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!("pill-scene-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the scratch directory is created");
        mount_directory(&root);
        Self { root }
    }

    fn write(&self, relative: &str, bytes: &[u8]) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a file below a directory"))
            .expect("the directory is created");
        std::fs::write(path, bytes).expect("the file is written");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A scene's entities, components and references all come back from the file,
/// including the scene reference and the entity reference it carried.
#[test]
fn a_project_scene_saves_and_loads_back_with_its_references() {
    let scratch = Scratch::new("round-trip");
    let mount_id = EntityId::from_name("mount");
    let hero_id = EntityId::from_name("hero");

    // The prop scene the rider references: an empty document is a valid scene.
    let prop_guid = AssetGuid::random().expect("a fresh guid");
    let prop = render_standalone::<Scene>(prop_guid, &SceneDocument::default())
        .expect("an empty scene renders");
    scratch.write("props/scenery.scene", &prop);

    // Step 1: the world that saves.
    let scene_guid = {
        let mut engine = engine_with_project();
        let prop = {
            let assets = engine
                .world_mut()
                .get_resource_mut::<AssetManager>()
                .expect("an engine owns an asset manager");
            assets
                .import_standalone::<Scene>(Path::new("props/scenery.scene"))
                .expect("the prop scene loads")
        };
        let mount = engine
            .world_mut()
            .create_entity()
            .with_id(mount_id)
            .build()
            .expect("the mount builds");
        engine
            .world_mut()
            .create_entity()
            .with_id(hero_id)
            .with(Rider {
                mount,
                prop: prop.handle,
                weight: 0.75,
            })
            .build()
            .expect("the hero builds");

        let registry = engine
            .world()
            .get_resource::<ImportRegistry>()
            .expect("an engine owns an import registry")
            .clone();
        let world = engine.world();
        let assets = world
            .get_resource::<AssetManager>()
            .expect("an engine owns an asset manager");
        let (document, report) = save_scene(world, assets, &registry, None);
        assert!(
            report.skipped_fields.is_empty(),
            "every field of the rider is representable: {:?}",
            report.skipped_fields
        );
        assert_eq!(report.components, 1, "only the rider's component");
        assert_eq!(document.entities.len(), 2);

        let scene_guid = AssetGuid::random().expect("a fresh guid");
        let bytes = render_standalone::<Scene>(scene_guid, &document).expect("the scene renders");
        scratch.write("scenes/main.scene", &bytes);
        scene_guid
    };

    // Step 2: an empty engine of the same project reads the file back.
    let mut engine = engine_with_project();
    let database = {
        let registry = engine
            .world()
            .get_resource::<ImportRegistry>()
            .expect("an engine owns an import registry");
        AssetDatabase::from_directory(&scratch.root, registry)
    };
    let scene = {
        let assets = engine
            .world_mut()
            .get_resource_mut::<AssetManager>()
            .expect("an engine owns an asset manager");
        let imported = assets
            .import_standalone::<Scene>(Path::new("scenes/main.scene"))
            .expect("the scene file loads");
        assert_eq!(
            imported.guid,
            Some(scene_guid),
            "the file keeps the guid it was saved with"
        );
        assets
            .get(imported.handle)
            .expect("the scene asset is live")
            .clone()
    };
    let report = load_scene(&mut engine, &database, scene.document());
    assert_eq!(report.entities, 2);
    assert_eq!(report.components, 1);
    assert!(
        report.unknown_components.is_empty(),
        "{:?}",
        report.unknown_components
    );
    assert!(
        report.skipped_fields.is_empty(),
        "{:?}",
        report.skipped_fields
    );
    assert_eq!(report.unresolved_references, 0);

    // Step 3: the values the file carried, read back through the components.
    let world = engine.world_mut();
    let hero = world.entity_by_id(hero_id).expect("the hero came back");
    let rider = *world
        .get_component::<Rider>(hero)
        .expect("the rider component came back");
    assert_eq!(rider.weight, 0.75, "the scalar came back");
    assert_eq!(
        world.entity_id(rider.mount),
        Some(mount_id),
        "the entity reference found the entity the document created"
    );
    assert_ne!(rider.mount, Entity::NULL);
    let assets = world
        .get_resource::<AssetManager>()
        .expect("an engine owns an asset manager");
    assert_eq!(
        assets.guid_of(rider.prop),
        Some(prop_guid),
        "the scene reference resolved to the prop scene file"
    );
}

/// The `.scene` extension is registered with the import registry, so a scan of
/// the project's asset directory finds scene files without a type list.
#[test]
fn the_scene_extension_is_registered() {
    let engine = engine_with_project();
    let world = engine.world();
    let registry = world
        .get_resource::<ImportRegistry>()
        .expect("an engine owns an import registry");
    assert_eq!(
        registry.type_for_extension("scene"),
        Some(Scene::metadata_type_name()),
        "a .scene file maps to the scene type"
    );
    assert!(registry.is_standalone_extension("scene"));
}

//! The startup scene loads between component registration and the project's
//! `init`.
//!
//! # Responsibilities
//!
//! - Prove the order: a scene queued before the artifact initializes is in the
//!   world by the time the project's own `init` body runs, so a project that
//!   seeds "up to a count" finds the scene's entities instead of adding its
//!   own over them.
//!
//! # Design
//!
//! The entry point is [`run_project_init`], the function `#[pill_project]`
//! generates its wrapper around - the same call the development host makes
//! through `pill_module_init` and the shipping runtime makes directly.

// Standard library
use std::cell::Cell;
use std::path::Path;

// External crates
use pill_engine::asset_store::mount_directory;
use pill_engine::component_registry::{register_all_components, run_project_init};
use pill_engine::{
    queue_startup_scene, render_standalone, save_scene, AssetManager, Engine, EntityId,
    ImportRegistry, PillComponent, Scene,
};

/// The component the scene carries.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillComponent)]
pub struct Beacon {
    /// How far the beacon reaches.
    power: f32,
}

// Entities and beacons counted when the project's `init` body ran.
//
// A thread-local rather than a static because the two tests in this file both
// call `run_project_init` with the same `fn` pointer - the only shape that
// signature accepts - so a shared static would let either test read the
// other's value. Each test reads what its own thread recorded.
thread_local! {
    static AT_INIT: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

/// A project init that records the world it was handed.
fn recording_init(engine: &mut Engine) -> u32 {
    let world = engine.world_mut();
    let entities = world.entity_rows().len();
    let beacons = pill_engine::Query::<&Beacon>::new(world).iter_mut().count();
    AT_INIT.with(|slot| slot.set(Some((entities, beacons))));
    0
}

/// What the current thread's `init` recorded, or a panic when it never ran.
fn recorded_at_init() -> (usize, usize) {
    AT_INIT
        .with(|slot| slot.get())
        .expect("init ran on this thread")
}

/// A scene file with one beacon entity, and the project starts with it.
#[test]
fn the_startup_scene_is_in_the_world_before_init_runs() {
    let root = std::env::temp_dir().join(format!("pill-startup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    mount_directory(&root);

    // Step 1: a world writes one beacon into a scene file.
    let beacon_id = EntityId::from_name("beacon");
    let bytes = {
        let mut engine = Engine::new();
        register_all_components(engine.world_mut()).expect("registration succeeds");
        engine.world_mut().register_standalone_asset::<Scene>();
        engine
            .world_mut()
            .create_entity()
            .with_id(beacon_id)
            .with(Beacon { power: 3.5 })
            .build()
            .expect("the beacon builds");
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
            "{:?}",
            report.skipped_fields
        );
        render_standalone::<Scene>(
            pill_engine::AssetGuid::random().expect("a fresh guid"),
            &document,
        )
        .expect("the scene renders")
    };
    let scene_path = root.join("scenes").join("start.scene");
    std::fs::create_dir_all(scene_path.parent().unwrap()).unwrap();
    std::fs::write(&scene_path, &bytes).unwrap();

    // Step 2: a fresh engine of the same project starts with it queued.
    let mut engine = Engine::new();
    engine.world_mut().register_standalone_asset::<Scene>();
    queue_startup_scene(engine.world_mut(), "scenes/start.scene");
    let status = run_project_init(&mut engine, recording_init);
    assert_eq!(status, 0, "the project initializes");

    // Step 3: the init body saw the scene's entity, and only it.
    let (entities, beacons) = recorded_at_init();
    assert_eq!(
        beacons, 1,
        "the scene's beacon existed before the project's init"
    );
    assert_eq!(entities, 1, "the init body saw exactly the scene's entity");
    let world = engine.world_mut();
    let beacon = world
        .entity_by_id(beacon_id)
        .expect("the beacon kept the id the document gave it");
    assert_eq!(
        world.get_component::<Beacon>(beacon).map(|b| b.power),
        Some(3.5)
    );

    let _ = std::fs::remove_dir_all(Path::new(&root));
}
/// A project that queues nothing initializes against its own world.
#[test]
fn a_project_without_a_startup_scene_is_untouched() {
    let mut engine = Engine::new();
    let status = run_project_init(&mut engine, recording_init);
    assert_eq!(status, 0);
    let (entities, beacons) = recorded_at_init();
    assert_eq!(entities, 0, "an empty project seeds no entity");
    assert_eq!(beacons, 0, "an empty project owns no beacon");
}

//! Integration-style unit tests for the native/C# ECS boundary.

use ecs_hybrid::{ComponentTicks, Engine, SystemAccess, SystemScheduler};

use super::abi::{ComponentChunk, NativeComponentBlob, NativeSystemAccess};
use super::backend::derive_system_access;
use super::commands::{
    ffi_queue_add_component, ffi_queue_create, ffi_queue_destroy, ffi_queue_remove_component,
    ffi_reserve_entity,
};
use super::components::{
    register_component_manifest, shared_component_bindings, stable_component_id, Color,
    ComponentBinding, ComponentBindings, Position, Sprite, StableComponentId,
};
use super::context::ActiveSystemGuard;
use super::queries::{ffi_get_component_chunk, ffi_get_entity_chunk};

/// Return the stable ID used by a component in the shared `TracyLive` namespace.
fn test_stable_id(name: &str) -> StableComponentId {
    stable_component_id(&format!("TracyLive.{name}"))
}

/// Build one ABI access descriptor for a named test component.
fn native_access(name: &str, mode: u8) -> NativeSystemAccess {
    let id = test_stable_id(name).0;
    NativeSystemAccess {
        component_key: id as u64,
        component_key_high: (id >> 64) as u64,
        mode,
    }
}

/// Request one native component chunk using the managed component identity.
fn get_test_chunk(name: &str, mode: u8, index: u32, output: *mut ComponentChunk) -> u8 {
    let id = test_stable_id(name).0;
    ffi_get_component_chunk(id as u64, (id >> 64) as u64, mode, index, output)
}

/// Convert concise test access declarations into scheduler metadata.
fn managed_access(entries: &[(&str, u8)]) -> SystemAccess {
    let native: Vec<_> = entries
        .iter()
        .map(|(name, mode)| native_access(name, *mode))
        .collect();
    let mut engine = Engine::new();
    let mut bindings = shared_component_bindings(&mut engine);
    for (name, _) in entries {
        let stable_id = test_stable_id(name);
        if bindings.contains_key(&stable_id) {
            continue;
        }
        let component_id = engine
            .world_mut()
            .register_dynamic_component(stable_id.0, format!("TracyLive.{name}"), 4, 4, 1)
            .unwrap();
        bindings.insert(
            stable_id,
            ComponentBinding::Dynamic {
                component_id,
                size: 4,
                align: 4,
            },
        );
    }
    derive_system_access(&native, &bindings)
        .expect("managed access should map to native components")
}

/// Populate a representative world containing shared and dynamic components.
fn setup_test_world(engine: &mut Engine) -> ComponentBindings {
    let shared = shared_component_bindings(engine);
    let stable_id = stable_component_id("TracyLive.PhysicsState");
    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.PhysicsState",
        "size": 28,
        "alignment": 4,
        "schema_hash": 1,
        "shared": false,
        "fields": []
    }]);
    let bindings =
        register_component_manifest(engine, &serde_json::to_vec(&manifest).unwrap(), shared)
            .unwrap();
    let physics = bindings[&stable_id].component_id();
    for _ in 0..100 {
        let entity = engine
            .world_mut()
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .with(Sprite {
                width: 0.0,
                height: 0.0,
                color: Color {
                    r: 1.0,
                    g: 0.3,
                    b: 0.3,
                    a: 1.0,
                },
            })
            .build()
            .unwrap();
        engine
            .world_mut()
            .add_dynamic_component_default(entity, physics)
            .unwrap();
    }
    bindings
}

/// Build an execution graph for the supplied managed system accesses.
fn scheduler_for(accesses: impl IntoIterator<Item = SystemAccess>) -> SystemScheduler {
    let mut scheduler = SystemScheduler::new();
    for access in accesses {
        scheduler.register_system(access);
    }
    scheduler.build_execution_graph();
    scheduler
}

/// Verify managed commands can create, migrate, and destroy a mixed entity.
#[test]
fn managed_command_abi_runs_mixed_lifecycle_through_the_native_queue() {
    let mut engine = Engine::new();
    let mut bindings = shared_component_bindings(&mut engine);
    let dynamic_a_key = stable_component_id("TracyLive.DynamicA");
    let dynamic_b_key = stable_component_id("TracyLive.DynamicB");
    let dynamic_a = engine
        .world_mut()
        .register_dynamic_component(dynamic_a_key.0, "TracyLive.DynamicA", 4, 4, 1)
        .unwrap();
    let dynamic_b = engine
        .world_mut()
        .register_dynamic_component(dynamic_b_key.0, "TracyLive.DynamicB", 4, 4, 2)
        .unwrap();
    bindings.insert(
        dynamic_a_key,
        ComponentBinding::Dynamic {
            component_id: dynamic_a,
            size: 4,
            align: 4,
        },
    );
    bindings.insert(
        dynamic_b_key,
        ComponentBinding::Dynamic {
            component_id: dynamic_b,
            size: 4,
            align: 4,
        },
    );
    let position_key = stable_component_id("TracyLive.Position");
    let position = Position { x: 9.0, y: 12.0 };
    let dynamic_a_value = 41_u32;
    let mut created = None;

    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, true);
            let mut entity = std::mem::MaybeUninit::uninit();
            assert_eq!(ffi_reserve_entity(entity.as_mut_ptr()), 1);
            // SAFETY: successful reserve initialized the output.
            let entity = unsafe { entity.assume_init() };
            let blobs = [
                NativeComponentBlob {
                    component_key: position_key.0 as u64,
                    component_key_high: (position_key.0 >> 64) as u64,
                    data: std::ptr::from_ref(&position).cast(),
                    size: std::mem::size_of::<Position>() as u32,
                },
                NativeComponentBlob {
                    component_key: dynamic_a_key.0 as u64,
                    component_key_high: (dynamic_a_key.0 >> 64) as u64,
                    data: std::ptr::from_ref(&dynamic_a_value).cast(),
                    size: 4,
                },
            ];
            assert_eq!(
                ffi_queue_create(&entity, blobs.as_ptr(), blobs.len() as u32),
                1
            );
            created = Some(entity);
        })
        .unwrap();

    let entity = created.unwrap();
    assert_eq!(engine.world().entity_count(), 1);
    assert_eq!(
        engine.world().get_component::<Position>(entity).unwrap().x,
        9.0
    );
    assert_eq!(
        engine
            .world()
            .dynamic_component_bytes(entity, dynamic_a)
            .unwrap(),
        41_u32.to_ne_bytes()
    );

    let dynamic_b_value = 77_u32;
    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, true);
            assert_eq!(
                ffi_queue_add_component(
                    &entity,
                    dynamic_b_key.0 as u64,
                    (dynamic_b_key.0 >> 64) as u64,
                    std::ptr::from_ref(&dynamic_b_value).cast(),
                    4,
                ),
                1
            );
            assert_eq!(
                ffi_queue_remove_component(
                    &entity,
                    dynamic_a_key.0 as u64,
                    (dynamic_a_key.0 >> 64) as u64,
                ),
                1
            );
        })
        .unwrap();
    assert!(engine
        .world()
        .dynamic_component_bytes(entity, dynamic_a)
        .is_none());
    assert_eq!(
        engine
            .world()
            .dynamic_component_bytes(entity, dynamic_b)
            .unwrap(),
        77_u32.to_ne_bytes()
    );
    assert!(engine.world().get_component::<Position>(entity).is_some());

    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, true);
            assert_eq!(ffi_queue_destroy(&entity), 1);
        })
        .unwrap();
    assert_eq!(engine.world().entity_count(), 0);
}

/// Verify command callbacks reject stale entities and undeclared Commands use.
#[test]
fn managed_command_abi_rejects_stale_generations_and_undeclared_commands() {
    let mut engine = Engine::new();
    let bindings = shared_component_bindings(&mut engine);
    let stale = engine
        .world_mut()
        .create_entity()
        .with(Position { x: 0.0, y: 0.0 })
        .build()
        .unwrap();
    assert!(engine.world_mut().destroy_entity(stale));
    let _replacement = engine.world_mut().reserve_entity();
    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, true);
            assert_eq!(ffi_queue_destroy(&stale), 5);
        })
        .unwrap();
    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, false);
            assert_eq!(ffi_queue_destroy(&stale), 4);
        })
        .unwrap();
}

/// Verify a managed Commands parameter makes its scheduler access exclusive.
#[test]
fn reflected_managed_commands_access_is_scheduler_exclusive() {
    let mut commands_access = managed_access(&[("Position", 0)]);
    commands_access.set_uses_commands(true);
    let disjoint_reader = managed_access(&[("Sprite", 0)]);
    let scheduler = scheduler_for([commands_access, disjoint_reader]);
    assert_different_batches(&scheduler, 0, 1);
}

/// Assert that all requested system indices occur together in one batch.
fn assert_same_batch(scheduler: &SystemScheduler, systems: &[usize]) {
    assert!(scheduler
        .execution_graph()
        .iter()
        .any(|batch| systems.iter().all(|system| batch.contains(system))));
}

/// Assert that two conflicting systems never occur in the same batch.
fn assert_different_batches(scheduler: &SystemScheduler, first: usize, second: usize) {
    assert!(!scheduler
        .execution_graph()
        .iter()
        .any(|batch| batch.contains(&first) && batch.contains(&second)));
}

/// Construct a zeroed chunk descriptor suitable as an FFI output slot.
fn empty_chunk() -> ComponentChunk {
    ComponentChunk {
        archetype_low: 0,
        archetype_high: 0,
        data: std::ptr::null_mut(),
        len: 0,
        element_size: 0,
        ticks: std::ptr::null_mut(),
        change_tick: 0,
    }
}

/// Reproduce the managed write-marker operation for one row's change tick.
///
/// # Safety
///
/// `chunk` must contain the live tick pointer returned by a native query
/// callback, and `row` must address that same chunk invocation.
unsafe fn simulate_managed_write(chunk: &ComponentChunk, row: usize) {
    assert!(row < chunk.len as usize);
    assert!(!chunk.ticks.is_null());
    // SAFETY: the chunk callback returns a tick slice parallel to the
    // component data and `row` was checked against that shared length.
    unsafe {
        (*chunk.ticks.add(row)).set_changed(ecs_hybrid::Tick::new(chunk.change_tick));
    }
}

/// Pin the component-tick fields to the layout consumed by managed code.
#[test]
fn component_chunk_change_tracking_abi_layout_is_stable() {
    assert_eq!(std::mem::size_of::<ComponentTicks>(), 8);
    assert_eq!(std::mem::offset_of!(ComponentTicks, changed), 4);
    assert_eq!(std::mem::size_of::<ComponentChunk>(), 48);
    assert_eq!(std::mem::offset_of!(ComponentChunk, ticks), 32);
    assert_eq!(std::mem::offset_of!(ComponentChunk, change_tick), 40);
}

/// Verify a C#-only manifest component can be registered and queried natively.
#[test]
fn managed_manifest_registers_and_queries_a_new_dynamic_component() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("Game.CustomOnlyInCSharp");
    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "Game.CustomOnlyInCSharp",
        "size": 4,
        "alignment": 4,
        "schema_hash": 12345,
        "shared": false,
        "fields": [{
            "name": "Value",
            "offset": 0,
            "size": 4,
            "primitive_type": "System.UInt32",
            "fields": []
        }]
    }]);
    let bindings =
        register_component_manifest(&mut engine, &serde_json::to_vec(&manifest).unwrap(), shared)
            .unwrap();
    let component_id = bindings[&stable_id].component_id();
    engine
        .world_mut()
        .create_dynamic_entity(&[(component_id, 77_u32.to_ne_bytes().to_vec())])
        .unwrap();

    let accesses = [NativeSystemAccess {
        component_key: stable_id.0 as u64,
        component_key_high: (stable_id.0 >> 64) as u64,
        mode: 1,
    }];
    let mut chunk = empty_chunk();
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
        assert_eq!(
            ffi_get_component_chunk(
                stable_id.0 as u64,
                (stable_id.0 >> 64) as u64,
                1,
                0,
                &mut chunk,
            ),
            1
        );
        assert_eq!(chunk.len, 1);
        assert_eq!(chunk.element_size, 4);
        assert_eq!(unsafe { *(chunk.data as *const u32) }, 77);
    }
}

/// Verify a shared managed mirror with a different field schema is rejected.
#[test]
fn managed_shared_component_schema_mismatch_is_rejected() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("TracyLive.Position");
    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.Position",
        "size": 8,
        "alignment": 4,
        "schema_hash": 0,
        "shared": true,
        "fields": [
            { "name": "Y", "offset": 0, "size": 4, "primitive_type": "System.Single", "fields": [] },
            { "name": "X", "offset": 4, "size": 4, "primitive_type": "System.Single", "fields": [] }
        ]
    }]);

    let error =
        register_component_manifest(&mut engine, &serde_json::to_vec(&manifest).unwrap(), shared)
            .err()
            .expect("an equal-sized but incompatible shared schema must fail");
    assert!(error.to_string().contains("field schema"));
}

/// Verify managed shared bindings use the renderer's concrete component types.
#[cfg(feature = "rendering")]
#[test]
fn csharp_world_supports_the_sprite_renderer_query() {
    let mut engine = Engine::new();
    setup_test_world(&mut engine);

    let mut query = ecs_hybrid::Query::<(&Position, &Sprite)>::new(engine.world_mut());
    assert_eq!(query.iter_mut().count(), 100);
}

/// Verify writers of different components can share one parallel batch.
#[test]
fn disjoint_managed_writers_share_a_parallel_batch() {
    let scheduler = scheduler_for([
        managed_access(&[("PhysicsState", 1)]),
        managed_access(&[("Position", 1)]),
        managed_access(&[("Sprite", 1)]),
    ]);

    assert_eq!(scheduler.execution_graph().len(), 1);
    assert_same_batch(&scheduler, &[0, 1, 2]);
}

/// Verify multiple readers of one component can execute concurrently.
#[test]
fn managed_readers_of_the_same_component_share_a_parallel_batch() {
    let scheduler = scheduler_for([
        managed_access(&[("Position", 0)]),
        managed_access(&[("Position", 0)]),
    ]);

    assert_eq!(scheduler.execution_graph().len(), 1);
    assert_same_batch(&scheduler, &[0, 1]);
}

/// Verify a reader and writer of one component are placed in separate batches.
#[test]
fn managed_reader_and_writer_are_scheduled_in_different_batches() {
    let scheduler = scheduler_for([
        managed_access(&[("Position", 0)]),
        managed_access(&[("Position", 1)]),
    ]);

    assert_eq!(scheduler.execution_graph().len(), 2);
    assert_different_batches(&scheduler, 0, 1);
}

/// Verify multiple writers of one component are placed in separate batches.
#[test]
fn managed_writers_of_the_same_component_are_scheduled_in_different_batches() {
    let scheduler = scheduler_for([
        managed_access(&[("Sprite", 1)]),
        managed_access(&[("Sprite", 1)]),
    ]);

    assert_eq!(scheduler.execution_graph().len(), 2);
    assert_different_batches(&scheduler, 0, 1);
}

/// Verify EntityTerm contributes no component conflict to scheduler access.
#[test]
fn entity_only_managed_system_does_not_create_a_scheduler_conflict() {
    // EntityTerm is intentionally omitted from the native component
    // access list exported by IQueryDescriptor.
    let scheduler = scheduler_for([
        managed_access(&[]),
        managed_access(&[("PhysicsState", 1), ("Position", 1), ("Sprite", 1)]),
    ]);

    assert_eq!(scheduler.execution_graph().len(), 1);
    assert_same_batch(&scheduler, &[0, 1]);
}

/// Verify optional query terms retain their underlying scheduler conflicts.
#[test]
fn optional_managed_access_conflicts_when_the_component_may_be_present() {
    // OptionalWrite<Sprite> exports the same scheduler write as Write<Sprite>;
    // optionality affects matching, never parallel safety.
    let scheduler = scheduler_for([
        managed_access(&[("PhysicsState", 1), ("Sprite", 0)]),
        managed_access(&[("Position", 1)]),
        managed_access(&[("Sprite", 1)]),
    ]);

    assert_eq!(scheduler.execution_graph().len(), 2);
    assert_same_batch(&scheduler, &[0, 1]);
    assert_different_batches(&scheduler, 0, 2);
    assert!(!scheduler
        .get_access(1)
        .unwrap()
        .conflicts_with(scheduler.get_access(2).unwrap()));
}

/// Verify marking one managed write is observed by Rust's Changed filter.
#[test]
fn one_managed_row_write_is_visible_to_rust_changed_filter() {
    let mut engine = Engine::new();
    let bindings = setup_test_world(&mut engine);
    let baseline = engine.world().change_tick();
    engine.world_mut().set_system_last_run(baseline);
    engine.world_mut().increment_change_tick();

    let accesses = [native_access("Position", 1)];
    let mut component_chunk = empty_chunk();
    let mut entity_chunk = empty_chunk();
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
        assert_eq!(get_test_chunk("Position", 1, 0, &mut component_chunk), 1);
        assert_eq!(ffi_get_entity_chunk(0, &mut entity_chunk), 1);
        unsafe { simulate_managed_write(&component_chunk, 37) };
    }

    let expected = unsafe { *((entity_chunk.data as *const ecs_hybrid::Entity).add(37)) };
    let mut changed =
        ecs_hybrid::Query::<(ecs_hybrid::Entity,), ecs_hybrid::Changed<Position>>::new(
            engine.world_mut(),
        );
    let hits: Vec<_> = changed.iter_mut().map(|(entity,)| entity).collect();
    assert_eq!(hits, vec![expected]);
}

/// Verify read-only managed iteration leaves component change ticks untouched.
#[test]
fn managed_read_only_chunk_does_not_trigger_changed_filter() {
    let mut engine = Engine::new();
    let bindings = setup_test_world(&mut engine);
    let baseline = engine.world().change_tick();
    engine.world_mut().set_system_last_run(baseline);
    engine.world_mut().increment_change_tick();

    let accesses = [native_access("Position", 0)];
    let mut chunk = empty_chunk();
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
        assert_eq!(get_test_chunk("Position", 0, 0, &mut chunk), 1);
        assert!(!chunk.ticks.is_null());
    }

    let mut changed =
        ecs_hybrid::Query::<(ecs_hybrid::Entity,), ecs_hybrid::Changed<Position>>::new(
            engine.world_mut(),
        );
    assert_eq!(changed.iter_mut().count(), 0);
}

/// Verify disjoint writable columns update only their corresponding row ticks.
#[test]
fn disjoint_managed_writes_mark_the_correct_tick_columns() {
    let mut engine = Engine::new();
    let bindings = setup_test_world(&mut engine);
    let baseline = engine.world().change_tick();
    engine.world_mut().set_system_last_run(baseline);
    engine.world_mut().increment_change_tick();

    let accesses = [native_access("Position", 1), native_access("Sprite", 1)];
    let mut positions = empty_chunk();
    let mut sprites = empty_chunk();
    let mut entities = empty_chunk();
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
        assert_eq!(get_test_chunk("Position", 1, 0, &mut positions), 1);
        assert_eq!(get_test_chunk("Sprite", 1, 0, &mut sprites), 1);
        assert_eq!(ffi_get_entity_chunk(0, &mut entities), 1);
        assert_ne!(positions.ticks, sprites.ticks);
        unsafe {
            simulate_managed_write(&positions, 3);
            simulate_managed_write(&sprites, 7);
        }
    }

    let entity_at = |row| unsafe { *((entities.data as *const ecs_hybrid::Entity).add(row)) };
    let mut changed_positions = ecs_hybrid::Query::<
        (ecs_hybrid::Entity,),
        ecs_hybrid::Changed<Position>,
    >::new(engine.world_mut());
    let position_hits: Vec<_> = changed_positions
        .iter_mut()
        .map(|(entity,)| entity)
        .collect();
    assert_eq!(position_hits, vec![entity_at(3)]);

    let mut changed_sprites =
        ecs_hybrid::Query::<(ecs_hybrid::Entity,), ecs_hybrid::Changed<Sprite>>::new(
            engine.world_mut(),
        );
    let sprite_hits: Vec<_> = changed_sprites.iter_mut().map(|(entity,)| entity).collect();
    assert_eq!(sprite_hits, vec![entity_at(7)]);
}

/// Verify entity columns are exposed only inside a scheduled managed scope.
#[test]
fn entity_chunks_are_available_only_during_a_managed_system() {
    let mut engine = Engine::new();
    let bindings = setup_test_world(&mut engine);
    let mut chunk = empty_chunk();

    assert_eq!(ffi_get_entity_chunk(0, &mut chunk), 3);
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &[], &bindings);
        assert_eq!(ffi_get_entity_chunk(0, &mut chunk), 1);
        assert_eq!(chunk.len, 100);
        assert_eq!(
            chunk.element_size as usize,
            std::mem::size_of::<ecs_hybrid::Entity>()
        );
        assert!(!chunk.data.is_null());
    }
    assert_eq!(ffi_get_entity_chunk(0, &mut chunk), 3);
}

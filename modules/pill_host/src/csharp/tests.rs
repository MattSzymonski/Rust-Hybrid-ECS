//! Integration-style unit tests for the native/C# ECS boundary.
//!
//! # Responsibilities
//!
//! - Verify command-callback ABI behaviour against a live engine.
//! - Verify component-chunk queries and scheduler access derivation.
//!
//! # Design
//!
//! Every test drives the [`Engine`] through the same ABI entry points the
//! managed runtime uses, always inside an [`ActiveSystemGuard`] scope so the
//! declared scheduler access is enforced exactly as in a live frame. Shared
//! fixtures build a representative world of shared and descriptor components,
//! while scheduler tests derive [`SystemAccess`] metadata from concise native
//! access declarations.

// External crates
use pill_core::error::EngineMessage;
use pill_engine::{
    ComponentId, ComponentTicks, Engine, Entity, SystemAccess, SystemError, SystemScheduler,
};

// Current crate
use super::abi::{ComponentChunk, NativeComponentBlob, NativeSystemAccess};
use super::backend::{
    checked_access_count, checked_system_count, derive_system_access, is_supported_manifest_length,
    MAX_ACCESSES_PER_SYSTEM, MAX_COMPONENT_MANIFEST_BYTES, MAX_SYSTEMS_PER_ASSEMBLY,
};
#[cfg(feature = "hot_reload")]
use super::backend::{
    poll_status_is_known, POLL_MANIFEST_PENDING, POLL_NO_CHANGE, POLL_REJECTED, POLL_RELOADED,
};
use super::commands::{
    ffi_queue_add_component, ffi_queue_create, ffi_queue_destroy, ffi_queue_remove_component,
    ffi_reserve_entity,
};
use super::components::{
    apply_component_manifest_on_reload, module_native_bindings, register_component_manifest,
    shared_component_bindings, stable_component_id, BindingStore, Color, ComponentBinding,
    ComponentBindings, ModuleExposedComponent, Position, Sprite, StableComponentId,
};
// `Color`, `Position` and `Sprite` above are the renderer's components,
// re-exported by `components` from `pill_master_renderer`.
use super::context::ActiveSystemGuard;
use super::manifest::parse_and_validate_manifest;
use super::queries::{
    ffi_entity_count, ffi_get_archetype_chunk, ffi_get_component_chunk, ffi_get_entity_chunk,
};
use super::resources::resource_target;

// =============================================================================
// Constants
// =============================================================================

// ABI status codes returned by the native callbacks under test. Every
// callback reports `1` on success; the chunk queries report `3` when no
// managed system is active, while the command callbacks additionally report
// `4` when the declared command scope cannot be entered and `5` when the
// entity generation no longer matches the live handle.
const ABI_SUCCESS: u8 = 1;
const ABI_OUT_OF_SCOPE: u8 = 3;
const ABI_COMMAND_SCOPE_DENIED: u8 = 4;
const ABI_STALE_ENTITY_GENERATION: u8 = 5;

/// Undeclared access on the query path: a component mode the system never
/// declared, or an archetype no validated term served before the entity path
/// asked for it (the same code, because both are "not declared").
const ABI_UNDECLARED_ACCESS: u8 = 4;

/// Number of entities populated by [`setup_test_world`]; row-count assertions
/// must agree with this bound.
const TEST_WORLD_ENTITY_COUNT: usize = 100;

// =============================================================================
// Test Helpers
// =============================================================================

/// Return the stable ID used by a component in the shared `TracyLive` namespace.
fn test_stable_id(name: &str) -> StableComponentId {
    stable_component_id(&format!("TracyLive.{name}"))
}

/// The witness these tests hand to `register_component_descriptor`.
///
/// The shapes registered here are four-byte integers named literally, which is
/// the same evidence the production path earns by running
/// `BLITTABLE_FIELD_TYPES` over a manifest.
fn test_witness() -> pill_engine::archetype::Blittability {
    pill_engine::archetype::Blittability::from_manifest_fields()
}

/// Build one ABI access descriptor for a named test component.
fn native_access(name: &str, mode: u8) -> NativeSystemAccess {
    let id = test_stable_id(name).0;
    NativeSystemAccess {
        component_key: id as u64,
        component_key_high: (id >> 64) as u64,
        mode,
        kind: 0,
    }
}

/// Request one native component chunk using the managed component identity.
fn get_test_chunk(name: &str, mode: u8, index: u32, output: *mut ComponentChunk) -> u8 {
    let id = test_stable_id(name).0;
    ffi_get_component_chunk(id as u64, (id >> 64) as u64, mode, index, output)
}

/// Convert concise test access declarations into scheduler metadata.
fn managed_access(entries: &[(&str, u8)]) -> SystemAccess {
    // Step 1: Build one native access descriptor per test declaration.
    let native: Vec<_> = entries
        .iter()
        .map(|(name, mode)| native_access(name, *mode))
        .collect();
    // Step 2: Register a managed binding for every component not yet shared.
    let mut engine = Engine::new();
    let mut bindings = shared_component_bindings(&mut engine);
    for (name, _) in entries {
        let stable_id = test_stable_id(name);
        if bindings.contains_key(&stable_id) {
            continue;
        }
        let component_id = engine
            .world_mut()
            .register_component_descriptor(
                stable_id.0,
                format!("TracyLive.{name}"),
                4,
                4,
                1,
                test_witness(),
            )
            .unwrap();
        bindings.insert(
            stable_id,
            ComponentBinding::Managed {
                component_id,
                size: 4,
                align: 4,
                schema_hash: 1,
            },
        );
    }
    // Step 3: Derive scheduler access from the bound component identities.
    derive_system_access(&native, &bindings)
        .expect("managed access should map to native components")
}

/// Populate a representative world containing shared and descriptor components.
fn setup_test_world(engine: &mut Engine) -> ComponentBindings {
    // Step 1: Register the shared `PhysicsState` component from a manifest.
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
    // Step 2: Populate entities with both shared and descriptor components.
    for _ in 0..TEST_WORLD_ENTITY_COUNT {
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
            .add_descriptor_component_default(entity, physics)
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

// =============================================================================
// Tests
// =============================================================================

/// Verify managed commands can create, migrate, and destroy a mixed entity.
#[test]
fn managed_command_abi_runs_mixed_lifecycle_through_the_native_queue() {
    let mut engine = Engine::new();
    let mut bindings = shared_component_bindings(&mut engine);
    let descriptor_a_key = stable_component_id("TracyLive.DescriptorA");
    let descriptor_b_key = stable_component_id("TracyLive.DescriptorB");
    let descriptor_a = engine
        .world_mut()
        .register_component_descriptor(
            descriptor_a_key.0,
            "TracyLive.DescriptorA",
            4,
            4,
            1,
            test_witness(),
        )
        .unwrap();
    let descriptor_b = engine
        .world_mut()
        .register_component_descriptor(
            descriptor_b_key.0,
            "TracyLive.DescriptorB",
            4,
            4,
            2,
            test_witness(),
        )
        .unwrap();
    bindings.insert(
        descriptor_a_key,
        ComponentBinding::Managed {
            component_id: descriptor_a,
            size: 4,
            align: 4,
            schema_hash: 1,
        },
    );
    bindings.insert(
        descriptor_b_key,
        ComponentBinding::Managed {
            component_id: descriptor_b,
            size: 4,
            align: 4,
            schema_hash: 2,
        },
    );
    let position_key = stable_component_id("TracyLive.Position");
    let position = Position { x: 9.0, y: 12.0 };
    let descriptor_a_value = 41_u32;
    let mut created = None;

    // Step 1: Create a mixed entity holding Position and DescriptorA through the
    // native command queue.
    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, true);
            let mut entity = std::mem::MaybeUninit::uninit();
            assert_eq!(ffi_reserve_entity(entity.as_mut_ptr()), ABI_SUCCESS);
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
                    component_key: descriptor_a_key.0 as u64,
                    component_key_high: (descriptor_a_key.0 >> 64) as u64,
                    data: std::ptr::from_ref(&descriptor_a_value).cast(),
                    size: 4,
                },
            ];
            assert_eq!(
                ffi_queue_create(&entity, blobs.as_ptr(), blobs.len() as u32),
                ABI_SUCCESS
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
            .descriptor_component_bytes(entity, descriptor_a)
            .unwrap(),
        41_u32.to_ne_bytes()
    );

    let descriptor_b_value = 77_u32;
    // Step 2: Swap DescriptorA for DescriptorB through the native command queue.
    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, true);
            assert_eq!(
                ffi_queue_add_component(
                    &entity,
                    descriptor_b_key.0 as u64,
                    (descriptor_b_key.0 >> 64) as u64,
                    std::ptr::from_ref(&descriptor_b_value).cast(),
                    4,
                ),
                ABI_SUCCESS
            );
            assert_eq!(
                ffi_queue_remove_component(
                    &entity,
                    descriptor_a_key.0 as u64,
                    (descriptor_a_key.0 >> 64) as u64,
                ),
                ABI_SUCCESS
            );
        })
        .unwrap();
    assert!(engine
        .world()
        .descriptor_component_bytes(entity, descriptor_a)
        .is_none());
    assert_eq!(
        engine
            .world()
            .descriptor_component_bytes(entity, descriptor_b)
            .unwrap(),
        77_u32.to_ne_bytes()
    );
    assert!(engine.world().get_component::<Position>(entity).is_some());

    // Step 3: Destroy the entity through the native command queue.
    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, true);
            assert_eq!(ffi_queue_destroy(&entity), ABI_SUCCESS);
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
    // Reserve the freed slot so it is reissued with a fresh generation; the
    // old handle must then be rejected as stale by every queue callback.
    let _replacement = engine.world_mut().reserve_entity();
    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, true);
            assert_eq!(ffi_queue_destroy(&stale), ABI_STALE_ENTITY_GENERATION);
        })
        .unwrap();
    engine
        .run_deferred_commands(|world, queue| {
            let _guard = ActiveSystemGuard::set_with_commands(world, queue, &[], &bindings, false);
            assert_eq!(ffi_queue_destroy(&stale), ABI_COMMAND_SCOPE_DENIED);
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
        entities: std::ptr::null(),
        len: 0,
        element_size: 0,
        ticks: std::ptr::null_mut(),
        change_tick: 0,
        // Zero is the "no scope" token, which is what an unfilled output slot
        // should carry: a chunk the host never wrote is not valid anywhere.
        scope_token: 0,
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
        (*chunk.ticks.add(row)).set_changed(pill_engine::Tick::new(chunk.change_tick));
    }
}

/// Pin the component-tick fields to the layout consumed by managed code in
/// `EngineApi.cs`, field for field and offset for offset.
#[test]
fn component_chunk_change_tracking_abi_layout_is_stable() {
    assert_eq!(std::mem::size_of::<ComponentTicks>(), 8);
    assert_eq!(std::mem::offset_of!(ComponentTicks, changed), 4);
    assert_eq!(std::mem::size_of::<ComponentChunk>(), 56);
    assert_eq!(std::mem::offset_of!(ComponentChunk, data), 16);
    assert_eq!(std::mem::offset_of!(ComponentChunk, entities), 24);
    assert_eq!(std::mem::offset_of!(ComponentChunk, ticks), 40);
    assert_eq!(std::mem::offset_of!(ComponentChunk, change_tick), 48);
}

/// Verify the archetype-scoped chunk lookup resolves the remaining terms of
/// an archetype without scanning chunk indices.
#[test]
fn archetype_chunk_lookup_resolves_components_and_entities() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    engine
        .world_mut()
        .create_entity()
        .with(Position { x: 1.0, y: 2.0 })
        .build()
        .unwrap();

    let position_id = test_stable_id("Position");
    let sprite_id = test_stable_id("Sprite");
    let accesses = [native_access("Position", 1), native_access("Sprite", 0)];
    let mut chunk = empty_chunk();
    let mut sprite_chunk = empty_chunk();
    let mut entity_chunk = empty_chunk();
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &shared);

        // Step 1: one index-based lookup yields the archetype identity the
        // managed enumerator would carry in its driver chunk.
        assert_eq!(get_test_chunk("Position", 1, 0, &mut chunk), ABI_SUCCESS);

        // Step 2: the archetype-scoped twin resolves the same column directly.
        assert_eq!(
            ffi_get_archetype_chunk(
                chunk.archetype_low,
                chunk.archetype_high,
                position_id.0 as u64,
                (position_id.0 >> 64) as u64,
                1,
                &mut chunk,
            ),
            ABI_SUCCESS
        );
        assert_eq!(chunk.len, 1);
        assert_eq!(chunk.element_size, std::mem::size_of::<Position>() as u32);
        // SAFETY: the lookup succeeded and the asserted geometry guarantees
        // the data pointer addresses one valid `Position` row.
        assert_eq!(unsafe { *(chunk.data as *const Position) }.x, 1.0);

        // Step 3: an authorized component the archetype does not carry
        // reports absence (`0`), which the enumerator turns into
        // "optional term not present".
        assert_eq!(
            ffi_get_archetype_chunk(
                chunk.archetype_low,
                chunk.archetype_high,
                sprite_id.0 as u64,
                (sprite_id.0 >> 64) as u64,
                0,
                &mut sprite_chunk,
            ),
            0
        );

        // Step 4: mode `2` returns the archetype's entity column without
        // consulting any component binding or access declaration.
        assert_eq!(
            ffi_get_archetype_chunk(
                chunk.archetype_low,
                chunk.archetype_high,
                0,
                0,
                2,
                &mut entity_chunk,
            ),
            ABI_SUCCESS
        );
        assert_eq!(entity_chunk.len, 1);
        assert_eq!(
            entity_chunk.element_size,
            std::mem::size_of::<Entity>() as u32
        );
    }
}

/// Verify a C#-only manifest component can be registered and queried natively.
#[test]
fn managed_manifest_registers_and_queries_a_new_descriptor_component() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("Project.CustomOnlyInCSharp");
    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "Project.CustomOnlyInCSharp",
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
        .create_descriptor_entity(&[(component_id, 77_u32.to_ne_bytes().to_vec())])
        .unwrap();

    let accesses = [NativeSystemAccess {
        component_key: stable_id.0 as u64,
        component_key_high: (stable_id.0 >> 64) as u64,
        mode: 1,
        kind: 0,
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
            ABI_SUCCESS
        );
        assert_eq!(chunk.len, 1);
        assert_eq!(chunk.element_size, 4);
        // SAFETY: the query returned success and the asserted length and
        // element size guarantee the chunk data pointer addresses one valid
        // `u32` inside the component column.
        assert_eq!(unsafe { *(chunk.data as *const u32) }, 77);
    }
}

/// A newly registered descriptor component still exposes its field layout.
///
/// The layout computation leaks every field name and struct tag, so it moved
/// from the top of the manifest loop - where it ran for entries that already
/// had a binding and for manifests refused as shared - into the registration
/// branch. The editor-visible outcome must not have changed with it.
#[test]
fn descriptor_registration_still_installs_field_layout() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("TracyLive.LayoutProbe");
    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.LayoutProbe",
        "size": 4,
        "alignment": 4,
        "schema_hash": 9,
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

    let layout = engine
        .world()
        .component_field_layout(component_id)
        .expect("a registered descriptor component keeps its field layout");
    assert_eq!(layout.len(), 1, "one manifest field means one descriptor");
    assert_eq!(layout[0].name, "Value");
    assert_eq!(layout[0].type_tag, "u32");
}

/// A caller that passes nowhere to write gets a status, not silent truncation.
///
/// `0` means "end of iteration" to every managed caller, so answering a null
/// output buffer with it turned a binding bug into a query that found no rows.
/// Status `5` names the mistake, and the managed `ValidateStatus` maps it to
/// an `ArgumentException`.
#[test]
fn null_output_pointer_reports_invalid_argument() {
    assert_eq!(
        ffi_get_component_chunk(0, 0, 0, 0, std::ptr::null_mut()),
        5,
        "a component chunk with no output buffer is a caller bug"
    );
    assert_eq!(
        ffi_get_archetype_chunk(0, 0, 0, 0, 0, std::ptr::null_mut()),
        5,
        "an archetype chunk with no output buffer is a caller bug"
    );
    assert_eq!(
        ffi_get_entity_chunk(0, std::ptr::null_mut()),
        5,
        "an entity chunk with no output buffer is a caller bug"
    );
    assert_eq!(
        ffi_entity_count(std::ptr::null_mut()),
        5,
        "an entity count with no output buffer is a caller bug"
    );
}

/// The served stride is the live column's, not the binding's copy.
///
/// Managed row arithmetic trusts `element_size`; if a binding ever described a
/// different layout than the column its pointer addresses, every row after the
/// first would be read at the wrong offset. The chunk callbacks therefore
/// serve the registered layout, and a binding that disagrees cannot change
/// what managed code multiplies by.
#[test]
fn descriptor_chunk_stride_comes_from_the_live_column() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("TracyLive.DriftProbe");
    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.DriftProbe",
        "size": 4,
        "alignment": 4,
        "schema_hash": 7,
        "shared": false,
        "fields": [{
            "name": "Value",
            "offset": 0,
            "size": 4,
            "primitive_type": "System.UInt32",
            "fields": []
        }]
    }]);
    let mut bindings =
        register_component_manifest(&mut engine, &serde_json::to_vec(&manifest).unwrap(), shared)
            .unwrap();
    let component_id = bindings[&stable_id].component_id();
    engine
        .world_mut()
        .create_descriptor_entity(&[(component_id, 77_u32.to_ne_bytes().to_vec())])
        .unwrap();

    // A store that disagrees with the registered column: the registration
    // path asserts the two agree, and the serving path must be immune to the
    // disagreement either way.
    bindings.insert(
        stable_id,
        ComponentBinding::Managed {
            component_id,
            size: 8,
            align: 8,
            schema_hash: 7,
        },
    );

    let accesses = [NativeSystemAccess {
        component_key: stable_id.0 as u64,
        component_key_high: (stable_id.0 >> 64) as u64,
        mode: 1,
        kind: 0,
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
            ABI_SUCCESS
        );
        assert_eq!(chunk.len, 1);
        assert_eq!(
            chunk.element_size, 4,
            "the stride must be the live column's, not the binding's copy"
        );
        // SAFETY: the query returned success and the asserted length and
        // element size guarantee the chunk data pointer addresses one valid
        // `u32` inside the component column.
        assert_eq!(unsafe { *(chunk.data as *const u32) }, 77);
    }
}

/// A module binding whose forwarded layout disagrees with the live column is
/// dropped at build time, and a stale binding that still reaches a query fails
/// closed instead of asserting.
#[test]
fn module_native_binding_rejects_live_layout_mismatch() {
    let mut engine = Engine::new();
    let stable_id = test_stable_id("ModuleThing");
    let module_id = engine
        .world_mut()
        .register_component_descriptor(
            stable_id.0,
            "TracyLive.ModuleThing",
            8,
            4,
            1,
            test_witness(),
        )
        .expect("the module component registers");
    engine
        .world_mut()
        .create_descriptor_entity(&[(module_id, vec![0_u8; 8])])
        .expect("the entity carries the registered layout");

    // The module forwards a size the live column does not have - a mirror
    // built against a stale generation - and its binding is dropped rather
    // than handed to managed code.
    let exposed = [ModuleExposedComponent {
        csharp_name: "TracyLive.ModuleThing".to_string(),
        component_id: module_id,
        size: 16,
        align: 4,
        fields: Vec::new(),
    }];
    let agreed = module_native_bindings(&mut engine, &exposed);
    assert!(
        agreed.is_empty(),
        "a binding that disagrees with the live column is not handed out"
    );

    // A binding that predates the column change and is already in a store
    // fails the query arm with the "unknown component" status instead of the
    // debug assertion that aborted debug hosts and vanished in release.
    //
    // `Sprite` stands in for the module's component here: it is real native
    // storage, which is exactly what the `ModuleNative` arm serves, and its
    // live layout is what the stale binding is compared against.
    let mut bindings = shared_component_bindings(&mut engine);
    let sprite_stable_id = test_stable_id("Sprite");
    bindings.insert(
        sprite_stable_id,
        ComponentBinding::ModuleNative {
            component_id: ComponentId::of::<Sprite>(),
            // Deliberately not the live layout, so the arm has to refuse.
            size: 64,
            align: 4,
        },
    );
    engine
        .world_mut()
        .create_entity()
        .with(Sprite {
            width: 1.0,
            height: 1.0,
            color: Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        })
        .build()
        .unwrap();
    let accesses = [native_access("Sprite", 1)];
    let mut chunk = empty_chunk();
    let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
    assert_eq!(
        ffi_get_component_chunk(
            sprite_stable_id.0 as u64,
            (sprite_stable_id.0 >> 64) as u64,
            1,
            0,
            &mut chunk,
        ),
        2,
        "a wrong-size binding reports failure instead of aborting"
    );
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
///
/// Runs in every build: the components are unconditional now, so the managed
/// bridge has one binding path rather than one per feature set.
#[test]
fn csharp_world_supports_the_sprite_renderer_query() {
    let mut engine = Engine::new();
    setup_test_world(&mut engine);

    let mut query = pill_engine::Query::<(&Position, &Sprite)>::new(engine.world_mut());
    assert_eq!(query.iter_mut().count(), TEST_WORLD_ENTITY_COUNT);
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
        assert_eq!(
            get_test_chunk("Position", 1, 0, &mut component_chunk),
            ABI_SUCCESS
        );
        assert_eq!(ffi_get_entity_chunk(0, &mut entity_chunk), ABI_SUCCESS);
        // SAFETY: the Position chunk was fetched successfully and row `37`
        // lies within its live length, so the chunk tick pointer is valid to
        // advance by that many rows.
        unsafe { simulate_managed_write(&component_chunk, 37) };
    }

    // SAFETY: the entity chunk was fetched successfully and row `37` indexes
    // a live entity within its declared length.
    let expected = unsafe { *((entity_chunk.entities as *const pill_engine::Entity).add(37)) };
    let mut changed =
        pill_engine::Query::<(pill_engine::Entity,), pill_engine::Changed<Position>>::new(
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
        assert_eq!(get_test_chunk("Position", 0, 0, &mut chunk), ABI_SUCCESS);
        assert!(!chunk.ticks.is_null());
    }

    let mut changed =
        pill_engine::Query::<(pill_engine::Entity,), pill_engine::Changed<Position>>::new(
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
        assert_eq!(
            get_test_chunk("Position", 1, 0, &mut positions),
            ABI_SUCCESS
        );
        assert_eq!(get_test_chunk("Sprite", 1, 0, &mut sprites), ABI_SUCCESS);
        assert_eq!(ffi_get_entity_chunk(0, &mut entities), ABI_SUCCESS);
        assert_ne!(positions.ticks, sprites.ticks);
        // SAFETY: both chunks were fetched successfully and rows `3` and `7`
        // fall within their live lengths, so each tick pointer is valid to
        // advance by the requested row.
        unsafe {
            simulate_managed_write(&positions, 3);
            simulate_managed_write(&sprites, 7);
        }
    }

    // SAFETY: the entity chunk was fetched successfully and each queried row
    // indexes a live entity within the chunk's declared length.
    let entity_at = |row| unsafe { *((entities.entities as *const pill_engine::Entity).add(row)) };
    let mut changed_positions = pill_engine::Query::<
        (pill_engine::Entity,),
        pill_engine::Changed<Position>,
    >::new(engine.world_mut());
    let position_hits: Vec<_> = changed_positions
        .iter_mut()
        .map(|(entity,)| entity)
        .collect();
    assert_eq!(position_hits, vec![entity_at(3)]);

    let mut changed_sprites = pill_engine::Query::<
        (pill_engine::Entity,),
        pill_engine::Changed<Sprite>,
    >::new(engine.world_mut());
    let sprite_hits: Vec<_> = changed_sprites.iter_mut().map(|(entity,)| entity).collect();
    assert_eq!(sprite_hits, vec![entity_at(7)]);
}

/// Verify entity columns are exposed only inside a scheduled managed scope.
#[test]
fn entity_chunks_are_available_only_during_a_managed_system() {
    let mut engine = Engine::new();
    let bindings = setup_test_world(&mut engine);
    let mut chunk = empty_chunk();

    assert_eq!(ffi_get_entity_chunk(0, &mut chunk), ABI_OUT_OF_SCOPE);
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &[], &bindings);
        assert_eq!(ffi_get_entity_chunk(0, &mut chunk), ABI_SUCCESS);
        assert_eq!(chunk.len as usize, TEST_WORLD_ENTITY_COUNT);
        assert_eq!(
            chunk.element_size as usize,
            std::mem::size_of::<pill_engine::Entity>()
        );
        assert!(
            chunk.data.is_null(),
            "entity rows must not be reachable through the writable `data` slot"
        );
        assert!(
            !chunk.entities.is_null(),
            "entity rows arrive in `entities`"
        );
    }
    assert_eq!(ffi_get_entity_chunk(0, &mut chunk), ABI_OUT_OF_SCOPE);
}

/// Entity columns are handed out through the const `entities` slot only.
///
/// The writable `data` slot stays null for them, so an aliasing write into
/// engine-owned entity rows cannot be written by accident the way it could
/// when both kinds of column shared one pointer field.
#[test]
fn entity_chunks_expose_a_const_pointer() {
    let mut engine = Engine::new();
    let bindings = setup_test_world(&mut engine);
    let mut chunk = empty_chunk();
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &[], &bindings);
        assert_eq!(ffi_get_entity_chunk(0, &mut chunk), ABI_SUCCESS);
        assert!(
            chunk.data.is_null(),
            "the writable slot must stay null for entity rows"
        );
        assert!(
            !chunk.entities.is_null(),
            "entity rows are served through the const slot"
        );
    }
}

/// The entity path serves only archetypes a validated term already reached.
///
/// Without that precondition, `mode == 2` would answer for any archetype id a
/// managed caller cared to guess - an oracle for component sets nothing
/// declared. The precondition lives per invocation, so it is gone again for
/// the next system, and outside a managed invocation the out-of-scope answer
/// takes precedence.
#[test]
fn mode_two_serves_only_observed_archetypes() {
    let mut engine = Engine::new();
    let bindings = setup_test_world(&mut engine);
    let accesses = [native_access("Position", 0)];
    // The id, learned the way the world itself knows it; the managed path
    // never holds one before a validated term served it.
    let archetype = engine
        .world()
        .entity_chunk(0)
        .expect("the world has an archetype")
        .0;
    let low = archetype.0 as u64;
    let high = (archetype.0 >> 64) as u64;
    let mut chunk = empty_chunk();

    assert_eq!(
        ffi_get_archetype_chunk(low, high, 0, 0, 2, &mut chunk),
        ABI_OUT_OF_SCOPE,
        "outside a managed invocation the scope answer comes first"
    );
    {
        let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);

        // Step 1: a fresh id is refused, because nothing declared it.
        assert_eq!(
            ffi_get_archetype_chunk(low, high, 0, 0, 2, &mut chunk),
            ABI_UNDECLARED_ACCESS,
            "an unobserved archetype must not be servable"
        );

        // Step 2: once a validated component term served the archetype, its
        // entity column resolves.
        assert_eq!(get_test_chunk("Position", 0, 0, &mut chunk), ABI_SUCCESS);
        assert_eq!(
            ffi_get_archetype_chunk(low, high, 0, 0, 2, &mut chunk),
            ABI_SUCCESS
        );
        assert_eq!(chunk.len as usize, TEST_WORLD_ENTITY_COUNT);
        assert!(!chunk.entities.is_null());
    }

    // Step 3: the observation does not survive the invocation.
    let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
    assert_eq!(
        ffi_get_archetype_chunk(low, high, 0, 0, 2, &mut chunk),
        ABI_UNDECLARED_ACCESS,
        "each invocation starts with no archetype observed"
    );
}

/// Verify a managed system that reports failure is recorded as a drained
/// system failure carrying the managed message.
#[test]
fn failing_managed_system_is_recorded_as_a_system_failure() {
    let mut engine = Engine::new();
    // SAFETY: the closure never touches the world, so the empty access
    // declaration is exact.
    unsafe {
        engine.register_system_with_access(
            "managed_failure",
            SystemAccess::new(),
            move |_world, _queue| -> Result<(), SystemError> {
                Err(SystemError::Managed {
                    message: String::from("index out of range"),
                })
            },
        );
    }
    engine.process_frame().unwrap();
    let failures = engine.drain_system_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].system, "managed_failure");
    assert!(failures[0]
        .to_plain_message()
        .contains("index out of range"));
}

/// Verify a healthy managed system leaves the frame failure record empty.
#[test]
fn succeeding_managed_system_leaves_no_system_failure() {
    let mut engine = Engine::new();
    // SAFETY: the closure never touches the world, so the empty access
    // declaration is exact.
    unsafe {
        engine.register_system_with_access(
            "managed_success",
            SystemAccess::new(),
            move |_world, _queue| Ok(()),
        );
    }
    engine.process_frame().unwrap();
    assert!(engine.drain_system_failures().is_empty());
}

/// Verifies that an excessively nested field manifest fails validation with a
/// regular error instead of exhausting the validation stack.
#[test]
fn deeply_nested_manifest_is_rejected_without_stack_overflow() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("TracyLive.DeeplyNested");

    // Build a field tree deeper than the validation depth budget without
    // nesting this test's own call stack. It must also stay below
    // serde_json's own recursion limit so the validator, not the parser,
    // produces the rejection.
    let mut field = serde_json::json!({
        "name": "leaf",
        "offset": 0,
        "size": 1,
        "primitive_type": "System.Byte",
        "fields": []
    });
    for _ in 0..40 {
        field = serde_json::json!({
            "name": "wrapper",
            "offset": 0,
            "size": 1,
            "primitive_type": "struct",
            "fields": [field]
        });
    }
    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.DeeplyNested",
        "size": 1,
        "alignment": 1,
        "schema_hash": 1,
        "shared": false,
        "fields": [field]
    }]);

    let error =
        register_component_manifest(&mut engine, &serde_json::to_vec(&manifest).unwrap(), shared)
            .err()
            .expect("an excessively nested manifest must be rejected");
    assert!(
        error.to_string().contains("nesting depth"),
        "unexpected error: {error}"
    );
}

/// A descriptor component may only contain blittable value types.
///
/// `ComponentColumn` moves rows with `ptr::copy` and frees its buffer without
/// running drop glue, and the engine shares those columns across threads on an
/// `unsafe impl Send`/`Sync`. A field owning a managed resource would be
/// duplicated on move, leaked on free, and raced on. Before this check the only
/// constraint on a field's type was that its name was non-empty.
#[test]
fn a_non_blittable_field_type_is_rejected() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("TracyLive.HasManagedField");

    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.HasManagedField",
        "size": 8,
        "alignment": 8,
        "schema_hash": 1,
        "shared": false,
        "fields": [{
            "name": "Label",
            "offset": 0,
            "size": 8,
            "primitive_type": "System.String",
            "fields": []
        }]
    }]);

    let error =
        register_component_manifest(&mut engine, &serde_json::to_vec(&manifest).unwrap(), shared)
            .err()
            .expect("a managed reference field must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("non-blittable") && message.contains("Label"),
        "the error should name the field and why it was refused: {message}"
    );
}

/// A non-blittable type nested inside a value type is rejected too, so the
/// recursive walk cannot be used to smuggle one past the check.
#[test]
fn a_nested_non_blittable_field_type_is_rejected() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("TracyLive.NestedManaged");

    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.NestedManaged",
        "size": 8,
        "alignment": 8,
        "schema_hash": 1,
        "shared": false,
        "fields": [{
            "name": "Inner",
            "offset": 0,
            "size": 8,
            "primitive_type": "struct",
            "fields": [{
                "name": "Handle",
                "offset": 0,
                "size": 8,
                "primitive_type": "System.Object",
                "fields": []
            }]
        }]
    }]);

    let error =
        register_component_manifest(&mut engine, &serde_json::to_vec(&manifest).unwrap(), shared)
            .err()
            .expect("a nested managed reference must be rejected");
    assert!(
        error.to_string().contains("Handle"),
        "the error should name the nested field: {error}"
    );
}

/// Verifies that overlapping component fields are rejected so conflicting
/// byte interpretations cannot corrupt data silently.
#[test]
fn overlapping_component_fields_are_rejected() {
    let mut engine = Engine::new();
    let shared = shared_component_bindings(&mut engine);
    let stable_id = stable_component_id("TracyLive.Overlapping");
    let manifest = serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.Overlapping",
        "size": 8,
        "alignment": 4,
        "schema_hash": 1,
        "shared": false,
        "fields": [
            { "name": "First", "offset": 0, "size": 6, "primitive_type": "struct", "fields": [] },
            { "name": "Second", "offset": 4, "size": 4, "primitive_type": "struct", "fields": [] }
        ]
    }]);
    let error =
        register_component_manifest(&mut engine, &serde_json::to_vec(&manifest).unwrap(), shared)
            .err()
            .expect("overlapping component fields must be rejected");
    assert!(
        error.to_string().contains("overlap"),
        "unexpected error: {error}"
    );
}

/// The three poll codes are the loader's whole vocabulary, and anything else
/// is a typed error naming the code instead of a silent "nothing happened".
#[cfg(feature = "hot_reload")]
#[test]
fn unknown_poll_status_is_a_typed_error() {
    assert!(poll_status_is_known(POLL_NO_CHANGE));
    assert!(poll_status_is_known(POLL_RELOADED));
    assert!(poll_status_is_known(POLL_REJECTED));
    // A parked version is a status the host answers, not one it rejects.
    // Leaving it unknown is what made a C# declaration edit wedge the reload
    // loop: the loader parked, the host reported an unknown status, and every
    // later poll returned the same thing.
    assert!(poll_status_is_known(POLL_MANIFEST_PENDING));
    assert!(!poll_status_is_known(4));
    assert!(!poll_status_is_known(u8::MAX));

    let error = pill_core::error::CSharpError::UnknownPollStatus { status: 9 };
    assert!(
        error.to_string().contains('9'),
        "the message names the code: {error}"
    );
}

/// Verifies that managed-reported manifest lengths are bounded before any
/// host allocation happens.
#[test]
fn manifest_length_bounds_reject_empty_and_oversized_values() {
    assert!(!is_supported_manifest_length(0));
    assert!(is_supported_manifest_length(1));
    assert!(is_supported_manifest_length(MAX_COMPONENT_MANIFEST_BYTES));
    assert!(!is_supported_manifest_length(
        MAX_COMPONENT_MANIFEST_BYTES + 1
    ));
    assert!(!is_supported_manifest_length(u32::MAX));
}

/// Verifies that the counts a managed assembly reports are bounded before they
/// size a host allocation, with the offending count and the limit carried in
/// the refusal.
#[test]
fn system_counts_beyond_the_caps_are_refused() {
    assert_eq!(
        checked_system_count(MAX_SYSTEMS_PER_ASSEMBLY).unwrap(),
        MAX_SYSTEMS_PER_ASSEMBLY as usize,
        "the cap itself is accepted"
    );
    assert!(matches!(
        checked_system_count(MAX_SYSTEMS_PER_ASSEMBLY + 1),
        Err(pill_core::error::CSharpError::SystemCountOutOfRange { count, limit })
            if count == MAX_SYSTEMS_PER_ASSEMBLY + 1 && limit == MAX_SYSTEMS_PER_ASSEMBLY
    ));

    assert_eq!(
        checked_access_count(MAX_ACCESSES_PER_SYSTEM).unwrap(),
        MAX_ACCESSES_PER_SYSTEM as usize,
        "the cap itself is accepted"
    );
    assert!(matches!(
        checked_access_count(MAX_ACCESSES_PER_SYSTEM + 1),
        Err(pill_core::error::CSharpError::SystemCountOutOfRange { count, limit })
            if count == MAX_ACCESSES_PER_SYSTEM + 1 && limit == MAX_ACCESSES_PER_SYSTEM
    ));
}

// =============================================================================
// Manifest apply on reload
// =============================================================================

/// Build one top-level field entry for a managed manifest.
fn manifest_field(name: &str, offset: usize, size: usize) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "offset": offset,
        "size": size,
        "primitive_type": "System.Single",
        "fields": [],
    })
}

/// Build one top-level field entry that declares a default literal.
fn manifest_field_with_default(
    name: &str,
    offset: usize,
    size: usize,
    primitive_type: &str,
    default: &str,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "offset": offset,
        "size": size,
        "primitive_type": primitive_type,
        "default": default,
        "fields": [],
    })
}

/// Build the serialized manifest for one four-byte-fielded test component.
fn manifest_bytes(
    name: &str,
    size: usize,
    alignment: usize,
    schema_hash: u64,
    fields: Vec<serde_json::Value>,
) -> Vec<u8> {
    let stable_id = stable_component_id(&format!("TracyLive.{name}"));
    serde_json::to_vec(&serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": format!("TracyLive.{name}"),
        "size": size,
        "alignment": alignment,
        "schema_hash": schema_hash,
        "shared": false,
        "fields": fields,
    }]))
    .expect("the test manifest serializes")
}

/// Build the serialized manifest for one entry that declares aliases.
fn manifest_bytes_with_aliases(
    name: &str,
    aliases: &[&str],
    size: usize,
    alignment: usize,
    schema_hash: u64,
    fields: Vec<serde_json::Value>,
) -> Vec<u8> {
    let stable_id = stable_component_id(&format!("TracyLive.{name}"));
    serde_json::to_vec(&serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": format!("TracyLive.{name}"),
        "size": size,
        "alignment": alignment,
        "schema_hash": schema_hash,
        "shared": false,
        "aliases": aliases,
        "fields": fields,
    }]))
    .expect("the test manifest serializes")
}

/// Register one descriptor test component from a manifest and hand back its
/// binding store, the way a managed project start leaves them.
fn store_with_component(
    engine: &mut Engine,
    name: &str,
    size: usize,
    alignment: usize,
    schema_hash: u64,
    fields: Vec<serde_json::Value>,
) -> (BindingStore, Vec<u8>) {
    let manifest = manifest_bytes(name, size, alignment, schema_hash, fields);
    let shared = shared_component_bindings(engine);
    let store = BindingStore::new(
        register_component_manifest(engine, &manifest, shared)
            .expect("the test manifest registers"),
    );
    (store, manifest)
}

/// A manifest that says what the bindings already hold applies nothing.
#[test]
fn an_unchanged_manifest_applies_nothing() {
    let mut engine = Engine::new();
    let (store, manifest) = store_with_component(
        &mut engine,
        "Unchanged",
        8,
        4,
        7,
        vec![manifest_field("a", 0, 4), manifest_field("b", 4, 4)],
    );

    let report = apply_component_manifest_on_reload(&mut engine, &manifest, &store)
        .expect("the same manifest applies cleanly");

    assert!(report.added.is_empty(), "nothing was added");
    assert!(report.migrated.is_empty(), "nothing was migrated");
    let component_id = store.read()[&test_stable_id("Unchanged")].component_id();
    assert_eq!(engine.world().component_layout(component_id), Some((8, 4)));
}

/// A reshaped descriptor component keeps its entities and moves their bytes.
#[test]
fn a_reshaped_descriptor_component_is_migrated_on_apply() {
    let mut engine = Engine::new();
    let (store, _before) = store_with_component(
        &mut engine,
        "Relayout",
        8,
        4,
        1,
        vec![manifest_field("a", 0, 4), manifest_field("b", 4, 4)],
    );
    let stable_id = test_stable_id("Relayout");
    let component_id = store.read()[&stable_id].component_id();
    let entity = engine
        .world_mut()
        .create_descriptor_entity(&[(component_id, [1.0_f32, 2.0].map(f32::to_ne_bytes).concat())])
        .expect("the entity carries the registered layout");

    // `b` first, then `a`, then two fields that did not exist.
    let after = manifest_bytes(
        "Relayout",
        16,
        8,
        2,
        vec![
            manifest_field("b", 0, 4),
            manifest_field("a", 4, 4),
            manifest_field("c", 8, 4),
            manifest_field("d", 12, 4),
        ],
    );
    let report = apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("a reshaped layout migrates");

    assert_eq!(report.migrated, vec!["TracyLive.Relayout".to_string()]);
    let mut expected = [2.0_f32, 1.0].map(f32::to_ne_bytes).concat();
    expected.extend_from_slice(&[0_u8; 8]);
    assert_eq!(
        engine
            .world()
            .descriptor_component_bytes(entity, component_id)
            .expect("the entity still carries the component"),
        expected.as_slice(),
        "values follow their field names and the new fields start zeroed"
    );
    assert_eq!(engine.world().component_layout(component_id), Some((16, 8)));
    // The table a system scope reads is the one that moved, not a copy of it.
    let ComponentBinding::Managed {
        size,
        align,
        schema_hash,
        ..
    } = store.read()[&stable_id]
    else {
        panic!("the binding is still managed");
    };
    assert_eq!((size, align, schema_hash), (16, 8, 2));
}

/// A rename declared through an alias migrates the predecessor's rows onto
/// the successor instead of being refused as a disappearance.
#[test]
fn a_renamed_component_carries_its_rows_through_the_alias() {
    // The apply path reads the process-wide resource table too, so this test
    // needs the same exclusivity the resource tests take or a leftover entry
    // from one of them reads as a vanished resource.
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let (store, _before) = store_with_component(
        &mut engine,
        "OldName",
        8,
        4,
        7,
        vec![manifest_field("a", 0, 4), manifest_field("b", 4, 4)],
    );
    let old_id = store.read()[&test_stable_id("OldName")].component_id();
    let entity = engine
        .world_mut()
        .create_descriptor_entity(&[(old_id, [1.0_f32, 2.0].map(f32::to_ne_bytes).concat())])
        .expect("the entity carries the registered layout");

    let after = manifest_bytes_with_aliases(
        "NewName",
        &["TracyLive.OldName"],
        8,
        4,
        7,
        vec![manifest_field("a", 0, 4), manifest_field("b", 4, 4)],
    );
    let report = apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("an aliased rename migrates");

    assert_eq!(
        report.renamed,
        vec!["TracyLive.OldName -> TracyLive.NewName".to_string()]
    );
    assert!(report.added.is_empty(), "a rename is not an addition");
    assert!(report.migrated.is_empty(), "a rename is not a relayout");
    let new_id = store.read()[&test_stable_id("NewName")].component_id();
    assert!(
        !store.read().contains_key(&test_stable_id("OldName")),
        "the predecessor key is gone from the binding table"
    );
    assert_eq!(
        engine
            .world()
            .descriptor_component_bytes(entity, new_id)
            .expect("the successor holds the row"),
        [1.0_f32, 2.0].map(f32::to_ne_bytes).concat().as_slice()
    );
    assert!(engine
        .world()
        .descriptor_component_bytes(entity, old_id)
        .is_none());
    assert_eq!(
        engine
            .world()
            .resolve_component_id_by_name_any("TracyLive.OldName")
            .unwrap(),
        None,
        "the old name is free again"
    );
    assert_eq!(
        engine
            .world()
            .resolve_component_id_by_name_any("TracyLive.NewName")
            .unwrap(),
        Some(new_id)
    );

    // Applying the same manifest again settles: the alias no longer resolves
    // to anything, so there is no predecessor left to rename.
    let again = apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("the second application settles");
    assert!(again.renamed.is_empty());
}

/// A field added with a default starts from that value, not zero.
#[test]
fn an_added_field_starts_from_its_declared_default() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let (store, _before) = store_with_component(
        &mut engine,
        "Defaults",
        4,
        4,
        1,
        vec![manifest_field("a", 0, 4)],
    );
    let stable_id = test_stable_id("Defaults");
    let component_id = store.read()[&stable_id].component_id();
    let entity = engine
        .world_mut()
        .create_descriptor_entity(&[(component_id, 1.0_f32.to_ne_bytes().to_vec())])
        .expect("the entity carries the registered layout");

    let after = manifest_bytes(
        "Defaults",
        8,
        4,
        2,
        vec![
            manifest_field("a", 0, 4),
            manifest_field_with_default("added", 4, 4, "System.Single", "7.5"),
        ],
    );
    let report = apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("the added field migrates");

    assert_eq!(report.migrated, vec!["TracyLive.Defaults".to_string()]);
    let mut expected = 1.0_f32.to_ne_bytes().to_vec();
    expected.extend_from_slice(&7.5_f32.to_ne_bytes());
    assert_eq!(
        engine
            .world()
            .descriptor_component_bytes(entity, component_id)
            .unwrap(),
        expected.as_slice(),
        "the carried value survives and the added field takes its default"
    );
}

/// A default declares what an empty field starts from; a field the row already
/// carries keeps its value even when the default for it changed.
#[test]
fn a_changed_default_never_resets_a_carried_field() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let (store, _before) = store_with_component(
        &mut engine,
        "Kept",
        4,
        4,
        1,
        vec![manifest_field("a", 0, 4)],
    );
    let stable_id = test_stable_id("Kept");
    let component_id = store.read()[&stable_id].component_id();
    let entity = engine
        .world_mut()
        .create_descriptor_entity(&[(component_id, 1.0_f32.to_ne_bytes().to_vec())])
        .expect("the entity carries the registered layout");

    // Same shape, new schema hash (which is what makes it a migration) and a
    // new default for the same field.
    let after = manifest_bytes(
        "Kept",
        4,
        4,
        2,
        vec![manifest_field_with_default(
            "a",
            0,
            4,
            "System.Single",
            "9.5",
        )],
    );
    apply_component_manifest_on_reload(&mut engine, &after, &store).expect("the reshape migrates");

    assert_eq!(
        engine
            .world()
            .descriptor_component_bytes(entity, component_id)
            .unwrap(),
        1.0_f32.to_ne_bytes().as_slice(),
        "a default never overwrites a value that was carried"
    );
}

/// A default that does not parse as its field's type, or that sits on a struct
/// field, is refused where the manifest is parsed.
#[test]
fn field_defaults_are_validated_against_their_field_type() {
    let wrong_type = manifest_bytes(
        "BadDefault",
        4,
        4,
        1,
        vec![manifest_field_with_default(
            "a",
            0,
            4,
            "System.Int32",
            "1.5",
        )],
    );
    let error = match parse_and_validate_manifest(&wrong_type) {
        Ok(_) => panic!("a default that does not parse as the field's type is refused"),
        Err(error) => error,
    };
    assert!(
        error.to_plain_message().contains("is not a valid"),
        "the refusal names the reason: {error}"
    );

    let nested_id = stable_component_id("TracyLive.NestedDefault");
    let nested_default = serde_json::to_vec(&serde_json::json!([{
        "stable_id_low": nested_id.0 as u64,
        "stable_id_high": (nested_id.0 >> 64) as u64,
        "full_name": "TracyLive.NestedDefault",
        "size": 4,
        "alignment": 4,
        "schema_hash": 1,
        "shared": false,
        "fields": [{
            "name": "inner",
            "offset": 0,
            "size": 4,
            "primitive_type": "System.Single",
            "default": "1.0",
            "fields": [manifest_field("leaf", 0, 4)],
        }],
    }]))
    .expect("the test manifest serializes");
    let error = match parse_and_validate_manifest(&nested_default) {
        Ok(_) => panic!("a default on a struct field is refused"),
        Err(error) => error,
    };
    assert!(
        error
            .to_plain_message()
            .contains("only supported on primitive fields"),
        "the refusal names the reason: {error}"
    );
}

/// The resource twin: a field added to a resource with a default starts from
/// it instead of zero.
#[test]
fn a_resource_field_added_with_a_default_starts_from_it() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let before = resource_manifest_bytes("Tuned", 4, 4, 11, vec![manifest_field("speed", 0, 4)]);
    register_component_manifest(&mut engine, &before, ComponentBindings::new())
        .expect("the resource registers");
    engine
        .world_mut()
        .insert_foreign_resource_bytes(resource_id_of("Tuned"), &1.0_f32.to_ne_bytes())
        .expect("the payload is the declared width");

    let after = resource_manifest_bytes(
        "Tuned",
        8,
        4,
        12,
        vec![
            manifest_field("speed", 0, 4),
            manifest_field_with_default("gain", 4, 4, "System.Single", "2.5"),
        ],
    );
    let store = BindingStore::new(ComponentBindings::new());
    let report = apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("the resource reshape migrates");

    assert_eq!(
        report.resources_migrated,
        vec!["TracyLive.Tuned".to_string()]
    );
    let mut expected = 1.0_f32.to_ne_bytes().to_vec();
    expected.extend_from_slice(&2.5_f32.to_ne_bytes());
    assert_eq!(
        engine
            .world()
            .foreign_resource_bytes(resource_id_of("Tuned"))
            .expect("the value is stored"),
        expected.as_slice(),
        "the carried value survives and the added field takes its default"
    );
}

/// A managed component the manifest stopped naming is retired: its rows go,
/// its registration goes, and a later declaration of the same name starts
/// clean instead of colliding with storage nothing tracks.
#[test]
fn a_vanished_component_is_retired_on_apply() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let (store, _manifest) = store_with_component(
        &mut engine,
        "Vanished",
        4,
        4,
        1,
        vec![manifest_field("a", 0, 4)],
    );
    let stable_id = test_stable_id("Vanished");
    let component_id = store.read()[&stable_id].component_id();
    let entity = engine
        .world_mut()
        .create_descriptor_entity(&[(component_id, 1.0_f32.to_ne_bytes().to_vec())])
        .expect("the entity carries the registered layout");

    let empty = serde_json::to_vec(&serde_json::json!([])).expect("an empty manifest serializes");
    let report = apply_component_manifest_on_reload(&mut engine, &empty, &store)
        .expect("a vanished component is retired, not refused");

    assert_eq!(report.retired, vec!["TracyLive.Vanished".to_string()]);
    assert!(
        !store.read().contains_key(&stable_id),
        "the binding is gone from the table"
    );
    assert!(
        engine
            .world()
            .descriptor_component_bytes(entity, component_id)
            .is_none(),
        "the row is gone"
    );
    // The entity carried only the retired component, and retiring removes it
    // like any removal, so the entity itself is gone too.
    assert_eq!(engine.world().entity_count(), 0);
    assert_eq!(
        engine
            .world()
            .resolve_component_id_by_name_any("TracyLive.Vanished")
            .unwrap(),
        None,
        "the registration is gone, so the name is free again"
    );

    // Re-declaring the name later is an ordinary addition, not a collision
    // with storage nothing tracks anymore.
    let readded = manifest_bytes("Vanished", 4, 4, 1, vec![manifest_field("a", 0, 4)]);
    let report = apply_component_manifest_on_reload(&mut engine, &readded, &store)
        .expect("the name can be declared again");
    assert_eq!(report.added, vec!["TracyLive.Vanished".to_string()]);
}

/// Alias rules are enforced where the manifest is parsed, so a colliding or
/// empty alias never reaches planning.
#[test]
fn alias_validation_refuses_empty_and_colliding_names() {
    let empty =
        manifest_bytes_with_aliases("Aliased", &["  "], 4, 4, 1, vec![manifest_field("a", 0, 4)]);
    let error = match parse_and_validate_manifest(&empty) {
        Ok(_) => panic!("an empty alias is refused"),
        Err(error) => error,
    };
    assert!(
        error.to_plain_message().contains("empty alias"),
        "the refusal names the reason: {error}"
    );

    let entry = |name: &str, aliases: &[&str]| {
        let stable_id = stable_component_id(&format!("TracyLive.{name}"));
        serde_json::json!({
            "stable_id_low": stable_id.0 as u64,
            "stable_id_high": (stable_id.0 >> 64) as u64,
            "full_name": format!("TracyLive.{name}"),
            "size": 4,
            "alignment": 4,
            "schema_hash": 1,
            "shared": false,
            "aliases": aliases,
            "fields": [manifest_field("a", 0, 4)],
        })
    };

    let live_collision = serde_json::to_vec(&serde_json::json!([
        entry("Live", &[]),
        entry("Other", &["TracyLive.Live"]),
    ]))
    .expect("the test manifest serializes");
    let error = match parse_and_validate_manifest(&live_collision) {
        Ok(_) => panic!("an alias naming a live declaration is refused"),
        Err(error) => error,
    };
    assert!(
        error.to_plain_message().contains("live declaration"),
        "the refusal names the reason: {error}"
    );

    let duplicate_alias = serde_json::to_vec(&serde_json::json!([
        entry("First", &["TracyLive.Gone"]),
        entry("Second", &["TracyLive.Gone"]),
    ]))
    .expect("the test manifest serializes");
    let error = match parse_and_validate_manifest(&duplicate_alias) {
        Ok(_) => panic!("one alias claimed twice is refused"),
        Err(error) => error,
    };
    assert!(
        error.to_plain_message().contains("declared by both"),
        "the refusal names both owners: {error}"
    );
}

/// A manifest whose application fails partway leaves none of it applied: the
/// journal is unwound, so the store, the bindings and the migrated rows are
/// byte-identical to their values before the call.
#[test]
fn a_refused_manifest_leaves_none_of_it_applied() {
    let mut engine = Engine::new();
    let (store, _manifest) = store_with_component(
        &mut engine,
        "Relayout",
        8,
        4,
        1,
        vec![manifest_field("a", 0, 4), manifest_field("b", 4, 4)],
    );
    let relayout_id = store.read()[&test_stable_id("Relayout")].component_id();
    let mut row = 1.0_f32.to_ne_bytes().to_vec();
    row.extend_from_slice(&2.0_f32.to_ne_bytes());
    let entity = engine
        .world_mut()
        .create_descriptor_entity(&[(relayout_id, row.clone())])
        .expect("the entity carries the registered layout");

    // A name already claimed by a live column: the second entry's registration
    // is refused by the engine, which no amount of validation can foresee.
    let claimed_stable_id = stable_component_id("TracyLive.Claimed");
    engine
        .world_mut()
        .register_component_descriptor(
            claimed_stable_id.0,
            "TracyLive.Claimed",
            4,
            4,
            9,
            test_witness(),
        )
        .expect("the claiming component registers");
    let claimed_id = engine
        .world()
        .resolve_component_id_by_name_any("TracyLive.Claimed")
        .expect("the name resolves to its registration")
        .expect("the name is claimed");
    engine
        .world_mut()
        .create_descriptor_entity(&[(claimed_id, 9_u32.to_ne_bytes().to_vec())])
        .expect("the claiming component has a live row");

    let store_len_before = store.read().len();

    // Entry 1 reshapes `Relayout` and migrates its row; entry 2 is a second
    // component claiming the taken name, so it fails after entry 1 applied.
    let relayout_entry = serde_json::json!({
        "stable_id_low": test_stable_id("Relayout").0 as u64,
        "stable_id_high": (test_stable_id("Relayout").0 >> 64) as u64,
        "full_name": "TracyLive.Relayout",
        "size": 16,
        "alignment": 8,
        "schema_hash": 2,
        "shared": false,
        "fields": [
            manifest_field("b", 0, 4),
            manifest_field("a", 4, 4),
            manifest_field("c", 8, 4),
            manifest_field("d", 12, 4),
        ],
    });
    // A fresh stable id under the claimed *name*: same identity would land on
    // the idempotent re-registration path instead of colliding.
    let colliding_stable_id = stable_component_id("TracyLive.ClaimedV2");
    let claimed_entry = serde_json::json!({
        "stable_id_low": colliding_stable_id.0 as u64,
        "stable_id_high": (colliding_stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.Claimed",
        "size": 4,
        "alignment": 4,
        "schema_hash": 9,
        "shared": false,
        "fields": [manifest_field("value", 0, 4)],
    });
    let refused = serde_json::to_vec(&serde_json::json!([relayout_entry, claimed_entry]))
        .expect("the refused manifest serializes");

    let error = apply_component_manifest_on_reload(&mut engine, &refused, &store)
        .expect_err("a name claimed by a live column refuses the entry");
    assert!(
        error.to_string().contains("Claimed"),
        "the refusal names the colliding component: {error}"
    );

    // Everything is as it was: same store size, the binding still describes
    // the old shape, the layout is back and the row is byte-for-byte intact.
    assert_eq!(store.read().len(), store_len_before);
    assert!(
        store.read().get(&colliding_stable_id).is_none(),
        "the refused addition left no binding behind"
    );
    let ComponentBinding::Managed {
        size,
        align,
        schema_hash,
        ..
    } = store.read()[&test_stable_id("Relayout")]
    else {
        panic!("the binding is still managed");
    };
    assert_eq!(
        (size, align, schema_hash),
        (8, 4, 1),
        "the binding rolled back to the shape the world is in now"
    );
    assert_eq!(engine.world().component_layout(relayout_id), Some((8, 4)));
    assert_eq!(
        engine
            .world()
            .descriptor_component_bytes(entity, relayout_id),
        Some(row.as_slice()),
        "the migrated rows were migrated back"
    );
}

/// The three refusals: a module mirror that changed, a vanished component, and
/// a manifest that cannot be trusted.
#[test]
fn the_apply_refuses_what_it_cannot_migrate() {
    // A `ModuleNative` binding the manifest disagrees with: the Rust side owns
    // that layout, so the mirror is simply wrong.
    let mut engine = Engine::new();
    let stable_id = test_stable_id("ModuleThing");
    let module_id = engine
        .world_mut()
        .register_component_descriptor(
            stable_id.0,
            "TracyLive.ModuleThing",
            8,
            4,
            1,
            test_witness(),
        )
        .expect("the module component registers");
    let mut bindings = shared_component_bindings(&mut engine);
    bindings.insert(
        stable_id,
        ComponentBinding::ModuleNative {
            component_id: module_id,
            size: 8,
            align: 4,
        },
    );
    let store = BindingStore::new(bindings);
    let changed = manifest_bytes("ModuleThing", 16, 8, 2, vec![manifest_field("a", 0, 4)]);
    let error = apply_component_manifest_on_reload(&mut engine, &changed, &store)
        .expect_err("a module mirror cannot change");
    assert!(
        error.to_string().contains("native component uses 8/4"),
        "the refusal names both layouts: {error}"
    );

    // A duplicated identity never reaches the world.
    let mut engine = Engine::new();
    let (store, manifest) = store_with_component(
        &mut engine,
        "Twice",
        4,
        4,
        1,
        vec![manifest_field("a", 0, 4)],
    );
    let entry: serde_json::Value = serde_json::from_slice::<serde_json::Value>(&manifest)
        .expect("the manifest parses")
        .as_array()
        .expect("one entry")
        .first()
        .expect("one entry")
        .clone();
    let duplicated = serde_json::to_vec(&serde_json::json!([entry.clone(), entry]))
        .expect("the duplicated manifest serializes");
    let error = apply_component_manifest_on_reload(&mut engine, &duplicated, &store)
        .expect_err("a duplicate is refused");
    assert!(
        error.to_string().contains("duplicate declaration"),
        "the refusal names the cause: {error}"
    );
}

// =============================================================================
// Managed resources
// =============================================================================

/// Serialises every test that touches the managed resource binding table.
///
/// The table is one process-wide map, because a resource is one value per world
/// rather than one per archetype. Cargo runs these tests on parallel threads in
/// a single process, so without this they would register into each other's
/// table and fail by interference rather than by defect.
static RESOURCE_TABLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the resource-table lock and start from an empty table.
///
/// The guard is returned so it lives for the body of the test. A poisoned lock
/// is recovered rather than propagated: it means an earlier resource test
/// panicked, which is already reported, and turning that into a cascade of
/// secondary failures hides the one that matters.
fn resource_test_scope() -> std::sync::MutexGuard<'static, ()> {
    let guard = RESOURCE_TABLE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    super::resources::reset_resource_bindings_for_test();
    guard
}

/// Build the serialized manifest for one managed resource.
///
/// Same document shape as a component's, with the `kind` tag that tells the
/// two apart - which is the whole of what a resource entry adds on the wire.
fn resource_manifest_bytes(
    name: &str,
    size: usize,
    alignment: usize,
    schema_hash: u64,
    fields: Vec<serde_json::Value>,
) -> Vec<u8> {
    let stable_id = stable_component_id(&format!("TracyLive.{name}"));
    serde_json::to_vec(&serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": format!("TracyLive.{name}"),
        "size": size,
        "alignment": alignment,
        "schema_hash": schema_hash,
        "shared": false,
        "kind": "resource",
        "fields": fields,
    }]))
    .expect("the test manifest serializes")
}

/// Build the serialized manifest for one managed resource that declares
/// aliases.
fn resource_manifest_bytes_with_aliases(
    name: &str,
    aliases: &[&str],
    size: usize,
    alignment: usize,
    schema_hash: u64,
    fields: Vec<serde_json::Value>,
) -> Vec<u8> {
    let stable_id = stable_component_id(&format!("TracyLive.{name}"));
    serde_json::to_vec(&serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": format!("TracyLive.{name}"),
        "size": size,
        "alignment": alignment,
        "schema_hash": schema_hash,
        "shared": false,
        "kind": "resource",
        "aliases": aliases,
        "fields": fields,
    }]))
    .expect("the test manifest serializes")
}

/// The engine id a registered managed resource was bound to.
///
/// Asked of the binding table rather than recomputed, so the test checks the
/// id the host actually uses instead of a second opinion about what it is.
fn resource_id_of(name: &str) -> pill_engine::ResourceId {
    resource_target(stable_component_id(&format!("TracyLive.{name}")))
        .expect("the resource is registered")
        .0
}

/// A manifest resource entry registers the resource and seeds its value.
///
/// The seed is the point: a managed declaration is all there is - nothing on
/// the C# side inserts a resource the way a Rust `init` does - so a resource
/// registered and left empty would fail the first `Res<T>` access in the frame
/// that follows.
#[test]
fn a_manifest_resource_is_registered_and_seeded() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let manifest = resource_manifest_bytes(
        "Tuning",
        8,
        4,
        11,
        vec![manifest_field("speed", 0, 4), manifest_field("gain", 4, 4)],
    );

    let bindings = register_component_manifest(&mut engine, &manifest, ComponentBindings::new())
        .expect("a resource-only manifest registers");

    assert!(
        bindings.is_empty(),
        "a resource entry registers no component column"
    );
    assert_eq!(
        engine
            .world()
            .foreign_resource_bytes(resource_id_of("Tuning")),
        Some(&[0_u8; 8][..]),
        "the resource starts as a defined all-zero value"
    );
}

/// A managed resource is snapshot-visible as soon as it is registered.
#[test]
fn a_manifest_resource_joins_the_snapshot() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let manifest = resource_manifest_bytes("Saved", 4, 4, 3, vec![manifest_field("count", 0, 4)]);
    register_component_manifest(&mut engine, &manifest, ComponentBindings::new())
        .expect("the manifest registers");
    engine
        .world_mut()
        .insert_foreign_resource_bytes(resource_id_of("Saved"), &7_u32.to_ne_bytes())
        .expect("the payload is the declared width");

    let snapshot = engine.world().snapshot_resources();

    assert!(
        snapshot.payload("TracyLive.Saved").is_some(),
        "a managed resource reaches a snapshot without any Rust type to derive from"
    );
}

/// A resource rename declared through an alias moves the stored value onto
/// the successor instead of being refused as a disappearance.
#[test]
fn a_renamed_resource_carries_its_value_through_the_alias() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let before = resource_manifest_bytes(
        "OldSettings",
        8,
        4,
        11,
        vec![manifest_field("speed", 0, 4), manifest_field("gain", 4, 4)],
    );
    register_component_manifest(&mut engine, &before, ComponentBindings::new())
        .expect("the resource registers");
    let old_id = resource_id_of("OldSettings");
    engine
        .world_mut()
        .insert_foreign_resource_bytes(old_id, &[1.0_f32, 2.0].map(f32::to_ne_bytes).concat())
        .expect("the payload is the declared width");

    let after = resource_manifest_bytes_with_aliases(
        "NewSettings",
        &["TracyLive.OldSettings"],
        8,
        4,
        11,
        vec![manifest_field("speed", 0, 4), manifest_field("gain", 4, 4)],
    );
    let store = BindingStore::new(ComponentBindings::new());
    let report = apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("an aliased resource rename migrates");

    assert_eq!(
        report.resources_renamed,
        vec!["TracyLive.OldSettings -> TracyLive.NewSettings".to_string()]
    );
    assert!(
        resource_target(test_stable_id("OldSettings")).is_none(),
        "the old binding is gone from the table"
    );
    assert_eq!(
        engine
            .world()
            .foreign_resource_bytes(resource_id_of("NewSettings")),
        Some(&[1.0_f32, 2.0].map(f32::to_ne_bytes).concat()[..]),
        "the value moved onto the successor"
    );
    assert_eq!(
        engine.world().foreign_resource_layout(old_id),
        None,
        "the predecessor declaration is dropped"
    );

    // Applying the same manifest again settles: the alias no longer names a
    // live binding, so there is no predecessor left to rename.
    let again = apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("the second application settles");
    assert!(again.resources_renamed.is_empty());
}

/// A resource entry marked shared is refused where it is declared.
#[test]
fn a_shared_resource_entry_is_refused() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let stable_id = stable_component_id("TracyLive.Confused");
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "stable_id_low": stable_id.0 as u64,
        "stable_id_high": (stable_id.0 >> 64) as u64,
        "full_name": "TracyLive.Confused",
        "size": 4,
        "alignment": 4,
        "schema_hash": 1,
        "shared": true,
        "kind": "resource",
        "fields": [manifest_field("value", 0, 4)],
    }]))
    .expect("the test manifest serializes");

    let Err(error) = register_component_manifest(&mut engine, &manifest, ComponentBindings::new())
    else {
        panic!("a shared resource has no native binding to be shared with");
    };

    assert!(
        error.to_plain_message().contains("marked shared"),
        "the refusal names what is wrong: {}",
        error.to_plain_message()
    );
}

/// A reshaped resource keeps its value and moves its bytes by field name.
#[test]
fn a_reshaped_resource_is_migrated_on_reload() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let before = resource_manifest_bytes(
        "Relaid",
        8,
        4,
        1,
        vec![manifest_field("a", 0, 4), manifest_field("b", 4, 4)],
    );
    let store = BindingStore::new(
        register_component_manifest(&mut engine, &before, ComponentBindings::new())
            .expect("the manifest registers"),
    );
    let id = resource_id_of("Relaid");
    engine
        .world_mut()
        .insert_foreign_resource_bytes(id, &[1.0_f32, 2.0].map(f32::to_ne_bytes).concat())
        .expect("the payload is the declared width");

    // `b` first, then `a`, then a field that did not exist.
    let after = resource_manifest_bytes(
        "Relaid",
        12,
        4,
        2,
        vec![
            manifest_field("b", 0, 4),
            manifest_field("a", 4, 4),
            manifest_field("c", 8, 4),
        ],
    );
    let report = apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("a reshaped resource migrates");

    assert_eq!(
        report.resources_migrated,
        vec!["TracyLive.Relaid".to_string()]
    );
    let mut expected = [2.0_f32, 1.0].map(f32::to_ne_bytes).concat();
    expected.extend_from_slice(&[0_u8; 4]);
    assert_eq!(
        engine.world().foreign_resource_bytes(id),
        Some(expected.as_slice()),
        "values follow their field names and the new field starts zeroed"
    );
}

/// A resource the arriving manifest stops declaring is retired: its value and
/// declaration go, and an unclaimed id is dropped outright.
#[test]
fn a_resource_dropped_from_the_manifest_is_retired() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let before = resource_manifest_bytes("Vanishing", 4, 4, 1, vec![manifest_field("a", 0, 4)]);
    let store = BindingStore::new(
        register_component_manifest(&mut engine, &before, ComponentBindings::new())
            .expect("the manifest registers"),
    );
    let id = resource_id_of("Vanishing");
    engine
        .world_mut()
        .insert_foreign_resource_bytes(id, &7_u32.to_ne_bytes())
        .expect("the payload is the declared width");

    let empty = serde_json::to_vec(&serde_json::json!([])).expect("an empty manifest serializes");
    let report = apply_component_manifest_on_reload(&mut engine, &empty, &store)
        .expect("a dropped resource is retired, not refused");

    assert_eq!(
        report.resources_retired,
        vec!["TracyLive.Vanishing".to_string()]
    );
    assert!(
        resource_target(stable_component_id("TracyLive.Vanishing")).is_none(),
        "the binding is gone from the table"
    );
    assert_eq!(
        engine.world().foreign_resource_layout(id),
        None,
        "the value and its declaration are gone"
    );
}

/// A shared resource another subject still claims survives the retirement of
/// this subject's declaration: `drop_resources` releases only unclaimed ids.
#[test]
fn a_claimed_resource_survives_a_dropped_declaration() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let before = resource_manifest_bytes("Shared", 4, 4, 1, vec![manifest_field("a", 0, 4)]);
    let store = BindingStore::new(
        register_component_manifest(&mut engine, &before, ComponentBindings::new())
            .expect("the manifest registers"),
    );
    let id = resource_id_of("Shared");
    engine
        .world_mut()
        .insert_foreign_resource_bytes(id, &7_u32.to_ne_bytes())
        .expect("the payload is the declared width");
    // A module declaring the same name holds a claim; the drop must skip it.
    engine.world_mut().retain_resource_claims(&[id]);

    let empty = serde_json::to_vec(&serde_json::json!([])).expect("an empty manifest serializes");
    let report = apply_component_manifest_on_reload(&mut engine, &empty, &store)
        .expect("the declaration is retired");

    assert_eq!(report.resources_retired.len(), 1);
    assert_eq!(
        engine.world().foreign_resource_bytes(id),
        Some(&7_u32.to_ne_bytes()[..]),
        "the claimed value survives this subject's retirement"
    );
}

/// A reflected resource access reaches the scheduler as a resource.
///
/// Component and resource keys are both 128-bit name hashes, so only the kind
/// byte tells them apart: recorded as a component, two systems writing one
/// resource would be free to run in the same batch.
#[test]
fn a_resource_access_is_derived_as_a_resource() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let manifest = resource_manifest_bytes("Scheduled", 4, 4, 1, vec![manifest_field("a", 0, 4)]);
    register_component_manifest(&mut engine, &manifest, ComponentBindings::new())
        .expect("the manifest registers");
    let stable_id = stable_component_id("TracyLive.Scheduled");

    let writer = derive_system_access(
        &[NativeSystemAccess {
            component_key: stable_id.0 as u64,
            component_key_high: (stable_id.0 >> 64) as u64,
            mode: 1,
            kind: 1,
        }],
        &ComponentBindings::new(),
    )
    .expect("the resource is registered");
    let reader = derive_system_access(
        &[NativeSystemAccess {
            component_key: stable_id.0 as u64,
            component_key_high: (stable_id.0 >> 64) as u64,
            mode: 0,
            kind: 1,
        }],
        &ComponentBindings::new(),
    )
    .expect("the resource is registered");

    assert!(
        writer.conflicts_with(&reader),
        "a writer and a reader of one resource cannot share a batch"
    );
    assert!(
        !reader.conflicts_with(&reader),
        "two readers of one resource can"
    );
}

/// A resource access for a key no manifest registered is refused by name.
#[test]
fn an_unregistered_resource_access_is_refused() {
    let _table = resource_test_scope();
    let stable_id = stable_component_id("TracyLive.NeverDeclared");

    let error = derive_system_access(
        &[NativeSystemAccess {
            component_key: stable_id.0 as u64,
            component_key_high: (stable_id.0 >> 64) as u64,
            mode: 0,
            kind: 1,
        }],
        &ComponentBindings::new(),
    )
    .expect_err("nothing registered this resource");

    assert!(
        error
            .to_plain_message()
            .contains("unregistered resource key"),
        "the refusal says resource rather than component: {}",
        error.to_plain_message()
    );
}

/// A managed resource's field layout reaches the engine, and survives a reshape.
///
/// The host is the only holder of a foreign resource's field names, so this is
/// what makes one inspectable at all - and a reload that moves the fields has
/// to republish them, because the engine drops the layout with the shape it
/// described.
#[test]
fn a_managed_resource_publishes_its_field_layout() {
    let _table = resource_test_scope();
    let mut engine = Engine::new();
    let before = resource_manifest_bytes(
        "Inspectable",
        8,
        4,
        1,
        vec![manifest_field("a", 0, 4), manifest_field("b", 4, 4)],
    );
    let store = BindingStore::new(
        register_component_manifest(&mut engine, &before, ComponentBindings::new())
            .expect("the manifest registers"),
    );
    let id = resource_id_of("Inspectable");

    let fields = engine
        .world()
        .resource_field_layout(id)
        .expect("registration published the layout");
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "a");
    assert_eq!(fields[1].offset, 4);

    // A reshape moves the fields; the arriving layout has to replace the one
    // the relayout dropped.
    let after = resource_manifest_bytes(
        "Inspectable",
        12,
        4,
        2,
        vec![
            manifest_field("b", 0, 4),
            manifest_field("a", 4, 4),
            manifest_field("c", 8, 4),
        ],
    );
    apply_component_manifest_on_reload(&mut engine, &after, &store)
        .expect("a reshaped resource migrates");

    let fields = engine
        .world()
        .resource_field_layout(id)
        .expect("the reload republished the layout");
    assert_eq!(fields.len(), 3, "the arriving shape is what is served");
    assert_eq!(fields[0].name, "b", "and it is the arriving order");
}

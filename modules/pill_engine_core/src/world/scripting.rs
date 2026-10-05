//! Script components: registration and per-frame dispatch.
//!
//! # Responsibilities
//!
//! - Registers a component type that also carries an `update` method, and
//!   records the per-type updater that reaches it.
//! - Runs every script component once per frame, through a context that defers
//!   structural mutation to the command queue.
//!
//! # Design
//!
//! A script component is an ordinary component first: it registers through the
//! same path as any other, and the script half is a second table beside it.
//! Dispatch is by plain function pointer rather than a trait object, so no
//! vtable belonging to a retired artifact is ever called, and the pointers are
//! replaced wholesale when a generation re-registers.
//!
//! The updater receives raw pointers to the world and the command queue because
//! it needs both while the world is already borrowed to walk the columns. They
//! are valid for exactly the duration of one dispatch, which a non-capturing
//! function pointer is what guarantees: it cannot stash them.

use super::*;

// =============================================================================
// Script Updater Type
// =============================================================================

/// Function that updates a script component.
///
/// Takes: (storage, index, entity, world_ptr, commands_ptr).
/// Uses raw pointers to create a `ScriptContext` inside `update_scripts`.
///
/// SAFETY: The raw-pointer arguments (`world_ptr`, `commands_ptr`) are only
/// valid during the `update_scripts` call. Using a plain function pointer
/// (not a closure) guarantees that no state is captured and the callee
/// cannot stash the pointers for later use.
pub(super) type ScriptUpdater =
    fn(&mut ComponentColumns, usize, Entity, *mut World, *mut CommandQueue);

// =============================================================================
// World - Script Components
// =============================================================================

impl World {
    /// Register a script component type with the World
    ///
    /// Script components have an update() method that gets called by update_scripts().
    /// This must be called for each script component type before it can be used.
    pub fn register_script_component<T>(&mut self)
    where
        T: ScriptComponent,
    {
        let _zone = crate::profile_scope!(
            "register script component",
            [(
                "Script component type being registered: {}",
                std::any::type_name::<T>()
            )]
        );
        // First register as a normal component
        self.register_component::<T>();

        // Then track it as a script component
        let component_id = ComponentId::of::<T>();
        if let Some(bit) = self.component_registry.get_bit(&component_id) {
            self.script_components.push((component_id, bit));

            // Register updater callback for this script component.
            // Uses a non-capturing closure coerced to a function pointer
            // so that no state (especially no raw pointer) is captured.
            // The raw pointers are passed fresh by `update_scripts` on
            // every invocation.
            self.script_updaters.insert(
                component_id,
                (|storage: &mut ComponentColumns,
                  index: usize,
                  entity: Entity,
                  world_ptr: *mut World,
                  commands_ptr: *mut CommandQueue| {
                    // Get mutable reference to the component
                    let component = storage.column_of_mut::<T>().get_mut::<T>(index);
                    // SAFETY: `world_ptr` and `commands_ptr` are derived from
                    // `&mut World` / `&mut CommandQueue` that are valid for the
                    // entire duration of `update_scripts`, which is the sole
                    // caller of every stored updater. The function-pointer
                    // representation prevents these pointers from being cached
                    // across calls.
                    unsafe {
                        let mut script_context =
                            ScriptContext::new(&mut *world_ptr, &mut *commands_ptr, entity);
                        component.update(&mut script_context);
                    }
                }) as ScriptUpdater,
            );
        }
    }

    /// Update all script components
    ///
    /// Calls update() on every script component in the world.
    /// Scripts receive a `ScriptContext` with:
    /// - Read-only world access for queries
    /// - Deferred command queue for structural changes
    ///
    /// This ensures all structural changes (add/remove component, destroy entity)
    /// are automatically deferred, preventing use-after-free bugs.
    pub(crate) fn update_scripts(&mut self, commands: &mut CommandQueue) {
        let _zone = crate::profile_scope!(
            "update scripts",
            [(
                "Script component types in world: {}",
                self.script_components.len()
            )]
        );
        // Step 1: Reserve the per-frame work list and capture raw pointers to
        // self and the command queue before any field borrows on self.
        let total_entities = self.entity_locations.len();
        let mut entities_to_update: Vec<(Entity, ArchetypeId, usize)> =
            Vec::with_capacity(total_entities);

        // Take raw pointers once, BEFORE any field borrows on self.
        let world_ptr = self as *mut World;
        let commands_ptr = commands as *mut CommandQueue;

        // Step 2: For each script component type, gather every entity that
        // carries it.
        for &(component_id, comp_bit) in &self.script_components {
            // Get the updater for this component type.
            // Function pointers are Copy - no allocation here.
            let updater = match self.script_updaters.get(&component_id) {
                Some(&u) => u,
                None => continue,
            };

            // Collect entities that have this script component
            for (archetype_id, archetype) in &self.archetypes {
                // Check if this archetype has the script component using bitmask
                let mut mask = ComponentMask::empty();
                mask.set(comp_bit);

                if archetype.matches_mask(&mask) {
                    // Collect all entities in this archetype
                    for (index, &entity) in archetype.entities.iter().enumerate() {
                        entities_to_update.push((entity, *archetype_id, index));
                    }
                }
            }

            // Step 3: Sort the gathered entities for deterministic order
            // across runs, then dispatch each one to its updater with the
            // captured raw pointers.
            entities_to_update.sort_by_key(|(_, aid, idx)| (*aid, *idx));

            for (entity, archetype_id, index) in entities_to_update.drain(..) {
                if let Some(archetype) = self.archetypes.get_mut(&archetype_id) {
                    // Call the updater with mutable storage access
                    updater(
                        &mut archetype.component_storages,
                        index,
                        entity,
                        world_ptr,
                        commands_ptr,
                    );
                }
            }
        }
    }
}

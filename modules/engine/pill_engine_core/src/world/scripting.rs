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
//! same path as any other, and the script half is one table of updaters keyed by
//! component id. Dispatching asks the registry for the component's bit each
//! frame instead of storing it, so a retired and re-registered type cannot be
//! dispatched through a stale bit, and registering twice cannot queue the same
//! update twice. Dispatch goes through a plain function pointer rather than a
//! trait object, so the only thing an updater can be is a non-capturing
//! function: it cannot capture state, stash the raw pointers it is handed, or
//! keep anything alive past the call. Registering the type again overwrites its
//! entry.
//!
//! The updater receives raw pointers to the world and the command queue because
//! it needs both while the world is already borrowed to walk the columns. They
//! are valid for exactly the duration of one dispatch.

use super::*;

// =============================================================================
// Script Updater Type
// =============================================================================

/// Function that updates a script component.
///
/// Arguments: the component storage, the row index, the entity, and raw
/// pointers to the world and command queue that `update_scripts` turns into a
/// [`ScriptContext`] for one dispatch.
///
/// # Safety
///
/// The two raw-pointer arguments are valid only for the duration of the call
/// that passes them. An implementation must not store either one, and a caller
/// must pass pointers that stay valid for the whole call. Being a plain `fn`
/// and not a closure is part of the contract: it cannot capture, so it has
/// nowhere to keep them.
pub(super) type ScriptUpdater =
    fn(&mut ComponentColumns, usize, Entity, *mut World, *mut CommandQueue);

// =============================================================================
// World - Script Components
// =============================================================================

impl World {
    /// Registers a component type whose `update` runs once per frame per
    /// entity.
    ///
    /// Registers `T` as an ordinary component first, then records the updater
    /// `update_scripts` dispatches through. Registering the same type again
    /// replaces its updater instead of adding a second one.
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
        // Step 1: Register it as an ordinary component, so the registry holds
        // a bit to address its column by.
        self.register_component::<T>();

        // Step 2: Store the updater. The closure captures nothing, so it
        // coerces to the function pointer; the raw pointers are handed in
        // fresh on every dispatch.
        self.script_updaters.insert(
            ComponentId::of::<T>(),
            (|storage: &mut ComponentColumns,
              index: usize,
              entity: Entity,
              world_ptr: *mut World,
              commands_ptr: *mut CommandQueue| {
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

    /// Runs `update` on every script component of every entity that carries
    /// one, once per frame.
    ///
    /// Scripts receive a [`ScriptContext`] with component and resource reads, a
    /// mutable component borrow, and the command queue. Every structural change
    /// (add/remove component, destroy entity) is deferred there, so it is
    /// applied after all scripts have finished and no script can migrate a row
    /// out from under itself.
    pub(crate) fn update_scripts(&mut self, commands: &mut CommandQueue) {
        let _zone = crate::profile_scope!(
            "update scripts",
            [(
                "Script component types in world: {}",
                self.script_updaters.len()
            )]
        );
        // Step 1: Reserve the per-frame work list and capture raw pointers to
        // self and the command queue before any field borrows on self.
        let total_entities = self.entity_locations.len();
        let mut entities_to_update: Vec<(Entity, ArchetypeId, usize)> =
            Vec::with_capacity(total_entities);

        // Taken once, before any field borrow of `self`.
        let world_ptr = self as *mut World;
        let commands_ptr = commands as *mut CommandQueue;

        // Step 2: For each registered script type, gather every entity that
        // carries it. The bit is read from the registry here rather than
        // stored at registration time, so a type that was retired and
        // re-registered on another bit is still found.
        for (&component_id, &updater) in &self.script_updaters {
            let Some(script_bit) = self.component_registry.get_bit(&component_id) else {
                continue;
            };

            // Every entity of an archetype whose mask carries the script bit.
            for (archetype_id, archetype) in &self.archetypes {
                if archetype.has_component_bit(script_bit) {
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

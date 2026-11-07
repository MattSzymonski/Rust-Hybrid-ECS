//! Script components that change the world only through deferred commands.
//!
//! # Responsibilities
//!
//! - Defines [`ScriptComponent`] - a component trait with an `update()` method.
//! - Provides [`ScriptContext`] - a restricted view of the ECS that queues
//!   every structural change.
//!
//! # Design
//!
//! A script gets a [`ScriptContext`], never the [`crate::Engine`] itself. The
//! context has no way to add or remove a component, or to destroy an entity,
//! except by queueing the command. So a script cannot move its own row out of
//! the storage its `&mut self` points into, which would leave that reference
//! dangling.

// Standard library

// External crates

// Current crate
use crate::commands::CommandQueue;
use crate::component::Component;
use crate::entity::Entity;
use crate::world::World;
use crate::Resource;

// =============================================================================
// ScriptContext
// =============================================================================

/// A restricted view of the ECS for script components.
///
/// The context gives a script:
/// - read-only world queries: a component's value, an entity's existence;
/// - a mutable borrow of a component on any entity;
/// - read-only access to resources;
/// - deferred structural changes: create or destroy an entity, add or remove
///   a component.
///
/// In an archetype-based ECS, adding or removing a component moves the entity's
/// row into another archetype. A script that did that through `&mut World`
/// while holding `&mut self` would be left with a reference into storage the
/// entity has left. This type never hands out `&mut World`, and every
/// structural change it offers is queued, so a script cannot do that.
pub struct ScriptContext<'a> {
    /// Mutable access to the world for component queries
    world: &'a mut World,
    /// The command queue the deferred structural changes go into
    commands: &'a mut CommandQueue,
    /// The entity this script is attached to
    owning_entity: Entity,
}

impl<'a> ScriptContext<'a> {
    /// Creates a new script context.
    pub(crate) fn new(
        world: &'a mut World,
        commands: &'a mut CommandQueue,
        owning_entity: Entity,
    ) -> Self {
        Self {
            world,
            commands,
            owning_entity,
        }
    }

    /// Gets the entity this script is attached to.
    #[inline]
    pub fn get_owning_entity(&self) -> Entity {
        self.owning_entity
    }

    // =========================================================================
    // World Access
    // =========================================================================

    /// Gets an immutable reference to a component on any entity.
    ///
    /// Returns `None` if the entity does not exist or does not carry `T`.
    pub fn get_component<T>(&self, entity: Entity) -> Option<&T>
    where
        T: Component,
    {
        self.world.get_component::<T>(entity)
    }

    /// Gets a mutable reference to a component on any entity.
    ///
    /// Returns `None` if the entity does not exist or does not carry `T`.
    ///
    /// The reference can point at the script's own component. If the script
    /// asks for its own type, `&mut self` and the returned `&mut T` alias,
    /// which Rust calls undefined behaviour. The API accepts that because it
    /// cannot break in practice. The only thing that can invalidate either
    /// reference is a structural change to the entity, and this context only
    /// queues those.
    pub fn get_component_mut<T>(&mut self, entity: Entity) -> Option<&mut T>
    where
        T: Component,
    {
        // Step 1: Get a raw pointer to the component's storage slot. The
        // lookup returns `None` when the entity is missing or does not carry `T`.
        let component_pointer = self.world.get_component_ptr_mut::<T>(entity)?;

        // Step 2: Turn the raw pointer back into a `&mut T` reference.
        // SAFETY: `World::get_component_ptr_mut` either returns `None` (already
        // handled by `?`) or a valid, aligned, non-null pointer to a live `T`
        // component. The `&mut self` borrow lasts for the whole lifetime of the
        // returned reference, and every structural change (add/remove
        // component, destroy entity) is queued, so the storage slot cannot be
        // moved or freed while the reference is alive. A second `&mut T` for
        // the script's own component type is what the API is for, as the doc
        // comment above says.
        Some(unsafe { &mut *component_pointer })
    }

    /// Checks whether an entity exists.
    ///
    /// Returns `false` for a destroyed entity and for an old handle whose id
    /// has been reused.
    pub fn entity_exists(&self, entity: Entity) -> bool {
        self.world.is_entity_valid(entity)
    }

    /// Gets an immutable reference to a resource.
    ///
    /// Returns `None` if the world holds no resource of type `T`.
    pub fn get_resource<T: Resource>(&self) -> Option<&T> {
        self.world.get_resource::<T>()
    }

    // =========================================================================
    // Deferred Commands
    // =========================================================================

    /// Queues creating a new entity.
    ///
    /// The entity id is allocated now, and the entity joins the world when the
    /// queued commands run, after every script in the frame has returned. The
    /// returned [`crate::commands::DeferredEntityBuilder`] adds components to
    /// it.
    pub fn create_entity(&mut self) -> crate::commands::DeferredEntityBuilder<'_> {
        crate::commands::DeferredEntityBuilder::new(self.commands, self.world)
    }

    /// Queues destroying an entity.
    ///
    /// The command runs after every script in the frame has returned, so a
    /// script may queue the destruction of the entity it runs on.
    pub fn destroy_entity(&mut self, entity: Entity) {
        self.commands.destroy_entity(entity);
    }

    /// Queues adding a component to an entity.
    ///
    /// The command runs after every script in the frame has returned.
    pub fn add_component<T>(&mut self, entity: Entity, component: T)
    where
        T: Component + Send,
    {
        self.commands.add_component_to_entity(entity, component);
    }

    /// Queues removing a component from an entity.
    ///
    /// The command runs after every script in the frame has returned.
    pub fn remove_component<T: Component>(&mut self, entity: Entity) {
        self.commands.remove_component_from_entity::<T>(entity);
    }

    /// Returns the command queue, so a script can queue a command this context
    /// has no method for.
    ///
    /// Queued commands still run only after every script in the frame has
    /// returned.
    pub fn get_commands(&mut self) -> &mut CommandQueue {
        self.commands
    }
}

// =============================================================================
// ScriptComponent
// =============================================================================

/// A component that runs its own update logic every frame.
///
/// The engine calls `update` once per frame for every entity that carries the
/// component. The call gets a [`ScriptContext`], which offers:
/// - read access to components and resources;
/// - a mutable borrow of a component;
/// - structural commands that are queued.
///
/// Adding or removing a component and destroying an entity all happen after
/// every script in the frame has returned.
///
/// # Examples
///
/// ```ignore
/// struct PlayerController {
///     speed: f32,
/// }
///
/// impl ScriptComponent for PlayerController {
///     fn update(&mut self, script_context: &mut ScriptContext) {
///         // Read a component through the context.
///         if let Some(position) = script_context.get_component::<Position>(script_context.get_owning_entity()) {
///             println!("Player at {:?}", position);
///         }
///
///         // Structural changes are only queued.
///         if self.speed > 100.0 {
///             script_context.destroy_entity(script_context.get_owning_entity());
///         }
///
///         // The script's own fields stay directly usable.
///         self.speed += 1.0;
///     }
/// }
/// ```
pub trait ScriptComponent: Component {
    /// Updates this script component.
    ///
    /// The engine calls it once per frame for every entity that carries this
    /// component. `script_context.get_owning_entity()` returns the entity the
    /// script is attached to.
    fn update(&mut self, script_context: &mut ScriptContext);
}

// ============================================================================
// Entity System
// ============================================================================
//! Entity identification and management.
//!
//! Entities are unique identifiers for game objects. They don't contain data
//! themselves, but serve as keys to look up associated components.

/// Entity is a unique identifier for a game object
///
/// Entities are lightweight handles that reference a collection of components.
///
/// ## Generations
///
/// When an entity is destroyed, its ID is added to a free list for recycling.
/// The generation is incremented each time an ID is reused. This prevents
/// "dangling handle" bugs where old references incorrectly access new entities:
///
/// ```ignore
/// let enemy = world.create_entity().with(Health(100)).build();  // ID 5, gen 0
/// world.destroy_entity(enemy);  // ID 5 added to free list with gen 1
///
/// let bullet = world.create_entity().with(Damage(10)).build();  // Reuses ID 5, gen 1
///
/// // Old handle is safely invalidated:
/// world.is_entity_valid(enemy);  // false - gen 0 != gen 1
/// world.get_component::<Health>(enemy);  // None - entity no longer exists
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity {
    pub(crate) id: u64,
    pub(crate) generation: u32,
}

impl Entity {
    /// Create a new entity with the given id and generation (for testing purposes)
    #[cfg(test)]
    pub(crate) fn new_for_test(id: u64, generation: u32) -> Self {
        Self { id, generation }
    }
}

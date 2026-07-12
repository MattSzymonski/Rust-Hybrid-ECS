/// Entity is an "object" in the ECS world.
///
/// Entities are actually lightweight handles that reference a collection of components.
///
/// ## Generations
///
/// When an entity is destroyed, its ID is added to a free list for recycling.
/// The generation is incremented each time an ID is reused. This prevents
/// "dangling handle" bugs where old references incorrectly access new entities:
///
/// ```no_run
/// # use ecs_hybrid::*;
/// # use trait_type_map::impl_trait_accessible;
/// # #[derive(Debug, Clone)] struct Health(f32);
/// # impl Component for Health {}
/// # #[derive(Debug, Clone)] struct Damage(f32);
/// # impl Component for Damage {}
/// # impl_trait_accessible!(dyn Component; Health, Damage);
/// # let mut world = World::new();
/// let enemy = world.create_entity().with(Health(100.0)).build().unwrap();  // ID 5, gen 0
/// world.destroy_entity(enemy);  // ID 5 added to free list with gen 1
///
/// let bullet = world.create_entity().with(Damage(10.0)).build().unwrap();  // Reuses ID 5, gen 1
///
/// // Old handle is safely invalidated:
/// assert!(!world.is_entity_valid(enemy));  // gen 0 != gen 1
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Entity {
    /// Unique numeric identifier for this entity slot.
    ///
    /// IDs are recycled when entities are destroyed to prevent unbounded growth.
    /// The same ID may be reused for different entities over time, distinguished
    /// by the `generation` field.
    pub(crate) id: u64,

    /// Generation counter to distinguish reused entity IDs.
    ///
    /// Incremented each time an entity ID is recycled. This allows detecting
    /// "stale" entity handles that reference a destroyed entity whose ID was
    /// reused for a new entity. A handle is valid only if both id AND generation
    /// match the current entity at that slot.
    pub(crate) generation: u32,
}

impl std::fmt::Display for Entity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}v{}", self.id, self.generation)
    }
}

impl Entity {
    /// Create a new entity with the given id and generation (for testing purposes only)
    #[cfg(test)]
    pub(crate) fn new_for_test(id: u64, generation: u32) -> Self {
        Self { id, generation }
    }

    /// Numeric ID of this entity (slot index, not unique on its own - pair
    /// with [`generation`](Self::generation) for a unique handle).
    #[inline]
    pub fn id(self) -> u64 {
        self.id
    }

    /// Generation counter that disambiguates recycled IDs.
    #[inline]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn entity_size_and_alignment() {
        assert_eq!(
            std::mem::size_of::<Entity>(),
            16,
            "Entity size changed — check field order"
        );
        assert_eq!(std::mem::align_of::<Entity>(), 8);
    }
}

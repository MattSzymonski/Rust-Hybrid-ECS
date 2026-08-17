//! Lightweight entity handles with generation-based invalidation.
//!
//! # Responsibilities
//!
//! - Defines the [`Entity`] handle type used throughout the ECS.
//! - Provides generation-counter logic to prevent dangling-handle bugs.
//! - Keeps the handle compact (16 bytes) for cheap copy and storage.
//!
//! # Design
//!
//! Entities are not objects - they are 64-bit IDs paired with a 32-bit
//! generation counter. When an entity is destroyed, its ID is recycled via
//! a free list and the generation is incremented. Any old handle still
//! holding the previous generation will fail validation, preventing
//! use-after-free bugs without reference counting or garbage collection.

// Standard library
use std::fmt;

// =============================================================================
// Entity
// =============================================================================

/// Lightweight handle referencing a collection of components in the [`World`].
///
/// Entities are 16 bytes, [`Copy`], and cheap to pass by value. The
/// generation counter disambiguates recycled IDs:
///
/// ```no_run
/// # use pill_engine::*;
/// # use trait_type_map::impl_trait_accessible;
/// # #[derive(Debug, Clone)] struct Health(f32);
/// # impl Component for Health {}
/// # #[derive(Debug, Clone)] struct Damage(f32);
/// # impl Component for Damage {}
/// # impl_trait_accessible!(dyn Component; Health, Damage);
/// # let mut world = World::new();
/// let enemy = world.create_entity().with(Health(100.0)).build().unwrap();
/// world.destroy_entity(enemy);
///
/// let bullet = world.create_entity().with(Damage(10.0)).build().unwrap();
///
/// // Old handle is safely invalidated:
/// assert!(!world.is_entity_valid(enemy));
/// ```
#[repr(C)]
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

// =============================================================================
// Entity - Inherent Implementations
// =============================================================================

impl Entity {
    // -------------------------------------------------------------------------
    // Construction
    // -------------------------------------------------------------------------

    /// Creates an entity handle for use in test fixtures only.
    #[cfg(test)]
    pub(crate) fn new_for_test(id: u64, generation: u32) -> Self {
        Self { id, generation }
    }

    // -------------------------------------------------------------------------
    // Property accessors
    // -------------------------------------------------------------------------

    /// Returns the numeric slot identifier for this entity.
    ///
    /// Not unique on its own - pair with [`generation`](Self::generation)
    /// for a fully unique handle.
    #[inline]
    pub fn id(self) -> u64 {
        self.id
    }

    /// Returns the generation counter that disambiguates recycled entity IDs.
    #[inline]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

// =============================================================================
// Entity - Trait Implementations
// =============================================================================

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}v{}", self.id, self.generation)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that `Entity` is 16 bytes with 8-byte alignment.
    #[test]
    fn entity_size_and_alignment() {
        assert_eq!(
            std::mem::size_of::<Entity>(),
            16,
            "Entity size changed - check field order"
        );
        assert_eq!(std::mem::align_of::<Entity>(), 8);
    }
}

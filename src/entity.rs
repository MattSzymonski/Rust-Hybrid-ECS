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
/// The generation field allows for entity recycling (not yet implemented).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity {
    pub(crate) id: u64,
    pub(crate) generation: u32,
}

impl Entity {
    /// Create a new entity with the given id and generation (for testing purposes)
    pub fn new_for_test(id: u64, generation: u32) -> Self {
        Self { id, generation }
    }
}

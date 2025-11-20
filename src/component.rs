// ============================================================================
// Component System
// ============================================================================
//! Component trait and identification system.
//!
//! Components are data containers that can be attached to entities.
//! Each component type is uniquely identified by its TypeId.

use std::any::TypeId;

/// Component marker trait - all components must be 'static
///
/// This trait marks types that can be stored as components in the ECS.
/// The 'static bound ensures components don't contain non-static references.
pub trait Component: 'static {}

/// ComponentId uniquely identifies a component type using its TypeId
///
/// This is used internally to track which components are present in archetypes
/// and to perform fast lookups in component storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ComponentId(TypeId);

impl ComponentId {
    /// Create a ComponentId for a given component type
    pub fn of<T: Component>() -> Self {
        ComponentId(TypeId::of::<T>())
    }
}

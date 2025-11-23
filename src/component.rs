// ============================================================================
// Component System
// ============================================================================
//! Component trait and identification system.
//!
//! Components are data containers that can be attached to entities.
//! Each component type is uniquely identified by its TypeId and assigned a bit index.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Mutex;

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
pub struct ComponentId(TypeId);

impl ComponentId {
    /// Create a ComponentId for a given component type
    pub fn of<T: Component>() -> Self {
        ComponentId(TypeId::of::<T>())
    }

    /// Get the underlying TypeId
    pub fn type_id(&self) -> TypeId {
        self.0
    }
}

/// Bitmask for efficiently representing sets of components
/// Supports up to 128 component types (can be extended if needed)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentMask {
    bits: u128,
}

impl ComponentMask {
    /// Create an empty component mask
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    /// Create a mask with a single component bit set
    pub fn single(bit_index: u8) -> Self {
        assert!(bit_index < 128, "Component bit index must be < 128");
        Self {
            bits: 1u128 << bit_index,
        }
    }

    /// Set a component bit
    pub fn set(&mut self, bit_index: u8) {
        assert!(bit_index < 128, "Component bit index must be < 128");
        self.bits |= 1u128 << bit_index;
    }

    /// Check if this mask contains a specific bit
    pub fn contains(&self, bit_index: u8) -> bool {
        assert!(bit_index < 128, "Component bit index must be < 128");
        (self.bits & (1u128 << bit_index)) != 0
    }

    /// Check if this mask contains all bits from another mask
    pub fn contains_all(&self, other: &ComponentMask) -> bool {
        (self.bits & other.bits) == other.bits
    }

    /// Combine two masks (union)
    pub fn union(&self, other: &ComponentMask) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }

    /// Get the raw bit value (for hashing/comparison)
    pub fn bits(&self) -> u128 {
        self.bits
    }
}

/// Global registry mapping ComponentId to bit indices
static COMPONENT_REGISTRY: Mutex<Option<ComponentRegistry>> = Mutex::new(None);

struct ComponentRegistry {
    id_to_bit: HashMap<ComponentId, u8>,
    next_bit: u8,
}

impl ComponentRegistry {
    fn new() -> Self {
        Self {
            id_to_bit: HashMap::new(),
            next_bit: 0,
        }
    }

    fn register(&mut self, component_id: ComponentId) -> u8 {
        if let Some(&bit) = self.id_to_bit.get(&component_id) {
            return bit;
        }

        assert!(
            self.next_bit < 128,
            "Too many component types registered (max 128)"
        );
        let bit = self.next_bit;
        self.id_to_bit.insert(component_id, bit);
        self.next_bit += 1;
        bit
    }

    fn get_bit(&self, component_id: &ComponentId) -> Option<u8> {
        self.id_to_bit.get(component_id).copied()
    }
}

/// Register a component type and get its bit index
pub fn register_component_bit<T: Component>() -> u8 {
    let component_id = ComponentId::of::<T>();
    let mut registry = COMPONENT_REGISTRY.lock().unwrap();
    if registry.is_none() {
        *registry = Some(ComponentRegistry::new());
    }
    registry.as_mut().unwrap().register(component_id)
}

/// Get the bit index for a component type (must be registered first)
pub fn get_component_bit<T: Component>() -> u8 {
    let component_id = ComponentId::of::<T>();
    let registry = COMPONENT_REGISTRY.lock().unwrap();
    registry
        .as_ref()
        .and_then(|r| r.get_bit(&component_id))
        .expect("Component type not registered")
}

/// Get the bit index for a ComponentId (must be registered first)
pub fn get_component_bit_by_id(component_id: &ComponentId) -> Option<u8> {
    let registry = COMPONENT_REGISTRY.lock().unwrap();
    registry.as_ref().and_then(|r| r.get_bit(component_id))
}

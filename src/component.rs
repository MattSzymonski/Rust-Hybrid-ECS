// ============================================================================
// Component System
// ============================================================================
//! Component trait and identification system.

use std::any::TypeId;
use std::collections::HashMap;

/// Component marker trait - all components must be 'static and Send
pub trait Component: Send + 'static {}

/// ComponentId uniquely identifies a component type using its TypeId
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(pub TypeId);

impl ComponentId {
    pub fn of<T: Component>() -> Self {
        ComponentId(TypeId::of::<T>())
    }
}

/// Bitmask for efficiently representing sets of components (supports up to 128 component types)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ComponentMask(u128);

impl ComponentMask {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn set(&mut self, bit_index: u8) {
        self.0 |= 1u128 << bit_index;
    }

    /// Check if a specific bit is set (O(1) component type check)
    #[inline]
    pub fn has_bit(&self, bit_index: u8) -> bool {
        (self.0 & (1u128 << bit_index)) != 0
    }

    pub fn contains_all(&self, other: &ComponentMask) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// Component registry - moved to World struct
pub struct ComponentRegistry {
    id_to_bit: HashMap<ComponentId, u8>,
    names: HashMap<ComponentId, String>,
    next_bit: u8,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self {
            id_to_bit: HashMap::new(),
            names: HashMap::new(),
            next_bit: 0,
        }
    }
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ComponentRegistry {
    pub fn register<T: Component>(&mut self) -> u8 {
        let component_id = ComponentId::of::<T>();
        if let Some(&bit) = self.id_to_bit.get(&component_id) {
            return bit;
        }
        assert!(
            self.next_bit < 128,
            "Component type limit exceeded: cannot register {} (max 128 component types). \
             Consider combining related components or using a component with interior data variants.",
            std::any::type_name::<T>()
        );
        let bit = self.next_bit;
        self.id_to_bit.insert(component_id, bit);
        self.names
            .insert(component_id, std::any::type_name::<T>().to_string());
        self.next_bit += 1;
        bit
    }

    pub fn get_bit(&self, component_id: &ComponentId) -> Option<u8> {
        self.id_to_bit.get(component_id).copied()
    }

    pub fn get_name(&self, component_id: &ComponentId) -> Option<&str> {
        self.names.get(component_id).map(|s| s.as_str())
    }
}

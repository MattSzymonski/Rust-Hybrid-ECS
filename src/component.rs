// ============================================================================
// Component System
// ============================================================================
//! Component trait and identification system.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Mutex;

/// Component marker trait - all components must be 'static
pub trait Component: 'static {}

/// ComponentId uniquely identifies a component type using its TypeId
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(TypeId);

impl ComponentId {
    pub fn of<T: Component>() -> Self {
        ComponentId(TypeId::of::<T>())
    }
}

/// Bitmask for efficiently representing sets of components (supports up to 128 component types)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentMask(u128);

impl ComponentMask {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn set(&mut self, bit_index: u8) {
        self.0 |= 1u128 << bit_index;
    }

    pub fn contains_all(&self, other: &ComponentMask) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// Global component registry
static COMPONENT_REGISTRY: Mutex<Option<ComponentRegistry>> = Mutex::new(None);

struct ComponentRegistry {
    id_to_bit: HashMap<ComponentId, u8>,
    names: HashMap<ComponentId, &'static str>,
    next_bit: u8,
}

impl ComponentRegistry {
    fn new() -> Self {
        Self {
            id_to_bit: HashMap::new(),
            names: HashMap::new(),
            next_bit: 0,
        }
    }

    fn register(&mut self, component_id: ComponentId, name: &'static str) -> u8 {
        if let Some(&bit) = self.id_to_bit.get(&component_id) {
            return bit;
        }
        assert!(self.next_bit < 128, "Too many component types (max 128)");
        let bit = self.next_bit;
        self.id_to_bit.insert(component_id, bit);
        self.names.insert(component_id, name);
        self.next_bit += 1;
        bit
    }
}

/// Register a component type and get its bit index
pub fn register_component<T: Component>() -> u8 {
    let mut registry = COMPONENT_REGISTRY.lock().unwrap();
    let registry = registry.get_or_insert_with(ComponentRegistry::new);
    registry.register(ComponentId::of::<T>(), std::any::type_name::<T>())
}

/// Get the bit index for a ComponentId
pub fn get_component_bit(component_id: &ComponentId) -> Option<u8> {
    COMPONENT_REGISTRY
        .lock()
        .unwrap()
        .as_ref()?
        .id_to_bit
        .get(component_id)
        .copied()
}

/// Get the registered name for a component type
pub fn get_component_name(component_id: &ComponentId) -> Option<&'static str> {
    COMPONENT_REGISTRY
        .lock()
        .unwrap()
        .as_ref()?
        .names
        .get(component_id)
        .copied()
}

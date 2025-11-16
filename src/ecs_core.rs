use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub struct ScriptComponent {
    pub some_value: f32,
}

// ScriptComponent trait - for components that have update logic
impl ScriptComponent {
    fn update(&mut self, world: &mut World) {}
}

// World manages all entities and components
pub struct World {
    components: Vec<ScriptComponent>,
}

impl World {
    pub fn new() -> Self {
        Self {
            components: Vec::new(),
        }
    }

    // Add a component to an entity
    pub fn add_component(&mut self, component: ScriptComponent) {
        self.components.push(component);
    }

    // Get a component from an entity
    pub fn get_component(&self, index: usize) -> Option<&ScriptComponent> {
        self.components.get(index)
    }

    // Get a mutable component from an entity
    pub fn get_component_mut(&mut self, index: usize) -> Option<&mut ScriptComponent> {
        self.components.get_mut(index)
    }

    pub fn components_iterator(&self) -> impl Iterator<Item = &ScriptComponent> {
        self.components.iter()
    }

    // Update all script components for all entities
    pub fn update_scripts(&mut self) {
        // UNSAFE: Get raw pointer to self to bypass borrow checker
        // This allows us to have mutable access to components while passing immutable world reference
        let world_ptr = self as *mut World;

        self.components.iter_mut().for_each(|component| unsafe {
            component.update(&mut *world_ptr);
        });
    }
}

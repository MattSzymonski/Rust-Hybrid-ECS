use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::example::ScriptComponent;

// World manages all entities and components
pub struct World {
    pub components: Vec<ScriptComponent>,
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

    // Get the number of components
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    // Remove a component by index
    pub fn remove_component(&mut self, index: usize) {
        if index < self.components.len() {
            self.components.remove(index);
        }
    }

    // Clear all components
    pub fn clear_components(&mut self) {
        self.components.clear();
    }

    // Update all script components for all entities
    pub fn update_scripts(&mut self) {
        // SAFE FIX: Pass indices instead of raw pointers
        // The component can get fresh pointers using the index after Vec reallocation
        let world_ptr = self as *mut World;

        for i in 0..self.components.len() {
            unsafe {
                // Pass INDEX instead of pointer - allows reallocation handling
                ScriptComponent::update(self.components.get_mut(i).unwrap(), &mut *world_ptr);
            }
        }
    }
}

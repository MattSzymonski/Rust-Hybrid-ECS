use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use trait_type_map::{TraitAccessible, TraitTypeMap, VecFamily};

// Component ID mapping for bitmask generation (lazy initialized)
static COMPONENT_IDS: OnceLock<Mutex<HashMap<TypeId, u32>>> = OnceLock::new();

// Get unique component bit for bitmask (limited to 64 component types)
fn get_component_bit<T: Component + 'static>() -> u64 {
    let ids = COMPONENT_IDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut ids = ids.lock().unwrap();
    let type_id = TypeId::of::<T>();

    if let Some(&id) = ids.get(&type_id) {
        1u64 << id
    } else {
        let id = ids.len() as u32;
        if id >= 64 {
            panic!("Too many component types! Maximum 64 supported for bitmask.");
        }
        ids.insert(type_id, id);
        1u64 << id
    }
}

// Entity is just a unique ID that doubles as an array index
// It also stores a bitmask of which components it has
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity {
    id: u64,
    pub(crate) bitmask: u64,
}

impl Entity {
    // Create a new entity with the given ID
    fn new(id: u64) -> Self {
        Self { id, bitmask: 0 }
    }

    // Get the index for array-based storage
    pub fn index(&self) -> usize {
        self.id as usize
    }

    // Get the ID
    pub fn id(&self) -> u64 {
        self.id
    }
}

pub trait Component {
    // Helper method to cast Component to ScriptComponent if possible
    fn as_script(&self) -> Option<&dyn ScriptComponent> {
        None
    }

    fn as_script_mut(&mut self) -> Option<&mut dyn ScriptComponent> {
        None
    }
}

// ScriptComponent trait - for components that have update logic
pub trait ScriptComponent: Component {
    fn update(&mut self, entity: Entity, world: &mut World);
}

// World manages all entities and components
pub struct World {
    next_entity_id: u64,
    entities: Vec<Entity>,
    components: TraitTypeMap<dyn Component, VecFamily>,
    // Track which component types are scripts (TypeId -> component bit)
    script_component_types: Vec<(TypeId, u64)>,
}

impl World {
    pub fn new() -> Self {
        Self {
            next_entity_id: 0,
            entities: Vec::new(),
            components: TraitTypeMap::new(),
            script_component_types: Vec::new(),
        }
    }

    pub fn register_component_type<T>(&mut self)
    where
        T: 'static + TraitAccessible<dyn Component> + Component + Default,
    {
        self.components.register_type_storage::<T>();

        // Check if this type is a script component by creating a temporary instance
        let temp = T::default();
        if temp.as_script().is_some() {
            let type_id = TypeId::of::<T>();
            let bit = get_component_bit::<T>();
            self.script_component_types.push((type_id, bit));
        }
    }

    // Create a new entity
    pub fn create_entity(&mut self) -> Entity {
        let entity = Entity::new(self.next_entity_id);
        self.next_entity_id += 1;
        self.entities.push(entity);
        entity
    }

    // Add a component to an entity
    pub fn add_component<T>(&mut self, entity: Entity, component: T)
    where
        T: 'static + Component + TraitAccessible<dyn Component>,
    {
        let component_storage = self.components.get_storage_mut::<T>();
        component_storage.push(component);

        // Update entity's bitmask
        let bit = get_component_bit::<T>();
        if let Some(stored_entity) = self.entities.get_mut(entity.index()) {
            stored_entity.bitmask |= bit;
        }
    }

    // Check if entity has a component (fast bitmask check)
    pub fn has_component<T: Component + 'static>(&self, entity: Entity) -> bool {
        let bit = get_component_bit::<T>();
        (entity.bitmask & bit) != 0
    }

    // Get a component from an entity
    pub fn get_component<T: Component + 'static>(&self, entity: Entity) -> Option<&T> {
        let component_storage = self.components.get_storage::<T>();
        component_storage.get(entity.index())
    }

    // Get a mutable component from an entity
    pub fn get_component_mut<T: Component + 'static>(&mut self, entity: Entity) -> Option<&mut T> {
        let component_storage = self.components.get_storage_mut::<T>();
        component_storage.get_mut(entity.index())
    }

    // Iterator-based query for two components (zero-allocation, bitmask optimized)
    pub fn get_two_component_iterator<A, B>(&self) -> impl Iterator<Item = (Entity, &A, &B)>
    where
        A: Component + 'static,
        B: Component + 'static,
    {
        // Generate filter bitmask
        let bit_a = get_component_bit::<A>();
        let bit_b = get_component_bit::<B>();
        let filter_bitmask = bit_a | bit_b;

        // Get storages
        let storage_a = self.components.get_storage::<A>();
        let storage_b = self.components.get_storage::<B>();

        // // Create iterator
        self.entities
            .iter()
            .filter(move |entity| entity.bitmask & filter_bitmask == filter_bitmask)
            .filter_map(move |&entity| {
                match (storage_a.get(entity.index()), storage_b.get(entity.index())) {
                    (Some(a), Some(b)) => Some((entity, a, b)),
                    _ => None,
                }
            })
    }

    // Update all script components for all entities
    pub fn update_scripts(&mut self) {
        // UNSAFE: Get raw pointer to self to bypass borrow checker
        // This allows us to have mutable access to components while passing immutable world reference
        let world_ptr = self as *mut World;

        // Iterate over all registered script component types
        for &(script_type_id, script_bit) in &self.script_component_types {
            // Get storage reference for this script component type
            if let Some(storage) = self.components.get_trait_storage_mut(script_type_id) {
                for entity in &self.entities {
                    // Fast bitmask check - does this entity have this script component?
                    if (entity.bitmask & script_bit) == 0 {
                        continue;
                    }

                    if let Some(component) = storage.get_mut(entity.index()) {
                        // Cast to ScriptComponent trait object and call update!
                        if let Some(script) = component.as_script_mut() {
                            unsafe {
                                script.update(*entity, &mut *world_ptr);
                            }
                        }
                    }
                }
            }
        }
    }
}

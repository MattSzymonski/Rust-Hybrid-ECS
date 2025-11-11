/// Core ECS implementation - the performance-critical parallel system
use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Component storage - type-erased for flexibility
pub trait ComponentStorage: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn remove(&mut self, entity: u64);
}

/// Concrete storage for a specific component type
pub struct TypedStorage<T: 'static> {
    components: HashMap<u64, Vec<T>>, // Support multiple components per entity
}

impl<T: 'static> TypedStorage<T> {
    pub fn new() -> Self {
        Self {
            components: HashMap::new(),
        }
    }

    pub fn insert(&mut self, entity: u64, component: T) {
        self.components
            .entry(entity)
            .or_insert_with(Vec::new)
            .push(component);
    }

    pub fn get(&self, entity: u64) -> Option<&T> {
        self.components.get(&entity).and_then(|v| v.first())
    }

    pub fn get_mut(&mut self, entity: u64) -> Option<&mut T> {
        self.components.get_mut(&entity).and_then(|v| v.first_mut())
    }

    pub fn get_all(&self, entity: u64) -> Vec<T>
    where
        T: Clone,
    {
        self.components.get(&entity).cloned().unwrap_or_default()
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, &T)> {
        self.components
            .iter()
            .filter_map(|(e, v)| v.first().map(|c| (*e, c)))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (u64, &mut T)> {
        self.components
            .iter_mut()
            .filter_map(|(e, v)| v.first_mut().map(|c| (*e, c)))
    }
}

impl<T: Send + Sync + 'static> ComponentStorage for TypedStorage<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn remove(&mut self, entity: u64) {
        self.components.remove(&entity);
    }
}

/// The core ECS world - thread-safe and parallel-friendly
pub struct World {
    next_entity_id: u64,
    storages: HashMap<TypeId, Box<dyn ComponentStorage>>,
    entities: Vec<u64>,
}

impl World {
    pub fn new() -> Self {
        Self {
            next_entity_id: 0,
            storages: HashMap::new(),
            entities: Vec::new(),
        }
    }

    pub fn create_entity(&mut self) -> u64 {
        let entity = self.next_entity_id;
        self.next_entity_id += 1;
        self.entities.push(entity);
        entity
    }

    /// Create a new entity ID without registering it yet
    pub fn create_entity_id(&mut self) -> u64 {
        let id = self.next_entity_id;
        self.next_entity_id += 1;
        id
    }

    /// Register an entity in the world
    pub fn register_entity(&mut self, entity: u64) {
        if !self.entities.contains(&entity) {
            self.entities.push(entity);
        }
    }

    pub fn add_component<T: Send + Sync + 'static>(&mut self, entity: u64, component: T) {
        let type_id = TypeId::of::<T>();

        let storage = self
            .storages
            .entry(type_id)
            .or_insert_with(|| Box::new(TypedStorage::<T>::new()));

        storage
            .as_any_mut()
            .downcast_mut::<TypedStorage<T>>()
            .unwrap()
            .insert(entity, component);
    }

    pub fn get_component<T: 'static>(&self, entity: u64) -> Option<&T> {
        let type_id = TypeId::of::<T>();
        self.storages
            .get(&type_id)?
            .as_any()
            .downcast_ref::<TypedStorage<T>>()
            .and_then(|storage| storage.get(entity))
    }

    pub fn get_component_mut<T: 'static>(&mut self, entity: u64) -> Option<&mut T> {
        let type_id = TypeId::of::<T>();
        self.storages
            .get_mut(&type_id)?
            .as_any_mut()
            .downcast_mut::<TypedStorage<T>>()
            .and_then(|storage| storage.get_mut(entity))
    }

    pub fn remove_component<T: 'static>(&mut self, entity: u64) {
        let type_id = TypeId::of::<T>();
        if let Some(storage) = self.storages.get_mut(&type_id) {
            storage.remove(entity);
        }
    }

    pub fn query<T: 'static>(&self) -> Option<impl Iterator<Item = (u64, &T)>> {
        let type_id = TypeId::of::<T>();
        self.storages
            .get(&type_id)
            .and_then(|storage| storage.as_any().downcast_ref::<TypedStorage<T>>())
            .map(|storage| storage.iter())
    }

    pub fn query_mut<T: 'static>(&mut self) -> Option<impl Iterator<Item = (u64, &mut T)>> {
        let type_id = TypeId::of::<T>();
        self.storages
            .get_mut(&type_id)
            .and_then(|storage| storage.as_any_mut().downcast_mut::<TypedStorage<T>>())
            .map(|storage| storage.iter_mut())
    }

    pub fn destroy_entity(&mut self, entity: u64) {
        // Remove from all storages
        for storage in self.storages.values_mut() {
            storage.remove(entity);
        }
        // Remove from entity list
        self.entities.retain(|e| *e != entity);
    }

    /// Get all entities
    pub fn entities(&self) -> impl Iterator<Item = u64> + '_ {
        self.entities.iter().copied()
    }

    /// Get all components of a specific type for an entity (for multiple instances)
    pub fn get_components<T: Clone + 'static>(&self, entity: u64) -> Vec<T> {
        let type_id = TypeId::of::<T>();
        if let Some(storage) = self.storages.get(&type_id) {
            if let Some(typed_storage) = storage.as_any().downcast_ref::<TypedStorage<T>>() {
                return typed_storage.get_all(entity);
            }
        }
        Vec::new()
    }

    /// Query two components - read-only for both
    pub fn query2<T1: 'static, T2: 'static>(&self) -> impl Iterator<Item = (&T1, &T2)> + '_ {
        let type_id1 = TypeId::of::<T1>();
        let type_id2 = TypeId::of::<T2>();

        let storage1 = self
            .storages
            .get(&type_id1)
            .and_then(|s| s.as_any().downcast_ref::<TypedStorage<T1>>());

        let storage2 = self
            .storages
            .get(&type_id2)
            .and_then(|s| s.as_any().downcast_ref::<TypedStorage<T2>>());

        // Iterate over entities that have both components
        storage1
            .into_iter()
            .flat_map(move |s1| s1.iter())
            .filter_map(move |(entity, comp1)| {
                storage2.and_then(|s2| s2.get(entity).map(|comp2| (comp1, comp2)))
            })
    }

    /// Query two components - first mutable, second read-only
    pub fn query2_mut<T1: 'static, T2: 'static>(&mut self) -> Vec<(&mut T1, &T2)> {
        let type_id1 = TypeId::of::<T1>();
        let type_id2 = TypeId::of::<T2>();

        // Get raw pointers to avoid borrow checker issues
        let storage1_ptr = self
            .storages
            .get_mut(&type_id1)
            .map(|s| s.as_mut() as *mut dyn ComponentStorage);
        let storage2_ptr = self
            .storages
            .get(&type_id2)
            .map(|s| s.as_ref() as *const dyn ComponentStorage);

        if storage1_ptr.is_none() || storage2_ptr.is_none() {
            return Vec::new();
        }

        let storage1_ptr = storage1_ptr.unwrap();
        let storage2_ptr = storage2_ptr.unwrap();

        unsafe {
            let s1 = (*storage1_ptr)
                .as_any_mut()
                .downcast_mut::<TypedStorage<T1>>()
                .unwrap();
            let s2 = (*storage2_ptr)
                .as_any()
                .downcast_ref::<TypedStorage<T2>>()
                .unwrap();

            // Collect entities that have both components
            let mut results = Vec::new();
            for (entity, comp1) in s1.iter_mut() {
                if let Some(comp2) = s2.get(entity) {
                    results.push((comp1, comp2));
                }
            }
            results
        }
    }
}

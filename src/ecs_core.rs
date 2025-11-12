/// Core ECS implementation - the performance-critical parallel system
use std::any::{Any, TypeId};
use std::collections::HashMap;

/// Component storage - type-erased for flexibility
pub trait ComponentStorage: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn remove(&mut self, entity: u64);
}

// ---------------------------------------------------------------------------------------------------------------------

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

    /// Get multiple mutable references from different entities
    /// SAFETY: Caller must ensure entity IDs are unique
    pub unsafe fn get_many_mut<const N: usize>(
        &mut self,
        entities: [u64; N],
    ) -> [Option<&mut T>; N] {
        let mut results: [Option<&mut T>; N] = std::array::from_fn(|_| None);

        for (i, &entity) in entities.iter().enumerate() {
            if let Some(vec) = self.components.get_mut(&entity) {
                if let Some(component) = vec.first_mut() {
                    // SAFETY: We're transmuting the lifetime to 'static temporarily
                    // This is safe because we know the caller ensures unique entity IDs
                    let ptr = component as *mut T;
                    results[i] = Some(&mut *ptr);
                }
            }
        }

        results
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

// ---------------------------------------------------------------------------------------------------------------------

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

    // pub fn get_component_non_st<T>(&self, entity: u64) -> Option<&T> {
    //     let type_id = TypeId::of::<T>();
    //     self.storages
    //         .get(&type_id)?
    //         .as_any()
    //         .downcast_ref::<TypedStorage<T>>()
    //         .and_then(|storage| storage.get(entity))
    // }

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

    /// Access all components of a specific type for an entity through a closure (no cloning)
    pub fn with_components<T: 'static, R, F>(&self, entity: u64, f: F) -> Option<R>
    where
        F: FnOnce(&[T]) -> R,
    {
        let type_id = TypeId::of::<T>();
        self.storages.get(&type_id).and_then(|storage| {
            storage
                .as_any()
                .downcast_ref::<TypedStorage<T>>()
                .and_then(|typed_storage| {
                    typed_storage
                        .components
                        .get(&entity)
                        .map(|vec| f(vec.as_slice()))
                })
        })
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

    /// Query two components - both mutable
    pub fn query2_mut_mut<T1: 'static, T2: 'static>(&mut self) -> Vec<(&mut T1, &mut T2)> {
        let type_id1 = TypeId::of::<T1>();
        let type_id2 = TypeId::of::<T2>();

        // Get raw pointers to avoid borrow checker issues
        let storage1_ptr = self
            .storages
            .get_mut(&type_id1)
            .map(|s| s.as_mut() as *mut dyn ComponentStorage);
        let storage2_ptr = self
            .storages
            .get_mut(&type_id2)
            .map(|s| s.as_mut() as *mut dyn ComponentStorage);

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
                .as_any_mut()
                .downcast_mut::<TypedStorage<T2>>()
                .unwrap();

            // First collect entity IDs that have both components
            let entities: Vec<u64> = s1
                .iter()
                .filter_map(|(entity, _)| {
                    if s2.get(entity).is_some() {
                        Some(entity)
                    } else {
                        None
                    }
                })
                .collect();

            // Now get mutable references using raw pointer access
            let mut results = Vec::new();
            let s1_map = &mut s1.components as *mut HashMap<u64, Vec<T1>>;
            let s2_map = &mut s2.components as *mut HashMap<u64, Vec<T2>>;

            for entity in entities {
                let comp1 = (*s1_map).get_mut(&entity).and_then(|v| v.first_mut());
                let comp2 = (*s2_map).get_mut(&entity).and_then(|v| v.first_mut());

                if let (Some(c1), Some(c2)) = (comp1, comp2) {
                    results.push((c1, c2));
                }
            }
            results
        }
    }

    /// Query three components - all read-only
    pub fn query3<T1: 'static, T2: 'static, T3: 'static>(
        &self,
    ) -> impl Iterator<Item = (&T1, &T2, &T3)> + '_ {
        let type_id1 = TypeId::of::<T1>();
        let type_id2 = TypeId::of::<T2>();
        let type_id3 = TypeId::of::<T3>();

        let storage1 = self
            .storages
            .get(&type_id1)
            .and_then(|s| s.as_any().downcast_ref::<TypedStorage<T1>>());

        let storage2 = self
            .storages
            .get(&type_id2)
            .and_then(|s| s.as_any().downcast_ref::<TypedStorage<T2>>());

        let storage3 = self
            .storages
            .get(&type_id3)
            .and_then(|s| s.as_any().downcast_ref::<TypedStorage<T3>>());

        storage1
            .into_iter()
            .flat_map(move |s1| s1.iter())
            .filter_map(move |(entity, comp1)| {
                storage2.and_then(|s2| {
                    s2.get(entity).and_then(|comp2| {
                        storage3.and_then(|s3| s3.get(entity).map(|comp3| (comp1, comp2, comp3)))
                    })
                })
            })
    }

    /// Query three components - all mutable
    pub fn query3_mut<T1: 'static, T2: 'static, T3: 'static>(
        &mut self,
    ) -> Vec<(&mut T1, &mut T2, &mut T3)> {
        let type_id1 = TypeId::of::<T1>();
        let type_id2 = TypeId::of::<T2>();
        let type_id3 = TypeId::of::<T3>();

        // Get raw pointers to avoid borrow checker issues
        let storage1_ptr = self
            .storages
            .get_mut(&type_id1)
            .map(|s| s.as_mut() as *mut dyn ComponentStorage);
        let storage2_ptr = self
            .storages
            .get_mut(&type_id2)
            .map(|s| s.as_mut() as *mut dyn ComponentStorage);
        let storage3_ptr = self
            .storages
            .get_mut(&type_id3)
            .map(|s| s.as_mut() as *mut dyn ComponentStorage);

        if storage1_ptr.is_none() || storage2_ptr.is_none() || storage3_ptr.is_none() {
            return Vec::new();
        }

        let storage1_ptr = storage1_ptr.unwrap();
        let storage2_ptr = storage2_ptr.unwrap();
        let storage3_ptr = storage3_ptr.unwrap();

        unsafe {
            let s1 = (*storage1_ptr)
                .as_any_mut()
                .downcast_mut::<TypedStorage<T1>>()
                .unwrap();
            let s2 = (*storage2_ptr)
                .as_any_mut()
                .downcast_mut::<TypedStorage<T2>>()
                .unwrap();
            let s3 = (*storage3_ptr)
                .as_any_mut()
                .downcast_mut::<TypedStorage<T3>>()
                .unwrap();

            // First collect entity IDs that have all three components
            let entities: Vec<u64> = s1
                .iter()
                .filter_map(|(entity, _)| {
                    if s2.get(entity).is_some() && s3.get(entity).is_some() {
                        Some(entity)
                    } else {
                        None
                    }
                })
                .collect();

            // Now get mutable references using raw pointer access
            let mut results = Vec::new();
            let s1_map = &mut s1.components as *mut HashMap<u64, Vec<T1>>;
            let s2_map = &mut s2.components as *mut HashMap<u64, Vec<T2>>;
            let s3_map = &mut s3.components as *mut HashMap<u64, Vec<T3>>;

            for entity in entities {
                let comp1 = (*s1_map).get_mut(&entity).and_then(|v| v.first_mut());
                let comp2 = (*s2_map).get_mut(&entity).and_then(|v| v.first_mut());
                let comp3 = (*s3_map).get_mut(&entity).and_then(|v| v.first_mut());

                if let (Some(c1), Some(c2), Some(c3)) = (comp1, comp2, comp3) {
                    results.push((c1, c2, c3));
                }
            }
            results
        }
    }
}

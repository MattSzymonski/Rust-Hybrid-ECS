// ECS-like Storage Architecture MVP
//
// This implementation provides a minimal Entity Component System with:
// - Entity: Unique identifiers for game objects
// - Component: Data containers (Position, Velocity, Name, etc.)
// - World: Manages all entities and their components
//
// Key Features:
// - Type-safe component storage using TypeId
// - Entity creation and management
// - Component add/remove/query operations
// - Query system for filtering entities by components
// - Systems can be implemented as functions that query and modify components

use std::any::{Any, TypeId};
use std::collections::HashMap;

// Entity is just a unique ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity(u64);

// Component trait marker - all components must implement this
pub trait Component: Any + Send + Sync {}

// ScriptComponent trait - for components that have update logic
pub trait ScriptComponent: Component {
    fn update(&mut self, entity: Entity, world: &World, ctx: &mut UpdateContext);
}

// Context for script updates that allows mutations
pub struct UpdateContext {
    // Store component mutations to apply after all scripts run
    position_updates: HashMap<Entity, Position>,
}

impl UpdateContext {
    fn new() -> Self {
        Self {
            position_updates: HashMap::new(),
        }
    }

    pub fn set_position(&mut self, entity: Entity, x: f32, y: f32) {
        self.position_updates.insert(entity, Position { x, y });
    }

    pub fn move_position(&mut self, entity: Entity, dx: f32, dy: f32, world: &World) {
        if let Some(pos) = world.get_component::<Position>(entity) {
            self.position_updates.insert(
                entity,
                Position {
                    x: pos.x + dx,
                    y: pos.y + dy,
                },
            );
        }
    }

    // Move position with collision detection
    pub fn move_position_with_collision(
        &mut self,
        entity: Entity,
        dx: f32,
        dy: f32,
        world: &World,
    ) {
        if let Some(pos) = world.get_component::<Position>(entity) {
            let mut new_x = pos.x + dx;
            let mut new_y = pos.y + dy;

            // Check collision with all box colliders in the world
            for (_collider_entity, collider_pos, collider) in
                world.query2::<Position, BoxCollider>()
            {
                // Create a temporary collider for the moving entity (assume small size)
                let mover_collider = BoxCollider::new(10.0, 10.0);
                let test_pos = Position { x: new_x, y: new_y };

                // Check if the new position would collide
                if mover_collider.overlaps(&test_pos, collider, collider_pos) {
                    // Collision detected - clamp to collider edge
                    let half_width = mover_collider.width / 2.0;
                    let half_height = mover_collider.height / 2.0;
                    let c_half_width = collider.width / 2.0;
                    let c_half_height = collider.height / 2.0;

                    // Calculate overlap on each axis
                    let overlap_left = (collider_pos.x - c_half_width) - (new_x + half_width);
                    let overlap_right = (new_x - half_width) - (collider_pos.x + c_half_width);
                    let overlap_bottom = (collider_pos.y - c_half_height) - (new_y + half_height);
                    let overlap_top = (new_y - half_height) - (collider_pos.y + c_half_height);

                    // Find the smallest overlap to determine collision direction
                    let min_overlap_x = if overlap_left.abs() < overlap_right.abs() {
                        overlap_left
                    } else {
                        overlap_right
                    };
                    let min_overlap_y = if overlap_bottom.abs() < overlap_top.abs() {
                        overlap_bottom
                    } else {
                        overlap_top
                    };

                    // Clamp position to collider edge
                    if min_overlap_x.abs() < min_overlap_y.abs() {
                        new_x += min_overlap_x;
                    } else {
                        new_y += min_overlap_y;
                    }
                }
            }

            self.position_updates
                .insert(entity, Position { x: new_x, y: new_y });
        }
    }
}

// Position component needs to be public here for UpdateContext
#[derive(Debug, Clone)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

impl Component for Position {}

// Sprite rendering component
#[derive(Debug, Clone)]
pub struct Sprite {
    pub color: (f32, f32, f32), // RGB color (0.0-1.0)
    pub width: f32,             // Width of the sprite
    pub height: f32,            // Height of the sprite
}

impl Component for Sprite {}

impl Sprite {
    pub fn new(color: (f32, f32, f32), width: f32, height: f32) -> Self {
        Self {
            color,
            width,
            height,
        }
    }
}

// Box Collider component - 2D axis-aligned bounding box
#[derive(Debug, Clone)]
pub struct BoxCollider {
    pub width: f32,
    pub height: f32,
}

impl Component for BoxCollider {}

impl BoxCollider {
    pub fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    // Check if a point is inside this collider (given the collider's position)
    pub fn contains_point(&self, collider_pos: &Position, point_x: f32, point_y: f32) -> bool {
        let half_width = self.width / 2.0;
        let half_height = self.height / 2.0;

        point_x >= collider_pos.x - half_width
            && point_x <= collider_pos.x + half_width
            && point_y >= collider_pos.y - half_height
            && point_y <= collider_pos.y + half_height
    }

    // Check if two box colliders overlap
    pub fn overlaps(&self, pos1: &Position, other: &BoxCollider, pos2: &Position) -> bool {
        let half_width1 = self.width / 2.0;
        let half_height1 = self.height / 2.0;
        let half_width2 = other.width / 2.0;
        let half_height2 = other.height / 2.0;

        let left1 = pos1.x - half_width1;
        let right1 = pos1.x + half_width1;
        let top1 = pos1.y + half_height1;
        let bottom1 = pos1.y - half_height1;

        let left2 = pos2.x - half_width2;
        let right2 = pos2.x + half_width2;
        let top2 = pos2.y + half_height2;
        let bottom2 = pos2.y - half_height2;

        !(right1 < left2 || left1 > right2 || top1 < bottom2 || bottom1 > top2)
    }
}

// Trait object wrapper for script storage that can be updated
trait ScriptStorageUpdater: Send + Sync {
    fn update_all(&mut self, world: &World, ctx: &mut UpdateContext);
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

// Concrete implementation for a specific script component type
struct TypedScriptStorage<T: ScriptComponent> {
    data: HashMap<Entity, T>,
}

impl<T: ScriptComponent + 'static> TypedScriptStorage<T> {
    fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    fn insert(&mut self, entity: Entity, component: T) {
        self.data.insert(entity, component);
    }
}

impl<T: ScriptComponent + 'static> ScriptStorageUpdater for TypedScriptStorage<T> {
    fn update_all(&mut self, world: &World, ctx: &mut UpdateContext) {
        // Collect entities to avoid borrow checker issues
        let entities: Vec<Entity> = self.data.keys().copied().collect();

        for entity in entities {
            if let Some(component) = self.data.get_mut(&entity) {
                component.update(entity, world, ctx);
            }
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// Type-erased storage for components
pub struct ComponentStorage {
    data: Box<dyn Any + Send + Sync>,
}

impl ComponentStorage {
    pub fn new<T: Component + 'static>() -> Self {
        Self {
            data: Box::new(HashMap::<Entity, T>::new()),
        }
    }

    pub fn insert<T: Component + 'static>(&mut self, entity: Entity, component: T) {
        if let Some(map) = self.data.downcast_mut::<HashMap<Entity, T>>() {
            map.insert(entity, component);
        }
    }

    pub fn get<T: Component + 'static>(&self, entity: Entity) -> Option<&T> {
        self.data
            .downcast_ref::<HashMap<Entity, T>>()
            .and_then(|map| map.get(&entity))
    }

    pub fn get_mut<T: Component + 'static>(&mut self, entity: Entity) -> Option<&mut T> {
        self.data
            .downcast_mut::<HashMap<Entity, T>>()
            .and_then(|map| map.get_mut(&entity))
    }

    pub fn remove<T: Component + 'static>(&mut self, entity: Entity) -> Option<T> {
        self.data
            .downcast_mut::<HashMap<Entity, T>>()
            .and_then(|map| map.remove(&entity))
    }

    pub fn entities<T: Component + 'static>(&self) -> Vec<Entity> {
        self.data
            .downcast_ref::<HashMap<Entity, T>>()
            .map(|map| map.keys().copied().collect())
            .unwrap_or_default()
    }
}

// World manages all entities and components
pub struct World {
    next_entity_id: u64,
    entities: Vec<Entity>,
    components: HashMap<TypeId, ComponentStorage>,
    script_components: HashMap<TypeId, Box<dyn ScriptStorageUpdater>>,
}

impl World {
    pub fn new() -> Self {
        Self {
            next_entity_id: 0,
            entities: Vec::new(),
            components: HashMap::new(),
            script_components: HashMap::new(),
        }
    }

    // Create a new entity
    pub fn create_entity(&mut self) -> Entity {
        let entity = Entity(self.next_entity_id);
        self.next_entity_id += 1;
        self.entities.push(entity);
        entity
    }

    // Add a component to an entity
    pub fn add_component<T: Component + 'static>(&mut self, entity: Entity, component: T) {
        let type_id = TypeId::of::<T>();
        self.components
            .entry(type_id)
            .or_insert_with(ComponentStorage::new::<T>)
            .insert(entity, component);
    }

    // Get a component from an entity
    pub fn get_component<T: Component + 'static>(&self, entity: Entity) -> Option<&T> {
        let type_id = TypeId::of::<T>();
        self.components
            .get(&type_id)
            .and_then(|storage| storage.get::<T>(entity))
    }

    // Get a mutable component from an entity
    pub fn get_component_mut<T: Component + 'static>(&mut self, entity: Entity) -> Option<&mut T> {
        let type_id = TypeId::of::<T>();
        self.components
            .get_mut(&type_id)
            .and_then(|storage| storage.get_mut::<T>(entity))
    }

    // Remove a component from an entity
    pub fn remove_component<T: Component + 'static>(&mut self, entity: Entity) -> Option<T> {
        let type_id = TypeId::of::<T>();
        self.components
            .get_mut(&type_id)
            .and_then(|storage| storage.remove::<T>(entity))
    }

    // Delete an entity and all its components
    #[allow(dead_code)]
    pub fn delete_entity(&mut self, entity: Entity) {
        self.entities.retain(|&e| e != entity);
        // Note: In a complete implementation, you'd track which components each entity has
        // and remove them from their respective storages
    }

    // Query for entities with specific components
    pub fn query<T: Component + 'static>(&self) -> Vec<(Entity, &T)> {
        let type_id = TypeId::of::<T>();
        if let Some(storage) = self.components.get(&type_id) {
            storage
                .entities::<T>()
                .into_iter()
                .filter_map(|entity| {
                    storage
                        .get::<T>(entity)
                        .map(|component| (entity, component))
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    // Query for entities with two components
    pub fn query2<T1: Component + 'static, T2: Component + 'static>(
        &self,
    ) -> Vec<(Entity, &T1, &T2)> {
        let type_id1 = TypeId::of::<T1>();
        let type_id2 = TypeId::of::<T2>();

        if let (Some(storage1), Some(storage2)) = (
            self.components.get(&type_id1),
            self.components.get(&type_id2),
        ) {
            let entities1 = storage1.entities::<T1>();
            let entities2 = storage2.entities::<T2>();

            entities1
                .iter()
                .filter(|e| entities2.contains(e))
                .filter_map(|&entity| {
                    if let (Some(c1), Some(c2)) =
                        (storage1.get::<T1>(entity), storage2.get::<T2>(entity))
                    {
                        Some((entity, c1, c2))
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    // Mutable query for entities with specific components
    #[allow(dead_code)]
    pub fn query_mut<T: Component + 'static>(&mut self) -> Vec<(Entity, &mut T)> {
        let type_id = TypeId::of::<T>();
        if let Some(storage) = self.components.get_mut(&type_id) {
            let entities = storage.entities::<T>();
            // We need to handle this carefully due to borrow checker
            // This is a simplified version - in production you'd use unsafe or interior mutability
            let storage_ptr = storage as *mut ComponentStorage;
            entities
                .into_iter()
                .filter_map(|entity| unsafe {
                    (*storage_ptr)
                        .get_mut::<T>(entity)
                        .map(|component| (entity, component))
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    // Add a script component to an entity
    pub fn add_script_component<T: ScriptComponent + 'static>(
        &mut self,
        entity: Entity,
        component: T,
    ) {
        let type_id = TypeId::of::<T>();
        let storage = self
            .script_components
            .entry(type_id)
            .or_insert_with(|| Box::new(TypedScriptStorage::<T>::new()));

        // Downcast to the concrete type to insert
        if let Some(typed_storage) = storage.as_any_mut().downcast_mut::<TypedScriptStorage<T>>() {
            typed_storage.insert(entity, component);
        }
    }

    // Update all script components for all entities
    pub fn update_scripts(&mut self) {
        // Create update context for collecting mutations
        let mut ctx = UpdateContext::new();

        // We need to iterate over script components and update them
        // This requires some unsafe pointer magic due to the borrow checker
        let script_type_ids: Vec<TypeId> = self.script_components.keys().copied().collect();

        for type_id in script_type_ids {
            // Get immutable reference to world for script updates
            // and mutable reference to the specific storage
            let world_ptr = self as *const World;

            if let Some(storage) = self.script_components.get_mut(&type_id) {
                unsafe {
                    // Pass immutable world reference to update function
                    storage.update_all(&*world_ptr, &mut ctx);
                }
            }
        }

        // Apply all collected mutations
        for (entity, pos) in ctx.position_updates {
            if let Some(existing_pos) = self.get_component_mut::<Position>(entity) {
                existing_pos.x = pos.x;
                existing_pos.y = pos.y;
            }
        }
    }
}

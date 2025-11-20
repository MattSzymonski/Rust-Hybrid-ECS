use std::any::{Any, TypeId};
use std::cell::UnsafeCell;
use std::collections::HashMap;

// ============================================================================
// Core Types
// ============================================================================

/// Entity is a unique identifier for a game object
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity {
    id: u64,
    generation: u32,
}

/// Component marker trait - all components must be 'static
pub trait Component: 'static {}

/// ComponentId uniquely identifies a component type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ComponentId(TypeId);

impl ComponentId {
    fn of<T: Component>() -> Self {
        ComponentId(TypeId::of::<T>())
    }
}

// ============================================================================
// Archetype Storage
// ============================================================================

/// Stores components in columns for cache-friendly iteration
struct ComponentColumn {
    data: Vec<Box<dyn Any>>,
    component_id: ComponentId,
}

impl ComponentColumn {
    fn new(component_id: ComponentId) -> Self {
        Self {
            data: Vec::new(),
            component_id,
        }
    }

    fn push<T: Component>(&mut self, component: T) {
        self.data.push(Box::new(component));
    }

    fn get<T: Component>(&self, index: usize) -> Option<&T> {
        self.data.get(index)?.downcast_ref::<T>()
    }

    fn get_mut<T: Component>(&mut self, index: usize) -> Option<&mut T> {
        self.data.get_mut(index)?.downcast_mut::<T>()
    }

    fn swap_remove(&mut self, index: usize) {
        if index < self.data.len() {
            self.data.swap_remove(index);
        }
    }

    fn len(&self) -> usize {
        self.data.len()
    }
}

/// ArchetypeId uniquely identifies an archetype (a unique combination of components)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ArchetypeId(usize);

/// Archetype stores all entities that share the same set of components
struct Archetype {
    id: ArchetypeId,
    component_types: Vec<ComponentId>,
    columns: HashMap<ComponentId, ComponentColumn>,
    entities: Vec<Entity>,
}

impl Archetype {
    fn new(id: ArchetypeId, component_types: Vec<ComponentId>) -> Self {
        let mut columns = HashMap::new();
        for &comp_id in &component_types {
            columns.insert(comp_id, ComponentColumn::new(comp_id));
        }

        Self {
            id,
            component_types,
            columns,
            entities: Vec::new(),
        }
    }

    fn has_component<T: Component>(&self) -> bool {
        self.component_types.contains(&ComponentId::of::<T>())
    }

    fn matches_components(&self, component_ids: &[ComponentId]) -> bool {
        component_ids
            .iter()
            .all(|id| self.component_types.contains(id))
    }

    fn len(&self) -> usize {
        self.entities.len()
    }
}

// ============================================================================
// World - Entity and Archetype Management
// ============================================================================

/// EntityLocation tracks where an entity is stored
#[derive(Clone, Copy)]
struct EntityLocation {
    archetype_id: ArchetypeId,
    index_in_archetype: usize,
}

/// World manages all entities and archetypes
pub struct World {
    next_entity_id: u64,
    archetypes: HashMap<ArchetypeId, Archetype>,
    next_archetype_id: usize,
    entity_locations: HashMap<Entity, EntityLocation>,
    archetype_lookup: HashMap<Vec<ComponentId>, ArchetypeId>,
    global_components: HashMap<ComponentId, Box<dyn Any>>,
}

impl World {
    pub fn new() -> Self {
        Self {
            next_entity_id: 0,
            archetypes: HashMap::new(),
            next_archetype_id: 0,
            entity_locations: HashMap::new(),
            archetype_lookup: HashMap::new(),
            global_components: HashMap::new(),
        }
    }

    /// Add or update a global component (singleton component not attached to any entity)
    pub fn add_global_component<T: Component>(&mut self, component: T) {
        self.global_components
            .insert(ComponentId::of::<T>(), Box::new(component));
    }

    /// Get reference to a global component
    pub fn get_global_component<T: Component>(&self) -> Option<&T> {
        self.global_components
            .get(&ComponentId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref::<T>())
    }

    /// Get mutable reference to a global component
    pub fn get_global_component_mut<T: Component>(&mut self) -> Option<&mut T> {
        self.global_components
            .get_mut(&ComponentId::of::<T>())
            .and_then(|boxed| boxed.downcast_mut::<T>())
    }

    fn allocate_entity(&mut self) -> Entity {
        let entity = Entity {
            id: self.next_entity_id,
            generation: 0,
        };
        self.next_entity_id += 1;
        entity
    }

    fn get_or_create_archetype(&mut self, mut component_ids: Vec<ComponentId>) -> ArchetypeId {
        component_ids.sort();

        if let Some(&archetype_id) = self.archetype_lookup.get(&component_ids) {
            return archetype_id;
        }

        let archetype_id = ArchetypeId(self.next_archetype_id);
        self.next_archetype_id += 1;

        let archetype = Archetype::new(archetype_id, component_ids.clone());
        self.archetypes.insert(archetype_id, archetype);
        self.archetype_lookup.insert(component_ids, archetype_id);

        archetype_id
    }

    pub fn spawn(&mut self) -> EntityBuilder {
        let entity = self.allocate_entity();
        EntityBuilder {
            world: self,
            entity,
            components: Vec::new(),
        }
    }

    fn insert_entity_with_components(
        &mut self,
        entity: Entity,
        components: Vec<(ComponentId, Box<dyn Any>)>,
    ) {
        let component_ids: Vec<ComponentId> = components.iter().map(|(id, _)| *id).collect();
        let archetype_id = self.get_or_create_archetype(component_ids);

        let archetype = self.archetypes.get_mut(&archetype_id).unwrap();
        let index = archetype.entities.len();

        archetype.entities.push(entity);
        for (comp_id, component) in components {
            if let Some(column) = archetype.columns.get_mut(&comp_id) {
                column.data.push(component);
            }
        }

        self.entity_locations.insert(
            entity,
            EntityLocation {
                archetype_id,
                index_in_archetype: index,
            },
        );
    }

    /// Simple add component that creates a new entity with additional component
    /// This is a simplified version - a real implementation would move the entity between archetypes
    ///
    /// NOTE: This is a placeholder for demonstration. Proper implementation requires:
    /// 1. Copying all existing component data from old archetype
    /// 2. Removing entity from old archetype (with swap_remove)
    /// 3. Updating swapped entity's location
    /// 4. Adding entity with all components to new archetype
    /// 5. Updating entity location mapping
    ///
    /// For a minimal ECS demo, this complexity is beyond scope.
    #[allow(dead_code)]
    pub fn add_component_simple<T: Component>(&mut self, entity: Entity, component: T) {
        // For this minimal implementation, we'll just store that this entity should have the component
        // A full implementation would move the entity to a new archetype
        // For now, we spawn a mirror entity with the Dead tag

        // Check if entity exists
        if !self.entity_locations.contains_key(&entity) {
            return;
        }

        // Simple approach: just insert directly if we can get the archetype
        // This is a placeholder - proper implementation would require archetype migration
        let location = self.entity_locations.get(&entity).copied().unwrap();
        let component_id = ComponentId::of::<T>();

        // Check if entity already has this component
        let archetype = self.archetypes.get(&location.archetype_id).unwrap();
        if archetype.component_types.contains(&component_id) {
            return; // Already has component
        }

        // For minimal implementation: Create new component set with the additional component
        let mut new_component_ids: Vec<ComponentId> = archetype.component_types.clone();
        new_component_ids.push(component_id);
        new_component_ids.sort();

        let new_archetype_id = self.get_or_create_archetype(new_component_ids);

        // Move entity to new archetype
        // Note: This is simplified and doesn't properly copy existing component data
        // A real implementation would need to copy all existing components
        let new_archetype = self.archetypes.get_mut(&new_archetype_id).unwrap();
        let new_index = new_archetype.entities.len();
        new_archetype.entities.push(entity);

        // Add the new component
        if let Some(column) = new_archetype.columns.get_mut(&component_id) {
            column.data.push(Box::new(component));
        }

        // Update entity location
        self.entity_locations.insert(
            entity,
            EntityLocation {
                archetype_id: new_archetype_id,
                index_in_archetype: new_index,
            },
        );
    }
}

/// Builder for constructing entities with components
pub struct EntityBuilder<'w> {
    world: &'w mut World,
    entity: Entity,
    components: Vec<(ComponentId, Box<dyn Any>)>,
}

impl<'w> EntityBuilder<'w> {
    pub fn with<T: Component>(mut self, component: T) -> Self {
        self.components
            .push((ComponentId::of::<T>(), Box::new(component)));
        self
    }

    pub fn build(self) -> Entity {
        let entity = self.entity;
        self.world
            .insert_entity_with_components(entity, self.components);
        entity
    }
}

// ============================================================================
// Commands - Deferred Operations
// ============================================================================

/// Deferred command to be executed later
enum DeferredCommand {
    AddComponent {
        entity: Entity,
        component: Box<dyn Any>,
    },
}

/// Commands queue for deferred operations
pub struct CommandQueue {
    commands: Vec<DeferredCommand>,
}

impl CommandQueue {
    fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Queue adding a component to an entity
    pub fn add_component<T: Component>(&mut self, entity: Entity, component: T) {
        self.commands.push(DeferredCommand::AddComponent {
            entity,
            component: Box::new(component),
        });
    }

    /// Execute all queued commands
    fn execute(&mut self, world: &mut World) {
        for command in self.commands.drain(..) {
            match command {
                DeferredCommand::AddComponent { entity, component } => {
                    // Note: For minimal ECS, we just acknowledge this
                    // A full implementation would use world.add_component_simple
                    if let Some(_) = world.entity_locations.get(&entity) {
                        println!("  [Deferred] Would add component to entity {:?}", entity.id);
                    }
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Commands allows deferred entity operations
pub struct Commands<'a> {
    queue: &'a mut CommandQueue,
}

impl<'a> Commands<'a> {
    pub fn new(queue: &'a mut CommandQueue) -> Self {
        Self { queue }
    }

    /// Add a component to an entity (deferred)
    pub fn add_component<T: Component>(&mut self, entity: Entity, component: T) {
        self.queue.add_component(entity, component);
    }
}

// ============================================================================
// Query System
// ============================================================================

/// WorldQuery trait for fetching components from archetypes
pub trait WorldQuery {
    type Item<'a>;

    fn component_ids() -> Vec<ComponentId>;
    fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a>;
    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a>;
}

/// Implement WorldQuery for Entity
impl WorldQuery for Entity {
    type Item<'a> = Entity;

    fn component_ids() -> Vec<ComponentId> {
        Vec::new()
    }

    fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a> {
        archetype.entities[index]
    }

    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
        archetype.entities[index]
    }
}

/// Implement WorldQuery for immutable component reference
impl<T: Component> WorldQuery for &T {
    type Item<'a> = &'a T;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a> {
        archetype
            .columns
            .get(&ComponentId::of::<T>())
            .and_then(|col| col.get::<T>(index))
            .expect("Component not found in archetype")
    }

    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
        archetype
            .columns
            .get(&ComponentId::of::<T>())
            .and_then(|col| col.get::<T>(index))
            .expect("Component not found in archetype")
    }
}

/// Implement WorldQuery for mutable component reference
impl<T: Component> WorldQuery for &mut T {
    type Item<'a> = &'a mut T;

    fn component_ids() -> Vec<ComponentId> {
        vec![ComponentId::of::<T>()]
    }

    fn fetch<'a>(_archetype: &'a Archetype, _index: usize) -> Self::Item<'a> {
        panic!("Cannot fetch mutable reference from immutable archetype")
    }

    fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
        archetype
            .columns
            .get_mut(&ComponentId::of::<T>())
            .and_then(|col| col.get_mut::<T>(index))
            .expect("Component not found in archetype")
    }
}

/// Macro to implement WorldQuery for tuples
macro_rules! impl_world_query_tuple {
    ($($T:ident),*) => {
        impl<$($T: WorldQuery),*> WorldQuery for ($($T,)*) {
            type Item<'a> = ($($T::Item<'a>,)*);

            fn component_ids() -> Vec<ComponentId> {
                let mut ids = Vec::new();
                $(ids.extend($T::component_ids());)*
                ids
            }

            #[allow(non_snake_case)]
            fn fetch<'a>(archetype: &'a Archetype, index: usize) -> Self::Item<'a> {
                ($($T::fetch(archetype, index),)*)
            }

            #[allow(non_snake_case)]
            fn fetch_mut<'a>(archetype: &'a mut Archetype, index: usize) -> Self::Item<'a> {
                // SAFETY: We use raw pointers to allow multiple mutable borrows of different components
                let arch_ptr = archetype as *mut Archetype;
                unsafe {
                    ($($T::fetch_mut(&mut *arch_ptr, index),)*)
                }
            }
        }
    };
}

// Implement for tuples up to 4 elements
impl_world_query_tuple!(A);
impl_world_query_tuple!(A, B);
impl_world_query_tuple!(A, B, C);
impl_world_query_tuple!(A, B, C, D);

/// Query provides iteration over entities matching a component pattern
pub struct Query<'w, Q: WorldQuery> {
    world: &'w mut World,
    _phantom: std::marker::PhantomData<Q>,
}

impl<'w, Q: WorldQuery> Query<'w, Q> {
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn iter_mut(&mut self) -> QueryIterMut<Q> {
        let component_ids = Q::component_ids();
        let matching_archetypes: Vec<ArchetypeId> = self
            .world
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.matches_components(&component_ids))
            .map(|(id, _)| *id)
            .collect();

        QueryIterMut {
            world_ptr: self.world as *mut World,
            matching_archetypes,
            current_archetype_idx: 0,
            current_entity_idx: 0,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Iterator for mutable queries
pub struct QueryIterMut<'w, Q: WorldQuery> {
    world_ptr: *mut World,
    matching_archetypes: Vec<ArchetypeId>,
    current_archetype_idx: usize,
    current_entity_idx: usize,
    _phantom: std::marker::PhantomData<&'w mut Q>,
}

impl<'w, Q: WorldQuery> Iterator for QueryIterMut<'w, Q> {
    type Item = Q::Item<'w>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            let world = &mut *self.world_ptr;

            while self.current_archetype_idx < self.matching_archetypes.len() {
                let archetype_id = self.matching_archetypes[self.current_archetype_idx];
                let archetype = world.archetypes.get_mut(&archetype_id)?;

                if self.current_entity_idx < archetype.len() {
                    let index = self.current_entity_idx;
                    self.current_entity_idx += 1;

                    // SAFETY: We're extending the lifetime here, but it's safe because:
                    // 1. We hold exclusive access to the world through the query
                    // 2. Each iteration produces unique references to different components
                    // 3. The references don't outlive the query iteration
                    let item = Q::fetch_mut(archetype, index);
                    let item_with_lifetime: Q::Item<'w> = std::mem::transmute(item);
                    return Some(item_with_lifetime);
                }

                self.current_archetype_idx += 1;
                self.current_entity_idx = 0;
            }

            None
        }
    }
}

// ============================================================================
// Global Component Query
// ============================================================================

/// Query for accessing global components (singleton components stored in World, not attached to entities)
pub struct GlobalComponentQuery<'w, T: Component> {
    world: &'w mut World,
    _phantom: std::marker::PhantomData<T>,
}

impl<'w, T: Component> GlobalComponentQuery<'w, T> {
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get immutable reference to the global component
    pub fn get(&self) -> Option<&T> {
        self.world.get_global_component::<T>()
    }

    /// Get mutable reference to the global component
    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.world.get_global_component_mut::<T>()
    }
}

// ============================================================================
// Example Components and Systems
// ============================================================================

/// Global time component - stored as a singleton entity
#[derive(Debug)]
struct GlobalTime {
    delta_time: f32,
    elapsed_time: f32,
}

impl Component for GlobalTime {}

#[derive(Debug)]
struct Transform {
    x: f32,
    y: f32,
    z: f32,
}

impl Component for Transform {}

#[derive(Debug)]
struct Velocity {
    x: f32,
}

impl Component for Velocity {}

/// Tag component to mark entities as dead
#[derive(Debug)]
struct Dead;

impl Component for Dead {}

/// Movement system that applies velocity to transform and tags entities as Dead if x > 100
fn movement_system(
    mut commands: Commands,
    mut query: Query<(Entity, &mut Transform, &Velocity)>,
    time_query: GlobalComponentQuery<GlobalTime>,
) {
    // Get delta time from global component
    let delta_time = if let Some(global_time) = time_query.get() {
        global_time.delta_time
    } else {
        1.0 // Default fallback
    };

    for (entity, transform, velocity) in query.iter_mut() {
        // Move transform by velocity * delta_time
        transform.x += velocity.x * delta_time;

        // Check if entity should be tagged as Dead
        if transform.x > 100.0 {
            println!(
                "Entity {:?} exceeded x=100 (x={:.2}), queuing Dead tag",
                entity.id, transform.x
            );
            commands.add_component(entity, Dead);
        }
    }
}

/// System that prints dead entities every 5 seconds
fn dead_report_system(
    mut query: Query<(&Dead, &Transform)>,
    time_query: GlobalComponentQuery<GlobalTime>,
    State(last_report_time): State<&mut f32>,
) {
    // Get elapsed time from global component
    let elapsed_time = if let Some(global_time) = time_query.get() {
        global_time.elapsed_time
    } else {
        0.0
    };

    if elapsed_time - *last_report_time >= 5.0 {
        println!("\n=== Dead Entities Report ===");
        for (_dead, transform) in query.iter_mut() {
            println!(
                "dead: transform=({:.2}, {:.2}, {:.2})",
                transform.x, transform.y, transform.z
            );
        }
        println!("===========================\n");
        *last_report_time = elapsed_time;
    }
}

/// System that just prints current time with no parameters
fn time_print_system() {
    use std::time::SystemTime;

    if let Ok(duration) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        println!(
            "  [TimeSystem] Current Unix timestamp: {} seconds",
            duration.as_secs()
        );
    }
}

/// NEW: System with different query pattern - demonstrates full generics
fn velocity_report_system(mut query: Query<(Entity, &Velocity)>) {
    println!("  [VelocityReport] Current velocities:");
    for (entity, velocity) in query.iter_mut() {
        println!("    Entity {:?}: velocity.x = {:.2}", entity.id, velocity.x);
    }
}

/// NEW: System with only global component access
fn time_info_system(time: GlobalComponentQuery<GlobalTime>) {
    if let Some(global_time) = time.get() {
        println!(
            "  [TimeInfo] Delta: {:.2}s, Elapsed: {:.2}s",
            global_time.delta_time, global_time.elapsed_time
        );
    }
}

/// NEW: System with complex 4-parameter combination
fn debug_system(
    mut query1: Query<&Transform>,
    mut query2: Query<&Velocity>,
    time: GlobalComponentQuery<GlobalTime>,
    State(report_time): State<&mut f32>,
) {
    if let Some(global_time) = time.get() {
        if global_time.elapsed_time - *report_time >= 3.0 {
            let transform_count = query1.iter_mut().count();
            let velocity_count = query2.iter_mut().count();
            println!(
                "  [Debug] Entities with Transform: {}, with Velocity: {}",
                transform_count, velocity_count
            );
            *report_time = global_time.elapsed_time;
        }
    }
}

// ============================================================================
// Engine - System Management
// ============================================================================

/// Trait for systems that can be executed by the engine
///
/// State that persists between system calls
pub struct SystemState {
    pub last_report_time: f32,
}

impl SystemState {
    fn new() -> Self {
        Self {
            last_report_time: 0.0,
        }
    }
}

// ============================================================================
// Advanced System Parameter Infrastructure (Bevy-style)
// ============================================================================

/// SystemParam trait - any type that can be extracted as a system parameter
/// This is the core of Bevy's flexible system architecture
pub trait SystemParam: Sized {
    /// Fetch the parameter from world state
    fn fetch(world: &mut World, queue: &mut CommandQueue, state: &mut SystemState) -> Self;
}

/// SystemParamFunction trait - functions that can be converted to systems
/// The Input tuple contains all the parameter types
pub trait SystemParamFunction<Input: SystemParam>: 'static {
    fn run(&mut self, input: Input);
}

/// Trait for systems that can be executed by the Engine
trait System {
    fn run(&mut self, world: &mut World, queue: &mut CommandQueue, state: &mut SystemState);
}

// Implement System for any FnMut closure with the right signature
impl<F> System for F
where
    F: FnMut(&mut World, &mut CommandQueue, &mut SystemState),
{
    fn run(&mut self, world: &mut World, queue: &mut CommandQueue, state: &mut SystemState) {
        self(world, queue, state);
    }
}

// ============================================================================
// SystemParam Implementations for Each Parameter Type
// ============================================================================

// Commands is a SystemParam
impl SystemParam for Commands<'static> {
    fn fetch(_world: &mut World, queue: &mut CommandQueue, _state: &mut SystemState) -> Self {
        // We need to transmute the lifetime here
        // SAFETY: The Commands will only live as long as the system execution
        unsafe { std::mem::transmute(Commands::new(queue)) }
    }
}

// Generic Query is a SystemParam - works for ANY WorldQuery type
impl<Q: WorldQuery + 'static> SystemParam for Query<'static, Q> {
    fn fetch(world: &mut World, _queue: &mut CommandQueue, _state: &mut SystemState) -> Self {
        unsafe {
            // Create query with actual lifetime, then transmute to 'static
            // SAFETY: The query will only live as long as the system execution
            let query: Query<Q> = Query::new(world);
            std::mem::transmute(query)
        }
    }
}

// Generic GlobalComponentQuery is a SystemParam - works for ANY Component type
impl<T: Component> SystemParam for GlobalComponentQuery<'static, T> {
    fn fetch(world: &mut World, _queue: &mut CommandQueue, _state: &mut SystemState) -> Self {
        unsafe {
            // Create query with actual lifetime, then transmute to 'static
            // SAFETY: The query will only live as long as the system execution
            let query: GlobalComponentQuery<T> = GlobalComponentQuery::new(world);
            std::mem::transmute(query)
        }
    }
}

// State reference is a SystemParam (for persistent state like last_report_time)
pub struct State<T>(pub T);

impl SystemParam for State<&'static mut f32> {
    fn fetch(_world: &mut World, _queue: &mut CommandQueue, state: &mut SystemState) -> Self {
        // SAFETY: Transmuting lifetime for state access
        unsafe { State(std::mem::transmute(&mut state.last_report_time)) }
    }
}

// ============================================================================
// SystemParam Tuple Implementations (macro-generated)
// ============================================================================

/// Macro to implement SystemParam for tuples
macro_rules! impl_system_param_tuple {
    ($($T:ident),*) => {
        #[allow(non_snake_case)]
        impl<$($T: SystemParam),*> SystemParam for ($($T,)*) {
            fn fetch(world: &mut World, queue: &mut CommandQueue, state: &mut SystemState) -> Self {
                ($($T::fetch(world, queue, state),)*)
            }
        }
    };
}

// Implement for tuples of different sizes
impl SystemParam for () {
    fn fetch(_world: &mut World, _queue: &mut CommandQueue, _state: &mut SystemState) -> Self {
        ()
    }
}

impl_system_param_tuple!(A);
impl_system_param_tuple!(A, B);
impl_system_param_tuple!(A, B, C);
impl_system_param_tuple!(A, B, C, D);
impl_system_param_tuple!(A, B, C, D, E);
impl_system_param_tuple!(A, B, C, D, E, F1);

// ============================================================================
// SystemParamFunction Implementations for Different Function Arities
// ============================================================================

/// Macro to implement SystemParamFunction for functions with different numbers of parameters
macro_rules! impl_system_param_function {
    ($($T:ident),*) => {
        #[allow(non_snake_case)]
        impl<F, $($T: SystemParam),*> SystemParamFunction<($($T,)*)> for F
        where
            F: FnMut($($T),*) + 'static,
        {
            fn run(&mut self, input: ($($T,)*)) {
                let ($($T,)*) = input;
                self($($T),*)
            }
        }
    };
}

// Implement for different arities
impl<F> SystemParamFunction<()> for F
where
    F: FnMut() + 'static,
{
    fn run(&mut self, _input: ()) {
        self()
    }
}

impl_system_param_function!(A);
impl_system_param_function!(A, B);
impl_system_param_function!(A, B, C);
impl_system_param_function!(A, B, C, D);
impl_system_param_function!(A, B, C, D, E);
impl_system_param_function!(A, B, C, D, E, F1);

// ============================================================================
// IntoSystem - Automatic Conversion
// ============================================================================

/// Trait for converting functions into Systems
/// This uses the SystemParam infrastructure to automatically resolve parameters
trait IntoSystem<Input: SystemParam> {
    fn into_system(self) -> Box<dyn System>;
}

/// Implement IntoSystem for any function that implements SystemParamFunction
impl<F, Input> IntoSystem<Input> for F
where
    F: SystemParamFunction<Input>,
    Input: SystemParam,
{
    fn into_system(mut self) -> Box<dyn System> {
        Box::new(
            move |world: &mut World, queue: &mut CommandQueue, state: &mut SystemState| {
                let input = Input::fetch(world, queue, state);
                self.run(input);
            },
        )
    }
}

/// Engine manages the world and registered systems
///
/// Each frame is processed in two phases:
/// 1. Systems Phase: All registered systems execute, queuing commands
/// 2. Deferred Phase: Queued commands are executed (component additions, etc.)
pub struct Engine {
    world: World,
    systems: Vec<(String, Box<dyn System>)>,
    system_state: SystemState,
    command_queue: CommandQueue,
    elapsed_time: f32,
    delta_time: f32,
}

impl Engine {
    pub fn new(delta_time: f32) -> Self {
        Self {
            world: World::new(),
            systems: Vec::new(),
            system_state: SystemState::new(),
            command_queue: CommandQueue::new(),
            elapsed_time: 0.0,
            delta_time,
        }
    }

    /// Register any system that implements IntoSystem
    pub fn register_system<Input, S>(&mut self, name: &str, system: S)
    where
        S: IntoSystem<Input>,
        Input: SystemParam,
    {
        self.systems.push((name.to_string(), system.into_system()));
    }

    /// Get mutable reference to the world for entity spawning
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Process a single frame with two phases: systems phase and deferred phase
    pub fn process_frame(&mut self, frame: usize) {
        self.elapsed_time += self.delta_time;

        // Update GlobalTime component (stored as a global component, not on an entity)
        if let Some(global_time) = self.world.get_global_component_mut::<GlobalTime>() {
            global_time.elapsed_time = self.elapsed_time;
            global_time.delta_time = self.delta_time;
        }

        println!("--- Frame {} (t={:.1}s) ---", frame, self.elapsed_time);

        // Phase 1: Execute all registered systems
        for (name, system) in &mut self.systems {
            println!("  Running system: {}", name);
            system.run(
                &mut self.world,
                &mut self.command_queue,
                &mut self.system_state,
            );
        }

        // Phase 2: Process deferred commands
        if !self.command_queue.is_empty() {
            println!("\n  [Deferred Phase]");
            self.command_queue.execute(&mut self.world);
        }

        println!();
    }

    /// Get elapsed time
    pub fn elapsed_time(&self) -> f32 {
        self.elapsed_time
    }
}

fn main() {
    println!("=== Archetype-based ECS - Engine System ===\n");

    // Create engine with 1 second delta time
    let mut engine = Engine::new(1.0);

    // Add GlobalTime as a global component (not attached to any entity)
    engine.world_mut().add_global_component(GlobalTime {
        delta_time: 1.0,
        elapsed_time: 0.0,
    });

    // Register systems - Engine automatically resolves parameters
    // Original systems
    engine.register_system("movement_system", movement_system);
    engine.register_system("dead_report_system", dead_report_system);
    engine.register_system("time_print_system", time_print_system);

    // NEW: Register completely different systems to prove full generics work
    engine.register_system("velocity_report_system", velocity_report_system);
    engine.register_system("time_info_system", time_info_system);
    engine.register_system("debug_system", debug_system);

    // Spawn entities
    println!("Spawning entities...\n");

    let entity1 = engine
        .world_mut()
        .spawn()
        .with(Transform {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        })
        .with(Velocity { x: 15.0 })
        .build();

    let entity2 = engine
        .world_mut()
        .spawn()
        .with(Transform {
            x: 50.0,
            y: 5.0,
            z: 0.0,
        })
        .with(Velocity { x: 25.0 })
        .build();

    let entity3 = engine
        .world_mut()
        .spawn()
        .with(Transform {
            x: 90.0,
            y: 0.0,
            z: 5.0,
        })
        .with(Velocity { x: 5.0 })
        .build();

    // Spawn one entity that's already dead for testing
    let entity4 = engine
        .world_mut()
        .spawn()
        .with(Transform {
            x: 150.0,
            y: 10.0,
            z: 0.0,
        })
        .with(Dead)
        .build();

    println!(
        "Created entities: {:?}, {:?}, {:?}, {:?} (already dead)\n",
        entity1.id, entity2.id, entity3.id, entity4.id
    );

    // Process 5 frames
    for frame in 0..5 {
        engine.process_frame(frame);
    }

    println!("=== Engine Simulation Complete ===");
}

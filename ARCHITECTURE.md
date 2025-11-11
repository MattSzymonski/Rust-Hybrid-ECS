# Architecture Documentation

## The Core Problem

When building game engines, there's a fundamental tradeoff between **developer ergonomics** and **performance**:

### Unity's Approach (OOP)
```csharp
// Natural and intuitive
GameObject player = GameObject.Instantiate();
player.AddComponent<Transform>();
player.GetComponent<Transform>().position = Vector3.zero;
```

**Pros:**
- Extremely intuitive for developers
- Immediate feedback - changes happen instantly
- No mental overhead about frame delays

**Cons:**
- Single-threaded by design
- Hard to parallelize systems
- Cache-unfriendly memory layout
- Object-oriented overhead

### Bevy's Approach (Pure ECS)
```rust
// High performance but less intuitive
commands.spawn().insert_bundle((
    Transform::default(),
    Velocity::default(),
));
// Entity not accessible until next frame!
```

**Pros:**
- Excellent performance
- Automatic parallelization
- Cache-friendly data layout
- Zero-cost abstractions

**Cons:**
- Learning curve for OOP developers
- Deferred operations cause "inconsistent state"
- Entity created but not accessible until later
- Less intuitive mental model

## The Hybrid Solution

This project implements a **wrapper-based hybrid architecture** that provides Unity-like ergonomics while maintaining ECS performance characteristics.

### Architecture Layers

```
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃  Layer 1: GameObject API (High-Level)       ┃  ← What game developers see
┃  - Unity-like interface                     ┃
┃  - Intuitive method chaining                ┃
┃  - Automatic synchronization                ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
                    ↓
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃  Layer 2: Command Buffer (Synchronization)  ┃  ← Solves inconsistent state
┃  - Queues deferred operations               ┃
┃  - Batches changes for efficiency           ┃
┃  - Executes at safe synchronization points  ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
                    ↓
┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃  Layer 3: ECS Core (Low-Level)              ┃  ← Performance backend
┃  - Entity/Component storage                 ┃
┃  - Cache-friendly iteration                 ┃
┃  - Parallel system execution ready          ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
```

## Detailed Component Architecture

### 1. GameObject Wrapper (`game_object.rs`)

The GameObject is a **smart handle** that wraps an Entity ID and provides Unity-like methods:

```rust
pub struct GameObject {
    entity: Entity,                              // The underlying ECS entity
    world: Arc<RwLock<World>>,                   // Shared world reference
    command_buffer: Arc<RwLock<CommandBuffer>>,  // Shared command buffer
    is_pending: bool,                            // Track deferred creation
}
```

**Key Features:**
- **Method chaining**: `obj.add_component(A).add_component(B)`
- **Smart references**: `ComponentRef<T>` manages locks automatically
- **Transparent synchronization**: Developers don't think about threading

**Usage:**
```rust
let player = scene.instantiate();
player.add_component(Transform::new(0.0, 0.0, 0.0))
      .add_component(Health::new(100.0));

// Access components with automatic locking
if let Some(health) = player.get_component::<Health>() {
    health.with(|h| println!("HP: {}", h.current));
}
```

### 2. Command Buffer (`command_buffer.rs`)

The command buffer **defers operations** to maintain thread safety and avoid the "inconsistent state" problem:

```rust
pub enum Command {
    CreateEntity(Box<dyn FnOnce(&mut World) -> Entity + Send>),
    AddComponent(Entity, Box<dyn FnOnce(&mut World, Entity) + Send>),
    RemoveComponent(Entity, Box<dyn FnOnce(&mut World, Entity) + Send>),
    DestroyEntity(Entity),
}
```

**How it solves the problem:**
1. During parallel system execution, entities can't be modified
2. Instead, operations are queued in the command buffer
3. At frame boundaries (synchronization points), all commands execute atomically
4. This prevents race conditions and maintains data consistency

**Example:**
```rust
// During a parallel system
let projectile = scene.instantiate();  // Queued
projectile.add_component(Transform::new(0.0, 0.0, 0.0));  // Queued

// At frame boundary
scene.apply_commands();  // All changes applied atomically
```

### 3. ECS Core (`ecs_core.rs`)

The low-level ECS implementation optimized for performance:

```rust
pub struct World {
    next_entity_id: u64,
    storages: HashMap<TypeId, Box<dyn ComponentStorage>>,
    entities: Vec<Entity>,
}
```

**Features:**
- **Type-erased storage**: Components stored in type-specific arrays
- **Cache-friendly iteration**: Query returns iterator over contiguous memory
- **Zero-cost abstractions**: No runtime overhead for type safety

**System iteration:**
```rust
// Iterate efficiently over all Transform components
if let Some(query) = world.query::<Transform>() {
    for (entity, transform) in query {
        // Process each entity
    }
}
```

### 4. System Executor (`systems.rs`)

Manages parallel execution of game systems:

```rust
pub trait System: Send + Sync {
    fn execute(&mut self, world: &mut World, delta_time: f32);
}
```

**Current implementation:**
- Sequential execution (simple and safe)
- Systems process entities in a loop

**Future enhancement:**
- Use Rayon for parallel execution
- Dependency graph to determine which systems can run concurrently
- Automatic detection of component conflicts

## Data Flow

### Frame Execution Cycle

```
┌─────────────────────────────────────────┐
│  1. Input Processing                    │
│     - Update player velocity, etc.      │
└─────────────────────────────────────────┘
                ↓
┌─────────────────────────────────────────┐
│  2. System Execution (Parallel)         │
│     - AI System                          │
│     - Physics System                     │  ← Systems can run in parallel
│     - Movement System                    │     if they don't conflict
│     - Collision System                   │
└─────────────────────────────────────────┘
                ↓
┌─────────────────────────────────────────┐
│  3. Apply Commands (Synchronization)    │
│     - Create queued entities             │
│     - Add/remove queued components       │  ← Thread-safe modification
│     - Destroy queued entities            │
└─────────────────────────────────────────┘
                ↓
┌─────────────────────────────────────────┐
│  4. Render                               │
│     - Draw sprites                       │
│     - Update UI                          │
└─────────────────────────────────────────┘
```

### Example: Creating an Entity During Gameplay

**Unity (Single-threaded):**
```csharp
// Create instantly
GameObject bullet = Instantiate(bulletPrefab);
bullet.transform.position = gunPosition;
// Bullet is immediately accessible
```

**Bevy (Pure ECS):**
```rust
// Deferred creation - confusing for Unity devs
commands.spawn().insert_bundle((
    Transform::from_translation(gun_position),
    Bullet,
));
// Bullet NOT accessible yet - will exist next frame
```

**Our Hybrid:**
```rust
// Looks immediate (Unity-like)
let bullet = scene.instantiate();
bullet.add_component(Transform::new(gun_x, gun_y, 0.0));
bullet.add_component(Bullet);

// Developer writes as if immediate, but it's actually deferred
// The magic: CommandBuffer handles synchronization transparently
scene.apply_commands();  // Called once per frame
```

## Memory Safety

### Thread Safety via RwLock

The World is wrapped in `Arc<RwLock<World>>`:

```rust
// Multiple readers OR one writer
let world_lock = scene.world();

// Read access (multiple systems can read simultaneously)
let world = world_lock.read();
let transform = world.get_component::<Transform>(entity);

// Write access (exclusive)
let mut world = world_lock.write();
world.add_component(entity, velocity);
```

### Avoiding Lifetime Issues

ComponentRef uses closures to manage lifetimes:

```rust
// Instead of returning &T (lifetime issues)
if let Some(health) = player.get_component::<Health>() {
    health.with(|h| {  // Closure ensures lock is held
        println!("HP: {}", h.current);
    });  // Lock released here
}
```

## Performance Characteristics

### Time Complexity

| Operation | Complexity | Notes |
|-----------|-----------|-------|
| Create entity | O(1) | Just increment counter |
| Add component | O(1) | HashMap insert |
| Get component | O(1) | HashMap lookup |
| Query iteration | O(n) | n = entities with component |
| System execution | O(n*m) | n = entities, m = systems |

### Memory Layout

Components of the same type are stored contiguously:

```
Transform Storage:
┌──────┬──────┬──────┬──────┬──────┐
│ Ent1 │ Ent2 │ Ent3 │ Ent4 │ Ent5 │  ← Cache-friendly!
└──────┴──────┴──────┴──────┴──────┘

vs Unity (OOP):
GameObject1 ──→ [Transform, Velocity, Health]  ← Scattered memory
GameObject2 ──→ [Transform, Sprite, Collider]
GameObject3 ──→ [Transform, AI, Velocity]
```

## Extending the System

### Adding a New Component

```rust
#[derive(Debug, Clone)]
pub struct Armor {
    pub defense: f32,
    pub durability: f32,
}

impl Armor {
    pub fn new(defense: f32, durability: f32) -> Self {
        Self { defense, durability }
    }
}

// Use it
player.add_component(Armor::new(50.0, 100.0));
```

### Adding a New System

```rust
struct DamageSystem;

impl System for DamageSystem {
    fn execute(&mut self, world: &mut World, delta_time: f32) {
        let entities: Vec<Entity> = world
            .query::<Health>()
            .map(|iter| iter.map(|(e, _)| e).collect())
            .unwrap_or_default();

        for entity in entities {
            if let (Some(health), Some(armor)) = (
                world.get_component_mut::<Health>(entity),
                world.get_component::<Armor>(entity),
            ) {
                // Apply armor logic
                let damage = 10.0;
                let reduced = damage - armor.defense;
                health.current -= reduced.max(0.0);
            }
        }
    }
}

// Register it
executor.add_system(DamageSystem);
```

## Future Enhancements

### 1. True Parallel Execution with Rayon

```rust
use rayon::prelude::*;

impl SystemExecutor {
    pub fn execute_parallel(&mut self, world: &mut World, delta_time: f32) {
        // Group systems by conflict sets
        let non_conflicting_groups = self.build_dependency_graph();
        
        for group in non_conflicting_groups {
            group.par_iter_mut().for_each(|system| {
                system.execute(world, delta_time);
            });
        }
    }
}
```

### 2. System Dependency Graph

```rust
struct SystemGraph {
    nodes: Vec<Box<dyn System>>,
    edges: Vec<(usize, usize)>,  // Dependency edges
}

impl SystemGraph {
    fn can_run_parallel(&self, sys1: usize, sys2: usize) -> bool {
        // Check if systems access conflicting components
        !self.has_write_conflict(sys1, sys2)
    }
}
```

### 3. Query Multiple Components

```rust
world.query2::<Transform, Velocity>()
    .for_each(|(entity, transform, velocity)| {
        transform.x += velocity.x * delta_time;
    });
```

### 4. Entity Hierarchy

```rust
#[derive(Debug, Clone)]
struct Parent(Entity);

#[derive(Debug, Clone)]
struct Children(Vec<Entity>);

// Usage
gun.add_component(Parent(player.entity()));
```

## Conclusion

This hybrid architecture successfully bridges the gap between Unity's intuitive OOP API and Bevy's high-performance ECS implementation:

✅ **Unity-like ergonomics**: Familiar API for developers transitioning from Unity  
✅ **ECS performance**: Cache-friendly iteration, parallel-ready systems  
✅ **Thread safety**: Command buffer and RwLock prevent race conditions  
✅ **Transparent complexity**: GameObject wrapper hides ECS details  
✅ **Extensible**: Easy to add components and systems  

The key insight: **Good abstractions don't sacrifice performance**. By wrapping ECS with a Unity-like API, we get the best of both worlds.

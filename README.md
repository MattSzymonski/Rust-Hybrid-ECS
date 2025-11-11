# Hybrid ECS Game Engine

A Rust-based game engine architecture that combines the performance benefits of Entity Component System (ECS) with the intuitive, object-oriented API familiar to Unity developers.

## The Problem

When designing game engine architectures, there's typically a tradeoff:

### Unity (Pure OOP)
- ✅ **Intuitive**: `GameObject.Instantiate()`, `AddComponent<T>()`, `GetComponent<T>()`
- ✅ **Immediate**: Changes happen instantly, no frame delay
- ❌ **Single-threaded**: Hard to parallelize
- ❌ **Performance**: Object-oriented overhead

### Bevy/Flecs (Pure ECS)
- ✅ **Performance**: Cache-friendly, data-oriented
- ✅ **Parallel**: Systems can run concurrently
- ❌ **Learning curve**: Different mental model
- ❌ **Deferred operations**: `commands.spawn()` takes effect next frame
- ❌ **Inconsistent state**: Entity created but not accessible until later

## The Solution: Hybrid Architecture

This project implements a **hybrid approach** that wraps a high-performance ECS backend with a Unity-like object-oriented API.

### Architecture Components

```
┌─────────────────────────────────────────┐
│  GameObject API (Unity-like)            │  ← Game Developer Interface
│  - scene.instantiate()                  │
│  - gameObject.add_component(T)          │
│  - gameObject.get_component<T>()        │
└─────────────────────────────────────────┘
                    ↓
┌─────────────────────────────────────────┐
│  Command Buffer                         │  ← Solves inconsistent state
│  - Defers operations                    │
│  - Maintains thread safety              │
│  - Executes at frame boundaries         │
└─────────────────────────────────────────┘
                    ↓
┌─────────────────────────────────────────┐
│  ECS Core (Performance)                 │  ← Backend
│  - Entity/Component storage             │
│  - Parallel system execution            │
│  - Cache-friendly data layout           │
└─────────────────────────────────────────┘
```

## Key Features

### 1. **Unity-like GameObject API**
```rust
// Create entities naturally
let player = scene.instantiate();
player
    .add_component(Transform::new(0.0, 0.0, 0.0))
    .add_component(Velocity::new(1.0, 0.0, 0.0))
    .add_component(Health::new(100.0));

// Access components
if let Some(health) = player.get_component::<Health>() {
    health.with(|h| println!("Health: {}", h.current));
}

// Modify components
if let Some(mut health) = player.get_component_mut::<Health>() {
    health.with(|h| h.current -= 10.0);
}
```

### 2. **Command Buffer for Thread Safety**
Operations are queued and executed at safe synchronization points:
```rust
// During gameplay (possibly from parallel systems)
let projectile = scene.instantiate();  // Queued
projectile.add_component(Transform::new(0.0, 0.0, 0.0));  // Queued

// At frame boundary
scene.apply_commands();  // All changes applied atomically
```

### 3. **ECS Performance Benefits**
Systems iterate over components efficiently:
```rust
impl System for MovementSystem {
    fn execute(&mut self, world: &mut World, delta_time: f32) {
        // Iterate efficiently over all Transform+Velocity entities
        for entity in world.query::<Transform>() {
            // Update transforms based on velocity
        }
    }
}
```

### 4. **Parallel System Execution**
Systems can run in parallel (with proper dependency management):
```rust
let mut executor = SystemExecutor::new();
executor.add_system(MovementSystem);
executor.add_system(PhysicsSystem);
executor.add_system(RenderSystem);

executor.execute(&mut world, delta_time);  // Can parallelize non-conflicting systems
```

## Comparison Table

| Feature          | Unity              | Bevy (Pure ECS)    | This Hybrid        |
|------------------|--------------------|--------------------|---------------------|
| **API Style**    | OOP, Immediate     | ECS, Deferred      | OOP Wrapper + ECS   |
| **Create Entity** | `Instantiate()`   | `commands.spawn()` | `scene.instantiate()` |
| **Add Component** | `AddComponent<T>()` | `.insert(Component)` | `.add_component(T)` |
| **Access Comp**  | `GetComponent<T>()` | `Query<&T>`       | `.get_component<T>()` |
| **Parallel Systems** | ✗ Single-threaded | ✓ Automatic       | ✓ Manual/Rayon      |
| **Intuitive**    | ✓✓ Very natural    | △ Learning curve   | ✓ Natural for Unity |
| **Performance**  | △ Limited          | ✓✓ Excellent       | ✓ Good (ECS backend) |
| **Thread Safety** | △ Manual locking  | ✓✓ Automatic       | ✓ CommandBuffer     |
| **State Sync**   | ✓ Always consistent | △ Frame delay     | ✓ Controlled delay  |

## Project Structure

```
src/
├── main.rs           - Demo application & examples
├── ecs_core.rs       - Core ECS implementation (Entity, World, Components)
├── command_buffer.rs - Deferred operation queue
├── game_object.rs    - Unity-like GameObject wrapper
└── systems.rs        - System execution framework
```

## Running the Demo

```bash
cargo run
```

The demo showcases:
1. Unity-like entity creation
2. Component manipulation
3. System execution across multiple frames
4. Dynamic entity creation during gameplay
5. Entity destruction
6. Comparison with Unity and Bevy approaches

## Usage Example

```rust
use ecs_hybrid::*;

// Create a scene
let scene = Scene::new();

// Create entities with Unity-like API
let player = scene.instantiate();
player
    .add_component(Name::new("Player"))
    .add_component(Transform::new(0.0, 0.0, 0.0))
    .add_component(Health::new(100.0));

// Create systems
let mut executor = SystemExecutor::new();
executor.add_system(MovementSystem);

// Game loop
loop {
    // Execute systems
    {
        let world_lock = scene.world();
        let mut world = world_lock.write();
        executor.execute(&mut world, delta_time);
    }
    
    // Apply deferred commands
    scene.apply_commands();
}
```

## Key Insights

1. **Unity API**: Natural but single-threaded, immediate changes
2. **Bevy ECS**: High performance but deferred operations can be confusing
3. **Hybrid**: Best of both - Unity-like API with ECS performance
4. **Command Buffer**: Solves the "inconsistent state" problem elegantly
5. **GameObject Wrapper**: Hides ECS complexity from game developers

## Future Enhancements

- [ ] Actual parallel system execution with Rayon
- [ ] System dependency graph for automatic parallelization
- [ ] Prefab/template system
- [ ] Entity hierarchy (parent-child relationships)
- [ ] Event system
- [ ] Resource management (singleton components)
- [ ] Query builder with multiple component types
- [ ] Compile-time system conflict detection

## License

This is a demonstration project created for educational purposes.

## Inspiration

This architecture is inspired by discussions about:
- Unity's GameObject model
- Bevy's ECS architecture
- The tradeoffs between OOP and data-oriented design
- Making high-performance systems accessible to developers familiar with Unity

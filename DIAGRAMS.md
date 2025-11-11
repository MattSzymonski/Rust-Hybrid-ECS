# Visual Architecture Diagrams

## Overview: The Three-Layer Architecture

```
╔══════════════════════════════════════════════════════════════════╗
║                     GAME DEVELOPER CODE                          ║
║  let player = scene.instantiate();                               ║
║  player.add_component(Transform::new(0, 0, 0));                  ║
║  player.add_component(Health::new(100));                         ║
╚══════════════════════════════════════════════════════════════════╝
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│                    LAYER 1: GameObject API                       │
│                                                                  │
│  pub struct GameObject {                                         │
│      entity: Entity,                 ← Entity ID                 │
│      world: Arc<RwLock<World>>,      ← Shared ECS world         │
│      command_buffer: Arc<...>,       ← Deferred operations      │
│  }                                                               │
│                                                                  │
│  Methods:                                                        │
│  - add_component<T>(component: T)                                │
│  - get_component<T>() -> Option<ComponentRef<T>>                 │
│  - remove_component<T>()                                         │
│  - destroy()                                                     │
└──────────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│                  LAYER 2: Command Buffer                         │
│                                                                  │
│  pub struct CommandBuffer {                                      │
│      commands: Vec<Command>                                      │
│  }                                                               │
│                                                                  │
│  Queued Commands:                                                │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐ │
│  │ CreateEntity    │  │ AddComponent    │  │ DestroyEntity   │ │
│  └─────────────────┘  └─────────────────┘  └─────────────────┘ │
│                                                                  │
│  execute() → Applies all at frame boundary                       │
└──────────────────────────────────────────────────────────────────┘
                              ↓
┌──────────────────────────────────────────────────────────────────┐
│                     LAYER 3: ECS Core                            │
│                                                                  │
│  pub struct World {                                              │
│      storages: HashMap<TypeId, ComponentStorage>                 │
│      entities: Vec<Entity>                                       │
│  }                                                               │
│                                                                  │
│  Component Storage (Type-erased):                                │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │  Transform   │  │   Velocity   │  │    Health    │          │
│  │  Storage     │  │   Storage    │  │   Storage    │          │
│  └──────────────┘  └──────────────┘  └──────────────┘          │
│  │ E1: (0,0,0)  │  │ E1: (1,0,0)  │  │ E1: 100/100  │          │
│  │ E2: (5,3,0)  │  │ E3: (-1,2,0) │  │ E2: 50/50    │          │
│  │ E3: (10,5,0) │  │              │  │              │          │
│  └──────────────┘  └──────────────┘  └──────────────┘          │
└──────────────────────────────────────────────────────────────────┘
```

## Data Flow: Creating an Entity

```
1. DEVELOPER CODE
   ┌─────────────────────────────────────┐
   │ let enemy = scene.instantiate();    │
   │ enemy.add_component(Transform);     │
   │ enemy.add_component(Health);        │
   └─────────────────────────────────────┘
                   ↓
2. IMMEDIATE MODE (Simple Case)
   ┌─────────────────────────────────────┐
   │ world.create_entity()               │ ← Returns Entity(42)
   │ world.add_component(42, Transform)  │ ← Direct write
   │ world.add_component(42, Health)     │ ← Direct write
   └─────────────────────────────────────┘
                   ↓
3. DEFERRED MODE (During System Execution)
   ┌─────────────────────────────────────┐
   │ cmd_buffer.push(CreateEntity)       │ ← Queued
   │ cmd_buffer.push(AddComponent)       │ ← Queued
   │ cmd_buffer.push(AddComponent)       │ ← Queued
   └─────────────────────────────────────┘
                   ↓
4. FRAME BOUNDARY
   ┌─────────────────────────────────────┐
   │ scene.apply_commands()              │
   │   → cmd_buffer.execute(world)       │
   │     → Creates Entity(42)            │
   │     → Adds Transform to 42          │
   │     → Adds Health to 42             │
   └─────────────────────────────────────┘
                   ↓
5. RESULT
   ┌─────────────────────────────────────┐
   │ Entity 42 now exists in World       │
   │ Can be queried by systems           │
   │ Available to game logic             │
   └─────────────────────────────────────┘
```

## System Execution Flow

```
FRAME START
    ↓
┌──────────────────────────────────────┐
│  INPUT PHASE                         │
│  - Read keyboard/mouse               │
│  - Update player velocity            │
│  - Handle game events                │
└──────────────────────────────────────┘
    ↓
┌──────────────────────────────────────┐
│  SYSTEM EXECUTION PHASE              │
│  ┌────────────────────────────────┐  │
│  │  AI System (Read/Write)        │  │
│  │  - Query: Transform, Enemy     │  │
│  │  - Update: Velocity            │  │
│  └────────────────────────────────┘  │
│           ↓ (Can run in parallel)    │
│  ┌────────────────────────────────┐  │
│  │  Movement System (Read/Write)  │  │
│  │  - Query: Transform, Velocity  │  │
│  │  - Update: Transform           │  │
│  └────────────────────────────────┘  │
│           ↓                           │
│  ┌────────────────────────────────┐  │
│  │  Collision System (Read only)  │  │
│  │  - Query: Transform, Collider  │  │
│  │  - Detect collisions           │  │
│  └────────────────────────────────┘  │
└──────────────────────────────────────┘
    ↓
┌──────────────────────────────────────┐
│  COMMAND BUFFER PHASE                │
│  - Execute all queued commands       │
│  - Create new entities               │
│  - Add/remove components             │
│  - Destroy entities                  │
└──────────────────────────────────────┘
    ↓
┌──────────────────────────────────────┐
│  RENDER PHASE                        │
│  - Query visible entities            │
│  - Submit draw calls                 │
│  - Update UI                         │
└──────────────────────────────────────┘
    ↓
FRAME END → Next Frame
```

## Memory Layout Comparison

### Unity (OOP) - Scattered Memory
```
GameObject 1                GameObject 2                GameObject 3
┌──────────────┐            ┌──────────────┐            ┌──────────────┐
│ Transform    │            │ Transform    │            │ Transform    │
│ x: 0, y: 0   │            │ x: 5, y: 3   │            │ x:10, y: 5   │
├──────────────┤            ├──────────────┤            ├──────────────┤
│ Health       │            │ Sprite       │            │ Velocity     │
│ hp: 100      │            │ tex: "..."   │            │ vx:1, vy:0   │
├──────────────┤            ├──────────────┤            ├──────────────┤
│ Velocity     │            │ Collider     │            │ AI           │
│ vx:1, vy:0   │            │ r: 16        │            │ type: chase  │
└──────────────┘            └──────────────┘            └──────────────┘
   0x1000                       0x2000                      0x3000
                    (Non-contiguous, cache-unfriendly)
```

### This ECS - Contiguous Memory
```
Transform Storage
┌────────┬────────┬────────┬────────┬────────┐
│ E1     │ E2     │ E3     │ E4     │ E5     │
│ (0,0)  │ (5,3)  │ (10,5) │ (15,2) │ (20,8) │
└────────┴────────┴────────┴────────┴────────┘
  0x1000   0x1008   0x1016   0x1024   0x1032
              (Contiguous, cache-friendly!)

Velocity Storage
┌────────┬────────┬────────┐
│ E1     │ E3     │ E5     │
│ (1,0)  │ (-1,2) │ (0,1)  │
└────────┴────────┴────────┘

Health Storage
┌────────┬────────┬────────┐
│ E1     │ E2     │ E4     │
│ 100    │ 50     │ 75     │
└────────┴────────┴────────┘
```

## Component Access Pattern

### Unity Pattern
```
GameObject player = FindObjectOfType<Player>();
Transform t = player.GetComponent<Transform>();
Health h = player.GetComponent<Health>();

// Behind the scenes:
// 1. Find GameObject (slow)
// 2. Look up Transform (hashtable/list search)
// 3. Look up Health (another search)
// All scattered in memory
```

### Our Hybrid Pattern (GameObject API)
```rust
let player = scene.instantiate();

// Looks like Unity...
if let Some(health) = player.get_component::<Health>() {
    health.with(|h| {
        // Use h
    });
}

// But actually:
// 1. Hash lookup by TypeId (O(1))
// 2. Index into contiguous array (cache-friendly)
// 3. Lock managed automatically
```

### Direct ECS Pattern (System)
```rust
// Iterate over ALL entities with Transform
for (entity, transform) in world.query::<Transform>() {
    // Sequential memory access
    // Can vectorize
    // Cache-friendly
}
```

## Threading Model

### Unity (Single-threaded)
```
Thread 1: EVERYTHING
┌─────────────────────────────────┐
│ Update()                        │
│  - Player movement              │
│  - Enemy AI                     │
│  - Physics                      │
│  - Collision                    │
│  - Rendering                    │
└─────────────────────────────────┘
```

### Bevy (Automatic Parallel)
```
Thread 1               Thread 2              Thread 3
┌─────────────┐       ┌─────────────┐       ┌─────────────┐
│ AI System   │       │ Animation   │       │ Particle    │
│ (R: Trans)  │       │ (W: Sprite) │       │ (W: Effect) │
│ (W: Vel)    │       │             │       │             │
└─────────────┘       └─────────────┘       └─────────────┘
        ↓                     ↓                     ↓
    ┌───────────────────────────────────────────────┐
    │         Command Buffer (Sync Point)           │
    └───────────────────────────────────────────────┘
```

### Our Hybrid (Manual Parallel Ready)
```
Thread 1               Thread 2              Thread 3
┌─────────────┐       ┌─────────────┐       ┌─────────────┐
│ System 1    │       │ System 2    │       │ System 3    │
│             │       │             │       │             │
│ (Can run if │       │ (Can run if │       │ (Can run if │
│  no overlap)│       │  no overlap)│       │  no overlap)│
└─────────────┘       └─────────────┘       └─────────────┘
        ↓                     ↓                     ↓
    ┌───────────────────────────────────────────────┐
    │    scene.apply_commands() (Sync Point)        │
    └───────────────────────────────────────────────┘
```

## Query Performance

### Finding All Entities with Component X

Unity (OOP):
```
Time: O(N) where N = ALL GameObjects
Memory: Scattered (bad cache)

foreach (GameObject obj in allObjects) {
    if (obj.HasComponent<X>()) {
        // Found one
    }
}
```

Our ECS:
```
Time: O(M) where M = entities with X
Memory: Contiguous (good cache)

for (entity, x) in world.query::<X>() {
    // Only iterates entities that have X
    // Sequential memory access
}
```

## The Hybrid Advantage

```
                UNITY              THIS HYBRID           BEVY
                  │                     │                 │
API Style         │ ──── Intuitive ──> │                 │
                  │                     │                 │
Performance       │                     │ <── Fast ───── │
                  │                     │                 │
Thread Safety     │                     │ <── Safe ───── │
                  │                     │                 │
Easy to Learn     │ ──── Simple ────> │                 │
                  │                     │                 │
Parallelization   │                     │ <── Ready ──── │
                  │                     │                 │

              Best of Both Worlds!
```

## Summary

The hybrid architecture provides:

✅ **Unity's API** (Familiar GameObject methods)  
✅ **ECS's Performance** (Cache-friendly iteration)  
✅ **Thread Safety** (Command buffer synchronization)  
✅ **Flexibility** (Can use GameObject or direct ECS)  
✅ **Gradual Learning** (Start simple, optimize later)

The key insight: **Layer abstractions to provide multiple interfaces to the same underlying system.**

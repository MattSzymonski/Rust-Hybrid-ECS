# Quick Start Guide

## Installation

```bash
cd d:\Programming\ecs-test
cargo build
```

## Run the Demos

### Basic Demo
```bash
cargo run
```

Shows:
- Unity-like object creation
- Component access
- System execution over multiple frames
- Dynamic entity creation
- Entity destruction
- Comparison table with Unity and Bevy

### Advanced Demo
```bash
cargo run --example advanced
```

Shows:
- Complex game world with multiple entity types
- AI system (enemies chase player)
- Collision detection
- Render system
- Dynamic projectile spawning

## 30-Second Tutorial

### 1. Create a Scene
```rust
use ecs_hybrid::*;

let scene = Scene::new();
```

### 2. Create GameObjects (Unity-like!)
```rust
let player = scene.instantiate();
player
    .add_component(Transform::new(0.0, 0.0, 0.0))
    .add_component(Velocity::new(1.0, 0.0, 0.0))
    .add_component(Health::new(100.0));
```

### 3. Create a System
```rust
struct MySystem;

impl System for MySystem {
    fn execute(&mut self, world: &mut World, delta_time: f32) {
        // Query entities with specific components
        let entities: Vec<Entity> = world
            .query::<Transform>()
            .map(|iter| iter.map(|(e, _)| e).collect())
            .unwrap_or_default();

        // Process each entity
        for entity in entities {
            if let Some(transform) = world.get_component_mut::<Transform>(entity) {
                transform.x += 1.0 * delta_time;
            }
        }
    }
}
```

### 4. Run Your Game Loop
```rust
let mut executor = SystemExecutor::new();
executor.add_system(MySystem);

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

## Common Patterns

### Pattern 1: Creating Entities with Multiple Components

```rust
let enemy = scene.instantiate();
enemy
    .add_component(Name::new("Goblin"))
    .add_component(Transform::new(10.0, 5.0, 0.0))
    .add_component(Health::new(50.0))
    .add_component(Velocity::new(-1.0, 0.0, 0.0));
```

### Pattern 2: Reading Components

```rust
if let Some(health) = player.get_component::<Health>() {
    health.with(|h| {
        println!("Player HP: {}", h.current);
    });
}
```

### Pattern 3: Modifying Components

```rust
if let Some(mut health) = player.get_component_mut::<Health>() {
    health.with(|h| {
        h.current -= 25.0;
        if h.current <= 0.0 {
            println!("Player died!");
        }
    });
}
```

### Pattern 4: Checking if Component Exists

```rust
if player.has_component::<Velocity>() {
    println!("Player can move");
}
```

### Pattern 5: Destroying Entities

```rust
enemy.destroy();
scene.apply_commands();  // Actually removes it
```

### Pattern 6: Spawning During Gameplay

```rust
// Can be called from anywhere, even during system execution
let projectile = scene.instantiate();
projectile
    .add_component(Transform::new(player_x, player_y, 0.0))
    .add_component(Velocity::new(5.0, 0.0, 0.0));

// Will be created at next apply_commands()
scene.apply_commands();
```

## Creating Your Own Components

```rust
#[derive(Debug, Clone)]
pub struct MyComponent {
    pub field1: f32,
    pub field2: String,
}

impl MyComponent {
    pub fn new(field1: f32, field2: impl Into<String>) -> Self {
        Self {
            field1,
            field2: field2.into(),
        }
    }
}

// Use it
entity.add_component(MyComponent::new(42.0, "hello"));
```

## Creating Your Own Systems

```rust
struct GravitySystem;

impl System for GravitySystem {
    fn execute(&mut self, world: &mut World, delta_time: f32) {
        let entities: Vec<Entity> = world
            .query::<Velocity>()
            .map(|iter| iter.map(|(e, _)| e).collect())
            .unwrap_or_default();

        for entity in entities {
            if let Some(velocity) = world.get_component_mut::<Velocity>(entity) {
                velocity.y -= 9.8 * delta_time; // Gravity
            }
        }
    }
}

// Add to executor
executor.add_system(GravitySystem);
```

## Full Example: Simple Game

```rust
use ecs_hybrid::*;

// Define game-specific components
#[derive(Debug, Clone)]
struct Player;

#[derive(Debug, Clone)]
struct Enemy;

#[derive(Debug, Clone)]
struct Bullet;

// Create a bullet spawning system
struct BulletSpawnSystem {
    spawn_timer: f32,
}

impl System for BulletSpawnSystem {
    fn execute(&mut self, world: &mut World, delta_time: f32) {
        self.spawn_timer -= delta_time;
        
        if self.spawn_timer <= 0.0 {
            self.spawn_timer = 1.0; // Spawn every second
            
            // Note: In real implementation, you'd use CommandBuffer here
            // This is simplified for demonstration
        }
    }
}

fn main() {
    let scene = Scene::new();
    
    // Setup player
    let player = scene.instantiate();
    player
        .add_component(Player)
        .add_component(Name::new("Player"))
        .add_component(Transform::new(0.0, 0.0, 0.0))
        .add_component(Velocity::new(0.0, 0.0, 0.0))
        .add_component(Health::new(100.0));
    
    // Setup enemies
    for i in 0..5 {
        let enemy = scene.instantiate();
        enemy
            .add_component(Enemy)
            .add_component(Name::new(format!("Enemy_{}", i)))
            .add_component(Transform::new(i as f32 * 20.0, 10.0, 0.0))
            .add_component(Velocity::new(0.0, -1.0, 0.0))
            .add_component(Health::new(30.0));
    }
    
    scene.apply_commands();
    
    // Setup systems
    let mut executor = SystemExecutor::new();
    executor.add_system(MovementSystem);
    executor.add_system(BulletSpawnSystem { spawn_timer: 1.0 });
    
    // Game loop (simplified - no real rendering)
    for frame in 0..60 {
        {
            let world_lock = scene.world();
            let mut world = world_lock.write();
            executor.execute(&mut world, 1.0 / 60.0);
        }
        
        scene.apply_commands();
        
        if frame % 10 == 0 {
            println!("Frame {}", frame);
        }
    }
}
```

## Tips and Best Practices

### ✅ DO:
- Call `scene.apply_commands()` once per frame at a safe point
- Use method chaining for clarity: `obj.add_component(A).add_component(B)`
- Keep components simple and data-focused
- Make systems process one concern at a time
- Use `with()` closures to access component data

### ❌ DON'T:
- Don't call `apply_commands()` in the middle of system execution
- Don't store references to components across frames
- Don't put complex logic in components (use systems instead)
- Don't forget to call `apply_commands()` after creating entities

## Debugging

### Print all entities with a component:
```rust
let world_lock = scene.world();
let world = world_lock.read();

if let Some(query) = world.query::<Name>() {
    for (entity, name) in query {
        println!("Entity {:?}: {}", entity, name.value);
    }
}
```

### Check how many entities exist:
```rust
let world_lock = scene.world();
let world = world_lock.read();

let count = world.query::<Transform>()
    .map(|iter| iter.count())
    .unwrap_or(0);
    
println!("Total entities: {}", count);
```

## Next Steps

1. Read `ARCHITECTURE.md` for deep dive into the design
2. Explore `examples/advanced.rs` for complex scenarios
3. Try creating your own components and systems
4. Consider adding Rayon for parallel system execution
5. Build a real game!

## Getting Help

- Check the comparison table in the main demo for Unity vs Bevy vs This
- Look at the system implementations for patterns
- Read component definitions for examples
- Review the architecture documentation for design rationale

Happy coding! 🚀

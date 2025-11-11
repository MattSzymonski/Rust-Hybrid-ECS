# Drop-Based Deferred Execution API

## Overview

The hybrid ECS now uses Rust's `Drop` trait to implement a smart deferred execution system. This gives you the best of both worlds: Unity-like immediate execution by default, with explicit control over deferred operations when needed.

## How It Works

### The Drop Trait Pattern

```rust
impl<'a, T> Drop for AddComponentCall<'a, T> {
    fn drop(&mut self) {
        if self.delayed {
            // Queue command for later
            command_buffer.add_component(entity, component);
        } else {
            // Execute immediately at semicolon!
            world.add_component(entity, component);
        }
    }
}
```

**Key Insight**: The builder is dropped (and thus executed) at the semicolon!

## API Patterns

### 1. Immediate Execution (Default)

```rust
entity.add_component(Transform::new(0.0, 0.0, 0.0));
// ↑ Executed IMMEDIATELY when semicolon is reached (Drop called)

assert!(entity.has_component::<Transform>());  // ✓ Works!
```

### 2. Deferred Execution (with `.delay()`)

```rust
entity.add_component(Transform::new(0.0, 0.0, 0.0)).delay();
// ↑ Queued for later, NOT executed yet

assert!(!entity.has_component::<Transform>());  // Still false

scene.apply_commands();  // NOW it executes
assert!(entity.has_component::<Transform>());   // ✓ Now true!
```

### 3. Fluent Chaining API (`.add()`)

```rust
entity
    .add(Transform::new(0.0, 0.0, 0.0))
    .add(Velocity::new(1.0, 0.0, 0.0))
    .add(Health::new(100.0));
// All execute immediately, returns &self for chaining
```

## Complete Example

```rust
let scene = Scene::new();
let player = scene.instantiate();

// Pattern 1: Immediate (Unity-like)
player.add_component(Transform::new(0.0, 0.0, 0.0));
println!("Transform exists: {}", player.has_component::<Transform>());  // true

// Pattern 2: Deferred (ECS-like)
player.add_component(Velocity::new(1.0, 0.0, 0.0)).delay();
println!("Velocity exists: {}", player.has_component::<Velocity>());    // false

// Pattern 3: Fluent chaining
player
    .add(Health::new(100.0))
    .add(Name::new("Player"));

// Apply deferred commands
scene.apply_commands();
println!("Velocity exists: {}", player.has_component::<Velocity>());    // now true!
```

## When to Use Each Pattern

### Use Immediate (default) When:
- ✓ Prototyping and quick iteration
- ✓ Simple entity setup
- ✓ You need components immediately accessible
- ✓ Single-threaded context

```rust
// Quick entity creation
let bullet = scene.instantiate();
bullet.add_component(Transform::new(x, y, 0.0));
bullet.add_component(Velocity::new(vx, vy, 0.0));
// Ready to use immediately!
```

### Use Deferred (`.delay()`) When:
- ✓ Parallel system execution
- ✓ Complex entity spawning from systems
- ✓ Avoiding mid-frame state inconsistencies
- ✓ Batching operations for performance

```rust
// In a system that spawns projectiles
for enemy in enemies_to_shoot {
    let projectile = scene.instantiate();
    projectile.add_component(Transform::from(enemy.pos)).delay();
    projectile.add_component(Velocity::toward(player)).delay();
    // Batched - all spawn together at frame end
}
```

### Use Fluent (`.add()`) When:
- ✓ Creating entities with multiple components
- ✓ You want clean, readable initialization code
- ✓ Chaining feels natural

```rust
scene.instantiate()
    .add(Transform::new(0.0, 0.0, 0.0))
    .add(Velocity::new(1.0, 0.0, 0.0))
    .add(Health::new(100.0))
    .add(Sprite::new("player.png"));
```

## Architecture Changes

### Entity Merged with GameObject

Before:
```rust
struct GameObject {
    entity: Entity,  // private
    world: Arc<RwLock<World>>,
    command_buffer: Arc<RwLock<CommandBuffer>>,
}
```

After:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity(pub u64);  // Now defined in game_object.rs

struct GameObject {
    pub entity: Entity,  // public field
    world: Arc<RwLock<World>>,
    command_buffer: Arc<RwLock<CommandBuffer>>,
}
```

**Benefits**:
- Entity is now just an ID with associated pointers
- Direct access to entity.entity field
- Simpler mental model
- No more redundant Entity types

### Removed `is_pending` Flag

The `is_pending` flag was unused and has been removed. The Drop pattern handles all deferred/immediate logic.

## Performance Comparison

Running `cargo run --example stress_test_bevy_style --release`:

| Pattern                   | FPS   | Frame Time | Ops/Second |
| ------------------------- | ----- | ---------- | ---------- |
| **Immediate**             | 1,698 | 0.589 ms   | 85M        |
| **Deferred (Bevy-style)** | 4,941 | 0.202 ms   | 247M       |

**Conclusion**: Deferred is ~3x faster for high-performance systems, but immediate is fast enough for most games and much easier to use.

## Demo

Run the examples to see it in action:

```bash
# See the Drop pattern in action
cargo run --example drop_demo

# Compare immediate vs deferred performance
cargo run --example stress_test_unity_style --release
cargo run --example stress_test_bevy_style --release
```

## Key Advantages

1. **Zero cognitive overhead**: Default behavior "just works" like Unity
2. **Explicit control**: `.delay()` makes deferred operations visible
3. **No magic**: Drop trait is standard Rust, easy to understand
4. **Type-safe**: Compiler prevents misuse
5. **Flexible**: Mix and match immediate/deferred as needed
6. **Clean code**: Fluent API for common cases

## Migration Guide

If you have old code using `.add_component()` for chaining:

```rust
// Old (still works, but change to .add for chaining)
entity
    .add_component(Transform::new(0.0, 0.0, 0.0))  // Returns builder
    .add_component(Velocity::new(1.0, 0.0, 0.0));  // Error: no method!

// New
entity
    .add(Transform::new(0.0, 0.0, 0.0))  // Returns &Self
    .add(Velocity::new(1.0, 0.0, 0.0));  // ✓ Works!

// Or keep non-chained immediate
entity.add_component(Transform::new(0.0, 0.0, 0.0));
entity.add_component(Velocity::new(1.0, 0.0, 0.0));
```

## Summary

The Drop-based API gives you:
- **Unity's immediacy** when you want it (default)
- **ECS's deferred control** when you need it (`.delay()`)
- **Clean syntax** for both patterns
- **Type safety** and compile-time guarantees
- **Flexibility** to mix patterns as needed

It's the best of both worlds! 🎉

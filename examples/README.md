# Stress Tests

This directory contains performance comparison tests for the hybrid ECS engine.

## Tests

### Unity-Style Iteration (`stress_test_unity_style.rs`)
- **Approach**: Entity list iteration + `get_component()` calls
- **Style**: Unity-like, familiar to GameObject programmers
- **Code Pattern**:
  ```rust
  let entities: Vec<Entity> = world.entities().collect();
  for entity in entities {
      if let (Some(transform), Some(velocity)) = (
          world.get_component_mut::<Transform>(entity),
          velocity,
      ) {
          // Process entity
      }
  }
  ```

### Bevy-Style Query (`stress_test_bevy_style.rs`)
- **Approach**: Direct component query iteration
- **Style**: ECS-native, optimal performance
- **Code Pattern**:
  ```rust
  for (transform, velocity) in world.query2_mut::<Transform, Velocity>() {
      // Process components directly
  }
  ```

## Scenario

Both tests simulate the same scenario:
- **10,000 entities** moving in a circle pattern toward a central obstacle
- **1 obstacle entity** with **5 box colliders** at different positions
- **1,000 frames** of simulation
- **50 million total collision checks** (10,000 entities × 1,000 frames × 5 colliders)

Entities check collision against all 5 box colliders and stop moving if they would intersect.

## Running the Tests

```bash
# Unity-style (familiar but slower)
cargo run --example stress_test_unity_style --release

# Bevy-style (optimal performance)
cargo run --example stress_test_bevy_style --release
```

## Expected Results

On a typical modern CPU, you should see:

| Style | FPS    | Frame Time | Checks/Second |
| ----- | ------ | ---------- | ------------- |
| Unity | ~1,600 | ~0.6 ms    | ~80M          |
| Bevy  | ~5,600 | ~0.2 ms    | ~280M         |

**Bevy-style is approximately 3-4x faster** due to:
1. **Cache coherency**: Queries iterate components directly (better memory layout)
2. **No hash lookups**: Direct component access vs HashMap lookups per entity
3. **Compiler optimizations**: Iterator chains optimize better than manual loops

## Key Insights

1. **Unity-style is still fast** - Good enough for most games
2. **Bevy-style wins on hot paths** - Use for performance-critical systems
3. **Both work with the same ECS** - Choose based on your needs:
   - Unity-style: Quick prototyping, familiar code
   - Bevy-style: Production systems, maximum performance

## Multi-Component Entities

These tests demonstrate entities with multiple components of the same type:
- The obstacle has **5 `BoxCollider` components**
- The ECS stores them as `Vec<T>` per entity
- `get_component()` returns the first one
- `get_components()` returns all of them

This is useful for:
- Composite colliders (multiple boxes/spheres per entity)
- Multiple renderers/sprites per object
- Complex entity hierarchies

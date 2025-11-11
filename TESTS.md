






## Implementation direct fetch
```rust
// Movement system implementation
pub struct MovementSystem;

impl System for MovementSystem {
    fn execute(&mut self, world: &mut World, delta_time: f32) {
        // Collect entities with both Transform and Velocity
        let entities: Vec<Entity> = world
            .query::<Transform>()
            .map(|iter| iter.map(|(e, _)| e).collect())
            .unwrap_or_default();

        // Update each entity
        for entity in entities {
            if let Some(velocity) = world.get_component::<Velocity>(entity) {
                let vel = velocity.clone();
                if let Some(transform) = world.get_component_mut::<Transform>(entity) {
                    transform.x += vel.x * delta_time;
                    transform.y += vel.y * delta_time;
                    transform.z += vel.z * delta_time;
                }
            }
        }
    }
}
```

```
=== Performance Test ===

Setting up test world...
✓ Created 10000 moving entities + 5000 static entities

Running 10,000 frame simulation...

=== Performance Results ===
Total frames:      10,000
Total entities:    15000 (10000 moving + 5000 static)
Systems per frame: 1 (MovementSystem)

Time taken:        6.337 seconds
Frames per second: 1578 FPS
Average frame time: 0.634 ms
Entity updates:    1000000 total
Updates per second: 157799

Sample entity after simulation:
  Entity_1039 - Position: (1198.91, 80.00, 0.00)

✓ Performance test completed!
```


## Implementation ECS iterator

```rust
// Movement system implementation
pub struct MovementSystem;

impl System for MovementSystem {
    fn execute(&mut self, world: &mut World, delta_time: f32) {
        // Clean ECS-style: iterate over pairs of (Transform, Velocity)
        for (transform, velocity) in world.query2_mut::<Transform, Velocity>() {
            transform.x += velocity.x * delta_time;
            transform.y += velocity.y * delta_time;
            transform.z += velocity.z * delta_time;
        }
    }
}

```

---

```
=== Performance Test ===

Setting up test world...
✓ Created 10000 moving entities + 5000 static entities

Running 10,000 frame simulation...

=== Performance Results ===
Total frames:      10,000
Total entities:    15000 (10000 moving + 5000 static)
Systems per frame: 1 (MovementSystem)

Time taken:        2.161 seconds
Frames per second: 4627 FPS
Average frame time: 0.216 ms
Entity updates:    1000000 total
Updates per second: 462716

Sample entity after simulation:
  Entity_7617 - Position: (7778.13, 80.00, 0.00)

✓ Performance test completed!
```
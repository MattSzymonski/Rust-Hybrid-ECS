//! Demonstrates the ECS scripting system end to end.
//!
//! # Responsibilities
//!
//! - Wires up a minimal [`Engine`] with scriptable components.
//! - Registers [`Counter`] as a script component and [`Position`] as a plain
//!   component, then drives them through several simulated frames.
//! - Shows how a script mutates its own data, reads and mutates other
//!   components on the same entity, and defers entity destruction.
//!
//! # Design
//!
//! A single [`Counter`] script owns its update logic and drives a `Position`
//! component through the [`ScriptContext`] APIs. Deferred operations such as
//! entity destruction are routed through the command buffer so they execute
//! after all scripts have run.

// External crates
use pill_engine::*;
use trait_type_map::impl_trait_accessible;

// =============================================================================
// Counter
// =============================================================================

/// A counter component that acts as a script.
///
/// Ticks toward a maximum on every frame and, once it maxes out, queues the
/// owning entity for destruction. Also demonstrates reading and mutating a
/// second component type on the same entity.
#[derive(Debug, Clone)]
struct Counter {
    /// Current accumulated counter value.
    value: i32,
    /// Amount added to `value` on every script update.
    increment: i32,
    /// Value at which the counter considers itself complete.
    max_value: i32,
}

impl Component for Counter {}

impl ScriptComponent for Counter {
    /// Runs one tick of the counter script for the current frame.
    fn update(&mut self, script_context: &mut ScriptContext) {
        // Step 1: Mutate the script's own data directly (always safe).
        self.value = (self.value + self.increment).min(self.max_value);
        println!("Counter: {} / {}", self.value, self.max_value);

        // Step 2: Read a component from the owning entity.
        if let Some(position) =
            script_context.get_component::<Position>(script_context.get_owning_entity())
        {
            println!("  Position: ({}, {})", position.x, position.y);
        }

        // Step 3: Mutate a component of a different type than self (safe).
        if let Some(position) =
            script_context.get_component_mut::<Position>(script_context.get_owning_entity())
        {
            position.y = self.value as f32;
            println!("  Updated Position.y to {}", position.y);
        }

        // Step 4: Queue entity destruction once the counter maxes out.
        // (Deferred - executes after all scripts have run.)
        if self.value >= self.max_value {
            println!("  Counter reached max! Queueing destruction...");
            script_context.destroy_entity(script_context.get_owning_entity());
        }

        // Step 5: Queue a component addition through the command buffer.
        let entity: Entity = script_context.get_owning_entity();

        script_context.get_commands().add_component_to_entity(
            entity,
            Position {
                x: 42.0,
                y: std::f32::consts::PI,
            },
        );
    }
}

// =============================================================================
// Position
// =============================================================================

/// A plain position component (not a script).
///
/// Stores two-dimensional coordinates; here it is driven indirectly by the
/// [`Counter`] script on the same entity.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Position {
    /// X coordinate in world space.
    x: f32,
    /// Y coordinate in world space.
    y: f32,
}

impl Component for Position {}

// Make components accessible through trait objects
impl_trait_accessible!(dyn Component; Counter, Position);

// =============================================================================
// Entry Point
// =============================================================================

/// Runs the scripting demo end to end.
///
/// Registers the [`Counter`] and [`Position`] components, creates two demo
/// entities, and simulates eight frames so the counter scripts can run.
fn main() {
    println!("=== ECS Scripting Example ===\n");

    let mut engine = Engine::new();

    // Step 1: Register the components with the world.
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_script_component::<Counter>();

    // Step 2: Create entity 1 - a counter script only.
    println!("Creating entity with counter...");
    let _entity2 = engine
        .world_mut()
        .create_entity()
        .with(Counter {
            value: 0,
            increment: 7,
            max_value: 50,
        })
        .build()
        .unwrap();

    // Step 3: Create entity 2 - a position plus a counter script.
    println!("Creating entity with position, and counter...\n");
    let _entity3 = engine
        .world_mut()
        .create_entity()
        .with(Position { x: 5.0, y: 10.0 })
        .with(Counter {
            value: 100,
            increment: 3,
            max_value: 120,
        })
        .build()
        .unwrap();

    // Step 4: Simulate several frames so the scripts can run.
    for frame in 1..=8 {
        println!("--- Frame {} ---", frame);
        engine.process_frame().unwrap();
        println!();
    }

    println!("=== Scripting Example Complete ===");
}

//! Demonstrates the ECS scripting system end to end.
//!
//! # Responsibilities
//!
//! - Builds a minimal [`Engine`] and registers [`Counter`] as a script
//!   component and [`Position`] as a plain component.
//! - Creates two demo entities and runs them through eight simulated frames.
//! - Shows how a script changes its own fields and reads and changes another
//!   component on the same entity.
//! - Queues destroying its own entity and adding a component. Both run after
//!   the scripts in the frame finish.
//!
//! # Design
//!
//! A [`Counter`] script holds its own update logic and receives
//! [`ScriptContext`], a restricted view of the world. The script changes a
//! [`Position`] on the same entity through that view. Structural changes,
//! such as destroying the entity or adding a component, are only queued. The
//! engine runs them after every script in the frame has finished.

// External crates
use pill_engine::*;

// =============================================================================
// Counter
// =============================================================================

/// A counter component that runs as a script.
///
/// On every update the counter adds its increment, up to `max_value`. When it
/// reaches that value it queues its own entity for destruction. The script
/// also reads and changes a component of another type on the same entity.
#[derive(Debug, Clone)]
struct Counter {
    /// Value reached so far, never above `max_value`.
    current_value: i32,
    /// Amount added to `current_value` on every update.
    increment: i32,
    /// Value at which the counter is finished.
    max_value: i32,
}

impl Component for Counter {}

impl ScriptComponent for Counter {
    /// Runs the counter script once for the current frame.
    fn update(&mut self, script_context: &mut ScriptContext) {
        // Step 1: Update the script's own fields. The script owns this data,
        // so no world access is involved.
        self.current_value = (self.current_value + self.increment).min(self.max_value);
        pill_core::info!("Counter: {} / {}", self.current_value, self.max_value);

        // Step 2: Read the `Position` component of the entity this script
        // belongs to.
        if let Some(position) =
            script_context.get_component::<Position>(script_context.get_owning_entity())
        {
            pill_core::info!("  Position: ({}, {})", position.x, position.y);
        }

        // Step 3: Change a component of another type on the same entity.
        if let Some(position) =
            script_context.get_component_mut::<Position>(script_context.get_owning_entity())
        {
            position.y = self.current_value as f32;
            pill_core::info!("  Updated Position.y to {}", position.y);
        }

        // Step 4: Queue destroying the entity once the counter reaches its
        // maximum. The engine runs queued commands after every script in the
        // frame has finished.
        if self.current_value >= self.max_value {
            pill_core::info!("  Counter reached max! Queueing destruction...");
            script_context.destroy_entity(script_context.get_owning_entity());
        }

        // Step 5: Queue adding a `Position` component to the same entity.
        let owning_entity: Entity = script_context.get_owning_entity();

        script_context.get_commands().add_component_to_entity(
            owning_entity,
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

/// A plain position component, not a script.
///
/// Stores two-dimensional coordinates. The [`Counter`] script on the same
/// entity changes its `y` field.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Position {
    /// X coordinate in world space.
    x: f32,
    /// Y coordinate in world space.
    y: f32,
}

impl Component for Position {}

// =============================================================================
// Entry Point
// =============================================================================

/// Runs the scripting demo end to end.
///
/// Registers the [`Counter`] and [`Position`] components, creates two demo
/// entities, and simulates eight frames so the counter scripts can run.
fn main() {
    // Install the engine's logger so this program's log lines and the
    // engine's own messages appear on the terminal.
    let _ = pill_core::telemetry::TelemetryBuilder::new().init();
    pill_core::info!("=== ECS Scripting Example ===");

    let mut engine = Engine::new();

    // Step 1: Register the components with the world.
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_script_component::<Counter>();

    // Step 2: Create the entity that carries only the counter script.
    pill_core::info!("Creating entity with counter...");
    let _counter_only_entity = engine
        .world_mut()
        .create_entity()
        .with(Counter {
            current_value: 0,
            increment: 7,
            max_value: 50,
        })
        .build()
        .unwrap();

    // Step 3: Create the entity that carries a position and a counter script.
    pill_core::info!("Creating entity with position, and counter...");
    let _position_and_counter_entity = engine
        .world_mut()
        .create_entity()
        .with(Position { x: 5.0, y: 10.0 })
        .with(Counter {
            current_value: 100,
            increment: 3,
            max_value: 120,
        })
        .build()
        .unwrap();

    // Step 4: Simulate several frames so the scripts can run.
    for frame in 1..=8 {
        pill_core::info!("--- Frame {} ---", frame);
        engine.process_frame().unwrap();
    }

    pill_core::info!("=== Scripting Example Complete ===");
}

// ============================================================================
// Engine - System Registration and Frame Execution
// ============================================================================
//! The Engine manages system execution and frame processing.
//!
//! It provides:
//! - System registration with names for debugging
//! - Two-phase frame processing: systems execute → deferred commands execute
//! - System state management (for persistent data between frames)

use crate::commands::CommandQueue;
use crate::system::{IntoSystem, System, SystemParam, SystemState};
use crate::world::World;

/// Wrapper for a registered system with its name and state
struct RegisteredSystem {
    name: &'static str,
    system: Box<dyn System>,
    state: SystemState,
}

/// The main Engine that drives the ECS
///
/// # Example
/// ```rust
/// let mut engine = Engine::new();
/// engine.register_system("movement", movement_system);
/// engine.register_system("collision", collision_system);
///
/// // Every frame:
/// engine.process_frame(&mut world);
/// ```
pub struct Engine {
    /// All registered systems with their names and states
    systems: Vec<RegisteredSystem>,
    /// Command queue for deferred operations
    queue: CommandQueue,
}

impl Engine {
    /// Create a new Engine with no systems
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
            queue: CommandQueue::new(),
        }
    }

    /// Register a system with a name
    ///
    /// The system can be any function whose parameters all implement SystemParam.
    /// The name is used for debugging and profiling.
    ///
    /// # Example
    /// ```rust
    /// engine.register_system("movement", |
    ///     mut commands: Commands,
    ///     query: Query<(Entity, &mut Position, &Velocity)>
    /// | {
    ///     // System implementation
    /// });
    /// ```
    pub fn register_system<F, Input>(&mut self, name: &'static str, system: F)
    where
        F: IntoSystem<Input>,
        Input: SystemParam,
    {
        self.systems.push(RegisteredSystem {
            name,
            system: system.into_system(),
            state: SystemState::new(),
        });
    }

    /// Process one frame - execute all systems then apply deferred commands
    ///
    /// This is the main loop of the ECS:
    /// 1. Execute all registered systems in order
    /// 2. Systems can queue commands (spawn, despawn, add component, etc.)
    /// 3. After all systems run, apply the queued commands
    ///
    /// This two-phase approach ensures structural changes don't interfere
    /// with systems that are still running.
    pub fn process_frame(&mut self, world: &mut World) {
        // Phase 1: Run all systems
        for registered in &mut self.systems {
            registered
                .system
                .run(world, &mut self.queue, &mut registered.state);
        }

        // Phase 2: Execute all deferred commands
        self.queue.execute(world);
    }
}

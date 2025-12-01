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
use crate::scheduler::{SystemAccess, SystemScheduler};
use crate::system::{IntoSystem, System, SystemParam, SystemState};
use crate::world::World;
use rayon::prelude::*;

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
/// engine.process_frame();
/// ```
pub struct Engine {
    /// All registered systems with their names and states
    systems: Vec<RegisteredSystem>,
    /// Command queue for deferred operations
    queue: CommandQueue,
    /// The ECS world
    world: World,
    /// System scheduler for parallel execution
    scheduler: SystemScheduler,
    /// Whether parallel execution is enabled
    parallel_execution: bool,
}

impl Engine {
    /// Create a new Engine with no systems
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
            queue: CommandQueue::new(),
            world: World::new(),
            scheduler: SystemScheduler::new(),
            // Parallel execution enabled now that Send bounds are in place
            parallel_execution: true,
        }
    }

    /// Enable or disable parallel system execution
    pub fn set_parallel_execution(&mut self, enabled: bool) {
        self.parallel_execution = enabled;
    }

    /// Print the execution graph for debugging
    pub fn print_execution_graph(&self) {
        let names: Vec<&str> = self.systems.iter().map(|s| s.name).collect();
        self.scheduler.print_execution_graph(&names);
    }

    /// Get a reference to the world
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Get a mutable reference to the world
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
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
        // Analyze system access pattern
        let mut access = SystemAccess::new();
        Input::report_access(&mut access);

        // Register with scheduler
        self.scheduler.register_system(access);

        // Store system
        self.systems.push(RegisteredSystem {
            name,
            system: system.into_system(),
            state: SystemState::new(),
        });

        // Rebuild execution graph
        self.scheduler.build_execution_graph();
    }

    /// Process one frame - execute all systems then apply deferred commands
    ///
    /// This is the main loop of the ECS:
    /// 1. Execute all registered systems (in parallel batches if enabled)
    /// 2. Systems can queue commands (spawn, despawn, add component, etc.)
    /// 3. After all systems run, apply the queued commands
    ///
    /// This two-phase approach ensures structural changes don't interfere
    /// with systems that are still running.
    pub fn process_frame(&mut self) {
        // Phase 1: Run all systems
        if self.parallel_execution && self.systems.len() > 1 {
            self.run_systems_parallel();
        } else {
            self.run_systems_sequential();
        }

        // Update script components after systems
        self.world.update_scripts();

        // Phase 2: Execute all deferred commands
        self.queue.execute_queued_commands(&mut self.world);
    }

    /// Run systems sequentially (fallback or when parallel is disabled)
    fn run_systems_sequential(&mut self) {
        for registered in &mut self.systems {
            registered
                .system
                .run(&mut self.world, &mut self.queue, &mut registered.state);
        }
    }

    /// Run systems in parallel batches based on dependency analysis
    ///
    /// SAFETY: This uses unsafe code to work around Rust's borrow checker.
    /// The safety is guaranteed by the scheduler's dependency analysis:
    /// - Systems in the same batch have been proven to access disjoint components
    /// - No two systems in a batch can have conflicting access (write-write or read-write)
    /// - Systems using Commands run exclusively (not in parallel with anything)
    fn run_systems_parallel(&mut self) {
        // Execute each batch
        for batch in self.scheduler.execution_graph() {
            if batch.len() == 1 {
                // Single system - run directly
                let idx = batch[0];
                let system_ptr = self.systems.as_mut_ptr();
                unsafe {
                    let registered = &mut *system_ptr.add(idx);
                    registered
                        .system
                        .run(&mut self.world, &mut self.queue, &mut registered.state);
                }
            } else {
                // Multiple systems - run in parallel using rayon
                // SAFETY: The scheduler guarantees that systems in the same batch access disjoint data
                // We use raw pointers and unsafe to allow parallel mutable access
                let world_ptr = &mut self.world as *mut World as usize;
                let queue_ptr = &mut self.queue as *mut CommandQueue as usize;
                let systems_ptr = self.systems.as_mut_ptr() as usize;

                batch.par_iter().for_each(|&idx| unsafe {
                    let world = &mut *(world_ptr as *mut World);
                    let queue = &mut *(queue_ptr as *mut CommandQueue);
                    let registered = &mut *(systems_ptr as *mut RegisteredSystem).add(idx);
                    registered.system.run(world, queue, &mut registered.state);
                });
            }
        }
    }
}

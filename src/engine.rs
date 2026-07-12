// ----------------------------------------------------------------------------
// Engine - System Registration and Frame Execution
// ----------------------------------------------------------------------------
//! The Engine manages system execution and frame processing.
//!
//! It provides:
//! - System registration with names for debugging
//! - Two-phase frame processing: systems execute → deferred commands execute
//! - System state management (for persistent data between frames)

use crate::commands::{CommandError, CommandQueue};
use crate::component::Tick;
use crate::scheduler::{SystemAccess, SystemScheduler};
use crate::system::{IntoSystem, System, SystemParam};
use crate::world::{set_per_thread_last_run_tick, World};
use rayon::prelude::*;

/// Wrapper for a registered system with its name
struct RegisteredSystem {
    name: &'static str,
    system: Box<dyn System>,
    /// World tick at which this system was last executed.
    ///
    /// Used to seed `World::system_last_run` so change-detection filters
    /// (`Changed<T>`, `Added<T>`) inside the system see only mutations
    /// that happened since its last run.
    last_run: u32,
    enabled: bool,
}

/// The main Engine that drives the ECS
///
/// # Example
/// ```no_run
/// # use ecs_hybrid::*;
/// # fn movement_system() {}
/// # fn collision_system() {}
/// let mut engine = Engine::new();
/// engine.register_system("movement", movement_system);
/// engine.register_system("collision", collision_system);
///
/// // Every frame:
/// engine.process_frame().unwrap();
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
    /// Whether the execution graph needs rebuilding (dirty after enable/disable)
    graph_dirty: bool,
    /// If true, `process_frame` returns an error immediately when any
    /// deferred command fails.  When false (default), errors are logged
    /// to stderr and execution continues.
    pub should_exit_on_error: bool,
}

impl Engine {
    /// Create a new Engine with no systems
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
            queue: CommandQueue::new(),
            world: World::new(),
            scheduler: SystemScheduler::new(),
            parallel_execution: true,
            graph_dirty: false,
            should_exit_on_error: false,
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// Enable or disable parallel system execution
    pub fn set_parallel_execution(&mut self, enabled: bool) {
        self.parallel_execution = enabled;
    }

    /// Enable a system by name
    ///
    /// Returns true if the system was found and enabled, false otherwise.
    pub fn enable_system(&mut self, name: &str) -> bool {
        self.set_system_enabled(name, true)
    }

    /// Disable a system by name
    ///
    /// Disabled systems are skipped during frame processing.
    /// Returns true if the system was found and disabled, false otherwise.
    pub fn disable_system(&mut self, name: &str) -> bool {
        self.set_system_enabled(name, false)
    }

    /// Set the enabled state of a system by name.
    ///
    /// Returns true if the system was found, false otherwise.
    /// Marks the execution graph as dirty so it is rebuilt before the
    /// next frame, ensuring parallel batches reflect the new enabled set.
    pub fn set_system_enabled(&mut self, name: &str, enabled: bool) -> bool {
        for system in &mut self.systems {
            if system.name == name {
                if system.enabled != enabled {
                    system.enabled = enabled;
                    self.graph_dirty = true;
                }
                return true;
            }
        }
        false
    }

    /// Check if a system is enabled
    ///
    /// Returns None if the system was not found.
    pub fn is_system_enabled(&self, name: &str) -> Option<bool> {
        self.systems
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.enabled)
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
    /// ```no_run
    /// # use ecs_hybrid::*;
    /// # #[derive(Debug, Clone)] struct Position { x: f32, y: f32 }
    /// # impl Component for Position {}
    /// # #[derive(Debug, Clone)] struct Velocity { vx: f32, vy: f32 }
    /// # impl Component for Velocity {}
    /// # let mut engine = Engine::new();
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

        // Precompute ComponentMask bitfields for O(1) conflict detection.
        access.build_component_masks(&self.world.component_registry);

        // Register with scheduler
        self.scheduler.register_system(access);

        // Store system
        self.systems.push(RegisteredSystem {
            name,
            system: system.into_system(),
            enabled: true,
            last_run: 0,
        });

        // Rebuild execution graph
        self.scheduler.build_execution_graph();
    }

    /// Process one frame - execute all systems then apply deferred commands
    ///
    /// This is the main loop of the ECS:
    /// 1. Execute all registered systems (in parallel batches if enabled)
    /// 2. Systems can queue commands (create, destroy, add component, etc.)
    /// 3. After all systems run, apply the queued commands
    ///
    /// This two-phase approach ensures structural changes don't interfere
    /// with systems that are still running.
    ///
    /// # Errors
    ///
    /// Returns `Err(Vec<CommandError>)` when [`should_exit_on_error`] is
    /// `true` and one or more deferred commands fail.  When the flag is
    /// `false` (default), errors are logged and `Ok(())` is returned.
    ///
    /// [`should_exit_on_error`]: Engine::should_exit_on_error
    pub fn process_frame(&mut self) -> Result<(), Vec<CommandError>> {
        let _tracy_frame = crate::profile_scope!("frame");

        // Bump the world tick so that any change-detection comparisons
        // performed by mutable queries during this frame use a fresh value.
        self.world.increment_change_tick();

        // Debug-only: clear the resource write-lock tracker so that the
        // isolation check only guards within a single frame.
        #[cfg(debug_assertions)]
        self.world.debug_clear_resource_locks();

        // Rebuild the execution graph if systems were enabled/disabled since
        // the last frame, so parallel batches reflect the current active set.
        if self.graph_dirty {
            self.scheduler.build_execution_graph();
            self.graph_dirty = false;
        }

        // Phase 1: Run all systems
        if self.parallel_execution && self.systems.len() > 1 {
            self.run_systems_parallel();
        } else {
            self.run_systems_sequential();
        }

        // Update script components after systems
        // Scripts receive a ScriptContext with read-only world access and deferred commands
        {
            let _zone = crate::profile_scope!("update_scripts");
            self.world.update_scripts(&mut self.queue);
        }

        // Phase 2: Execute all deferred commands (including those from scripts)
        let result;
        {
            let _zone = crate::profile_scope!("execute_commands");
            result = self
                .queue
                .execute_queued_commands(&mut self.world, self.should_exit_on_error);
        }
        crate::profile_frame_mark!();
        result
    }

    /// Run systems sequentially (fallback or when parallel is disabled)
    fn run_systems_sequential(&mut self) {
        let _zone = crate::profile_scope!("systems/sequential");
        for registered in &mut self.systems {
            if !registered.enabled {
                continue;
            }
            let _tracy_sys = crate::profile_scope_dyn!(registered.name);
            // Seed the change-detection baseline for this system: filters
            // such as `Changed<T>` will compare against `last_run`.
            self.world.system_last_run = registered.last_run;
            let started_at = self.world.change_tick().get();
            registered.system.run(&mut self.world, &mut self.queue);
            // Record the tick that was current at system entry so the next
            // run sees mutations that happened during this run.
            registered.last_run = started_at;
        }
        // Reset the baseline so ad-hoc queries between frames behave
        // predictably.
        self.world.system_last_run = 0;
    }

    /// Run systems in parallel batches based on dependency analysis
    ///
    /// SAFETY: This uses unsafe code to work around Rust's borrow checker.
    /// The safety is guaranteed by the scheduler's dependency analysis:
    /// - Systems in the same batch have been proven to access disjoint components
    /// - No two systems in a batch can have conflicting access (write-write or read-write)
    /// - Systems using Commands run exclusively (not in parallel with anything)
    fn run_systems_parallel(&mut self) {
        let _zone = crate::profile_scope!("systems/parallel");
        // Execute each batch
        for systems_batch in self.scheduler.execution_graph() {
            if systems_batch.len() == 1 {
                // Single system - run directly
                let system_index = systems_batch[0];
                let registered = &mut self.systems[system_index];
                if !registered.enabled {
                    continue;
                }
                let _tracy_sys = crate::profile_scope_dyn!(registered.name);
                self.world.system_last_run = registered.last_run;
                let started_at = self.world.change_tick().get();
                registered.system.run(&mut self.world, &mut self.queue);
                registered.last_run = started_at;
            } else {
                // Multiple systems - run in parallel using rayon.
                //
                // For change-detection: every system in the batch has, by
                // construction, disjoint component access from every other
                // system. This means each system's filter only inspects
                // ticks the scheduler has reserved for that system, so it
                // is safe to share `system_last_run` even though several
                // systems read it concurrently. We pre-compute each
                // system's `last_run` and seed it via a per-thread local,
                // sidestepping the world-level field for parallel batches.
                let started_at = self.world.change_tick().get();
                let last_runs: Vec<u32> = systems_batch
                    .iter()
                    .map(|&system_index| self.systems[system_index].last_run)
                    .collect();

                // - Prepare raw pointers for parallel access -
                //
                // Rust's borrow checker prevents us from passing `&mut self.world`,
                // `&mut self.queue`, and `&mut self.systems[idx]` into a Rayon
                // closure, even though the scheduler has *proven* that these
                // accesses are disjoint.  We sidestep the checker with raw
                // pointers, which carry no borrow obligations.
                //
                // Each pointer is wrapped in `SendPtrMut` so the closure can be
                // sent (`Send`) and shared (`Sync`) across Rayon worker threads.
                // The wrapper is zero-cost: a single `*mut T` field that LLVM
                // inlines away at any optimisation level.
                //
                // `addr_of_mut!` is used instead of `&mut ... as *mut T` because
                // it preserves pointer provenance - the compiler can still track
                // which allocation the pointer was derived from, enabling better
                // alias analysis and compatibility with strict-provenance
                // hardware (CHERI, Arm MTE).
                let world_ptr =
                    crate::query::ptr::SendPtrMut::new(std::ptr::addr_of_mut!(self.world));
                let queue_ptr =
                    crate::query::ptr::SendPtrMut::new(std::ptr::addr_of_mut!(self.queue));
                let systems_ptr = crate::query::ptr::SendPtrMut::new(self.systems.as_mut_ptr());

                // Pre-compute enabled flags to avoid capturing self.systems in closure
                // This is a small fixed-size read that avoids Vec allocation in most cases
                let enabled_flags: Vec<bool> = systems_batch
                    .iter()
                    .map(|&system_index| self.systems[system_index].enabled)
                    .collect();

                // Pre-compute system names for tracing spans in the parallel closure.
                let system_names: Vec<&'static str> = systems_batch
                    .iter()
                    .map(|&system_index| self.systems[system_index].name)
                    .collect();

                systems_batch
                    .par_iter()
                    .zip(enabled_flags.par_iter())
                    .zip(last_runs.par_iter())
                    .zip(system_names.par_iter())
                    // `move` transfers ownership of the three SendPtrMut
                    // handles into the closure.  SendPtrMut is Send + Sync
                    // (see query/ptr.rs), so Rayon can safely move and share
                    // the closure across its worker thread pool.
                    .for_each(move |(((&system_index, &enabled), &last_run), &name)| {
                        if !enabled {
                            return;
                        }

                        let _tracy_sys = crate::profile_scope_dyn!(name);

                        // Seed per-thread change-detection baseline.
                        // Because the scheduler guarantees that no two
                        // systems in this batch touch the same component,
                        // each thread can independently set its own
                        // `last_run` override without synchronization.
                        let previous_override =
                            set_per_thread_last_run_tick(Some(Tick::new(last_run)));

                        unsafe {
                            // SAFETY:
                            //
                            // Dereferencing these raw pointers is sound
                            // because the scheduler's conflict analysis
                            // (see scheduler.rs) guarantees:
                            //
                            // 1. No two systems in the same batch have
                            //    conflicting access (read-write or
                            //    write-write) to any component or resource.
                            //
                            // 2. Systems that use Commands are always
                            //    isolated into their own single-system
                            //    batches, so `queue` is never aliased
                            //    across threads here.
                            //
                            // 3. Each `system_index` is unique within
                            //    the batch, so `systems_ptr.add(system_index)`
                            //    yields a non-overlapping `RegisteredSystem`
                            //    - no two threads touch the same slot.
                            //
                            // 4. `world_ptr`, `queue_ptr`, and
                            //    `systems_ptr` all point to allocations
                            //    owned by `Engine` (in `self`), which
                            //    outlives the parallel iteration because
                            //    `run_systems_parallel` borrows `&mut self`
                            //    for its entire duration.
                            let world = &mut *world_ptr.as_ptr();
                            let queue = &mut *queue_ptr.as_ptr();
                            let registered_system = &mut *systems_ptr.as_ptr().add(system_index);
                            registered_system.system.run(world, queue);
                        }

                        // Restore the previous thread-local baseline so
                        // unrelated queries on this thread (e.g. from a
                        // nested rayon pool) are unaffected.
                        set_per_thread_last_run_tick(previous_override);
                    });

                // After the batch finishes, advance every system's
                // last_run to the tick we observed at batch start.
                for &system_index in systems_batch {
                    self.systems[system_index].last_run = started_at;
                }
            }
        }
        self.world.system_last_run = 0;
    }
}

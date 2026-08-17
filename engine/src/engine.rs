//! System registration, frame execution, and parallel dispatch orchestration.
//!
//! # Responsibilities
//!
//! - Registers systems by name and manages their lifecycle (enable/disable).
//! - Orchestrates the two-phase frame: execute systems in parallel batches,
//!   then apply deferred commands from the queue.
//! - Manages frame-rate limiting via `set_fps_limit`.
//! - Tracks per-system execution duration for adaptive parallelism hints.
//! - Provides `world()` and `world_mut()` accessors for ad-hoc queries.
//!
//! # Design
//!
//! The engine owns the [`World`], [`SystemScheduler`], and [`CommandQueue`].
//! Each frame: (1) bump the world tick, (2) walk the scheduler's execution
//! graph running systems in parallel batches via Rayon, (3) flush deferred
//! commands. Systems that use [`Commands`] always run sequentially due to
//! their exclusive-world-access requirement.

// Standard library
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// External crates
use rayon::prelude::*;

// Current crate
use crate::commands::{CommandError, CommandQueue};
use crate::component::Tick;
use crate::config::ParallelProcessingConfig;
use crate::error::{SystemError, SystemFailure};
use crate::scheduler::{SystemAccess, SystemScheduler};
use crate::system::{IntoSystem, System, SystemParam};
use crate::world::{set_per_thread_last_run_tick, World};

// =============================================================================
// RegisteredSystem
// =============================================================================

/// Wrapper for a registered system with its name
struct RegisteredSystem {
    name: String,
    system: Box<dyn System>,
    /// World tick at which this system was last executed.
    ///
    /// Used to seed `World::system_last_run` so change-detection filters
    /// (`Changed<T>`, `Added<T>`) inside the system see only mutations
    /// that happened since its last run.
    last_run: u32,
    enabled: bool,
    /// Exponential moving average of this system's wall-clock execution
    /// duration (nanoseconds).  Fed to the query iterator so it can pick
    /// an optimal number of parallel groups.  Smoothing factor ≈ 1/32
    /// gives a ~32-frame averaging window.
    average_duration: u64,
    /// Wall-clock duration (nanoseconds) from the most recent frame.
    /// Populated during `run_systems_sequential` / `run_systems_parallel`
    /// and consumed by the parallel-utilization metrics.
    last_duration: u64,
}

/// The main Engine that drives the ECS
///
/// # Example
/// ```no_run
/// # use pill_engine::*;
/// # fn movement_system() {}
/// # fn collision_system() {}
/// let mut engine = Engine::new();
/// engine.register_system("movement", movement_system);
/// engine.register_system("collision", collision_system);
///
/// // Every frame:
/// engine.process_frame().unwrap();
/// ```

// =============================================================================
// Engine
// =============================================================================

pub struct Engine {
    /// All registered systems with their names and states
    systems: Vec<RegisteredSystem>,
    /// Command queue for deferred operations
    queue: CommandQueue,
    /// The ECS world
    world: World,
    /// System scheduler for parallel execution
    scheduler: SystemScheduler,
    /// Failures recorded by systems during the most recent frames, drained
    /// once per frame by the reporting boundary. A failing system never
    /// aborts its batch or the frame.
    system_failures: Vec<SystemFailure>,
    /// Whether parallel execution is enabled
    parallel_execution: bool,
    /// Whether the execution graph needs rebuilding (dirty after enable/disable)
    graph_dirty: bool,
    /// If true, `process_frame` returns an error immediately when any
    /// deferred command fails.  When false (default), errors are logged
    /// to stderr and execution continues.
    pub should_exit_on_error: bool,
    /// Optional frame rate limit. When set, `process_frame` sleeps the
    /// remaining budget after completing the frame's work.
    frame_budget: Option<Duration>,
    /// When true, the FPS limiter sleep is wrapped in a Tracy zone
    /// (`frame_wait`). Turn off to declutter the timeline.
    pub trace_frame_wait: bool,
    /// Last seen archetype generation, used to detect when archetypes are
    /// added or removed so we can emit per-archetype memory breakdowns.
    last_archetype_generation: u64,
}

impl Engine {
    // -------------------------------------------------------------------------
    // Construction
    // -------------------------------------------------------------------------

    /// Create a new Engine with no systems
    pub fn new() -> Self {
        // !!! Order matters: name the main thread in Tracy BEFORE any zone
        // is emitted on it.  Tracy auto-registers a thread the first time
        // it sees a zone, and thread list ordering follows registration
        // order.  Naming first ensures "main" sits at the top.
        crate::profile_thread!("main");

        // Wrap initialization in a non-continuous Tracy frame.
        let _init = crate::profile_non_continuous_frame!("engine init");

        // Configure plot appearance in Tracy UI.
        crate::profile_plot_config!(entity_count, tracy_client::PlotConfiguration::default());
        crate::profile_plot_config!(archetype_count, tracy_client::PlotConfiguration::default());
        crate::profile_plot_config!(frame_time_us, tracy_client::PlotConfiguration::default());
        crate::profile_plot_config!(
            memory_estimate_kb,
            tracy_client::PlotConfiguration::default()
        );
        crate::profile_plot_config!(
            commands_executed,
            tracy_client::PlotConfiguration::default()
        );
        crate::profile_plot_config!(
            parallel_utilization_pct,
            tracy_client::PlotConfiguration::default()
        );
        crate::profile_plot_config!(
            batch_packing_pct,
            tracy_client::PlotConfiguration::default()
        );

        // Warm up the Rayon thread pool so the first parallel batch
        // does not pay thread-spawning costs (can be 1-10ms on some OS).
        rayon::broadcast(|_ctx| {});

        // Report detected system hardware so users can tune config.
        crate::config::print_system_specs();

        // Print the active parallel-iteration knobs.
        crate::config::print_parallel_config();

        Self {
            systems: Vec::new(),
            queue: CommandQueue::new(),
            world: World::new(),
            scheduler: SystemScheduler::new(),
            system_failures: Vec::new(),
            parallel_execution: true,
            graph_dirty: false,
            should_exit_on_error: false,
            frame_budget: None,
            trace_frame_wait: true,
            last_archetype_generation: 0,
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    // -------------------------------------------------------------------------
    // Configuration
    // -------------------------------------------------------------------------

    /// Enable or disable parallel system execution
    pub fn set_parallel_execution(&mut self, enabled: bool) {
        self.parallel_execution = enabled;
    }

    /// Limit the frame rate to `fps` frames per second.
    ///
    /// After completing the frame's work, `process_frame` sleeps the
    /// remaining budget. Pass `0.0` or `f64::INFINITY` to disable.
    ///
    /// # Example
    /// ```no_run
    /// # use pill_engine::*;
    /// # let mut engine = Engine::new();
    /// engine.set_fps_limit(30.0); // Cap at 30 FPS
    /// ```
    pub fn set_fps_limit(&mut self, fps: f64) {
        self.frame_budget = if fps > 0.0 && fps.is_finite() {
            Some(Duration::from_nanos((1_000_000_000.0 / fps) as u64))
        } else {
            None
        };
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

    // -------------------------------------------------------------------------
    // Inspection
    // -------------------------------------------------------------------------

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
        let names: Vec<&str> = self.systems.iter().map(|s| s.name.as_str()).collect();
        self.scheduler.print_execution_graph(&names);
    }

    // -------------------------------------------------------------------------
    // World Access
    // -------------------------------------------------------------------------

    /// Get a reference to the world
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Get a mutable reference to the world
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    // -------------------------------------------------------------------------
    // System Registration
    // -------------------------------------------------------------------------

    /// Register a system with a name
    ///
    /// The system can be any function whose parameters all implement SystemParam.
    /// The name is used for debugging and profiling.
    ///
    /// # Example
    /// ```no_run
    /// # use pill_engine::*;
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
    pub fn register_system<F, Input>(&mut self, name: impl Into<String>, system: F)
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
            name: name.into(),
            system: system.into_system(),
            enabled: true,
            last_run: 0,
            average_duration: 0,
            last_duration: 0,
        });

        // Rebuild execution graph
        self.scheduler.build_execution_graph();
    }

    /// Register a system whose access pattern was derived outside Rust's
    /// [`SystemParam`] machinery (for example from a managed-language system
    /// signature).
    ///
    /// # Safety
    ///
    /// `access` must describe every component/resource the system may touch.
    /// An incomplete declaration can make the scheduler run conflicting
    /// systems concurrently and cause undefined behavior.
    pub unsafe fn register_system_with_access<F>(
        &mut self,
        name: impl Into<String>,
        mut access: SystemAccess,
        system: F,
    ) where
        F: FnMut(&mut World, &mut CommandQueue) -> Result<(), SystemError> + Send + 'static,
    {
        access.build_component_masks(&self.world.component_registry);
        self.scheduler.register_system(access);
        self.systems.push(RegisteredSystem {
            name: name.into(),
            system: Box::new(system),
            enabled: true,
            last_run: 0,
            average_duration: 0,
            last_duration: 0,
        });
        self.scheduler.build_execution_graph();
    }
    /// Remove all registered systems, clearing the scheduler state.
    ///
    /// This is used during hot-reload: the old game DLL's system function
    /// pointers become invalid after unloading, so all systems must be
    /// dropped and re-registered from the newly loaded DLL.
    ///
    /// Component registrations, entities, and resources are **not** cleared —
    /// only systems are affected. The world state persists across reloads.
    pub fn clear_systems(&mut self) {
        self.systems.clear();
        self.scheduler.clear();
        self.graph_dirty = false;
        // Reset the execution graph so it does not reference stale indices.
        self.scheduler.build_execution_graph();
    }

    /// Drain the system failures recorded since the previous drain.
    ///
    /// The reporting boundary calls this once per frame and logs each
    /// failure's semantic message. A failed system never aborts its batch,
    /// so the remaining systems still complete the frame.
    pub fn drain_system_failures(&mut self) -> Vec<SystemFailure> {
        std::mem::take(&mut self.system_failures)
    }

    /// Read the system failures recorded since the previous drain without
    /// clearing them.
    pub fn system_failures(&self) -> &[SystemFailure] {
        &self.system_failures
    }

    // -------------------------------------------------------------------------
    // Frame Processing
    // -------------------------------------------------------------------------

    /// Run a one-shot command producer and flush its deferred mutations.
    ///
    /// This is primarily used by scripting-runtime startup hooks. The callback
    /// receives the same world/queue pair as an ordinary command-producing
    /// system, and all structural changes become visible only after it returns.
    pub fn run_deferred_commands<F>(&mut self, run: F) -> Result<(), Vec<CommandError>>
    where
        F: FnOnce(&mut World, &mut CommandQueue),
    {
        run(&mut self.world, &mut self.queue);
        // One-shot startup has no following frame in which to surface a
        // failure, so always return command errors to its caller.
        self.queue.execute_queued_commands(&mut self.world, true)
    }

    /// Run a command-producing closure without applying its commands.
    ///
    /// The transactional counterpart of [`Self::run_deferred_commands`]: the
    /// caller commits with [`Self::flush_deferred_commands`] or rolls back
    /// with [`Self::discard_deferred_commands`], so a failed phase can never
    /// leave partial world state behind.
    pub fn queue_deferred_commands<F>(&mut self, run: F)
    where
        F: FnOnce(&mut World, &mut CommandQueue),
    {
        run(&mut self.world, &mut self.queue);
    }

    /// Apply every command queued since the last flush.
    ///
    /// # Errors
    ///
    /// Returns one entry per command that could not be applied.
    pub fn flush_deferred_commands(&mut self) -> Result<(), Vec<CommandError>> {
        self.queue.execute_queued_commands(&mut self.world, true)
    }

    /// Discard every command queued since the last flush without applying it.
    pub fn discard_deferred_commands(&mut self) {
        self.queue.clear();
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
        let frame_start = Instant::now();
        let result;

        {
            // Tracy zone: frame = actual work, not including sleep
            let _tracy_frame = crate::profile_scope!(
                "frame",
                [
                    ("Active entities in world: {}", self.world.entity_count()),
                    ("Archetypes in world: {}", self.world.archetypes.len())
                ]
            );

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

            // Emit per-archetype memory breakdown when archetypes are added or removed.
            let current_generation = self.world.archetype_generation;
            if current_generation != self.last_archetype_generation {
                self.last_archetype_generation = current_generation;
                for _archetype in self.world.archetypes.values() {
                    crate::profile_message!(
                        "archetype {:?}: {} entities, {} component types, ~{} bytes",
                        _archetype.id,
                        _archetype.entity_count(),
                        _archetype.component_types.len(),
                        _archetype.memory_estimate(&self.world.component_registry),
                    );
                }
                crate::profile_message!(
                    "total world memory estimate: ~{} bytes ({} KB)",
                    self.world.memory_estimate(),
                    self.world.memory_estimate() / 1024,
                );
            }

            // Phase 1: Run all systems
            if self.parallel_execution && self.systems.len() > 1 {
                self.run_systems_parallel();
            } else {
                self.run_systems_sequential();
            }

            // Compute parallel-utilization metrics and emit Tracy plots.
            #[cfg(feature = "profiling")]
            if !self.systems.is_empty() {
                let durations: Vec<u64> = self.systems.iter().map(|s| s.last_duration).collect();
                crate::profiling::emit_parallel_utilization_plots(
                    &durations,
                    frame_start.elapsed().as_nanos() as u64,
                    rayon::current_num_threads(),
                    self.scheduler.execution_graph(),
                );
            }

            // Scripts receive a ScriptContext with read-only world access and deferred commands
            {
                let _zone = crate::profile_scope!("update scripts");
                self.world.update_scripts(&mut self.queue);
            }
            crate::profile_secondary_frame_mark!("scripts");

            // Phase 2: Execute all deferred commands (including those from scripts)
            {
                let _zone = crate::profile_scope!(
                    "execute commands",
                    [("Commands queued this frame: {}", !self.queue.is_empty())]
                );
                result = self
                    .queue
                    .execute_queued_commands(&mut self.world, self.should_exit_on_error);
            }
            crate::profile_secondary_frame_mark!("commands");

            // Check for duplicate iterator labels within this frame.
            {
                let mut timing = self
                    .world
                    .iterator_timings
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !timing.visited_duplicated_iterator_labels.is_empty() {
                    let duplicates = std::mem::take(&mut timing.visited_duplicated_iterator_labels);
                    crate::profile_warn!(
                        "duplicate parallel-iterator labels this frame: {:?} - two iterators sharing a .label() corrupt per-label timing",
                        duplicates
                    );
                    tracing::warn!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        labels = ?duplicates,
                        "duplicate parallel-iterator labels this frame; two iterators sharing a .label() corrupt per-label timing"
                    );
                }
                timing.visited_iterator_labels.clear();
            }

            crate::profile_frame_mark!();

            // Emit per-frame time-series plots for Tracy (native plot API).
            crate::profile_plot!(entity_count, self.world.entity_count() as f64);
            crate::profile_plot!(archetype_count, self.world.archetypes.len() as f64);
            crate::profile_plot!(frame_time_us, frame_start.elapsed().as_micros() as f64);
            // Memory estimate: sums archetype columns, entity locations, resources, and overhead.
            crate::profile_plot!(
                memory_estimate_kb,
                self.world.memory_estimate() as f64 / 1024.0
            );
            // ECS internals.
            crate::profile_plot!(free_entity_ids, self.world.free_entity_ids.len() as f64);
            crate::profile_plot!(component_types, self.world.component_registry.len() as f64);
            crate::profile_plot!(resource_count, self.world.resources.len() as f64);
            crate::profile_plot!(registered_systems, self.systems.len() as f64);
            crate::profile_plot!(
                execution_batches,
                self.scheduler.execution_graph().len() as f64
            );
            crate::profile_plot!(
                commands_executed,
                self.world.commands_executed_this_frame as f64
            );

            // Repeated numerical state also flows through the shared `metrics`
            // recorder so it can reach HUDs, files, and remote sinks without
            // duplicating the Tracy plots above.
            #[cfg(feature = "metrics")]
            {
                metrics::gauge!("ecs.entities").set(self.world.entity_count() as f64);
                metrics::gauge!("ecs.archetypes").set(self.world.archetypes.len() as f64);
                metrics::gauge!("ecs.systems").set(self.systems.len() as f64);
                metrics::gauge!("ecs.components").set(self.world.component_registry.len() as f64);
                metrics::histogram!("engine.frame_time_ms")
                    .record(frame_start.elapsed().as_secs_f64() * 1000.0);
            }
        } // <- frame zone ends here

        // FPS limiter - sleep remaining budget OUTSIDE the frame zone
        if let Some(budget) = self.frame_budget {
            let elapsed = frame_start.elapsed();
            if elapsed < budget {
                if self.trace_frame_wait {
                    let _zone = crate::profile_scope!(
                        "frame wait",
                        [
                            ("Frame budget (microseconds): {}", budget.as_micros()),
                            ("Work elapsed (microseconds): {}", elapsed.as_micros())
                        ]
                    );
                    std::thread::sleep(budget - elapsed);
                } else {
                    std::thread::sleep(budget - elapsed);
                }
            }
        }

        result
    }

    // -------------------------------------------------------------------------
    // Internal: Sequential Execution
    // -------------------------------------------------------------------------

    /// Run systems sequentially (fallback or when parallel is disabled)
    fn run_systems_sequential(&mut self) {
        let _zone = crate::profile_scope!(
            "systems sequential",
            [(
                "{} enabled systems ({} total registered)",
                self.systems.iter().filter(|s| s.enabled).count(),
                self.systems.len()
            )]
        );
        for registered_system in &mut self.systems {
            if !registered_system.enabled {
                continue;
            }
            let _tracy_sys = crate::profile_scope!(
                "system: {}",
                registered_system.name;
                [("System last ran at tick: {}", registered_system.last_run), ("System splitting hint execution time (ns): {}", registered_system.average_duration)]
            );
            // Seed the change-detection baseline for this system: filters
            // such as `Changed<T>` will compare against `last_run`.
            self.world.system_last_run = registered_system.last_run;

            // Feed timing hint to query iterators inside this system.
            let system_start = Instant::now();

            let started_at = self.world.change_tick().get();
            let result = registered_system
                .system
                .run(&mut self.world, &mut self.queue);
            if let Err(error) = result {
                self.system_failures
                    .push(SystemFailure::new(registered_system.name.clone(), error));
            }
            // Record the tick that was current at system entry so the next
            // run sees mutations that happened during this run.
            registered_system.last_run = started_at;
            // Update splitting hint of execution time.
            let elapsed = system_start.elapsed().as_nanos() as u64;
            let old_avg = registered_system.average_duration;
            let delta = elapsed as i64 - old_avg as i64;
            registered_system.average_duration =
                (old_avg as i64 + delta / ParallelProcessingConfig::SPLITTING_HINT_WINDOW) as u64;
            // Store instantaneous duration for utilization metric.
            registered_system.last_duration = elapsed;
        }
        // Reset the baseline so ad-hoc queries between frames behave
        // predictably.
        self.world.system_last_run = 0;
    }

    // -------------------------------------------------------------------------
    // Internal: Parallel Execution
    // -------------------------------------------------------------------------

    /// Run systems in parallel batches based on dependency analysis
    ///
    /// SAFETY: This uses unsafe code to work around Rust's borrow checker.
    /// The safety is guaranteed by the scheduler's dependency analysis:
    /// - Systems in the same batch have been proven to access disjoint components
    /// - No two systems in a batch can have conflicting access (write-write or read-write)
    /// - Systems using Commands run exclusively (not in parallel with anything)
    fn run_systems_parallel(&mut self) {
        let _zone = crate::profile_scope!(
            "run systems parallel",
            [
                (
                    "{} enabled systems ({} total)",
                    self.systems.iter().filter(|s| s.enabled).count(),
                    self.systems.len()
                ),
                (
                    "{} parallel batches",
                    self.scheduler.execution_graph().len()
                )
            ]
        );
        // Execute each batch
        let batches_len = self.scheduler.execution_graph().len();
        for (batch_index, systems_batch) in self.scheduler.execution_graph().iter().enumerate() {
            let batch_size = systems_batch.len();
            let _zone: crate::profiling::TracyZone = crate::profile_scope!(
                "run systems batch {}/{} ({} systems)",
                batch_index + 1,
                batches_len,
                batch_size
            );
            if batch_size == 1 {
                // Single system - run directly on main thread, skip rayon
                let system_index = systems_batch[0];
                let registered = &mut self.systems[system_index];
                if !registered.enabled {
                    continue;
                }
                self.world.system_last_run = registered.last_run;

                // Feed timing hint to query iterators inside this system.
                let system_start = Instant::now();

                let started_at = self.world.change_tick().get();

                let _zone = crate::profile_scope!(
                    "system: {}",
                    registered.name;
                    [("System last ran at tick: {}", registered.last_run), ("System splitting hint execution time (ns): {}", registered.average_duration)]
                );
                let result = registered.system.run(&mut self.world, &mut self.queue);
                if let Err(error) = result {
                    self.system_failures
                        .push(SystemFailure::new(registered.name.clone(), error));
                }
                registered.last_run = started_at;
                // Update splitting hint of execution time.
                let elapsed = system_start.elapsed().as_nanos() as u64;
                let old_avg = registered.average_duration;
                let delta = elapsed as i64 - old_avg as i64;
                registered.average_duration = (old_avg as i64
                    + delta / ParallelProcessingConfig::SPLITTING_HINT_WINDOW)
                    as u64;
                // Store instantaneous duration for utilization metric.
                registered.last_duration = elapsed;
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

                // Prepare stage: collect per-system data + create raw pointers
                let _zone = crate::profile_scope!("setup systems batch ({} systems)", batch_size);
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

                // Pre-compute system names for profiling spans in the parallel closure.
                let system_names: Vec<&str> = systems_batch
                    .iter()
                    .map(|&system_index| self.systems[system_index].name.as_str())
                    .collect();
                drop(_zone);

                // Dispatch stage: execute all systems in this batch via Rayon.
                // Use a single indexed parallel iterator instead of chained
                // zip() - avoids deep iterator nesting that causes Rayon
                // to spend more time splitting than executing on small batches.
                let _zone = crate::profile_scope!(
                    "dispatch systems batch ({} systems across rayon)",
                    batch_size
                );
                let batch_len = systems_batch.len();

                // Track how many Rayon tasks were spawned.
                let task_count = Arc::new(AtomicUsize::new(0));

                // Collect failed systems from the worker threads; the
                // scheduler guarantees disjoint system state, but the error
                // buffer itself is shared and must stay synchronized.
                let batch_failures = Mutex::new(Vec::<(String, SystemError)>::new());

                (0..batch_len).into_par_iter().for_each(|i| {
                    task_count.fetch_add(1, Ordering::Relaxed);

                    // Name the parallel worker thread for Tracy visibility.
                    crate::profile_thread!("parallel worker");

                    if !enabled_flags[i] {
                        return;
                    }

                    let _zone = crate::profile_scope!(
                        "system: {}",
                        system_names[i];
                        [("System position in batch: {}", i), ("Total systems in this batch: {}", batch_len)]
                    );

                    let previous_override =
                        set_per_thread_last_run_tick(Some(Tick::new(last_runs[i])));

                    unsafe {
                        let world = &mut *world_ptr.as_ptr();
                        let system_start = Instant::now();
                        let queue = &mut *queue_ptr.as_ptr();
                        let system_index = systems_batch[i];
                        let registered_system = &mut *systems_ptr.as_ptr().add(system_index);
                        let result = registered_system.system.run(world, queue);
                        if let Err(error) = result {
                            batch_failures
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .push((system_names[i].to_string(), error));
                        }
                        // Update splitting hint of execution time.
                        let elapsed = system_start.elapsed().as_nanos() as u64;
                        let old_avg = registered_system.average_duration;
                        let delta = elapsed as i64 - old_avg as i64;
                        registered_system.average_duration =
                            (old_avg as i64 + delta / ParallelProcessingConfig::SPLITTING_HINT_WINDOW) as u64;
                        // Store instantaneous duration for utilization metric.
                        registered_system.last_duration = elapsed;
                    }

                    set_per_thread_last_run_tick(previous_override);
                });

                crate::profile_message!(
                    "parallel system dispatch: batch {} of {} completed, {} systems spawned as {} rayon tasks across the thread pool",
                    batch_index + 1,
                    batches_len,
                    batch_len,
                    task_count.load(Ordering::Relaxed),
                );
                drop(_zone);

                // Fold the batch's failures into the per-frame record. The
                // order follows the workers' completion, not the batch index.
                let mut collected = batch_failures
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                self.system_failures.extend(
                    collected
                        .drain(..)
                        .map(|(system, error)| SystemFailure::new(system, error)),
                );

                // After the batch finishes, advance every system's
                // last_run to the tick we observed at batch start.
                let _advance_zone =
                    crate::profile_scope!("batch advance ticks ({} systems)", systems_batch.len());
                for &system_index in systems_batch {
                    self.systems[system_index].last_run = started_at;
                }
            }
        }
        self.world.system_last_run = 0;
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use pill_core::error::EngineMessage as _;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    /// A failing system is recorded with its name and drained by the frame.
    #[test]
    fn failing_system_is_recorded_and_drained_by_the_frame() {
        let mut engine = Engine::new();
        engine.register_system("failing_system", || -> Result<(), SystemError> {
            Err(SystemError::Failure {
                message: String::from("simulation exploded"),
            })
        });
        engine.process_frame().unwrap();
        let failures = engine.drain_system_failures();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].system, "failing_system");
        assert_eq!(
            failures[0].to_plain_message(),
            "system failing_system failed: simulation exploded"
        );
        // Draining cleared the record for the boundary...
        assert!(engine.system_failures().is_empty());
        // ...and the same failing system fails again on the next frame.
        engine.process_frame().unwrap();
        assert_eq!(engine.drain_system_failures().len(), 1);
    }

    /// Unit-returning systems still run without producing a failure record.
    #[test]
    fn unit_system_runs_without_recording_a_failure() {
        let mut engine = Engine::new();
        engine.register_system("unit_system", || {});
        engine.process_frame().unwrap();
        assert!(engine.system_failures().is_empty());
    }

    /// A failed system never aborts the systems that follow it.
    #[test]
    fn failing_system_does_not_abort_the_remaining_systems() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_system = Arc::clone(&counter);
        let mut engine = Engine::new();
        engine.register_system("failing_system", || -> Result<(), SystemError> {
            Err(SystemError::Failure {
                message: String::from("boom"),
            })
        });
        engine.register_system("counting_system", move || {
            counter_system.fetch_add(1, AtomicOrdering::SeqCst);
        });
        engine.process_frame().unwrap();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(engine.drain_system_failures().len(), 1);
    }

    /// Parallel batches record every failing system in the batch.
    #[test]
    fn parallel_batch_records_every_failing_system() {
        let mut engine = Engine::new();
        engine.register_system("first_failure", || -> Result<(), SystemError> {
            Err(SystemError::Failure {
                message: String::from("first"),
            })
        });
        engine.register_system("second_failure", || -> Result<(), SystemError> {
            Err(SystemError::Failure {
                message: String::from("second"),
            })
        });
        engine.process_frame().unwrap();
        let failures = engine.drain_system_failures();
        assert_eq!(failures.len(), 2);
        assert!(failures
            .iter()
            .any(|failure| failure.system == "first_failure"));
        assert!(failures
            .iter()
            .any(|failure| failure.system == "second_failure"));
    }
}

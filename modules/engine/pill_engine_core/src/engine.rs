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
//!
//! The engine is the one owner of the registered systems, and each record
//! carries the access pattern the scheduler derives its conflict data from.
//! Every structural change - a registration, a removal, a clear - invalidates
//! the scheduler; the frame rebuilds the batches once, before it dispatches.

// Standard library
use std::sync::Mutex;
use std::time::Duration;

// External crates
use pill_core::platform::Instant;
use pill_core::rayon;
use pill_core::rayon::prelude::*;
use pill_core::warn;

// Current crate
use crate::commands::{CommandError, CommandQueue};
use crate::component::Tick;
use crate::config::ParallelProcessingConfig;
use crate::error::{SystemError, SystemFailure};
use crate::scheduler::{SystemAccess, SystemScheduler};
use crate::system::{IntoSystem, System, SystemParam};
use crate::world::{set_per_thread_last_run_tick, set_per_thread_this_run_tick, World};

// =============================================================================
// RegisteredSystem
// =============================================================================

/// Identifies which loaded module registered a system.
///
/// A hot reload replaces one module's code while the rest of the process keeps
/// running. The systems of the module being swapped must be removed, because
/// their function pointers refer to the retired library, but the systems of
/// every other module and of the project must survive untouched. Tagging each
/// registration with its owner is what makes that separation possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SystemOwner(pub u64);

impl SystemOwner {
    /// Owner of systems registered by the host or by the project module.
    pub const PROJECT: Self = Self(0);

    /// Owner of systems the engine registers for itself.
    ///
    /// No reload path clears these: the host retires systems with
    /// [`Engine::clear_systems_owned_by`], naming the project or one module,
    /// and an engine-owned system is neither. That is what it is for - the
    /// function pointer lives in `pill_engine_core.dll`, which is never
    /// unloaded, so there is nothing to retire.
    ///
    /// `u64::MAX`, which no [`Self::extension`] index reaches.
    pub const ENGINE: Self = Self(u64::MAX);

    /// Owner of the extension loaded at `index`.
    ///
    /// Offset by one so that no extension can collide with
    /// [`Self::PROJECT`].
    pub const fn extension(index: usize) -> Self {
        Self(index as u64 + 1)
    }

    /// The extension index this owner encodes, or `None` for the project and
    /// the engine.
    pub fn extension_index(self) -> Option<usize> {
        if self == Self::PROJECT || self == Self::ENGINE {
            None
        } else {
            Some((self.0 - 1) as usize)
        }
    }
}

impl std::fmt::Display for SystemOwner {
    /// `project`, `engine`, or `extension <n>` - the one place a consumer
    /// turns an owner into text.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.extension_index() {
            None if *self == Self::PROJECT => formatter.write_str("project"),
            None => formatter.write_str("engine"),
            Some(index) => write!(formatter, "extension {index}"),
        }
    }
}

/// One registered system with its runtime bookkeeping.
///
/// Holds the system's name, boxed instance, access pattern, enabled state,
/// and the per-frame timing data that feeds adaptive parallelism and
/// profiling.
struct RegisteredSystem {
    /// Run after deferred commands, including while paused.
    post_update: bool,
    /// Name used for registration, debugging, and profiling.
    name: String,
    /// Module that registered this system, used to scope a hot reload.
    owner: SystemOwner,
    /// Boxed system instance, invoked with the world and command queue.
    system: Box<dyn System>,
    /// The components and resources this system touches.
    ///
    /// Owned here rather than by the scheduler so the access list and the
    /// system list cannot fall out of step: there is one list, and the
    /// scheduler derives its conflict data from it on demand.
    access: SystemAccess,
    /// World tick at which this system was last executed.
    ///
    /// Used to seed `World::system_last_run` so change-detection filters
    /// (`Changed<T>`, `Added<T>`) inside the system see only mutations
    /// that happened since its last run.
    last_run: u32,
    /// Whether this system participates in frame execution.
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

/// One member of a parallel batch, as the worker closure sees it.
///
/// A batch's per-member data used to live in four index-parallel vectors
/// (batch position, last run, reserved tick, enabled flag, name), which only
/// a comment kept aligned. One record per member makes the alignment a field.
struct BatchMember<'a> {
    /// Position in the engine's registered-system vector.
    system_index: usize,
    /// Tick at which this system last ran, from before the batch started.
    last_run: u32,
    /// Tick reserved for this member on the dispatching thread.
    this_run: u32,
    /// Registration name, for the Tracy span and the failure record.
    name: &'a str,
    /// Whether this member runs in the update phase of this batch.
    runs: bool,
}

/// A read-only snapshot of one registered system, in registration order.
///
/// `index` is the position in the registration vector and is the stable key
/// for toggling, because system **names are not unique** —
/// [`Engine::set_system_enabled`] matches the first system with a given name.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemSnapshot {
    /// Position in the registration vector; the un-ambiguous toggle key.
    pub index: usize,
    /// Registration name used for display, profiling, and (ambiguous) lookup.
    pub name: String,
    /// Module that registered this system: the project or one extension.
    pub owner: SystemOwner,
    /// Whether the system currently participates in frame execution.
    pub enabled: bool,
    /// Whether the system has a hot-patch dispatch slot in the registry.
    pub hot_patchable: bool,
    /// True when another registered system shares this name, so the UI can
    /// warn instead of toggling an ambiguous target.
    pub name_is_ambiguous: bool,
}

// =============================================================================
// Engine
// =============================================================================

/// The main engine that drives the ECS.
///
/// Owns the [`World`], [`SystemScheduler`], and [`CommandQueue`] and drives
/// the two-phase frame: systems execute (in parallel batches when enabled),
/// then deferred commands are applied. Systems are registered by name and can
/// be enabled or disabled at runtime.
///
/// # Examples
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
    /// When true, `process_frame` skips the system-execution phase (but still
    /// bumps the world tick, updates scripts, and flushes deferred commands).
    ///
    /// Deliberately a field rather than a host-side convention: an editor
    /// needs the engine itself to keep ticking and flushing while systems are
    /// frozen so it can mutate a paused world and observe the result.
    systems_paused: bool,
    /// When true, the next `process_frame` runs exactly one system phase even
    /// while paused, then pauses again. Set by
    /// [`Engine::request_single_step`].
    single_step_pending: bool,
    /// If true, `process_frame` returns every deferred-command failure it
    /// collected. When false (default), failures are logged and the frame
    /// reports success.
    ///
    /// Set through [`Engine::set_should_exit_on_error`], like every other knob
    /// on this type.
    should_exit_on_error: bool,
    /// Optional frame rate limit. When set, `process_frame` sleeps the
    /// remaining budget after completing the frame's work.
    frame_budget: Option<Duration>,
    /// When true, the FPS limiter sleep is wrapped in a Tracy zone
    /// (`frame_wait`). Turn off to declutter the timeline.
    ///
    /// Set through [`Engine::set_trace_frame_wait`].
    trace_frame_wait: bool,
    /// Last seen archetype generation, used to detect when archetypes are
    /// added or removed so we can emit per-archetype memory breakdowns.
    last_archetype_generation: u64,
    /// Owner assigned to systems registered from now on.
    ///
    /// The host scopes each module's initialization between
    /// [`Engine::begin_module_registration`] and
    /// [`Engine::end_module_registration`], so modules register systems exactly
    /// as before and still end up correctly attributed.
    active_owner: SystemOwner,
    /// Owners whose systems are suspended by
    /// [`Engine::set_systems_enabled_for_owner`].
    ///
    /// Kept as a set of owners rather than only as the systems' flags, so a
    /// system registered *while* its owner is suspended starts disabled: a new
    /// module generation initializing during a suspension must not run early.
    /// A `Vec` because only a handful of owners ever exist.
    disabled_owners: Vec<SystemOwner>,
    /// Dispatch slots for hot-patchable systems, keyed by registration name.
    ///
    /// Deliberately NOT behind `#[cfg(feature = "hot_patch")]`. Gating the field
    /// would change this struct's layout between feature configurations, and a
    /// module built with the other configuration reconstructs `&mut Engine` from
    /// a raw pointer through [`EngineApi`](crate::EngineApi) - so a layout
    /// difference would be memory corruption rather than a compile error. An
    /// unused empty map costs a few bytes once; the divergence would cost
    /// correctness.
    hot_patch_registry: crate::hot_patch::HotPatchRegistry,
}

impl Engine {
    // -------------------------------------------------------------------------
    // Construction
    // -------------------------------------------------------------------------

    /// Creates an engine with no registered systems, the engine-owned
    /// resources every build assumes (`Time`, `Input`, `AssetManager`) and the
    /// `Scene` asset type registered before any generation starts, so a reload
    /// cannot retire it.
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

        // Print the active parallel-iteration knobs.
        crate::config::print_parallel_config();

        // Engine-owned resources, present before any project or module runs so
        // nothing has to check whether they exist. All are advanced or owned
        // by the engine itself rather than by a registered system, so a hot
        // reload - which retires systems by owner - cannot take them away.
        let mut world = World::new();
        world.insert_resource(crate::time::Time::new());
        world.insert_resource(crate::input::Input::new());
        world.insert_resource(crate::asset::AssetManager::new());
        // The scene type is the engine's own, so every build lists and loads a
        // `.scene` file without a module or a project registering it. This runs
        // before any generation starts, so the registration marker it inserts
        // sits outside every module's `registered_since` diff - which is what
        // keeps a reload from retiring it.
        world.register_standalone_asset::<crate::scene::Scene>();

        let engine = Self {
            systems: Vec::new(),
            queue: CommandQueue::new(),
            world,
            scheduler: SystemScheduler::new(),
            system_failures: Vec::new(),
            parallel_execution: true,
            systems_paused: false,
            single_step_pending: false,
            should_exit_on_error: false,
            frame_budget: None,
            trace_frame_wait: true,
            last_archetype_generation: 0,
            active_owner: SystemOwner::PROJECT,
            disabled_owners: Vec::new(),
            hot_patch_registry: crate::hot_patch::HotPatchRegistry::new(),
        };

        // The ECS state report is opt-in: `set_ecs_diagnostics_interval` is the
        // only thing that registers it. It is not registered here because it
        // reads the whole world under exclusive access - a scheduler barrier
        // every frame - and prints a multi-line block once per interval, which
        // a shipping build must not pay for and no suite should have to filter
        // out. Engine-owned when it is registered, attributed to
        // `SystemOwner::ENGINE`, which no reload retires.
        engine
    }
}

impl Engine {
    // -------------------------------------------------------------------------
    // Configuration
    // -------------------------------------------------------------------------

    /// Enables or disables parallel system execution.
    ///
    /// When disabled, systems run sequentially in registration order.
    pub fn set_parallel_execution(&mut self, enabled: bool) {
        self.parallel_execution = enabled;
    }

    /// How often the built-in ECS state report prints.
    ///
    /// The report is opt-in: [`Engine::new`] registers nothing, and this is the
    /// call that registers it. `None` turns it off again, removing the system.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use pill_engine::diagnostics::DEFAULT_REPORT_INTERVAL;
    /// # let mut engine = pill_engine::Engine::new();
    /// engine.set_ecs_diagnostics_interval(Some(DEFAULT_REPORT_INTERVAL)); // on
    /// engine.set_ecs_diagnostics_interval(Some(Duration::from_secs(10)));  // slower
    /// engine.set_ecs_diagnostics_interval(None);                           // off
    /// ```
    ///
    /// Each call re-registers, so exactly one such system is registered however
    /// many times it is called. A registered report costs one `Duration`
    /// comparison on the frames it does not print, plus the scheduler barrier
    /// its exclusive world access makes every frame; `None` removes both.
    pub fn set_ecs_diagnostics_interval(&mut self, interval: Option<Duration>) {
        // Re-register rather than mutate: the interval is captured by the
        // closure, and replacing it keeps exactly one such system registered.
        self.clear_systems_owned_by(SystemOwner::ENGINE);
        self.register_ecs_diagnostics_system(interval);
    }

    /// Register the periodic ECS state report under
    /// [`SystemOwner::ENGINE`].
    ///
    /// `None`, or a zero interval, registers nothing - which is how the report
    /// is disabled.
    fn register_ecs_diagnostics_system(&mut self, interval: Option<Duration>) {
        let Some(interval) = interval.filter(|interval| !interval.is_zero()) else {
            return;
        };

        let mut access = SystemAccess::new();
        // The report reads the entire world - every archetype, the registry,
        // the resources - which no per-component declaration can describe.
        // Declaring exclusive world access is what makes that sound: the
        // scheduler never places a system that conflicts with everything into a
        // parallel batch, so nothing else is touching the world while it runs.
        access.set_uses_commands(true);

        // Schedule state lives in the closure, which the registration takes as
        // an `FnMut`.
        let mut next_report: Option<Duration> = None;
        let previous_owner = self.active_owner;
        self.active_owner = SystemOwner::ENGINE;
        // SAFETY: `access` declares exclusive world access, which is a superset
        // of everything the system touches; it only reads, and never retains a
        // borrow past the call.
        unsafe {
            self.register_system_with_access(
                crate::diagnostics::SYSTEM_NAME,
                access,
                // The closure owns the schedule. `None` until the first
                // frame, so the first report lands one interval in rather than
                // immediately at startup, when the world has just been built.
                move |world: &mut World, _queue: &mut CommandQueue| {
                    let elapsed = world
                        .get_resource::<crate::time::Time>()
                        .map_or(Duration::ZERO, crate::time::Time::elapsed);
                    match next_report {
                        Some(due) if elapsed >= due => {}
                        Some(_) => return Ok(()),
                        None => {
                            next_report = Some(elapsed + interval);
                            return Ok(());
                        }
                    }
                    // Measured from now rather than from the time that was
                    // due, so a stalled frame does not leave a backlog that
                    // prints several reports in a row to catch up.
                    next_report = Some(elapsed + interval);
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        "{}",
                        crate::diagnostics::EcsSnapshot::gather(world).render()
                    );
                    Ok(())
                },
            );
        }
        self.active_owner = previous_owner;
    }

    /// Stop executing systems without stopping the frame.
    ///
    /// A paused engine still bumps the world tick, still updates scripts and
    /// still flushes deferred commands, so an editor can mutate a frozen world
    /// and see the result. Only the system-execution phase is skipped.
    pub fn set_systems_paused(&mut self, paused: bool) {
        self.systems_paused = paused;
        if !paused {
            // Resuming also cancels any pending single step: there is nothing
            // to step one frame of if systems are running freely.
            self.single_step_pending = false;
        }
    }

    /// Whether the system-execution phase is currently skipped.
    pub fn systems_paused(&self) -> bool {
        self.systems_paused
    }

    /// Run exactly one more system phase on the next `process_frame`, then
    /// pause again.
    ///
    /// Used by the editor's step control: it works whether or not the engine
    /// is currently paused and leaves the engine paused afterwards.
    pub fn request_single_step(&mut self) {
        self.single_step_pending = true;
    }

    /// Limits the frame rate to `fps` frames per second.
    ///
    /// After completing the frame's work, `process_frame` sleeps the
    /// remaining budget. Pass `0.0` or `f64::INFINITY` to disable.
    ///
    /// # Examples
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

    /// Chooses whether a deferred-command failure fails the frame.
    ///
    /// When `true`, `process_frame` returns every failure it collected and the
    /// caller decides what to do; when `false` (the default), failures are
    /// logged and the frame reports success.
    pub fn set_should_exit_on_error(&mut self, should_exit: bool) {
        self.should_exit_on_error = should_exit;
    }

    /// Whether a deferred-command failure fails the frame.
    pub fn should_exit_on_error(&self) -> bool {
        self.should_exit_on_error
    }

    /// Chooses whether the frame-rate limiter's sleep is traced.
    ///
    /// The zone is named `frame_wait`; turn it off to declutter the timeline.
    pub fn set_trace_frame_wait(&mut self, trace: bool) {
        self.trace_frame_wait = trace;
    }

    /// Enables a system by name.
    ///
    /// The name-keyed convenience over [`Self::set_system_enabled_at`]. Names
    /// are not unique, so a caller that has a [`SystemSnapshot`] should use
    /// that snapshot's index instead: this one hits the first match.
    ///
    /// Returns `true` if the system was found and enabled, `false` otherwise.
    pub fn enable_system(&mut self, name: &str) -> bool {
        self.set_system_enabled(name, true)
    }

    /// Disables a system by name.
    ///
    /// The name-keyed convenience over [`Self::set_system_enabled_at`]; see
    /// [`Self::enable_system`] for the caveat about duplicate names.
    ///
    /// Returns `true` if the system was found and disabled, `false` otherwise.
    pub fn disable_system(&mut self, name: &str) -> bool {
        self.set_system_enabled(name, false)
    }

    /// Sets the enabled state of a system by name.
    ///
    /// Returns `true` if the system was found, `false` otherwise. The batch
    /// graph does not change: a disabled system keeps its place and is skipped
    /// at dispatch, because batches are decided by conflicts alone.
    pub fn set_system_enabled(&mut self, name: &str, enabled: bool) -> bool {
        for system in &mut self.systems {
            if system.name == name {
                system.enabled = enabled;
                return true;
            }
        }
        false
    }

    // -------------------------------------------------------------------------
    // Inspection
    // -------------------------------------------------------------------------

    /// Checks whether a system is enabled.
    ///
    /// Returns `None` if the system was not found.
    pub fn is_system_enabled(&self, name: &str) -> Option<bool> {
        self.systems
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.enabled)
    }

    /// Whether a system with this name is registered.
    ///
    /// The existence question on its own; [`Self::is_system_enabled`] also
    /// answers it, but reads as a predicate about enabled-ness. Names are not
    /// unique, so this says `true` for any registration carrying the name.
    pub fn is_registered(&self, name: &str) -> bool {
        self.system_index(name).is_some()
    }

    /// The registration index of the first system named `name`.
    ///
    /// That index is what [`Self::set_system_enabled_at`] takes and what
    /// [`SystemSnapshot::index`] carries, so a caller that needs to mutate a
    /// specific registration resolves it here rather than by name.
    pub fn system_index(&self, name: &str) -> Option<usize> {
        self.systems.iter().position(|s| s.name == name)
    }

    /// Snapshot of every registered system, in registration order.
    ///
    /// The editor's Systems tab reads this; it never requires knowing the
    /// concrete system type. `hot_patchable` reports whether the system has a
    /// dispatch slot in the hot-patch registry (only present when the host was
    /// built with the `hot_patch` feature).
    pub fn system_snapshots(&self) -> Vec<SystemSnapshot> {
        self.systems
            .iter()
            .enumerate()
            .map(|(index, system)| {
                let name_is_ambiguous = self
                    .systems
                    .iter()
                    .filter(|other| other.name == system.name)
                    .count()
                    > 1;
                SystemSnapshot {
                    index,
                    name: system.name.clone(),
                    owner: system.owner,
                    enabled: system.enabled,
                    hot_patchable: self.hot_patch_registry.get(&system.name).is_some(),
                    name_is_ambiguous,
                }
            })
            .collect()
    }

    /// Enable or disable a system by registration index, unambiguously.
    ///
    /// Unlike [`Self::set_system_enabled`], which matches by name and therefore
    /// hits the first of several same-named systems, this targets exactly one
    /// registration. Returns `false` when the index is out of range.
    pub fn set_system_enabled_at(&mut self, index: usize, enabled: bool) -> bool {
        let Some(system) = self.systems.get_mut(index) else {
            return false;
        };
        system.enabled = enabled;
        true
    }

    /// Prints the execution graph for debugging.
    ///
    /// Rebuilds the batches first when the system set changed since the last
    /// frame, so the report matches what the next frame would dispatch.
    pub fn print_execution_graph(&mut self) {
        self.scheduler.ensure_execution_graph(
            self.systems
                .iter()
                .map(|registered_system| &registered_system.access),
        );
        let mut lines = Vec::new();
        for (batch_index, batch) in self.scheduler.execution_graph().iter().enumerate() {
            lines.push(format!(
                "Batch {batch_index}: {} systems (parallel)",
                batch.len()
            ));
            for &system_index in batch {
                let Some(registered_system) = self.systems.get(system_index) else {
                    lines.push(format!("  - <unknown system {system_index}>"));
                    continue;
                };
                let access = &registered_system.access;
                lines.push(format!(
                    "  - {} (reads: {}, writes: {}, res_reads: {}, res_writes: {}, commands: {})",
                    registered_system.name,
                    access.reads_count(),
                    access.writes_count(),
                    access.resource_reads_count(),
                    access.resource_writes_count(),
                    access.uses_commands
                ));
            }
        }
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            "{}",
            pill_core::telemetry::log_block("System execution graph", lines)
        );
    }

    // -------------------------------------------------------------------------
    // World Access
    // -------------------------------------------------------------------------

    /// Returns a shared reference to the world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Returns a mutable reference to the world.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    // -------------------------------------------------------------------------
    // Input
    // -------------------------------------------------------------------------

    /// Queue an input event from the frontend; it takes effect at the start of
    /// the next frame. See [`crate::input`].
    pub fn push_input_event(&mut self, event: crate::input::InputEvent) {
        if let Some(input) = self.world.get_resource_mut::<crate::input::Input>() {
            input.push_event(event);
        }
    }

    /// Take the rumble requests systems queued, for the frontend that owns the
    /// gamepads to play.
    pub fn take_rumble_requests(&mut self) -> Vec<crate::input::RumbleRequest> {
        self.world
            .get_resource_mut::<crate::input::Input>()
            .map(crate::input::Input::take_rumble_requests)
            .unwrap_or_default()
    }

    // -------------------------------------------------------------------------
    // System Registration
    // -------------------------------------------------------------------------

    /// Registers a system with a name.
    ///
    /// The system can be any function whose parameters all implement
    /// [`SystemParam`]. The name is used for debugging and profiling.
    ///
    /// # Examples
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
        let (system_name, access, boxed_system) = self.prepare_system(name, system);
        self.push_system(system_name, access, boxed_system, false);
    }

    /// Register a post-flush system. Commands emitted here apply next frame.
    pub fn register_post_update_system<F, Input>(&mut self, name: impl Into<String>, system: F)
    where
        F: IntoSystem<Input>,
        Input: SystemParam,
    {
        let (system_name, access, boxed_system) = self.prepare_system(name, system);
        self.push_system(system_name, access, boxed_system, true);
    }

    /// Derives a registration's access pattern and boxes its system.
    ///
    /// The half both registration entry points share: report the access the
    /// parameter tuple declares, then box the system - through a hot-patch
    /// dispatch slot when that feature is on, so a later patch is one atomic
    /// store rather than a re-registration.
    fn prepare_system<F, Input>(
        &mut self,
        name: impl Into<String>,
        system: F,
    ) -> (String, SystemAccess, Box<dyn System>)
    where
        F: IntoSystem<Input>,
        Input: SystemParam,
    {
        let mut access = SystemAccess::new();
        Input::report_access(&mut access);
        let system_name: String = name.into();

        // Box the system. With hot patching enabled the call to the user's
        // function goes through a dispatch slot the engine keeps a handle to,
        // so a later patch is one atomic store rather than a re-registration.
        #[cfg(feature = "hot_patch")]
        let boxed_system = {
            let slot = std::sync::Arc::new(crate::hot_patch::HotSlot::new());
            // The qualified path is what a generated patch reports, and what a
            // panic in a patched body is attributed to; the display name is
            // the fallback when the function path cannot be resolved.
            let function_path = crate::hot_patch::function_path::<F>();
            let faults_key = function_path.clone().unwrap_or_else(|| system_name.clone());
            let boxed = system.into_hot_system(std::sync::Arc::clone(&slot), faults_key);

            // Registered under BOTH names. The display name is what callers and
            // logs use; the function's real path is what `#[pill_hot]` derives
            // and what a generated patch reports. They routinely differ - the
            // example project registers `physics_system` as `"ball_physics"` -
            // and a patch pipeline that only knew one of them would fail to
            // find the slot.
            self.hot_patch_registry
                .insert(system_name.clone(), std::sync::Arc::clone(&slot));
            if let Some(path) = function_path {
                if path != system_name {
                    self.hot_patch_registry.insert(path, slot);
                }
            }
            boxed
        };
        #[cfg(not(feature = "hot_patch"))]
        let boxed_system = system.into_system();

        (system_name, access, boxed_system)
    }

    // -------------------------------------------------------------------------
    // Hot patching
    // -------------------------------------------------------------------------

    /// Replace one registered system's implementation without re-registering it.
    ///
    /// The system keeps its identity, its position in the scheduler graph, its
    /// access metadata and its change-detection ticks; the `World` is untouched.
    /// The replacement takes effect on the system's next invocation, so callers
    /// should apply patches at a frame boundary, where no system is running.
    ///
    /// `signature_hash` must be the value
    /// [`hot_patch::signature_hash`](crate::hot_patch::signature_hash) produces
    /// for the replacement's parameter tuple and return type. A mismatch is
    /// refused, because the running call site was compiled for the old shape.
    ///
    /// # Errors
    ///
    /// Returns [`HotPatchError`](crate::hot_patch::HotPatchError) when no
    /// hot-patchable system is registered under `name` (`UnknownSystem`), when
    /// the signature moved (`SignatureMismatch`), or for a null replacement
    /// (`NullAddress`). In every case the running implementation stays in
    /// place.
    pub fn hot_patch(
        &mut self,
        name: &str,
        address: usize,
        signature_hash: u64,
    ) -> Result<(), crate::hot_patch::HotPatchError> {
        let slot = self.hot_patch_registry.get(name).ok_or_else(|| {
            crate::hot_patch::HotPatchError::UnknownSystem {
                name: name.to_string(),
            }
        })?;
        slot.install(name, address, signature_hash)
    }

    /// The implementation a system was registered with, and its signature.
    ///
    /// A host uses this to roll a patched system back to generation zero: the
    /// code the running artifact was actually built with. `None` when no such
    /// system is registered, or when it is not hot-patchable.
    pub fn hot_patch_baseline(&self, name: &str) -> Option<(usize, u64)> {
        let slot = self.hot_patch_registry.get(name)?;
        let original = slot.original();
        if original == 0 {
            return None;
        }
        Some((original, slot.signature_hash()))
    }

    /// Read-only access to the hot-patch registry, for hosts and tooling.
    pub fn hot_patch_registry(&self) -> &crate::hot_patch::HotPatchRegistry {
        &self.hot_patch_registry
    }

    /// Registers a system whose access pattern was derived outside Rust's
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
        access: SystemAccess,
        system: F,
    ) where
        F: FnMut(&mut World, &mut CommandQueue) -> Result<(), SystemError> + Send + 'static,
    {
        self.push_system(name.into(), access, Box::new(system), false);
    }

    /// Pushes one registered system and invalidates the scheduler's graph.
    ///
    /// Every registration entry point ends here, so a system's owner, its
    /// phase, its suspended-at-registration state and the invalidation cannot
    /// drift between them. `post_update` is a parameter rather than a field
    /// written after the push, so a record never exists with a phase that
    /// disagrees with the batches it was placed in.
    ///
    /// The batches themselves are not built here: the frame rebuilds them once
    /// from the whole list, so a module registering many systems pays for one
    /// rebuild rather than one per registration.
    fn push_system(
        &mut self,
        name: String,
        access: SystemAccess,
        system: Box<dyn System>,
        post_update: bool,
    ) {
        self.systems.push(RegisteredSystem {
            post_update,
            name,
            owner: self.active_owner,
            system,
            access,
            // Starts suspended when its owner is (see `disabled_owners`).
            enabled: !self.disabled_owners.contains(&self.active_owner),
            last_run: 0,
            average_duration: 0,
            last_duration: 0,
        });
        self.scheduler.invalidate();
    }
    /// Attributes every system registered from now on to `owner`.
    ///
    /// The host wraps a module's initialization in this call and
    /// [`Self::end_module_registration`], so module code registers systems by
    /// name while the engine records which module each system came from.
    pub fn begin_module_registration(&mut self, owner: SystemOwner) {
        self.active_owner = owner;
    }

    /// Returns registration attribution to [`SystemOwner::PROJECT`].
    pub fn end_module_registration(&mut self) {
        self.active_owner = SystemOwner::PROJECT;
    }

    /// Suspends or resumes every system registered by `owner`.
    ///
    /// Disabling also covers systems `owner` registers later, until it is
    /// enabled again: a module generation that initializes during a suspension
    /// registers its systems disabled. The suspension outlives
    /// [`Self::clear_systems_owned_by`], which removes systems but not the
    /// owner's state. Enabling turns every system of `owner` back on, including
    /// any disabled individually with [`Self::set_system_enabled`] meanwhile.
    ///
    /// Returns the number of systems `owner` currently has. The batch graph is
    /// not touched: it lists every registered system and a disabled one is
    /// skipped at dispatch.
    pub fn set_systems_enabled_for_owner(&mut self, owner: SystemOwner, enabled: bool) -> usize {
        // Step 1: Record the owner's state for systems registered later.
        if enabled {
            self.disabled_owners.retain(|disabled| *disabled != owner);
        } else if !self.disabled_owners.contains(&owner) {
            self.disabled_owners.push(owner);
        }

        // Step 2: Apply it to the systems registered so far.
        let mut owned = 0;
        for system in self
            .systems
            .iter_mut()
            .filter(|system| system.owner == owner)
        {
            owned += 1;
            system.enabled = enabled;
        }
        owned
    }

    /// Whether systems of `owner` are allowed to run, i.e. it isn't suspended
    /// by [`Self::set_systems_enabled_for_owner`].
    pub fn is_owner_enabled(&self, owner: SystemOwner) -> bool {
        !self.disabled_owners.contains(&owner)
    }

    /// Removes only the systems registered by `owner`.
    ///
    /// Used when hot reloading one extension: the replacement library's
    /// systems are different function pointers, so the retiring generation's
    /// systems must go, while the project and every other module keep running.
    /// Component registrations, entities, and resources are untouched.
    ///
    /// Returns the number of systems removed.
    pub fn clear_systems_owned_by(&mut self, owner: SystemOwner) -> usize {
        // Forget the dispatch slots of the systems about to be removed, so a
        // later patch can never install into a system that no longer runs.
        let mut removed_count = 0;
        for registered_system in &self.systems {
            if registered_system.owner == owner {
                self.hot_patch_registry.remove(&registered_system.name);
                removed_count += 1;
            }
        }
        if removed_count == 0 {
            return 0;
        }

        self.systems
            .retain(|registered_system| registered_system.owner != owner);
        // The graph's indices address the system list, whose positions have
        // shifted; the next frame rebuilds it from the survivors.
        self.scheduler.invalidate();
        removed_count
    }

    /// Removes all registered systems, clearing the scheduler state.
    ///
    /// Their dispatch slots are retired with them, so nothing can be patched
    /// into a system that no longer runs.
    ///
    /// Component registrations, entities, and resources are **not** cleared —
    /// only systems are affected. The world state persists.
    pub fn clear_systems(&mut self) {
        self.systems.clear();
        // Every system is gone, so no dispatch slot may remain patchable.
        self.hot_patch_registry = crate::hot_patch::HotPatchRegistry::new();
        // The graph and its conflict matrix describe systems that no longer
        // exist; the next frame derives an empty one from the empty list.
        self.scheduler.clear();
    }

    /// Drains the system failures recorded since the previous drain.
    ///
    /// The reporting boundary calls this once per frame and logs each
    /// failure's semantic message. A failed system never aborts its batch,
    /// so the remaining systems still complete the frame.
    pub fn drain_system_failures(&mut self) -> Vec<SystemFailure> {
        std::mem::take(&mut self.system_failures)
    }

    /// Reads the system failures recorded since the previous drain without
    /// clearing them.
    pub fn system_failures(&self) -> &[SystemFailure] {
        &self.system_failures
    }

    // -------------------------------------------------------------------------
    // Frame Processing
    // -------------------------------------------------------------------------

    /// Runs a one-shot command producer and flushes its deferred mutations.
    ///
    /// This is primarily used by scripting-runtime startup hooks. The callback
    /// receives the same world/queue pair as an ordinary command-producing
    /// system, and all structural changes become visible only after it returns.
    ///
    /// # Errors
    ///
    /// Returns one entry per command that could not be applied.
    pub fn run_deferred_commands<F>(&mut self, run: F) -> Result<(), Vec<CommandError>>
    where
        F: FnOnce(&mut World, &mut CommandQueue),
    {
        run(&mut self.world, &mut self.queue);
        // One-shot startup has no following frame in which to surface a
        // failure, so always return command errors to its caller.
        self.queue.execute_queued_commands(&mut self.world, true)
    }

    /// Runs a command-producing closure without applying its commands.
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

    /// Applies every command queued since the last flush.
    ///
    /// # Errors
    ///
    /// Returns one entry per command that could not be applied.
    pub fn flush_deferred_commands(&mut self) -> Result<(), Vec<CommandError>> {
        self.queue.execute_queued_commands(&mut self.world, true)
    }

    /// Discards every command queued since the last flush without applying it.
    pub fn discard_deferred_commands(&mut self) {
        self.queue.clear();
    }

    /// Processes one frame: executes all systems, then applies deferred commands.
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

            // Step 1: Bump the world change tick and reset per-frame debug state.
            //
            // Bump the world tick so that any change-detection comparisons
            // performed by mutable queries during this frame use a fresh value.
            self.world.increment_change_tick();

            // Advance the clock before anything reads it, so every system in
            // this frame sees the same values and none sees last frame's.
            //
            // Done here rather than in a registered system because
            // `ensure_execution_graph` batches in registration order: a time
            // system would only be ordered ahead of its readers if every
            // project registered it first, and a project that registers one
            // late would hand its earlier systems stale values with nothing in
            // the API to reveal it. `get_resource_mut` rather than the tracked
            // variant, since bumping a change tick for a resource that changes
            // every single frame tells a reader nothing.
            if let Some(time) = self.world.get_resource_mut::<crate::time::Time>() {
                time.advance();
            }

            // Apply the input the frontend queued since the last frame, for
            // the same reason and in the same way as the clock: every system
            // in this frame sees the same keys, buttons and motion.
            if let Some(input) = self.world.get_resource_mut::<crate::input::Input>() {
                input.begin_frame();
            }

            // Debug-only: clear the resource write-lock tracker so that the
            // isolation check only guards within a single frame.
            #[cfg(debug_assertions)]
            self.world.debug_clear_resource_locks();

            // Step 2: Rebuild the execution graph when the system set changed
            // since the last frame, so parallel batches reflect the active
            // set. Both the conflict matrix and the batches are derived here,
            // once per change, from the systems' own access patterns.
            self.scheduler.ensure_execution_graph(
                self.systems
                    .iter()
                    .map(|registered_system| &registered_system.access),
            );

            // Step 3: Emit per-archetype memory breakdowns when the archetype
            // set changed since the last frame.
            self.emit_archetype_memory_breakdown();

            // Step 4: Run all systems (parallel batches or sequential fallback).
            //
            // A paused engine skips this phase but still bumps the world tick
            // (Step 1), still updates scripts (Step 5), and still flushes
            // deferred commands (Step 6). `request_single_step` runs exactly
            // one phase and re-pauses by clearing the pending flag here.
            if !self.systems_paused || self.single_step_pending {
                if self.parallel_execution && self.systems.len() > 1 {
                    self.run_systems_parallel();
                } else {
                    self.run_systems_sequential(false);
                }
                self.single_step_pending = false;
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

            // Step 5: Update scripts with read-only world access and deferred commands.
            {
                let _zone = crate::profile_scope!("update scripts");
                self.world.update_scripts(&mut self.queue);
            }
            crate::profile_secondary_frame_mark!("scripts");

            // Step 6: Execute all deferred commands (including those from scripts).
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
            self.run_systems_sequential(true);

            // Step 7: Check for duplicate iterator labels within this frame and
            // emit the per-frame time-series plots and metrics.
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
                    warn!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        labels = ?duplicates,
                        "duplicate parallel-iterator labels this frame; two iterators sharing a .label() corrupt per-label timing"
                    );
                }
                timing.visited_iterator_labels.clear();
            }

            crate::profile_frame_mark!();

            self.emit_frame_telemetry(frame_start);
        } // <- frame zone ends here

        // FPS limiter - sleep remaining budget OUTSIDE the frame zone
        self.wait_for_frame_budget(frame_start);

        result
    }

    /// Emit the per-frame Tracy plots and metrics-reporter gauges.
    #[cfg_attr(not(feature = "profiling"), allow(unused_variables))]
    fn emit_frame_telemetry(&self, frame_start: Instant) {
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

        // The same numbers also flow through the shared `metrics` recorder,
        // which forwards them to Tracy's plot API when a profiler is attached.
        // It keeps no copy of its own: a reader that wants these values reads
        // Tracy or logs them.
        #[cfg(feature = "metrics")]
        {
            metrics::gauge!("ecs.entities").set(self.world.entity_count() as f64);
            metrics::gauge!("ecs.archetypes").set(self.world.archetypes.len() as f64);
            metrics::gauge!("ecs.systems").set(self.systems.len() as f64);
            metrics::gauge!("ecs.components").set(self.world.component_registry.len() as f64);
            metrics::histogram!("engine.frame_time_ms")
                .record(frame_start.elapsed().as_secs_f64() * 1000.0);
        }
    }

    /// Emit one memory-breakdown block when the archetype set has changed.
    #[cfg_attr(not(feature = "profiling"), allow(unused_variables))]
    fn emit_archetype_memory_breakdown(&mut self) {
        let current_generation = self.world.archetype_generation;
        if current_generation == self.last_archetype_generation {
            return;
        }
        self.last_archetype_generation = current_generation;
        for archetype in self.world.archetypes.values() {
            crate::profile_message!(
                "archetype {:?}: {} entities, {} component types, ~{} bytes",
                archetype.id,
                archetype.entity_count(),
                archetype.component_types.len(),
                archetype.memory_estimate(),
            );
        }
        crate::profile_message!(
            "total world memory estimate: ~{} bytes ({} KB)",
            self.world.memory_estimate(),
            self.world.memory_estimate() / 1024,
        );
    }

    /// Sleep off whatever remains of the frame budget, outside the frame zone.
    fn wait_for_frame_budget(&self, frame_start: Instant) {
        let Some(budget) = self.frame_budget else {
            return;
        };
        let elapsed = frame_start.elapsed();
        if elapsed >= budget {
            return;
        }
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

    // -------------------------------------------------------------------------
    // Internal: Sequential Execution
    // -------------------------------------------------------------------------

    /// Runs systems sequentially: when parallel execution is disabled, when
    /// only one system is registered, and for the post-update phase, which
    /// always follows the command flush on the calling thread.
    fn run_systems_sequential(&mut self, post_update: bool) {
        let _zone = crate::profile_scope!(
            "systems sequential",
            [(
                "{} enabled systems ({} total registered)",
                self.systems.iter().filter(|s| s.enabled).count(),
                self.systems.len()
            )]
        );
        let world = &mut self.world;
        let queue = &mut self.queue;
        let failures = &mut self.system_failures;
        for registered_system in &mut self.systems {
            if !registered_system.enabled || registered_system.post_update != post_update {
                continue;
            }
            run_registered_system(world, queue, failures, registered_system);
        }
        // Reset the baseline so ad-hoc queries between frames behave
        // predictably.
        self.world.system_last_run = 0;
    }

    // -------------------------------------------------------------------------
    // Internal: Parallel Execution
    // -------------------------------------------------------------------------

    /// Runs systems in parallel batches based on the scheduler's dependency analysis.
    ///
    /// Each batch is proven by the scheduler to access disjoint components and
    /// resources, so its systems can run concurrently on Rayon worker threads.
    /// Systems that use [`Commands`] run exclusively (never in parallel with
    /// anything) because they require exclusive access to the world.
    ///
    /// # Safety
    ///
    /// This function contains `unsafe` code that rebuilds `&mut` references from
    /// raw pointers. Its soundness rests on the scheduler's guarantees: systems
    /// in one batch access disjoint state, command-using systems run alone, and
    /// every batch index stays within the `systems` vector.
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
                let runs = {
                    let registered_system = &self.systems[system_index];
                    // The post-flush phase runs after the command flush, never
                    // from a batch.
                    registered_system.enabled && !registered_system.post_update
                };
                if runs {
                    let registered_system = &mut self.systems[system_index];
                    run_registered_system(
                        &mut self.world,
                        &mut self.queue,
                        &mut self.system_failures,
                        registered_system,
                    );
                }
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

                // Step 1: Collect per-system data and raw pointers for disjoint
                // parallel access. One record per batch member replaces the
                // index-parallel vectors this used to carry, so the alignment
                // between a member's index, its ticks, its name and whether it
                // runs is structural rather than a comment.
                let _zone = crate::profile_scope!("setup systems batch ({} systems)", batch_size);
                let systems_ptr = crate::query::ptr::SendPtrMut::new(self.systems.as_mut_ptr());
                let mut members: Vec<BatchMember<'_>> = Vec::with_capacity(batch_size);
                for &system_index in systems_batch {
                    let registered_system = &self.systems[system_index];
                    members.push(BatchMember {
                        system_index,
                        last_run: registered_system.last_run,
                        // Reserve one tick per batch member on this thread,
                        // before any pointer is handed out: this is the last
                        // point where `&mut self.world` is legitimately held,
                        // and it is what keeps the workers from
                        // read-modify-writing the shared counter. Disabled
                        // members reserve too, so every member carries the
                        // tick its index reserved.
                        this_run: self.world.increment_change_tick().get(),
                        name: registered_system.name.as_str(),
                        runs: registered_system.enabled && !registered_system.post_update,
                    });
                }

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
                // it never forms a reference first: a `&mut` to memory the
                // workers are about to write through would assert the exclusive
                // access this function cannot uphold. Deriving the pointer
                // directly also keeps its provenance tied to the allocation,
                // which strict-provenance targets (CHERI, Arm MTE) rely on.
                let world_ptr =
                    crate::query::ptr::SendPtrMut::new(std::ptr::addr_of_mut!(self.world));
                let queue_ptr =
                    crate::query::ptr::SendPtrMut::new(std::ptr::addr_of_mut!(self.queue));
                drop(_zone);

                // Step 2: Dispatch all systems in the batch across Rayon workers.
                // Use a single indexed parallel iterator instead of chained
                // zip() - avoids deep iterator nesting that causes Rayon
                // to spend more time splitting than executing on small batches.
                let _zone = crate::profile_scope!(
                    "dispatch systems batch ({} systems across rayon)",
                    batch_size
                );

                // Collect failed systems from the worker threads; the
                // scheduler guarantees disjoint system state, but the error
                // buffer itself is shared and must stay synchronized.
                let batch_failures = Mutex::new(Vec::<(String, SystemError)>::new());

                members.into_par_iter().enumerate().for_each(|(_position, member)| {
                    // Name the parallel worker thread for Tracy visibility.
                    crate::profile_thread!("parallel worker");

                    if !member.runs {
                        return;
                    }

                    let _zone = crate::profile_scope!(
                        "system: {}",
                        member.name;
                        [("System position in batch: {}", _position), ("Total systems in this batch: {}", batch_size)]
                    );

                    let previous_override =
                        set_per_thread_last_run_tick(Some(Tick::new(member.last_run)));
                    let previous_this_run =
                        set_per_thread_this_run_tick(Some(Tick::new(member.this_run)));

                    // SAFETY: The scheduler proved that the systems in this
                    // batch access disjoint components and resources, so no two
                    // worker threads alias the same world, queue, or system
                    // state; command-using systems never share a batch. Each
                    // pointer was derived from `self` while `&mut self` was held
                    // for the whole batch, so the underlying allocations outlive
                    // every closure invocation. `member.system_index` comes from
                    // `systems_batch`, which the scheduler built from
                    // `self.systems`, so `add(system_index)` stays within the
                    // `systems` allocation. The only shared mutable buffer,
                    // `batch_failures`, is guarded by a `Mutex`, and per-thread
                    // last-run ticks are stored thread-locally. Rebuilding
                    // `&mut World`, `&mut CommandQueue`, and `&mut
                    // RegisteredSystem` from these pointers is therefore valid:
                    // disjoint, in-bounds, alive for the duration of use, and
                    // type-valid.
                    unsafe {
                        let world = &mut *world_ptr.as_ptr();
                        let system_start = Instant::now();
                        let queue = &mut *queue_ptr.as_ptr();
                        let registered_system = &mut *systems_ptr.as_ptr().add(member.system_index);
                        let result = registered_system.system.run(world, queue);
                        if let Err(error) = result {
                            batch_failures
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .push((member.name.to_string(), error));
                        }
                        record_system_run(
                            registered_system,
                            started_at,
                            system_start.elapsed().as_nanos() as u64,
                        );
                    }

                    set_per_thread_this_run_tick(previous_this_run);
                    set_per_thread_last_run_tick(previous_override);
                });

                crate::profile_message!(
                    "parallel system dispatch: batch {} of {} completed, {} systems spawned as {} rayon tasks across the thread pool",
                    batch_index + 1,
                    batches_len,
                    batch_size,
                    batch_size,
                );
                drop(_zone);

                // Step 3: Fold the batch's failures into the frame record and
                // advance every system's last-run tick. The failure order follows
                // the workers' completion, not the batch index.
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

/// Runs one registered system on the calling thread.
///
/// The one definition of a run - the change-detection baseline, the timing,
/// the failure record and the bookkeeping - shared by the sequential loop, the
/// single-system batch lane and the post-flush phase. The parallel closure
/// keeps its own body: its bookkeeping goes through thread-local ticks and a
/// shared failure buffer.
///
/// Takes the engine's fields rather than `&mut Engine` so the caller can hold
/// the scheduler's execution graph borrowed for its loop.
fn run_registered_system(
    world: &mut World,
    queue: &mut CommandQueue,
    failures: &mut Vec<SystemFailure>,
    registered_system: &mut RegisteredSystem,
) {
    let _tracy_sys = crate::profile_scope!(
        "system: {}",
        registered_system.name;
        [("System last ran at tick: {}", registered_system.last_run), ("System splitting hint execution time (ns): {}", registered_system.average_duration)]
    );
    // Seed the change-detection baseline for this system: filters such as
    // `Changed<T>` compare against `last_run`.
    world.system_last_run = registered_system.last_run;

    // Feed the timing hint to the query iterators inside this system.
    let system_start = Instant::now();
    let started_at = world.change_tick().get();
    if let Err(error) = registered_system.system.run(world, queue) {
        failures.push(SystemFailure::new(registered_system.name.clone(), error));
    }
    record_system_run(
        registered_system,
        started_at,
        system_start.elapsed().as_nanos() as u64,
    );
}

/// Apply the bookkeeping every dispatch path owes a system that just ran.
///
/// One definition for the sequential path, the single-system batch lane and
/// the parallel batch closure: the change-detection baseline advances to the
/// tick captured at system entry (so the next run sees mutations that
/// happened during this run), the splitting hint tracks the observed
/// duration with the configured smoothing window, and the instantaneous
/// duration feeds the utilization metric. The batch-wide `last_run` advance
/// after the closure stays where it is: it also covers disabled members,
/// which never enter a worker task.
fn record_system_run(
    registered_system: &mut RegisteredSystem,
    started_at: u32,
    elapsed_nanos: u64,
) {
    registered_system.last_run = started_at;
    // Update splitting hint of execution time.
    let old_avg = registered_system.average_duration;
    let delta = elapsed_nanos as i64 - old_avg as i64;
    registered_system.average_duration =
        (old_avg as i64 + delta / ParallelProcessingConfig::SPLITTING_HINT_WINDOW) as u64;
    // Store instantaneous duration for utilization metric.
    registered_system.last_duration = elapsed_nanos;
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    // Standard library
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    // External crates
    use pill_core::error::EngineMessage as _;

    // Current crate
    use super::*;

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

    /// A panicking system is contained: it becomes a recorded failure, the
    /// frame survives, and three consecutive panics queue one fault report for
    /// the host to roll the function back with.
    ///
    /// Hot-patch only: containment lives in the dispatch slot a patched system
    /// calls through, and without the feature there is no patch to roll back.
    #[cfg(feature = "hot_patch")]
    #[test]
    fn a_panicking_system_is_contained_and_reports_a_fault() {
        let mut engine = Engine::new();
        // The streak table is process-global and tests share the process, so
        // this function's name is unique to this test.
        let name = "engine_tests::panicking_system";
        engine.register_system(name, || -> () {
            panic!("kaboom in the patched body");
        });
        let _ = crate::hot_patch::take_fault_reports();
        for frame in 1..crate::hot_patch::PANIC_ROLLBACK_FRAMES {
            engine.process_frame().unwrap();
            let failures = engine.drain_system_failures();
            assert_eq!(failures.len(), 1, "frame {frame}");
            assert!(
                failures[0].to_plain_message().contains("panicked: kaboom"),
                "frame {frame}: {}",
                failures[0].to_plain_message()
            );
            assert!(
                crate::hot_patch::take_fault_reports().is_empty(),
                "frame {frame} reported before the threshold"
            );
        }
        engine.process_frame().unwrap();
        let reports = crate::hot_patch::take_fault_reports();
        assert_eq!(reports.len(), 1, "the third panic must report the function");
        assert_eq!(reports[0].function, name);
        assert!(reports[0].message.contains("kaboom"));
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

    /// A post-update system runs after the update phase and the command flush,
    /// even when it was registered before the system whose work it follows.
    ///
    /// The phase is a property of the registration, not of its position: a
    /// post-update system registered first would run first if registration
    /// order decided, and the command flush would then happen after it.
    #[test]
    fn post_update_systems_run_after_the_update_phase_whatever_their_order() {
        let order: Arc<std::sync::Mutex<Vec<&'static str>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut engine = Engine::new();
        let post_update_order = Arc::clone(&order);
        engine.register_post_update_system("after_flush", move || {
            post_update_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push("post_update");
        });
        let update_order = Arc::clone(&order);
        engine.register_system("before_flush", move || {
            update_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push("update");
        });

        engine.process_frame().unwrap();

        let order = order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            *order,
            vec!["update", "post_update"],
            "the post-update phase follows the update phase and the command flush"
        );
    }

    /// Every parallel batch member gets exactly one reserved tick: queries
    /// inside one system see the same tick however often they ask, and two
    /// systems in one batch never share one - which is what keeps the shared
    /// counter out of the workers' hands.
    #[test]
    fn parallel_batch_systems_receive_one_tick_each() {
        let observed: Arc<std::sync::Mutex<Vec<[u32; 2]>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let mut engine = Engine::new();
        for name in ["tick_first", "tick_second"] {
            let observed = Arc::clone(&observed);
            // SAFETY: both systems declare no component or resource access at
            // all, so the scheduler batches them together and they touch
            // nothing but the tick counter.
            unsafe {
                engine.register_system_with_access(
                    name,
                    SystemAccess::new(),
                    move |world: &mut World, _queue: &mut CommandQueue| {
                        let first = world.increment_change_tick().get();
                        let second = world.increment_change_tick().get();
                        observed
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push([first, second]);
                        Ok(())
                    },
                );
            }
        }

        engine.process_frame().unwrap();

        let observed = observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(observed.len(), 2, "both systems ran");
        for ticks in observed.iter() {
            assert_eq!(
                ticks[0], ticks[1],
                "one system's run carries one tick, however many queries ask for it"
            );
        }
        assert_ne!(
            observed[0][0], observed[1][0],
            "each batch member reserves its own tick"
        );
    }

    /// Clearing one module's systems leaves every other owner's systems
    /// registered and still running, which is what makes per-module hot reload
    /// isolated from the project and from other modules.
    #[test]
    fn clearing_one_owner_leaves_other_owners_running() {
        let project_runs = Arc::new(AtomicUsize::new(0));
        let module_runs = Arc::new(AtomicUsize::new(0));

        let mut engine = Engine::new();
        let project_counter = Arc::clone(&project_runs);
        engine.register_system("project_system", move || {
            project_counter.fetch_add(1, AtomicOrdering::SeqCst);
        });

        let module_owner = SystemOwner::extension(0);
        engine.begin_module_registration(module_owner);
        let module_counter = Arc::clone(&module_runs);
        engine.register_system("module_system", move || {
            module_counter.fetch_add(1, AtomicOrdering::SeqCst);
        });
        engine.end_module_registration();

        engine.process_frame().unwrap();
        assert_eq!(project_runs.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(module_runs.load(AtomicOrdering::SeqCst), 1);

        // Reloading the module removes only its own system.
        assert_eq!(engine.clear_systems_owned_by(module_owner), 1);
        engine.process_frame().unwrap();
        assert_eq!(project_runs.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(module_runs.load(AtomicOrdering::SeqCst), 1);

        // The project keeps running after its own systems are cleared too.
        assert_eq!(engine.clear_systems_owned_by(SystemOwner::PROJECT), 1);
        engine.process_frame().unwrap();
        assert_eq!(project_runs.load(AtomicOrdering::SeqCst), 2);
    }

    /// Registers one counting system under `owner` and returns its counter.
    fn register_counting_system(
        engine: &mut Engine,
        owner: SystemOwner,
        name: &str,
    ) -> Arc<AtomicUsize> {
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&runs);
        engine.begin_module_registration(owner);
        engine.register_system(name.to_owned(), move || {
            counter.fetch_add(1, AtomicOrdering::SeqCst);
        });
        engine.end_module_registration();
        runs
    }

    /// Suspending an owner stops its systems and only its systems, in both the
    /// sequential and the parallel scheduler, and resuming restores them.
    #[test]
    fn suspending_an_owner_stops_only_its_systems() {
        for parallel in [false, true] {
            let mut engine = Engine::new();
            engine.set_parallel_execution(parallel);
            let module_owner = SystemOwner::extension(2);
            let project_runs =
                register_counting_system(&mut engine, SystemOwner::PROJECT, "project");
            let first_runs = register_counting_system(&mut engine, module_owner, "module_first");
            let second_runs = register_counting_system(&mut engine, module_owner, "module_second");

            assert_eq!(engine.set_systems_enabled_for_owner(module_owner, false), 2);
            assert!(!engine.is_owner_enabled(module_owner));
            engine.process_frame().unwrap();
            assert_eq!(
                project_runs.load(AtomicOrdering::SeqCst),
                1,
                "parallel={parallel}"
            );
            assert_eq!(
                first_runs.load(AtomicOrdering::SeqCst),
                0,
                "parallel={parallel}"
            );
            assert_eq!(
                second_runs.load(AtomicOrdering::SeqCst),
                0,
                "parallel={parallel}"
            );

            assert_eq!(engine.set_systems_enabled_for_owner(module_owner, true), 2);
            assert!(engine.is_owner_enabled(module_owner));
            engine.process_frame().unwrap();
            assert_eq!(
                project_runs.load(AtomicOrdering::SeqCst),
                2,
                "parallel={parallel}"
            );
            assert_eq!(
                first_runs.load(AtomicOrdering::SeqCst),
                1,
                "parallel={parallel}"
            );
            assert_eq!(
                second_runs.load(AtomicOrdering::SeqCst),
                1,
                "parallel={parallel}"
            );
        }
    }

    /// A system registered while its owner is suspended starts disabled, so a
    /// module generation that initializes during a suspension does not run
    /// early; resuming the owner starts it.
    #[test]
    fn systems_registered_during_a_suspension_start_disabled() {
        let mut engine = Engine::new();
        let module_owner = SystemOwner::extension(0);
        assert_eq!(engine.set_systems_enabled_for_owner(module_owner, false), 0);

        let runs = register_counting_system(&mut engine, module_owner, "late");
        assert_eq!(engine.is_system_enabled("late"), Some(false));
        engine.process_frame().unwrap();
        assert_eq!(runs.load(AtomicOrdering::SeqCst), 0);

        engine.set_systems_enabled_for_owner(module_owner, true);
        engine.process_frame().unwrap();
        assert_eq!(runs.load(AtomicOrdering::SeqCst), 1);
    }

    /// Clearing a suspended owner's systems still works, and the suspension
    /// survives it: the replacement generation's systems start disabled.
    #[test]
    fn a_suspension_survives_clearing_the_owners_systems() {
        let mut engine = Engine::new();
        let module_owner = SystemOwner::extension(1);
        register_counting_system(&mut engine, module_owner, "old_generation");
        engine.set_systems_enabled_for_owner(module_owner, false);

        assert_eq!(engine.clear_systems_owned_by(module_owner), 1);
        assert!(!engine.is_owner_enabled(module_owner));

        let runs = register_counting_system(&mut engine, module_owner, "new_generation");
        engine.process_frame().unwrap();
        assert_eq!(runs.load(AtomicOrdering::SeqCst), 0);

        engine.set_systems_enabled_for_owner(module_owner, true);
        engine.process_frame().unwrap();
        assert_eq!(runs.load(AtomicOrdering::SeqCst), 1);
    }

    /// Registration outside a module scope is attributed to the project, and
    /// scoping is restored after the module finishes registering.
    #[test]
    fn registration_scope_returns_to_the_project_owner() {
        let mut engine = Engine::new();
        engine.begin_module_registration(SystemOwner::extension(3));
        engine.register_system("owned_by_module", || {});
        engine.end_module_registration();
        engine.register_system("owned_by_project", || {});

        // Only the module's system is removed by clearing its owner.
        assert_eq!(engine.clear_systems_owned_by(SystemOwner::extension(3)), 1);
        assert_eq!(engine.is_system_enabled("owned_by_project"), Some(true));
        assert_eq!(engine.is_system_enabled("owned_by_module"), None);
    }

    /// Clearing an owner with no registered systems changes nothing, so a
    /// module that failed to register cannot disturb the rest of the schedule.
    #[test]
    fn clearing_an_unused_owner_is_a_no_op() {
        let mut engine = Engine::new();
        engine.register_system("project_system", || {});
        assert_eq!(engine.clear_systems_owned_by(SystemOwner::extension(7)), 0);
        assert_eq!(engine.is_system_enabled("project_system"), Some(true));
    }

    /// After removing one system of several, the scheduler's access patterns
    /// stay aligned with the engine's system list, so the surviving systems are
    /// still scheduled and executed.
    #[test]
    fn scheduler_stays_aligned_after_removing_a_middle_system() {
        let first_runs = Arc::new(AtomicUsize::new(0));
        let last_runs = Arc::new(AtomicUsize::new(0));

        let mut engine = Engine::new();
        let first_counter = Arc::clone(&first_runs);
        engine.register_system("first", move || {
            first_counter.fetch_add(1, AtomicOrdering::SeqCst);
        });

        let middle_owner = SystemOwner::extension(1);
        engine.begin_module_registration(middle_owner);
        engine.register_system("middle", || {});
        engine.end_module_registration();

        let last_counter = Arc::clone(&last_runs);
        engine.register_system("last", move || {
            last_counter.fetch_add(1, AtomicOrdering::SeqCst);
        });

        engine.clear_systems_owned_by(middle_owner);
        engine.process_frame().unwrap();

        assert_eq!(first_runs.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(last_runs.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(engine.is_system_enabled("middle"), None);
    }

    /// A paused engine skips the system phase but keeps accepting frames, and
    /// `request_single_step` runs exactly one more phase before pausing again.
    #[test]
    fn paused_engine_skips_systems_but_step_runs_exactly_one_phase() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_system = Arc::clone(&counter);
        let mut engine = Engine::new();
        engine.register_system("counter", move || {
            counter_system.fetch_add(1, AtomicOrdering::SeqCst);
        });

        engine.process_frame().unwrap();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 1);

        // Paused: frames still process (tick bump, command flush) but no
        // system runs.
        engine.set_systems_paused(true);
        engine.process_frame().unwrap();
        engine.process_frame().unwrap();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 1);

        // One explicit step advances exactly one phase.
        engine.request_single_step();
        engine.process_frame().unwrap();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 2);
        engine.process_frame().unwrap();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 2);

        // Resuming runs systems again.
        engine.set_systems_paused(false);
        engine.process_frame().unwrap();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 3);
        assert!(!engine.systems_paused());
    }

    /// `system_snapshots` reports owners, enabled state, and name ambiguity,
    /// and `set_system_enabled_at` toggles exactly one of two same-named
    /// systems.
    #[test]
    fn system_snapshots_report_owners_and_toggle_by_index() {
        let mut engine = Engine::new();
        // This test is about project and module ownership and indexes the
        // snapshot list directly. The ECS report is opt-in and nothing here
        // turns it on; the call keeps the indices meaning what they would if a
        // project did so first.
        engine.set_ecs_diagnostics_interval(None);
        engine.register_system("duplicate", || {});
        engine.begin_module_registration(SystemOwner::extension(0));
        engine.register_system("duplicate", || {});
        engine.end_module_registration();
        engine.register_system("project_only", || {});

        let snapshots = engine.system_snapshots();
        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0].index, 0);
        assert_eq!(snapshots[0].owner, SystemOwner::PROJECT);
        assert_eq!(snapshots[1].owner, SystemOwner::extension(0));
        assert!(snapshots.iter().all(|system| system.enabled));
        assert!(snapshots[0].name_is_ambiguous);
        assert!(snapshots[1].name_is_ambiguous);
        assert!(!snapshots[2].name_is_ambiguous);

        // Toggle by index affects exactly the first same-named system.
        assert!(engine.set_system_enabled_at(0, false));
        assert!(!engine.set_system_enabled_at(99, false));
        let after = engine.system_snapshots();
        assert!(!after[0].enabled);
        assert!(after[1].enabled);
        // The name-keyed query sees the first (now disabled) registration.
        assert_eq!(engine.is_system_enabled("duplicate"), Some(false));
    }

    /// The ECS report is opt-in: a fresh engine registers nothing and the
    /// setter is the one call that registers it.
    ///
    /// The docs on `Engine::new`, the setter and the `diagnostics` module all
    /// promise this shape, so the test fails if either side drifts: registering
    /// the report while the engine is built breaks the first assertion, and a
    /// setter that no longer registers breaks the second.
    #[test]
    fn the_ecs_report_is_opt_in() {
        let report_is_registered = |engine: &Engine| {
            engine.system_snapshots().into_iter().any(|system| {
                system.name == crate::diagnostics::SYSTEM_NAME
                    && system.owner == SystemOwner::ENGINE
            })
        };

        let mut engine = Engine::new();
        assert!(
            !report_is_registered(&engine),
            "Engine::new registers no report; a project opts in"
        );

        engine.set_ecs_diagnostics_interval(Some(crate::diagnostics::DEFAULT_REPORT_INTERVAL));
        assert!(
            report_is_registered(&engine),
            "the setter registers the report it documents"
        );

        engine.set_ecs_diagnostics_interval(None);
        assert!(
            !report_is_registered(&engine),
            "None removes the report again"
        );
    }
}

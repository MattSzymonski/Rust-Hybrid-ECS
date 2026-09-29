//! The sequence every reload runs once its replacement image is loaded.
//!
//! # Responsibilities
//!
//! - Capture the retiring generation's persistable metadata.
//! - Clear its systems and initialize the new generation, rolling back on
//!   failure.
//! - Drop columns for types the new generation no longer registers.
//! - Re-home every native column onto a still-mapped generation.
//! - Migrate persistable schemas that changed across the swap.
//! - Retire the previous image into the bounded graveyard.
//!
//! # Design
//!
//! Extensions and the project ran this sequence as two separate 250-line
//! functions whose differences were almost entirely log wording. They differ in
//! exactly three ways, and only those three are expressed here:
//!
//! 1. An extension tags its registrations with its [`SystemOwner`], so a
//!    reload clears only its own systems; the project owns the scheduler
//!    outright and tags nothing.
//! 2. Their log wording differs, and not decoratively - the Python suites in
//!    `devops/tests` tell a project reload from a module reload by matching
//!    those exact phrases.
//! 3. Only a module's component names are reported onward, via [`ReloadCommit`],
//!    for the C# backend to expose; the project has no use for them.
//!
//! The first two are carried by [`ReloadSubjectKind`], so a caller states which
//! kind of subject it is and gets both behaviours together rather than wiring
//! them up separately.
//!
//! One difference was **not** preserved: the project path used to skip the
//! stranded-factory cleanup that runs when a new generation fails to initialize
//! after registering component types. That was an inconsistency rather than a
//! design decision, and unifying gives the project the cleanup too.
//!
//! **The order of the steps is load-bearing and must not be rearranged.**
//! `drop_forgotten_components` calls into the retiring image's drop glue, so it
//! has to run while that image is still mapped. `rehome_native_columns` then
//! re-points every column's function table at a generation that is still mapped.
//! Both must happen before the graveyard evicts anything, which is the only
//! reason a bound of two generations is sound. Reordering them is a
//! use-after-unmap that surfaces two reloads later, not immediately.

// Standard library
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::Instant;

// External crates
use pill_core::error::BuildError;
use pill_core::{debug, error, info, warn};
use pill_engine::{ComponentId, Engine, EngineApi, SystemOwner, World};

// Current crate
use crate::analytics;
use crate::native_library::NativeLibrary;

/// Consecutive deferred evictions tolerated before the warning becomes an
/// error.
///
/// The eviction gate is correct to defer while a column still references a
/// retired image, but a gate that defers forever is a slow address-space leak
/// wearing a warning. Escalating makes a genuinely stuck state visible while
/// there is still room to act on it, without turning the ordinary one-off
/// deferral into noise.
const MAX_DEFERRED_EVICTIONS: u32 = 3;

/// Maximum number of retired generations kept mapped per subject.
///
/// The immediately previous generation must stay mapped because engine-owned
/// pointers may still refer to its code; anything older can be evicted, because
/// the two passes above have re-pointed everything that could reach it.
pub(crate) const MAX_GRAVEYARD_GENERATIONS: usize = 2;

/// Completion line for a project reload.
///
/// The four constants below are an interface, not prose. `devops/core/
/// suite_common.py` and `devops/tests/test_hot_reload_suite.py` tell a project
/// reload from a module reload by grepping host output for these exact
/// phrases, and `devops/tests/test_log_contract.py` fails if either member of
/// a pair stops appearing verbatim in Rust source. That is why the project and
/// module wordings are spelled out separately instead of sharing one message
/// with an interpolated noun: an interpolated message is not greppable, so
/// nothing would catch it being reworded.
const PROJECT_RELOAD_COMPLETE: &str = "hot reload complete";

/// Completion line for an extension reload.
const MODULE_RELOAD_COMPLETE: &str = "extension hot reload complete";

/// Forgotten-type warning for a project reload.
const PROJECT_FORGOTTEN_TYPES: &str = "component type(s) no longer registered by the project; their data stays in the world but is orphaned (the new generation cannot read it)";

/// Forgotten-type warning for an extension reload.
const MODULE_FORGOTTEN_TYPES: &str = "component type(s) no longer registered by this module; their data stays in the world but is orphaned (the new generation cannot read it)";

/// Which kind of subject a reload is running for.
///
/// Carries the two differences that are not about the subject's identity: how
/// its registrations are scoped, and the exact wording of the two log lines the
/// Python suites match on. Both belong together, because a caller that gets one
/// right and the other wrong produces a reload that behaves correctly and is
/// reported as the wrong subject.
pub(crate) enum ReloadSubjectKind {
    /// The game project: owns the scheduler outright, and is the only subject
    /// whose completion line the migration and auto-reload suites match on.
    Project,
    /// An extension: contributes systems alongside the project
    /// and every other module, so its registrations are tagged with its owner.
    Extension,
}

impl ReloadSubjectKind {
    /// Whether registrations made during `init` are tagged with the subject's
    /// owner, so a later reload clears only that subject's systems.
    ///
    /// The project owns the scheduler outright and tags nothing.
    fn scopes_registration(&self) -> bool {
        matches!(self, Self::Extension)
    }

    /// Final log line emitted once the swap succeeds.
    fn completion_message(&self) -> &'static str {
        match self {
            Self::Project => PROJECT_RELOAD_COMPLETE,
            Self::Extension => MODULE_RELOAD_COMPLETE,
        }
    }

    /// Warning emitted when the new generation stops registering a type whose
    /// data is still in the world.
    fn forgotten_types_warning(&self) -> &'static str {
        match self {
            Self::Project => PROJECT_FORGOTTEN_TYPES,
            Self::Extension => MODULE_FORGOTTEN_TYPES,
        }
    }

    /// Failure line for a build that produced no usable replacement.
    ///
    /// Kept per-kind rather than interpolated: the Python suites match the
    /// project's wording verbatim, and `test_log_contract.py` fails if a
    /// sourced token stops appearing in Rust.
    fn build_failed_message(&self) -> &'static str {
        match self {
            Self::Project => "build failed; keeping the old project module",
            Self::Extension => "build failed; keeping the old module generation",
        }
    }

    /// Failure line for a replacement that could not be loaded.
    fn load_failed_message(&self) -> &'static str {
        match self {
            Self::Project => "failed to load the new library; keeping the old project module",
            Self::Extension => "failed to load the new library; keeping the old module generation",
        }
    }

    /// Failure line for a replacement the load-time validation refused.
    fn rejected_message(&self) -> &'static str {
        match self {
            // The project is built from this workspace by this host, so its
            // ABI is guaranteed by construction and this arm is unreachable;
            // it exists for totality.
            Self::Project => "rejected the new library; keeping the current generation",
            Self::Extension => "rejected the new library; keeping the old module generation",
        }
    }
}

/// One subject's state, borrowed for the length of a reload.
///
/// Built fresh per reload rather than stored: it borrows the caller's library
/// slot and graveyard mutably, and only for the transaction.
/// Whether a subject's retired generations are ever unmapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GraveyardPolicy {
    /// Keep at most [`MAX_GRAVEYARD_GENERATIONS`] retired images mapped and
    /// evict the oldest past that, once no column still needs its tables.
    Bounded,
    /// Never unmap a retired image.
    ///
    /// For a subject whose images leave pointers behind that nothing here can
    /// track or release. The renderer module is one: wgpu, the graphics driver
    /// and the `tracing` callsite registry in the shared `pill_core.dll` all
    /// keep state that points into the image that created it, and a renderer
    /// whose oldest generation was evicted hung the host's frame loop. The cost
    /// is one mapped image per reload, which a development session can afford.
    KeepAll,
}

pub(crate) struct ReloadTransaction<'a> {
    /// Name used in every log line and analytics record.
    pub(crate) subject: &'a str,
    /// Which kind of subject this is, which fixes its log wording and whether
    /// its registrations are scoped.
    pub(crate) kind: ReloadSubjectKind,
    /// Which systems this reload is allowed to clear.
    pub(crate) owner: SystemOwner,
    /// The generation being retired; still mapped for the whole transaction.
    pub(crate) current: &'a mut NativeLibrary,
    /// Previously retired generations, still mapped.
    pub(crate) old_libraries: &'a mut Vec<NativeLibrary>,
    /// Persistable type names the previous `init` registered.
    pub(crate) registered_type_names: &'a mut Vec<String>,
    /// Resource ids the previous generation of this subject registered.
    ///
    /// Compared against what the new generation registers, so a resource type
    /// the subject has stopped owning can be dropped while the image holding
    /// its drop function is still mapped.
    pub(crate) registered_resource_ids: &'a mut Vec<pill_engine::ResourceId>,
    /// Whether retired generations may be unmapped.
    pub(crate) graveyard_policy: GraveyardPolicy,
}

/// What a committed reload registered, for the caller to record.
pub(crate) struct ReloadCommit {
    /// Every component type name the new generation registered, plain or
    /// persistable. The C# backend needs this to expose a module's native
    /// components; the project has no use for it.
    pub(crate) exposed_component_names: Vec<String>,
}

/// What one generation's `init` registered, while its image was mapped.
pub(crate) struct GenerationInit {
    /// Status the artifact's entry point returned; zero means success.
    pub(crate) status: u32,
    /// Persistable component type names this generation registered, for
    /// forgotten-type detection on the next reload.
    pub(crate) registered_type_names: Vec<String>,
    /// Resource ids this generation claimed, so retiring it releases exactly
    /// those and no other subject's.
    pub(crate) registered_resource_ids: Vec<pill_engine::ResourceId>,
    /// Every component type name registered - plain and persistable alike -
    /// for the C# backend's bindings.
    pub(crate) component_names: Vec<String>,
}

/// Capture, initialize and record one native generation.
///
/// The shared half of a subject's `start`. The project and an extension run
/// exactly the same sequence around their entry point: take the registration
/// sequences first, scope the registration when the subject uses a scope,
/// invoke the entry point under a timer, then either record what the
/// generation registered or - on a non-zero status - release everything the
/// failed image owns while it is still mapped.
///
/// `scope` is the owner registrations are tagged with, or `None` for a subject
/// that owns the scheduler outright. `clearing_owner` names the systems a
/// failed init removes, which is the subject's own owner either way.
pub(crate) fn initialize_generation(
    engine: &mut Engine,
    engine_api: &EngineApi,
    subject: &str,
    scope: Option<SystemOwner>,
    clearing_owner: SystemOwner,
    library: &NativeLibrary,
    first_load_failure: FirstLoadFailure,
) -> GenerationInit {
    // Step 1: Sequences before the call, so `since` compares this generation
    // against the state just before it ran and not against zero - the log
    // accumulates across every artifact, and `since(0)` would claim the
    // engine's own resources for this subject.
    let persist_sequence = engine.world().persist_registration_sequence();
    let component_sequence = engine.world().component_registration_sequence();
    let resource_sequence = engine.world().resource_registration_sequence();

    // Step 2: The entry point, under the subject's registration scope when it
    // has one.
    let status = {
        let started = Instant::now();
        let status = match scope {
            Some(owner) => {
                engine.begin_module_registration(owner);
                let status = library.call_init(engine_api);
                engine.end_module_registration();
                status
            }
            None => library.call_init(engine_api),
        };
        analytics::record_init(subject, started.elapsed().as_secs_f64() * 1000.0);
        status
    };

    if status != 0 {
        match first_load_failure {
            FirstLoadFailure::ClearWorld => {
                clear_failed_generation(engine, subject, clearing_owner)
            }
            FirstLoadFailure::ClearSystemsOnly => {
                let removed = engine.clear_systems_owned_by(clearing_owner);
                info!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = subject,
                    removed_systems = removed,
                    "cleared the failed generation's systems and kept the world"
                );
            }
        }
        return GenerationInit {
            status,
            registered_type_names: Vec::new(),
            registered_resource_ids: Vec::new(),
            component_names: Vec::new(),
        };
    }

    // Step 3: What this generation owns, claimed in the world too so one
    // subject's retirement cannot take a value another subject shares.
    let registered_resource_ids = engine
        .world()
        .resource_ids_registered_since(resource_sequence);
    engine
        .world_mut()
        .retain_resource_claims(&registered_resource_ids);
    GenerationInit {
        status,
        registered_type_names: engine
            .world()
            .persist_type_names_registered_since(persist_sequence),
        registered_resource_ids,
        component_names: engine
            .world()
            .registered_component_names_since(component_sequence),
    }
}

/// What a failed first load of an artifact costs the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FirstLoadFailure {
    /// Replace the whole world ([`clear_failed_generation`]). The only sound
    /// answer for an artifact that may own data: its columns and resources
    /// carry drop glue from the image about to be unmapped.
    ClearWorld,
    /// Clear only the artifact's systems and keep the world.
    ///
    /// Sound only together with two promises the caller keeps: the artifact
    /// registers no data (the renderer module's components, assets and
    /// resources are `pill_renderer_api`'s, registered by the host), and the
    /// failed image is never unmapped, so anything it did touch still points
    /// at mapped code. The renderer module first loads when the window opens,
    /// after the project has filled the world, which is why wiping the world
    /// there would cost the scene.
    ClearSystemsOnly,
}

/// Release everything one failed generation owns, while its image is mapped.
///
/// The failed image's systems are `Box<dyn System>` trait objects, and its
/// data - resources it inserted, columns its entities live in - carries drop
/// glue from the same image. Systems first, then the world by replacement: the
/// replacement's drop is what runs every one of those functions, and both
/// have to run before the image can be unmapped.
fn clear_failed_generation(engine: &mut Engine, subject: &str, owner: SystemOwner) {
    engine.clear_systems_owned_by(owner);
    let abandoned = std::mem::replace(engine.world_mut(), World::new());
    let resources = abandoned.resource_count();
    drop(abandoned);
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        module = subject,
        resources,
        "cleared the world before unmapping the generation that failed to initialize"
    );
}

/// What to verify about a freshly loaded replacement before it is committed.
pub(crate) enum LoadValidation {
    /// Nothing: this host built the subject from the workspace it owns, so the
    /// contract is guaranteed by construction.
    None,
    /// The optional module ABI revision export. Extensions are workspace
    /// members any `cargo build` can refresh, so the revision is checked
    /// rather than assumed.
    ModuleAbi,
}

/// Build, load and commit one native generation, for either subject.
///
/// The project and an extension perform the same three steps in the same
/// order, with the same requirement that a failure anywhere leaves the current
/// generation running: compile (`build`), load a private copy, then swap
/// through [`ReloadTransaction`]. Their differences - which build function to
/// call, whether the ABI check applies, and the exact refusal wording the
/// Python suites match - are parameters, not a second lifecycle.
///
/// `before_commit` runs once the replacement has built, loaded and passed
/// validation, immediately before the swap - and not at all when an earlier
/// step refused it. It is where a subject releases state that must be dropped
/// while the retiring image is still mapped and that the transaction itself
/// does not know about: the renderer module detaches its GPU backend there.
///
/// Returns the commit when the swap happened, `None` when anything was
/// refused.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_load_and_commit(
    engine: &mut Engine,
    engine_api: &EngineApi,
    workspace_root: &Path,
    build: impl FnOnce(Option<(&AtomicU64, u64)>) -> Result<PathBuf, BuildError>,
    cancel_flag: Option<(&AtomicU64, u64)>,
    validation: LoadValidation,
    before_commit: &mut dyn FnMut(),
    transaction: ReloadTransaction<'_>,
) -> Option<ReloadCommit> {
    let subject = transaction.subject;
    let kind = &transaction.kind;

    // Step 1: Compile before touching engine state, so a compiler error can
    // never remove the systems of the working generation. A newer save during
    // the build cancels it and the next frame retries.
    let output_path = match build(cancel_flag) {
        Ok(path) => path,
        Err(error) => {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = subject,
                error = %error,
                "{}", kind.build_failed_message()
            );
            return None;
        }
    };

    // Step 2: Load and validate the replacement transactionally, leaving the
    // active generation untouched until it is ready to initialize.
    let new_library = match NativeLibrary::load_copy(&output_path, workspace_root, subject) {
        Ok(library) => library,
        Err(error) => {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = subject,
                error = %error,
                "{}", kind.load_failed_message()
            );
            return None;
        }
    };
    if let LoadValidation::ModuleAbi = validation {
        if let Err(error) = new_library.check_module_abi(subject) {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = subject,
                error = %error,
                "{}", kind.rejected_message()
            );
            return None;
        }
    }

    // Step 3: The shared transaction, whose own step order is load-bearing.
    before_commit();
    transaction.commit(engine, engine_api, new_library)
}

impl ReloadTransaction<'_> {
    /// Begin the new generation's registration scope, when this subject uses one.
    fn begin_registration(&self, engine: &mut Engine) {
        if self.kind.scopes_registration() {
            engine.begin_module_registration(self.owner);
        }
    }

    /// End it again.
    fn end_registration(&self, engine: &mut Engine) {
        if self.kind.scopes_registration() {
            engine.end_module_registration();
        }
    }

    /// Park a library in the graveyard, evicting the oldest generation when
    /// the bound is crossed - but only once no surviving column still needs
    /// its tables.
    ///
    /// A retired image is never unmapped at the moment it stops being current:
    /// pointers into it - storage tables, patch stubs, persist metadata - can
    /// still be live. The eviction log line is what identifies a later fault
    /// as belonging to an image released here rather than somewhere else.
    ///
    /// The `world` argument turns "a bound of two generations is sound" into a
    /// checked claim: a column whose component id has no factory left still
    /// holds a function table from whichever image created it, so a non-zero
    /// [`World::columns_without_factory`] count defers the unmap with a warning
    /// rather than risking a call into freed memory.
    fn retire_library(&mut self, library: NativeLibrary, world: &World) {
        self.old_libraries.push(library);
        if self.graveyard_policy == GraveyardPolicy::KeepAll {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                subject = self.subject,
                generations = self.old_libraries.len(),
                "keeping every retired generation mapped"
            );
            return;
        }
        if self.old_libraries.len() > MAX_GRAVEYARD_GENERATIONS {
            let orphaned = world.columns_without_factory();
            if orphaned != 0 {
                let deferrals = Self::deferred_eviction_count(self.subject, true);
                // One deferral is ordinary - a column is mid-migration and the
                // next reload clears it. A run of them is not: nothing is
                // releasing those columns, so every future reload adds a mapped
                // image that can never be evicted.
                if deferrals >= MAX_DEFERRED_EVICTIONS {
                    error!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        subject = self.subject,
                        generations = self.old_libraries.len(),
                        columns = orphaned,
                        consecutive_deferrals = deferrals,
                        "eviction has been deferred repeatedly: native columns still reference retired images and are not being released, so every reload now leaks a mapped image"
                    );
                } else {
                    warn!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        subject = self.subject,
                        generations = self.old_libraries.len(),
                        columns = orphaned,
                        "deferring eviction: native columns still reference a retired image"
                    );
                }
                return;
            }
            // The gate opened, so whatever held those columns is gone.
            Self::deferred_eviction_count(self.subject, false);
            // Dropping the evicted generation unmaps its image and deletes its
            // temporary file on disk. Logged before the drop: anything still
            // holding a pointer into that image faults inside it, and this line
            // is what tells that apart from a crash somewhere else.
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                subject = self.subject,
                generations = self.old_libraries.len(),
                "evicting the oldest retired generation"
            );
            drop(self.old_libraries.remove(0));
        }
    }

    /// Count consecutive deferred evictions for one subject.
    ///
    /// `defer` increments and returns the new count; clearing resets it to zero
    /// and returns zero. Keyed by subject because the project and each extension
    ///  retire independently, and one stuck subject should not mask or be
    /// masked by another.
    fn deferred_eviction_count(subject: &str, defer: bool) -> u32 {
        use std::collections::HashMap;
        use std::sync::Mutex;
        static COUNTS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
        let mut guard = COUNTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counts = guard.get_or_insert_with(HashMap::new);
        if !defer {
            counts.remove(subject);
            return 0;
        }
        let entry = counts.entry(subject.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Run the whole sequence against an already-loaded replacement.
    ///
    /// Returns `None` when the new generation failed to initialize and the
    /// previous one was restored, in which case nothing was swapped and the
    /// caller keeps running what it had.
    pub(crate) fn commit(
        mut self,
        engine: &mut Engine,
        engine_api: &EngineApi,
        new_library: NativeLibrary,
    ) -> Option<ReloadCommit> {
        // Step 3: Capture the retiring generation's persistable metadata before
        // its registrations are replaced. The new init below re-registers the
        // same type names, so without this capture the old serializer pointers
        // would be lost before migration could use them. The previous DLL stays
        // mapped (Step 6), which keeps those function pointers valid.
        let previous_metadata_by_name = engine.world().capture_persist_type_metadata();
        let previous_manifest = engine.world().persist_type_manifest();
        // The resource half of the same capture, and needed for the same
        // reason: a resource's serializer is monomorphized for the shape the
        // retiring build declared, and it is the only code that can read the
        // stored value once the arriving build changes that shape.
        let previous_resource_manifest = engine.world().persist_resource_manifest();

        // Step 4: Swap the systems. Only the systems this subject owns are
        // removed, so everything else in the scheduler keeps running across the
        // swap - the project and the other modules when a module reloads, the
        // modules when the project reloads.
        let removed = engine.clear_systems_owned_by(self.owner);
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = self.subject,
            removed_systems = removed,
            "cleared the retiring generation's systems"
        );

        let init_started = Instant::now();
        // Capture the registration sequence before init so the types this new
        // generation registered can be compared against the previous ones.
        let registration_sequence = engine.world().persist_registration_sequence();
        let component_registration_sequence = engine.world().component_registration_sequence();
        let resource_registration_sequence = engine.world().resource_registration_sequence();
        // Entities alive before the incoming generation's init. Migration
        // converts only these: anything init spawns already carries the new
        // schema, and the retiring serializer would misread it while a
        // same-layout column is rebuilt.
        let pre_swap_entities = engine.world().capture_live_entities();
        // Step 5: announce the retiring generation's persistable names. A
        // rebuilt image has a fresh `TypeId` for every type name it declares,
        // which at the registration site looks exactly like another binary
        // claiming the same name - and the world's collision guard refuses that
        // shape. Only the host knows these names belonged to the generation it
        // is replacing, so a reload that does not say so fails its own init and
        // rolls back.
        let retiring_type_names = self.registered_type_names.clone();
        engine
            .world_mut()
            .supersede_persist_registrations(&retiring_type_names);
        self.begin_registration(engine);
        let status = new_library.call_init(engine_api);
        self.end_registration(engine);
        engine.world_mut().clear_superseded_persist_registrations();
        analytics::record_init(self.subject, init_started.elapsed().as_secs_f64() * 1000.0);
        if status != 0 {
            // The replacement failed to register. Roll back to the previous
            // generation: init is required to be idempotent, so re-running it
            // restores the systems that were just cleared. No migration runs on
            // the rollback path because the old registrations are intact.
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.subject,
                status,
                "new generation failed to initialize; rolling back"
            );
            engine.clear_systems_owned_by(self.owner);

            // What the failed generation managed to register before giving up.
            // `clear_systems_owned_by` retires systems and their dispatch slots
            // but not component registrations, and a component's storage factory
            // holds function pointers into the image that registered it - the
            // one about to be retired at the end of this branch. Ids, not names:
            // the rollback generation re-registers the same names under fresh
            // `TypeId`s, so only the ids tell its entries apart from the failed
            // generation's leftovers.
            let failed_ids = engine
                .world()
                .registered_component_ids_since(component_registration_sequence);
            // The resource half of the same capture: the ids the failed
            // generation claimed, to be compared against what the rollback
            // generation re-claims below.
            let failed_resource_ids = engine
                .world()
                .resource_ids_registered_since(resource_registration_sequence);

            let rollback_sequence = engine.world().component_registration_sequence();
            let rollback_resource_sequence = engine.world().resource_registration_sequence();
            // Announced again: the failed generation consumed the marks for the
            // names it got as far as re-registering, and the rollback init
            // re-registers all of them.
            engine
                .world_mut()
                .supersede_persist_registrations(&retiring_type_names);
            self.begin_registration(engine);
            let rollback_status = self.current.call_init(engine_api);
            self.end_registration(engine);
            engine.world_mut().clear_superseded_persist_registrations();
            if rollback_status != 0 {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.subject,
                    status = rollback_status,
                    "rollback also failed; this module now contributes no systems"
                );
            }

            // Anything the rollback re-registered is safe: its fresh id holds
            // fresh factories pointing into the still-mapped generation. What
            // is left over is an id only the failed generation registered,
            // whose factory keeps pointing into the image being retired. Its
            // rows go too: the generated wrapper drains the registration-error
            // slot after the user's `init`, so a failing init can already have
            // spawned entities carrying the type - and those rows would be
            // dropped through an unmapped image at the next despawn.
            let rollback_ids = engine
                .world()
                .registered_component_ids_since(rollback_sequence);
            let stranded: Vec<ComponentId> = failed_ids
                .into_iter()
                .filter(|id| !rollback_ids.contains(id))
                .collect();
            if !stranded.is_empty() {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.subject,
                    count = stranded.len(),
                    ids = ?stranded,
                    "dropping registrations from the failed generation; their storage factories point into the image being retired"
                );
                let dropped = engine.world_mut().drop_forgotten_component_ids(&stranded);
                debug!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    dropped, "removed rows that belonged to the failed generation"
                );
            }
            // The purge above is meant to leave nothing listing a stranded id;
            // assert it so a future change that misses a path fails here, in a
            // debug build, rather than as a crash inside a retired image.
            debug_assert!(
                stranded
                    .iter()
                    .all(|id| !engine.world().any_archetype_lists_component(*id)),
                "a stranded component id still has an archetype entry after the purge"
            );

            // Resources need the stronger version of the same treatment. A
            // component column's factory is re-pointed by re-registration, but a
            // stored resource *value* carries its own drop function, so one the
            // failed generation inserted under an id the rollback generation
            // does not claim has to be dropped now - while the failing image is
            // still mapped - rather than left for the graveyard to invalidate.
            let rollback_resource_ids = engine
                .world()
                .resource_ids_registered_since(rollback_resource_sequence);
            let stranded_resources: Vec<pill_engine::ResourceId> = failed_resource_ids
                .into_iter()
                .filter(|id| !rollback_resource_ids.contains(id))
                .collect();
            if !stranded_resources.is_empty() {
                let dropped = engine.world_mut().drop_resources(&stranded_resources);
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.subject,
                    dropped,
                    "dropped resources claimed only by the failed generation"
                );
            }
            // Whatever survives still points its drop at whichever generation
            // supplied the current table, so re-home onto the rollback image the
            // same way the success path does. Every id the failed generation
            // touched has either been re-claimed (table refreshed by the rollback
            // init) or dropped just above, so no table still points into the
            // failing image.
            engine.world_mut().rehome_resources();
            // Asset columns follow the same rule: the rollback init re-declares
            // the asset types its generation owns, so their tables point back
            // at the image that stays mapped. A type only the failed generation
            // declared has no owner to re-claim it - the asset store keeps no
            // per-subject claims yet - so it keeps that generation's table.
            engine.world_mut().rehome_assets();
            // Retire the failed image instead of unmapping it. The purge above
            // is designed to leave nothing pointing into it, but "designed to"
            // is not a proof: an image that is parked costs a `FreeLibrary`
            // later, while one that is unmapped costs a crash if any pointer
            // survived. The graveyard's own bound is what eventually releases
            // it.
            self.retire_library(new_library, engine.world());
            return None;
        }

        // Detect persistable component types the new generation stopped
        // registering. Such data is NOT wiped by migration — the type is
        // absent from the changed-name set, so its column and metadata linger
        // while the new generation cannot read them. Surface it instead of
        // letting the type silently orphan.
        let newly_registered = engine
            .world()
            .persist_type_names_registered_since(registration_sequence);
        let all_registered = engine
            .world()
            .registered_component_names_since(component_registration_sequence);
        let forgotten_type_names: Vec<String> = self
            .registered_type_names
            .iter()
            .filter(|name| !newly_registered.iter().any(|current| current == *name))
            .cloned()
            .collect();
        if !forgotten_type_names.is_empty() {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.subject,
                forgotten_types = ?forgotten_type_names,
                "{}", self.kind.forgotten_types_warning()
            );

            // Drop the orphaned columns only for types the new generation does
            // not register at all (not even as a plain component). A type merely
            // downgraded from persistable to plain keeps live data, so its
            // columns must survive. This runs while the generation that last
            // registered the type is still mapped, so the drop is safe.
            let truly_forgotten: Vec<String> = forgotten_type_names
                .iter()
                .filter(|name| !all_registered.iter().any(|current| current == *name))
                .cloned()
                .collect();
            if !truly_forgotten.is_empty() {
                let dropped_entities = engine
                    .world_mut()
                    .drop_forgotten_components(&truly_forgotten);
                debug!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.subject,
                    dropped_entities,
                    "dropped orphaned columns for component types no longer registered"
                );
            }
        }
        *self.registered_type_names = newly_registered;

        // The resource twin of the block above. A resource holds a drop
        // function belonging to the artifact that inserted it, and nothing
        // clears resources on reload, so a type this subject has stopped
        // registering must be dropped here - while the retiring image is still
        // mapped - rather than left for the graveyard to invalidate.
        let claimed_now = engine
            .world()
            .resource_ids_registered_since(resource_registration_sequence);
        let retired: Vec<pill_engine::ResourceId> = self
            .registered_resource_ids
            .iter()
            .filter(|id| !claimed_now.contains(id))
            .copied()
            .collect();
        // The claim list is a delta between generations, so the world's claim
        // refcount moves by the same delta: ids this generation newly claims
        // gain a claim, and the retired ones lose theirs. `drop_resources`
        // then skips whatever another subject still claims - a shared resource
        // the project stopped registering must survive for a module that did
        // not stop.
        let newly_claimed: Vec<pill_engine::ResourceId> = claimed_now
            .iter()
            .filter(|id| !self.registered_resource_ids.contains(id))
            .copied()
            .collect();
        engine.world_mut().retain_resource_claims(&newly_claimed);
        if !retired.is_empty() {
            engine.world_mut().release_resource_claims(&retired);
            let dropped = engine.world_mut().drop_resources(&retired);
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.subject,
                dropped,
                "dropped resources for types this generation no longer registers"
            );
        }
        *self.registered_resource_ids = claimed_now;
        // Refresh the C#-exposed component set to the new generation's
        // registrations (plain and persistable alike).
        // Returned to the caller rather than stored: only a module
        // reports its component names onward to the C# backend.

        // Step 4b: Re-home every native storage column to the freshly loaded
        // generation's function table. Columns created by older generations
        // hold function pointers into their own DLL; refreshing them here (the
        // old DLLs are still mapped) keeps drops and upcasts valid when those
        // DLLs are later evicted from the reload graveyard.
        engine.world_mut().rehome_native_columns();

        // Step 4c: The same treatment for resources. A resource holds a drop
        // function belonging to the artifact that inserted it, and nothing
        // clears resources on reload, so without this a retiring generation's
        // image would be evicted while a live resource still pointed its
        // destructor into it.
        engine.world_mut().rehome_resources();

        // Step 4d: And for asset columns. The asset store outlives the reload
        // too, and a column's per-type table - drop, upcast, take - is code
        // from whichever artifact declared or first used the type. Re-pointing
        // it here keeps the store usable after that artifact's image leaves
        // the graveyard.
        engine.world_mut().rehome_assets();

        // Step 5: Migrate persistable schemas that changed across the swap.
        // Types are matched by stable name rather than runtime ComponentId,
        // which can differ between generations, so data follows a renamed or
        // reshaped component. Unchanged columns keep their allocations and
        // change-detection ticks, making the common reload path cheap.
        let migrate_started = Instant::now();
        let current_schema_by_name: HashMap<String, u64> = engine
            .world()
            .persist_type_manifest()
            .into_iter()
            .map(|entry| (entry.type_name, entry.schema_hash))
            .collect();
        let changed_type_names: HashSet<String> = previous_manifest
            .iter()
            .filter_map(|entry| {
                current_schema_by_name
                    .get(&entry.type_name)
                    .filter(|&&current_hash| current_hash != entry.schema_hash)
                    .map(|_| entry.type_name.clone())
            })
            .collect();

        if changed_type_names.is_empty() {
            // Avoid touching archetype storage when every persisted layout is
            // byte-for-byte compatible with the previous generation.
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.subject,
                "schema unchanged for all persistable component types - fast path"
            );
        } else {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.subject,
                changed_types = changed_type_names.len(),
                "migrating changed persistable module types"
            );
            let report = engine.world_mut().migrate_changed_persistable_components(
                &previous_metadata_by_name,
                &changed_type_names,
                Some(&pre_swap_entities),
            );
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.subject,
                migrated_types = report.migrated_type_count,
                migrated_entities = report.migrated_entity_count,
                "persistable migration complete"
            );
            if !report.skipped_type_names.is_empty() {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.subject,
                    skipped_types = ?report.skipped_type_names,
                    "migration skipped some module component types"
                );
            }
        }
        // Step 5a: Migrate reshaped resources. `rehome_resources` above swapped
        // each live resource's function table without touching its bytes, so a
        // resource whose fields changed would otherwise be read through the new
        // type over the old value's memory. Matched by persistence name, like
        // components, and run while the retiring image is still mapped because
        // the old serializer lives in it.
        let migrated_resources = engine
            .world_mut()
            .migrate_changed_persistable_resources(&previous_resource_manifest);
        if !migrated_resources.is_empty() {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.subject,
                migrated_resources = ?migrated_resources,
                "reshaped resources migrated"
            );
        }

        analytics::record_migrate(
            self.subject,
            migrate_started.elapsed().as_secs_f64() * 1000.0,
        );

        // Step 5b: Prove every surviving native column still owns a mapped
        // function table before the retiring image can ever be evicted. A
        // column whose id has no factory is the "same name, fresh TypeId" case
        // the name-keyed forgotten-type sweep cannot reach - the name resolves
        // to the new id - so it is dropped here, through the table of the
        // generation that produced it, while that generation is still mapped.
        let orphaned_columns = engine.world_mut().drop_columns_without_factory();
        if orphaned_columns != 0 {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.subject,
                orphaned_columns,
                "dropped native columns whose registration is gone"
            );
        }

        // Step 6: Retire the previous library without unmapping it. Component
        // operations and persist metadata registered by that generation may
        // still be referenced by engine-owned pointers.
        let previous_library = std::mem::replace(&mut *self.current, new_library);
        self.retire_library(previous_library, engine.world());

        analytics::record_reload(self.subject);

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = self.subject,
            entities = engine.world().entity_count(),
            graveyard = self.old_libraries.len(),
            "{}", self.kind.completion_message()
        );

        Some(ReloadCommit {
            exposed_component_names: all_registered,
        })
    }
}

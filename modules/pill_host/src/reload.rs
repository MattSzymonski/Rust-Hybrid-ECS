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
//! Optional modules and the project ran this sequence as two separate 250-line
//! functions whose differences were almost entirely log wording. They differ in
//! exactly three ways, and only those three are expressed here:
//!
//! 1. An optional module tags its registrations with its [`SystemOwner`], so a
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
use std::time::Instant;

// External crates
use pill_core::{debug, error, info, warn};
use pill_engine::{Engine, EngineApi, SystemOwner};

// Current crate
use crate::analytics;
use crate::native_library::NativeLibrary;

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

/// Completion line for an optional-module reload.
const MODULE_RELOAD_COMPLETE: &str = "optional module hot reload complete";

/// Forgotten-type warning for a project reload.
const PROJECT_FORGOTTEN_TYPES: &str = "component type(s) no longer registered by the project; their data stays in the world but is orphaned (the new generation cannot read it)";

/// Forgotten-type warning for an optional-module reload.
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
    /// An optional engine module: contributes systems alongside the project
    /// and every other module, so its registrations are tagged with its owner.
    OptionalModule,
}

impl ReloadSubjectKind {
    /// Whether registrations made during `init` are tagged with the subject's
    /// owner, so a later reload clears only that subject's systems.
    ///
    /// The project owns the scheduler outright and tags nothing.
    fn scopes_registration(&self) -> bool {
        matches!(self, Self::OptionalModule)
    }

    /// Final log line emitted once the swap succeeds.
    fn completion_message(&self) -> &'static str {
        match self {
            Self::Project => PROJECT_RELOAD_COMPLETE,
            Self::OptionalModule => MODULE_RELOAD_COMPLETE,
        }
    }

    /// Warning emitted when the new generation stops registering a type whose
    /// data is still in the world.
    fn forgotten_types_warning(&self) -> &'static str {
        match self {
            Self::Project => PROJECT_FORGOTTEN_TYPES,
            Self::OptionalModule => MODULE_FORGOTTEN_TYPES,
        }
    }
}

/// One subject's state, borrowed for the length of a reload.
///
/// Built fresh per reload rather than stored: it borrows the caller's library
/// slot and graveyard mutably, and only for the transaction.
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
}

/// What a committed reload registered, for the caller to record.
pub(crate) struct ReloadCommit {
    /// Every component type name the new generation registered, plain or
    /// persistable. The C# backend needs this to expose a module's native
    /// components; the project has no use for it.
    pub(crate) exposed_component_names: Vec<String>,
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

    /// Run the whole sequence against an already-loaded replacement.
    ///
    /// Returns `None` when the new generation failed to initialize and the
    /// previous one was restored, in which case nothing was swapped and the
    /// caller keeps running what it had.
    pub(crate) fn commit(
        self,
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
        self.begin_registration(engine);
        let status = new_library.call_init(engine_api);
        self.end_registration(engine);
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
            // one about to be dropped and unmapped at the end of this branch.
            let failed_registrations = engine
                .world()
                .registered_component_names_since(component_registration_sequence);

            let rollback_sequence = engine.world().component_registration_sequence();
            self.begin_registration(engine);
            let rollback_status = self.current.call_init(engine_api);
            self.end_registration(engine);
            if rollback_status != 0 {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.subject,
                    status = rollback_status,
                    "rollback also failed; this module now contributes no systems"
                );
            }

            // Anything the rollback re-registered is safe: registering a type
            // overwrites its factory with pointers into the still-mapped
            // generation. What is left over is a type only the failed generation
            // knew about, whose factory would keep pointing into an unmapped
            // image. No entity can carry such a type - the generation that
            // defines it never finished initialising - so this frees registry
            // entries rather than data.
            let rollback_registrations = engine
                .world()
                .registered_component_names_since(rollback_sequence);
            let stranded: Vec<String> = failed_registrations
                .into_iter()
                .filter(|name| !rollback_registrations.contains(name))
                .collect();
            if !stranded.is_empty() {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.subject,
                    types = stranded.join(", ").as_str(),
                    "dropping registrations from the failed generation; their storage factories point into the image being unmapped"
                );
                engine.world_mut().drop_forgotten_components(&stranded);
            }
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
        analytics::record_migrate(
            self.subject,
            migrate_started.elapsed().as_secs_f64() * 1000.0,
        );

        // Step 6: Retire the previous library without unmapping it. Component
        // operations and persist metadata registered by that generation may
        // still be referenced by engine-owned pointers.
        self.old_libraries
            .push(std::mem::replace(&mut *self.current, new_library));
        if self.old_libraries.len() > MAX_GRAVEYARD_GENERATIONS {
            // Dropping the evicted generation unmaps its image and deletes its
            // temporary file on disk.
            drop(self.old_libraries.remove(0));
        }

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

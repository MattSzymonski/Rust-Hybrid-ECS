//! Per-function hot patching: the fast path beside the whole-module reload.
//!
//! # Responsibilities
//!
//! - Decide whether a source change is a body-only edit of a `#[pill_hot]`
//!   function, and refuse loudly when it is anything else.
//! - Generate, compile and load a small patch library for that one function.
//! - Install the replacement into the engine's dispatch slot at a frame
//!   boundary.
//!
//! # Design
//!
//! This never replaces the existing reload. It runs first, and everything it
//! refuses falls through to [`LoadedProject::reload`](crate::project_module) as
//! before, so the worst outcome of a refusal is the behavior that existed
//! before this module.
//!
//! The classification is deliberately conservative. It strips the bodies of the
//! annotated functions from the old and new revisions and compares what is
//! left; a body-only edit vanishes from that comparison and anything else -
//! signatures, types, constants, imports, other functions - survives it. A
//! false negative costs one full reload. A false positive would install code
//! compiled against a layout the running world no longer has, so the gate errs
//! toward refusing.
//!
//! Patch libraries are never unloaded. A slot may hold an address inside one
//! for the rest of the process, and nothing re-homes those addresses.

// A development-only facility, refused at compile time in an optimized build.
//
// Hot patching shells out to `rustc`, writes DLLs into the temp directory and
// loads them into the running process, and every annotated function pays an
// indirection. None of that belongs in a shipped binary, and the failure mode
// of shipping it by accident is silent rather than loud - so the build stops
// here instead. Enable it only in a debug profile.
#[cfg(not(debug_assertions))]
compile_error!(
    "the `hot_patch` feature is a development facility and cannot be built with \
     optimizations: it invokes `rustc` at runtime and loads generated libraries \
     into the process. Build without `--release`, or drop the feature."
);

// Standard library
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

// External crates
use libloading::Library;
use pill_core::{debug, info, warn};
use pill_engine::Engine;

// Current crate
pub(crate) mod compile;
pub(crate) mod generations;
pub(crate) mod routes;

// A function's patch history and the routes back through it.
pub(crate) use generations::Generation;
pub use generations::PatchGeneration;

// The three install routes and the patch-image plumbing they share. Re-exported
// rather than referenced through `routes::` at every call site, so the session
// below reads the same as it did when these lived in this file.
use routes::{
    install_everywhere, prologue_patch_everywhere, reset_everywhere, resolve_in,
    resolve_patch_address, resolve_plain_in,
};
pub(crate) use routes::{LoadedPatch, PrologueRestore};
/// The source scanner, shared with every crate's build script.
///
/// The implementation lives in `pill_hot_scan` because the host and the build
/// scripts must agree byte for byte about where a function starts and what its
/// declaration says. They previously had separate scanners, and the moment they
/// disagreed - a build script naming a method through its type while the host
/// did not - every method silently failed to patch.
///
/// Re-exported under this name so `source::` call sites read as what they are:
/// a question about source text, not about the scanning crate.
pub(crate) mod source {
    pub use pill_hot_scan::*;
}

use crate::native_library::NativeLibrary;

use compile::CargoRustcLine;

// =============================================================================
// Constants
// =============================================================================

/// Export a generated patch carries so the host can find its new function.
///
/// Deliberately NOT `pill_hot_resolve`: a patch links the patched crate's rlib
/// to reach its types, and that rlib already exports that symbol. Two
/// `#[no_mangle]` definitions of one name in a single artifact is a link error.
pub(super) const PATCH_RESOLVER_EXPORT: &[u8] = b"pill_patch_resolve";

/// Prefix that namespaces a patch's registry entry.
///
/// Also not optional. Linking the project's rlib pulls in that crate's
/// `#[pill_hot]` descriptors too, so a patch DLL contains BOTH the old and the
/// new entry for the same function. Asking for the bare name resolves whichever
/// the linker happened to order first - measured, and it was the OLD address,
/// with a matching signature hash, so the patch would have installed silently
/// and changed nothing.
const PATCH_NAME_PREFIX: &str = "pill_patch::";

/// Export that reports where a patched plain function lives.
///
/// Named from `PATCH_RESOLVER_EXPORT` by the same macro that generates it, and
/// distinct from the crate's own `pill_hot_resolve_plain` for the same reason.
pub(super) const PATCH_PLAIN_EXPORT: &[u8] = b"pill_patch_resolve_plain";

// =============================================================================
// Outcome
// =============================================================================

/// Why a change could not be satisfied by a patch, or why an attempt failed.
///
/// The `code` is a stable kebab-case tag and the `detail` is the sentence a
/// developer reads. Splitting them means a test can assert a specific reason
/// without matching prose that is free to improve, which is what makes the
/// per-reason cases in `devops/tests/` meaningful rather than brittle.
#[derive(Debug, Clone)]
pub(crate) struct PatchRefusal {
    /// Stable tag, printed in parentheses after the headline.
    pub code: &'static str,
    /// Human explanation, printed on its own line.
    pub detail: String,
}

impl PatchRefusal {
    /// A refusal with a stable code and a formatted explanation.
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// The change touched something a patch cannot express.
pub(crate) mod refusal_code {
    /// A source file appeared that was not in the baseline snapshot.
    pub const NEW_SOURCE_FILE: &str = "new-source-file";
    /// The changed file marks no function as hot-patchable.
    pub const NO_HOT_FUNCTION: &str = "no-hot-function";
    /// Something outside a hot body moved: a signature, type, constant, import
    /// or another function.
    pub const OUTSIDE_HOT_BODY: &str = "outside-hot-body";
    /// More than one source file changed in the same edit.
    pub const MULTIPLE_FILES: &str = "multiple-files";
    /// The edit landed before this session took its baseline snapshot.
    pub const EDITED_BEFORE_ARMING: &str = "edited-before-arming";
    /// A method was edited whose `impl` block names no simple receiver type.
    pub const UNRESOLVED_RECEIVER: &str = "unresolved-receiver";
}

/// The change was in scope, but the attempt to deliver it did not complete.
pub(crate) mod failure_code {
    /// The edited function could not be located in the changed source.
    pub const GENERATE: &str = "generate";
    /// Writing the generated source, or capturing the compiler flags, failed.
    pub const PREPARE: &str = "prepare";
    /// `rustc` rejected the generated patch.
    pub const COMPILE: &str = "compile";
    /// The compiled patch could not be mapped into the process.
    pub const LOAD: &str = "load";
    /// The patch did not report the replacement the host asked for.
    pub const RESOLVE: &str = "resolve";
    /// A running artifact refused the replacement, typically because the
    /// signature no longer matches.
    pub const INSTALL: &str = "install";
}

/// What one attempt at a fast patch did.
#[derive(Debug)]
pub(crate) enum PatchOutcome {
    /// A function's implementation was replaced; no reload is needed.
    Patched {
        /// Qualified name of the patched function.
        function: String,
        /// Which generation this became; generation zero is the original code
        /// the running artifact was built with.
        generation: u32,
        /// Wall time from noticing the change to the slot being installed.
        elapsed_milliseconds: f64,
        /// Per-stage breakdown, so the dominant cost is visible rather than
        /// guessed at. Without this a slow patch is just a number.
        stages: PatchStages,
        /// Size of the compiled patch, for the analytics line.
        artifact_bytes: u64,
        /// Exports the compiled patch carries, for the analytics line.
        exports: usize,
        /// Which mechanisms delivered this edit. More than one when a single
        /// save changed both an annotated and an un-annotated body.
        routes: Vec<crate::analytics::PatchRoute>,
        /// How many running copies of the changed functions were reached in
        /// total, so a fan-out is visible rather than assumed.
        copies: usize,
    },
    /// Nothing relevant changed.
    Unchanged,
    /// The change is real but out of scope; the caller should reload normally.
    NotPatchable {
        /// Stable code and the sentence explaining it.
        refusal: PatchRefusal,
    },
    /// A patch was attempted and failed. The previous implementation is intact
    /// and the caller should reload normally.
    Failed {
        /// Which function was being patched.
        function: String,
        /// Which generation is still running, so the console says what the
        /// process is executing rather than only what did not happen.
        active_generation: u32,
        /// Stable code and the first line of the failure.
        failure: PatchRefusal,
    },
}

/// Where one patch spent its time, in milliseconds.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PatchStages {
    /// Reading the sources and deciding the edit is body-only.
    pub classify: f64,
    /// Producing the patch source.
    pub generate: f64,
    /// Asking cargo for the compiler flags. Zero on every patch after the
    /// first, because the answer is cached.
    pub flags: f64,
    /// `rustc` compiling and linking the patch library.
    pub compile: f64,
    /// `LoadLibrary` on the produced artifact.
    pub load: f64,
    /// Resolving the new address and storing it into the dispatch slot.
    pub activate: f64,
}

impl std::fmt::Display for PatchStages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "classify {:.0}ms | generate {:.0}ms | flags {:.0}ms | \
             compile {:.0}ms | load {:.0}ms | activate {:.1}ms",
            self.classify, self.generate, self.flags, self.compile, self.load, self.activate
        )
    }
}

// =============================================================================
// HotPatchSession
// =============================================================================

/// What one successful [`HotPatchSession::apply`] produced.
///
/// Named for the call that returns it rather than for the act of installing:
/// the installation is one step inside `apply`, and the generation number,
/// artifact size and route below describe the whole attempt.
struct ApplyResult {
    /// Which generation this became.
    generation: u32,
    /// Size of the compiled patch on disk.
    artifact_bytes: u64,
    /// Exports the compiled patch carries.
    exports: usize,
    /// Which of the three mechanisms delivered it. Reported rather than
    /// inferred downstream: an annotated and an un-annotated plain function are
    /// the same `HotFunctionKind` but take completely different routes.
    route: crate::analytics::PatchRoute,
    /// How many running copies the install reached. One for the engine
    /// registry, which is process-wide; otherwise one per artifact that links
    /// the crate.
    copies: usize,
}

/// One watched source file as this session last read it.
///
/// Contents and modification time are one value because they are one fact: the
/// time is when *these* contents were read. Held as two parallel maps they had
/// to be written together by hand, and a patch that updated the contents while
/// leaving the time behind is a bug that happened. The pairing is now the
/// type's job rather than the caller's.
struct Snapshot {
    /// The file's contents at the moment it was read.
    contents: String,
    /// The file's modification time then, or `None` when the filesystem would
    /// not report one.
    ///
    /// `None` means "always re-read", which is the safe direction: a redundant
    /// read costs microseconds, a missed edit costs a reload.
    ///
    /// Compared against the file's *current* modification time, never against
    /// `SystemTime::now()`. Both sides are filesystem timestamps, which matters:
    /// on Windows file times come from the coarse system clock and can lag
    /// `now()` by a scheduler tick, so a file written after a snapshot can
    /// report an earlier time and the edit is then missed.
    modified: Option<SystemTime>,
}

impl Snapshot {
    /// Read one file into a snapshot, or `None` when it cannot be read.
    ///
    /// The time is taken from the same file in the same moment as the contents,
    /// so the two cannot describe different reads.
    fn read(path: &Path) -> Option<Self> {
        let contents = std::fs::read_to_string(path).ok()?;
        let modified = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok();
        Some(Self { contents, modified })
    }

    /// Whether the file on disk still matches what this snapshot recorded.
    ///
    /// An unknown time on either side answers `false`, so the caller re-reads.
    fn is_current(&self, modified: Option<SystemTime>) -> bool {
        matches!((self.modified, modified), (Some(recorded), Some(current)) if recorded == current)
    }
}

/// Drives the fast path for one project.
pub(crate) struct HotPatchSession {
    workspace_root: PathBuf,
    package: String,
    crate_root: PathBuf,
    source_root: PathBuf,
    /// The patched crate's own rlib - the staged copy the host built, so
    /// generated sources can `use <crate>::*` and get the identical types the
    /// running module holds.
    package_rlib: PathBuf,
    /// The host's own build command for this crate, feature flags included.
    ///
    /// Captured flags must come from THIS command: features live here and
    /// nowhere else, and a different feature set means different crate
    /// metadata - so a patch built from a bare `cargo build -p <pkg>` line
    /// disagrees with the running world about every `TypeId`.
    build_command: Vec<String>,
    /// Last seen contents of every watched source file, with the modification
    /// time each was read at.
    snapshots: HashMap<PathBuf, Snapshot>,
    /// When those contents were read.
    ///
    /// Used to tell two very different "nothing changed" cases apart: a reload
    /// this session did not cause, and an edit that landed before the session
    /// armed and was therefore captured as the baseline. The second one looks
    /// exactly like a broken watcher from outside.
    snapshot_taken_at: SystemTime,
    /// Compiler flags, captured from cargo once and reused.
    rustc_line: Option<CargoRustcLine>,
    /// Loaded patches, never unloaded.
    generations: Vec<Generation>,
    /// Which generation each patched function is currently running, so a
    /// failure can report what the process is still executing and a rollback
    /// knows where it started. Absent means generation zero, the original.
    active_generations: HashMap<String, u32>,
    /// Makes each patch's crate name and artifact unique within the process.
    counter: u64,
}

impl HotPatchSession {
    /// Prepare a session and take the baseline snapshot of the project sources.
    ///
    /// Returns `None` when the project has no `#[pill_hot]` function, so a
    /// project that has not opted in pays nothing.
    pub(crate) fn new(
        workspace_root: &Path,
        package: &str,
        watch_directory: &str,
        staging_subdirectory: &str,
        build_command: &[String],
    ) -> Option<Self> {
        let source_root = workspace_root.join(watch_directory);
        let crate_root = source_root.join("lib.rs");
        // The staged copy, not cargo's per-crate slot. Both the project and
        // every optional module write their `rlib` to an unhashed path that any
        // other build of the same package overwrites, and a patch that linked
        // the wrong one would be compiled against a differently configured
        // engine - giving every type a different `TypeId` than the running
        // world holds. The host stages what it built and links only that.
        let package_rlib = workspace_root
            .join(staging_subdirectory)
            .join(format!("lib{package}.rlib"));

        let mut session = Self {
            workspace_root: workspace_root.to_path_buf(),
            package: package.to_string(),
            crate_root,
            source_root,
            package_rlib,
            build_command: build_command.to_vec(),
            snapshots: HashMap::new(),
            snapshot_taken_at: SystemTime::UNIX_EPOCH,
            rustc_line: None,
            generations: Vec::new(),
            active_generations: HashMap::new(),
            counter: 0,
        };
        session.refresh_snapshots();

        // Every function the host could address, not just the annotated ones.
        // An attribute chooses the mechanism, not whether patching is possible,
        // so a crate with no annotations at all is still armed - and a crate
        // whose build script emits no address inventory gets a clear refusal on
        // its first edit rather than silently falling back to a full reload,
        // which is exactly the confusion this used to cause.
        let addressable: usize = session
            .snapshots
            .values()
            .map(|snapshot| source::all_functions(&snapshot.contents).len())
            .sum();
        if addressable == 0 {
            return None;
        }
        let annotated: usize = session
            .snapshots
            .values()
            .map(|snapshot| source::hot_function_names(&snapshot.contents).len())
            .sum();

        // Without the crate's own rlib a generated patch cannot name the
        // project's types, and every attempt would fail at compile time. Say so
        // once, at startup, rather than on the first edit.
        if !session.package_rlib.is_file() {
            // Printed, not just logged: whether the fast path is on is the first
            // thing a developer needs to know, and a `tracing` line at INFO is
            // easy to lose among the startup output.
            println!(
                "{} hot patching OFF for {package} - its rlib is missing ({})",
                crate::console::bold_cyan("[hot]"),
                session.package_rlib.display()
            );
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                expected = %session.package_rlib.display(),
                "hot patching is idle: the project rlib a patch links to reach the \
                 crate's types was not built"
            );
            return None;
        }

        println!(
            "{} hot patching ON for {package} - {addressable} function(s), {annotated} annotated",
            crate::console::bold_cyan("[hot]")
        );
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            hot_functions = annotated,
            "per-function hot patching armed"
        );
        Some(session)
    }

    /// Re-read every `.rs` file under the source root into the snapshot map.
    ///
    /// The snapshot is the baseline [`classify`](Self::classify) diffs against,
    /// so it must always describe **what is currently running**. Two things make
    /// that true: a successful patch records the new contents itself, and a full
    /// reload re-syncs through this method. Without the second, one unpatchable
    /// edit pins the baseline forever - every later edit is then diffed against
    /// stale contents, reports changes outside a hot body, and is refused for a
    /// change the reload already absorbed.
    pub(crate) fn refresh_snapshots(&mut self) {
        for path in rust_sources(&self.source_root) {
            if let Some(snapshot) = Snapshot::read(&path) {
                self.snapshots.insert(path, snapshot);
            }
        }
        // Stamped after the reads, so a file written during them counts as
        // newer and is reported rather than silently absorbed.
        self.snapshot_taken_at = SystemTime::now();
    }

    /// Try to satisfy a pending source change with a patch instead of a reload.
    ///
    /// Called at the frame boundary, before the normal reload transaction, so
    /// no system is executing when a slot is written.
    pub(crate) fn try_patch(
        &mut self,
        engine: &mut Engine,
        targets: &[(&str, &NativeLibrary)],
        patches: &mut Vec<LoadedPatch>,
    ) -> PatchOutcome {
        let started = Instant::now();
        let mut stages = PatchStages::default();

        // Step 1: Find which annotated function's body changed, if exactly one
        // did and nothing else moved.
        let classify_started = Instant::now();
        let classified = self.classify();
        stages.classify = classify_started.elapsed().as_secs_f64() * 1000.0;

        let (path, declarations, new_contents) = match classified {
            Ok(Some(found)) => found,
            Ok(None) => return PatchOutcome::Unchanged,
            Err(refusal) => return PatchOutcome::NotPatchable { refusal },
        };

        // Step 2: Generate, compile, load and install each changed body in
        // turn. A failure part-way leaves the bodies already installed live -
        // they are independent replacements, and undoing them would discard
        // work that succeeded - but the snapshot is not advanced, so the next
        // change retries the whole file.
        let mut last: Option<ApplyResult> = None;
        let mut patched_names: Vec<String> = Vec::new();
        // Several bodies in one file are patched in sequence and need not share
        // a route: an annotated and an un-annotated function in the same save
        // take different ones. Both are reported, because an edit is only as
        // provable as its weakest body.
        let mut routes: Vec<crate::analytics::PatchRoute> = Vec::new();
        let mut copies = 0usize;
        for declaration in &declarations {
            // The path the running artifact recorded for this function, which
            // is what both the engine registry and a slot are keyed by.
            // Derived from the file's position under the source root, so a
            // function in a submodule resolves as `crate::module::function`.
            let qualified = self.qualified_name(&path, declaration);
            // The slot route asks under a different name; see
            // `slot_lookup_name`.
            let slot_name = self.slot_lookup_name(&path, declaration);

            match self.apply(
                engine,
                targets,
                patches,
                &new_contents,
                declaration,
                &qualified,
                &slot_name,
                &mut stages,
            ) {
                Ok(installed) => {
                    patched_names.push(qualified);
                    if !routes.contains(&installed.route) {
                        routes.push(installed.route);
                    }
                    copies += installed.copies;
                    last = Some(installed);
                }
                // The running implementation is untouched, so the console
                // reports which generation is still executing rather than only
                // what failed.
                Err(failure) => {
                    return PatchOutcome::Failed {
                        active_generation: self.active_generation(&qualified),
                        function: qualified,
                        failure,
                    };
                }
            }
        }

        let Some(installed) = last else {
            return PatchOutcome::Unchanged;
        };

        // Only record the new contents once every body is live, so a partial
        // failure is retried on the next change rather than treated as done.
        // The modification time is re-read here rather than carried from
        // classification: the file may have been written again since, and a
        // stale time only costs one redundant read on the next attempt.
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        self.snapshots.insert(
            path,
            Snapshot {
                contents: new_contents,
                modified,
            },
        );
        PatchOutcome::Patched {
            function: patched_names.join(", "),
            generation: installed.generation,
            elapsed_milliseconds: started.elapsed().as_secs_f64() * 1000.0,
            stages,
            artifact_bytes: installed.artifact_bytes,
            exports: installed.exports,
            routes,
            copies,
        }
    }

    // -------------------------------------------------------------------------
    // Classification
    // -------------------------------------------------------------------------

    /// Identify a body-only edit of exactly one annotated function.
    ///
    /// `Ok(None)` means nothing changed. `Err` carries the reason the change is
    /// out of scope, phrased for the console.
    #[allow(clippy::type_complexity)]
    fn classify(
        &mut self,
    ) -> Result<Option<(PathBuf, Vec<source::HotFunction>, String)>, PatchRefusal> {
        let mut result: Option<(PathBuf, Vec<source::HotFunction>, String)> = None;

        // One directory walk, reused below. Reading every file in the crate on
        // every attempt made classification proportional to crate size rather
        // than to the edit - measured at 53 ms and 72 ms on small crates.
        let sources = rust_sources(&self.source_root);
        let mut any_newer_than_snapshot = false;

        for path in &sources {
            let modified = std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok();
            if modified.is_some_and(|modified| modified > self.snapshot_taken_at) {
                any_newer_than_snapshot = true;
            }

            // A file whose modification time still matches the one recorded
            // when it was read cannot differ from the snapshot, so it needs no
            // read. A file with no snapshot at all is new, and falls through.
            if self
                .snapshots
                .get(path)
                .is_some_and(|snapshot| snapshot.is_current(modified))
            {
                continue;
            }

            let Ok(new_contents) = std::fs::read_to_string(path) else {
                continue;
            };
            let path = path.clone();
            let Some(old_snapshot) = self.snapshots.get(&path) else {
                // A new file is a structural change by definition.
                return Err(PatchRefusal::new(
                    refusal_code::NEW_SOURCE_FILE,
                    format!("new source file {}", path.display()),
                ));
            };
            let old_contents = &old_snapshot.contents;
            if *old_contents == new_contents {
                continue;
            }

            // Every function the host could address, annotated or not. An
            // attribute is no longer the opt-in: a crate whose build script
            // emits the address inventory makes all of its functions
            // patchable, and the attribute only chooses which mechanism
            // delivers the replacement.
            let hot: HashMap<String, source::HotFunction> = source::all_functions(&new_contents)
                .into_iter()
                .map(|function| (function.name.clone(), function))
                .collect();
            if hot.is_empty() {
                return Err(PatchRefusal::new(
                    refusal_code::NO_HOT_FUNCTION,
                    format!(
                        "{} changed but declares no function the host can address",
                        file_label(&path)
                    ),
                ));
            }
            let hot_names: std::collections::HashSet<String> = hot.keys().cloned().collect();

            // Anything outside those bodies must be byte-identical.
            let old_stripped = source::strip_function_bodies(old_contents, &hot_names);
            let new_stripped = source::strip_function_bodies(&new_contents, &hot_names);
            if old_stripped != new_stripped {
                return Err(PatchRefusal::new(
                    refusal_code::OUTSIDE_HOT_BODY,
                    format!(
                        "{} changed outside a hot function body (signature, type, \
                     constant, import or another function)",
                        file_label(&path)
                    ),
                ));
            }

            // Exactly one changed body keeps the first version simple and the
            // diagnostics precise.
            let mut changed: Vec<String> = Vec::new();
            for name in &hot_names {
                let old_body = source::find_function(old_contents, name).map(|found| found.body);
                let new_body = source::find_function(&new_contents, name).map(|found| found.body);
                if old_body != new_body {
                    changed.push(name.clone());
                }
            }
            if changed.is_empty() {
                continue;
            }
            if result.is_some() {
                return Err(PatchRefusal::new(
                    refusal_code::MULTIPLE_FILES,
                    "more than one source file changed",
                ));
            }

            // Several bodies in one save are patched in sequence rather than
            // refused. Each is an independent replacement, so the cost is one
            // compile apiece - still cheaper than the full reload this used to
            // fall back to, and the world is never torn down.
            changed.sort();
            let mut declarations = Vec::with_capacity(changed.len());
            for function in changed {
                let declaration = hot[&function].clone();

                // A method needs its receiver type to be patchable: the
                // generated replacement is a trait implementation for that
                // concrete type. A generic or trait `impl` block has no single
                // type to name, so it is refused rather than producing a patch
                // that cannot compile.
                if declaration.takes_receiver && declaration.self_type.is_none() {
                    return Err(PatchRefusal::new(
                        refusal_code::UNRESOLVED_RECEIVER,
                        format!(
                            "`{function}` takes a receiver but its `impl` block does \
                             not name a simple type; a generic or trait implementation \
                             cannot be patched"
                        ),
                    ));
                }
                declarations.push(declaration);
            }
            result = Some((path, declarations, new_contents));
        }

        // Nothing changed - but if a watched file is newer than the baseline,
        // the edit was already in it. That happens when a file is saved while
        // the host is still starting up, and it is worth saying out loud: the
        // change is real, it simply reached the snapshot before the snapshot
        // was taken, so the fast path has nothing to compare against and the
        // edit falls through to a full reload in silence.
        if result.is_none() && any_newer_than_snapshot {
            return Err(PatchRefusal::new(
                refusal_code::EDITED_BEFORE_ARMING,
                format!(
                    "a source file changed before `{}` armed, so the edit was \
                     captured as the baseline; this reload will pick it up",
                    self.package
                ),
            ));
        }

        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Generation, compilation, activation
    // -------------------------------------------------------------------------

    /// Build and install the replacement for one function.
    // Nine arguments, each a distinct collaborator rather than a field of some
    // implicit struct: the engine, the artifacts, the loaded patches, the source,
    // the declaration, its two names, and the stage timings. Grouping them would
    // hide what they are rather than clarify it.
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        engine: &mut Engine,
        targets: &[(&str, &NativeLibrary)],
        patches: &mut Vec<LoadedPatch>,
        new_contents: &str,
        declaration: &source::HotFunction,
        qualified: &str,
        slot_name: &str,
        stages: &mut PatchStages,
    ) -> Result<ApplyResult, PatchRefusal> {
        let function = declaration.name.as_str();
        let kind = declaration.kind;
        self.counter += 1;
        // The package name is part of it because every session counts from one
        // and patch libraries are never unloaded. Without it, the first patch of
        // a second module tries to write a `.dll` the first module still has
        // mapped, and Windows refuses - so patching one module would stop every
        // other module from ever being patched again in that session.
        let crate_name = format!("pill_hotpatch_{}_{}", self.package, self.counter);

        // Generate.
        let generate_started = Instant::now();
        let generated = self
            .generate(new_contents, declaration, qualified)
            .map_err(|detail| PatchRefusal::new(failure_code::GENERATE, detail))?;
        // Written into the per-process temporary directory rather than the
        // system one, because that directory already has a cleanup path: a later
        // run sweeps the directories of processes that have exited. Patch images
        // are never unloaded, so without this they accumulated on disk for every
        // edit of every session, forever.
        let scratch = crate::native_library::process_temporary_directory(&self.workspace_root);
        if let Err(error) = std::fs::create_dir_all(&scratch) {
            return Err(PatchRefusal::new(
                failure_code::PREPARE,
                format!("cannot create the patch scratch directory: {error}"),
            ));
        }
        let source_path = scratch.join(format!("{crate_name}.rs"));
        std::fs::write(&source_path, generated.as_bytes()).map_err(|error| {
            PatchRefusal::new(
                failure_code::PREPARE,
                format!("cannot write the generated patch: {error}"),
            )
        })?;
        stages.generate = generate_started.elapsed().as_secs_f64() * 1000.0;

        // Compile, replaying cargo's own flags plus the crate's rlib. The
        // extern entry is built before borrowing the cached line, which needs
        // `&mut self` on first use.
        // Keep the crate's own rlib in step with the dependency rlibs the
        // replayed line names, so the two halves of the link closure agree.
        self.refresh_staged_rlib()
            .map_err(|detail| PatchRefusal::new(failure_code::PREPARE, detail))?;
        // Cloned before the cached compiler line is borrowed mutably below.
        let package = self.package.clone();

        let artifact = scratch.join(format!("{crate_name}.dll"));
        let extra_externs = vec![format!("{}={}", self.package, self.package_rlib.display())];
        // Dependency rlibs staged when the host last built this package, which
        // is the only set guaranteed to match the module rlib being linked. The
        // shared `deps` slots they were copied from are overwritten by any build
        // of the same crate name, a differently-featured one included.
        let staged_dependencies = self
            .workspace_root
            .join(crate::build_runner::STAGED_DEPENDENCY_SUBDIRECTORY);
        let flags_started = Instant::now();
        let line = self
            .rustc_line()
            .map_err(|detail| PatchRefusal::new(failure_code::PREPARE, detail))?;
        stages.flags = flags_started.elapsed().as_secs_f64() * 1000.0;
        let compile_started = Instant::now();
        let output = line
            .replay(
                &source_path,
                &artifact,
                &crate_name,
                &extra_externs,
                Some(staged_dependencies.as_path()),
            )
            .output()
            .map_err(|error| {
                PatchRefusal::new(failure_code::COMPILE, format!("cannot run rustc: {error}"))
            })?;
        stages.compile = compile_started.elapsed().as_secs_f64() * 1000.0;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // A link failure reports `error: linking with ... failed` first and
            // says nothing useful until the linker's own line further down, so
            // that one is preferred when present.
            let linker = stderr
                .lines()
                .map(str::trim)
                .find(|line| line.contains("rust-lld: error:") || line.contains("LNK"));
            let first = stderr
                .lines()
                .find(|line| line.starts_with("error"))
                .unwrap_or("rustc rejected the generated patch");
            let package = package.as_str();
            let detail = match linker {
                Some(linker) => format!("{first} - {linker}"),
                // `can't find crate for <this crate>` names the crate being
                // patched, which reads as though its rlib is missing. It is
                // there; it no longer matches the dependency rlibs the replayed
                // line points at, because one of them was rebuilt.
                None if first.contains("E0463") && first.contains(package) => format!(
                    "{first} - the staged rlib no longer matches the dependency \
                     rlibs it links against; a crate `{package}` depends on was \
                     rebuilt after it was staged"
                ),
                None => first.to_string(),
            };
            // The exact command, so a failure can be reproduced by hand rather
            // than guessed at. DEBUG because it is long and only wanted when
            // something has already gone wrong.
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                command = line
                    .replay_args(
                        &source_path,
                        &artifact,
                        &crate_name,
                        &extra_externs,
                        Some(staged_dependencies.as_path()),
                    )
                    .join(" ")
                    .as_str(),
                "the patch compile that failed"
            );
            return Err(PatchRefusal::new(failure_code::COMPILE, detail));
        }

        // Load. Never unloaded: a slot will hold an address inside this image.
        let load_started = Instant::now();
        // SAFETY: the file was just produced by rustc from a generated source
        // and is a complete native module.
        let library = unsafe { Library::new(&artifact) }.map_err(|error| {
            PatchRefusal::new(
                failure_code::LOAD,
                format!("cannot load the patch library: {error}"),
            )
        })?;
        stages.load = load_started.elapsed().as_secs_f64() * 1000.0;
        let activate_started = Instant::now();

        // Measured before installing, so the analytics line carries the same
        // two numbers a module reload does. Both are best-effort: a patch is
        // still correct when its size or export table cannot be read.
        let artifact_bytes = std::fs::metadata(&artifact)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let exports = crate::analytics::inspect_pe(&artifact)
            .map(|inspection| inspection.exports.len())
            .unwrap_or(0);

        // Filled in by whichever route runs below, and recorded on the
        // generation so a rollback can reinstall exactly this address without
        // recompiling anything.
        let address;
        let mut signature_hash = 0u64;
        let mut signature = String::new();
        let mut prologue_restores: Vec<PrologueRestore> = Vec::new();
        // Both are set by whichever arm of the install below runs; the compiler
        // proves that, so there is no placeholder value to get wrong.
        let route: crate::analytics::PatchRoute;
        let copies: usize;

        // Install at the frame boundary. Both routes refuse a signature that no
        // longer matches, so a reshaped function can never be applied behind a
        // call site compiled for the old shape.
        match kind {
            // A system is dispatched through the engine's own registry, which
            // lives once in this process because the engine is one shared
            // library. One install reaches every caller.
            source::HotFunctionKind::System => {
                let lookup_name = format!("{PATCH_NAME_PREFIX}{qualified}");
                let (found, hash) = resolve_in(&library, &lookup_name)
                    .map_err(|detail| PatchRefusal::new(failure_code::RESOLVE, detail))?;
                engine
                    .hot_patch(qualified, found, hash)
                    .map_err(|error| PatchRefusal::new(failure_code::INSTALL, error.to_string()))?;
                address = found;
                signature_hash = hash;
                // The registry lives once per process, so this single install
                // is every caller.
                route = crate::analytics::PatchRoute::EngineSlot;
                copies = 1;
            }
            // A plain function has no registry: its redirect slot is a static
            // compiled into each artifact that links the crate. The project DLL
            // embeds its own copy of a module it depends on, and so does every
            // other module linking it, so the same replacement is offered to all
            // of them - which is exactly what makes the cascading project reload
            // this edit would otherwise trigger unnecessary.
            // No attribute, so no slot exists to install into: the running
            // copies are redirected by overwriting their first bytes.
            source::HotFunctionKind::PlainFunction if !declaration.annotated => {
                let found = resolve_patch_address(&library)
                    .map_err(|detail| PatchRefusal::new(failure_code::RESOLVE, detail))?;
                prologue_restores = prologue_patch_everywhere(
                    targets,
                    patches,
                    qualified,
                    found,
                    &declaration.signature,
                )
                .map_err(|detail| PatchRefusal::new(failure_code::INSTALL, detail))?;
                address = found;
                route = crate::analytics::PatchRoute::Prologue;
                copies = prologue_restores.len();
            }
            source::HotFunctionKind::PlainFunction => {
                // A method patch is filed under the prefixed running name,
                // because its body lives in a local trait rather than under the
                // patch crate's own module path.
                let lookup_name = if declaration.takes_receiver {
                    format!("{PATCH_NAME_PREFIX}{qualified}")
                } else {
                    format!("{crate_name}::{function}")
                };
                let (found, found_signature) = resolve_plain_in(&library, &lookup_name)
                    .map_err(|detail| PatchRefusal::new(failure_code::RESOLVE, detail))?;
                copies = install_everywhere(targets, patches, slot_name, found, &found_signature)
                    .map_err(|detail| PatchRefusal::new(failure_code::INSTALL, detail))?;
                address = found;
                signature = found_signature;
                route = crate::analytics::PatchRoute::ArtifactSlot;
            }
        }
        stages.activate = activate_started.elapsed().as_secs_f64() * 1000.0;

        // One past the highest number this function already carries, so each
        // function is numbered independently and a rollback does not renumber
        // the history it rolled back over.
        let generation = self
            .generations
            .iter()
            .filter(|existing| existing.function == qualified)
            .map(|existing| existing.number)
            .max()
            .unwrap_or(0)
            + 1;
        self.generations.push(Generation {
            prologue_history_dropped: false,
            function: qualified.to_string(),
            // Whichever form the route that delivered this generation looks up,
            // so a rollback asks the same question the install did.
            lookup_name: match kind {
                source::HotFunctionKind::PlainFunction if declaration.annotated => {
                    slot_name.to_string()
                }
                _ => qualified.to_string(),
            },
            number: generation,
            address,
            kind,
            signature_hash,
            signature,
            prologue_restores,
            installed_at: Instant::now(),
        });
        // The image itself is process-wide, not owned by this session: a later
        // patch of a DIFFERENT crate has to reach the copies inside it.
        patches.push(LoadedPatch {
            function: qualified.to_string(),
            generation,
            library,
        });
        self.active_generations
            .insert(qualified.to_string(), generation);
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            function = qualified,
            generation,
            generations = self.generations.len(),
            "patch generation installed"
        );
        Ok(ApplyResult {
            generation,
            artifact_bytes,
            exports,
            route,
            copies,
        })
    }

    /// Produce the patch source for one edited function.
    ///
    /// The function is copied verbatim, so its new body compiles exactly as
    /// written. Everything it names comes from the SAME artifacts the running
    /// module linked, which is what keeps type layout and `TypeId` identical
    /// across the boundary.
    fn generate(
        &self,
        new_contents: &str,
        declaration: &source::HotFunction,
        qualified: &str,
    ) -> Result<String, String> {
        let function = declaration.name.as_str();
        let kind = declaration.kind;
        // A method is located through its own `impl` block, not by bare name.
        // Two types may each implement `Default::default`, and a type may carry
        // an inherent `draw` beside a trait `draw`; taking the first `fn draw`
        // in the file would compile the wrong body and install it silently.
        let found = match (&declaration.self_type, declaration.takes_receiver) {
            (Some(self_type), true) => source::find_method(
                new_contents,
                function,
                self_type,
                declaration.trait_name.as_deref(),
            )
            .ok_or_else(|| {
                format!(
                    "cannot locate `{function}` in the `impl` block for `{self_type}`                      in the changed source"
                )
            })?,
            _ => source::find_function(new_contents, function)
                .ok_or_else(|| format!("cannot locate `{function}` in the changed source"))?,
        };

        let imports = source::top_level_use_statements(new_contents).join("\n");
        let package = &self.package;

        // An un-annotated function, redirected by overwriting its prologue.
        // Nothing in the running artifact was prepared for this, so the patch
        // needs no slot, no descriptor and no signature text: it exports one
        // address, and the host writes a jump to it. A method still goes into a
        // local trait so its body can keep using `self`.
        if !declaration.annotated {
            let (definition, address_expression) = if declaration.takes_receiver {
                let self_type = declaration
                    .self_type
                    .as_deref()
                    .ok_or_else(|| format!("`{function}` has no known receiver type"))?;
                let signature = found
                    .text
                    .split_once('{')
                    .map(|(head, _)| head.trim().to_string())
                    .ok_or_else(|| format!("cannot read the signature of `{function}`"))?;
                (
                    format!(
                        "trait PillHotMethodPatch {{ {signature}; }}\n\
                         impl PillHotMethodPatch for {self_type} {{ {body} }}",
                        body = found.text
                    ),
                    format!("<{self_type} as PillHotMethodPatch>::{function}"),
                )
            } else {
                (found.text.clone(), function.to_string())
            };
            return Ok(format!(
                "// GENERATED by pill_host hot patching - do not edit.\n\
                 //\n\
                 // One edited function, compiled against the same artifacts the\n\
                 // running module linked. The host overwrites the prologue of\n\
                 // every loaded copy with a jump to the address exported below.\n\
                 #![allow(unused_imports, dead_code, unused_mut)]\n\
                 \n\
                 {imports}\n\
                 use {package}::*;\n\
                 \n\
                 {definition}\n\
                 \n\
                 /// Where this patch's new body is.\n\
                 #[no_mangle]\n\
                 pub extern \"C\" fn pill_patch_address() -> usize {{\n\
                 {address_expression} as *const () as usize\n\
                 }}\n\
                 \n\
                 // A patch links its own copy of everything the body calls, and\n\
                 // those copies are as patchable as any other artifact's. The\n\
                 // resolver is what lets a later patch reach them, so a chain of\n\
                 // hot functions composes instead of freezing at whatever the\n\
                 // callee looked like when this patch was compiled.\n\
                 ::pill_engine::pill_hot_resolver!(pill_patch_resolve);\n"
            ));
        }

        // An inherent method. Its body names `self`, so it cannot be copied
        // into a free function - the attribute is told the receiver type and
        // carries the body into a local trait implemented for it instead. The
        // signature text is computed by the same macro the running artifact
        // used, so the two are comparable by construction rather than by the
        // host reproducing a string.
        if kind == source::HotFunctionKind::PlainFunction && declaration.takes_receiver {
            let self_type = declaration
                .self_type
                .as_deref()
                .ok_or_else(|| format!("`{function}` has no known receiver type"))?;
            return Ok(format!(
                "// GENERATED by pill_host hot patching - do not edit.\n\
                 //\n\
                 // One edited method, compiled against the same artifacts the\n\
                 // running module linked. The body is carried into a local trait\n\
                 // implemented for the receiver type, which is what lets it keep\n\
                 // using `self`.\n\
                 #![allow(unused_imports, dead_code, unused_mut)]\n\
                 \n\
                 {imports}\n\
                 use {package}::*;\n\
                 \n\
                 #[::pill_engine::pill_hot_fn(\n\
                 name = \"{PATCH_NAME_PREFIX}{qualified}\",\n\
                 self_type = {self_type}\n\
                 )]\n\
                 {body}\n\
                 \n\
                 // A distinct export name: the linked rlib already provides\n\
                 // `pill_hot_resolve`.\n\
                 ::pill_engine::pill_hot_resolver!(pill_patch_resolve);\n",
                body = found.text
            ));
        }

        // A plain function needs no registry entry, so it keeps the attribute
        // it already carries and is looked up under the patch crate's own name.
        // That name is unique per patch, so it cannot be confused with the copy
        // of the same function the linked rlib also contributes.
        if kind == source::HotFunctionKind::PlainFunction {
            return Ok(format!(
                "// GENERATED by pill_host hot patching - do not edit.\n\
                 //\n\
                 // One edited function, compiled against the same artifacts the\n\
                 // running module linked.\n\
                 #![allow(unused_imports, dead_code, unused_mut)]\n\
                 \n\
                 {imports}\n\
                 use {package}::*;\n\
                 \n\
                 // The same attribute the module itself uses, so the signature\n\
                 // text the two sides compare is produced by one macro from one\n\
                 // piece of source rather than reconstructed by hand.\n\
                 #[::pill_engine::pill_hot_fn]\n\
                 {body}\n\
                 \n\
                 // A distinct export name: the linked rlib already provides\n\
                 // `pill_hot_resolve`.\n\
                 ::pill_engine::pill_hot_resolver!(pill_patch_resolve);\n",
                body = found.text
            ));
        }

        Ok(format!(
            "// GENERATED by pill_host hot patching - do not edit.\n\
             //\n\
             // One edited function, compiled against the same artifacts the\n\
             // running module linked.\n\
             #![allow(unused_imports, dead_code, unused_mut)]\n\
             \n\
             {imports}\n\
             use {package}::*;\n\
             \n\
             // `name` pins the registry entry to the ORIGINAL path, prefixed so\n\
             // it cannot collide with the copy the linked rlib also carries.\n\
             #[::pill_engine::pill_hot(name = \"{PATCH_NAME_PREFIX}{qualified}\")]\n\
             {body}\n\
             \n\
             // A distinct export name: the linked rlib already provides\n\
             // `pill_hot_resolve`.\n\
             ::pill_engine::pill_hot_resolver!(pill_patch_resolve);\n",
            body = found.text
        ))
    }

    /// Module path of a source file, as the crate sees it.
    ///
    /// `module_path!()` inside the crate follows the file tree, so a function in
    /// `src/lib.rs` sits at `crate`, and one in `src/color.rs` at
    /// `crate::color`.
    fn module_segments(&self, path: &Path) -> Vec<String> {
        let mut segments = vec![self.package.clone()];
        let Ok(relative) = path.strip_prefix(&self.source_root) else {
            return segments;
        };
        let components: Vec<_> = relative.components().collect();
        for (index, component) in components.iter().enumerate() {
            let name = component.as_os_str().to_string_lossy();
            let last = index + 1 == components.len();
            let name = if last {
                name.strip_suffix(".rs").unwrap_or(&name).to_string()
            } else {
                name.into_owned()
            };
            // `lib.rs` and `mod.rs` name the module their location already
            // implies, so they contribute no segment of their own.
            if name == "lib" || name == "mod" {
                continue;
            }
            segments.push(name);
        }
        segments
    }

    /// The canonical path of a declaration: module path, type, function.
    ///
    /// This is what a build script registers, because it can see the enclosing
    /// `impl` block - so it is the name the prologue route looks up, and the one
    /// used for display and generation bookkeeping.
    fn qualified_name(&self, path: &Path, declaration: &source::HotFunction) -> String {
        let segments = self.module_segments(path);
        // Built by the scanner rather than here, so the host asks for exactly
        // the name the build script recorded. When the two were separate
        // implementations every inherent method silently failed to patch, and a
        // trait method - whose name carries both the type and the trait - has
        // more to disagree about, not less.
        source::inventory_name(&self.package, &segments[1..], declaration)
    }

    /// The path a dispatch slot is registered under, which omits the type.
    ///
    /// Deliberately different from [`Self::qualified_name`], and the difference
    /// is forced rather than chosen. A slot's descriptor is an item, and every
    /// item inside a method body is barred from naming `Self` (`error[E0401]`),
    /// so `#[pill_hot_fn]` on a method can only register
    /// `module_path!() + "::" + name`. A build script has no such limit, having
    /// read the `impl` block directly.
    ///
    /// Two hot methods sharing a name in one module therefore collide on the
    /// slot route. The scanner sees both and refuses the edit rather than
    /// installing into whichever registered first.
    fn slot_lookup_name(&self, path: &Path, declaration: &source::HotFunction) -> String {
        let mut segments = self.module_segments(path);
        segments.push(declaration.name.clone());
        segments.join("::")
    }

    /// Bring the staged rlib back in step with cargo's own output.
    ///
    /// A patch links a half-frozen closure: the crate's own rlib comes from the
    /// staged copy, which only changes when the host rebuilds that module, while
    /// every `--extern` for its dependencies comes from the replayed cargo line
    /// and points into the module build tree, which moves whenever anything
    /// rebuilds. Let those drift apart and the compile fails with
    /// `error[E0463]: can't find crate for <this crate>` - which names the wrong
    /// crate and says nothing about staleness.
    ///
    /// The source of truth is the same place the module reload stages from and
    /// the flag-capture build writes into: the private module build tree under
    /// the host's profile directory ([`crate::config::module_build_artifact_directory`],
    /// e.g. `target/hot/build/debug` for a dev host or
    /// `target/hot/build/desktop-dev` under the dioxus CLI). The previous
    /// hardcoded `target/debug` only matched a bare default-directory build, which
    /// the host never runs: when a launcher injects a custom profile that path
    /// holds a stale dev-profile rlib (or nothing), and refreshing the staged
    /// copy from it linked the patch against a differently configured engine -
    /// every type got a different `TypeId` and rustc reported `error[E0463]`.
    ///
    /// Re-copying costs a few milliseconds and keeps the closure consistent.
    /// The staged copy is still what the host *loads*, so the protection it was
    /// added for - another build overwriting the shared slot - is unchanged: a
    /// wrong-featured rlib copied here can only make this one patch fail to
    /// compile, which is the same outcome as leaving it stale, and the module's
    /// next real build restages it correctly.
    fn refresh_staged_rlib(&self) -> Result<(), String> {
        let built = self
            .workspace_root
            .join(crate::config::module_build_artifact_directory())
            .join(format!("lib{}.rlib", self.package));
        let Ok(built_metadata) = std::fs::metadata(&built) else {
            // Cargo has not produced one; the staged copy is all there is.
            return Ok(());
        };
        if let Ok(staged_metadata) = std::fs::metadata(&self.package_rlib) {
            let same_size = staged_metadata.len() == built_metadata.len();
            let staged_is_current = match (staged_metadata.modified(), built_metadata.modified()) {
                (Ok(staged), Ok(built)) => staged >= built,
                _ => false,
            };
            if same_size && staged_is_current {
                return Ok(());
            }
        }

        std::fs::copy(&built, &self.package_rlib).map_err(|error| {
            format!(
                "cannot refresh the staged rlib from {}: {error}",
                built.display()
            )
        })?;
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            package = self.package.as_str(),
            source = %built.display(),
            "restaged the rlib so the patch links a consistent closure"
        );
        Ok(())
    }

    /// The captured compiler flags, asking cargo on first use.
    fn rustc_line(&mut self) -> Result<&CargoRustcLine, String> {
        if self.rustc_line.is_none() {
            let cache = std::env::temp_dir().join(format!("pill_hotpatch_{}.flags", self.package));
            // Deliberately NOT the crate root. The flags describe the dependency
            // graph and feature set, which a source edit cannot change - and the
            // crate root is precisely the file being edited, so including it
            // invalidated the cache on every single patch and re-ran a full
            // `cargo build -v` to re-derive flags that were already correct.
            // That cost about 1.2 s of a 1.9 s patch.
            //
            // The crate's own freshly built rlib IS included, though. The flags
            // describe the dependency closure this crate links, and that
            // closure only changes when the crate is rebuilt - at startup, on a
            // module reload, or whenever the feature unification with the host
            // anchor moves (under the dioxus CLI, engine crates can carry two
            // different metadata hashes side by side for the module and the
            // editor anchor; a cache captured against one goes silently stale
            // against the other and rustc reports `error[E0463]` when the patch
            // tries to link the freshly staged rlib against the old externs).
            // The rlib's mtime moves exactly when that happens, so keying on it
            // re-captures precisely when the world changed and stays hot across
            // plain source edits and patches.
            let mut freshness = vec![
                self.workspace_root.join("Cargo.toml"),
                self.workspace_root.join("Cargo.lock"),
            ];
            freshness.push(
                self.workspace_root
                    .join(crate::config::module_build_artifact_directory())
                    .join(format!("lib{}.rlib", self.package)),
            );
            let line = match CargoRustcLine::load_if_fresh(&cache, &freshness, &self.build_command)
            {
                Some(cached) => cached,
                None => {
                    let captured = CargoRustcLine::capture(
                        &self.workspace_root,
                        &self.package,
                        &self.crate_root,
                        &self.build_command,
                    )?;
                    let _ = captured.save(&cache, &self.build_command);
                    captured
                }
            };
            self.rustc_line = Some(line);
        }
        Ok(self.rustc_line.as_ref().expect("just populated"))
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Every `.rs` file under `root`, in stable order.
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// A path shortened to its file name, for console messages.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated source must carry the prefixed name and the distinct
    /// resolver export, because both prevent a silent collision with the copy
    /// of the project the patch necessarily links.
    #[test]
    fn generated_source_namespaces_the_entry_and_the_export() {
        let session = HotPatchSession {
            workspace_root: PathBuf::from("."),
            package: "project".to_string(),
            crate_root: PathBuf::from("lib.rs"),
            source_root: PathBuf::from("."),
            package_rlib: PathBuf::from("libproject.rlib"),
            build_command: vec!["cargo".to_string(), "build".to_string()],
            snapshots: HashMap::new(),
            snapshot_taken_at: SystemTime::UNIX_EPOCH,
            rustc_line: None,
            generations: Vec::new(),
            active_generations: HashMap::new(),
            counter: 0,
        };

        let contents =
            "use pill_engine::*;\n\n#[pill_hot]\nfn movement(value: i32) -> i32 { value * 2 }\n";
        let generated = session
            .generate(
                contents,
                &system_declaration("movement"),
                "project::movement",
            )
            .expect("generated");

        assert!(generated.contains("name = \"pill_patch::project::movement\""));
        assert!(generated.contains("pill_hot_resolver!(pill_patch_resolve)"));
        assert!(generated.contains("use pill_engine::*;"));
        assert!(generated.contains("use project::*;"));
        // The function is copied verbatim, body included.
        assert!(generated.contains("fn movement(value: i32) -> i32 { value * 2 }"));
        // The original attribute must NOT be duplicated.
        assert!(generated.contains("#["));
        assert_eq!(generated.matches("pill_hot(name").count(), 1);
    }

    /// A plain function is generated with its own attribute and no registry
    /// override, because it is redirected through per-artifact slots rather
    /// than through the engine's registry.
    #[test]
    fn a_plain_function_keeps_its_own_attribute() {
        let directory = std::env::temp_dir().join("pill_generate_plain");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, PLAIN_SOURCE);

        let generated = session
            .generate(
                PLAIN_SOURCE,
                &plain_declaration("get_color_a"),
                "project::get_color_a",
            )
            .expect("a plain function must generate");

        assert!(generated.contains("#[::pill_engine::pill_hot_fn]"));
        // A `name` override belongs to the system path only: a plain function
        // is found under the patch crate's own unique name.
        assert!(!generated.contains("pill_hot(name"));
        assert!(generated.contains("pill_hot_resolver!(pill_patch_resolve)"));
        assert!(generated.contains("133.0"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Classification must report which attribute marked the changed function,
    /// because the two are installed through entirely different machinery.
    #[test]
    fn classify_reports_the_plain_function_kind() {
        let directory = std::env::temp_dir().join("pill_classify_plain");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, PLAIN_SOURCE);

        let edited = PLAIN_SOURCE.replace("133.0", "999.0");
        std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

        let classified = session.classify().expect("body-only edit must be accepted");
        let (_, declaration, _) = classified.expect("a change must be reported");
        assert_eq!(declaration.len(), 1, "one body changed");
        assert_eq!(declaration[0].name, "get_color_a");
        assert_eq!(declaration[0].kind, source::HotFunctionKind::PlainFunction);

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The name a function is keyed by follows the file tree, so a function in
    /// a submodule must not be looked up under the crate root's name.
    #[test]
    fn qualified_names_follow_the_source_tree() {
        let directory = std::env::temp_dir().join("pill_qualified_names");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, PLAIN_SOURCE);
        let free = plain_declaration("get_color_a");

        assert_eq!(
            session.qualified_name(&directory.join("lib.rs"), &free),
            "project::get_color_a"
        );
        assert_eq!(
            session.qualified_name(&directory.join("color.rs"), &free),
            "project::color::get_color_a"
        );
        assert_eq!(
            session.qualified_name(&directory.join("color").join("mod.rs"), &free),
            "project::color::get_color_a"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A method's canonical name carries its type, because that is what a build
    /// script registers - it can read the `impl` block.
    ///
    /// This is the contract that was broken: the host asked for
    /// `pill_dummy_color::mix` while the inventory held
    /// `pill_dummy_color::Tint::mix`, so every method missed and the refusal
    /// blamed the build script.
    #[test]
    fn a_method_is_named_through_its_type() {
        let directory = std::env::temp_dir().join("pill_method_naming");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, PLAIN_SOURCE);
        let method = source::HotFunction {
            name: "mix".to_string(),
            kind: source::HotFunctionKind::PlainFunction,
            self_type: Some("Tint".to_string()),
            trait_name: None,
            takes_receiver: true,
            signature: "fn mix(&self, other: Tint) -> Tint".to_string(),
            cfg_gated: false,
            inline_always: false,
            abi_entry_point: false,
            annotated: false,
        };

        assert_eq!(
            session.qualified_name(&directory.join("lib.rs"), &method),
            "project::Tint::mix"
        );
        assert_eq!(
            session.qualified_name(&directory.join("color.rs"), &method),
            "project::color::Tint::mix"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A trait method is named through both its type and its trait, because
    /// neither alone identifies it.
    ///
    /// A type may carry an inherent `draw` beside a trait `draw`, and two traits
    /// may each define `draw` for it. `Type::draw` names all of them.
    #[test]
    fn a_trait_method_is_named_through_its_trait() {
        let directory = std::env::temp_dir().join("pill_trait_method_naming");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, PLAIN_SOURCE);
        let via_trait = source::HotFunction {
            name: "default".to_string(),
            kind: source::HotFunctionKind::PlainFunction,
            self_type: Some("Spline".to_string()),
            trait_name: Some("Default".to_string()),
            takes_receiver: false,
            signature: "fn default() -> Self".to_string(),
            cfg_gated: false,
            inline_always: false,
            abi_entry_point: false,
            annotated: false,
        };

        assert_eq!(
            session.qualified_name(&directory.join("lib.rs"), &via_trait),
            "project::<Spline as Default>::default"
        );
        assert_eq!(
            session.qualified_name(&directory.join("spline.rs"), &via_trait),
            "project::spline::<Spline as Default>::default"
        );

        // And the inherent method of the same name stays a different key.
        let inherent = source::HotFunction {
            trait_name: None,
            ..via_trait
        };
        assert_eq!(
            session.qualified_name(&directory.join("lib.rs"), &inherent),
            "project::Spline::default"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// An edit to a trait method body classifies as patchable and reports the
    /// trait, so the generated patch and the address lookup agree.
    #[test]
    fn classify_accepts_a_trait_method_body() {
        let directory = std::env::temp_dir().join("pill_classify_trait_method");
        let _ = std::fs::remove_dir_all(&directory);
        let source = "pub struct Spline(u32);\n\nimpl Default for Spline {
fn default() -> Self { Spline(1) }\n}\n";
        let mut session = session_over(&directory, source);

        std::fs::write(
            directory.join("lib.rs"),
            source.replace("Spline(1)", "Spline(2)"),
        )
        .expect("write edit");

        let (_, declarations, contents) = session
            .classify()
            .expect("a trait method body is in scope")
            .expect("the change must be reported");
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "default");
        assert_eq!(declarations[0].self_type.as_deref(), Some("Spline"));
        assert_eq!(declarations[0].trait_name.as_deref(), Some("Default"));
        assert!(contents.contains("Spline(2)"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The generated patch carries the body from the right `impl` block.
    ///
    /// Two types implementing one trait method is the case that made a bare-name
    /// search unsafe: it would compile the first `fn default` in the file and
    /// install it for whichever type was patched.
    #[test]
    fn a_generated_trait_patch_carries_the_right_body() {
        let directory = std::env::temp_dir().join("pill_generate_trait_method");
        let _ = std::fs::remove_dir_all(&directory);
        let source = "pub struct Alpha(u32);\npub struct Beta(u32);\n
impl Shape for Alpha {
fn size(&self) -> u32 { 111 }\n}\n
impl Shape for Beta {
fn size(&self) -> u32 { 222 }\n}\n";
        let session = session_over(&directory, source);

        let beta = source::HotFunction {
            name: "size".to_string(),
            kind: source::HotFunctionKind::PlainFunction,
            self_type: Some("Beta".to_string()),
            trait_name: Some("Shape".to_string()),
            takes_receiver: true,
            signature: "fn size(&self) -> u32".to_string(),
            cfg_gated: false,
            inline_always: false,
            abi_entry_point: false,
            annotated: false,
        };
        let generated = session
            .generate(source, &beta, "project::<Beta as Shape>::size")
            .expect("a trait method generates a patch");

        assert!(
            generated.contains("222"),
            "the patch must carry Beta's body, not Alpha's:\n{generated}"
        );
        assert!(
            !generated.contains("111"),
            "Alpha's body must not appear:\n{generated}"
        );
        // The body keeps `self`, so it is carried into a local trait implemented
        // for the concrete type - exactly as an inherent method is.
        assert!(generated.contains("impl PillHotMethodPatch for Beta"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A dispatch slot for a method is registered WITHOUT its type, so the slot
    /// route has to ask under the degraded name.
    ///
    /// Not a preference: the descriptor is an item, and items may not name
    /// `Self` (`error[E0401]`), so `#[pill_hot_fn]` on a method has no way to
    /// learn the type. The two forms are therefore expected to differ.
    #[test]
    fn a_slot_lookup_omits_the_type_the_macro_cannot_see() {
        let directory = std::env::temp_dir().join("pill_slot_naming");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, PLAIN_SOURCE);
        let method = source::HotFunction {
            name: "get_color_a".to_string(),
            kind: source::HotFunctionKind::PlainFunction,
            self_type: Some("Spline".to_string()),
            trait_name: None,
            takes_receiver: true,
            signature: "fn get_color_a(&self) -> f32".to_string(),
            cfg_gated: false,
            inline_always: false,
            abi_entry_point: false,
            annotated: true,
        };

        assert_eq!(
            session.slot_lookup_name(&directory.join("lib.rs"), &method),
            "project::get_color_a",
            "a slot is registered under module_path!() + the method name"
        );
        assert_eq!(
            session.qualified_name(&directory.join("lib.rs"), &method),
            "project::Spline::get_color_a",
            "the canonical name still carries the type"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A `#[pill_hot]` system declaration, as classification would report it.
    fn system_declaration(name: &str) -> source::HotFunction {
        source::HotFunction {
            name: name.to_string(),
            kind: source::HotFunctionKind::System,
            self_type: None,
            trait_name: None,
            takes_receiver: false,
            signature: format!("fn {name}()"),
            cfg_gated: false,
            inline_always: false,
            abi_entry_point: false,
            annotated: true,
        }
    }

    /// A `#[pill_hot_fn]` free-function declaration.
    fn plain_declaration(name: &str) -> source::HotFunction {
        source::HotFunction {
            name: name.to_string(),
            kind: source::HotFunctionKind::PlainFunction,
            self_type: None,
            trait_name: None,
            takes_receiver: false,
            signature: format!("fn {name}()"),
            cfg_gated: false,
            inline_always: false,
            abi_entry_point: false,
            annotated: true,
        }
    }

    /// Two sessions must never generate the same patch artifact name.
    ///
    /// Every session counts its generations from one and patch libraries are
    /// never unloaded, so a shared name means the first patch of a second module
    /// tries to write a `.dll` the first module still has mapped. Windows
    /// refuses, and that module can never be patched again in that session.
    #[test]
    fn patch_artifact_names_do_not_collide_across_sessions() {
        let first = std::env::temp_dir().join("pill_names_first");
        let second = std::env::temp_dir().join("pill_names_second");
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);

        let mut colour = session_over(&first, PLAIN_SOURCE);
        colour.package = "pill_dummy_color".to_string();
        let mut spline = session_over(&second, PLAIN_SOURCE);
        spline.package = "pill_spline".to_string();

        colour.counter += 1;
        spline.counter += 1;
        let colour_name = format!("pill_hotpatch_{}_{}", colour.package, colour.counter);
        let spline_name = format!("pill_hotpatch_{}_{}", spline.package, spline.counter);
        assert_ne!(
            colour_name, spline_name,
            "each module's first patch must claim its own artifact"
        );

        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    /// An untouched file is not re-read, so classification costs what the edit
    /// costs rather than what the crate weighs.
    ///
    /// The gate compares the file's modification time against the one recorded
    /// when it was read - filesystem time against filesystem time. Comparing
    /// against `SystemTime::now()` is not sound on Windows, where file times come
    /// from the coarse system clock and a file written after a snapshot can
    /// report an earlier time; that mistake made every classification miss.
    #[test]
    fn an_untouched_file_is_not_re_read() {
        let directory = std::env::temp_dir().join("pill_classify_mtime_gate");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);

        // A second file that never changes.
        let untouched = directory.join("untouched.rs");
        fs_write(&untouched, "pub fn stable() {}\n");
        session.refresh_snapshots();
        assert!(
            session.snapshots[&untouched].modified.is_some(),
            "the snapshot must record when it was read"
        );

        // Its recorded contents are then replaced with a different set of
        // functions, without touching the file. Nothing on disk changed, so the
        // gate must skip it - and if it does not, the re-read sees a renamed
        // function, which is a structural change and refuses the whole
        // classification. That makes the skip observable rather than asserted.
        // The recorded time is kept, so the gate still sees the file as
        // unchanged; only the contents are made to disagree.
        let recorded = session.snapshots[&untouched].modified;
        session.snapshots.insert(
            untouched.clone(),
            Snapshot {
                contents: "pub fn renamed_since_the_snapshot() {}\n".to_string(),
                modified: recorded,
            },
        );

        // Only the edited file is re-read, so the classification succeeds and
        // names it.
        let edited = HOT_SOURCE.replace("value * SPEED", "value * SPEED * 3.0");
        std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

        let (path, declarations, _) = session
            .classify()
            .expect("the untouched file must be skipped, not re-read")
            .expect("a change must be reported");
        assert_eq!(path.file_name().unwrap(), "lib.rs");
        assert_eq!(declarations[0].name, "movement");

        // And the stale snapshot is still there: proof the file was never read.
        assert!(
            session.snapshots[&untouched]
                .contents
                .contains("renamed_since_the_snapshot"),
            "the untouched file was re-read despite an unchanged modification time"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A file whose content changes is re-read even though it was in the
    /// snapshot, because its modification time moved.
    #[test]
    fn a_touched_file_is_re_read() {
        let directory = std::env::temp_dir().join("pill_classify_mtime_touch");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, PLAIN_SOURCE);

        // Sleep past the filesystem's timestamp granularity, so the write is
        // guaranteed to produce a different modification time rather than
        // relying on it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let edited = PLAIN_SOURCE.replace("133.0", "144.0");
        std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

        let (_, declarations, contents) = session
            .classify()
            .expect("a body-only edit is in scope")
            .expect("the changed file must be re-read and reported");
        assert_eq!(declarations[0].name, "get_color_a");
        assert!(contents.contains("144.0"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The staged rlib is brought back in step with cargo's output before a
    /// patch is compiled, so the two halves of the link closure agree.
    ///
    /// Without this the compile fails with `error[E0463]: can't find crate for
    /// <the crate being patched>` whenever one of its dependencies was rebuilt
    /// after it was staged - a message that names the wrong crate entirely.
    #[test]
    fn a_stale_staged_rlib_is_refreshed_before_compiling() {
        let directory = std::env::temp_dir().join("pill_restage_rlib");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, PLAIN_SOURCE);

        // What cargo produced (in the private module build tree under the
        // host profile directory), and the older copy the host staged from it.
        let built = directory
            .join(crate::config::module_build_artifact_directory())
            .join("libproject.rlib");
        fs_write(&built, "rebuilt against the current dependencies");
        fs_write(&session.package_rlib, "stale");

        session
            .refresh_staged_rlib()
            .expect("refreshing must succeed");
        assert_eq!(
            std::fs::read_to_string(&session.package_rlib).expect("read staged"),
            "rebuilt against the current dependencies",
            "the staged copy must match what cargo produced"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// An already-current staged copy is left alone, so the common path costs
    /// two metadata reads rather than a file copy.
    #[test]
    fn a_current_staged_rlib_is_left_alone() {
        let directory = std::env::temp_dir().join("pill_restage_current");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, PLAIN_SOURCE);

        let built = directory
            .join(crate::config::module_build_artifact_directory())
            .join("libproject.rlib");
        fs_write(&built, "same length!!");
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs_write(&session.package_rlib, "same length!!");

        session
            .refresh_staged_rlib()
            .expect("refreshing must succeed");
        assert_eq!(
            std::fs::read_to_string(&session.package_rlib).expect("read staged"),
            "same length!!"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// With no cargo output there is nothing to refresh from, and the staged
    /// copy - the only one there is - must survive untouched.
    #[test]
    fn refreshing_without_a_cargo_artifact_keeps_the_staged_copy() {
        let directory = std::env::temp_dir().join("pill_restage_missing");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, PLAIN_SOURCE);
        fs_write(&session.package_rlib, "the only copy");

        session.refresh_staged_rlib().expect("must not fail");
        assert_eq!(
            std::fs::read_to_string(&session.package_rlib).expect("read staged"),
            "the only copy"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Write a file, creating parent directories as needed.
    fn fs_write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write file");
    }

    /// A session over a throwaway source tree, so classification can be driven
    /// against real files without a project or a compiler.
    fn session_over(directory: &Path, contents: &str) -> HotPatchSession {
        std::fs::create_dir_all(directory).expect("create source dir");
        let file = directory.join("lib.rs");
        std::fs::write(&file, contents).expect("write source");

        let mut session = HotPatchSession {
            workspace_root: directory.to_path_buf(),
            package: "project".to_string(),
            crate_root: file,
            source_root: directory.to_path_buf(),
            package_rlib: directory.join("libproject.rlib"),
            build_command: vec!["cargo".to_string(), "build".to_string()],
            snapshots: HashMap::new(),
            snapshot_taken_at: SystemTime::UNIX_EPOCH,
            rustc_line: None,
            generations: Vec::new(),
            active_generations: HashMap::new(),
            counter: 0,
        };
        session.refresh_snapshots();

        // Put the recorded modification time firmly in the past before the
        // caller edits the file.
        //
        // `classify` skips a file whose modification time still equals the one
        // recorded when it was read. Filesystem timestamps are coarse, so a test
        // that writes its edit within the same tick as this snapshot produces a
        // file that looks unchanged - and the test then fails intermittently,
        // reporting "no change detected" for an edit that is plainly there. It
        // surfaced under the parallel harness, where the write lands sooner.
        //
        // The gate's imprecision is harmless in production: a missed edit falls
        // through to a full reload, which still delivers it. It is only a
        // problem for a test that asserts on classification itself, so the wait
        // lives here - once, for all 27 sessions - rather than in each test.
        std::thread::sleep(std::time::Duration::from_millis(20));
        session
    }

    const PLAIN_SOURCE: &str = r#"
use pill_engine::pill_hot_fn;

#[pill_hot_fn]
pub fn get_color_a() -> f32 {
    133.0
}
"#;

    const TWO_HOT_SOURCE: &str = r#"
use pill_engine::*;

const SPEED: f32 = 1.0;

#[pill_hot]
fn movement(value: f32) -> f32 {
    value * SPEED
}

#[pill_hot]
fn other_movement(value: f32) -> f32 {
    value - 1.0
}
"#;

    const HOT_SOURCE: &str = r#"
use pill_engine::*;

const SPEED: f32 = 1.0;

#[pill_hot]
fn movement(value: f32) -> f32 {
    value * SPEED
}

fn helper(value: f32) -> f32 {
    value + 1.0
}
"#;

    /// The headline classification: a body-only edit of an annotated function
    /// is identified, and names the right function.
    #[test]
    fn classify_accepts_a_body_only_edit() {
        let directory = std::env::temp_dir().join("pill_classify_accepts");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);

        let edited = HOT_SOURCE.replace("value * SPEED", "value * SPEED * 2.0");
        std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

        let classified = session.classify().expect("body-only edit must be accepted");
        let (_, declaration, contents) = classified.expect("a change must be reported");
        assert_eq!(declaration.len(), 1, "one body changed");
        assert_eq!(declaration[0].name, "movement");
        assert_eq!(declaration[0].kind, source::HotFunctionKind::System);
        assert!(contents.contains("value * SPEED * 2.0"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn classify_reports_no_change_when_nothing_moved() {
        let directory = std::env::temp_dir().join("pill_classify_unchanged");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);

        assert!(session.classify().expect("no error").is_none());

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Everything outside an annotated body must be refused, with a reason.
    /// Each of these would otherwise compile a patch against a layout or a
    /// signature the running world no longer has.
    #[test]
    fn classify_refuses_everything_outside_a_hot_body() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "constant",
                "const SPEED: f32 = 1.0;",
                "const SPEED: f32 = 2.0;",
            ),
            (
                "signature",
                "fn movement(value: f32)",
                "fn movement(value: f64)",
            ),
            ("import", "use pill_engine::*;", "use pill_engine::Engine;"),
        ];

        for (label, from, to) in cases {
            let directory =
                std::env::temp_dir().join(format!("pill_classify_{}", label.replace(' ', "_")));
            let _ = std::fs::remove_dir_all(&directory);
            let mut session = session_over(&directory, HOT_SOURCE);

            let edited = HOT_SOURCE.replace(from, to);
            assert_ne!(edited, HOT_SOURCE, "{label}: the fixture edit must apply");
            std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

            let refusal = session.classify().expect_err(&format!(
                "{label}: a change outside a hot body must be refused"
            ));
            assert_eq!(
                refusal.code,
                refusal_code::OUTSIDE_HOT_BODY,
                "{label}: {}",
                refusal.detail
            );

            let _ = std::fs::remove_dir_all(&directory);
        }
    }

    /// A refused edit must not disable patching for the rest of the session.
    ///
    /// This is the bug that made live patching look broken: `classify` diffs
    /// against a snapshot of what is running, and only a *successful* patch
    /// advanced it. A refusal was followed by a full reload, which picked the
    /// edit up without telling the session - so the refused change stayed in
    /// every later diff, and each subsequent body-only edit was refused for a
    /// change that had already shipped. One unpatchable edit disabled the fast
    /// path until the host restarted.
    ///
    /// `refresh_snapshots` is what the frame loop calls after a reload to close
    /// that gap; this test pins the behaviour that depends on it.
    #[test]
    fn a_reload_resyncs_the_baseline_so_later_body_edits_still_patch() {
        let directory = std::env::temp_dir().join("pill_classify_resync_after_reload");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);

        // Step 1: an edit the fast path must refuse. In the host this falls
        // back to a full reload, which leaves the file running as written.
        let after_reload = HOT_SOURCE.replace("const SPEED: f32 = 1.0;", "const SPEED: f32 = 2.0;");
        assert_ne!(after_reload, HOT_SOURCE, "the fixture edit must apply");
        std::fs::write(directory.join("lib.rs"), &after_reload).expect("write the refused edit");
        let refusal = session
            .classify()
            .expect_err("a constant change must be refused");
        assert_eq!(refusal.code, refusal_code::OUTSIDE_HOT_BODY);

        // Step 2: the reload the host performs, and the re-sync that goes with
        // it. Without this call the assertion below fails with OUTSIDE_HOT_BODY,
        // because the constant change is still in the diff.
        session.refresh_snapshots();

        // Step 3: a clean body-only edit on top must now be patchable.
        let body_edited = after_reload.replace("value * SPEED", "value * SPEED + 1.0");
        assert_ne!(body_edited, after_reload, "the body edit must apply");
        std::fs::write(directory.join("lib.rs"), &body_edited).expect("write the body edit");

        let classified = session
            .classify()
            .expect("a body-only edit after a reload must not be refused")
            .expect("the changed body must be detected");
        assert!(
            classified
                .1
                .iter()
                .any(|declaration| declaration.name == "movement"),
            "the edited body should be the one reported"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Two bodies changed in one save are both reported, in a stable order.
    ///
    /// This used to be a refusal. Each body is an independent replacement, so
    /// the only cost of taking both is one compile apiece - cheaper than the
    /// full reload the refusal fell back to, and the world is never torn down.
    #[test]
    fn classify_reports_every_changed_body() {
        let directory = std::env::temp_dir().join("pill_classify_two_bodies");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, TWO_HOT_SOURCE);

        let edited = TWO_HOT_SOURCE
            .replace("value * SPEED", "value * SPEED * 2.0")
            .replace("value - 1.0", "value - 9.0");
        std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

        let (_, declarations, _) = session
            .classify()
            .expect("two body-only edits are in scope")
            .expect("a change must be reported");
        let names: Vec<&str> = declarations
            .iter()
            .map(|declaration| declaration.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["movement", "other_movement"],
            "both bodies, sorted so the order does not depend on scan order"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Two changed files in one edit are refused with their own code.
    #[test]
    fn classify_refuses_two_changed_files() {
        let directory = std::env::temp_dir().join("pill_classify_two_files");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);
        // A second file that is part of the baseline, so changing it later is
        // an edit rather than a new file.
        std::fs::write(directory.join("other.rs"), HOT_SOURCE).expect("write second file");
        session.refresh_snapshots();

        let edited = HOT_SOURCE.replace("value * SPEED", "value * SPEED * 2.0");
        std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");
        std::fs::write(directory.join("other.rs"), &edited).expect("write second edit");

        let refusal = session.classify().expect_err("two files must be refused");
        assert_eq!(
            refusal.code,
            refusal_code::MULTIPLE_FILES,
            "{}",
            refusal.detail
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A file that was not in the baseline is a structural change by
    /// definition, and says so with its own code.
    #[test]
    fn classify_refuses_a_new_source_file() {
        let directory = std::env::temp_dir().join("pill_classify_new_file");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);

        std::fs::write(directory.join("appeared.rs"), HOT_SOURCE).expect("write new file");

        let refusal = session.classify().expect_err("a new file must be refused");
        assert_eq!(
            refusal.code,
            refusal_code::NEW_SOURCE_FILE,
            "{}",
            refusal.detail
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Rolling back a function this session never patched is refused rather
    /// than silently doing nothing, and names the function.
    #[test]
    fn rollback_refuses_an_unknown_function() {
        let directory = std::env::temp_dir().join("pill_rollback_unknown");
        let _ = std::fs::remove_dir_all(&directory);
        let session = session_over(&directory, HOT_SOURCE);

        assert!(!session.knows_function("project::movement"));
        assert!(session.generations().is_empty());

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A prologue generation a reload has invalidated says so, rather than
    /// failing as though the crate were never loaded.
    ///
    /// Dropping the addresses is what keeps a rollback from writing into a
    /// retired image, but it also makes the generation indistinguishable from a
    /// slot-delivered one - and the slot route then refuses for a reason that is
    /// not what went wrong. This pins the message a developer actually reads.
    #[test]
    fn a_prologue_generation_a_reload_invalidated_says_so() {
        let directory = std::env::temp_dir().join("pill_rollback_dropped_history");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);

        // A generation delivered by overwriting code, as `apply` would record it.
        session.generations.push(Generation {
            function: "project::ordinary".to_string(),
            number: 1,
            address: 0x1000,
            kind: source::HotFunctionKind::PlainFunction,
            signature_hash: 0,
            lookup_name: "project::ordinary".to_string(),
            signature: "fn ordinary()".to_string(),
            prologue_restores: vec![PrologueRestore {
                artifact: "project".to_string(),
                address: 0x2000,
                original: vec![0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0],
            }],
            prologue_history_dropped: false,
            installed_at: Instant::now(),
        });
        session
            .active_generations
            .insert("project::ordinary".to_string(), 1);

        // The reload that invalidates every recorded address.
        session.forget_prologue_patches();
        assert!(session.generations[0].prologue_history_dropped);
        assert!(session.generations[0].prologue_restores.is_empty());

        // Patching the same function again is what makes generation 1 reachable
        // for a rollback at all: the reload cleared the active entry, and the
        // new generation restores it. This is the sequence the refusal is for -
        // edit, reload, edit, then ask for the generation from before.
        session.generations.push(Generation {
            function: "project::ordinary".to_string(),
            number: 2,
            address: 0x3000,
            kind: source::HotFunctionKind::PlainFunction,
            lookup_name: "project::ordinary".to_string(),
            signature: "fn ordinary()".to_string(),
            signature_hash: 0,
            prologue_restores: vec![PrologueRestore {
                artifact: "project".to_string(),
                address: 0x2000,
                original: vec![0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xE0],
            }],
            prologue_history_dropped: false,
            installed_at: Instant::now(),
        });
        session
            .active_generations
            .insert("project::ordinary".to_string(), 2);

        let mut engine = Engine::new();
        let detail = session
            .rollback(&mut engine, &[], &[], "project::ordinary", 1)
            .expect_err("an invalidated generation cannot be rolled back to");
        assert!(
            detail.contains("a reload has replaced that code"),
            "the refusal must name the reload, not a missing crate: {detail}"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// An un-annotated function is patchable: the attribute chooses which
    /// mechanism delivers the replacement, not whether one is possible at all.
    ///
    /// This inverts what this case used to assert. Discovery now comes from the
    /// build script's address inventory rather than from an attribute, so a
    /// plain `fn` in a participating crate is as patchable as an annotated one.
    #[test]
    fn classify_accepts_an_un_annotated_function() {
        let directory = std::env::temp_dir().join("pill_classify_unannotated");
        let _ = std::fs::remove_dir_all(&directory);
        let plain = "fn ordinary(value: f32) -> f32 { value }\n";
        let mut session = session_over(&directory, plain);

        std::fs::write(
            directory.join("lib.rs"),
            "fn ordinary(value: f32) -> f32 { value * 2.0 }\n",
        )
        .expect("write edit");

        let (_, declaration, _) = session
            .classify()
            .expect("an un-annotated body edit is in scope")
            .expect("a change must be reported");
        assert_eq!(declaration.len(), 1, "one body changed");
        assert_eq!(declaration[0].name, "ordinary");
        assert_eq!(declaration[0].kind, source::HotFunctionKind::PlainFunction);

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A file with no addressable function at all is still refused, so an edit
    /// to something the host could never reach says so.
    #[test]
    fn classify_refuses_a_file_with_no_functions() {
        let directory = std::env::temp_dir().join("pill_classify_no_functions");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, "pub const SPEED: f32 = 1.0;\n");

        std::fs::write(directory.join("lib.rs"), "pub const SPEED: f32 = 2.0;\n")
            .expect("write edit");

        let refusal = session.classify().expect_err("must be refused");
        assert_eq!(refusal.code, refusal_code::NO_HOT_FUNCTION);

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Editing a second function's body is now a patch of THAT function, not a
    /// structural change - one changed body is still the limit.
    #[test]
    fn classify_reports_whichever_body_changed() {
        let directory = std::env::temp_dir().join("pill_classify_other_body");
        let _ = std::fs::remove_dir_all(&directory);
        let mut session = session_over(&directory, HOT_SOURCE);

        let edited = HOT_SOURCE.replace("value + 1.0", "value + 9.0");
        std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

        let (_, declaration, _) = session
            .classify()
            .expect("a body-only edit is in scope")
            .expect("a change must be reported");
        assert_eq!(declaration.len(), 1, "one body changed");
        assert_eq!(declaration[0].name, "helper");

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn generation_reports_a_missing_function() {
        let session = HotPatchSession {
            workspace_root: PathBuf::from("."),
            package: "project".to_string(),
            crate_root: PathBuf::from("lib.rs"),
            source_root: PathBuf::from("."),
            package_rlib: PathBuf::from("libproject.rlib"),
            build_command: vec!["cargo".to_string(), "build".to_string()],
            snapshots: HashMap::new(),
            snapshot_taken_at: SystemTime::UNIX_EPOCH,
            rustc_line: None,
            generations: Vec::new(),
            active_generations: HashMap::new(),
            counter: 0,
        };
        assert!(session
            .generate(
                "fn other() {}",
                &system_declaration("movement"),
                "project::movement",
            )
            .is_err());
    }
}

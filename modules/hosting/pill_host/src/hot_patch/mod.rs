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
use std::time::SystemTime;

// External crates
use pill_core::platform::Instant;
use pill_core::{debug, info, warn};
use pill_engine::Engine;

// Current crate
/// Patch compilation: the rustc invocation and its artifacts.
pub(crate) mod compile;
/// Patch generations: the history a route can lead back through.
pub(crate) mod generations;
/// Patch routes: where a body is installed and how calls reach it.
pub(crate) mod routes;
/// Patch builds: compiling and loading a patch away from the frame thread.
mod worker;

// A function's patch history and the routes back through it.
pub(crate) use generations::Generation;
pub use generations::PatchGeneration;

// The captured compiler line and its cache, re-exported for the build runner:
// a module build harvests the same line this pipeline would otherwise pay a
// second `cargo build -v` to discover.
pub(crate) use compile::{flags_cache_path, parse_rustc_line};

// One background build's pieces, re-exported for the frame loop that starts a
// build and installs its product at a later frame boundary.
pub(crate) use worker::{build_attempt, BodyOutcome, PatchAttempt};

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

pub(crate) use compile::CargoRustcLine;

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
    /// An associated function without a receiver was edited: the generated
    /// patch has no `Self` to compile its body against.
    pub const ASSOCIATED_WITHOUT_RECEIVER: &str = "associated-without-receiver";
    /// Two addressable declarations in one file share a bare name.
    pub const DUPLICATE_DECLARATION: &str = "duplicate-declaration";
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

/// What starting a patch attempt found, and whether a build is owed.
///
/// The frame loop reads these the same way it read the in-line pipeline's
/// outcomes: `Unchanged` and `NotPatchable` leave the pending edit for the
/// reload path, `Failed` reports what is still running, and `Ready` hands the
/// build to a worker whose product is installed by
/// [`HotPatchSession::activate_attempt`].
pub(crate) enum BeginOutcome {
    /// Nothing relevant changed.
    Unchanged,
    /// The change is real but out of scope; the caller should reload normally.
    NotPatchable {
        /// Stable code and the sentence explaining it.
        refusal: PatchRefusal,
    },
    /// Preparing the replacement failed; the caller should reload normally.
    Failed {
        /// Which function was being prepared.
        function: String,
        /// Which generation is still running, so the console says what the
        /// process is executing rather than only what did not happen.
        active_generation: u32,
        /// Stable code and the first line of the failure.
        failure: PatchRefusal,
    },
    /// A build is owed: hand it to a worker, then activate its product.
    Ready(worker::PatchAttempt),
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

impl PatchStages {
    /// Add one body's timings into the running total for the whole save.
    ///
    /// `classify` is measured once per save rather than once per body, so the
    /// per-body structs leave it at zero and adding it changes nothing.
    pub fn merge(&mut self, other: &PatchStages) {
        self.classify += other.classify;
        self.generate += other.generate;
        self.flags += other.flags;
        self.compile += other.compile;
        self.load += other.load;
        self.activate += other.activate;
    }
}

// =============================================================================
// HotPatchSession
// =============================================================================

/// What one successfully installed body produced.
///
/// Named for the install rather than for the whole attempt: one attempt may
/// carry several bodies, and the generation number, artifact size and route
/// below describe this one.
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

/// One file's in-scope edit, with every field the steps after classification
/// need.
///
/// The four fields travel together for the same reason [`Snapshot`] is one
/// value: the modification time is when *these* contents were read. A step that
/// re-derives either half from a later stat pairs one observation's bytes with
/// another's timestamp - which is how a save made during the patch came to be
/// recorded as already delivered.
#[derive(Debug)]
struct ClassifiedEdit {
    /// The changed file.
    path: PathBuf,
    /// Its changed bodies, sorted by name.
    declarations: Vec<source::HotFunction>,
    /// The file's contents as read during classification.
    new_contents: String,
    /// The file's modification time when those contents were read, or `None`
    /// when the filesystem would not report one.
    modified: Option<SystemTime>,
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
        // The crate root is `src/lib.rs` unless the manifest's `[lib] path`
        // moves it, which extensions do so their file name is unique
        // across the dependency graph. Resolved through the same helper the
        // build script uses, because a root mistaken for a module would prefix
        // every patch name with a segment the inventory does not carry.
        let crate_root = source_root
            .parent()
            .map(source::crate_root_file)
            .unwrap_or_else(|| source_root.join("lib.rs"));
        // The staged copy, not cargo's per-crate slot. Both the project and
        // every extension write their `rlib` to an unhashed path that any
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
            // A warning: whether the fast path is on is the first thing a
            // developer needs to know, and an INFO line is easy to lose among
            // the startup output.
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = package,
                expected = %session.package_rlib.display(),
                "hot patching is idle: the project rlib a patch links to reach the \
                 crate's types was not built"
            );
            return None;
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = package,
            functions = addressable,
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

    /// Prepare a build for a pending source change, without compiling it.
    ///
    /// Everything here stays on the calling thread and touches only this
    /// session and the filesystem: classification, source generation, the
    /// staged rlib, the replayed compiler line. None of it touches the engine
    /// or a loaded artifact, which is what lets the caller hand the result to
    /// another thread and install it at a later frame boundary.
    ///
    /// The expensive half - `rustc` and `LoadLibrary` - is
    /// [`worker::build_attempt`]'s job; the product is installed by
    /// [`Self::activate_attempt`].
    pub(crate) fn begin_attempt(&mut self) -> BeginOutcome {
        let started = Instant::now();
        let mut stages = PatchStages::default();

        // Step 1: Find which functions' bodies changed, if the edit is
        // body-only and nothing else moved.
        let classify_started = Instant::now();
        let classified = self.classify();
        stages.classify = classify_started.elapsed().as_secs_f64() * 1000.0;

        let edit = match classified {
            Ok(Some(found)) => found,
            Ok(None) => return BeginOutcome::Unchanged,
            Err(refusal) => return BeginOutcome::NotPatchable { refusal },
        };
        let path = edit.path;

        // Step 2: Generate each changed body's source and collect its build
        // inputs. A failure here leaves nothing of this attempt live - the
        // running implementation is untouched - so the caller reloads, the
        // same outcome the in-line pipeline reported for it.
        let mut bodies = Vec::with_capacity(edit.declarations.len());
        for declaration in edit.declarations {
            // The path the running artifact recorded for this function, which
            // is what both the engine registry and a slot are keyed by.
            // Derived from the file's position under the source root, so a
            // function in a submodule resolves as `crate::module::function`.
            let qualified = self.qualified_name(&path, &declaration);
            // The slot route asks under a different name; see
            // `slot_lookup_name`.
            let slot_name = self.slot_lookup_name(&path, &declaration);
            match self.prepare_body(
                &edit.new_contents,
                &declaration,
                &qualified,
                &slot_name,
                &mut stages,
            ) {
                Ok(body) => bodies.push(body),
                // The running implementation is untouched, so the console
                // reports which generation is still executing rather than
                // only what failed.
                Err(failure) => {
                    return BeginOutcome::Failed {
                        active_generation: self.active_generation(&qualified),
                        function: qualified,
                        failure,
                    };
                }
            }
        }

        BeginOutcome::Ready(worker::PatchAttempt {
            path,
            new_contents: edit.new_contents,
            modified: edit.modified,
            bodies,
            stages,
            started,
        })
    }

    // -------------------------------------------------------------------------
    // Classification
    // -------------------------------------------------------------------------

    /// Identify a body-only edit of exactly one annotated function.
    ///
    /// `Ok(None)` means nothing changed. `Err` carries the reason the change is
    /// out of scope, phrased for the console. A match carries the file's
    /// contents and the modification time read together, so the snapshot the
    /// caller writes describes one observation rather than two.
    fn classify(&mut self) -> Result<Option<ClassifiedEdit>, PatchRefusal> {
        let mut result: Option<ClassifiedEdit> = None;

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
            let functions = source::all_functions(&new_contents);
            // Two declarations sharing a bare name cannot be told apart by
            // the machinery below: the map keeps the *last* declaration while
            // `function_bodies` keeps the *first*, so an edit to the first
            // would install the second's body under the first's address. The
            // scanner returns both deliberately - deduplicating by bare name
            // once silently dropped declarations - which is why the collision
            // is refused here, matching the module doc.
            {
                let mut seen: std::collections::HashSet<&str> =
                    std::collections::HashSet::with_capacity(functions.len());
                if let Some(duplicate) = functions
                    .iter()
                    .map(|function| function.name.as_str())
                    .find(|name| !seen.insert(name))
                {
                    return Err(PatchRefusal::new(
                        refusal_code::DUPLICATE_DECLARATION,
                        format!(
                            "{} declares two functions named `{duplicate}`; rename one of \
                             them or split them across files",
                            file_label(&path)
                        ),
                    ));
                }
            }
            let hot: HashMap<String, source::HotFunction> = functions
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
            // diagnostics precise. Both revisions are scanned once for every
            // body at a time rather than once per function: asking about each
            // function separately rebuilt the file's code mask on every
            // question, which made classification of a 500-line file cost tens
            // of milliseconds on every save.
            let old_bodies = source::function_bodies(old_contents, &hot_names);
            let new_bodies = source::function_bodies(&new_contents, &hot_names);
            let mut changed: Vec<String> = Vec::new();
            for name in &hot_names {
                if old_bodies.get(name) != new_bodies.get(name) {
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
                // A receiver-less associated function has no `Self` to carry:
                // the generated patch emits the body at the top level, where
                // `Self` cannot be named, so the compile is doomed. Refused
                // here the outcome is a cheap `NotPatchable` instead of a patch
                // failure plus a full reload.
                if !declaration.takes_receiver
                    && (declaration.self_type.is_some() || declaration.trait_name.is_some())
                {
                    let owner = match (&declaration.trait_name, &declaration.self_type) {
                        (Some(trait_name), Some(self_type)) => {
                            format!("`{trait_name}` for `{self_type}`")
                        }
                        (Some(trait_name), None) => format!("the `{trait_name}` trait"),
                        (None, Some(self_type)) => format!("`{self_type}`"),
                        (None, None) => "its `impl` block".to_string(),
                    };
                    return Err(PatchRefusal::new(
                        refusal_code::ASSOCIATED_WITHOUT_RECEIVER,
                        format!(
                            "`{function}` is an associated function without a receiver in \
                             {owner}; a patch has no `Self` to compile its body against"
                        ),
                    ));
                }
                declarations.push(declaration);
            }
            result = Some(ClassifiedEdit {
                path,
                declarations,
                new_contents,
                modified,
            });
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

    /// Generate one body's patch source and collect everything its build
    /// needs, up to but not including running `rustc`.
    ///
    /// `slot_name` is carried, not read: activation files the slot route
    /// under it. `totals` receives this body's timings once preparation
    /// succeeds, so a failed body's partial cost stays out of the report -
    /// the same rule the in-line pipeline had.
    fn prepare_body(
        &mut self,
        new_contents: &str,
        declaration: &source::HotFunction,
        qualified: &str,
        slot_name: &str,
        totals: &mut PatchStages,
    ) -> Result<worker::PreparedBody, PatchRefusal> {
        let mut stages = PatchStages::default();
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

        // Keep the crate's own rlib in step with the dependency rlibs the
        // replayed line names, so the two halves of the link closure agree.
        self.refresh_staged_rlib()
            .map_err(|detail| PatchRefusal::new(failure_code::PREPARE, detail))?;
        let package = self.package.clone();

        let artifact_path = scratch.join(format!("{crate_name}.dll"));
        let extra_externs = vec![format!("{}={}", self.package, self.package_rlib.display())];
        // Dependency rlibs staged when the host last built this package, which
        // is the only set guaranteed to match the module rlib being linked. The
        // shared `deps` slots they were copied from are overwritten by any build
        // of the same crate name, a differently-featured one included.
        let staged_dependencies = self
            .workspace_root
            .join(crate::build_runner::STAGED_DEPENDENCY_SUBDIRECTORY);
        let flags_started = Instant::now();
        // Cloned out of the cache rather than borrowed: the build that replays
        // it runs on another thread, which cannot hold a borrow of this
        // session.
        let line = self
            .rustc_line()
            .map_err(|detail| PatchRefusal::new(failure_code::PREPARE, detail))?
            .clone();
        stages.flags = flags_started.elapsed().as_secs_f64() * 1000.0;
        totals.merge(&stages);

        Ok(worker::PreparedBody {
            declaration: declaration.clone(),
            qualified: qualified.to_string(),
            slot_name: slot_name.to_string(),
            crate_name,
            source_path,
            artifact_path,
            extra_externs,
            staged_dependencies,
            line,
            package,
        })
    }

    /// Install a finished build at the frame boundary.
    ///
    /// The compile and the load ran on another thread; `outcomes` is its
    /// report, one entry per body in order, stopping at the first failure.
    /// Everything that touches the engine, the loaded artifacts or a dispatch
    /// slot happens here, on the thread that owns the frame boundary - a
    /// requirement rather than a preference, because the prologue route
    /// rewrites live code.
    pub(crate) fn activate_attempt(
        &mut self,
        engine: &mut Engine,
        targets: &[(&str, &NativeLibrary)],
        patches: &mut Vec<LoadedPatch>,
        attempt: worker::PatchAttempt,
        outcomes: Vec<worker::BodyOutcome>,
    ) -> PatchOutcome {
        let worker::PatchAttempt {
            path,
            new_contents,
            modified,
            bodies,
            mut stages,
            started,
        } = attempt;
        let mut last: Option<ApplyResult> = None;
        let mut patched_names: Vec<String> = Vec::new();
        // Several bodies in one file are patched in sequence and need not share
        // a route: an annotated and an un-annotated function in the same save
        // take different ones. Both are reported, because an edit is only as
        // provable as its weakest body.
        let mut routes: Vec<crate::analytics::PatchRoute> = Vec::new();
        let mut copies = 0usize;
        for (body, outcome) in bodies.into_iter().zip(outcomes) {
            let installed = match outcome {
                worker::BodyOutcome::Built(built) => {
                    match self.install_body(engine, targets, patches, &body, built, &mut stages) {
                        Ok(installed) => installed,
                        // The running implementation is untouched, so the
                        // console reports which generation is still executing
                        // rather than only what failed.
                        Err(failure) => {
                            return PatchOutcome::Failed {
                                active_generation: self.active_generation(&body.qualified),
                                function: body.qualified,
                                failure,
                            };
                        }
                    }
                }
                worker::BodyOutcome::Failed(failure) => {
                    return PatchOutcome::Failed {
                        active_generation: self.active_generation(&body.qualified),
                        function: body.qualified,
                        failure,
                    };
                }
            };
            patched_names.push(body.qualified);
            if !routes.contains(&installed.route) {
                routes.push(installed.route);
            }
            copies += installed.copies;
            last = Some(installed);
        }

        let Some(installed) = last else {
            return PatchOutcome::Unchanged;
        };

        // Only record the new contents once every body is live, so a partial
        // failure is retried on the next change rather than treated as done.
        // The time recorded is the one read WITH these contents, never a fresh
        // stat: a save that landed during the compile must leave the snapshot
        // stale, or the next attempt skips it as already delivered.
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

    /// Install one built patch: park the image, resolve the replacement and
    /// write it into the running artifact(s).
    ///
    /// One iteration of what used to be `apply`, from the mapped image to the
    /// recorded generation. The compile, the load and the artifact inspection
    /// that used to precede it run on a worker thread now; what remains is
    /// exactly the part that must happen at the frame boundary.
    fn install_body(
        &mut self,
        engine: &mut Engine,
        targets: &[(&str, &NativeLibrary)],
        patches: &mut Vec<LoadedPatch>,
        body: &worker::PreparedBody,
        built: worker::BuiltBody,
        totals: &mut PatchStages,
    ) -> Result<ApplyResult, PatchRefusal> {
        let declaration = &body.declaration;
        let function = declaration.name.as_str();
        let kind = declaration.kind;
        let qualified = body.qualified.as_str();
        let slot_name = body.slot_name.as_str();
        let crate_name = body.crate_name.as_str();
        let mut stages = PatchStages::default();
        let activate_started = Instant::now();

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
        // Park the image before a single byte is written. `Library` unmaps on
        // drop, and both routes below can refuse early - a stub already written
        // into another artifact names an address inside this image, so a
        // refusal must leave it owned rather than free it. The image is
        // process-wide, not owned by this session: a later patch of a
        // different crate has to reach the copies inside it. The routes below
        // see the graveyard *without* this entry, so a patch image is never
        // offered its own replacement.
        patches.push(LoadedPatch {
            function: qualified.to_string(),
            generation,
            library: built.library,
        });
        let library = &patches[patches.len() - 1].library;
        let existing_patches = &patches[..patches.len() - 1];

        // Install at the frame boundary. Both routes refuse a signature that no
        // longer matches, so a reshaped function can never be applied behind a
        // call site compiled for the old shape.
        match kind {
            // A system is dispatched through the engine's own registry, which
            // lives once in this process because the engine is one shared
            // library. One install reaches every caller.
            source::HotFunctionKind::System => {
                let lookup_name = format!("{PATCH_NAME_PREFIX}{qualified}");
                let (found, hash) = resolve_in(library, &lookup_name)
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
                let found = resolve_patch_address(library)
                    .map_err(|detail| PatchRefusal::new(failure_code::RESOLVE, detail))?;
                prologue_restores = prologue_patch_everywhere(
                    targets,
                    existing_patches,
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
                let (found, found_signature) = resolve_plain_in(library, &lookup_name)
                    .map_err(|detail| PatchRefusal::new(failure_code::RESOLVE, detail))?;
                copies = install_everywhere(
                    targets,
                    existing_patches,
                    slot_name,
                    found,
                    &found_signature,
                )
                .map_err(|detail| PatchRefusal::new(failure_code::INSTALL, detail))?;
                address = found;
                signature = found_signature;
                route = crate::analytics::PatchRoute::ArtifactSlot;
            }
        }
        stages.activate = activate_started.elapsed().as_secs_f64() * 1000.0;

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
        self.active_generations
            .insert(qualified.to_string(), generation);
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            function = qualified,
            generation,
            generations = self.generations.len(),
            "patch generation installed"
        );
        totals.merge(&stages);

        Ok(ApplyResult {
            generation,
            artifact_bytes: built.artifact_bytes,
            exports: built.exports,
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
    ///
    /// The crate root contributes no segment whatever it is named: a manifest
    /// may point `[lib] path` at a file other than `lib.rs`, and treating that
    /// file as a module would name every function in the crate one segment too
    /// deep.
    fn module_segments(&self, path: &Path) -> Vec<String> {
        let mut segments = vec![self.package.clone()];
        if path == self.crate_root {
            return segments;
        }
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
        // A wrapper build compiles the extension as a dependency, so the
        // newest rlib under the build's `deps` directory is the one that
        // build produced; a direct build leaves an unhashed copy the same
        // helper finds.
        let Some(built) = crate::build_runner::newest_extension_rlib(
            &self
                .workspace_root
                .join(crate::config::module_build_artifact_directory()),
            &self.package,
        ) else {
            // Cargo has not produced one; the staged copy is all there is.
            return Ok(());
        };
        let Ok(built_metadata) = std::fs::metadata(&built) else {
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
            let cache = compile::flags_cache_path(&self.workspace_root, &self.package);
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
            // module reload, or whenever the engine feature set the build
            // resolves with changes (a host rebuilt with profiling or another
            // engine feature moves it; a cache captured against one feature set
            // goes silently stale against another and rustc reports
            // `error[E0463]` when the patch tries to link the freshly staged
            // rlib against the old externs). The rlib's mtime moves exactly
            // when that happens, so keying on it re-captures precisely when the
            // world changed and stays hot across plain source edits and
            // patches.
            //
            // The staged copy, not a build-tree path: a wrapper build leaves
            // the extension's rlib under a hashed name the session cannot
            // know, and the staged file is refreshed from it right before this
            // runs - it is also what the patch actually links.
            let mut freshness = vec![
                self.workspace_root.join("Cargo.toml"),
                self.workspace_root.join("Cargo.lock"),
            ];
            freshness.push(self.package_rlib.clone());
            // The cargo configuration and the pinned toolchain decide the same
            // flags from outside the manifest: a wrapper, a target directory
            // override, or a toolchain switch changes what a replayed line
            // means without moving `Cargo.toml` or the lockfile. Added only
            // when they exist, because `load_if_fresh` refuses an unreadable
            // input and an unconditional push would disable the cache in
            // workspaces that have neither file.
            for relative in [".cargo/config.toml", "rust-toolchain.toml"] {
                let input = self.workspace_root.join(relative);
                if input.exists() {
                    freshness.push(input);
                }
            }
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
mod tests;

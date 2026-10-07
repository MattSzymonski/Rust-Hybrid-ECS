//! The background half of the fast patch: compile, load, pre-warm.
//!
//! # Responsibilities
//!
//! - Own everything one patch build needs, so it can travel to another thread
//!   and back without borrowing the frame, the engine or the session.
//! - Run the replayed `rustc` invocation and map the produced library, reading
//!   the artifact once first so the platform's scan of a freshly written file
//!   happens here rather than at the frame boundary.
//!
//! # Design
//!
//! A patch build - one `rustc` invocation plus one `LoadLibrary` - is the
//! hundreds of milliseconds that used to freeze the frame loop on every save.
//! The frame thread prepares a [`PatchAttempt`], a worker turns it into
//! [`BodyOutcome`]s, and the frame boundary installs them. Nothing in this
//! file touches the engine, a loaded artifact or the session, which is what
//! makes the middle step safe anywhere: no dispatch slot is written, no
//! registry changes, nothing that belongs to the thread owning the boundary.

// Standard library
use std::path::PathBuf;
use std::time::SystemTime;

// External crates
use libloading::Library;
use pill_core::debug;
use pill_core::platform::Instant;

// Current crate
use super::compile::CargoRustcLine;
use super::{failure_code, source, PatchRefusal, PatchStages};

/// One pending patch attempt: what the frame thread prepared, and what a
/// worker or the activation still needs.
///
/// Owns its inputs so it can outlive the frame's borrows while the build runs;
/// nothing here points into the engine, an artifact or the session.
pub(crate) struct PatchAttempt {
    /// The changed file, for the snapshot recorded once every body is live.
    pub(super) path: PathBuf,
    /// The file's contents as classification read them.
    pub(super) new_contents: String,
    /// The modification time when those contents were read.
    pub(super) modified: Option<SystemTime>,
    /// One entry per changed body, in the order they were classified.
    pub(super) bodies: Vec<PreparedBody>,
    /// Timings so far; the worker and the activation add theirs.
    pub(super) stages: PatchStages,
    /// When the attempt began, so the LIVE report spans preparation, the
    /// build and the wait for the next frame boundary. The frame loop also
    /// spans its analytics total from here: the build runs between frames, so
    /// the frame that collects the attempt cannot time the total itself.
    pub(crate) started: Instant,
}

/// One changed body, ready to compile.
pub(crate) struct PreparedBody {
    /// The declaration as the scanner reported it.
    pub(super) declaration: source::HotFunction,
    /// The name the engine registry and the prologue route are keyed by.
    pub(super) qualified: String,
    /// The name a dispatch slot is keyed by, which omits the receiver type.
    /// Carried rather than read during preparation: activation files the slot
    /// route under it.
    pub(super) slot_name: String,
    /// The patch crate's name, unique within the process.
    pub(super) crate_name: String,
    /// The generated source on disk.
    pub(super) source_path: PathBuf,
    /// Where the compiled library is written.
    pub(super) artifact_path: PathBuf,
    /// `--extern` pairs beyond the ones the replayed line carries.
    pub(super) extra_externs: Vec<String>,
    /// Where the dependency rlibs the replayed line names are staged.
    pub(super) staged_dependencies: PathBuf,
    /// The captured compiler invocation to replay.
    pub(super) line: CargoRustcLine,
    /// The package name, for the "can't find crate" hint.
    pub(super) package: String,
}

/// What the build produced for one body.
pub(crate) enum BodyOutcome {
    /// Compiled and mapped; the frame boundary installs it.
    Built(BuiltBody),
    /// The build failed. The running implementation is untouched, and every
    /// body after this one was not attempted - the pipeline's stop-at-first-
    /// failure rule, kept so a partially delivered save behaves as it did.
    Failed(PatchRefusal),
}

/// A compiled and loaded patch image.
pub(crate) struct BuiltBody {
    /// The mapped library. Never unloaded: a dispatch slot will point inside
    /// it for the rest of the process.
    pub(super) library: Library,
    /// Size on disk, for the analytics line.
    pub(super) artifact_bytes: u64,
    /// Milliseconds spent in `rustc`.
    pub(super) compile_ms: f64,
    /// Milliseconds spent mapping the image.
    pub(super) load_ms: f64,
}

/// Compile and map every body of one attempt.
///
/// Runs on the build thread. Stops at the first failure, exactly as the
/// in-line pipeline did: the bodies before it are still installable, and the
/// snapshot is not advanced unless every body installs.
pub(crate) fn build_attempt(mut attempt: PatchAttempt) -> (PatchAttempt, Vec<BodyOutcome>) {
    let mut outcomes = Vec::with_capacity(attempt.bodies.len());
    for body in &attempt.bodies {
        match build_body(body) {
            Ok(built) => {
                let stages = PatchStages {
                    compile: built.compile_ms,
                    load: built.load_ms,
                    ..PatchStages::default()
                };
                attempt.stages.merge(&stages);
                outcomes.push(BodyOutcome::Built(built));
            }
            Err(failure) => {
                outcomes.push(BodyOutcome::Failed(failure));
                break;
            }
        }
    }
    (attempt, outcomes)
}

/// Run the replayed compiler over one body and map its product.
fn build_body(body: &PreparedBody) -> Result<BuiltBody, PatchRefusal> {
    let source_path = body.source_path.as_path();
    let artifact_path = body.artifact_path.as_path();
    let crate_name = body.crate_name.as_str();
    let staged_dependencies = Some(body.staged_dependencies.as_path());

    let compile_started = Instant::now();
    let output = body
        .line
        .replay(
            source_path,
            artifact_path,
            crate_name,
            &body.extra_externs,
            staged_dependencies,
        )
        .output()
        .map_err(|error| {
            PatchRefusal::new(failure_code::COMPILE, format!("cannot run rustc: {error}"))
        })?;
    let compile_ms = compile_started.elapsed().as_secs_f64() * 1000.0;
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
        let package = body.package.as_str();
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
            command = body
                .line
                .replay_args(
                    source_path,
                    artifact_path,
                    crate_name,
                    &body.extra_externs,
                    staged_dependencies,
                )
                .join(" ")
                .as_str(),
            "the patch compile that failed"
        );
        return Err(PatchRefusal::new(failure_code::COMPILE, detail));
    }

    // Read the freshly written image once, on this thread, before loading it.
    // The first read of a new multi-megabyte file pays the platform's
    // anti-malware scan - measured at 93-108 ms for a project-sized patch and
    // 7.8 ms once warm - and paying it here keeps it off the frame boundary;
    // the `LoadLibrary` below then maps a page-cached file. The read is
    // deliberately unchecked: a file that cannot be read will fail the load
    // with a better error than this could produce.
    let _ = std::fs::read(artifact_path);

    let load_started = Instant::now();
    // SAFETY: the file was just produced by rustc from a generated source and
    // is a complete native module.
    let library = unsafe { Library::new(artifact_path) }.map_err(|error| {
        PatchRefusal::new(
            failure_code::LOAD,
            format!("cannot load the patch library: {error}"),
        )
    })?;
    let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;

    // Measured before installing, so the analytics line carries the same
    // artifact size a module reload does. Best-effort: a patch is still
    // correct when its size cannot be read.
    let artifact_bytes = std::fs::metadata(artifact_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    Ok(BuiltBody {
        library,
        artifact_bytes,
        compile_ms,
        load_ms,
    })
}

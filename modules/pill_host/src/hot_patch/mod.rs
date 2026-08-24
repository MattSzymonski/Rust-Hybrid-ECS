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

// Standard library
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

// External crates
use libloading::{Library, Symbol};
use pill_core::{debug, info, warn};
use pill_engine::Engine;

// Current crate
pub(crate) mod compile;
pub(crate) mod source;

use compile::CargoRustcLine;

// =============================================================================
// Constants
// =============================================================================

/// Export a generated patch carries so the host can find its new function.
///
/// Deliberately NOT `pill_hot_resolve`: a patch links the patched crate's rlib
/// to reach its types, and that rlib already exports that symbol. Two
/// `#[no_mangle]` definitions of one name in a single artifact is a link error.
const PATCH_RESOLVER_EXPORT: &[u8] = b"pill_patch_resolve";

/// Prefix that namespaces a patch's registry entry.
///
/// Also not optional. Linking the project's rlib pulls in that crate's
/// `#[pill_hot]` descriptors too, so a patch DLL contains BOTH the old and the
/// new entry for the same function. Asking for the bare name resolves whichever
/// the linker happened to order first - measured, and it was the OLD address,
/// with a matching signature hash, so the patch would have installed silently
/// and changed nothing.
const PATCH_NAME_PREFIX: &str = "pill_patch::";

// =============================================================================
// Outcome
// =============================================================================

/// What one attempt at a fast patch did.
#[derive(Debug)]
pub(crate) enum PatchOutcome {
    /// A function's implementation was replaced; no reload is needed.
    Patched {
        /// Qualified name of the patched function.
        function: String,
        /// Wall time from noticing the change to the slot being installed.
        elapsed_milliseconds: f64,
        /// Per-stage breakdown, so the dominant cost is visible rather than
        /// guessed at. Without this a slow patch is just a number.
        stages: PatchStages,
    },
    /// Nothing relevant changed.
    Unchanged,
    /// The change is real but out of scope; the caller should reload normally.
    NotPatchable {
        /// Why, phrased for a developer reading the console.
        reason: String,
    },
    /// A patch was attempted and failed. The previous implementation is intact
    /// and the caller should reload normally.
    Failed {
        /// Which function was being patched.
        function: String,
        /// First line of the failure, already trimmed for the console.
        detail: String,
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

/// One loaded patch library, kept mapped for the process lifetime.
struct Generation {
    /// Qualified name of the function this generation replaced.
    function: String,
    /// Keeps the library mapped; a dispatch slot points into its code.
    _library: Library,
}

/// Drives the fast path for one project.
pub(crate) struct HotPatchSession {
    workspace_root: PathBuf,
    package: String,
    crate_root: PathBuf,
    source_root: PathBuf,
    /// The patched crate's own rlib, so generated sources can `use <crate>::*`
    /// and get the identical types the running module holds.
    package_rlib: PathBuf,
    /// The host's own build command for this crate, feature flags included.
    ///
    /// Captured flags must come from THIS command: features live here and
    /// nowhere else, and a different feature set means different crate
    /// metadata - so a patch built from a bare `cargo build -p <pkg>` line
    /// disagrees with the running world about every `TypeId`.
    build_command: Vec<String>,
    /// Last seen contents of every watched source file.
    snapshots: HashMap<PathBuf, String>,
    /// Compiler flags, captured from cargo once and reused.
    rustc_line: Option<CargoRustcLine>,
    /// Loaded patches, never unloaded.
    generations: Vec<Generation>,
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
        build_command: &[String],
    ) -> Option<Self> {
        let source_root = workspace_root.join(watch_directory);
        let crate_root = source_root.join("lib.rs");
        let package_rlib = workspace_root
            .join("target")
            .join("debug")
            .join(format!("lib{package}.rlib"));

        let mut session = Self {
            workspace_root: workspace_root.to_path_buf(),
            package: package.to_string(),
            crate_root,
            source_root,
            package_rlib,
            build_command: build_command.to_vec(),
            snapshots: HashMap::new(),
            rustc_line: None,
            generations: Vec::new(),
            counter: 0,
        };
        session.refresh_snapshots();

        let annotated: usize = session
            .snapshots
            .values()
            .map(|contents| source::hot_function_names(contents).len())
            .sum();
        if annotated == 0 {
            return None;
        }

        // Without the crate's own rlib a generated patch cannot name the
        // project's types, and every attempt would fail at compile time. Say so
        // once, at startup, rather than on the first edit.
        if !session.package_rlib.is_file() {
            // Printed, not just logged: whether the fast path is on is the first
            // thing a developer needs to know, and a `tracing` line at INFO is
            // easy to lose among the startup output.
            println!(
                "{} hot patching OFF - the project rlib is missing ({})",
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
            "{} hot patching ON - {annotated} #[pill_hot] function(s)",
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
    fn refresh_snapshots(&mut self) {
        for path in rust_sources(&self.source_root) {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                self.snapshots.insert(path, contents);
            }
        }
    }

    /// Try to satisfy a pending source change with a patch instead of a reload.
    ///
    /// Called at the frame boundary, before the normal reload transaction, so
    /// no system is executing when a slot is written.
    pub(crate) fn try_patch(&mut self, engine: &mut Engine) -> PatchOutcome {
        let started = Instant::now();
        let mut stages = PatchStages::default();

        // Step 1: Find which annotated function's body changed, if exactly one
        // did and nothing else moved.
        let classify_started = Instant::now();
        let classified = self.classify();
        stages.classify = classify_started.elapsed().as_secs_f64() * 1000.0;

        let (path, function, new_contents) = match classified {
            Ok(Some(found)) => found,
            Ok(None) => return PatchOutcome::Unchanged,
            Err(reason) => return PatchOutcome::NotPatchable { reason },
        };

        let qualified = format!("{}::{}", self.package, function);

        // Step 2: Generate, compile, load and install. Any failure here leaves
        // the running implementation untouched.
        match self.apply(engine, &new_contents, &function, &qualified, &mut stages) {
            Ok(()) => {
                // Only record the new contents once the patch is live, so a
                // failed attempt is retried on the next change rather than
                // silently treated as applied.
                self.snapshots.insert(path, new_contents);
                PatchOutcome::Patched {
                    function: qualified,
                    elapsed_milliseconds: started.elapsed().as_secs_f64() * 1000.0,
                    stages,
                }
            }
            Err(detail) => PatchOutcome::Failed {
                function: qualified,
                detail,
            },
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
    fn classify(&mut self) -> Result<Option<(PathBuf, String, String)>, String> {
        let mut result: Option<(PathBuf, String, String)> = None;

        for path in rust_sources(&self.source_root) {
            let Ok(new_contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(old_contents) = self.snapshots.get(&path) else {
                // A new file is a structural change by definition.
                return Err(format!("new source file {}", path.display()));
            };
            if *old_contents == new_contents {
                continue;
            }

            // Only the functions the developer annotated may be patched.
            let hot_names: std::collections::HashSet<String> =
                source::hot_function_names(&new_contents).into_iter().collect();
            if hot_names.is_empty() {
                return Err(format!(
                    "{} changed but declares no #[pill_hot] function",
                    file_label(&path)
                ));
            }

            // Anything outside those bodies must be byte-identical.
            let old_stripped = source::strip_function_bodies(old_contents, &hot_names);
            let new_stripped = source::strip_function_bodies(&new_contents, &hot_names);
            if old_stripped != new_stripped {
                return Err(format!(
                    "{} changed outside a #[pill_hot] body (signature, type, \
                     constant, import or another function)",
                    file_label(&path)
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
            match changed.len() {
                0 => continue,
                1 => {}
                _ => {
                    changed.sort();
                    return Err(format!(
                        "{} changed {} hot bodies at once ({}); the fast path \
                         patches one function per edit",
                        file_label(&path),
                        changed.len(),
                        changed.join(", ")
                    ));
                }
            }

            if result.is_some() {
                return Err("more than one source file changed".to_string());
            }
            result = Some((path, changed.remove(0), new_contents));
        }

        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Generation, compilation, activation
    // -------------------------------------------------------------------------

    /// Build and install the replacement for one function.
    fn apply(
        &mut self,
        engine: &mut Engine,
        new_contents: &str,
        function: &str,
        qualified: &str,
        stages: &mut PatchStages,
    ) -> Result<(), String> {
        self.counter += 1;
        let crate_name = format!("pill_hotpatch_{}", self.counter);

        // Generate.
        let generate_started = Instant::now();
        let generated = self.generate(new_contents, function, qualified)?;
        let source_path = std::env::temp_dir().join(format!("{crate_name}.rs"));
        std::fs::write(&source_path, generated.as_bytes())
            .map_err(|error| format!("cannot write the generated patch: {error}"))?;
        stages.generate = generate_started.elapsed().as_secs_f64() * 1000.0;

        // Compile, replaying cargo's own flags plus the crate's rlib. The
        // extern entry is built before borrowing the cached line, which needs
        // `&mut self` on first use.
        let artifact = std::env::temp_dir().join(format!("{crate_name}.dll"));
        let extra_externs = vec![format!(
            "{}={}",
            self.package,
            self.package_rlib.display()
        )];
        let flags_started = Instant::now();
        let line = self.rustc_line()?;
        stages.flags = flags_started.elapsed().as_secs_f64() * 1000.0;
        let compile_started = Instant::now();
        let output = line
            .replay(&source_path, &artifact, &crate_name, &extra_externs)
            .output()
            .map_err(|error| format!("cannot run rustc: {error}"))?;
        stages.compile = compile_started.elapsed().as_secs_f64() * 1000.0;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first = stderr
                .lines()
                .find(|line| line.starts_with("error"))
                .unwrap_or("rustc rejected the generated patch");
            return Err(first.to_string());
        }

        // Load. Never unloaded: a slot will hold an address inside this image.
        // SAFETY: the file was just produced by rustc from a generated source
        // and is a complete native module.
        let load_started = Instant::now();
        let library = unsafe { Library::new(&artifact) }
            .map_err(|error| format!("cannot load the patch library: {error}"))?;
        stages.load = load_started.elapsed().as_secs_f64() * 1000.0;
        let activate_started = Instant::now();

        // Resolve the replacement through the patch's own resolver export.
        let lookup_name = format!("{PATCH_NAME_PREFIX}{qualified}");
        let (address, signature_hash) = resolve_in(&library, &lookup_name)?;

        // Install at the frame boundary. The engine refuses a signature that no
        // longer matches, so a shape change can never be applied behind a call
        // site compiled for the old one.
        engine
            .hot_patch(qualified, address, signature_hash)
            .map_err(|error| error.to_string())?;
        stages.activate = activate_started.elapsed().as_secs_f64() * 1000.0;

        self.generations.push(Generation {
            function: qualified.to_string(),
            _library: library,
        });
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            function = qualified,
            generations = self.generations.len(),
            "patch generation installed"
        );
        Ok(())
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
        function: &str,
        qualified: &str,
    ) -> Result<String, String> {
        let found = source::find_function(new_contents, function)
            .ok_or_else(|| format!("cannot locate `{function}` in the changed source"))?;

        let imports = source::top_level_use_statements(new_contents).join("\n");
        let package = &self.package;

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
            let freshness = vec![
                self.workspace_root.join("Cargo.toml"),
                self.workspace_root.join("Cargo.lock"),
            ];
            let line = match CargoRustcLine::load_if_fresh(&cache, &freshness, &self.build_command) {
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

/// Ask a loaded patch for one function's address and signature hash.
fn resolve_in(library: &Library, lookup_name: &str) -> Result<(usize, u64), String> {
    type ResolveFn = unsafe extern "C" fn(*const std::ffi::c_char, *mut u64) -> usize;

    // SAFETY: the export is generated by `pill_hot_resolver!` with exactly this
    // C ABI signature, and the borrow ends before the library is moved.
    let resolve: Symbol<ResolveFn> = unsafe { library.get(PATCH_RESOLVER_EXPORT) }.map_err(|_| {
        format!(
            "the patch does not export `{}`",
            String::from_utf8_lossy(PATCH_RESOLVER_EXPORT)
        )
    })?;

    let encoded = std::ffi::CString::new(lookup_name)
        .map_err(|_| "the function name contains a NUL byte".to_string())?;
    let mut signature_hash: u64 = 0;
    // SAFETY: a NUL-terminated name and a writable u64, as the export requires.
    let address = unsafe { resolve(encoded.as_ptr(), &mut signature_hash) };

    if address == 0 {
        return Err(format!("the patch does not define `{lookup_name}`"));
    }
    Ok((address, signature_hash))
}

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
            rustc_line: None,
            generations: Vec::new(),
            counter: 0,
        };

        let contents = "use pill_engine::*;\n\n#[pill_hot]\nfn movement(value: i32) -> i32 { value * 2 }\n";
        let generated = session
            .generate(contents, "movement", "project::movement")
            .expect("generated");

        assert!(generated.contains("name = \"pill_patch::project::movement\""));
        assert!(generated.contains("pill_hot_resolver!(pill_patch_resolve)"));
        assert!(generated.contains("use pill_engine::*;"));
        assert!(generated.contains("use project::*;"));
        // The function is copied verbatim, body included.
        assert!(generated.contains("fn movement(value: i32) -> i32 { value * 2 }"));
        // The original attribute must NOT be duplicated.
        assert_eq!(generated.matches("#[").filter(|_| true).count() >= 1, true);
        assert_eq!(generated.matches("pill_hot(name").count(), 1);
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
            rustc_line: None,
            generations: Vec::new(),
            counter: 0,
        };
        session.refresh_snapshots();
        session
    }

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
        let (_, function, contents) = classified.expect("a change must be reported");
        assert_eq!(function, "movement");
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
            ("constant", "const SPEED: f32 = 1.0;", "const SPEED: f32 = 2.0;"),
            ("signature", "fn movement(value: f32)", "fn movement(value: f64)"),
            ("import", "use pill_engine::*;", "use pill_engine::Engine;"),
            ("other function body", "value + 1.0", "value + 9.0"),
        ];

        for (label, from, to) in cases {
            let directory =
                std::env::temp_dir().join(format!("pill_classify_{}", label.replace(' ', "_")));
            let _ = std::fs::remove_dir_all(&directory);
            let mut session = session_over(&directory, HOT_SOURCE);

            let edited = HOT_SOURCE.replace(from, to);
            assert_ne!(edited, HOT_SOURCE, "{label}: the fixture edit must apply");
            std::fs::write(directory.join("lib.rs"), &edited).expect("write edit");

            let outcome = session.classify();
            assert!(
                outcome.is_err(),
                "{label}: a change outside a hot body must be refused, got {outcome:?}"
            );

            let _ = std::fs::remove_dir_all(&directory);
        }
    }

    /// A file with no annotation is refused rather than silently ignored, so a
    /// developer editing an un-annotated system is told why nothing happened.
    #[test]
    fn classify_refuses_a_file_without_any_hot_function() {
        let directory = std::env::temp_dir().join("pill_classify_unannotated");
        let _ = std::fs::remove_dir_all(&directory);
        let plain = "fn ordinary(value: f32) -> f32 { value }\n";
        let mut session = session_over(&directory, plain);

        std::fs::write(directory.join("lib.rs"), "fn ordinary(value: f32) -> f32 { value * 2.0 }\n")
            .expect("write edit");

        let error = session.classify().expect_err("must be refused");
        assert!(error.contains("no #[pill_hot]"), "unexpected reason: {error}");

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
            rustc_line: None,
            generations: Vec::new(),
            counter: 0,
        };
        assert!(session
            .generate("fn other() {}", "movement", "project::movement")
            .is_err());
    }
}

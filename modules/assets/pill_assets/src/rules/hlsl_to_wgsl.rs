//! HLSL/Slang source in, WGSL out, through the `slangc` CLI.
//!
//! # Responsibilities
//!
//! - Compile every HLSL stage source the root holds, under `shaders/` or flat.
//! - Infer the stage and entry point from the file name, refusing any name that
//!   does not declare one.
//! - Resolve the compiler to an explicit path, from `PILL_SLANGC` or `PATH`,
//!   and refuse one that resolves inside the working directory.
//! - Kill a compiler that outlives the cook's bound, so one hung `slangc`
//!   cannot block every crate built behind it.
//! - Surface `slangc`'s own diagnostics when a compile fails.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::{walk_files, CookError, Rule};

/// Variable that names the compiler a cook runs, instead of searching `PATH`.
const SLANGC_ENV: &str = "PILL_SLANGC";

/// The compiler's file name on this platform.
const SLANGC_EXECUTABLE: &str = if cfg!(windows) {
    "slangc.exe"
} else {
    "slangc"
};

/// How long one `slangc` run may take before the cook kills it.
///
/// A shader tree is cooked from the build script of every crate that names it,
/// so a compiler that never exits would stall the whole build, not one file.
const SLANGC_TIMEOUT: Duration = Duration::from_secs(120);

/// How often a running `slangc` is checked for having finished.
const SLANGC_POLL: Duration = Duration::from_millis(25);

/// Compiles `shaders/*.hlsl` to WGSL beside the source through `slangc`.
///
/// The stage and entry point come from the file name: `*_vertex.hlsl` is a
/// vertex stage entered at `vs_main`, `*_fragment.hlsl` a fragment stage entered
/// at `fs_main`. Any other name fails instead of guessing, so adding a stage is
/// a decision made here rather than something inferred from a typo.
///
/// Sources `#include` an `include/` beside them; [`HlslToWgsl::with_include`]
/// adds directories a header is shared from. [`HlslToWgsl::new`] reads a
/// `shaders/` level below the root and [`HlslToWgsl::flat`] the root itself.
/// The compiler comes from `PILL_SLANGC` or the first `slangc` on `PATH`, and
/// one that resolves inside the working directory is refused.
pub struct HlslToWgsl {
    /// Glob, relative to the root, selecting the sources.
    glob: &'static str,
    /// Directories handed to `slangc -I`, searched after the including file's
    /// own directory, in the order given.
    include_dirs: Vec<PathBuf>,
}

impl HlslToWgsl {
    /// The rule for a root with a `shaders/` directory: no include path, so
    /// sources resolve against their own tree.
    pub fn new() -> Self {
        Self {
            glob: "shaders/*.hlsl",
            include_dirs: Vec::new(),
        }
    }

    /// The rule for a root that is itself the shaders directory.
    ///
    /// For a tree whose sources have no `shaders/` level to sit in - a shared
    /// one holding a handful of stages, say, where the extra directory earns
    /// nothing.
    pub fn flat() -> Self {
        Self {
            glob: "*.hlsl",
            include_dirs: Vec::new(),
        }
    }

    /// Also search `directory` for the headers the sources `#include`.
    ///
    /// Repeatable; directories are searched in the order given.
    #[must_use]
    pub fn with_include(mut self, directory: impl Into<PathBuf>) -> Self {
        self.include_dirs.push(directory.into());
        self
    }
}

impl Default for HlslToWgsl {
    fn default() -> Self {
        Self::new()
    }
}

impl Rule for HlslToWgsl {
    fn name(&self) -> &'static str {
        "hlsl_to_wgsl"
    }

    fn input_glob(&self) -> &'static str {
        // A `shaders/` level, or the root itself for a flat tree. Headers are
        // never matched: they live in `include/`, beside it.
        self.glob
    }

    fn output_for(&self, input: &Path) -> PathBuf {
        input.with_extension("wgsl")
    }

    fn extra_inputs(&self, input: &Path) -> Result<Vec<PathBuf>, CookError> {
        // Everything the sources `#include` is an input, whether it sits in the
        // tree's own `include/` or in one shared with other trees. None of it
        // matches the rule's glob, so without this a header edit would leave
        // every output looking fresh.
        let mut headers = Vec::new();
        if let Some(parent) = input.parent() {
            headers.extend(walk_files(&parent.join("include"))?);
        }
        for directory in &self.include_dirs {
            headers.extend(walk_files(directory)?);
        }
        Ok(headers)
    }

    fn build(&self, input: &Path, output: &Path) -> Result<(), CookError> {
        let (entry, stage) = stage_for(input)?;
        let compiler = slangc_path().map_err(|detail| CookError::Rule {
            rule: self.name(),
            input: input.to_owned(),
            detail,
        })?;
        let mut slangc = Command::new(&compiler);
        slangc
            .arg(input)
            .args(["-target", "wgsl", "-entry", entry, "-stage", stage]);
        for directory in &self.include_dirs {
            slangc.arg("-I").arg(directory);
        }
        // `-O0 -g` keep the intermediate variable names, so the emitted WGSL
        // still reads like the HLSL it came from.
        slangc.args(["-O0", "-g", "-o"]).arg(output);
        let cooked =
            run_bounded(slangc, &compiler, SLANGC_TIMEOUT).map_err(|detail| CookError::Rule {
                rule: self.name(),
                input: input.to_owned(),
                detail,
            })?;

        if !cooked.status.success() {
            return Err(CookError::Rule {
                rule: self.name(),
                input: input.to_owned(),
                detail: format!(
                    "slangc exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    cooked.status.code(),
                    String::from_utf8_lossy(&cooked.stdout).trim_end(),
                    String::from_utf8_lossy(&cooked.stderr).trim_end(),
                ),
            });
        }
        Ok(())
    }
}

/// Run `command` to completion, killing it once `timeout` has passed.
///
/// Output is captured the way [`Command::output`] captures it, so a failed
/// compile still reports the compiler's own diagnostics. A run that outlives
/// the bound is killed and reported; a compiler that ignores the kill is still
/// waited for, so no child outlives the cook.
fn run_bounded(mut command: Command, compiler: &Path, timeout: Duration) -> Result<Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{}: {error}", compiler.display()))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => return Err(format!("{}: {error}", compiler.display())),
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{} did not finish within {timeout:?} and was killed",
                compiler.display()
            ));
        }
        std::thread::sleep(SLANGC_POLL);
    }
    child
        .wait_with_output()
        .map_err(|error| format!("{}: {error}", compiler.display()))
}

/// The `slangc` a cook may run.
///
/// A bare name is never handed to the OS. `CreateProcess` on Windows searches
/// the working directory before `PATH`, and on any platform a `PATH` entry can
/// name it, so a project carrying a `slangc.exe` beside its crate could have it
/// executed by a cook that was only asked to compile a shader. `PILL_SLANGC`
/// names the compiler outright; without it the search walks `PATH` in order and
/// refuses any candidate that resolves inside the working directory.
fn slangc_path() -> Result<PathBuf, String> {
    let working_directory = std::env::current_dir()
        .map_err(|error| format!("cannot read the working directory: {error}"))?;
    resolve_slangc(
        std::env::var_os(SLANGC_ENV),
        std::env::var_os("PATH"),
        &working_directory,
    )
}

/// [`slangc_path`] over values a caller supplies, so a test can pin the `PATH`
/// it resolves against instead of the process's.
fn resolve_slangc(
    pinned: Option<OsString>,
    path_variable: Option<OsString>,
    working_directory: &Path,
) -> Result<PathBuf, String> {
    let working_directory = working_directory.canonicalize().map_err(|error| {
        format!(
            "cannot resolve the working directory {}: {error}",
            working_directory.display()
        )
    })?;
    if let Some(pinned) = pinned {
        let pinned = PathBuf::from(pinned);
        checked(&pinned, &working_directory)
            .map_err(|reason| format!("{SLANGC_ENV} names {} but {reason}", pinned.display()))?;
        return Ok(pinned);
    }
    for directory in path_variable
        .iter()
        .flat_map(|paths| std::env::split_paths(paths))
    {
        // An empty entry means the working directory, which is the one place a
        // cook must not take a compiler from.
        if directory.as_os_str().is_empty() || !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(SLANGC_EXECUTABLE);
        if !candidate.is_file() {
            continue;
        }
        checked(&candidate, &working_directory)
            .map_err(|reason| format!("{} is on PATH but {reason}", candidate.display()))?;
        return Ok(candidate);
    }
    Err(format!(
        "no {SLANGC_EXECUTABLE} found: set {SLANGC_ENV} to its path, or install Slang \
         from https://github.com/shader-slang/slang/releases"
    ))
}

/// `Ok` when a cook may run `candidate`, `Err` with the reason it may not.
fn checked(candidate: &Path, working_directory: &Path) -> Result<(), String> {
    if !candidate.is_absolute() {
        return Err(
            "it is not an absolute path, so it would resolve against the working directory"
                .to_owned(),
        );
    }
    if !candidate.is_file() {
        return Err("it is not a file".to_owned());
    }
    let resolved = candidate
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if resolved.starts_with(working_directory) {
        return Err(
            "it is inside the working directory, where a project could have put it".to_owned(),
        );
    }
    Ok(())
}

/// The entry point and stage `file` declares through its name.
///
/// # Errors
///
/// Returns [`CookError::Rule`] when the name carries neither `_vertex` nor
/// `_fragment`, because compiling with a guessed stage would produce a shader
/// that fails at pipeline creation instead of here.
fn stage_for(file: &Path) -> Result<(&'static str, &'static str), CookError> {
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.ends_with("_vertex.hlsl") {
        return Ok(("vs_main", "vertex"));
    }
    if name.ends_with("_fragment.hlsl") {
        return Ok(("fs_main", "fragment"));
    }
    Err(CookError::Rule {
        rule: "hlsl_to_wgsl",
        input: file.to_owned(),
        detail: format!(
            "cannot infer a stage from {name}: name it *_vertex.hlsl or *_fragment.hlsl"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_sits_beside_the_source() {
        let rule = HlslToWgsl::new();

        assert_eq!(
            rule.output_for(Path::new("shaders/default_vertex.hlsl")),
            PathBuf::from("shaders/default_vertex.wgsl")
        );
        assert_eq!(rule.input_glob(), "shaders/*.hlsl");
    }

    #[test]
    fn a_flat_tree_takes_its_sources_from_the_root() {
        assert_eq!(HlslToWgsl::new().input_glob(), "shaders/*.hlsl");
        assert_eq!(HlslToWgsl::flat().input_glob(), "*.hlsl");
    }

    #[test]
    fn an_include_path_is_searched_after_the_trees_own() {
        let shared = Path::new("../common_shaders");
        let configured = HlslToWgsl::new().with_include(shared);

        assert_eq!(configured.include_dirs, vec![PathBuf::from(shared)]);
        assert!(HlslToWgsl::new().include_dirs.is_empty());
    }

    #[test]
    fn the_stage_comes_from_the_name_suffix() {
        assert_eq!(
            stage_for(Path::new("shaders/unlit_fragment.hlsl")).expect("fragment"),
            ("fs_main", "fragment")
        );
        assert_eq!(
            stage_for(Path::new("shaders/default_vertex.hlsl")).expect("vertex"),
            ("vs_main", "vertex")
        );
    }

    #[test]
    fn a_name_without_a_stage_is_refused() {
        let refused = stage_for(Path::new("shaders/helpers.hlsl"));

        assert!(matches!(refused, Err(CookError::Rule { .. })));
    }

    /// A compiler pinned to a path that is not there fails the cook by name, so
    /// the developer who pinned it is told what the cook looked at instead of
    /// being handed the diagnostics of whichever `slangc` PATH happened to
    /// hold.
    #[test]
    fn a_pinned_compiler_that_is_not_there_is_reported() {
        let missing = std::env::temp_dir().join("pill_assets_no_such_slangc");
        std::env::set_var(SLANGC_ENV, &missing);

        let built = HlslToWgsl::new().build(
            Path::new("shaders/default_vertex.hlsl"),
            Path::new("shaders/default_vertex.wgsl"),
        );

        std::env::remove_var(SLANGC_ENV);
        let Err(CookError::Rule { detail, .. }) = built else {
            panic!("a missing pinned compiler must fail the cook: {built:?}");
        };
        assert!(detail.contains(SLANGC_ENV), "{detail}");
    }

    /// A scratch directory for the resolver, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir()
            .join("pill_assets_slangc")
            .join(format!("{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    /// A directory holding a `slangc` a cook could run.
    fn with_compiler(name: &str) -> PathBuf {
        let directory = scratch(name);
        std::fs::write(directory.join(SLANGC_EXECUTABLE), b"a compiler").expect("compiler");
        directory
    }

    fn path_of(directory: &Path) -> OsString {
        std::env::join_paths([directory]).expect("one entry")
    }

    /// The hazard the resolution exists for: a project that carries its own
    /// `slangc.exe` beside its crate, on `PATH` because it is the working
    /// directory.
    #[test]
    fn a_compiler_inside_the_working_directory_is_refused() {
        let project = with_compiler("planted");

        let refused = resolve_slangc(None, Some(path_of(&project)), &project);

        let detail = refused.expect_err("a project-local compiler must be refused");
        assert!(detail.contains("working directory"), "{detail}");
    }

    #[test]
    fn the_first_path_entry_holding_a_compiler_wins() {
        let tools = with_compiler("tools");
        let elsewhere = scratch("elsewhere");
        let empty = scratch("empty");
        let path_variable =
            std::env::join_paths([&empty, &tools, &elsewhere]).expect("three entries");

        assert_eq!(
            resolve_slangc(None, Some(path_variable), &elsewhere),
            Ok(tools.join(SLANGC_EXECUTABLE))
        );
    }

    #[test]
    fn a_pinned_compiler_outside_the_working_directory_is_taken() {
        let tools = with_compiler("pinned");
        let project = scratch("project");

        assert_eq!(
            resolve_slangc(
                Some(tools.join(SLANGC_EXECUTABLE).into_os_string()),
                None,
                &project
            ),
            Ok(tools.join(SLANGC_EXECUTABLE))
        );
    }

    /// A pin is a path, never a name: a name would resolve against the working
    /// directory again, which is the search this replaced.
    #[test]
    fn a_pinned_name_is_refused() {
        let project = with_compiler("named");

        let refused = resolve_slangc(Some(OsString::from(SLANGC_EXECUTABLE)), None, &project);

        let detail = refused.expect_err("a bare name is not a path");
        assert!(detail.contains("absolute"), "{detail}");
    }

    #[test]
    fn nothing_found_names_the_variable_to_set() {
        let project = scratch("nothing");

        let refused = resolve_slangc(None, Some(OsString::new()), &project);

        let detail = refused.expect_err("an empty PATH has no compiler");
        assert!(detail.contains(SLANGC_ENV), "{detail}");
    }

    /// A compiler that never exits is killed at the bound: one wedged process
    /// must not hold every crate built after it.
    #[test]
    fn a_compiler_that_never_finishes_is_killed() {
        let detail = run_bounded(
            slow_command(),
            Path::new(SLANGC_EXECUTABLE),
            Duration::from_millis(100),
        )
        .expect_err("the command outlives the bound");

        assert!(detail.contains("did not finish within"), "{detail}");
        assert!(detail.contains(SLANGC_EXECUTABLE), "{detail}");
    }

    /// A command that stays alive far longer than any test's bound, without
    /// writing enough to fill a pipe.
    fn slow_command() -> Command {
        if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", "ping", "-n", "30", "127.0.0.1"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 30"]);
            command
        }
    }
}

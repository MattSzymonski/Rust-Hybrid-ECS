//! Capturing and replaying the exact `rustc` invocation cargo uses.
//!
//! # Responsibilities
//!
//! - Ask cargo once, in verbose mode, for the command it builds a crate with.
//! - Replay that command with a different input file and output path.
//! - Cache the result, invalidated by manifest and source modification times.
//!
//! # Design
//!
//! Hand-reconstructing `--extern` and `-L native` was the hot-patch prototype's
//! most brittle part: it hardcoded one rlib metadata hash and three registry
//! paths, so it broke on any dependency change and could not run on another
//! machine. Cargo already knows the right answer, so this asks it rather than
//! guessing, and a patch therefore links exactly the artifacts the running
//! module linked - which is what keeps type layout AND `TypeId` identical
//! across the boundary.
//!
//! ```ignore
//! let line = CargoRustcLine::capture(&workspace, "project", &lib_rs)?;
//! let output = line
//!     .replay(&patch_source, &patch_dll, "pill_hotpatch_7", &[project_rlib])
//!     .output()?;
//! ```


use std::path::{Path, PathBuf};
use std::process::Command;

// =============================================================================
// Flag classification
// =============================================================================

/// Flags whose value is a SEPARATE following token (`-C` `opt-level=0`), so the
/// parser must consume two tokens to keep pairs together.
const TWO_TOKEN_FLAGS: &[&str] = &[
    "-C",
    "-L",
    "-l",
    "-Z",
    "--extern",
    "--crate-name",
    "--crate-type",
    "--out-dir",
    "--target",
    "--check-cfg",
    "--cfg",
    "--edition",
    "--error-format",
    "--emit",
    "--sysroot",
];

/// `-C` values dropped on replay.
///
/// - `metadata` / `extra-filename` belong to the original crate's identity and
///   would collide with it.
/// - `incremental` measured SLOWER for a single-file patch (551 ms vs 465 ms):
///   the whole crate is one codegen unit and changes on every patch, so the
///   cache never hits and only costs bookkeeping.
const DROPPED_CODEGEN_VALUES: &[&str] = &["metadata=", "extra-filename=", "incremental="];

/// Whole flags dropped on replay, with their value token when they have one.
///
/// The diagnostics flags are dropped so `rustc` prints human-readable errors
/// straight into the host console; `--emit` and `--out-dir` are replaced by a
/// plain `-o`; the crate identity flags are replaced by the patch's own.
const DROPPED_FLAGS: &[&str] = &[
    "--error-format",
    "--json",
    "--emit",
    "--out-dir",
    "--crate-name",
    "--crate-type",
];

// =============================================================================
// CargoRustcLine
// =============================================================================

/// One captured `rustc` command line, ready to be replayed for a patch build.
#[derive(Debug, Clone, PartialEq)]
pub struct CargoRustcLine {
    /// Absolute path to the `rustc` cargo actually used, so a toolchain switch
    /// is followed rather than guessed.
    pub program: String,
    /// Every argument except the input source path, which is substituted.
    pub args: Vec<String>,
}

impl CargoRustcLine {
    // -------------------------------------------------------------------------
    // Capture
    // -------------------------------------------------------------------------

    /// Ask cargo for the `rustc` line it uses to build `package`'s library.
    ///
    /// Cargo only prints the invocation when it actually compiles, so an
    /// up-to-date crate produces nothing. In that case `crate_root` has its
    /// modification time bumped (contents untouched, so version control is
    /// unaffected) and the build is retried once.
    ///
    /// # Errors
    ///
    /// Returns a message when cargo cannot be run, when the build fails, or
    /// when no invocation for `package` appears even after forcing a rebuild.
    /// `build_command` MUST be the host's own command for this crate, feature
    /// flags included. Capturing a bare `cargo build -p <package>` instead
    /// yields a line that links a differently-configured `pill_engine`, and a
    /// different feature set means different crate metadata - so every
    /// `TypeId` in the patch would differ from the running world's and the
    /// patched code would find no components and no resources.
    pub fn capture(
        workspace_dir: &Path,
        package: &str,
        crate_root: &Path,
        build_command: &[String],
    ) -> Result<Self, String> {
        if let Some(line) = Self::try_capture(workspace_dir, package, build_command)? {
            return Ok(line);
        }

        // The crate was fresh, so cargo stayed silent. Touch the root source to
        // force exactly one rebuild of this crate and ask again.
        touch(crate_root)?;
        Self::try_capture(workspace_dir, package, build_command)?.ok_or_else(|| {
            format!(
                "cargo -v printed no rustc invocation for `{package}` even after \
                 forcing a rebuild of {}",
                crate_root.display()
            )
        })
    }

    /// Run one verbose build and extract the invocation, if cargo emitted one.
    fn try_capture(
        workspace_dir: &Path,
        package: &str,
        build_command: &[String],
    ) -> Result<Option<Self>, String> {
        let (program, arguments) = build_command
            .split_first()
            .ok_or_else(|| "the build command is empty".to_string())?;

        let output = Command::new(program)
            .args(arguments)
            .arg("-v")
            .current_dir(workspace_dir)
            .output()
            .map_err(|error| format!("cannot run cargo in {}: {error}", workspace_dir.display()))?;

        // Cargo writes the `Running` lines to stderr.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            return Err(format!("cargo build -p {package} failed:\n{stderr}"));
        }

        // Several crates may be rebuilt; take the one that names this package as
        // its crate. `--crate-name <package>` identifies it unambiguously
        // because cargo builds one library target per package here.
        for raw in stderr.lines() {
            let Some(invocation) = extract_backticked(raw) else {
                continue;
            };
            if !invocation.contains("rustc") {
                continue;
            }
            let tokens = tokenize(&invocation);
            if tokens.len() < 2 {
                continue;
            }
            let names_this_package = tokens
                .windows(2)
                .any(|pair| pair[0] == "--crate-name" && pair[1] == package);
            if !names_this_package {
                continue;
            }
            return Ok(Some(Self {
                program: tokens[0].clone(),
                args: tokens[1..].to_vec(),
            }));
        }
        Ok(None)
    }

    // -------------------------------------------------------------------------
    // Replay
    // -------------------------------------------------------------------------

    /// Build the `rustc` command that compiles `input` into a patch cdylib at
    /// `output`, reusing every dependency flag cargo resolved.
    ///
    /// The captured line's crate identity, diagnostics format, emit list and
    /// output directory are replaced; everything that describes the dependency
    /// graph (`--extern`, `-L`, `--cfg`, `-C prefer-dynamic`, the linker) is
    /// carried across untouched. That is what keeps the patch's view of every
    /// type - layout AND `TypeId` - identical to the running host's.
    /// `extra_externs` are additional `name=path` entries appended as
    /// `--extern`. A patch needs at least the patched crate's own rlib, so its
    /// generated source can `use project::*` and get the identical types the
    /// running module holds.
    pub fn replay(
        &self,
        input: &Path,
        output: &Path,
        crate_name: &str,
        extra_externs: &[String],
        staged_dependencies: Option<&Path>,
    ) -> Command {
        let mut command = Command::new(&self.program);
        command.args(self.replay_args(
            input,
            output,
            crate_name,
            extra_externs,
            staged_dependencies,
        ));
        command
    }

    /// The argument vector `replay` passes to `rustc`, exposed for tests and
    /// for logging a reproducible command line.
    ///
    /// `staged_dependencies`, when given, redirects every `--extern` that names
    /// a shared per-crate slot to the copy staged there. Those slots are
    /// overwritten in place by any build of the same crate name, feature set
    /// included, so linking them directly is what made a patch fail with
    /// `error[E0463]` after an unrelated `cargo build`. Hash-qualified paths are
    /// left alone: they already name one exact configuration.
    pub fn replay_args(
        &self,
        input: &Path,
        output: &Path,
        crate_name: &str,
        extra_externs: &[String],
        staged_dependencies: Option<&Path>,
    ) -> Vec<String> {
        let mut arguments = Vec::with_capacity(self.args.len() + 8);
        let mut index = 0;

        // Ahead of everything cargo recorded, so rustc finds the staged copies
        // first. This is not the same job as redirecting `--extern` below, and
        // both are needed: `--extern` only maps crates the patch source names
        // itself, while the module rlib's own dependencies are resolved by
        // searching the `-L` paths for a crate with the right name and metadata
        // hash. Leave the shared `deps` directory first and rustc finds whatever
        // variant was last written there, rejects it, and reports
        // `error[E0463]: can't find crate for <the module>` - blaming the module
        // rather than the dependency that actually moved.
        if let Some(directory) = staged_dependencies {
            arguments.push("-L".to_string());
            arguments.push(format!("dependency={}", directory.display()));
        }

        while index < self.args.len() {
            let token = &self.args[index];
            let has_separate_value =
                TWO_TOKEN_FLAGS.contains(&token.as_str()) && index + 1 < self.args.len();

            // Drop the flags whose replacements are appended below. Both the
            // `--flag value` and `--flag=value` spellings appear in cargo output.
            let bare = token.split('=').next().unwrap_or(token);
            if DROPPED_FLAGS.contains(&bare) {
                index += if has_separate_value { 2 } else { 1 };
                continue;
            }

            // `-C metadata=...`, `-C extra-filename=...`, `-C incremental=...`
            if token == "-C" && has_separate_value {
                let value = &self.args[index + 1];
                if DROPPED_CODEGEN_VALUES
                    .iter()
                    .any(|prefix| value.starts_with(prefix))
                {
                    index += 2;
                    continue;
                }
            }

            // The original crate's source file is a positional argument; the
            // patch supplies its own.
            if !token.starts_with('-') && token.ends_with(".rs") {
                index += 1;
                continue;
            }

            arguments.push(token.clone());
            if has_separate_value {
                let value = &self.args[index + 1];
                arguments.push(match (token.as_str(), staged_dependencies) {
                    ("--extern", Some(directory)) => redirect_extern(value, directory),
                    _ => value.clone(),
                });
            }
            index += if has_separate_value { 2 } else { 1 };
        }

        // Anything the patch needs beyond what the original crate linked -
        // principally that crate's own rlib.
        for extern_entry in extra_externs {
            arguments.push("--extern".to_string());
            arguments.push(extern_entry.clone());
        }

        // No PDB for the patch.
        //
        // Measured, not guessed: this is 22% of the compile. Linking is 75% of a
        // patch's wall time - a body that references nothing links in 90 ms
        // against 360 ms for a real one - because the linker reads the whole
        // engine closure, 21 MB of `pill_engine` and 12 MB of `pill_core`, to
        // emit one function. Most of that bulk is debug info, and building a PDB
        // from it costs 80 ms and writes 18 MB per patch. `-C debuginfo=0` does
        // NOT avoid it: the debug info lives in the inputs, not in the patch, so
        // only telling the linker to skip the PDB entirely helps.
        //
        // The trade is real and deliberate: patched code has no debugger
        // symbols. A patch is generated code that exists for a few seconds, and
        // a prologue-patched function already defeats breakpoints set on it -
        // so the symbols were of little use, while the 80 ms is paid on every
        // save.
        //
        // MSVC-only because `/DEBUG:NONE` is a link.exe/lld-link flag. Other
        // targets keep whatever cargo recorded.
        #[cfg(target_env = "msvc")]
        {
            arguments.push("-C".to_string());
            arguments.push("link-arg=/DEBUG:NONE".to_string());
        }

        // The patch's own identity and output.
        arguments.push("--crate-name".to_string());
        arguments.push(crate_name.to_string());
        arguments.push("--crate-type".to_string());
        arguments.push("cdylib".to_string());
        arguments.push("-o".to_string());
        arguments.push(output.display().to_string());
        arguments.push(input.display().to_string());
        arguments
    }

    // -------------------------------------------------------------------------
    // Cache
    // -------------------------------------------------------------------------

    /// Persist the captured line so a later host process skips the cargo probe.
    ///
    /// One token per line, program first. Tokens never contain newlines (they
    /// are paths and flags), so no escaping is needed.
    pub fn save(&self, path: &Path, build_command: &[String]) -> std::io::Result<()> {
        let mut text = String::with_capacity(4096);
        // The command this was captured for, so a feature change invalidates it.
        text.push_str(&build_command.join("\u{1}"));
        text.push('\n');
        text.push_str(&self.program);
        text.push('\n');
        for argument in &self.args {
            text.push_str(argument);
            text.push('\n');
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)
    }

    /// Reload a previously saved line.
    ///
    /// Returns `None` when the cache is absent, when any input that could change
    /// the flags is newer than it, or when `build_command` differs from the one
    /// the cache was captured with.
    ///
    /// The build command is part of the key because feature flags live there and
    /// nowhere else: changing them changes crate metadata and therefore every
    /// `TypeId`, while leaving `Cargo.toml`, `Cargo.lock` and the sources
    /// untouched. Keying on mtimes alone would happily replay flags for a
    /// differently-configured engine.
    pub fn load_if_fresh(path: &Path, inputs: &[PathBuf], build_command: &[String]) -> Option<Self> {
        let cached_at = std::fs::metadata(path).ok()?.modified().ok()?;
        for input in inputs {
            let modified = std::fs::metadata(input).ok()?.modified().ok()?;
            if modified > cached_at {
                return None;
            }
        }
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines();

        // First line is the command this was captured for.
        let recorded_command = lines.next()?;
        if recorded_command != build_command.join("\u{1}") {
            return None;
        }
        let program = lines.next()?.to_string();
        Some(Self {
            program,
            args: lines.map(|line| line.to_string()).collect(),
        })
    }
}

// =============================================================================
// Free functions
// =============================================================================

/// Point one `--extern name=path` at its staged copy, when there is one.
///
/// Only a shared per-crate slot is redirected - a `.rlib` whose filename carries
/// no `-<16 hex digits>` metadata suffix. Everything else is returned unchanged,
/// including entries this cannot parse: linking the original path is what
/// happened before staging existed, so an unrecognized entry degrades to the old
/// behaviour rather than to a broken command line.
fn redirect_extern(entry: &str, staged_dependencies: &Path) -> String {
    let Some((name, path)) = entry.split_once('=') else {
        return entry.to_string();
    };
    let Some(file_name) = Path::new(path).file_name().and_then(|name| name.to_str()) else {
        return entry.to_string();
    };
    if !is_shared_slot_rlib(file_name) {
        return entry.to_string();
    }
    let staged = staged_dependencies.join(file_name);
    if !staged.is_file() {
        return entry.to_string();
    }
    format!("{name}={}", staged.display())
}

/// Whether a `deps` filename is a shared per-crate slot rather than one
/// qualified by a metadata hash.
///
/// Kept beside [`redirect_extern`] and mirrored by the staging side in
/// `build_runner`: the two must agree about which files are shared, or a file
/// is staged and never linked, or linked and never staged.
fn is_shared_slot_rlib(file_name: &str) -> bool {
    let Some(stem) = file_name.strip_suffix(".rlib") else {
        return false;
    };
    match stem.rsplit_once('-') {
        Some((_, suffix)) => {
            !(suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()))
        }
        None => true,
    }
}

/// Bump a file's modification time without touching its contents.
///
/// Rewriting the same bytes is enough for cargo's mtime-based freshness check
/// and leaves version control untouched, since the content hash is unchanged.
fn touch(path: &Path) -> Result<(), String> {
    let contents = std::fs::read(path)
        .map_err(|error| format!("cannot read {} to touch it: {error}", path.display()))?;
    std::fs::write(path, &contents)
        .map_err(|error| format!("cannot touch {}: {error}", path.display()))
}

/// Pull the command out of cargo's ``Running `...` `` line.
fn extract_backticked(line: &str) -> Option<String> {
    let start = line.find('`')? + 1;
    let end = line.rfind('`')?;
    if end <= start {
        return None;
    }
    Some(line[start..end].to_string())
}

/// Split a command line into tokens, honouring cargo's single-quote grouping.
///
/// Cargo quotes any argument containing a space or a backslash, which on
/// Windows means most paths. Quotes are grouping only - there is no escape
/// sequence inside them, and paths cannot contain a single quote.
fn tokenize(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut inside_quotes = false;
    let mut has_content = false;

    for character in command.chars() {
        match character {
            '\'' => {
                inside_quotes = !inside_quotes;
                // An empty '' is still a real (empty) argument.
                has_content = true;
            }
            character if character.is_whitespace() && !inside_quotes => {
                if has_content {
                    tokens.push(std::mem::take(&mut current));
                    has_content = false;
                }
            }
            character => {
                current.push(character);
                has_content = true;
            }
        }
    }
    if has_content {
        tokens.push(current);
    }
    tokens
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed but structurally faithful copy of a real cargo -v line.
    fn sample() -> &'static str {
        "     Running `C:\\rustc.exe --crate-name project --edition=2021 \
         'D:\\proj\\src\\lib.rs' --error-format=json \
         --json=diagnostic-rendered-ansi --crate-type cdylib --emit=dep-info,link \
         -C embed-bitcode=no -C debuginfo=2 --check-cfg 'cfg(docsrs,test)' \
         -C metadata=b74d3ef2a041325c --out-dir 'D:\\proj\\target\\debug\\deps' \
         -C linker=rust-lld -C 'incremental=D:\\proj\\target\\debug\\incremental' \
         -L 'dependency=D:\\proj\\target\\debug\\deps' \
         --extern 'pill_engine=D:\\proj\\target\\debug\\deps\\libpill_engine-319.rlib' \
         --extern 'serde=D:\\proj\\target\\debug\\deps\\libserde-1094916a36d6bb5e.rlib' \
         --extern 'pill_dummy_color=D:\\proj\\target\\debug\\deps\\libpill_dummy_color.rlib' \
         -C prefer-dynamic -L 'native=C:\\reg\\windows_x86_64_msvc-0.52.6\\lib'`"
    }

    fn parsed() -> CargoRustcLine {
        let invocation = extract_backticked(sample()).expect("backticked command");
        let tokens = tokenize(&invocation);
        CargoRustcLine {
            program: tokens[0].clone(),
            args: tokens[1..].to_vec(),
        }
    }

    #[test]
    fn tokenizer_groups_single_quoted_paths() {
        let tokens = tokenize("a 'b c' 'd\\e' f");
        assert_eq!(tokens, vec!["a", "b c", "d\\e", "f"]);
    }

    #[test]
    fn capture_finds_program_and_arguments() {
        let line = parsed();
        assert_eq!(line.program, "C:\\rustc.exe");
        assert!(line.args.contains(&"--crate-name".to_string()));
        assert!(line.args.contains(&"-C".to_string()));
    }

    /// A shared per-crate slot is one cargo overwrites in place; a
    /// hash-qualified name belongs to one exact configuration.
    ///
    /// The distinction decides which `--extern` entries get redirected to a
    /// staged copy, and the staging side in `build_runner` must agree with it -
    /// disagree and a file is either staged and never linked, or linked and
    /// never staged.
    #[test]
    fn shared_slots_are_told_apart_from_hash_qualified_artifacts() {
        // Workspace crates: one slot per crate name, overwritten by any build.
        assert!(is_shared_slot_rlib("libpill_dummy_color.rlib"));
        assert!(is_shared_slot_rlib("libpill_core.rlib"));
        // A crate name containing a hyphen is still a shared slot; only a
        // trailing 16-hex-digit suffix marks a configuration.
        assert!(is_shared_slot_rlib("libtrait_type-map.rlib"));
        // Hash-qualified, so it already names one configuration.
        assert!(!is_shared_slot_rlib("libpill_engine-190d6c0e2d2eaf24.rlib"));
        // A suffix of the wrong length or with non-hex digits is not a hash.
        assert!(is_shared_slot_rlib("libthing-190d6c0e2d2eaf2.rlib"));
        assert!(is_shared_slot_rlib("libthing-190d6c0e2d2eaf2z.rlib"));
        // Not an rlib at all.
        assert!(!is_shared_slot_rlib("pill_core.dll"));
    }

    /// Only a shared slot with a staged copy is redirected. Everything else is
    /// returned unchanged, so an unrecognized entry degrades to the behaviour
    /// that existed before staging rather than to a broken command line.
    #[test]
    fn only_staged_shared_slots_are_redirected() {
        let staged = std::env::temp_dir().join("pill_staged_externs_test");
        let _ = std::fs::remove_dir_all(&staged);
        std::fs::create_dir_all(&staged).expect("create staging dir");
        std::fs::write(staged.join("libpill_dummy_color.rlib"), b"staged")
            .expect("write staged copy");

        // Staged shared slot: redirected.
        let redirected = redirect_extern(
            "pill_dummy_color=D:\\proj\\target\\debug\\deps\\libpill_dummy_color.rlib",
            &staged,
        );
        assert!(
            redirected.ends_with("libpill_dummy_color.rlib"),
            "got: {redirected}"
        );
        assert!(redirected.starts_with("pill_dummy_color="));
        assert!(
            redirected.contains("pill_staged_externs_test"),
            "must point into the staging directory: {redirected}"
        );

        // Shared slot with nothing staged: left alone.
        let untouched = redirect_extern(
            "pill_spline=D:\\proj\\target\\debug\\deps\\libpill_spline.rlib",
            &staged,
        );
        assert!(untouched.ends_with("deps\\libpill_spline.rlib"), "got: {untouched}");

        // Hash-qualified: never redirected, even if a same-named file exists.
        let hashed = "pill_engine=D:\\proj\\deps\\libpill_engine-190d6c0e2d2eaf24.rlib";
        assert_eq!(redirect_extern(hashed, &staged), hashed);

        // Unparseable entries are returned verbatim.
        assert_eq!(redirect_extern("no_equals_sign", &staged), "no_equals_sign");

        let _ = std::fs::remove_dir_all(&staged);
    }

    /// The redirect reaches `replay_args`, which is where it has to happen.
    #[test]
    fn replay_redirects_a_staged_dependency() {
        let staged = std::env::temp_dir().join("pill_staged_replay_test");
        let _ = std::fs::remove_dir_all(&staged);
        std::fs::create_dir_all(&staged).expect("create staging dir");
        std::fs::write(staged.join("libpill_dummy_color.rlib"), b"staged")
            .expect("write staged copy");

        let line = parsed();
        let joined = line
            .replay_args(
                Path::new("patch.rs"),
                Path::new("patch.dll"),
                "pill_hotpatch_1",
                &[],
                Some(staged.as_path()),
            )
            .join(" ");

        assert!(
            joined.contains(&format!(
                "pill_dummy_color={}",
                staged.join("libpill_dummy_color.rlib").display()
            )),
            "the staged copy must be linked: {joined}"
        );
        // A genuinely hash-qualified dependency is untouched.
        assert!(joined.contains(
            "serde=D:\\proj\\target\\debug\\deps\\libserde-1094916a36d6bb5e.rlib"
        ));

        let _ = std::fs::remove_dir_all(&staged);
    }

    #[test]
    fn replay_keeps_every_dependency_flag() {
        let line = parsed();
        let arguments = line.replay_args(
            Path::new("patch.rs"),
            Path::new("patch.dll"),
            "pill_hotpatch_1",
            &[],
            None,
        );
        let joined = arguments.join(" ");

        // The dependency graph must survive verbatim - this is what keeps
        // TypeId identical between host and patch.
        assert!(joined.contains("dependency=D:\\proj\\target\\debug\\deps"));
        assert!(joined.contains("pill_engine=D:\\proj\\target\\debug\\deps\\libpill_engine-319.rlib"));
        assert!(joined.contains("native=C:\\reg\\windows_x86_64_msvc-0.52.6\\lib"));
        assert!(joined.contains("prefer-dynamic"));
        assert!(joined.contains("linker=rust-lld"));
        assert!(joined.contains("--edition=2021"));
        assert!(joined.contains("cfg(docsrs,test)"));
    }

    #[test]
    fn replay_drops_the_original_crate_identity_and_output() {
        let line = parsed();
        let arguments = line.replay_args(
            Path::new("patch.rs"),
            Path::new("patch.dll"),
            "pill_hotpatch_1",
            &[],
            None,
        );
        let joined = arguments.join(" ");

        assert!(!joined.contains("metadata=b74d3ef2a041325c"));
        assert!(!joined.contains("--out-dir"));
        assert!(!joined.contains("--emit"));
        assert!(!joined.contains("error-format"));
        assert!(!joined.contains("--json"));
        // Incremental measured slower for a single-file patch, so it is dropped.
        assert!(!joined.contains("incremental="));
        // The original crate's source must not survive as a positional argument.
        assert!(!joined.contains("lib.rs"));
    }

    #[test]
    fn replay_installs_the_patch_identity_and_output() {
        let line = parsed();
        let arguments = line.replay_args(
            Path::new("patch.rs"),
            Path::new("out.dll"),
            "pill_hotpatch_7",
            &[],
            None,
        );

        let name_index = arguments.iter().position(|a| a == "--crate-name").unwrap();
        assert_eq!(arguments[name_index + 1], "pill_hotpatch_7");

        let type_index = arguments.iter().position(|a| a == "--crate-type").unwrap();
        assert_eq!(arguments[type_index + 1], "cdylib");

        let out_index = arguments.iter().position(|a| a == "-o").unwrap();
        assert_eq!(arguments[out_index + 1], "out.dll");

        assert_eq!(arguments.last().unwrap(), "patch.rs");
        // Exactly one crate identity survives.
        assert_eq!(arguments.iter().filter(|a| *a == "--crate-name").count(), 1);
        assert_eq!(arguments.iter().filter(|a| *a == "--crate-type").count(), 1);
    }

    #[test]
    fn save_and_load_round_trip() {
        let command = vec!["cargo".to_string(), "build".to_string()];
        let line = parsed();
        let directory = std::env::temp_dir().join("pill_cargo_line_test");
        let path = directory.join("line.txt");
        line.save(&path, &command).expect("save");
        let loaded = CargoRustcLine::load_if_fresh(&path, &[], &command).expect("load");
        assert_eq!(line, loaded);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn load_rejects_a_stale_cache() {
        let command = vec!["cargo".to_string(), "build".to_string()];
        let directory = std::env::temp_dir().join("pill_cargo_line_stale");
        std::fs::create_dir_all(&directory).expect("create dir");
        let cache = directory.join("line.txt");
        let manifest = directory.join("Cargo.toml");

        parsed().save(&cache, &command).expect("save");
        // Written after the cache, so the cache must be rejected.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&manifest, "[package]").expect("write manifest");

        assert!(CargoRustcLine::load_if_fresh(&cache, &[manifest], &command).is_none());
        let _ = std::fs::remove_dir_all(&directory);
    }
}

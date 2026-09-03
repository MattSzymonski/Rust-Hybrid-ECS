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
/// Identifies how a cached compiler line was tokenized.
///
/// Bump this whenever [`tokenize`] changes, so caches written by the previous
/// parser are re-captured instead of replayed. Nothing else in the cache key can
/// detect that: a parser fix leaves the manifests, the lockfile and the build
/// command exactly as they were.
const CACHE_FORMAT_VERSION: &str = "pill-hotpatch-flags-v3";

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

/// Where a package's captured flags are cached between processes.
///
/// The system temporary directory rather than the build tree, because the
/// cache describes the CURRENT host's universe and must not be mistaken for a
/// build artifact; its freshness is decided by
/// [`CargoRustcLine::load_if_fresh`], not by its location.
///
/// Shared by the two writers so they cannot disagree: the fast-patch pipeline,
/// which captures on demand, and [`crate::build_runner::run_build_command`],
/// which harvests the same line for free out of a build it was going to run
/// anyway.
pub(crate) fn flags_cache_path(package: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pill_hotpatch_{package}.flags"))
}

/// Pull the invocation for `crate_name` out of one line of cargo's `-v` output.
///
/// Returns `None` for every line that is not a `Running \`rustc ...\`` naming
/// that crate, so a caller can feed it cargo's whole stderr stream a line at a
/// time. Any rustc wrapper cargo ran (sccache) is unwrapped, exactly as the
/// on-demand capture does - the patch replays the compiler directly and the
/// wrapper's own CLI would reject rustc's arguments.
pub(crate) fn parse_rustc_line(line: &str, crate_name: &str) -> Option<CargoRustcLine> {
    let invocation = extract_backticked(line)?;
    if !invocation.contains("rustc") {
        return None;
    }
    let tokens = tokenize(&invocation);
    if tokens.len() < 2 {
        return None;
    }
    let names_this_crate = tokens
        .windows(2)
        .any(|pair| pair[0] == "--crate-name" && pair[1] == crate_name);
    if !names_this_crate {
        return None;
    }
    let compiler_index = compiler_token_index(&tokens);
    Some(CargoRustcLine {
        program: tokens[compiler_index].clone(),
        args: tokens[compiler_index + 1..].to_vec(),
    })
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

        let mut command = Command::new(program);
        command.args(arguments).current_dir(workspace_dir);
        // The full reload path and this flag-capture path both invoke the
        // module's configured cargo command, and both must agree with the host
        // binary that is running right now. Re-apply everything the reload path
        // applies: the spawned-build environment (profile-driven `RUSTFLAGS`
        // handling, and under the dioxus CLI the mirror of dx's
        // `RUSTC_WORKSPACE_WRAPPER`), plus the shared overrides (the private
        // target directory, the host anchor package and the custom profile
        // definition). Missing the profile definition makes cargo reject a
        // launcher-injected profile such as `desktop-dev`; an environment that
        // differs from the module build's instead compiles the crate with
        // different codegen flags or a different wrapper hash than the module
        // was built with, which changes every metadata hash and makes the
        // captured `--extern` closure disagree with the staged rlib - rustc
        // then reports `error[E0463]` and every edit falls back to a full
        // reload.
        if program == "cargo" {
            command.envs(
                crate::config::spawned_build_environment()
                    .into_iter()
                    .map(|(key, value)| (key, value)),
            );
            crate::build_runner::apply_cargo_host_overrides(&mut command, workspace_dir);
        }
        let output = command
            .arg("-v")
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
        Ok(stderr
            .lines()
            .find_map(|raw| parse_rustc_line(raw, package)))
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
                    // The patch is linked by the toolchain's own LLD even when
                    // cargo recorded a different linker; see
                    // [`patch_linker_value`].
                    ("-C", _) => patch_linker_value(value),
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
        // How the tokens in this file were produced. A cache written by an older
        // tokenizer holds tokens split the wrong way, and nothing else in the key
        // would notice: the manifests, the lockfile and the build command are all
        // unchanged by fixing the parser. Bumping this is what makes a stale
        // cache be re-captured rather than replayed.
        text.push_str(CACHE_FORMAT_VERSION);
        text.push('\n');
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
    pub fn load_if_fresh(
        path: &Path,
        inputs: &[PathBuf],
        build_command: &[String],
    ) -> Option<Self> {
        let cached_at = std::fs::metadata(path).ok()?.modified().ok()?;
        for input in inputs {
            let modified = std::fs::metadata(input).ok()?.modified().ok()?;
            if modified > cached_at {
                return None;
            }
        }
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines();

        // Written by a different tokenizer, so its tokens cannot be trusted.
        if lines.next()? != CACHE_FORMAT_VERSION {
            return None;
        }
        // Then the command this was captured for.
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

/// Linker the patch is built with, given the one cargo recorded.
///
/// Returns the value unchanged for every `-C` other than `linker=`, and for a
/// `linker=` that already names an LLD.
///
/// **Why the patch overrides the linker at all.** The workspace asks for
/// `rust-lld` (`modules/.cargo/config.toml`), but the editor launcher has to
/// force `link.exe` for the whole dx session, because dx drives the link
/// through its own linker proxy and that proxy cannot run LLD. The patch is not
/// linked by dx: the host runs `rustc` itself, so nothing forces `link.exe`
/// here beyond the flag having been captured from a build that did run under
/// the launcher.
///
/// **What it is worth.** Measured on the same patch and the same inputs, LLD
/// links this closure in ~236 ms against link.exe's ~950 ms - the single
/// largest difference between the editor's patch time and the standalone
/// host's, and about 55 % of the editor's whole per-patch cost.
///
/// **Why it is safe.** The linker choice affects only the ephemeral patch
/// image. Both linkers consume the same object files and import libraries and
/// both accept the `/DEBUG:NONE` the replay appends; the patch never links
/// anything the module did not, and a linker that failed would refuse the patch
/// and fall back to a module reload rather than produce a wrong one.
fn patch_linker_value(value: &str) -> String {
    let Some(linker) = value.strip_prefix("linker=") else {
        return value.to_string();
    };
    let file_name = Path::new(linker)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(linker)
        .to_ascii_lowercase();
    if file_name.contains("lld") {
        return value.to_string();
    }
    format!("linker={PATCH_LINKER}")
}

/// The linker [`patch_linker_value`] substitutes.
///
/// `rust-lld` is resolved by rustc out of the active toolchain's sysroot
/// (`lib/rustlib/<target>/bin`), so it needs nothing installed and follows a
/// toolchain switch. It is the same linker `modules/.cargo/config.toml` already
/// asks every non-dx build in this repository to use.
const PATCH_LINKER: &str = "rust-lld";

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
    // Which quote character opened the group being read, if any. Cargo uses
    // single quotes for most arguments but switches to double quotes with
    // backslash escapes when the value itself contains a double quote - which
    // `--check-cfg 'cfg(feature, values("rendering"))'` does. Treating only
    // single quotes as grouping split that one argument in half and handed
    // rustc `values(\"rendering\"))"` as a second input filename, so every
    // patch of a crate with features failed with "multiple input filenames
    // provided" and fell back to a full reload.
    let mut opened_by: Option<char> = None;
    let mut escaped = false;
    let mut has_content = false;

    for character in command.chars() {
        // A backslash inside double quotes escapes the next character; inside
        // single quotes cargo does not escape, so a backslash is literal - which
        // matters because every Windows path in this command line is full of
        // them.
        if escaped {
            current.push(character);
            has_content = true;
            escaped = false;
            continue;
        }
        if character == '\\' && opened_by == Some('"') {
            escaped = true;
            continue;
        }

        match character {
            '\'' | '"' => match opened_by {
                // Closing the group this character opened.
                Some(opener) if opener == character => opened_by = None,
                // Inside the other kind of quote, so it is literal content.
                Some(_) => {
                    current.push(character);
                    has_content = true;
                }
                // An empty '' is still a real (empty) argument.
                None => {
                    opened_by = Some(character);
                    has_content = true;
                }
            },
            character if character.is_whitespace() && opened_by.is_none() => {
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

/// Index of the actual `rustc` binary inside a captured invocation.
///
/// Returns 0 when none of the tokens names a rustc binary, which keeps a plain
/// invocation - whose first token is the compiler - unchanged.
///
/// Cargo can run rustc through a wrapper (a `RUSTC_WRAPPER` such as sccache),
/// which makes a captured line read `sccache C:\...\rustc.exe --crate-name
/// ...`. The patch replays the captured program directly with rustc arguments,
/// and the wrapper's own CLI rejects those, so the wrapper token is dropped and
/// the first token that names a rustc binary becomes the program.
fn compiler_token_index(tokens: &[String]) -> usize {
    tokens
        .iter()
        .position(|token| {
            Path::new(token)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.to_ascii_lowercase().contains("rustc"))
        })
        .unwrap_or(0)
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

    /// Cargo double-quotes an argument whose value contains a double quote, and
    /// escapes the inner ones - which `--check-cfg` does for any crate that has
    /// features.
    ///
    /// This is the bug that made the project crate unpatchable. Treating only
    /// single quotes as grouping split the argument in half, and rustc received
    /// the tail as a second input filename: "multiple input filenames provided".
    /// Every project edit fell back to a full reload, with the real cause buried
    /// in a message that never mentioned quoting.
    #[test]
    fn tokenizer_handles_double_quoted_arguments_with_escapes() {
        let tokens =
            tokenize(r#"--check-cfg "cfg(feature, values(\"rendering\"))" --edition=2021"#);
        assert_eq!(
            tokens,
            vec![
                "--check-cfg".to_string(),
                r#"cfg(feature, values("rendering"))"#.to_string(),
                "--edition=2021".to_string(),
            ],
            "the check-cfg value must stay one argument"
        );
    }

    /// A backslash is literal inside single quotes, which every Windows path in
    /// this command line depends on.
    #[test]
    fn tokenizer_keeps_backslashes_in_single_quoted_paths() {
        let tokens = tokenize(r"'D:\\proj\\target\\debug\\deps' next");
        assert_eq!(
            tokens,
            vec![
                r"D:\\proj\\target\\debug\\deps".to_string(),
                "next".to_string()
            ]
        );
    }

    /// One kind of quote inside the other is literal content, not a delimiter.
    #[test]
    fn tokenizer_treats_the_other_quote_as_content() {
        assert_eq!(
            tokenize(r#"'it\"s one token' after"#),
            vec![r#"it\"s one token"#.to_string(), "after".to_string()]
        );
    }

    /// A rustc wrapper (sccache under `RUSTC_WRAPPER`) appears ahead of the
    /// real compiler in cargo's `Running` line. The patch replays the captured
    /// program with rustc arguments, so capture must drop the wrapper and keep
    /// the first token that names a rustc binary - which is the first token on
    /// a plain invocation, leaving that case unchanged.
    #[test]
    fn a_rustc_wrapper_is_unwrapped_to_the_real_compiler() {
        let plain = tokenize(r"C:\rustc.exe --crate-name project --edition=2021");
        assert_eq!(
            compiler_token_index(&plain),
            0,
            "a plain invocation already names rustc first"
        );

        let wrapped = tokenize(
            r"sccache C:\Users\me\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe --crate-name project --edition=2021",
        );
        assert_eq!(
            compiler_token_index(&wrapped),
            1,
            "the wrapper token is dropped in favour of the real rustc"
        );

        // No token names rustc: fall back to the first token, which preserves
        // the pre-wrapper behaviour rather than guessing.
        let unrelated = tokenize(r"--crate-name project --edition=2021");
        assert_eq!(compiler_token_index(&unrelated), 0);
    }

    /// Quote one argument the way cargo does, in either style.
    ///
    /// Cargo quotes an argument that contains whitespace or a backslash. It
    /// usually reaches for single quotes, but switches to double quotes with
    /// backslash escapes for some values - both spellings were observed in this
    /// workspace for `--check-cfg`, from the same cargo. The tokenizer has to
    /// read both, so the round-trip below drives both.
    fn quote_like_cargo(argument: &str, double: bool) -> String {
        if double {
            format!(
                "\"{}\"",
                argument.replace('\\', "\\\\").replace('"', "\\\"")
            )
        } else {
            format!("'{argument}'")
        }
    }

    /// Every argument shape this command line actually carries must survive a
    /// quote-then-tokenize round trip, in both quoting styles.
    ///
    /// This is the general form of the bug that made every crate with features
    /// unpatchable: one argument was split in two, rustc took the tail as a
    /// second input filename, and the patch failed with a message that never
    /// mentioned quoting. A single hand-written case would not have caught it,
    /// because the broken shape only appears for `--check-cfg` on a crate that
    /// has features - so the shapes are enumerated instead.
    #[test]
    fn every_argument_shape_survives_a_quoting_round_trip() {
        let arguments = [
            // The shape that broke: nested double quotes inside the value.
            r#"cfg(feature, values("rendering"))"#,
            r#"cfg(feature, values("default", "module-abi", "rendering"))"#,
            // No features: no nested quotes, which is why some crates worked.
            "cfg(docsrs,test)",
            // Windows paths - backslashes everywhere, and spaces in some.
            r"D:\\proj\\target\\debug\\deps",
            r"dependency=D:\\proj\\target\\debug\\deps",
            r"C:\\Program Files\\rustc\\lib",
            r"incremental=D:\\proj\\target\\debug\\incremental",
            // Ordinary flags and values.
            "--edition=2021",
            "embed-bitcode=no",
            "pill_engine=D:\\proj\\libpill_engine-190d6c0e2d2eaf24.rlib",
            // A lone apostrophe inside a double-quoted value.
            r#"it's one argument"#,
        ];

        for argument in arguments {
            for double in [false, true] {
                // A single-quoted group cannot carry a single quote, and cargo
                // would not produce one - skip that impossible pairing.
                if !double && argument.contains('\'') {
                    continue;
                }
                let quoted = quote_like_cargo(argument, double);
                let tokens = tokenize(&quoted);
                assert_eq!(
                    tokens,
                    vec![argument.to_string()],
                    "round trip failed for {argument:?} quoted with {}",
                    if double {
                        "double quotes"
                    } else {
                        "single quotes"
                    }
                );
            }
        }
    }

    /// A whole command line tokenizes into flags and values, never into
    /// fragments.
    ///
    /// The failure this guards is specific and silent: a split argument leaves a
    /// token that is a piece of a value, and rustc treats a token it cannot
    /// recognize as an input filename. Asserting the count is what makes a split
    /// visible - a fragment is always one token too many.
    #[test]
    fn a_realistic_command_line_yields_no_fragments() {
        let line = concat!(
            r#"C:\rustc.exe --crate-name project --edition=2021 "#,
            r#"'D:\proj\src\lib.rs' --check-cfg 'cfg(docsrs,test)' "#,
            r#"--check-cfg "cfg(feature, values(\"rendering\"))" "#,
            r#"-C 'incremental=D:\proj\target\debug\incremental' "#,
            r#"-L 'dependency=D:\proj\target\debug\deps'"#,
        );
        let tokens = tokenize(line);

        assert_eq!(
            tokens.len(),
            13,
            "a fragment would show up as an extra token: {tokens:#?}"
        );
        // The value that used to be split in half.
        assert!(
            tokens.contains(&r#"cfg(feature, values("rendering"))"#.to_string()),
            "the check-cfg value must be one token: {tokens:#?}"
        );
        // Nothing that rustc would mistake for an input file. Only the real
        // source path may end in `.rs`, and only quoted paths may contain a
        // separator, so anything else ending in `.rs` is a fragment.
        let source_paths: Vec<&String> = tokens
            .iter()
            .filter(|token| token.ends_with(".rs"))
            .collect();
        assert_eq!(
            source_paths.len(),
            1,
            "exactly one token may look like a source file: {source_paths:#?}"
        );
        // A leftover quote in any token means a group was not closed.
        for token in &tokens {
            assert!(
                !token.contains('\'') && !token.starts_with('"'),
                "token carries an unbalanced quote, so grouping went wrong: {token:?}"
            );
        }
    }

    /// The cache is rejected when it was written by a different tokenizer.
    ///
    /// Without this the parser fix would not have reached anyone: the cache key
    /// is the manifests, the lockfile and the build command, and fixing a parser
    /// changes none of them, so every existing cache would replay the tokens the
    /// old parser produced.
    #[test]
    fn a_cache_from_a_different_tokenizer_is_rejected() {
        let directory = std::env::temp_dir().join("pill_flags_cache_version");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create cache dir");
        let cache = directory.join("flags");
        let inputs: Vec<std::path::PathBuf> = Vec::new();
        let command = vec!["cargo".to_string(), "build".to_string()];

        // Written by the current tokenizer: accepted.
        parsed().save(&cache, &command).expect("save");
        assert!(CargoRustcLine::load_if_fresh(&cache, &inputs, &command).is_some());

        // The same content with an older version marker: refused.
        let text = std::fs::read_to_string(&cache).expect("read");
        let older = text.replacen(CACHE_FORMAT_VERSION, "pill-hotpatch-flags-v1", 1);
        std::fs::write(&cache, older).expect("write");
        assert!(
            CargoRustcLine::load_if_fresh(&cache, &inputs, &command).is_none(),
            "a cache from another tokenizer must be re-captured, not replayed"
        );

        let _ = std::fs::remove_dir_all(&directory);
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
        assert!(
            untouched.ends_with("deps\\libpill_spline.rlib"),
            "got: {untouched}"
        );

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
        assert!(
            joined.contains("serde=D:\\proj\\target\\debug\\deps\\libserde-1094916a36d6bb5e.rlib")
        );

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
        assert!(
            joined.contains("pill_engine=D:\\proj\\target\\debug\\deps\\libpill_engine-319.rlib")
        );
        assert!(joined.contains("native=C:\\reg\\windows_x86_64_msvc-0.52.6\\lib"));
        assert!(joined.contains("prefer-dynamic"));
        assert!(joined.contains("linker=rust-lld"));
        assert!(joined.contains("--edition=2021"));
        assert!(joined.contains("cfg(docsrs,test)"));
    }

    /// The patch links through the toolchain's LLD whatever cargo recorded.
    ///
    /// The editor launcher forces `link.exe` on the whole dx session because
    /// dx's linker proxy cannot drive LLD, and that flag is captured with the
    /// rest of the command. Replaying it costs ~290 ms of extra link time per
    /// patch (measured on the same inputs) for a linker choice that only ever
    /// affects the ephemeral patch image.
    #[test]
    fn the_patch_links_with_lld_even_when_cargo_recorded_link_exe() {
        assert_eq!(patch_linker_value("linker=link.exe"), "linker=rust-lld");
        assert_eq!(
            patch_linker_value(r"linker=C:\Program Files\...\link.exe"),
            "linker=rust-lld"
        );
        // Already an LLD: left exactly as cargo recorded it, so a workspace
        // that names a specific LLD build keeps it.
        assert_eq!(patch_linker_value("linker=rust-lld"), "linker=rust-lld");
        assert_eq!(
            patch_linker_value("linker=lld-link.exe"),
            "linker=lld-link.exe"
        );
        // Every other `-C` value passes through untouched.
        assert_eq!(patch_linker_value("prefer-dynamic"), "prefer-dynamic");
        assert_eq!(patch_linker_value("debuginfo=2"), "debuginfo=2");
    }

    /// A captured `link.exe` is rewritten inside a full replay, not only in
    /// isolation - and nothing else about the linker line moves.
    #[test]
    fn replay_rewrites_a_captured_link_exe() {
        let mut line = parsed();
        for argument in &mut line.args {
            if argument == "linker=rust-lld" {
                *argument = "linker=link.exe".to_string();
            }
        }
        let arguments = line.replay_args(
            Path::new("patch.rs"),
            Path::new("patch.dll"),
            "pill_hotpatch_1",
            &[],
            None,
        );
        let joined = arguments.join(" ");
        assert!(joined.contains("linker=rust-lld"), "{joined}");
        assert!(!joined.contains("link.exe"), "{joined}");
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

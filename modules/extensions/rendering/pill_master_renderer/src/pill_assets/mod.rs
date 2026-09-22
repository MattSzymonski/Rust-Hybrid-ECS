//! Native asset cooking adapted from Pill-Engine (af052d1).
//!
//! # Responsibilities
//!
//! - Orders conversion rules by their declared dependencies.
//! - Hashes source snapshots, cooker code, and shader compiler version for caching.
//! - Publishes immutable generation directories through an atomic manifest update.
//!
//! # Design
//!
//! [`Pipeline`] runs rules inside one working directory and uses modification
//! times to skip current primary outputs. [`cook`] adds the publication boundary:
//! it snapshots sources into staging, runs the pipeline there, and publishes only
//! after all rules succeed. Readers keep using the previous manifest on failure.
//!
//! The generation hash includes all source files, conservatively covering sidecars
//! and shader includes. It also includes cooker source text, so documentation-only
//! changes to those files deliberately invalidate the existing cook cache.

// Standard library
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// External crates
use anyhow::{Context, Result};

// =============================================================================
// Rule Modules and Exports
// =============================================================================

pub mod formats;
pub mod rules;

pub use rules::{
    default_rules, EquirectToIBL, GlbToCookedMesh, HlslToWgsl, ObjToCookedMesh, PngToCookedTex,
    ProceduralEquirect,
};

// =============================================================================
// Conversion Contract
// =============================================================================

/// One named native conversion rule with a primary output and optional side outputs.
///
/// Names must be unique within a pipeline. Dependencies refer to these names,
/// not source paths, and are scheduled before this rule discovers its inputs.
pub trait Rule {
    /// Glob (relative to the pipeline root) selecting source inputs.
    fn input_glob(&self) -> &'static str;

    /// Map an input path to the output path it produces.
    fn output_for(&self, input: &Path) -> PathBuf;

    /// Run the tool that turns `input` → `output`. Called only when stale.
    fn build(&self, input: &Path, output: &Path) -> Result<()>;

    /// Human-readable rule name for logs/errors.
    fn name(&self) -> &'static str;

    /// Rule dependencies, executed before this rule. Unknown names and cycles fail.
    fn dependencies(&self) -> &[&str] {
        &[]
    }
}

// =============================================================================
// Pipeline Execution
// =============================================================================

/// Rule collection executed against a single working directory.
pub struct Pipeline {
    /// Root used for input glob expansion and primary-output containment checks.
    pub root: PathBuf,
    /// Available rules; declared dependencies determine execution order.
    pub rules: Vec<Box<dyn Rule>>,
}

/// Inputs and primary outputs observed during cooking.
///
/// Rule runs report paths in their working directory. A whole-generation cache
/// hit instead reports source-relative inputs and the cached generation directory.
#[derive(Default, Debug)]
pub struct Stats {
    /// Every input matched by any rule. Build scripts emit cargo:rerun-if-changed for these.
    pub discovered: Vec<PathBuf>,
    /// Outputs we actually wrote this run.
    pub rebuilt: Vec<PathBuf>,
    /// Outputs that were already up-to-date (skipped).
    pub skipped: Vec<PathBuf>,
}

impl Pipeline {
    /// Resolve dependencies, discover inputs, and build stale primary outputs.
    ///
    /// # Errors
    ///
    /// Rejects duplicate names, unknown dependencies, dependency cycles, duplicate
    /// primary destinations, and primary paths outside the root. Glob, filesystem,
    /// and converter failures include rule/input context.
    pub fn run(&self) -> Result<Stats> {
        let mut stats = Stats::default();

        // Step 1: topologically order the rules before any converter writes output.
        let mut ordered = Vec::new();
        let mut visiting = std::collections::HashSet::new();
        let mut done = std::collections::HashSet::new();
        /// Depth-first dependency traversal; the active set detects back edges.
        fn visit<'a>(
            rule: &'a dyn Rule,
            rules: &'a [Box<dyn Rule>],
            visiting: &mut std::collections::HashSet<String>,
            done: &mut std::collections::HashSet<String>,
            ordered: &mut Vec<&'a dyn Rule>,
        ) -> Result<()> {
            if done.contains(rule.name()) {
                return Ok(());
            }
            anyhow::ensure!(
                visiting.insert(rule.name().into()),
                "asset rule dependency cycle at {}",
                rule.name()
            );
            for dependency in rule.dependencies() {
                let parent = rules
                    .iter()
                    .find(|r| r.name() == *dependency)
                    .with_context(|| {
                        format!("unknown dependency {dependency} for {}", rule.name())
                    })?;
                visit(parent.as_ref(), rules, visiting, done, ordered)?;
            }
            visiting.remove(rule.name());
            done.insert(rule.name().into());
            ordered.push(rule);
            Ok(())
        }
        let names: std::collections::HashSet<_> = self.rules.iter().map(|r| r.name()).collect();
        anyhow::ensure!(names.len() == self.rules.len(), "duplicate asset rule name");
        for rule in &self.rules {
            visit(
                rule.as_ref(),
                &self.rules,
                &mut visiting,
                &mut done,
                &mut ordered,
            )?;
        }
        // Step 2: discover inputs after dependencies have generated their intermediate files.
        let mut destinations = std::collections::HashSet::new();
        for rule in ordered {
            let pattern = self.root.join(rule.input_glob());
            let pattern_str = pattern
                .to_str()
                .with_context(|| format!("non-UTF8 path in pipeline root: {pattern:?}"))?;

            let matches = glob::glob(pattern_str).with_context(|| {
                format!("invalid glob {pattern_str:?} for rule {}", rule.name())
            })?;

            for entry in matches {
                let input =
                    entry.with_context(|| format!("glob entry error for rule {}", rule.name()))?;
                stats.discovered.push(input.clone());

                let output = rule.output_for(&input);
                anyhow::ensure!(
                    destinations.insert(output.clone()),
                    "multiple asset rules produce {output:?}"
                );
                anyhow::ensure!(
                    output.starts_with(&self.root),
                    "rule output escaped pipeline root"
                );

                if is_up_to_date(&input, &output) {
                    stats.skipped.push(output);
                    continue;
                }

                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("create output dir {parent:?}"))?;
                }

                rule.build(&input, &output).with_context(|| {
                    format!("rule {} failed: {input:?} -> {output:?}", rule.name())
                })?;
                stats.rebuilt.push(output);
            }
        }

        Ok(stats)
    }
}

// =============================================================================
// Primary Output Freshness
// =============================================================================

/// Treat a primary output as current only when both timestamps exist and it is newer.
fn is_up_to_date(input: &Path, output: &Path) -> bool {
    let in_mtime = mtime(input);
    let out_mtime = mtime(output);
    match (in_mtime, out_mtime) {
        (Some(i), Some(o)) => o >= i,
        _ => false,
    }
}

/// Read a modification time, treating missing or inaccessible paths as stale.
fn mtime(p: &Path) -> Option<SystemTime> {
    p.metadata().ok()?.modified().ok()
}

// =============================================================================
// Generation Cache and Publication
// =============================================================================

/// Publish a complete immutable generation, then atomically replace its manifest.
/// Hashing every source dependency conservatively rebuilds the generation when
/// includes or sidecar files change. Runtime readers never observe partial output.
///
/// # Errors
///
/// Returns errors for invalid source/output roots, symbolic-link inputs, source
/// reads, required compiler discovery, rule execution, and output publication.
/// A failed build leaves the public manifest pointing at the previous generation.
pub fn cook(input: &Path, output: &Path) -> Result<Stats> {
    let input = input.canonicalize().context("input directory")?;
    std::fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    anyhow::ensure!(
        !output.starts_with(&input),
        "cooked output must be outside the source directory"
    );
    /// Collect source-relative file paths recursively, rejecting symbolic links.
    fn files(root: &Path, at: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(at)? {
            let entry = entry?;
            anyhow::ensure!(
                !entry.file_type()?.is_symlink(),
                "asset symlinks are not supported: {:?}",
                entry.path()
            );
            if entry.file_type()?.is_dir() {
                files(root, &entry.path(), result)?;
            } else {
                result.push(entry.path().strip_prefix(root)?.to_path_buf());
            }
        }
        Ok(())
    }
    // Step 1: snapshot all source bytes and hash them with the cooker/tool versions.
    let mut sources = Vec::new();
    files(&input, &input, &mut sources)?;
    sources.sort();
    let mut hash = 0xcbf29ce484222325u64;
    /// Extend a deterministic FNV-1a hash with the next byte sequence.
    fn feed(hash: &mut u64, bytes: &[u8]) {
        for b in bytes {
            *hash = (*hash ^ u64::from(*b)).wrapping_mul(0x100000001b3);
        }
    }
    feed(&mut hash, b"pill-assets-format-5-rules-3");
    feed(&mut hash, include_str!("mod.rs").as_bytes());
    feed(&mut hash, include_str!("formats.rs").as_bytes());
    feed(&mut hash, include_str!("../assets.rs").as_bytes());
    for code in [
        include_str!("rules/hlsl_to_wgsl.rs"),
        include_str!("rules/png_to_cooked_tex.rs"),
        include_str!("rules/obj_to_cooked_mesh.rs"),
        include_str!("rules/glb_to_cooked_mesh.rs"),
        include_str!("rules/procedural_equirect.rs"),
        include_str!("rules/equirect_to_ibl.rs"),
    ] {
        feed(&mut hash, code.as_bytes());
    }
    let mut snapshots = Vec::new();
    for file in &sources {
        let bytes = std::fs::read(input.join(file))?;
        feed(&mut hash, file.to_string_lossy().as_bytes());
        feed(&mut hash, &(bytes.len() as u64).to_le_bytes());
        feed(&mut hash, &bytes);
        snapshots.push(bytes);
    }
    if sources
        .iter()
        .any(|p| p.extension().is_some_and(|e| e == "hlsl"))
    {
        let version = std::process::Command::new("slangc")
            .arg("-version")
            .output()
            .context("slangc is required to cook HLSL; install Slang and add slangc to PATH")?;
        anyhow::ensure!(version.status.success(), "slangc -version failed");
        feed(&mut hash, &version.stdout);
        feed(&mut hash, &version.stderr);
    }
    // Step 2: reuse a complete immutable generation when its manifest already exists.
    let generation = format!("{hash:016x}");
    let final_dir = output.join(&generation);
    if final_dir.join("manifest.json").is_file() {
        let manifest = std::fs::read(final_dir.join("manifest.json"))?;
        if std::fs::read(output.join("manifest.json")).ok().as_deref() != Some(manifest.as_slice())
        {
            atomic_manifest(&output, &manifest)?;
        }
        return Ok(Stats {
            discovered: sources,
            skipped: vec![final_dir],
            rebuilt: Vec::new(),
        });
    }
    // Step 3: build in a private sibling directory, leaving the published generation live.
    let staging = output.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    std::fs::create_dir(&staging)?;
    let result = (|| -> Result<Stats> {
        for (file, bytes) in sources.iter().zip(snapshots) {
            let dest = staging.join(file);
            std::fs::create_dir_all(dest.parent().unwrap())?;
            std::fs::write(dest, bytes)?;
        }
        // The rule order is an explicit DAG: PNG/procedural environment precede IBL.
        let pipeline = Pipeline {
            root: staging.clone(),
            rules: default_rules(),
        };
        let stats = pipeline.run()?;
        let mut outputs = Vec::new();
        files(&staging, &staging, &mut outputs)?;
        outputs.sort();
        let assets:Vec<_>=outputs.into_iter().filter(|p|matches!(p.extension().and_then(|e|e.to_str()),Some("cooked_mesh"|"cooked_tex"|"wgsl"|"material"))).map(|p| {
            let name=p.to_string_lossy().replace('\\',"/");let mut id=0xcbf29ce484222325;feed(&mut id,name.as_bytes());
            serde_json::json!({"id":format!("{id:016x}"),"name":name,"path":format!("{generation}/{name}"),"dependencies":sources.iter().map(|p|p.to_string_lossy().replace('\\',"/")).collect::<Vec<_>>()})
        }).collect();
        let manifest = serde_json::to_vec_pretty(
            &serde_json::json!({"version":1,"generation":generation,"assets":assets}),
        )?;
        // Step 4: finish the generation before replacing the manifest visible to readers.
        std::fs::write(staging.join("manifest.json"), &manifest)?;
        std::fs::rename(&staging, &final_dir)?;
        atomic_manifest(&output, &manifest)?;
        Ok(stats)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// Write a sibling temporary file, then rename it over the public manifest.
///
/// The final rename is the publication point: readers see a complete old or new
/// manifest, and filesystem failures propagate to the caller.
fn atomic_manifest(root: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = root.join(format!("manifest.{}.tmp", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, root.join("manifest.json"))?;
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests;

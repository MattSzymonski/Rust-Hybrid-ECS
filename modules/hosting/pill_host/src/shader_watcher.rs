//! Development shader reload, host half: watch the renderer data crate's
//! shaders, re-cook edits on a worker thread, and queue the results.
//!
//! # Responsibilities
//!
//! - Watch the `shaders/` directory of the renderer's data crate for HLSL
//!   edits, and debounce them like source edits.
//! - Cook the whole tree on the worker thread with
//!   [`pill_assets::cook_shader_tree`], the routine the data crate's build
//!   script runs, so a runtime cook and a build agree on files and headers.
//! - Queue each re-cooked file as a [`CookedShader`] for the main thread, which
//!   hands it to the data crate's `pill_render_data_shader_changed` export.
//!
//! # Design
//!
//! The worker thread runs only host code and `slangc`: it never calls into a
//! module image, so a data crate reload can swap the image at any time without
//! a thread running inside the one being retired. The main thread delivers the
//! queue at the frame boundary, after the reloads, through the export of the
//! data generation current at that moment.
//!
//! Cooking only writes stale outputs, and a header is an input of every source,
//! so an edited source re-cooks that source and an edited header re-cooks every
//! source. A cook `slangc` rejects is logged and queues nothing: the assets keep
//! their text, and the previous shader keeps drawing until a save fixes it.
//!
//! What is queued is every output whose text differs from the last text the
//! worker knew for it, not only what this cook rebuilt: a data crate rebuild
//! runs the same cook from its build script, and when that one gets to an
//! edited source first, the worker finds nothing stale - the edit still has to
//! reach the assets the running world holds.
//!
//! Only `.hlsl` events count. The cooked `.wgsl` files are written beside the
//! sources, so the worker's own output would otherwise feed back in as edits.

// Standard library
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

// External crates
use pill_core::error::WatcherError;
use pill_core::platform::Instant;
use pill_core::{info, warn};

// Current crate
use crate::watcher::{is_relevant_event, spawn_settled_watcher};

/// Extension of the sources whose edits start a cook.
const SOURCE_EXTENSION: &str = "hlsl";

/// One re-cooked WGSL file, waiting for the main thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CookedShader {
    /// The file relative to the watched `shaders/`, with `/` separators.
    pub(crate) relative_path: String,
    /// The file's new text.
    pub(crate) wgsl: String,
}

/// A running shader watcher; dropping it lets the worker thread wind down.
pub(crate) struct ShaderWatcher {
    /// Re-cooked files, in the order the worker produced them.
    cooked: Receiver<CookedShader>,
}

impl ShaderWatcher {
    /// Watch `shaders_directory` and cook edits on a worker thread.
    ///
    /// Returns `Ok(None)` when the directory does not exist: a data crate
    /// without shaders has nothing to reload.
    ///
    /// # Errors
    ///
    /// Returns a [`WatcherError`] when the watcher cannot be created or the
    /// directory cannot be registered.
    pub(crate) fn spawn(
        crate_name: &str,
        shaders_directory: PathBuf,
    ) -> Result<Option<Self>, WatcherError> {
        if !shaders_directory.is_dir() {
            return Ok(None);
        }

        // Step 1: Spawn the shared watcher worker with the shader acceptance
        // (`.hlsl` sources only; the cooked `.wgsl` files are written beside
        // them and would otherwise feed back in as edits) and the cook step.
        let (cooked_sender, cooked) = channel::<CookedShader>();
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = crate_name,
            directory = %shaders_directory.display(),
            "watching renderer shader sources for edits"
        );
        // The worker starts from the outputs as they are now, which is what
        // the running data module embedded.
        let mut known_outputs = read_outputs(&shaders_directory);
        spawn_settled_watcher(
            shaders_directory,
            "pill shader watcher",
            "shader watcher error",
            is_relevant_event,
            |path: &Path| {
                path.extension()
                    .is_some_and(|extension| extension == SOURCE_EXTENSION)
            },
            move |root, edited| {
                for shader in cook(root, &edited, &mut known_outputs) {
                    if cooked_sender.send(shader).is_err() {
                        // The host dropped the watcher: stop.
                        return false;
                    }
                }
                true
            },
        )?;

        Ok(Some(Self { cooked }))
    }

    /// Every re-cooked file queued since the last call, oldest first.
    pub(crate) fn drain(&self) -> Vec<CookedShader> {
        self.cooked.try_iter().collect()
    }
}

/// The text of every cooked output in the tree, by path; an output that cannot
/// be read is left out, so the next cook reports it.
fn read_outputs(shaders_directory: &Path) -> HashMap<PathBuf, String> {
    let Ok(layout) = pill_assets::shader_tree_layout(shaders_directory) else {
        return HashMap::new();
    };
    let mut outputs = HashMap::new();
    for source_directory in &layout.source_directories {
        let Ok(entries) = std::fs::read_dir(source_directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "wgsl")
            {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    outputs.insert(path, text);
                }
            }
        }
    }
    outputs
}

/// Cook the tree after `edited` settled, and return every output whose text
/// differs from `known_outputs`, which it then records.
///
/// A failed cook is logged and yields nothing, so the assets keep their text.
fn cook(
    shaders_directory: &Path,
    edited: &[PathBuf],
    known_outputs: &mut HashMap<PathBuf, String>,
) -> Vec<CookedShader> {
    let started = Instant::now();
    let report = match pill_assets::cook_shader_tree(shaders_directory) {
        Ok(report) => report,
        Err(error) => {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "shader cook failed; keeping the previous shaders: {error}"
            );
            return Vec::new();
        }
    };

    let mut changed = Vec::new();
    for source in &report.discovered_sources {
        let output = source.with_extension("wgsl");
        let Some(relative_path) = relative_to(shaders_directory, &output) else {
            continue;
        };
        let wgsl = match std::fs::read_to_string(&output) {
            Ok(wgsl) => wgsl,
            Err(error) => {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    path = %output.display(),
                    "cooked shader could not be read: {error}"
                );
                continue;
            }
        };
        if known_outputs.get(&output) == Some(&wgsl) {
            continue;
        }
        known_outputs.insert(output, wgsl.clone());
        changed.push(CookedShader {
            relative_path,
            wgsl,
        });
    }
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        edited = edited.len(),
        recooked = report.rebuilt_outputs.len(),
        changed = changed.len(),
        cook_ms = started.elapsed().as_millis() as u64,
        "shader sources re-cooked"
    );
    changed
}

/// `path` relative to `root`, with `/` separators, as the data crate's shader
/// table spells it; `None` for a path outside `root`.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_uses_forward_slashes() {
        let root = Path::new("crate").join("shaders");
        let output = root
            .join("pbr_pipeline")
            .join("shaders")
            .join("pbr_fragment.wgsl");

        assert_eq!(
            relative_to(&root, &output).as_deref(),
            Some("pbr_pipeline/shaders/pbr_fragment.wgsl")
        );
        assert_eq!(relative_to(&root, Path::new("elsewhere.wgsl")), None);
    }

    #[test]
    fn a_missing_directory_starts_no_watcher() {
        let missing = std::env::temp_dir().join("pill_host_shader_watcher_missing_directory");

        let watcher = ShaderWatcher::spawn("test", missing).expect("not an error");

        assert!(watcher.is_none());
    }
}

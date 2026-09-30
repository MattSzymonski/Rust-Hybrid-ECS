//! Development shader reload: an edited HLSL file reaches the GPU without a
//! restart.
//!
//! # Responsibilities
//!
//! - Watch the directories the renderer's HLSL sources and headers live in.
//! - After an edit settles, re-cook them with the same routine `build.rs` runs.
//! - Put the re-cooked WGSL into the shader assets built from it, which is all
//!   the GPU side needs to rebuild exactly those shaders.
//!
//! # Design
//!
//! A shader edit is an asset edit. The renderer's pipelines store their shaders
//! as `Shader` assets, and `RenderingResourcesManager` rebuilds any asset whose
//! content version moved - so this module never touches a device, a pipeline or
//! a bind group. Writing through [`AssetManager::get_by_name_mut`] is what moves
//! the version; the next frame's sync does the rest, and the resource epoch it
//! bumps refreshes every bind group that referenced the old shader.
//!
//! Which asset a file feeds comes from [`all_shader_sources`], the same table the
//! pipelines embed their WGSL from, so the two cannot name different files. An
//! asset is only rewritten when the cooked text differs from what it holds, so
//! a save that changes nothing costs one cook and no GPU work.
//!
//! Failures stay local. A cook `slangc` rejects is logged and leaves every
//! asset as it was, so the previous shader keeps drawing until the next save
//! fixes it; WGSL that cooks but fails to compile on the GPU is already logged
//! and skipped per asset by the resources manager.
//!
//! Paths are the crate's own source tree (`CARGO_MANIFEST_DIR`), which is why
//! this is a development feature: a shipped binary has no sources to watch.

// Standard library
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

// External crates
use notify::event::EventKind;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use pill_core::{info, warn};
use pill_engine::AssetManager;

// Current crate
use crate::assets::Shader;
use crate::config::{all_shader_sources, shader_roots, ShaderSourceRecord};

/// How long the sources must stay quiet before a cook starts.
///
/// Editors save in several writes (a temporary file, a rename, a timestamp
/// touch); cooking on the first would read a half-written file.
const SETTLE_DURATION: Duration = Duration::from_millis(150);

/// Watches the renderer's shader sources and applies edits to the asset store.
pub struct ShaderReloader {
    /// The crate's `shaders/`, the root every [`ShaderSourceRecord`] path is
    /// relative to.
    config_directory: PathBuf,
    /// Held only to keep the watch alive; dropping it stops the events.
    _watcher: RecommendedWatcher,
    /// Paths of shader sources the watcher saw change.
    changed_paths: Receiver<PathBuf>,
    /// When the most recent change arrived, while a cook is still owed.
    last_change: Option<Instant>,
}

/// What one [`ShaderReloader::poll`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShaderReloadReport {
    /// Shader assets whose WGSL was replaced.
    pub updated_shaders: Vec<&'static str>,
    /// Whether a cook ran and failed, leaving every asset unchanged.
    pub cook_failed: bool,
}

impl ShaderReloader {
    /// Watch the crate's own shader sources.
    ///
    /// # Errors
    ///
    /// Returns the watcher's error when it cannot be created or a directory
    /// cannot be registered.
    pub fn new() -> notify::Result<Self> {
        Self::watching(Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders"))
    }

    /// Watch the shader sources under `config_directory`.
    ///
    /// # Errors
    ///
    /// Returns the watcher's error when it cannot be created or a directory
    /// cannot be registered.
    pub fn watching(config_directory: PathBuf) -> notify::Result<Self> {
        let (sender, changed_paths) = channel::<PathBuf>();
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let Ok(event) = result else {
                return;
            };
            // Access events are reads, including the cooker's own; only a
            // write, a new file or a removal can change what cooks.
            if !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            for path in event.paths {
                if path
                    .extension()
                    .is_some_and(|extension| extension == "hlsl")
                {
                    // Failure only means the reloader was dropped.
                    let _ = sender.send(path);
                }
            }
        })?;

        // One non-recursive watch per directory the sources can live in: the
        // cooked `.wgsl` outputs sit beside the sources, and watching them too
        // would feed every cook back in as a new change.
        for directory in shader_roots::shader_source_directories(&config_directory) {
            if directory.is_dir() {
                watcher.watch(&directory, RecursiveMode::NonRecursive)?;
            }
        }
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            directory = %config_directory.display(),
            "watching renderer shader sources for edits"
        );

        Ok(Self {
            config_directory,
            _watcher: watcher,
            changed_paths,
            last_change: None,
        })
    }

    /// Apply any settled shader edit to `assets`. Call once per frame.
    ///
    /// Returns immediately while nothing changed or an edit is still settling.
    pub fn poll(&mut self, assets: &mut AssetManager) -> ShaderReloadReport {
        // Step 1: Note the newest change; each one restarts the settle window.
        while let Ok(path) = self.changed_paths.try_recv() {
            pill_core::debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                path = %path.display(),
                "shader source changed"
            );
            self.last_change = Some(Instant::now());
        }
        let Some(last_change) = self.last_change else {
            return ShaderReloadReport::default();
        };
        if last_change.elapsed() < SETTLE_DURATION {
            return ShaderReloadReport::default();
        }
        self.last_change = None;

        // Step 2: Cook. A rejected source leaves the previous WGSL on disk and
        // in the store, so the frame keeps drawing with the old shader.
        let cooked = match shader_roots::cook_config_shaders(&self.config_directory) {
            Ok(cooked) => cooked,
            Err(error) => {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    "shader cook failed; keeping the previous shaders: {error}"
                );
                return ShaderReloadReport {
                    updated_shaders: Vec::new(),
                    cook_failed: true,
                };
            }
        };

        // Step 3: Hand the new text to the assets built from it.
        let updated_shaders =
            apply_cooked_sources(assets, &self.config_directory, all_shader_sources());
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            recooked = cooked.rebuilt_outputs.len(),
            updated = updated_shaders.len(),
            shaders = ?updated_shaders,
            "shader reload applied"
        );
        ShaderReloadReport {
            updated_shaders,
            cook_failed: false,
        }
    }
}

/// Replace the WGSL of every recorded shader asset whose cooked files differ
/// from what it holds, and return the names of those it replaced.
///
/// A record whose asset is not in the store - a pipeline this project never
/// installed - is skipped, as is one whose files cannot be read, which is
/// logged: the asset keeps the text it has.
pub(crate) fn apply_cooked_sources<'a>(
    assets: &mut AssetManager,
    config_directory: &Path,
    records: impl IntoIterator<Item = &'a ShaderSourceRecord>,
) -> Vec<&'static str> {
    let mut updated = Vec::new();
    for record in records {
        let Some(current) = assets.get_by_name::<Shader>(record.shader_asset_name) else {
            continue;
        };
        let read = |relative_path: &str| {
            std::fs::read_to_string(config_directory.join(relative_path)).map_err(|error| {
                warn!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    path = relative_path,
                    "cooked shader could not be read: {error}"
                );
            })
        };
        let (Ok(vertex), Ok(fragment)) = (
            read(record.vertex.relative_path),
            read(record.fragment.relative_path),
        ) else {
            continue;
        };
        if current.vertex_wgsl == vertex && current.fragment_wgsl == fragment {
            continue;
        }
        // The mutable borrow is what moves the asset's content version, so it
        // is taken only for a shader whose text really changed.
        if let Some(shader) = assets.get_by_name_mut::<Shader>(record.shader_asset_name) {
            shader.vertex_wgsl = vertex;
            shader.fragment_wgsl = fragment;
            updated.push(record.shader_asset_name);
        }
    }
    updated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigShaderFile;

    /// A scratch `config` directory holding one vertex and one fragment file.
    fn scratch_config(name: &str) -> PathBuf {
        let directory = std::env::temp_dir()
            .join("pill_master_renderer_shader_reload")
            .join(format!("{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        std::fs::write(directory.join("test_vertex.wgsl"), "vertex one").expect("vertex");
        std::fs::write(directory.join("test_fragment.wgsl"), "fragment one").expect("fragment");
        directory
    }

    /// A record for the two scratch files, stored as `test.shader`.
    const RECORD: ShaderSourceRecord = ShaderSourceRecord {
        shader_asset_name: "test.shader",
        vertex: ConfigShaderFile {
            relative_path: "test_vertex.wgsl",
            embedded_source: "vertex one",
        },
        fragment: ConfigShaderFile {
            relative_path: "test_fragment.wgsl",
            embedded_source: "fragment one",
        },
    };

    /// A store holding `test.shader`, built from the embedded text.
    fn store_with_shader() -> (AssetManager, pill_engine::Handle<Shader>) {
        let mut assets = AssetManager::new();
        let shader = Shader::new("test")
            .with_wgsl(
                RECORD.vertex.embedded_source,
                RECORD.fragment.embedded_source,
            )
            .build()
            .expect("both stages set");
        let handle = assets
            .add_named(RECORD.shader_asset_name, shader)
            .expect("a free name");
        (assets, handle)
    }

    #[test]
    fn an_edited_file_replaces_the_text_and_moves_the_version() {
        let config = scratch_config("edited");
        let (mut assets, handle) = store_with_shader();
        let before = assets.content_version(handle);
        std::fs::write(config.join("test_fragment.wgsl"), "fragment two").expect("edit");

        let updated = apply_cooked_sources(&mut assets, &config, [&RECORD]);

        assert_eq!(updated, ["test.shader"]);
        let shader = assets.get(handle).expect("still live");
        assert_eq!(shader.fragment_wgsl, "fragment two");
        assert_eq!(shader.vertex_wgsl, "vertex one");
        assert_ne!(assets.content_version(handle), before);
    }

    #[test]
    fn unchanged_files_leave_the_version_alone() {
        let config = scratch_config("unchanged");
        let (mut assets, handle) = store_with_shader();
        let before = assets.content_version(handle);

        let updated = apply_cooked_sources(&mut assets, &config, [&RECORD]);

        assert!(updated.is_empty());
        assert_eq!(assets.content_version(handle), before);
    }

    #[test]
    fn a_shader_the_store_does_not_hold_is_skipped() {
        let config = scratch_config("absent");
        let mut assets = AssetManager::new();

        assert!(apply_cooked_sources(&mut assets, &config, [&RECORD]).is_empty());
    }
}

//! Development asset reload: watch the project's `res`, and turn edits into
//! the source paths whose assets need importing again.
//!
//! # Responsibilities
//!
//! - Watch the project's asset directory recursively, and debounce edits like
//!   source edits.
//! - Map each edited file to the source asset it concerns: a source is
//!   itself, a metadata file (`a.png.meta`) is its source (`a.png`).
//! - Queue each change as an [`AssetChange`] for the main thread, which
//!   imports or reimports it through the world's
//!   [`ImportRegistry`](pill_engine::ImportRegistry).
//!
//! # Design
//!
//! The worker thread runs only host code: it never calls into a module image,
//! so a reload can swap any image at any time without a thread running inside
//! the one being retired. The main thread applies the queue at the frame
//! boundary, after the reloads, through the registrations current at that
//! moment - the same split the shader watcher uses.
//!
//! The watcher reports paths, not decisions. Whether a path is an asset at
//! all (its extension has a registered type), whether it is loaded, and what
//! to do about a deleted source are the main thread's to decide, where the
//! registry and the asset store are.
//!
//! Dot-files are ignored: a metadata write goes through a temporary sibling
//! named `.<file>.<pid>-<n>.tmp`, and the file it produces is what counts.

// Standard library
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

// External crates
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use pill_core::error::WatcherError;
use pill_core::{error, info};

// Current crate
use crate::watcher::{debounce_duration, is_relevant_event};

/// The extension of a metadata file, appended to its source's name.
const METADATA_SUFFIX: &str = ".meta";

/// What happened to one source asset, as far as the files say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssetChange {
    /// The source or its metadata file was created or edited: import it, or
    /// reimport it when it is loaded.
    Edited {
        /// The source, relative to the asset directory, `/`-separated.
        source: String,
        /// Whether the event was on the metadata file rather than the source.
        through_metadata: bool,
    },
    /// The source file is gone.
    SourceRemoved {
        /// The source, relative to the asset directory, `/`-separated.
        source: String,
    },
    /// The metadata file is gone while its source remains.
    MetadataRemoved {
        /// The source, relative to the asset directory, `/`-separated.
        source: String,
    },
}

/// A running asset watcher; dropping it lets the worker thread wind down.
pub(crate) struct AssetWatcher {
    /// Changes, oldest first, one per source per debounce window.
    changes: Receiver<AssetChange>,
}

impl AssetWatcher {
    /// Watch `asset_directory` and queue what changes in it.
    ///
    /// Returns `Ok(None)` when the directory does not exist: a project without
    /// `res` has nothing to reload.
    ///
    /// # Errors
    ///
    /// Returns a [`WatcherError`] when the watcher cannot be created or the
    /// directory cannot be registered.
    pub(crate) fn spawn(asset_directory: PathBuf) -> Result<Option<Self>, WatcherError> {
        if !asset_directory.is_dir() {
            return Ok(None);
        }

        // Step 1: Forward every relevant path to the worker. A rename arrives
        // as a remove of the old name and a create of the new one, which is
        // all this stage needs (Stage 9 handles renames as such).
        let (path_sender, paths) = channel::<PathBuf>();
        let mut watcher = RecommendedWatcher::new(
            move |result: Result<Event, notify::Error>| match result {
                Ok(event) if is_relevant_event(&event.kind) || is_rename(&event.kind) => {
                    for path in event.paths {
                        // Failure only means the worker has shut down.
                        let _ = path_sender.send(path);
                    }
                }
                Ok(_) => {}
                // Never panic inside the callback, which some backends run on
                // their own threads; report and continue.
                Err(error) => {
                    error!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        error = %error,
                        "asset watcher error"
                    );
                }
            },
            Config::default(),
        )
        .map_err(|source| WatcherError::CreationFailed { source })?;
        watcher
            .watch(&asset_directory, RecursiveMode::Recursive)
            .map_err(|source| WatcherError::RegistrationFailed {
                path: asset_directory.display().to_string(),
                source,
            })?;
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            directory = %asset_directory.display(),
            "[assets] watching the project's assets for edits"
        );

        // Step 2: The worker: settle, classify, queue.
        let (change_sender, changes) = channel::<AssetChange>();
        std::thread::Builder::new()
            .name("pill asset watcher".to_owned())
            .spawn(move || {
                // Owned here so the watch lasts exactly as long as the worker.
                let _watcher = watcher;
                while let Ok(first_path) = paths.recv() {
                    std::thread::sleep(debounce_duration());
                    let mut touched = vec![first_path];
                    touched.extend(paths.try_iter());
                    for change in classify(&asset_directory, &touched) {
                        if change_sender.send(change).is_err() {
                            // The host dropped the watcher: stop.
                            return;
                        }
                    }
                }
            })
            .map_err(|source| WatcherError::CreationFailed {
                source: notify::Error::io(source),
            })?;

        Ok(Some(Self { changes }))
    }

    /// Every change queued since the last call, oldest first.
    pub(crate) fn drain(&self) -> Vec<AssetChange> {
        self.changes.try_iter().collect()
    }
}

/// Whether the event is one side of a rename, which some backends report as
/// neither a create nor a remove.
fn is_rename(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Modify(ModifyKind::Name(
            RenameMode::Any | RenameMode::From | RenameMode::To | RenameMode::Both
        ))
    )
}

/// One change per source among `touched`, decided by what is on disk now.
///
/// What exists after the debounce is what counts, not the event kinds: an
/// editor saving through a temporary file reports a remove and a create for
/// one save, and the file being there is what says it was an edit.
fn classify(asset_directory: &Path, touched: &[PathBuf]) -> Vec<AssetChange> {
    // Keyed by source, so several events for one asset within a window - the
    // source and its metadata, or one save reported three times - become one.
    let mut by_source: BTreeMap<String, AssetChange> = BTreeMap::new();
    for path in touched {
        let Some(relative) = relative_to(asset_directory, path) else {
            continue;
        };
        let file_name = relative.rsplit('/').next().unwrap_or(&relative);
        if file_name.starts_with('.') || path.is_dir() {
            continue;
        }
        let (source, through_metadata) = match relative.strip_suffix(METADATA_SUFFIX) {
            Some(source) if !source.is_empty() => (source.to_owned(), true),
            _ => (relative.clone(), false),
        };
        let source_exists = asset_directory.join(&source).is_file();
        let change = if !source_exists {
            AssetChange::SourceRemoved {
                source: source.clone(),
            }
        } else if through_metadata && !path.is_file() {
            AssetChange::MetadataRemoved {
                source: source.clone(),
            }
        } else {
            AssetChange::Edited {
                source: source.clone(),
                through_metadata,
            }
        };
        // A source edit outranks a metadata event for the same asset in one
        // window: both mean "decode again", and the source edit is the more
        // informative of the two in the log.
        let keep_existing = matches!(
            (by_source.get(&source), &change),
            (
                Some(AssetChange::Edited {
                    through_metadata: false,
                    ..
                }),
                AssetChange::Edited { .. }
            )
        );
        if !keep_existing {
            by_source.insert(source, change);
        }
    }
    by_source.into_values().collect()
}

/// `path` relative to `root`, with `/` separators, or `None` outside it.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let text = relative.to_str()?.replace('\\', "/");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch asset directory, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("pill-asset-watcher-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("textures")).unwrap();
            Self(root)
        }

        fn write(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::write(&path, b"x").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_metadata_edit_is_a_change_to_its_source() {
        let scratch = Scratch::new("metadata-edit");
        scratch.write("textures/a.png");
        let metadata = scratch.write("textures/a.png.meta");

        assert_eq!(
            classify(&scratch.0, &[metadata]),
            [AssetChange::Edited {
                source: "textures/a.png".to_owned(),
                through_metadata: true,
            }]
        );
    }

    #[test]
    fn several_events_for_one_asset_become_one_change() {
        let scratch = Scratch::new("coalesce");
        let source = scratch.write("textures/a.png");
        let metadata = scratch.write("textures/a.png.meta");

        assert_eq!(
            classify(&scratch.0, &[source.clone(), metadata, source]),
            [AssetChange::Edited {
                source: "textures/a.png".to_owned(),
                through_metadata: false,
            }]
        );
    }

    #[test]
    fn removed_sources_and_metadata_are_told_apart() {
        let scratch = Scratch::new("removed");
        scratch.write("textures/kept.png");

        let changes = classify(
            &scratch.0,
            &[
                scratch.0.join("textures/gone.png"),
                scratch.0.join("textures/kept.png.meta"),
            ],
        );

        assert_eq!(
            changes,
            [
                AssetChange::SourceRemoved {
                    source: "textures/gone.png".to_owned()
                },
                AssetChange::MetadataRemoved {
                    source: "textures/kept.png".to_owned()
                },
            ]
        );
    }

    #[test]
    fn dot_files_directories_and_outside_paths_are_ignored() {
        let scratch = Scratch::new("ignored");
        let temporary = scratch.write("textures/.a.png.meta.12-0.tmp");

        let changes = classify(
            &scratch.0,
            &[
                temporary,
                scratch.0.join("textures"),
                std::env::temp_dir().join("x.png"),
            ],
        );

        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn a_missing_directory_starts_no_watcher() {
        let missing = std::env::temp_dir().join("pill-asset-watcher-definitely-missing");
        assert!(AssetWatcher::spawn(missing).unwrap().is_none());
    }
}

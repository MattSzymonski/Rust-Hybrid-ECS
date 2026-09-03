//! Versioned editor-layout loading and recoverable writes.

use std::path::{Path, PathBuf};

use pill_core::error::EngineMessage;

use super::LayoutModel;
use crate::error::LayoutPersistenceError;

const APPLICATION_DIRECTORY: &str = "RustHybridEcs";
const LAYOUT_FILE: &str = "editor_layout.json";

/// Load the saved workspace, falling back safely on any invalid document.
pub fn load_or_default() -> LayoutModel {
    let path = layout_path();
    // A missing file is the normal first-run state and stays silent.
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return LayoutModel::default_editor();
    };
    let model = match serde_json::from_str::<LayoutModel>(&contents) {
        Ok(model) => model,
        Err(source) => {
            report_load_failure(
                &path,
                LayoutPersistenceError::Invalid {
                    path: path.display().to_string(),
                    source,
                },
            );
            return LayoutModel::default_editor();
        }
    };
    if let Err(source) = model.validate() {
        report_load_failure(&path, LayoutPersistenceError::Validation { source });
        return LayoutModel::default_editor();
    }
    // A layout saved by an older editor schema may not contain the panels this
    // build knows about (for example `Systems`); refuse it loudly instead of
    // silently rendering a workspace without a way to open them.
    if model.schema_version != super::model::LAYOUT_SCHEMA_VERSION {
        eprintln!(
            "[editor] Ignoring saved layout '{}': schema v{} is stale (this build is v{})",
            path.display(),
            model.schema_version,
            super::model::LAYOUT_SCHEMA_VERSION
        );
        return LayoutModel::default_editor();
    }
    model
}

/// Log one rejected layout document and continue with the default layout.
fn report_load_failure(path: &Path, error: LayoutPersistenceError) {
    eprintln!(
        "[editor] Ignoring invalid saved layout '{}': {}",
        path.display(),
        error.to_plain_message()
    );
}

/// Persist a validated model through a temporary file and recoverable replace.
pub fn save(model: &LayoutModel) {
    if let Err(error) = save_to_path(model, layout_path()) {
        eprintln!(
            "[editor] Cannot save dock layout: {}",
            error.to_plain_message()
        );
    }
}

/// Install `model` at `path` without ever corrupting the previous document.
fn save_to_path(model: &LayoutModel, path: PathBuf) -> Result<(), LayoutPersistenceError> {
    model
        .validate()
        .map_err(|source| LayoutPersistenceError::Validation { source })?;
    let parent = path
        .parent()
        .ok_or_else(|| LayoutPersistenceError::Filesystem {
            path: path.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "layout path has no parent",
            ),
        })?;
    std::fs::create_dir_all(parent).map_err(|source| LayoutPersistenceError::Filesystem {
        path: parent.display().to_string(),
        source,
    })?;
    let temporary = path.with_extension("json.tmp");
    let backup = path.with_extension("json.bak");
    let document = serde_json::to_vec_pretty(model)
        .map_err(|source| LayoutPersistenceError::Serialization { source })?;
    std::fs::write(&temporary, document).map_err(|source| LayoutPersistenceError::Filesystem {
        path: temporary.display().to_string(),
        source,
    })?;

    // Unix rename replaces atomically. Windows rename does not replace an
    // existing target, so retain the previous valid file as a short-lived
    // backup while installing the new document.
    if std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(&backup);
        if path.exists() {
            std::fs::rename(&path, &backup).map_err(|source| {
                LayoutPersistenceError::Filesystem {
                    path: backup.display().to_string(),
                    source,
                }
            })?;
        }
        if let Err(source) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::rename(&backup, &path);
            return Err(LayoutPersistenceError::Filesystem {
                path: path.display().to_string(),
                source,
            });
        }
        let _ = std::fs::remove_file(backup);
    }
    Ok(())
}

fn layout_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(APPLICATION_DIRECTORY)
        .join(LAYOUT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_layout_round_trips_through_a_file() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-layout-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("layout.json");
        let model = LayoutModel::default_editor();
        save_to_path(&model, path.clone()).unwrap();
        let restored: LayoutModel = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored, model);
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Unwritable destinations surface as a typed filesystem error.
    #[test]
    fn save_reports_filesystem_failures_as_typed_errors() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-layout-fail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Occupying the directory path with a regular file makes
        // `create_dir_all` fail, which is the first filesystem step.
        std::fs::write(&directory, "not a directory").unwrap();
        let path = directory.join("layout.json");
        let model = LayoutModel::default_editor();
        let error = save_to_path(&model, path).unwrap_err();
        assert!(error
            .to_plain_message()
            .contains("failed to write the dock layout"));
        let _ = std::fs::remove_file(directory);
    }
}

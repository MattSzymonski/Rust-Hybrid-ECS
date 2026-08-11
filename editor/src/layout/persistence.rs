//! Versioned editor-layout loading and recoverable writes.

use std::path::PathBuf;

use super::LayoutModel;

const APPLICATION_DIRECTORY: &str = "RustHybridEcs";
const LAYOUT_FILE: &str = "editor_layout.json";

/// Load the saved workspace, falling back safely on any invalid document.
pub fn load_or_default() -> LayoutModel {
    let path = layout_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return LayoutModel::default_editor();
    };
    match serde_json::from_str::<LayoutModel>(&contents)
        .map_err(|error| error.to_string())
        .and_then(|model| {
            model
                .validate()
                .map(|_| model)
                .map_err(|error| error.to_string())
        }) {
        Ok(model) => model,
        Err(error) => {
            eprintln!(
                "[editor] Ignoring invalid saved layout '{}': {error}",
                path.display()
            );
            LayoutModel::default_editor()
        }
    }
}

/// Persist a validated model through a temporary file and recoverable replace.
pub fn save(model: &LayoutModel) {
    if let Err(error) = save_to_path(model, layout_path()) {
        eprintln!("[editor] Cannot save dock layout: {error}");
    }
}

fn save_to_path(model: &LayoutModel, path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    model.validate()?;
    let parent = path.parent().ok_or("layout path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let backup = path.with_extension("json.bak");
    std::fs::write(&temporary, serde_json::to_vec_pretty(model)?)?;

    // Unix rename replaces atomically. Windows rename does not replace an
    // existing target, so retain the previous valid file as a short-lived
    // backup while installing the new document.
    if std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(&backup);
        if path.exists() {
            std::fs::rename(&path, &backup)?;
        }
        if let Err(error) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::rename(&backup, &path);
            return Err(error.into());
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
}

//! The project's per-manager settings files: created from the manager's
//! defaults, and applied to the live manager.
//!
//! # Responsibilities
//!
//! - Finds every manager resource a loaded generation registered
//!   (`World::manager_resource_ids`).
//! - Writes `<project>/settings/<name>_settings.yaml` from the manager's
//!   complete default document when the project has none, and leaves an
//!   existing file exactly as written.
//! - Applies a manager's document to its live value through the applier the
//!   type registered, and reports which of those happened.
//!
//! # Design
//!
//! A manager owns a subsystem - the audio device, a physics world - and takes
//! its tuning from one flat `key: value` document. The manager itself is the
//! authority on which keys exist: it declares a default document with every key
//! spelled out, and it refuses a document it cannot use. This module only moves
//! bytes: it never invents a key and never edits what a person wrote.
//!
//! The directory is `<project>/settings`, beside `project_settings.yaml` and
//! deliberately outside `res`: a settings file is not an asset, is not loaded
//! through the asset store, and is never imported. That is also why it bakes
//! into a bundle like `logging:` rather than like the theme.
//!
//! Only the development host calls `apply_all`: a shipping or web bundle has no
//! writable project directory and applies the documents its generator baked
//! instead (`pill_player`'s `StaticProject`).
//!
//! A refused document changes nothing. The manager keeps the values it has, and
//! one warning names the manager, the file and the reason - the same rule the
//! theme pipeline applies to a refused override. The file is left as written,
//! because a person is looking at it while it fails.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_core::telemetry::telemetry_target::HOT_RELOAD;
use pill_core::{debug, info, warn};
use pill_engine::{SettingsDocument, SettingsOutcome, World};

// =============================================================================
// Constants
// =============================================================================

/// Suffix of one manager's settings file, appended to its settings name.
const SETTINGS_FILE_SUFFIX: &str = "_settings.yaml";

// =============================================================================
// Types
// =============================================================================

/// What one pass over every manager did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ManagerSettingsReport {
    /// Files written because the project had none.
    pub(crate) created: usize,
    /// Documents the manager accepted.
    pub(crate) applied: usize,
    /// Documents refused by the manager or by the parser.
    pub(crate) refused: usize,
}

// =============================================================================
// Free Functions
// =============================================================================

/// The path of one manager's settings file inside `settings_directory`.
///
/// `None` when the manager's settings name cannot be a file name: a name with a
/// path separator or `..` would let a module write outside the project.
pub(crate) fn settings_file_path(
    settings_directory: &Path,
    settings_name: &str,
) -> Option<PathBuf> {
    let valid = !settings_name.is_empty()
        && settings_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');
    if !valid {
        return None;
    }
    Some(settings_directory.join(format!("{settings_name}{SETTINGS_FILE_SUFFIX}")))
}

/// The manager name a settings file name stands for.
///
/// `pill_audio_settings.yaml` is `pill_audio`. `None` for a file that is not a
/// manager's settings file at all.
pub(crate) fn settings_name_of_file(file_name: &str) -> Option<String> {
    let stem = file_name.strip_suffix(SETTINGS_FILE_SUFFIX)?;
    if stem.is_empty() {
        return None;
    }
    Some(stem.to_owned())
}

/// Applies every manager's settings file, creating a missing file first.
///
/// Runs once after the project has loaded, which is the first moment every
/// manager exists: the project's `init` is the last artifact to register one.
pub(crate) fn apply_all(world: &mut World, settings_directory: &Path) -> ManagerSettingsReport {
    let mut report = ManagerSettingsReport::default();
    for id in world.manager_resource_ids() {
        let Some(entry) = world.manager_settings(id) else {
            continue;
        };
        let Some(path) = settings_file_path(settings_directory, &entry.settings_name) else {
            warn!(
                target: HOT_RELOAD,
                manager = entry.settings_name.as_str(),
                type_name = entry.declaring_type_name.as_str(),
                "[settings] a manager's settings name is not a file name; its settings cannot be stored"
            );
            continue;
        };
        let mut created = false;
        // Step 1: Read the file, writing the manager's defaults when the
        // project has none. A write that fails leaves the manager on its
        // defaults and says so.
        let document_text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(write_error) = write_default_document(&path, &entry.default_document) {
                    warn!(
                        target: HOT_RELOAD,
                        manager = entry.settings_name.as_str(),
                        path = %path.display(),
                        error = %write_error,
                        "[settings] could not create a manager's settings file; the manager keeps its defaults"
                    );
                    continue;
                }
                created = true;
                report.created += 1;
                info!(
                    target: HOT_RELOAD,
                    manager = entry.settings_name.as_str(),
                    type_name = entry.declaring_type_name.as_str(),
                    path = %path.display(),
                    "[settings] created the manager settings file"
                );
                entry.default_document.clone()
            }
            Err(error) => {
                warn!(
                    target: HOT_RELOAD,
                    manager = entry.settings_name.as_str(),
                    path = %path.display(),
                    error = %error,
                    "[settings] could not read a manager's settings file; the manager keeps its defaults"
                );
                continue;
            }
        };
        // Step 2: Apply it, and say what the manager decided.
        match apply_document(
            world,
            id,
            &entry.settings_name,
            &path,
            &document_text,
            created,
            false,
        ) {
            Some(SettingsOutcome::Applied) => report.applied += 1,
            Some(SettingsOutcome::Refused) => report.refused += 1,
            None => {}
        }
    }
    report
}

/// Re-applies the settings of the manager whose file changed.
///
/// Returns `None` when no manager owns that file any more, or when the file is
/// gone - a deleted file leaves the manager on the values it already has until
/// the next start.
pub(crate) fn apply_named(
    world: &mut World,
    settings_directory: &Path,
    settings_name: &str,
) -> Option<SettingsOutcome> {
    let id = world.manager_resource_ids().into_iter().find(|id| {
        world
            .manager_settings(*id)
            .is_some_and(|entry| entry.settings_name == settings_name)
    })?;
    let entry = world.manager_settings(id)?;
    let path = settings_file_path(settings_directory, &entry.settings_name)?;
    let document_text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            debug!(
                target: HOT_RELOAD,
                manager = entry.settings_name.as_str(),
                path = %path.display(),
                error = %error,
                "[settings] a manager's settings file could not be read; keeping the applied values"
            );
            return None;
        }
    };
    apply_document(
        world,
        id,
        &entry.settings_name,
        &path,
        &document_text,
        false,
        true,
    )
}

/// Parses `document_text` and hands it to the manager, logging the outcome.
///
/// Returns `None` when the id no longer holds a value, which happens when a
/// reload replaced the manager between the listing and the call.
fn apply_document(
    world: &mut World,
    id: pill_engine::ResourceId,
    settings_name: &str,
    path: &Path,
    document_text: &str,
    created: bool,
    changed: bool,
) -> Option<SettingsOutcome> {
    let document = match SettingsDocument::parse(document_text) {
        Ok(document) => document,
        Err(error) => {
            warn!(
                target: HOT_RELOAD,
                manager = settings_name,
                path = %path.display(),
                reason = %error,
                "[settings] refused manager settings; the manager keeps the values it has"
            );
            return Some(SettingsOutcome::Refused);
        }
    };
    match world.apply_manager_settings(id, &document)? {
        SettingsOutcome::Applied => {
            info!(
                target: HOT_RELOAD,
                manager = settings_name,
                path = %path.display(),
                created,
                changed,
                keys = document.len(),
                "[settings] applied manager settings"
            );
            Some(SettingsOutcome::Applied)
        }
        SettingsOutcome::Refused => {
            warn!(
                target: HOT_RELOAD,
                manager = settings_name,
                path = %path.display(),
                "[settings] refused manager settings; the manager keeps the values it has"
            );
            Some(SettingsOutcome::Refused)
        }
    }
}

/// Writes `default_document` to `path`, creating the directory if needed.
fn write_default_document(path: &Path, default_document: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, default_document)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    // Standard library
    use std::sync::atomic::{AtomicU32, Ordering};

    // External crates
    use pill_engine::{ManagerResource, Resource};

    // Current crate
    use super::*;
    use crate::config::PROJECT_SETTINGS_DIRECTORY;

    /// Counts successful applies across every manager of this test module.
    static APPLIED_COUNT: AtomicU32 = AtomicU32::new(0);

    /// A manager the tests configure: one number and one flag.
    #[derive(Debug, PartialEq)]
    struct FixtureManager {
        /// The number the applier reads.
        gain: f32,
        /// The flag the applier reads.
        enabled: bool,
    }

    impl FixtureManager {
        /// The values [`ManagerResource::default_settings`] describes.
        fn new() -> Self {
            Self {
                gain: 1.0,
                enabled: true,
            }
        }
    }

    impl Resource for FixtureManager {
        fn shared_name() -> Option<&'static str> {
            Some("pill_development_host::manager_settings::tests::FixtureManager")
        }
    }

    impl ManagerResource for FixtureManager {
        fn settings_name() -> &'static str {
            "fixture"
        }

        fn default_settings() -> &'static str {
            "# Fixture settings. Every key, with the value it takes when untouched.\nenabled: true\ngain: 1.0\n"
        }

        fn apply_settings(&mut self, settings: &SettingsDocument) -> SettingsOutcome {
            if !settings.unknown_keys(&["enabled", "gain"]).is_empty() {
                return SettingsOutcome::Refused;
            }
            if let Some(enabled) = settings.bool("enabled") {
                self.enabled = enabled;
            }
            if let Some(gain) = settings.float("gain") {
                self.gain = gain as f32;
            }
            APPLIED_COUNT.fetch_add(1, Ordering::SeqCst);
            SettingsOutcome::Applied
        }
    }

    /// A scratch project directory, removed with the test.
    struct ScratchProject {
        root: PathBuf,
    }

    impl ScratchProject {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "pill-manager-settings-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("a scratch project");
            Self { root }
        }

        fn settings_directory(&self) -> PathBuf {
            self.root.join(PROJECT_SETTINGS_DIRECTORY)
        }

        fn settings_file(&self) -> PathBuf {
            self.settings_directory().join("fixture_settings.yaml")
        }
    }

    impl Drop for ScratchProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A world holding the fixture manager.
    fn world_with_manager() -> World {
        let mut world = World::new();
        world.insert_manager_resource(FixtureManager::new());
        world
    }

    /// A project without a settings file gets one holding every default, and
    /// the manager is applied from it.
    #[test]
    fn a_missing_file_is_created_with_every_default_and_applied() {
        let project = ScratchProject::new("created");
        let mut world = world_with_manager();

        let report = apply_all(&mut world, &project.settings_directory());

        assert_eq!(
            report,
            ManagerSettingsReport {
                created: 1,
                applied: 1,
                refused: 0
            }
        );
        let written =
            std::fs::read_to_string(project.settings_file()).expect("the file was written");
        assert_eq!(written, FixtureManager::default_settings());
        assert_eq!(
            world.get_resource::<FixtureManager>(),
            Some(&FixtureManager::new())
        );
    }

    /// An existing file is applied and never rewritten, and an unknown key is
    /// refused without touching the manager.
    #[test]
    fn an_existing_file_is_applied_and_a_refused_one_changes_nothing() {
        let project = ScratchProject::new("existing");
        std::fs::create_dir_all(project.settings_directory()).expect("the directory");
        std::fs::write(project.settings_file(), "enabled: false\ngain: 3.5\n")
            .expect("the settings file");
        let mut world = world_with_manager();

        let report = apply_all(&mut world, &project.settings_directory());

        assert_eq!(report.created, 0);
        assert_eq!(report.applied, 1);
        let manager = world.get_resource::<FixtureManager>().expect("it is there");
        assert!(!manager.enabled);
        assert_eq!(manager.gain, 3.5);

        std::fs::write(project.settings_file(), "enabled: false\ngian: 3.5\n")
            .expect("the settings file");
        let refused = apply_all(&mut world, &project.settings_directory());

        assert_eq!(refused.refused, 1);
        assert_eq!(refused.applied, 0);
        let manager = world.get_resource::<FixtureManager>().expect("it is there");
        assert_eq!(
            manager,
            &FixtureManager {
                gain: 3.5,
                enabled: false
            },
            "the refused document changed nothing"
        );
    }

    /// The live path re-applies one manager by name, and does nothing for a
    /// file no manager owns.
    #[test]
    fn a_named_file_re_applies_only_that_manager() {
        let project = ScratchProject::new("named");
        let mut world = world_with_manager();
        apply_all(&mut world, &project.settings_directory());
        std::fs::write(project.settings_file(), "gain: 2.0\n").expect("the settings file");

        let applied = apply_named(&mut world, &project.settings_directory(), "fixture");

        assert_eq!(applied, Some(SettingsOutcome::Applied));
        assert_eq!(
            world
                .get_resource::<FixtureManager>()
                .expect("it is there")
                .gain,
            2.0
        );
        assert_eq!(
            apply_named(&mut world, &project.settings_directory(), "unknown"),
            None
        );
    }

    /// A settings name that could escape the project is refused before any path
    /// is built from it.
    #[test]
    fn a_settings_name_cannot_leave_the_settings_directory() {
        let directory = Path::new("project/settings");

        assert!(settings_file_path(directory, "../escape").is_none());
        assert!(settings_file_path(directory, "sub/dir").is_none());
        assert!(settings_file_path(directory, "").is_none());
        assert_eq!(
            settings_file_path(directory, "pill_audio"),
            Some(directory.join("pill_audio_settings.yaml"))
        );
    }
}

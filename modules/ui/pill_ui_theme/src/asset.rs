//! The `.theme` standalone asset.
//!
//! # Responsibilities
//!
//! - [`THEME_SHARED_NAME`]: the pinned asset identity across binaries.
//! - The [`Asset`](pill_engine_core::Asset) and
//!   [`StandaloneAsset`](pill_engine_core::StandaloneAsset) implementations for
//!   [`ThemeOverrides`], so a `.theme` file loads by extension through the
//!   world's import registry like a `.material` or `.scene`.
//!
//! # Design
//!
//! The file is a standalone asset: its own header carries `format_version`,
//! `asset_type` and `guid`, and the override body sits under `asset`, so there
//! is no `.meta`. The type has no references to resolve, so `from_document` is
//! the identity: the document *is* the asset. The pinned shared name keeps one
//! asset column across the project, `pill_project_ui`, the editor shell and
//! `pill_editor_core`, each of which compiles its own copy of this crate.

// External crates
use trait_type_map::impl_trait_accessible;

// Current crate
use pill_engine_core::{Asset, AssetLoadResult, AssetManager, StandaloneAsset};

use crate::overrides::ThemeOverrides;

/// The pinned shared name of [`ThemeOverrides`]; see the comment on its `Asset`
/// implementation.
pub const THEME_SHARED_NAME: &str = "pill_ui_theme::ThemeOverrides";

impl Asset for ThemeOverrides {
    fn shared_name() -> Option<&'static str> {
        Some(THEME_SHARED_NAME)
    }
}

impl StandaloneAsset for ThemeOverrides {
    type Document = ThemeOverrides;

    const FILE_EXTENSION: &'static str = "theme";

    fn from_document(
        _name: &str,
        document: ThemeOverrides,
        _assets: &AssetManager,
    ) -> AssetLoadResult<Self> {
        Ok(document)
    }
}

// The trait object the registry stores; every asset type registers one.
impl_trait_accessible!(dyn Asset; ThemeOverrides);

#[cfg(test)]
mod tests {
    // Standard library
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    // External crates
    use pill_engine_core::asset_store::mount_directory;
    use pill_engine_core::{render_standalone, Asset, AssetGuid, AssetManager, World};

    // Current crate
    use crate::overrides::ThemeOverrides;
    use crate::register::{load_overrides, ThemePathScope};
    use crate::{register, THEME_SHARED_NAME};

    /// Serializes this crate's tests that mount the process-wide asset
    /// directory.
    static MOUNT_LOCK: Mutex<()> = Mutex::new(());

    /// A scratch `res` directory mounted under the mounted-directory lock.
    struct ScratchRes {
        root: PathBuf,
        _mounted: MutexGuard<'static, ()>,
    }

    impl ScratchRes {
        fn new(label: &str) -> Self {
            let mounted = MOUNT_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root =
                std::env::temp_dir().join(format!("pill-ui-theme-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("ui")).unwrap();
            mount_directory(&root);
            Self {
                root,
                _mounted: mounted,
            }
        }
    }

    impl Drop for ScratchRes {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_shared_name_is_pinned() {
        assert_eq!(ThemeOverrides::shared_name(), Some(THEME_SHARED_NAME));
        assert_eq!(THEME_SHARED_NAME, "pill_ui_theme::ThemeOverrides");
    }

    #[test]
    fn registration_makes_theme_a_standalone_extension() {
        let mut world = World::new();
        register(&mut world);
        let registry = world
            .get_resource::<pill_engine_core::ImportRegistry>()
            .unwrap();
        assert!(registry.is_standalone_extension("theme"));
    }

    #[test]
    fn a_theme_file_round_trips_through_the_asset_store() {
        let res = ScratchRes::new("round-trip");
        let document = ThemeOverrides::from_json_str(
            r##"{"palette": {"accent": "#33d17a"}, "fonts": {"body": "fonts/body.ttf"}}"##,
        )
        .unwrap();
        let bytes = render_standalone::<ThemeOverrides>(
            AssetGuid::random().expect("a fresh guid"),
            &document,
        )
        .expect("the document renders");
        std::fs::write(res.root.join("ui/accent.theme"), bytes).unwrap();

        let mut world = World::new();
        register(&mut world);
        let handle = load_overrides(&mut world, "ui/accent.theme", ThemePathScope::Game).unwrap();
        let assets = world.get_resource::<AssetManager>().unwrap();
        let loaded = assets.get(handle).expect("the theme stays loaded");

        assert_eq!(
            serde_json::to_value(loaded).unwrap(),
            serde_json::to_value(&document).unwrap()
        );
    }
}

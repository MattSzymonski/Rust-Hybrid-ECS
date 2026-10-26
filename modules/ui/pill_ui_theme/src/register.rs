//! Registration, path scopes and the loaders.
//!
//! # Responsibilities
//!
//! - [`register()`]: declares the `.theme` standalone asset and the font asset
//!   on a world.
//! - [`ThemePathScope`]: the game/editor path rule - a game never loads editor
//!   content, and an editor layer only loads from `editor/`.
//! - [`load_overrides`]: imports a `.theme` file and returns its handle, shaped
//!   like `Engine::load_scene_file` but without the scene step.
//! - [`load_font`]: imports the font a theme layer named, with the content
//!   version a UI's font signature reads.
//!
//! # Design
//!
//! The scope check runs before the store sees the path, so a path outside the
//! caller's scope is refused with the rule, not a filesystem error. Both scopes
//! go through `checked_relative`: an absolute path or a `..` segment is refused
//! before the asset store can resolve it.

// Standard library
use std::path::Path;

// Current crate
use pill_engine_core::asset_metadata::checked_relative;
use pill_engine_core::{AssetManager, Handle, World};

use crate::font::FontAsset;
use crate::overrides::ThemeOverrides;

/// Which part of a project's `res` a theme path may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemePathScope {
    /// A game's own theme: anything below `res` except `editor/`.
    Game,
    /// An editor layer: only below `res/editor/`.
    Editor,
}

impl ThemePathScope {
    /// Apply this scope's rule to `path`, returning the normalized asset path.
    ///
    /// # Errors
    ///
    /// A message naming the rule when a game names `editor/...`, when an editor
    /// layer names anything outside it, or when the path is absolute or leaves
    /// the asset directory.
    pub fn check(self, path: &str) -> Result<String, String> {
        let normalized = checked_relative(path)?;
        let key = normalized.to_string_lossy().replace('\\', "/");
        let under_editor = key == "editor" || key.starts_with("editor/");
        match self {
            Self::Game if under_editor => Err(format!(
                "`{path}` is editor-only content; a game theme must be outside `editor/`"
            )),
            Self::Editor if !under_editor => Err(format!(
                "`{path}` must be under `editor/` for an editor theme"
            )),
            _ => Ok(key),
        }
    }
}

/// Declare the `.theme` standalone asset and the font asset on `world`.
///
/// Call this once from the registration of the crate that owns the theme (a
/// project's `install_with_theme`, the editor's registration). Registering the
/// same type from several binaries is supported; the newest live registration
/// serves the type's import entry points.
pub fn register(world: &mut World) {
    world.register_standalone_asset::<ThemeOverrides>();
    world.register_imported_asset::<FontAsset>();
}

/// Import the `.theme` file at `path` and return its handle.
///
/// `path` is below the project's asset directory; `scope` decides whether a
/// game or an editor layer may name it.
///
/// # Errors
///
/// The scope rule's message, a missing `AssetManager`, or why the file could
/// not be read, is not a theme file, or does not fit the override document.
pub fn load_overrides(
    world: &mut World,
    path: &str,
    scope: ThemePathScope,
) -> Result<Handle<ThemeOverrides>, String> {
    let path = scope.check(path)?;
    let assets = world
        .get_resource_mut::<AssetManager>()
        .ok_or_else(|| "the engine has no AssetManager".to_string())?;
    let outcome = assets
        .import_standalone::<ThemeOverrides>(Path::new(&path))
        .map_err(|error| format!("`{path}` did not load: {error}"))?;
    Ok(outcome.handle)
}

/// A font a theme layer named, loaded.
pub struct LoadedFont {
    /// The font's handle in the asset store.
    pub handle: Handle<FontAsset>,
    /// The store's content version for it: a signature hashes this rather
    /// than the bytes, and it moves exactly when the file does.
    pub version: Option<u64>,
}

/// Import the font a theme layer named and return its handle.
///
/// `path` is below the project's asset directory and follows the same scope
/// rule as [`load_overrides`]. A file with no `.meta` gets one, so a font a
/// code default names is usable from the first run.
///
/// # Errors
///
/// The scope rule's message, why the file could not be read or is empty, or a
/// guid conflict with another loaded font.
pub fn load_font(
    world: &mut World,
    path: &str,
    scope: ThemePathScope,
) -> Result<LoadedFont, String> {
    let path = scope.check(path)?;
    let handle = world
        .load_asset::<FontAsset>(&path)
        .map_err(|error| format!("font `{path}` did not load: {error}"))?;
    let version = world
        .get_resource::<AssetManager>()
        .and_then(|assets| assets.content_version(handle));
    Ok(LoadedFont { handle, version })
}

#[cfg(test)]
mod tests {
    use super::ThemePathScope;

    #[test]
    fn a_game_may_not_name_editor_content() {
        assert_eq!(
            ThemePathScope::Game.check("ui/a.theme").unwrap(),
            "ui/a.theme"
        );
        let refused = ThemePathScope::Game.check("editor/a.theme").unwrap_err();
        assert!(refused.contains("editor/"), "{refused}");
        assert!(ThemePathScope::Game.check("editor").is_err());
    }

    #[test]
    fn an_editor_layer_must_name_editor_content() {
        assert_eq!(
            ThemePathScope::Editor.check("editor/a.theme").unwrap(),
            "editor/a.theme"
        );
        let refused = ThemePathScope::Editor.check("ui/a.theme").unwrap_err();
        assert!(refused.contains("editor/"), "{refused}");
    }

    #[test]
    fn both_scopes_refuse_paths_outside_the_asset_directory() {
        for scope in [ThemePathScope::Game, ThemePathScope::Editor] {
            assert!(scope.check("../a.theme").is_err());
            assert!(scope.check("C:/a.theme").is_err());
            assert!(scope.check("/a.theme").is_err());
        }
    }

    #[test]
    fn a_leading_dot_segment_is_normalized_away() {
        assert_eq!(
            ThemePathScope::Game.check("./ui/a.theme").unwrap(),
            "ui/a.theme"
        );
    }
}

//! Asset previews on the editor's side: the preference that
//! turns them on, and the tiles whose previews are worth queueing.
//!
//! # Responsibilities
//!
//! - [`RENDER_PREVIEWS_KEY`] and [`render_previews`]: the preference a person
//!   flips in the Preferences tab.
//! - [`preview_asset`]: which renderer preview a listing row asks for, from its
//!   asset type, its file and its guid.
//! - [`PreviewQueue`]: the tile paths one grid pass drew, in draw order.
//!
//! # Design
//!
//! Nothing here renders: the renderer draws the preview and the executable
//! keeps the resulting image, so this module only decides *what* to ask for.

// Standard library
use std::path::Path;

// External crates
use serde_json::{Map, Value};

// Current crate
use crate::editor_state::AssetListing;

/// The preference key previews live under.
pub const RENDER_PREVIEWS_KEY: &str = "editor.render_asset_previews";

/// Whether the file asks for rendered asset previews; only a stored `false`
/// turns them off, so a fresh preferences file renders them and an absent key
/// cannot silently change the posture.
pub fn render_previews(document: &Map<String, Value>) -> bool {
    match crate::preferences::lookup(document, RENDER_PREVIEWS_KEY) {
        Some(Value::Bool(enabled)) => *enabled,
        Some(Value::String(text)) => !text.trim().eq_ignore_ascii_case("false"),
        _ => true,
    }
}

/// The preview a listing row asks for: a texture, a material or a mesh, when
/// its type is one of those. A texture and a mesh are named by their file
/// below their own root, because a project may load an asset under its own
/// name or not load it at all; a material is named by the guid its sidecar
/// carries.
pub fn preview_asset(row: &AssetListing, root: &Path) -> Option<pill_renderer_api::PreviewAsset> {
    match row.kind.type_leaf() {
        Some("Texture") => Some(pill_renderer_api::PreviewAsset::Texture {
            path: preview_path(row, root)?.to_string_lossy().into_owned(),
        }),
        Some("Material") => Some(pill_renderer_api::PreviewAsset::Material {
            guid: row.guid.clone()?,
        }),
        Some("Mesh") => Some(pill_renderer_api::PreviewAsset::Mesh {
            path: preview_path(row, root)?.to_string_lossy().into_owned(),
        }),
        _ => None,
    }
}

/// The file a row's preview reads: its path below the project's asset
/// directory, or below its own module's directory - a module's resources do
/// not live in the project at all. `None` when the row names a module that is
/// no longer loaded, because the project's tree cannot stand in for it.
fn preview_path(row: &AssetListing, root: &Path) -> Option<std::path::PathBuf> {
    if row.root.is_empty() {
        return Some(root.join(&row.path));
    }
    pill_engine::asset_store::module_roots()
        .into_iter()
        .find(|module_root| module_root.module == row.root)
        .map(|module_root| module_root.directory.join(&row.path))
}

/// The preview a listing row's key asks for, by looking the row up.
///
/// The key is what the panel names a row by - the path for the project's own
/// tree, prefixed with the extensions root for a module's file - so a module's
/// texture and a project file that share a relative path cannot be confused.
pub fn preview_asset_for<'a>(
    assets: impl IntoIterator<Item = &'a AssetListing>,
    key: &str,
    root: &Path,
) -> Option<pill_renderer_api::PreviewAsset> {
    assets
        .into_iter()
        .find(|row| row.key() == key)
        .and_then(|row| preview_asset(row, root))
}

/// The tile paths one pass of the Assets grid drew, in draw order.
///
/// [`PreviewQueue::clear`] starts a pass and [`PreviewQueue::push`] records a
/// drawn tile, so a reader can ask for previews only for what is visible.
#[derive(Default)]
pub struct PreviewQueue {
    /// The visible tiles' paths, in draw order.
    pub paths: Vec<String>,
}

impl pill_engine::Resource for PreviewQueue {}

impl PreviewQueue {
    /// Start a new pass's list of visible tiles.
    pub fn clear(&mut self) {
        self.paths.clear();
    }

    /// Record one tile that was drawn.
    pub fn push(&mut self, path: &str) {
        self.paths.push(path.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_state::AssetKind;

    fn row(path: &str, kind: &str, guid: Option<&str>) -> AssetListing {
        AssetListing {
            root: String::new(),
            path: path.to_string(),
            read_only: false,
            kind: AssetKind::parse(kind),
            standalone: false,
            has_meta: guid.is_some(),
            guid: guid.map(str::to_string),
        }
    }

    /// The preference reads a boolean and a boolean's text; a file that says
    /// nothing renders them, and only a stored `false` turns them off.
    #[test]
    fn the_preview_preference_reads_booleans_and_defaults_on() {
        let mut document = Map::new();
        assert!(render_previews(&document));
        crate::preferences::set_key(&mut document, RENDER_PREVIEWS_KEY, Value::Bool(true)).unwrap();
        assert!(render_previews(&document));
        crate::preferences::set_key(&mut document, RENDER_PREVIEWS_KEY, Value::Bool(false))
            .unwrap();
        assert!(!render_previews(&document));
        crate::preferences::set_key(
            &mut document,
            RENDER_PREVIEWS_KEY,
            Value::String(" TRUE ".to_string()),
        )
        .unwrap();
        assert!(render_previews(&document));
        crate::preferences::set_key(
            &mut document,
            RENDER_PREVIEWS_KEY,
            Value::String("false".to_string()),
        )
        .unwrap();
        assert!(!render_previews(&document));
        // Anything that is not the word "false" leaves them on: a typo must
        // not take the feature away from someone who asked for it.
        crate::preferences::set_key(
            &mut document,
            RENDER_PREVIEWS_KEY,
            Value::String("yes".to_string()),
        )
        .unwrap();
        assert!(render_previews(&document));
    }

    /// A row's preview comes from its type's last segment: a texture and a
    /// mesh name their file below the asset root, a material names its guid,
    /// and a row whose type has no preview asks for nothing.
    #[test]
    fn a_row_names_the_preview_its_type_asks_for() {
        let root = Path::new("C:/project/res");
        let assets = vec![
            row("textures/stone.png", "demo::Texture", Some("aa")),
            row("materials/red.material", "demo::Material", Some("bb")),
            row("models/cube.obj", "demo::Mesh", Some("cc")),
            row("models/loose.obj", "demo::Mesh", None),
            row("scenes/main.scene", "demo::Scene", Some("dd")),
        ];
        assert_eq!(
            preview_asset_for(&assets, "textures/stone.png", root),
            Some(pill_renderer_api::PreviewAsset::Texture {
                path: root
                    .join("textures/stone.png")
                    .to_string_lossy()
                    .into_owned()
            })
        );
        assert_eq!(
            preview_asset_for(&assets, "materials/red.material", root),
            Some(pill_renderer_api::PreviewAsset::Material {
                guid: "bb".to_string()
            })
        );
        assert_eq!(
            preview_asset_for(&assets, "models/cube.obj", root),
            Some(pill_renderer_api::PreviewAsset::Mesh {
                path: root.join("models/cube.obj").to_string_lossy().into_owned()
            })
        );
        // A mesh needs no sidecar: the file is what the pass decodes.
        assert_eq!(
            preview_asset_for(&assets, "models/loose.obj", root),
            Some(pill_renderer_api::PreviewAsset::Mesh {
                path: root.join("models/loose.obj").to_string_lossy().into_owned()
            })
        );
        assert!(preview_asset_for(&assets, "scenes/main.scene", root).is_none());
        assert!(preview_asset_for(&assets, "textures/missing.png", root).is_none());
    }

    /// A module's file resolves below its own root, and a row naming a module
    /// that is not loaded asks for nothing rather than reading the project's
    /// tree at the same path.
    #[test]
    fn a_modules_file_never_resolves_into_the_projects_tree() {
        let root = Path::new("C:/project/res");
        let mut module_row = row("textures/sky.png", "demo::Texture", Some("aa"));
        module_row.root = "a-module-that-is-not-loaded".to_owned();
        let key = module_row.key();
        assert_eq!(
            key,
            "extensions/a-module-that-is-not-loaded/textures/sky.png"
        );
        assert_eq!(preview_asset(&module_row, root), None);
        assert_eq!(preview_asset_for(&[module_row], &key, root), None);
    }
}

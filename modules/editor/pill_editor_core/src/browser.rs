//! The Assets tab's project browser: the folder the grid lists, the tree's
//! open folders, and the listing math both panes share.
//!
//! # Responsibilities
//!
//! - [`ProjectBrowser`]: the browser's session state - the folder the grid
//!   lists, the folders the tree shows open, and the tile size the grid
//!   draws.
//! - The panel's roots ([`ROOTS`], [`is_root`], [`root_of`]): the project's
//!   own tree, and every extension's read-only resources beside it.
//! - The listing math both panes read: [`children`], [`child_folders`],
//!   [`breadcrumb`], [`parent_folder`], [`file_name`] and [`tile_columns`].
//! - [`drop_folder_relative`]: where a dropped file lands, validated so a
//!   folder outside the asset directory is refused rather than joined.
//!
//! # Design
//!
//! The snapshot's asset listing is one flat, project-relative list of files
//! and folders ([`AssetListing`]), so both panes derive what they show from
//! that list: the tree walks its folder rows, the grid keeps the open
//! folder's direct children, and the breadcrumb cuts the open folder's path.
//! The panel's roots are the exception that shapes the rest: a row whose key
//! *is* a root is never listed as somebody's child, so the extensions' rows
//! reach the tree only through their own root row. Nothing here touches egui
//! or the disk, which is what lets the tree, the grid, the breadcrumb and the
//! scripted channel agree on one state.

// Standard library
use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::path::PathBuf;

// External crates
use pill_core::telemetry::telemetry_target;

// Current crate
use crate::editor_state::{AssetListing, EXTENSIONS_ROOT};

/// The label the project's root row and the breadcrumb start with.
pub const ROOT_LABEL: &str = "Assets";

/// One root of the Assets panel: the key it opens and the label its row
/// carries.
///
/// A root is the panel's own level, not a folder inside another root's tree:
/// the tree draws both of them at the left edge, and a row whose key *is* a
/// root is never listed as somebody's child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetRoot {
    /// The key the root's row opens; empty for the project's own tree.
    pub key: &'static str,
    /// The label the root's row and the breadcrumb's first segment carry.
    pub label: &'static str,
    /// Whether everything below the root is an extension's read-only
    /// resource: listed and inspected, never written into.
    pub read_only: bool,
}

/// The panel's roots, in the order its tree draws them: the project's own
/// assets first, every extension's resources beside them.
pub const ROOTS: [AssetRoot; 2] = [
    AssetRoot {
        key: "",
        label: ROOT_LABEL,
        read_only: false,
    },
    AssetRoot {
        key: EXTENSIONS_ROOT,
        label: EXTENSIONS_ROOT,
        read_only: true,
    },
];

/// Whether `key` is one of the panel's roots.
pub fn is_root(key: &str) -> bool {
    ROOTS.iter().any(|root| root.key == key)
}

/// The root `folder` belongs to: the extensions' for everything under
/// [`EXTENSIONS_ROOT`], the project's tree for anything else.
pub fn root_of(folder: &str) -> AssetRoot {
    ROOTS
        .iter()
        .copied()
        .rfind(|root| {
            !root.key.is_empty()
                && (folder == root.key || folder.starts_with(&format!("{}/", root.key)))
        })
        .unwrap_or(ROOTS[0])
}

/// The tile size the grid starts at, in points.
pub const DEFAULT_TILE_SIZE: f32 = 72.0;

/// The tile sizes the grid's slider spans, in points.
pub const TILE_SIZE_RANGE: RangeInclusive<f32> = 40.0..=120.0;

/// The Assets tab's project browser: the two panes' shared state.
///
/// Session-scoped, like the toolbox: a restart starts the browser at the
/// project's asset root. The state lives in the shared `dylib` with the rest
/// of the session, so a shell reload keeps the folder the user was looking
/// at.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectBrowser {
    /// The folder the grid lists, relative to the asset root; empty for the
    /// root itself.
    pub folder: String,
    /// The folders whose tree rows show their children.
    pub expanded: BTreeSet<String>,
    /// The tile size the grid draws, in points.
    pub tile_size: f32,
    /// Each tile's last logged rectangle, keyed by its path: logged once per
    /// placement, so a suite can drag a tile without reading pixels.
    pub tile_rects: Vec<(String, [f32; 4])>,
    /// Each folder tree row's last logged rectangle, keyed by the folder: what
    /// a suite right-clicks without reading pixels.
    pub tree_rects: Vec<(String, [f32; 4])>,
    /// The grid pane's last logged rectangle: the area whose right-click opens
    /// the container menu.
    pub pane_rect: Option<[f32; 4]>,
}

impl Default for ProjectBrowser {
    fn default() -> Self {
        Self {
            folder: String::new(),
            expanded: BTreeSet::new(),
            tile_size: DEFAULT_TILE_SIZE,
            tile_rects: Vec::new(),
            tree_rects: Vec::new(),
            pane_rect: None,
        }
    }
}

impl ProjectBrowser {
    /// Open `folder` in the grid, logging the token the suite watches.
    /// Returns whether the open folder changed.
    ///
    /// The folder's ancestors open in the tree with it, so the tree always
    /// shows where the grid is looking.
    pub fn open(&mut self, folder: &str) -> bool {
        let folder = folder.trim_matches('/').to_string();
        self.expand_ancestors(&folder);
        if self.folder == folder {
            return false;
        }
        self.folder = folder;
        pill_core::info!(
            target: telemetry_target::ECS,
            path = self.folder.as_str(),
            "[editor] browser folder"
        );
        true
    }

    /// Open or close a folder's tree row. Returns whether it moved.
    pub fn toggle_expanded(&mut self, folder: &str) -> bool {
        if self.expanded.contains(folder) {
            self.expanded.remove(folder);
            false
        } else {
            self.expanded.insert(folder.to_string());
            true
        }
    }

    /// Whether the tree shows `folder` open.
    pub fn is_expanded(&self, folder: &str) -> bool {
        self.expanded.contains(folder)
    }

    /// Open every ancestor of `path` in the tree, so a folder the grid shows
    /// or a file the selection names has a visible row.
    pub fn expand_ancestors(&mut self, path: &str) {
        let mut ancestor = parent_folder(path);
        while !ancestor.is_empty() {
            self.expanded.insert(ancestor.to_string());
            ancestor = parent_folder(ancestor);
        }
    }

    /// Fall back to the nearest folder the listing still holds when the open
    /// folder vanished (deleted, renamed, or the project reloaded onto
    /// another tree). Returns whether the open folder changed.
    pub fn settle(&mut self, assets: &[AssetListing]) -> bool {
        if self.folder.is_empty() {
            return false;
        }
        // A root has no row of its own - the panel draws it - so it can never
        // vanish the way a folder can.
        let exists = is_root(&self.folder)
            || assets
                .iter()
                .any(|row| row.kind == "folder" && row.key() == self.folder);
        if exists {
            return false;
        }
        let mut fallback = parent_folder(&self.folder).to_string();
        while !fallback.is_empty() {
            if is_root(&fallback)
                || assets
                    .iter()
                    .any(|row| row.kind == "folder" && row.key() == fallback)
            {
                return self.open(&fallback);
            }
            fallback = parent_folder(&fallback).to_string();
        }
        self.open("")
    }
}

/// Whether a row is a folder: the one kind the tree and the grid treat
/// differently.
pub fn is_folder(row: &AssetListing) -> bool {
    row.kind == "folder"
}

/// The folder a path lives in: its parent, or empty for the asset root.
pub fn parent_folder(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

/// A path's own name: its last `/`-separated segment.
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether `path` is a direct child of `folder` (the empty folder is the
/// asset root).
pub fn is_direct_child(path: &str, folder: &str) -> bool {
    parent_folder(path) == folder
}

/// The direct children of `folder`, folders first and each group in
/// case-insensitive name order - the order both panes list them in.
///
/// A root's row is never a child: the panel draws its roots at its own level,
/// so the project's tree holds the project's content alone.
pub fn children<'a>(assets: &'a [AssetListing], folder: &str) -> Vec<&'a AssetListing> {
    let mut rows: Vec<&AssetListing> = assets
        .iter()
        .filter(|row| {
            let key = row.key();
            !key.is_empty() && !is_root(&key) && is_direct_child(&key, folder)
        })
        .collect();
    sort_rows(&mut rows);
    rows
}

/// The direct child folders of `folder`, in the same order [`children`]
/// lists them.
pub fn child_folders<'a>(assets: &'a [AssetListing], folder: &str) -> Vec<&'a AssetListing> {
    let mut rows: Vec<&AssetListing> = assets
        .iter()
        .filter(|row| {
            let key = row.key();
            is_folder(row) && !is_root(&key) && is_direct_child(&key, folder)
        })
        .collect();
    sort_rows(&mut rows);
    rows
}

/// The breadcrumb of `folder`: the root it belongs to first, then one
/// `(path, label)` per segment, each path cumulative - so a click can open
/// any ancestor.
pub fn breadcrumb(folder: &str) -> Vec<(String, String)> {
    let root = root_of(folder);
    let mut segments = vec![(root.key.to_string(), root.label.to_string())];
    let mut cumulative = root.key.to_string();
    // The root's own segment is already in place; the rest starts after it.
    let rest = if root.key.is_empty() {
        folder
    } else {
        folder
            .strip_prefix(root.key)
            .unwrap_or(folder)
            .trim_start_matches('/')
    };
    for segment in rest.split('/').filter(|segment| !segment.is_empty()) {
        if !cumulative.is_empty() {
            cumulative.push('/');
        }
        cumulative.push_str(segment);
        segments.push((cumulative.clone(), segment.to_string()));
    }
    segments
}

/// How many tiles fit across `available_width` points when each cell takes
/// `cell_width` - at least one, so a narrow pane still draws its rows.
pub fn tile_columns(available_width: f32, cell_width: f32) -> usize {
    // A zero, negative, NaN or infinite cell has no honest column count.
    if !cell_width.is_finite() || cell_width <= 0.0 {
        return 1;
    }
    ((available_width / cell_width).floor() as usize).max(1)
}

/// The folder a drop lands in, relative to the asset root, from the browser's
/// `res/…` folder label.
///
/// `res` itself is the asset root - an empty relative path - and everything
/// under it goes through the same validator every other asset command uses, so
/// a folder the scripted channel climbed out of (`res/../..`) is refused
/// instead of joined onto the root: the copy would land outside the project.
///
/// # Errors
///
/// The validator's message when the folder is not a path inside the asset
/// directory.
pub fn drop_folder_relative(folder: &str) -> Result<PathBuf, String> {
    let sub_folder = match folder {
        "res" => "",
        other => other.strip_prefix("res/").unwrap_or(other),
    };
    if sub_folder.is_empty() {
        return Ok(PathBuf::new());
    }
    pill_engine::asset_metadata::checked_relative(sub_folder)
}

/// Folders before files, then case-insensitive by name.
///
/// The name comes from the row's key, not from its own path: a module root's
/// row carries an empty path, so its name only exists in the key.
fn sort_rows(rows: &mut [&AssetListing]) {
    rows.sort_by_cached_key(|row| (!is_folder(row), file_name(&row.key()).to_lowercase()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, kind: &str) -> AssetListing {
        AssetListing {
            root: String::new(),
            path: path.to_string(),
            read_only: false,
            kind: kind.to_string(),
            standalone: kind == "Material",
            has_meta: false,
            guid: None,
        }
    }

    fn listing() -> Vec<AssetListing> {
        vec![
            row("models", "folder"),
            row("models/cube.obj", "Mesh"),
            row("models/plane.obj", "Mesh"),
            row("models/nested", "folder"),
            row("models/nested/deep.obj", "Mesh"),
            row("shaders", "folder"),
            row("shaders/code.wgsl", "file"),
            row("config.ini", "file"),
            row("textures", "folder"),
            row("textures/skin.png", "Texture"),
            row("textures/stone.material", "Material"),
        ]
    }

    /// A row of an extension's read-only resources, at `path` inside `root`.
    fn module_row(root: &str, path: &str) -> AssetListing {
        AssetListing {
            root: root.to_string(),
            path: path.to_string(),
            read_only: true,
            kind: "folder".to_string(),
            standalone: false,
            has_meta: false,
            guid: None,
        }
    }

    /// An extension's module and one folder inside it, as the host lists them.
    fn with_a_module() -> Vec<AssetListing> {
        let mut assets = listing();
        assets.push(module_row("pill_master_renderer_data", ""));
        assets.push(module_row("pill_master_renderer_data", "defaults"));
        assets
    }

    /// The panel has two roots, and an extension's resources hang under theirs
    /// rather than inside the project's tree.
    #[test]
    fn the_extensions_root_sits_beside_the_project_tree() {
        let assets = with_a_module();

        let top: Vec<String> = children(&assets, "").iter().map(|row| row.key()).collect();
        assert_eq!(top, vec!["models", "shaders", "textures", "config.ini"]);
        assert!(top.iter().all(|key| !is_root(key)));

        let modules: Vec<String> = children(&assets, EXTENSIONS_ROOT)
            .iter()
            .map(|row| row.key())
            .collect();
        assert_eq!(modules, vec!["extensions/pill_master_renderer_data"]);

        let inside: Vec<String> = children(&assets, "extensions/pill_master_renderer_data")
            .iter()
            .map(|row| row.key())
            .collect();
        assert_eq!(
            inside,
            vec!["extensions/pill_master_renderer_data/defaults".to_string()]
        );

        // A browser sitting in a module's tree settles onto the folder it lists.
        let mut browser = ProjectBrowser::default();
        assert!(browser.open("extensions/pill_master_renderer_data/defaults"));
        assert!(!browser.settle(&assets));
    }

    /// A root has no row of its own, so it never settles away - and a folder
    /// whose module unloaded lands back on the root it hung under.
    #[test]
    fn the_extensions_root_outlives_the_rows_below_it() {
        let mut browser = ProjectBrowser::default();
        assert!(browser.open(EXTENSIONS_ROOT));
        assert!(!browser.settle(&listing()));

        assert!(browser.open("extensions/unloaded/defaults"));
        assert!(browser.settle(&listing()));
        assert_eq!(browser.folder, EXTENSIONS_ROOT);
    }

    /// The breadcrumb starts at the root the folder belongs to, and its first
    /// segment opens that root.
    #[test]
    fn the_breadcrumb_names_the_root_the_folder_belongs_to() {
        let segments = breadcrumb("extensions/pill_master_renderer_data/defaults");
        let paths: Vec<&str> = segments.iter().map(|(path, _)| path.as_str()).collect();
        let labels: Vec<&str> = segments.iter().map(|(_, label)| label.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "extensions",
                "extensions/pill_master_renderer_data",
                "extensions/pill_master_renderer_data/defaults",
            ]
        );
        assert_eq!(
            labels,
            vec!["extensions", "pill_master_renderer_data", "defaults"]
        );
        assert_eq!(
            breadcrumb(EXTENSIONS_ROOT),
            vec![(EXTENSIONS_ROOT.to_string(), EXTENSIONS_ROOT.to_string())]
        );
        assert_eq!(root_of("models/nested").key, "");
        assert_eq!(root_of(EXTENSIONS_ROOT).key, EXTENSIONS_ROOT);
    }

    #[test]
    fn children_keep_folders_first_then_names() {
        let assets = listing();
        let names: Vec<&str> = children(&assets, "")
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert_eq!(names, vec!["models", "shaders", "textures", "config.ini"]);
        let model_names: Vec<&str> = children(&assets, "models")
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert_eq!(
            model_names,
            vec!["models/nested", "models/cube.obj", "models/plane.obj"]
        );
    }

    #[test]
    fn child_folders_are_the_tree_rows() {
        let assets = listing();
        let folders: Vec<&str> = child_folders(&assets, "models")
            .iter()
            .map(|row| row.path.as_str())
            .collect();
        assert_eq!(folders, vec!["models/nested"]);
        assert_eq!(child_folders(&assets, "").len(), 3);
        assert!(child_folders(&assets, "shaders").is_empty());
    }

    #[test]
    fn breadcrumb_starts_at_the_root_and_accumulates() {
        let segments = breadcrumb("models/nested");
        let paths: Vec<&str> = segments.iter().map(|(path, _)| path.as_str()).collect();
        let labels: Vec<&str> = segments.iter().map(|(_, label)| label.as_str()).collect();
        assert_eq!(paths, vec!["", "models", "models/nested"]);
        assert_eq!(labels, vec!["Assets", "models", "nested"]);
        assert_eq!(breadcrumb("").len(), 1);
    }

    #[test]
    fn parents_and_names_come_off_the_slashes() {
        assert_eq!(parent_folder("models/cube.obj"), "models");
        assert_eq!(parent_folder("config.ini"), "");
        assert_eq!(file_name("models/nested/deep.obj"), "deep.obj");
        assert_eq!(file_name("models"), "models");
        assert!(is_direct_child("models/cube.obj", "models"));
        assert!(!is_direct_child("models/nested/deep.obj", "models"));
    }

    #[test]
    fn opening_a_folder_expands_its_ancestors() {
        let mut browser = ProjectBrowser::default();
        assert!(browser.open("models/nested"));
        assert!(browser.is_expanded("models"));
        assert!(!browser.is_expanded("models/nested"));
        assert_eq!(browser.folder, "models/nested");
        // Opening the same folder again moves nothing.
        assert!(!browser.open("/models/nested/"));
    }

    #[test]
    fn a_vanished_folder_falls_back_to_its_nearest_ancestor() {
        let mut browser = ProjectBrowser::default();
        browser.open("models/nested");
        let assets = vec![
            row("models", "folder"),
            row("models/cube.obj", "Mesh"),
            row("config.ini", "file"),
        ];
        assert!(browser.settle(&assets));
        assert_eq!(browser.folder, "models");
        // A tree without the folder at all lands on the root.
        let mut browser = ProjectBrowser::default();
        browser.open("gone/deeper");
        assert!(browser.settle(&assets));
        assert_eq!(browser.folder, "");
        // The root never settles away.
        assert!(!browser.settle(&assets));
    }

    #[test]
    fn toggling_a_tree_row_flips_its_state() {
        let mut browser = ProjectBrowser::default();
        assert!(browser.toggle_expanded("models"));
        assert!(browser.is_expanded("models"));
        assert!(!browser.toggle_expanded("models"));
        assert!(!browser.is_expanded("models"));
    }

    #[test]
    fn tile_columns_fit_the_pane_and_never_reach_zero() {
        assert_eq!(tile_columns(400.0, 80.0), 5);
        assert_eq!(tile_columns(79.0, 80.0), 1);
        assert_eq!(tile_columns(0.0, 80.0), 1);
        assert_eq!(tile_columns(400.0, 0.0), 1);
    }

    #[test]
    fn a_drop_in_the_root_folder_is_the_asset_root() {
        assert_eq!(drop_folder_relative("res"), Ok(PathBuf::new()));
    }

    #[test]
    fn a_drop_in_a_named_folder_stays_under_the_root() {
        assert_eq!(
            drop_folder_relative("res/models/nested"),
            Ok(PathBuf::from("models/nested"))
        );
    }

    #[test]
    fn a_drop_folder_that_climbs_out_is_refused() {
        for folder in ["res/../..", "res/models/../../..", "../..", "/res/models"] {
            assert!(
                drop_folder_relative(folder).is_err(),
                "`{folder}` was accepted as a drop folder"
            );
        }
    }
}

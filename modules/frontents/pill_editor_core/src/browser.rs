//! The Assets tab's project browser: the folder the grid lists, the tree's
//! open folders, and the listing math both panes share.
//!
//! # Responsibilities
//!
//! - [`ProjectBrowser`]: the browser's session state - the folder the grid
//!   lists, the folders the tree shows open, and the tile size the grid
//!   draws.
//! - The listing math both panes read: [`children`], [`child_folders`],
//!   [`breadcrumb`], [`parent_folder`], [`file_name`] and [`tile_columns`].
//!
//! # Design
//!
//! The snapshot's asset listing is one flat, project-relative list of files
//! and folders ([`AssetListing`]), so both panes derive what they show from
//! that list: the tree walks its folder rows, the grid keeps the open
//! folder's direct children, and the breadcrumb cuts the open folder's path.
//! Nothing here touches egui or the disk, which is what lets the tree, the
//! grid, the breadcrumb and the scripted channel agree on one state.

// Standard library
use std::collections::BTreeSet;
use std::ops::RangeInclusive;

// External crates
use pill_core::telemetry::telemetry_target;

// Current crate
use crate::editor_state::AssetListing;

/// The label the root row and the breadcrumb start with.
pub const ROOT_LABEL: &str = "Assets";

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
}

impl Default for ProjectBrowser {
    fn default() -> Self {
        Self {
            folder: String::new(),
            expanded: BTreeSet::new(),
            tile_size: DEFAULT_TILE_SIZE,
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
        let exists = assets
            .iter()
            .any(|row| row.kind == "folder" && row.path == self.folder);
        if exists {
            return false;
        }
        let mut fallback = parent_folder(&self.folder).to_string();
        while !fallback.is_empty() {
            if assets
                .iter()
                .any(|row| row.kind == "folder" && row.path == fallback)
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
pub fn children<'a>(assets: &'a [AssetListing], folder: &str) -> Vec<&'a AssetListing> {
    let mut rows: Vec<&AssetListing> = assets
        .iter()
        .filter(|row| !row.path.is_empty() && is_direct_child(&row.path, folder))
        .collect();
    sort_rows(&mut rows);
    rows
}

/// The direct child folders of `folder`, in the same order [`children`]
/// lists them.
pub fn child_folders<'a>(assets: &'a [AssetListing], folder: &str) -> Vec<&'a AssetListing> {
    let mut rows: Vec<&AssetListing> = assets
        .iter()
        .filter(|row| is_folder(row) && is_direct_child(&row.path, folder))
        .collect();
    sort_rows(&mut rows);
    rows
}

/// The breadcrumb of `folder`: the root first, then one `(path, label)` per
/// segment, each path cumulative - so a click can open any ancestor.
pub fn breadcrumb(folder: &str) -> Vec<(String, String)> {
    let mut segments = vec![(String::new(), ROOT_LABEL.to_string())];
    let mut cumulative = String::new();
    for segment in folder.split('/').filter(|segment| !segment.is_empty()) {
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

/// Folders before files, then case-insensitive by name.
fn sort_rows(rows: &mut [&AssetListing]) {
    rows.sort_by(|left, right| {
        is_folder(right).cmp(&is_folder(left)).then_with(|| {
            file_name(&left.path)
                .to_lowercase()
                .cmp(&file_name(&right.path).to_lowercase())
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, kind: &str) -> AssetListing {
        AssetListing {
            path: path.to_string(),
            kind: kind.to_string(),
            standalone: kind == "Material",
            has_meta: false,
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
}

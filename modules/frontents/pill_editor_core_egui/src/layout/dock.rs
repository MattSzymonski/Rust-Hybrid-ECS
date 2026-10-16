//! The editor's tabs and the default dock layout.
//!
//! # Responsibilities
//!
//! - [`EditorTab`]: the MVP's two tabs - the world's entities and systems -
//!   and their titles.
//! - [`EditorDock`]: the `egui_dock` state with the default layout, the
//!   schema version its documents carry, and [`EditorDock::settled`] which
//!   replaces the placeholder rectangles an unpacked layout has not
//!   computed yet.
//!
//! # Design
//!
//! The dock state is the shell's long-lived state, so it lives in the shared
//! core crate rather than the reloadable shell module: a reload of the shell
//! finds the layout exactly where the user left it, and persistence carries
//! it across restarts.

// External crates
use pill_ui_egui::egui::Rect;
use pill_ui_egui::egui_dock::{DockState, Node};
use serde::{Deserialize, Serialize};

/// One tab of the egui editor.
///
/// The MVP's two tabs (plan decisions E2, Stage 7). The enum is serialized
/// into the saved layout, so adding a tab is a schema change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EditorTab {
    /// The world's entities: id, name, click to select.
    Entities,
    /// The world's systems: name, stage, enabled.
    Systems,
}

impl EditorTab {
    /// Every tab, in tab-strip order.
    pub const ALL: [EditorTab; 2] = [EditorTab::Entities, EditorTab::Systems];

    /// The tab's display title.
    pub fn title(self) -> &'static str {
        match self {
            EditorTab::Entities => "Entities",
            EditorTab::Systems => "Systems",
        }
    }
}

/// The editor's dock: an `egui_dock` state with the MVP's default layout.
///
/// The shell keeps one of these in its session resource, so a reload of the
/// shell module finds the dock exactly where the user left it; persistence
/// carries it across restarts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditorDock {
    /// The schema this document was written with.
    schema_version: u32,
    /// The dock's tabs and splits.
    state: DockState<EditorTab>,
}

impl Default for EditorDock {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorDock {
    /// A dock with the default layout: one tab set holding both tabs.
    pub fn new() -> Self {
        Self {
            schema_version: super::LAYOUT_SCHEMA_VERSION,
            state: DockState::new(EditorTab::ALL.to_vec()),
        }
    }

    /// The dock state, for the shell's `DockArea`.
    pub fn state(&self) -> &DockState<EditorTab> {
        &self.state
    }

    /// The dock state, mutably: the shell runs the tabs through this, and
    /// `egui_dock` applies drags and closes to it directly.
    pub fn state_mut(&mut self) -> &mut DockState<EditorTab> {
        &mut self.state
    }

    /// The schema version the document carries.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Whether `tab` is docked somewhere: the main surface or a window.
    pub fn contains(&self, tab: EditorTab) -> bool {
        // `DockState::find_tab` reports the surface and node holding the tab.
        self.state.find_tab(&tab).is_some()
    }

    /// Whether the document was written by this build's schema.
    pub(crate) fn is_current(&self) -> bool {
        self.schema_version == super::LAYOUT_SCHEMA_VERSION
    }

    /// A copy with placeholder geometry replaced by finite values.
    ///
    /// A fresh dock's nodes start with `Rect::NOTHING` (`±INF`) until the
    /// first `DockArea::show` lays them out; serde_json writes non-finite
    /// floats as `null`, which cannot be read back. The values are
    /// recomputed on the next show, so a zero placeholder is safe.
    pub(crate) fn settled(&self) -> Self {
        let mut dock = self.clone();
        for (_surface, node) in dock.state.iter_all_nodes_mut() {
            match node {
                Node::Leaf(leaf) => {
                    leaf.rect = settle_rect(leaf.rect);
                    leaf.viewport = settle_rect(leaf.viewport);
                }
                Node::Vertical(split) | Node::Horizontal(split) => {
                    split.rect = settle_rect(split.rect);
                }
                // An empty node holds no geometry to settle.
                Node::Empty => {}
            }
        }
        dock
    }
}

/// `rect`, or a finite zero rect when it still holds placeholder infinities.
fn settle_rect(rect: Rect) -> Rect {
    let finite = |value: f32| value.is_finite();
    if finite(rect.min.x) && finite(rect.min.y) && finite(rect.max.x) && finite(rect.max.y) {
        rect
    } else {
        Rect::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_layout_holds_both_tabs() {
        let dock = EditorDock::new();
        assert!(dock.contains(EditorTab::Entities));
        assert!(dock.contains(EditorTab::Systems));
    }

    #[test]
    fn a_layout_round_trips_through_serde() {
        let dock = EditorDock::new();
        // The save path settles the placeholder rects first; a fresh dock is
        // the worst case and must round-trip.
        let json = serde_json::to_string(&dock.settled()).expect("serialize the dock");
        let restored: EditorDock = serde_json::from_str(&json).expect("deserialize the dock");
        assert!(restored.contains(EditorTab::Entities));
        assert!(restored.contains(EditorTab::Systems));
        assert_eq!(restored.schema_version(), dock.schema_version());
    }
}

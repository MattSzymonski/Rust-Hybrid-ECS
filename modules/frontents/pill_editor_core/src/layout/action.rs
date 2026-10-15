//! The layout action vocabulary: what the dock accepts and where a tab lands.
//!
//! # Responsibilities
//!
//! - Define the actions a user interaction is reduced to.
//! - Keep them serializable: actions cross the drag/drop and persistence paths.

use serde::{Deserialize, Serialize};

use super::{NodeId, PanelKind};

/// Region of a tabset at which a dragged tab will be inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DockLocation {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

/// Complete set of durable layout mutations.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutAction {
    SelectTab {
        tab: NodeId,
    },
    MoveTab {
        tab: NodeId,
        target_tabset: NodeId,
        insertion_index: usize,
    },
    DockTab {
        tab: NodeId,
        target_tabset: NodeId,
        location: DockLocation,
        ratio: f32,
    },
    ResizeSplit {
        row: NodeId,
        splitter_index: usize,
        first_weight: f32,
        second_weight: f32,
    },
    CloseTab {
        tab: NodeId,
    },
    DetachTab {
        tab: NodeId,
    },
    /// Move a tab out of the dock into a free-standing window (Stage 29);
    /// the tab node stays alive and a [`LayoutAction::RedockTab`] brings it
    /// back. Refused when it would leave the dock empty.
    PopOutTab {
        tab: NodeId,
    },
    /// Return a floated tab to the dock, selected, in `target_tabset`.
    RedockTab {
        tab: NodeId,
        target_tabset: NodeId,
    },
    /// Record a floating window's size and last known position (Stage 29);
    /// the platform is the only one that knows a position, and it may be
    /// `None` where it does not.
    SetFloatingBounds {
        tab: NodeId,
        width: f32,
        height: f32,
        position: Option<(f32, f32)>,
    },
    OpenTab {
        panel: PanelKind,
        target_tabset: NodeId,
    },
    ResetLayout,
}

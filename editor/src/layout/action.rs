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
    OpenTab {
        panel: PanelKind,
        target_tabset: NodeId,
    },
    ResetLayout,
}

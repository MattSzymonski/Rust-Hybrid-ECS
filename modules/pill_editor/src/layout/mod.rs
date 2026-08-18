//! Serializable dock model, validated mutations, and deterministic geometry.

mod action;
mod geometry;
mod id;
mod model;
mod persistence;
mod reducer;

pub use action::{DockLocation, LayoutAction};
pub use geometry::{compute_layout, DropTarget, LayoutMetrics, LayoutSnapshot, Rect};
pub use id::NodeId;
pub use model::{Axis, LayoutModel, LayoutNode, PanelKind};
pub use persistence::{load_or_default, save};
pub use reducer::{LayoutChange, LayoutError};

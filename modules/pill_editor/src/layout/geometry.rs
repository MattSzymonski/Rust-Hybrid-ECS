use std::collections::BTreeMap;

use super::model::LayoutNode;
use super::{Axis, DockLocation, LayoutModel, NodeId, PanelKind};

/// Logical-pixel rectangle used by both Dioxus and renderer bridging.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x <= self.x + self.width && y <= self.y + self.height
    }
}

/// Central geometry values shared with the dock theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutMetrics {
    pub tab_strip_height: f64,
    pub splitter_thickness: f64,
    pub minimum_pane_width: f64,
    pub minimum_pane_height: f64,
    pub drop_edge_fraction: f64,
    pub minimum_drop_zone: f64,
}

impl Default for LayoutMetrics {
    fn default() -> Self {
        Self {
            tab_strip_height: 30.0,
            splitter_thickness: 5.0,
            minimum_pane_width: 120.0,
            minimum_pane_height: 80.0,
            drop_edge_fraction: 0.25,
            minimum_drop_zone: 32.0,
        }
    }
}

/// One draggable divider between adjacent row children.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Splitter {
    pub row: NodeId,
    pub index: usize,
    pub axis: Axis,
    pub rect: Rect,
}

/// Tab insertion button geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabButtonRect {
    pub tabset: NodeId,
    pub tab: NodeId,
    pub index: usize,
    pub rect: Rect,
}

/// One valid center or edge docking zone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DropTarget {
    pub tabset: NodeId,
    pub location: DockLocation,
    pub rect: Rect,
    pub preview: Rect,
}

/// Immutable result of one complete layout calculation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayoutSnapshot {
    pub node_rects: BTreeMap<NodeId, Rect>,
    pub tab_strip_rects: BTreeMap<NodeId, Rect>,
    pub content_rects: BTreeMap<NodeId, Rect>,
    pub selected_panel_rects: BTreeMap<NodeId, Rect>,
    pub tab_buttons: Vec<TabButtonRect>,
    pub splitters: Vec<Splitter>,
    pub drop_targets: Vec<DropTarget>,
    pub scene_rect: Option<Rect>,
}

impl LayoutSnapshot {
    /// Resolve the highest-priority drop target under a logical pointer.
    pub fn drop_target_at(&self, x: f64, y: f64) -> Option<DropTarget> {
        self.drop_targets
            .iter()
            .find(|target| target.rect.contains(x, y))
            .copied()
    }
}

/// Calculate every rectangle without consulting the DOM.
pub fn compute_layout(
    model: &LayoutModel,
    available: Rect,
    metrics: LayoutMetrics,
) -> LayoutSnapshot {
    let mut snapshot = LayoutSnapshot::default();
    layout_node(model, model.root, available, metrics, &mut snapshot);
    snapshot
}

fn layout_node(
    model: &LayoutModel,
    id: NodeId,
    rect: Rect,
    metrics: LayoutMetrics,
    snapshot: &mut LayoutSnapshot,
) {
    snapshot.node_rects.insert(id, rect);
    match model.node(id) {
        Some(LayoutNode::Row(row)) => {
            let child_count = row.children.len();
            if child_count == 0 {
                return;
            }
            let main_size = match row.axis {
                Axis::Horizontal => rect.width,
                Axis::Vertical => rect.height,
            };
            let splitter_total = metrics.splitter_thickness * child_count.saturating_sub(1) as f64;
            let distributable = (main_size - splitter_total).max(0.0);
            let total_weight: f64 = row.children.iter().map(|child| child.weight as f64).sum();
            let mut cursor = match row.axis {
                Axis::Horizontal => rect.x,
                Axis::Vertical => rect.y,
            };
            let end = cursor + main_size;

            for (index, child) in row.children.iter().enumerate() {
                let remaining_splitters =
                    child_count.saturating_sub(index + 1) as f64 * metrics.splitter_thickness;
                let child_size = if index + 1 == child_count {
                    (end - cursor - remaining_splitters).max(0.0)
                } else {
                    distributable * child.weight as f64 / total_weight.max(f64::EPSILON)
                };
                let child_rect = match row.axis {
                    Axis::Horizontal => Rect::new(cursor, rect.y, child_size, rect.height),
                    Axis::Vertical => Rect::new(rect.x, cursor, rect.width, child_size),
                };
                layout_node(model, child.node, child_rect, metrics, snapshot);
                cursor += child_size;

                if index + 1 < child_count {
                    let splitter_rect = match row.axis {
                        Axis::Horizontal => {
                            Rect::new(cursor, rect.y, metrics.splitter_thickness, rect.height)
                        }
                        Axis::Vertical => {
                            Rect::new(rect.x, cursor, rect.width, metrics.splitter_thickness)
                        }
                    };
                    snapshot.splitters.push(Splitter {
                        row: id,
                        index,
                        axis: row.axis,
                        rect: splitter_rect,
                    });
                    cursor += metrics.splitter_thickness;
                }
            }
        }
        Some(LayoutNode::TabSet(tabset)) => {
            let strip_height = metrics.tab_strip_height.min(rect.height);
            let strip = Rect::new(rect.x, rect.y, rect.width, strip_height);
            let content = Rect::new(
                rect.x,
                rect.y + strip_height,
                rect.width,
                (rect.height - strip_height).max(0.0),
            );
            snapshot.tab_strip_rects.insert(id, strip);
            snapshot.content_rects.insert(id, content);

            let mut tab_x = strip.x;
            for (index, tab_id) in tabset.tabs.iter().enumerate() {
                let title_width = model
                    .tab(*tab_id)
                    .map(|tab| tab.title.chars().count() as f64 * 7.5 + 34.0)
                    .unwrap_or(90.0)
                    .clamp(72.0, 180.0);
                let button_width = title_width.min((strip.x + strip.width - tab_x).max(0.0));
                snapshot.tab_buttons.push(TabButtonRect {
                    tabset: id,
                    tab: *tab_id,
                    index,
                    rect: Rect::new(tab_x, strip.y, button_width, strip.height),
                });
                tab_x += button_width;
            }

            if let Some(selected) = tabset.selected {
                snapshot.selected_panel_rects.insert(selected, content);
                if model
                    .tab(selected)
                    .is_some_and(|tab| tab.panel == PanelKind::Scene)
                {
                    snapshot.scene_rect = Some(content);
                }
            }
            add_drop_targets(snapshot, id, content, metrics);
        }
        _ => {}
    }
}

fn add_drop_targets(
    snapshot: &mut LayoutSnapshot,
    tabset: NodeId,
    content: Rect,
    metrics: LayoutMetrics,
) {
    let edge_width = (content.width * metrics.drop_edge_fraction)
        .max(metrics.minimum_drop_zone)
        .min(content.width / 2.0);
    let edge_height = (content.height * metrics.drop_edge_fraction)
        .max(metrics.minimum_drop_zone)
        .min(content.height / 2.0);
    let center = Rect::new(
        content.x + edge_width,
        content.y + edge_height,
        (content.width - edge_width * 2.0).max(0.0),
        (content.height - edge_height * 2.0).max(0.0),
    );

    // Center precedes edges to make the overlap priority explicit.
    snapshot.drop_targets.push(DropTarget {
        tabset,
        location: DockLocation::Center,
        rect: center,
        preview: content,
    });
    snapshot.drop_targets.extend([
        DropTarget {
            tabset,
            location: DockLocation::Left,
            rect: Rect::new(content.x, content.y, edge_width, content.height),
            preview: Rect::new(content.x, content.y, content.width * 0.5, content.height),
        },
        DropTarget {
            tabset,
            location: DockLocation::Right,
            rect: Rect::new(
                content.x + content.width - edge_width,
                content.y,
                edge_width,
                content.height,
            ),
            preview: Rect::new(
                content.x + content.width * 0.5,
                content.y,
                content.width * 0.5,
                content.height,
            ),
        },
        DropTarget {
            tabset,
            location: DockLocation::Top,
            rect: Rect::new(content.x, content.y, content.width, edge_height),
            preview: Rect::new(content.x, content.y, content.width, content.height * 0.5),
        },
        DropTarget {
            tabset,
            location: DockLocation::Bottom,
            rect: Rect::new(
                content.x,
                content.y + content.height - edge_height,
                content.width,
                edge_height,
            ),
            preview: Rect::new(
                content.x,
                content.y + content.height * 0.5,
                content.width,
                content.height * 0.5,
            ),
        },
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_fills_root_and_has_scene_viewport() {
        let model = LayoutModel::default_editor();
        let root = Rect::new(0.0, 0.0, 1280.0, 800.0);
        let snapshot = compute_layout(&model, root, LayoutMetrics::default());
        assert_eq!(snapshot.node_rects[&model.root], root);
        let scene = snapshot.scene_rect.unwrap();
        assert!(scene.width > 500.0);
        assert!(scene.height > 500.0);
    }

    #[test]
    fn every_splitter_stays_inside_its_row() {
        let model = LayoutModel::default_editor();
        let snapshot = compute_layout(
            &model,
            Rect::new(0.0, 0.0, 1280.0, 800.0),
            LayoutMetrics::default(),
        );
        for splitter in &snapshot.splitters {
            let row = snapshot.node_rects[&splitter.row];
            assert!(row.contains(splitter.rect.x, splitter.rect.y));
            assert!(splitter.rect.x + splitter.rect.width <= row.x + row.width + 0.001);
            assert!(splitter.rect.y + splitter.rect.height <= row.y + row.height + 0.001);
        }
    }

    #[test]
    fn drop_target_resolves_center_before_edges() {
        let model = LayoutModel::default_editor();
        let snapshot = compute_layout(
            &model,
            Rect::new(0.0, 0.0, 1280.0, 800.0),
            LayoutMetrics::default(),
        );
        let scene = snapshot.scene_rect.unwrap();
        let target = snapshot
            .drop_target_at(scene.x + scene.width / 2.0, scene.y + scene.height / 2.0)
            .unwrap();
        assert_eq!(target.location, DockLocation::Center);
    }
}

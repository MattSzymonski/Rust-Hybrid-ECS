//! Applies one layout action to the model and reports what changed.
//!
//! # Responsibilities
//!
//! - Commit a validated mutation, or refuse it with a typed error.
//! - Report the `LayoutChange` a committed action produced.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use super::model::{
    FloatingTab, LayoutNode, RowNode, TabSetNode, WeightedChild, FLOATING_TAB_DEFAULT_SIZE,
};
use super::{Axis, DockLocation, LayoutAction, LayoutModel, NodeId, PanelKind};

/// Summary returned after one committed mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutChange {
    pub structure_changed: bool,
}

/// Validation or mutation failure that leaves the original model untouched.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutError {
    UnsupportedVersion(u32),
    MissingNode(NodeId),
    WrongNodeKind(NodeId),
    InvalidChild(NodeId),
    InvalidSelection(NodeId),
    InvalidWeight(NodeId),
    EmptyRow(NodeId),
    EmptyTabSet(NodeId),
    Cycle(NodeId),
    MultipleParents(NodeId),
    UnreachableNode(NodeId),
    InvalidNextId,
    InvalidActiveTabSet(NodeId),
    DuplicatePanel(PanelKind),
    NotCloseable(NodeId),
    WouldEmptyLayout,
    InvalidSplitter(NodeId),
}

impl Display for LayoutError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid dock layout: {self:?}")
    }
}

impl std::error::Error for LayoutError {}

/// Apply an action transactionally and validate the complete result.
pub(crate) fn reduce(
    model: &mut LayoutModel,
    action: LayoutAction,
) -> Result<LayoutChange, LayoutError> {
    if matches!(action, LayoutAction::ResetLayout) {
        *model = LayoutModel::default_editor();
        return Ok(LayoutChange {
            structure_changed: true,
        });
    }

    let mut candidate = model.clone();
    let structure_changed = apply_in_place(&mut candidate, action)?;
    normalize(&mut candidate)?;
    candidate.validate()?;
    *model = candidate;
    Ok(LayoutChange { structure_changed })
}

fn apply_in_place(model: &mut LayoutModel, action: LayoutAction) -> Result<bool, LayoutError> {
    match action {
        LayoutAction::SelectTab { tab } => {
            let tabset_id = model.parent_of(tab).ok_or(LayoutError::MissingNode(tab))?;
            let tabset = tabset_mut(model, tabset_id)?;
            if !tabset.tabs.contains(&tab) {
                return Err(LayoutError::InvalidChild(tabset_id));
            }
            tabset.selected = Some(tab);
            model.active_tabset = Some(tabset_id);
            Ok(false)
        }
        LayoutAction::MoveTab {
            tab,
            target_tabset,
            insertion_index,
        } => {
            move_tab(model, tab, target_tabset, insertion_index)?;
            Ok(true)
        }
        LayoutAction::DockTab {
            tab,
            target_tabset,
            location,
            ratio,
        } => {
            if location == DockLocation::Center {
                let index = model
                    .tabset(target_tabset)
                    .ok_or(LayoutError::WrongNodeKind(target_tabset))?
                    .tabs
                    .len();
                move_tab(model, tab, target_tabset, index)?;
            } else {
                dock_tab_at_edge(model, tab, target_tabset, location, ratio)?;
            }
            Ok(true)
        }
        LayoutAction::ResizeSplit {
            row,
            splitter_index,
            first_weight,
            second_weight,
        } => {
            if !first_weight.is_finite()
                || !second_weight.is_finite()
                || first_weight <= 0.0
                || second_weight <= 0.0
            {
                return Err(LayoutError::InvalidWeight(row));
            }
            let row_node = row_mut(model, row)?;
            if splitter_index + 1 >= row_node.children.len() {
                return Err(LayoutError::InvalidSplitter(row));
            }
            row_node.children[splitter_index].weight = first_weight;
            row_node.children[splitter_index + 1].weight = second_weight;
            Ok(false)
        }
        LayoutAction::CloseTab { tab } => {
            let tab_node = model.tab(tab).ok_or(LayoutError::WrongNodeKind(tab))?;
            if !tab_node.closeable {
                return Err(LayoutError::NotCloseable(tab));
            }
            // A floating tab has no dock parent (Stage 29); closing it is
            // dropping its window and its node together.
            if let Some(index) = model.floating.iter().position(|entry| entry.tab == tab) {
                model.floating.remove(index);
                model.nodes.remove(&tab);
                return Ok(true);
            }
            remove_tab(model, tab)?;
            model.nodes.remove(&tab);
            Ok(true)
        }
        LayoutAction::DetachTab { tab } => {
            if model
                .nodes
                .values()
                .filter(|node| matches!(node, LayoutNode::Tab(_)))
                .count()
                <= 1
            {
                return Err(LayoutError::WouldEmptyLayout);
            }
            if model.tab(tab).is_none() {
                return Err(LayoutError::WrongNodeKind(tab));
            }
            remove_tab(model, tab)?;
            model.nodes.remove(&tab);
            Ok(true)
        }
        LayoutAction::PopOutTab { tab } => {
            if model.tab(tab).is_none() {
                return Err(LayoutError::WrongNodeKind(tab));
            }
            if model.floating.iter().any(|entry| entry.tab == tab) {
                return Err(LayoutError::InvalidChild(tab));
            }
            // The dock must keep at least one tab; a pop-out that would
            // leave it empty is refused outright, like a detach.
            let docked = model
                .nodes
                .values()
                .filter(|node| matches!(node, LayoutNode::Tab(_)))
                .count();
            if docked <= model.floating.len() + 1 {
                return Err(LayoutError::WouldEmptyLayout);
            }
            remove_tab(model, tab)?;
            let (width, height) = FLOATING_TAB_DEFAULT_SIZE;
            model.floating.push(FloatingTab {
                tab,
                width,
                height,
                position: None,
            });
            Ok(true)
        }
        LayoutAction::RedockTab { tab, target_tabset } => {
            if model.tab(tab).is_none() {
                return Err(LayoutError::WrongNodeKind(tab));
            }
            if model.tabset(target_tabset).is_none() {
                return Err(LayoutError::WrongNodeKind(target_tabset));
            }
            let Some(index) = model.floating.iter().position(|entry| entry.tab == tab) else {
                return Err(LayoutError::InvalidChild(tab));
            };
            model.floating.remove(index);
            let tabset = tabset_mut(model, target_tabset)?;
            tabset.tabs.push(tab);
            tabset.selected = Some(tab);
            model.active_tabset = Some(target_tabset);
            Ok(true)
        }
        LayoutAction::SetFloatingBounds {
            tab,
            width,
            height,
            position,
        } => {
            let Some(entry) = model.floating.iter_mut().find(|entry| entry.tab == tab) else {
                return Err(LayoutError::InvalidChild(tab));
            };
            entry.width = width;
            entry.height = height;
            entry.position = position;
            Ok(false)
        }
        LayoutAction::OpenTab {
            panel,
            target_tabset,
        } => {
            // The Scene and Game views may repeat (Stage 29): several scene
            // views, each its own surface and camera, are the stage's
            // point. Every other panel is one instance.
            let repeatable = matches!(panel, PanelKind::Scene | PanelKind::Game);
            let existing = model
                .nodes
                .values()
                .any(|node| matches!(node, LayoutNode::Tab(tab) if tab.panel == panel));
            if existing && !repeatable {
                return Err(LayoutError::DuplicatePanel(panel));
            }
            if model.tabset(target_tabset).is_none() {
                return Err(LayoutError::WrongNodeKind(target_tabset));
            }
            // The workspace's own Scene tab is its permanent view; every
            // further view closes like an ordinary tab.
            let closeable = !matches!(panel, PanelKind::Scene) || existing;
            let tab = model.add_tab(panel, closeable);
            let tabset = tabset_mut(model, target_tabset)?;
            tabset.tabs.push(tab);
            tabset.selected = Some(tab);
            model.active_tabset = Some(target_tabset);
            Ok(true)
        }
        LayoutAction::ResetLayout => unreachable!(),
    }
}

fn move_tab(
    model: &mut LayoutModel,
    tab: NodeId,
    target: NodeId,
    insertion_index: usize,
) -> Result<(), LayoutError> {
    if model.tab(tab).is_none() {
        return Err(LayoutError::WrongNodeKind(tab));
    }
    if model.tabset(target).is_none() {
        return Err(LayoutError::WrongNodeKind(target));
    }
    remove_tab(model, tab)?;
    {
        let target_tabset = tabset_mut(model, target)?;
        let index = insertion_index.min(target_tabset.tabs.len());
        target_tabset.tabs.insert(index, tab);
        target_tabset.selected = Some(tab);
    }
    model.active_tabset = Some(target);
    Ok(())
}

fn remove_tab(model: &mut LayoutModel, tab: NodeId) -> Result<NodeId, LayoutError> {
    let source = model.parent_of(tab).ok_or(LayoutError::MissingNode(tab))?;
    let tabset = tabset_mut(model, source)?;
    let index = tabset
        .tabs
        .iter()
        .position(|candidate| *candidate == tab)
        .ok_or(LayoutError::InvalidChild(source))?;
    tabset.tabs.remove(index);
    if tabset.selected == Some(tab) {
        tabset.selected = tabset
            .tabs
            .get(index)
            .or_else(|| {
                index
                    .checked_sub(1)
                    .and_then(|index| tabset.tabs.get(index))
            })
            .copied();
    }
    Ok(source)
}

fn dock_tab_at_edge(
    model: &mut LayoutModel,
    tab: NodeId,
    target: NodeId,
    location: DockLocation,
    ratio: f32,
) -> Result<(), LayoutError> {
    if model.tabset(target).is_none() || model.tab(tab).is_none() {
        return Err(LayoutError::WrongNodeKind(target));
    }
    let source = remove_tab(model, tab)?;
    if source == target && model.tabset(target).is_some_and(|set| set.tabs.is_empty()) {
        return Err(LayoutError::WouldEmptyLayout);
    }

    let new_tabset = model.add_tabset(vec![tab], tab);
    let desired_axis = match location {
        DockLocation::Left | DockLocation::Right => Axis::Horizontal,
        DockLocation::Top | DockLocation::Bottom => Axis::Vertical,
        DockLocation::Center => unreachable!(),
    };
    let before = matches!(location, DockLocation::Left | DockLocation::Top);
    let ratio = ratio.clamp(0.1, 0.9);

    if let Some(parent) = model.parent_of(target) {
        let same_axis = model
            .row(parent)
            .is_some_and(|row| row.axis == desired_axis);
        if same_axis {
            let row = row_mut(model, parent)?;
            let index = row
                .children
                .iter()
                .position(|child| child.node == target)
                .ok_or(LayoutError::InvalidChild(parent))?;
            let original = row.children[index].weight;
            let new_weight = original * ratio;
            row.children[index].weight = original - new_weight;
            let insert_at = if before { index } else { index + 1 };
            row.children.insert(
                insert_at,
                WeightedChild {
                    node: new_tabset,
                    weight: new_weight,
                },
            );
        } else {
            let children = if before {
                vec![(new_tabset, ratio), (target, 1.0 - ratio)]
            } else {
                vec![(target, 1.0 - ratio), (new_tabset, ratio)]
            };
            let nested = model.add_row(desired_axis, children);
            let parent_row = row_mut(model, parent)?;
            let child = parent_row
                .children
                .iter_mut()
                .find(|child| child.node == target)
                .ok_or(LayoutError::InvalidChild(parent))?;
            child.node = nested;
        }
    } else if model.root == target {
        let children = if before {
            vec![(new_tabset, ratio), (target, 1.0 - ratio)]
        } else {
            vec![(target, 1.0 - ratio), (new_tabset, ratio)]
        };
        model.root = model.add_row(desired_axis, children);
    } else {
        return Err(LayoutError::MissingNode(target));
    }

    model.active_tabset = Some(new_tabset);
    Ok(())
}

fn tabset_mut(model: &mut LayoutModel, id: NodeId) -> Result<&mut TabSetNode, LayoutError> {
    match model.nodes.get_mut(&id) {
        Some(LayoutNode::TabSet(tabset)) => Ok(tabset),
        _ => Err(LayoutError::WrongNodeKind(id)),
    }
}

fn row_mut(model: &mut LayoutModel, id: NodeId) -> Result<&mut RowNode, LayoutError> {
    match model.nodes.get_mut(&id) {
        Some(LayoutNode::Row(row)) => Ok(row),
        _ => Err(LayoutError::WrongNodeKind(id)),
    }
}

fn normalize(model: &mut LayoutModel) -> Result<(), LayoutError> {
    let root = normalize_node(model, model.root).ok_or(LayoutError::WouldEmptyLayout)?;
    model.root = root;

    let mut reachable = BTreeSet::new();
    collect_reachable(model, root, &mut reachable);
    // A floating tab's node is alive outside the dock tree (Stage 29): an
    // entry whose node is gone is dropped, and every live one keeps its
    // node.
    let mut floating = std::mem::take(&mut model.floating);
    floating.retain(|entry| {
        let alive = model.tab(entry.tab).is_some();
        if alive {
            reachable.insert(entry.tab);
        }
        alive
    });
    model.floating = floating;
    model.nodes.retain(|id, _| reachable.contains(id));
    model.next_id = model
        .nodes
        .keys()
        .map(|id| id.0)
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    // The fallback rule lives on the model, so this normalisation, the view
    // and the shell cannot disagree about which tabset is active.
    model.active_tabset = model.resolved_active_tabset();
    Ok(())
}

fn normalize_node(model: &mut LayoutModel, id: NodeId) -> Option<NodeId> {
    let node = model.nodes.get(&id)?.clone();
    match node {
        LayoutNode::Tab(_) => Some(id),
        LayoutNode::TabSet(mut tabset) => {
            tabset.tabs.retain(|tab| model.tab(*tab).is_some());
            if tabset.tabs.is_empty() {
                model.nodes.remove(&id);
                return None;
            }
            if !tabset
                .selected
                .is_some_and(|tab| tabset.tabs.contains(&tab))
            {
                tabset.selected = tabset.tabs.first().copied();
            }
            model.nodes.insert(id, LayoutNode::TabSet(tabset));
            Some(id)
        }
        LayoutNode::Row(row) => {
            let mut children = Vec::new();
            for child in row.children {
                let Some(child_id) = normalize_node(model, child.node) else {
                    continue;
                };
                if let Some(LayoutNode::Row(nested)) = model.nodes.get(&child_id).cloned() {
                    if nested.axis == row.axis {
                        let nested_total: f32 = nested.children.iter().map(|c| c.weight).sum();
                        for nested_child in nested.children {
                            children.push(WeightedChild {
                                node: nested_child.node,
                                weight: child.weight * nested_child.weight
                                    / nested_total.max(0.001),
                            });
                        }
                        model.nodes.remove(&child_id);
                        continue;
                    }
                }
                children.push(WeightedChild {
                    node: child_id,
                    weight: child.weight,
                });
            }

            match children.len() {
                0 => {
                    model.nodes.remove(&id);
                    None
                }
                1 => {
                    model.nodes.remove(&id);
                    Some(children[0].node)
                }
                _ => {
                    let total: f32 = children.iter().map(|child| child.weight).sum();
                    for child in &mut children {
                        child.weight = child.weight / total.max(0.001) * 100.0;
                    }
                    model.nodes.insert(
                        id,
                        LayoutNode::Row(RowNode {
                            axis: row.axis,
                            children,
                        }),
                    );
                    Some(id)
                }
            }
        }
    }
}

fn collect_reachable(model: &LayoutModel, id: NodeId, reachable: &mut BTreeSet<NodeId>) {
    if !reachable.insert(id) {
        return;
    }
    match model.node(id) {
        Some(LayoutNode::Row(row)) => {
            for child in &row.children {
                collect_reachable(model, child.node, reachable);
            }
        }
        Some(LayoutNode::TabSet(tabset)) => {
            for tab in &tabset.tabs {
                collect_reachable(model, *tab, reachable);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab_for(model: &LayoutModel, panel: PanelKind) -> NodeId {
        model
            .nodes
            .iter()
            .find_map(|(id, node)| match node {
                LayoutNode::Tab(tab) if tab.panel == panel => Some(*id),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn default_layout_validates_and_round_trips() {
        let model = LayoutModel::default_editor();
        model.validate().unwrap();
        let json = serde_json::to_string_pretty(&model).unwrap();
        let restored: LayoutModel = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, model);
        restored.validate().unwrap();
    }

    #[test]
    fn moving_a_tab_updates_source_and_target() {
        let mut model = LayoutModel::default_editor();
        let console = tab_for(&model, PanelKind::Console);
        let scene = tab_for(&model, PanelKind::Scene);
        let scene_set = model.parent_of(scene).unwrap();
        model
            .apply(LayoutAction::MoveTab {
                tab: console,
                target_tabset: scene_set,
                insertion_index: 1,
            })
            .unwrap();
        assert_eq!(model.parent_of(console), Some(scene_set));
        assert_eq!(model.tabset(scene_set).unwrap().selected, Some(console));
        model.validate().unwrap();
    }

    #[test]
    fn edge_docking_creates_a_new_tabset() {
        let mut model = LayoutModel::default_editor();
        let console = tab_for(&model, PanelKind::Console);
        let scene = tab_for(&model, PanelKind::Scene);
        let scene_set = model.parent_of(scene).unwrap();
        model
            .apply(LayoutAction::DockTab {
                tab: console,
                target_tabset: scene_set,
                location: DockLocation::Left,
                ratio: 0.3,
            })
            .unwrap();
        assert_ne!(model.parent_of(console), Some(scene_set));
        model.validate().unwrap();
    }

    #[test]
    fn every_edge_docking_direction_preserves_invariants() {
        for location in [
            DockLocation::Left,
            DockLocation::Right,
            DockLocation::Top,
            DockLocation::Bottom,
        ] {
            let mut model = LayoutModel::default_editor();
            let console = tab_for(&model, PanelKind::Console);
            let scene = tab_for(&model, PanelKind::Scene);
            let scene_set = model.parent_of(scene).unwrap();
            model
                .apply(LayoutAction::DockTab {
                    tab: console,
                    target_tabset: scene_set,
                    location,
                    ratio: 0.35,
                })
                .unwrap();
            model.validate().unwrap();
        }
    }

    #[test]
    fn closing_the_only_tab_normalizes_its_empty_branch() {
        let mut model = LayoutModel::default_editor();
        let console = tab_for(&model, PanelKind::Console);
        model
            .apply(LayoutAction::CloseTab { tab: console })
            .unwrap();
        assert!(model
            .nodes
            .values()
            .all(|node| !matches!(node, LayoutNode::Tab(tab) if tab.panel == PanelKind::Console)));
        model.validate().unwrap();
    }

    /// Detaching is structural removal and is allowed for the non-closeable Scene tab.
    #[test]
    fn detaching_scene_removes_it_and_preserves_a_valid_layout() {
        let mut model = LayoutModel::default_editor();
        let scene = tab_for(&model, PanelKind::Scene);

        model.apply(LayoutAction::DetachTab { tab: scene }).unwrap();

        assert!(model.tab(scene).is_none());
        assert!(model.validate().is_ok());
    }

    /// A pop-out cannot consume the final dock node because the root must remain valid.
    #[test]
    fn detaching_the_last_tab_is_rejected_atomically() {
        let mut model = LayoutModel::default_editor();
        for panel in [
            PanelKind::Hierarchy,
            PanelKind::Inspector,
            PanelKind::Systems,
            PanelKind::Console,
            // Problems shares the Console's tab set; both have to go.
            PanelKind::Problems,
            PanelKind::Statistics,
            PanelKind::Preferences,
            PanelKind::Gallery,
            PanelKind::Assets,
            // The Game view shares the Scene view's tab set, so it has to
            // go first for the Scene detach below to be the last one.
            PanelKind::Game,
        ] {
            let tab = tab_for(&model, panel);
            model.apply(LayoutAction::DetachTab { tab }).unwrap();
        }
        let before = model.clone();
        let scene = tab_for(&model, PanelKind::Scene);

        assert_eq!(
            model.apply(LayoutAction::DetachTab { tab: scene }),
            Err(LayoutError::WouldEmptyLayout)
        );
        assert_eq!(model, before);
    }

    #[test]
    fn resizing_changes_only_the_adjacent_pair() {
        let mut model = LayoutModel::default_editor();
        let row = model
            .nodes
            .iter()
            .find_map(|(id, node)| match node {
                LayoutNode::Row(row) if row.children.len() == 3 => Some(*id),
                _ => None,
            })
            .unwrap();
        let third_before = model.row(row).unwrap().children[2].weight;
        // The pair's new weights keep its old sum: only then is the third
        // child's weight untouched (a sum that changes rescales the row).
        let pair_sum =
            model.row(row).unwrap().children[0].weight + model.row(row).unwrap().children[1].weight;
        model
            .apply(LayoutAction::ResizeSplit {
                row,
                splitter_index: 0,
                first_weight: pair_sum * 0.4,
                second_weight: pair_sum * 0.6,
            })
            .unwrap();
        let resized = model.row(row).unwrap();
        // The third child stays where it was, within the rounding a rescale
        // by one introduces.
        assert!((resized.children[2].weight - third_before).abs() < 0.001);
        assert!(resized.children[0].weight < resized.children[1].weight);
    }

    #[test]
    fn rejected_action_does_not_mutate_model() {
        let mut model = LayoutModel::default_editor();
        let original = model.clone();
        let scene = tab_for(&model, PanelKind::Scene);
        assert!(model.apply(LayoutAction::CloseTab { tab: scene }).is_err());
        assert_eq!(model, original);
    }

    #[test]
    fn normalize_falls_back_to_the_first_tabset_when_none_is_active() {
        let mut model = LayoutModel::default_editor();
        model.active_tabset = None;
        normalize(&mut model).unwrap();
        let first_tabset = model
            .nodes
            .iter()
            .find_map(|(id, node)| matches!(node, LayoutNode::TabSet(_)).then_some(*id));
        assert_eq!(model.active_tabset, first_tabset);
        assert_eq!(model.active_tabset, model.resolved_active_tabset());
    }

    /// Popping a tab out keeps its node, hands it the default window size
    /// and survives validation and a JSON round trip; a redock puts it back
    /// selected (Stage 29).
    #[test]
    fn popping_out_and_redocking_a_tab_round_trips() {
        let mut model = LayoutModel::default_editor();
        let console = tab_for(&model, PanelKind::Console);
        let home = model.parent_of(console).unwrap();

        model
            .apply(LayoutAction::PopOutTab { tab: console })
            .unwrap();
        assert!(model.tab(console).is_some(), "the node stays alive");
        assert_eq!(model.floating.len(), 1);
        assert_eq!(model.floating[0].tab, console);
        assert_eq!(
            (model.floating[0].width, model.floating[0].height),
            super::super::model::FLOATING_TAB_DEFAULT_SIZE
        );
        assert!(model.parent_of(console).is_none(), "it left the dock");
        model.validate().unwrap();
        let json = serde_json::to_string_pretty(&model).unwrap();
        let restored: LayoutModel = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, model, "the floating list is on disk");
        restored.validate().unwrap();

        model
            .apply(LayoutAction::RedockTab {
                tab: console,
                target_tabset: home,
            })
            .unwrap();
        assert!(model.floating.is_empty());
        assert_eq!(model.parent_of(console), Some(home));
        assert_eq!(model.tabset(home).unwrap().selected, Some(console));
        model.validate().unwrap();
    }

    /// A pop-out that would leave the dock with nothing to draw is refused
    /// atomically, and bounds changes land exactly once.
    #[test]
    fn a_pop_out_never_empties_the_dock_and_bounds_stick() {
        let mut model = LayoutModel::default_editor();
        for panel in [
            PanelKind::Hierarchy,
            PanelKind::Inspector,
            PanelKind::Systems,
            PanelKind::Console,
            PanelKind::Problems,
            PanelKind::Statistics,
            PanelKind::Preferences,
            PanelKind::Gallery,
            PanelKind::Assets,
            PanelKind::Game,
            PanelKind::Scene,
        ] {
            let tab = tab_for(&model, panel);
            let _ = model.apply(LayoutAction::PopOutTab { tab });
        }
        // Whatever remains docked, one more pop-out cannot empty it.
        let before = model.clone();
        let docked = model.nodes.iter().find_map(|(id, node)| match node {
            LayoutNode::Tab(_) if model.floating.iter().all(|entry| entry.tab != *id) => Some(*id),
            _ => None,
        });
        if let Some(tab) = docked {
            assert_eq!(
                model.apply(LayoutAction::PopOutTab { tab }),
                Err(LayoutError::WouldEmptyLayout)
            );
            assert_eq!(model, before, "the refusal is atomic");
        }

        let console = tab_for(&model, PanelKind::Console);
        if model.floating.iter().any(|entry| entry.tab == console) {
            model
                .apply(LayoutAction::SetFloatingBounds {
                    tab: console,
                    width: 640.0,
                    height: 480.0,
                    position: Some((120.0, 80.0)),
                })
                .unwrap();
            let entry = model
                .floating
                .iter()
                .find(|entry| entry.tab == console)
                .unwrap();
            assert_eq!((entry.width, entry.height), (640.0, 480.0));
            assert_eq!(entry.position, Some((120.0, 80.0)));
            model.validate().unwrap();
        }
    }

    /// Several Scene views are the stage's point: each opens with a numbered
    /// title, stays closeable (only the workspace's own Scene tab does not)
    /// and validates together with the first.
    #[test]
    fn a_second_scene_view_opens_numbered_and_valid() {
        let mut model = LayoutModel::default_editor();
        let scene = tab_for(&model, PanelKind::Scene);
        assert!(!model.tab(scene).unwrap().closeable);
        let target = model.parent_of(scene).unwrap();

        model
            .apply(LayoutAction::OpenTab {
                panel: PanelKind::Scene,
                target_tabset: target,
            })
            .unwrap();
        let scenes: Vec<&super::super::model::TabNode> = model
            .nodes
            .values()
            .filter_map(|node| match node {
                LayoutNode::Tab(tab) if tab.panel == PanelKind::Scene => Some(tab),
                _ => None,
            })
            .collect();
        assert_eq!(scenes.len(), 2);
        assert!(scenes
            .iter()
            .any(|tab| tab.title == "Scene 2" && tab.closeable));
        model.validate().unwrap();
    }
}

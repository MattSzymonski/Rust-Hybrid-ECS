use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use super::model::{LayoutNode, RowNode, TabSetNode, WeightedChild};
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
    DuplicateScene,
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
            remove_tab(model, tab)?;
            model.nodes.remove(&tab);
            Ok(true)
        }
        LayoutAction::OpenTab {
            panel,
            target_tabset,
        } => {
            if model
                .nodes
                .values()
                .any(|node| matches!(node, LayoutNode::Tab(tab) if tab.panel == panel))
            {
                return Err(LayoutError::DuplicatePanel(panel));
            }
            if model.tabset(target_tabset).is_none() {
                return Err(LayoutError::WrongNodeKind(target_tabset));
            }
            let tab = model.add_tab(panel, panel != PanelKind::Scene);
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
    model.nodes.retain(|id, _| reachable.contains(id));
    model.next_id = model
        .nodes
        .keys()
        .map(|id| id.0)
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    if !model
        .active_tabset
        .is_some_and(|id| model.tabset(id).is_some())
    {
        model.active_tabset = model
            .nodes
            .iter()
            .find_map(|(id, node)| matches!(node, LayoutNode::TabSet(_)).then_some(*id));
    }
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
        model
            .apply(LayoutAction::ResizeSplit {
                row,
                splitter_index: 0,
                first_weight: 30.0,
                second_weight: 45.0,
            })
            .unwrap();
        let resized = model.row(row).unwrap();
        assert_eq!(resized.children[2].weight, third_before);
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
}

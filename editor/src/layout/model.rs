use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{LayoutAction, LayoutChange, LayoutError, NodeId};

pub const LAYOUT_SCHEMA_VERSION: u32 = 1;

/// Direction in which a row distributes its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Serializable editor panel identity used by the panel factory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelKind {
    Scene,
    Hierarchy,
    Inspector,
    Console,
    Statistics,
}

impl PanelKind {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Scene => "Scene",
            Self::Hierarchy => "Hierarchy",
            Self::Inspector => "Inspector",
            Self::Console => "Console",
            Self::Statistics => "Statistics",
        }
    }
}

/// One weighted child within a row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeightedChild {
    pub node: NodeId,
    pub weight: f32,
}

/// A horizontal or vertical collection of rows/tabsets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowNode {
    pub axis: Axis,
    pub children: Vec<WeightedChild>,
}

/// A collection of selectable tabs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabSetNode {
    pub tabs: Vec<NodeId>,
    pub selected: Option<NodeId>,
}

/// Serializable metadata for one editor panel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabNode {
    pub title: String,
    pub panel: PanelKind,
    pub closeable: bool,
}

/// Node variants accepted by the dock tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNode {
    Row(RowNode),
    TabSet(TabSetNode),
    Tab(TabNode),
}

/// Versioned, normalized dock workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutModel {
    pub schema_version: u32,
    pub root: NodeId,
    pub nodes: BTreeMap<NodeId, LayoutNode>,
    pub active_tabset: Option<NodeId>,
    pub(crate) next_id: u64,
}

impl LayoutModel {
    /// Build the editor's initial multi-pane workspace.
    pub fn default_editor() -> Self {
        let mut model = Self {
            schema_version: LAYOUT_SCHEMA_VERSION,
            root: NodeId(0),
            nodes: BTreeMap::new(),
            active_tabset: None,
            next_id: 1,
        };

        let hierarchy = model.add_tab(PanelKind::Hierarchy, true);
        let scene = model.add_tab(PanelKind::Scene, false);
        let inspector = model.add_tab(PanelKind::Inspector, true);
        let statistics = model.add_tab(PanelKind::Statistics, true);
        let console = model.add_tab(PanelKind::Console, true);

        let hierarchy_set = model.add_tabset(vec![hierarchy], hierarchy);
        let scene_set = model.add_tabset(vec![scene], scene);
        let inspector_set = model.add_tabset(vec![inspector], inspector);
        let statistics_set = model.add_tabset(vec![statistics], statistics);
        let console_set = model.add_tabset(vec![console], console);

        let right = model.add_row(
            Axis::Vertical,
            vec![(inspector_set, 55.0), (statistics_set, 45.0)],
        );
        let top = model.add_row(
            Axis::Horizontal,
            vec![(hierarchy_set, 20.0), (scene_set, 55.0), (right, 25.0)],
        );
        let root = model.add_row(Axis::Vertical, vec![(top, 76.0), (console_set, 24.0)]);
        model.root = root;
        model.active_tabset = Some(scene_set);
        model
    }

    /// Apply one atomic action through the validated reducer.
    pub fn apply(&mut self, action: LayoutAction) -> Result<LayoutChange, LayoutError> {
        super::reducer::reduce(self, action)
    }

    pub fn node(&self, id: NodeId) -> Option<&LayoutNode> {
        self.nodes.get(&id)
    }

    pub fn tab(&self, id: NodeId) -> Option<&TabNode> {
        match self.node(id) {
            Some(LayoutNode::Tab(tab)) => Some(tab),
            _ => None,
        }
    }

    pub fn tabset(&self, id: NodeId) -> Option<&TabSetNode> {
        match self.node(id) {
            Some(LayoutNode::TabSet(tabset)) => Some(tabset),
            _ => None,
        }
    }

    pub fn row(&self, id: NodeId) -> Option<&RowNode> {
        match self.node(id) {
            Some(LayoutNode::Row(row)) => Some(row),
            _ => None,
        }
    }

    pub(crate) fn allocate_id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    pub(crate) fn add_tab(&mut self, panel: PanelKind, closeable: bool) -> NodeId {
        let id = self.allocate_id();
        self.nodes.insert(
            id,
            LayoutNode::Tab(TabNode {
                title: panel.title().to_string(),
                panel,
                closeable,
            }),
        );
        id
    }

    pub(crate) fn add_tabset(&mut self, tabs: Vec<NodeId>, selected: NodeId) -> NodeId {
        let id = self.allocate_id();
        self.nodes.insert(
            id,
            LayoutNode::TabSet(TabSetNode {
                tabs,
                selected: Some(selected),
            }),
        );
        id
    }

    pub(crate) fn add_row(&mut self, axis: Axis, children: Vec<(NodeId, f32)>) -> NodeId {
        let id = self.allocate_id();
        self.nodes.insert(
            id,
            LayoutNode::Row(RowNode {
                axis,
                children: children
                    .into_iter()
                    .map(|(node, weight)| WeightedChild { node, weight })
                    .collect(),
            }),
        );
        id
    }

    pub(crate) fn parent_of(&self, child: NodeId) -> Option<NodeId> {
        self.nodes.iter().find_map(|(id, node)| match node {
            LayoutNode::Row(row) if row.children.iter().any(|entry| entry.node == child) => {
                Some(*id)
            }
            LayoutNode::TabSet(tabset) if tabset.tabs.contains(&child) => Some(*id),
            _ => None,
        })
    }

    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.schema_version != LAYOUT_SCHEMA_VERSION {
            return Err(LayoutError::UnsupportedVersion(self.schema_version));
        }
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        self.validate_node(self.root, &mut visiting, &mut visited)?;

        if let Some(id) = self.nodes.keys().find(|id| !visited.contains(id)) {
            return Err(LayoutError::UnreachableNode(*id));
        }
        if self.next_id <= self.nodes.keys().map(|id| id.0).max().unwrap_or(0) {
            return Err(LayoutError::InvalidNextId);
        }
        if let Some(active) = self.active_tabset {
            if self.tabset(active).is_none() {
                return Err(LayoutError::InvalidActiveTabSet(active));
            }
        }

        let scene_count = visited
            .iter()
            .filter_map(|id| self.tab(*id))
            .filter(|tab| tab.panel == PanelKind::Scene)
            .count();
        if scene_count > 1 {
            return Err(LayoutError::DuplicateScene);
        }
        Ok(())
    }

    fn validate_node(
        &self,
        id: NodeId,
        visiting: &mut BTreeSet<NodeId>,
        visited: &mut BTreeSet<NodeId>,
    ) -> Result<(), LayoutError> {
        if !visiting.insert(id) {
            return Err(LayoutError::Cycle(id));
        }
        if !visited.insert(id) {
            return Err(LayoutError::MultipleParents(id));
        }
        match self.node(id).ok_or(LayoutError::MissingNode(id))? {
            LayoutNode::Row(row) => {
                if row.children.is_empty() {
                    return Err(LayoutError::EmptyRow(id));
                }
                for child in &row.children {
                    if !child.weight.is_finite() || child.weight <= 0.0 {
                        return Err(LayoutError::InvalidWeight(id));
                    }
                    if matches!(self.node(child.node), Some(LayoutNode::Tab(_))) {
                        return Err(LayoutError::InvalidChild(id));
                    }
                    self.validate_node(child.node, visiting, visited)?;
                }
            }
            LayoutNode::TabSet(tabset) => {
                if tabset.tabs.is_empty() {
                    return Err(LayoutError::EmptyTabSet(id));
                }
                if !tabset
                    .selected
                    .is_some_and(|tab| tabset.tabs.contains(&tab))
                {
                    return Err(LayoutError::InvalidSelection(id));
                }
                for tab in &tabset.tabs {
                    if !matches!(self.node(*tab), Some(LayoutNode::Tab(_))) {
                        return Err(LayoutError::InvalidChild(id));
                    }
                    self.validate_node(*tab, visiting, visited)?;
                }
            }
            LayoutNode::Tab(_) => {}
        }
        visiting.remove(&id);
        Ok(())
    }
}

impl Default for LayoutModel {
    fn default() -> Self {
        Self::default_editor()
    }
}

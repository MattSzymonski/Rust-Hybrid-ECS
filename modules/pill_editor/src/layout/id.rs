use serde::{Deserialize, Serialize};

/// Stable identifier serialized with the workspace layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u64);

impl NodeId {
    /// Return a DOM-safe stable identifier.
    pub fn dom_id(self, prefix: &str) -> String {
        format!("{prefix}-{}", self.0)
    }
}

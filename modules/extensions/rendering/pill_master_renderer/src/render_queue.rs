//! Stable material and mesh ordering for instanced PBR draws.
//!
//! # Responsibilities
//!
//! - Builds comparable keys from complete logical asset IDs.
//! - Keeps instances sharing a material and mesh adjacent after sorting.
//!
//! # Design
//!
//! Adapted from Pill-Engine graphics/render_queue.rs (af052d1). The source used
//! small slot fields; this key preserves both 64-bit IDs so different assets
//! cannot alias merely because their lower bits match. Derived ordering compares
//! material first, then mesh, matching the backend's consecutive batch scan.

// =============================================================================
// Batch Ordering
// =============================================================================

/// Lexicographic batch key with the material as the primary sort field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RenderQueueKey {
    /// Full stable material ID.
    pub material: u64,
    /// Full stable mesh ID within the material group.
    pub mesh: u64,
}

/// Compose a sort key without truncating either asset identity.
pub fn compose_pbr_render_queue_key(material: u64, mesh: u64) -> RenderQueueKey {
    RenderQueueKey { material, mesh }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    /// Distinct upper ID bits must survive composition of the batch key.
    #[test]
    fn large_asset_ids_do_not_alias() {
        assert_ne!(
            compose_pbr_render_queue_key(1, 1),
            compose_pbr_render_queue_key(257, 257)
        );
    }
}

//! An ordered list of passes, as one asset.
//!
//! # Responsibilities
//!
//! - Hold the passes a frame runs, in the order the game wrote them.
//! - Stay a handle list rather than a copy of the passes, so one pass can appear
//!   in several pipelines and editing a pass does not duplicate it.
//!
//! # Design
//!
//! This asset is only the declaration. Building the chain - ordering,
//! resolving target names, creating the pipelines the GPU runs - happens in the
//! renderer when the game hands this asset over through `RenderingManager`,
//! because that is the side that owns the device.

use pill_engine::{Asset, Handle};

use crate::RenderPass;

/// The passes a frame runs, in the order they were added.
#[derive(Clone, Debug, Default)]
pub struct RenderingPipeline {
    /// Pass handles, in the order they run. A pass's own `order` key refines
    /// this when the renderer sorts the chain.
    pub passes: Vec<Handle<RenderPass>>,
}

impl RenderingPipeline {
    /// An empty pipeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one pass.
    pub fn add(&mut self, pass: Handle<RenderPass>) {
        self.passes.push(pass);
    }

    /// Append one pass, for chaining.
    pub fn with_pass(mut self, pass: Handle<RenderPass>) -> Self {
        self.passes.push(pass);
        self
    }
}

impl Asset for RenderingPipeline {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_keep_the_order_they_were_added() {
        let first = Handle::from_raw(0, 1);
        let second = Handle::from_raw(1, 1);
        let mut pipeline = RenderingPipeline::new().with_pass(first);

        pipeline.add(second);

        assert_eq!(pipeline.passes, vec![first, second]);
    }

    #[test]
    fn an_empty_pipeline_has_no_passes() {
        assert!(RenderingPipeline::new().passes.is_empty());
    }
}

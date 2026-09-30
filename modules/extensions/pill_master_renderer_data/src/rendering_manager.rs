//! The world resource holding the pipeline the renderer runs.
//!
//! # Responsibilities
//!
//! - Store the pipeline the game set, and nothing else.
//! - Cross the module boundary: a project DLL sets the pipeline, the host's
//!   renderer executes it, and both sides have to agree on one resource.
//!
//! # Design
//!
//! Inserted by `pill_master_renderer::register`, so it exists before any project
//! code runs and `set_pipeline` never has to check for it. The `shared_name` is
//! what makes the host binary and a project module agree on the resource id,
//! exactly as `RenderFrame` does; without it the renderer would read a resource
//! the project never wrote.

use pill_engine::{Handle, Resource};

use crate::RenderingPipeline;

/// The pipeline the renderer runs, as the game declared it.
#[derive(Clone, Debug, Default)]
pub struct RenderingManager {
    pipeline: Option<Handle<RenderingPipeline>>,
}

impl RenderingManager {
    /// A manager with no pipeline: the renderer keeps its built-in chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand the renderer a pipeline to run.
    ///
    /// This is the call that makes the renderer build the chain: on its next
    /// frame it resolves the passes, checks every offscreen target against the
    /// pass that produces it, and creates the pipelines it does not have yet.
    /// Until then the renderer keeps drawing with its built-in chain.
    pub fn set_pipeline(&mut self, pipeline: Handle<RenderingPipeline>) {
        self.pipeline = Some(pipeline);
    }

    /// The pipeline the renderer should run, if the game set one.
    pub fn pipeline(&self) -> Option<Handle<RenderingPipeline>> {
        self.pipeline
    }

    /// Drop the pipeline, returning the renderer to its built-in chain.
    pub fn clear(&mut self) {
        self.pipeline = None;
    }
}

impl Resource for RenderingManager {
    fn shared_name() -> Option<&'static str> {
        Some("pill_master_renderer::resources::rendering_manager::RenderingManager")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_manager_has_no_pipeline() {
        assert_eq!(RenderingManager::new().pipeline(), None);
    }

    #[test]
    fn setting_and_clearing_the_pipeline_moves_one_handle() {
        let pipeline = Handle::from_raw(3, 1);
        let mut manager = RenderingManager::new();

        manager.set_pipeline(pipeline);
        assert_eq!(manager.pipeline(), Some(pipeline));

        manager.clear();
        assert_eq!(manager.pipeline(), None);
    }

    #[test]
    fn the_resource_crosses_the_module_boundary_by_name() {
        assert!(RenderingManager::shared_name().is_some());
    }
}

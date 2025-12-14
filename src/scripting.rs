// ============================================================================
// Scripting System
// ============================================================================
//! Script components that can update themselves each frame.

use crate::component::Component;
use crate::engine::Engine;
use crate::entity::Entity;

/// Trait for components that can execute logic each frame
///
/// Script components have an update() method that gets called by the scripting system.
/// They receive their entity ID and mutable engine access to modify game state.
pub trait ScriptComponent: Component {
    /// Update this script component
    ///
    /// Called once per frame by the scripting system.
    /// The component can modify itself and interact with the engine.
    fn update(&mut self, entity: Entity, engine: &mut Engine);
}

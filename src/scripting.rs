// ============================================================================
// Scripting System
// ============================================================================
//! Script components that can update themselves each frame.

use crate::component::Component;
use crate::entity::Entity;
use crate::world::World;

/// Trait for components that can execute logic each frame
///
/// Script components have an update() method that gets called by the scripting system.
/// They receive their entity ID and mutable world access to modify game state.
pub trait ScriptComponent: Component {
    /// Update this script component
    ///
    /// Called once per frame by the scripting system.
    /// The component can modify itself and interact with the world.
    fn update(&mut self, entity: Entity, world: &mut World);
}

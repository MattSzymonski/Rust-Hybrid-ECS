/// Game systems - the parallel execution units
use crate::ecs_core::World;
use parking_lot::RwLock;
use std::sync::Arc;

/// Trait for systems that can run in parallel
pub trait System: Send + Sync {
    fn execute(&mut self, world: &mut World, delta_time: f32);
}

/// System executor - manages parallel system execution
pub struct SystemExecutor {
    systems: Vec<Box<dyn System>>,
}

impl SystemExecutor {
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
        }
    }

    pub fn add_system<S: System + 'static>(&mut self, system: S) {
        self.systems.push(Box::new(system));
    }

    /// Execute all systems - in real implementation, this would use rayon or similar
    /// for parallel execution
    pub fn execute(&mut self, world: &mut World, delta_time: f32) {
        // In a real implementation, systems would run in parallel here
        // For simplicity, we run them sequentially
        // With proper ECS design, systems that don't conflict can run in parallel
        for system in &mut self.systems {
            system.execute(world, delta_time);
        }
    }
}

impl Default for SystemExecutor {
    fn default() -> Self {
        Self::new()
    }
}

// Example system trait for user convenience
pub trait GameSystem {
    fn update(&mut self, world: Arc<RwLock<World>>, delta_time: f32);
}

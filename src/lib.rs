// Library exports for the hybrid ECS engine

mod command_buffer;
mod ecs_core;
mod game_object;
mod systems;

pub use command_buffer::CommandBuffer;
pub use ecs_core::World;
pub use game_object::{ComponentRef, ComponentRefMut, Entity, GameObject, Scene};
pub use systems::{GameSystem, System, SystemExecutor};

// Re-export common components
#[derive(Debug, Clone)]
pub struct Transform {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Transform {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

#[derive(Debug, Clone)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Velocity {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

#[derive(Debug, Clone)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

impl Health {
    pub fn new(max: f32) -> Self {
        Self { current: max, max }
    }
}

#[derive(Debug, Clone)]
pub struct Name {
    pub value: String,
}

impl Name {
    pub fn new(name: impl Into<String>) -> Self {
        Self { value: name.into() }
    }
}

// Movement system implementation
pub struct MovementSystem;

impl System for MovementSystem {
    fn execute(&mut self, world: &mut World, delta_time: f32) {
        // Clean ECS-style: iterate over pairs of (Transform, Velocity)
        for (transform, velocity) in world.query2_mut::<Transform, Velocity>() {
            transform.x += velocity.x * delta_time;
            transform.y += velocity.y * delta_time;
            transform.z += velocity.z * delta_time;
        }
    }
}

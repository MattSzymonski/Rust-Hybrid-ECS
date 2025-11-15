use std::collections::HashMap;

use crate::ecs_core::{Component, Entity, ScriptComponent, World};

// Macro to implement Component trait with ScriptComponent casting for script types
macro_rules! impl_script_component {
    ($type:ty) => {
        impl Component for $type {
            fn as_script(&self) -> Option<&dyn ScriptComponent> {
                Some(self)
            }

            fn as_script_mut(&mut self) -> Option<&mut dyn ScriptComponent> {
                Some(self)
            }
        }
    };
}

// Context for script updates that allows mutations
pub struct UpdateContext {
    // Store component mutations to apply after all scripts run
    position_updates: HashMap<Entity, Position>,
}

impl UpdateContext {
    pub fn new() -> Self {
        Self {
            position_updates: HashMap::new(),
        }
    }

    pub fn set_position(&mut self, entity: Entity, x: f32, y: f32) {
        self.position_updates.insert(entity, Position { x, y });
    }

    pub fn move_position(&mut self, entity: Entity, dx: f32, dy: f32, world: &World) {
        if let Some(pos) = world.get_component::<Position>(entity) {
            self.position_updates.insert(
                entity,
                Position {
                    x: pos.x + dx,
                    y: pos.y + dy,
                },
            );
        }
    }

    // Move position with collision detection
    pub fn move_position_with_collision(
        &mut self,
        entity: Entity,
        dx: f32,
        dy: f32,
        world: &World,
    ) {
        if let Some(pos) = world.get_component::<Position>(entity) {
            let mut new_x = pos.x + dx;
            let mut new_y = pos.y + dy;

            // Check collision using iterator (zero allocation!)
            for (_collider_entity, collider_pos, collider) in
                world.get_two_component_iterator::<Position, BoxCollider>()
            {
                // Create a temporary collider for the moving entity (assume small size)
                let mover_collider = BoxCollider::new(10.0, 10.0);
                let test_pos = Position { x: new_x, y: new_y };

                // Check if the new position would collide
                if mover_collider.overlaps(&test_pos, collider, collider_pos) {
                    // Collision detected - clamp to collider edge
                    let half_width = mover_collider.width / 2.0;
                    let half_height = mover_collider.height / 2.0;
                    let c_half_width = collider.width / 2.0;
                    let c_half_height = collider.height / 2.0;

                    // Calculate overlap on each axis
                    let overlap_left = (collider_pos.x - c_half_width) - (new_x + half_width);
                    let overlap_right = (new_x - half_width) - (collider_pos.x + c_half_width);
                    let overlap_bottom = (collider_pos.y - c_half_height) - (new_y + half_height);
                    let overlap_top = (new_y - half_height) - (collider_pos.y + c_half_height);

                    // Find the smallest overlap to determine collision direction
                    let min_overlap_x = if overlap_left.abs() < overlap_right.abs() {
                        overlap_left
                    } else {
                        overlap_right
                    };
                    let min_overlap_y = if overlap_bottom.abs() < overlap_top.abs() {
                        overlap_bottom
                    } else {
                        overlap_top
                    };

                    // Clamp position to collider edge
                    if min_overlap_x.abs() < min_overlap_y.abs() {
                        new_x += min_overlap_x;
                    } else {
                        new_y += min_overlap_y;
                    }
                }
            }

            self.position_updates
                .insert(entity, Position { x: new_x, y: new_y });
        }
    }
}

// ------------------------------------ Position -----------------------------------------

// Position component needs to be public here for UpdateContext
#[derive(Debug, Clone, Default)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

impl Component for Position {}

// ------------------------------------ Sprite -------------------------------------------

// Sprite rendering component
#[derive(Debug, Clone)]
pub struct Sprite {
    pub color: (f32, f32, f32), // RGB color (0.0-1.0)
    pub width: f32,             // Width of the sprite
    pub height: f32,            // Height of the sprite
}

impl Component for Sprite {}

impl Sprite {
    pub fn new(color: (f32, f32, f32), width: f32, height: f32) -> Self {
        Self {
            color,
            width,
            height,
        }
    }
}

// ---------------------------------- BoxCollider ----------------------------------------

// Box Collider component - 2D axis-aligned bounding box
#[derive(Debug, Clone, Default)]
pub struct BoxCollider {
    pub width: f32,
    pub height: f32,
}

impl Component for BoxCollider {}

impl BoxCollider {
    pub fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    // Check if a point is inside this collider (given the collider's position)
    pub fn contains_point(&self, collider_pos: &Position, point_x: f32, point_y: f32) -> bool {
        let half_width = self.width / 2.0;
        let half_height = self.height / 2.0;

        point_x >= collider_pos.x - half_width
            && point_x <= collider_pos.x + half_width
            && point_y >= collider_pos.y - half_height
            && point_y <= collider_pos.y + half_height
    }

    // Check if two box colliders overlap
    pub fn overlaps(&self, pos1: &Position, other: &BoxCollider, pos2: &Position) -> bool {
        let half_width1 = self.width / 2.0;
        let half_height1 = self.height / 2.0;
        let half_width2 = other.width / 2.0;
        let half_height2 = other.height / 2.0;

        let left1 = pos1.x - half_width1;
        let right1 = pos1.x + half_width1;
        let top1 = pos1.y + half_height1;
        let bottom1 = pos1.y - half_height1;

        let left2 = pos2.x - half_width2;
        let right2 = pos2.x + half_width2;
        let top2 = pos2.y + half_height2;
        let bottom2 = pos2.y - half_height2;

        !(right1 < left2 || left1 > right2 || top1 < bottom2 || bottom1 > top2)
    }
}

// ----------------------------------- Velocity -----------------------------------

// Example components
#[derive(Debug, Default)]
pub struct Velocity {
    pub dx: f32,
    pub dy: f32,
}

impl Component for Velocity {}

// ------------------------------------- Name --------------------------------------------

#[derive(Debug, Default)]
pub struct Name(pub String);

impl Component for Name {}

// ------------------------------------ Scripts ------------------------------------------

// Script components - these have update logic
pub struct MoverScript {
    pub speed: f32,
}

impl Default for MoverScript {
    fn default() -> Self {
        Self { speed: 0.0 }
    }
}

impl_script_component!(MoverScript);

impl ScriptComponent for MoverScript {
    fn update(&mut self, entity: Entity, world: &mut World) {
        // Access and modify the entity's Position based on Velocity
        if let Some(vel) = world.get_component::<Velocity>(entity) {
            let dx = vel.dx * self.speed;
            let dy = vel.dy * self.speed;

            // Use context to schedule the position update
            //ctx.move_position(entity, dx, dy, world);

            if let Some(pos) = world.get_component::<Position>(entity) {
                println!(
                    "  MoverScript updating Entity {:?}: moving from ({}, {}) by ({}, {})",
                    entity, pos.x, pos.y, dx, dy
                );
            }
        }
    }
}

// -------------------------------- Collision Mover ------------------------------------

// Mover script with collision detection
pub struct CollisionMoverScript {
    pub speed: f32,
}

impl Default for CollisionMoverScript {
    fn default() -> Self {
        Self { speed: 0.0 }
    }
}

impl_script_component!(CollisionMoverScript);

impl ScriptComponent for CollisionMoverScript {
    fn update(&mut self, entity: Entity, world: &mut World) {
        // Access and modify the entity's Position based on Velocity
        if let Some(vel) = world.get_component::<Velocity>(entity) {
            let dx = vel.dx * self.speed;
            let dy = vel.dy * self.speed;

            // Use context to schedule the position update WITH collision detection
            // ctx.move_position_with_collision(entity, dx, dy, world);

            if let Some(pos) = world.get_component::<Position>(entity) {
                println!(
                    "  CollisionMoverScript updating Entity {:?}: moving from ({}, {}) by ({}, {})",
                    entity, pos.x, pos.y, dx, dy
                );
            }
        }
    }
}

// -------------------------------- Silent Collision Mover ------------------------------

// Silent collision mover for performance testing
#[derive(Debug, Default)]
pub struct SilentCollisionMoverScript {
    pub speed: f32,
}

impl_script_component!(SilentCollisionMoverScript);

impl ScriptComponent for SilentCollisionMoverScript {
    fn update(&mut self, entity: Entity, world: &mut World) {
        if let Some(vel) = world.get_component::<Velocity>(entity) {
            let dx = vel.dx * self.speed;
            let dy = vel.dy * self.speed;

            //ctx.move_position_with_collision(entity, dx, dy, world);

            if let Some(pos) = world.get_component::<Position>(entity) {
                let mut new_x = pos.x + dx;
                let mut new_y = pos.y + dy;

                // Check collision using iterator (zero allocation!)
                for (_collider_entity, collider_pos, collider) in
                    world.get_two_component_iterator::<Position, BoxCollider>()
                {
                    // Create a temporary collider for the moving entity (assume small size)
                    let mover_collider = BoxCollider::new(10.0, 10.0);
                    let test_pos = Position { x: new_x, y: new_y };

                    // Check if the new position would collide
                    if mover_collider.overlaps(&test_pos, collider, collider_pos) {
                        // Collision detected - clamp to collider edge
                        let half_width = mover_collider.width / 2.0;
                        let half_height = mover_collider.height / 2.0;
                        let c_half_width = collider.width / 2.0;
                        let c_half_height = collider.height / 2.0;

                        // Calculate overlap on each axis
                        let overlap_left = (collider_pos.x - c_half_width) - (new_x + half_width);
                        let overlap_right = (new_x - half_width) - (collider_pos.x + c_half_width);
                        let overlap_bottom =
                            (collider_pos.y - c_half_height) - (new_y + half_height);
                        let overlap_top = (new_y - half_height) - (collider_pos.y + c_half_height);

                        // Find the smallest overlap to determine collision direction
                        let min_overlap_x = if overlap_left.abs() < overlap_right.abs() {
                            overlap_left
                        } else {
                            overlap_right
                        };
                        let min_overlap_y = if overlap_bottom.abs() < overlap_top.abs() {
                            overlap_bottom
                        } else {
                            overlap_top
                        };

                        // Clamp position to collider edge
                        if min_overlap_x.abs() < min_overlap_y.abs() {
                            new_x += min_overlap_x;
                        } else {
                            new_y += min_overlap_y;
                        }
                    }
                }

                if let Some(pos) = world.get_component_mut::<Position>(entity) {
                    pos.x = new_x;
                    pos.y = new_y;
                }
            }
        }
    }
}

// ----------------------------------- Logger Script -------------------------------------
#[derive(Debug, Default)]
pub struct LoggerScript {
    pub message: String,
}

impl_script_component!(LoggerScript);

impl ScriptComponent for LoggerScript {
    fn update(&mut self, entity: Entity, world: &mut World) {
        if let Some(name) = world.get_component::<Name>(entity) {
            println!("  LoggerScript: {} - {}", name.0, self.message);
        } else {
            println!("  LoggerScript on Entity {:?}: {}", entity, self.message);
        }
    }
}

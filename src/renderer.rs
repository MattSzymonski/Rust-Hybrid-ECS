use crate::ecs_core::{Position, Sprite, World};
use flo_draw::canvas::*;
use flo_draw::*;

pub struct Renderer {
    canvas: DrawingTarget,
    window_width: f32,
    window_height: f32,
}

impl Renderer {
    pub fn new(window_width: f32, window_height: f32) -> Self {
        // Create a window with a canvas
        let canvas = create_drawing_window("ECS Sprite Renderer");

        // Set up canvas dimensions once
        canvas.draw(|gc| {
            gc.canvas_height(window_height);
            gc.center_region(0.0, 0.0, window_width, window_height);
        });

        Self {
            canvas,
            window_width,
            window_height,
        }
    }

    pub fn clear(&self) {
        self.canvas.draw(|gc| {
            // Clear the canvas with a dark background
            gc.clear_canvas(canvas::Color::Rgba(0.1, 0.1, 0.15, 1.0));
        });
    }

    pub fn render(&self, world: &World) {
        self.canvas.draw(|gc| {
            // Clear canvas
            gc.clear_canvas(canvas::Color::Rgba(0.1, 0.1, 0.15, 1.0));

            // Set up canvas dimensions and transform
            gc.canvas_height(self.window_height);
            gc.center_region(0.0, 0.0, self.window_width, self.window_height);

            // Query all entities with Position and Sprite components
            for (_entity, pos, sprite) in world.query2::<Position, Sprite>() {
                // Convert world coordinates to screen coordinates
                // World origin (0,0) should be at center of screen
                let screen_x = self.window_width / 2.0 + pos.x;
                let screen_y = self.window_height / 2.0 - pos.y; // Flip Y axis (screen Y goes down)

                // Set color
                let (r, g, b) = sprite.color;
                gc.fill_color(canvas::Color::Rgba(r, g, b, 1.0));
                gc.stroke_color(canvas::Color::Rgba(r * 0.8, g * 0.8, b * 0.8, 1.0));
                gc.line_width(2.0);

                // Draw a square for the sprite using path
                gc.new_path();
                let half_size = sprite.size;
                let x = screen_x;
                let y = screen_y;

                gc.move_to(x - half_size, y - half_size);
                gc.line_to(x + half_size, y - half_size);
                gc.line_to(x + half_size, y + half_size);
                gc.line_to(x - half_size, y + half_size);
                gc.close_path();
                gc.fill();
                gc.stroke();

                // Draw a center point to indicate the entity position
                gc.fill_color(canvas::Color::Rgba(1.0, 1.0, 1.0, 0.8));
                gc.new_path();
                gc.move_to(x - 2.0, y - 2.0);
                gc.line_to(x + 2.0, y - 2.0);
                gc.line_to(x + 2.0, y + 2.0);
                gc.line_to(x - 2.0, y + 2.0);
                gc.close_path();
                gc.fill();
            }
        });
    }

    pub fn present(&self) {
        // In flo_draw, drawing is automatically presented
        // No additional delay needed here
    }
}

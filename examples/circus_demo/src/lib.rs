//! Fifty thousand pills drifting through a curl noise field, among ancient
//! pillars, watched through a free flying camera.
//!
//! # Responsibilities
//!
//! - Register the renderer's data, this project's components and its tuning
//!   resource.
//! - Load the level's meshes and materials (`resources`), build the scene
//!   (`game`), and register the systems that move the pills and the camera,
//!   the one that draws the debug overlay (`debug_overlay`), and the tuning
//!   overlay (`ui`).
//!
//! # Design
//!
//! `init` runs again on every hot reload, on the same world. Every step it
//! takes is idempotent: imports hand back the assets already loaded, the scene
//! is only built into a world that does not hold it yet, and the tuning
//! resource is only inserted when no earlier generation left one behind.

// The crate's own name, so hot-patchable files (they cannot use `crate::`
// paths: a generated patch replays their imports into a synthetic crate that
// links this rlib) can name its types as `circus_demo::...`. The engine core
// uses the same self-alias for its derive expansions.
extern crate self as circus_demo;

// External crates
use pill_engine::{pill_project, Engine};

mod camera;
mod curl_noise_system;
mod debug_overlay;
mod game;
mod resources;
mod ui;

// The items the hot-patchable overlay names; reachable in a generated patch
// through its `use circus_demo::*`.
pub use game::{DemoState, PILL_COUNT};

/// Registers everything the demo uses, loads its assets and builds the scene.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    // Installs the renderer's PBR chain, whose shader every material here names.
    pill_master_renderer_data::register(engine);
    game::register(engine.world_mut());

    let assets = match resources::load(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            pill_engine::tracing::error!("asset loading failed: {error}");
            return 1;
        }
    };
    if let Err(error) = game::create_scene(engine.world_mut(), &assets) {
        pill_engine::tracing::error!("scene creation failed: {error}");
        return 1;
    }

    // The tuning overlay's egui context: installed once and kept across a
    // project reload, so the reloaded overlay finds the same fonts and the same
    // widget state (the shape `DemoState` uses). `install` claims both resource
    // types every generation - the reload releases exactly the resources the
    // new generation does not re-register - and inserts the values only when no
    // earlier generation left them behind. The frame slot it installs is also
    // what tells the renderer that this world's UI is the game's.
    pill_game_ui::install(engine);

    engine.register_system("fps_camera", camera::fps_camera_system);
    engine.register_system("camera_fov", camera::camera_fov_changing_system);
    engine.register_system("curl_noise", curl_noise_system::curl_noise_system);
    engine.register_system("demo_control", game::demo_control_system);
    engine.register_system("debug_overlay", debug_overlay::debug_overlay_system);
    engine.register_system("tuning_overlay", ui::tuning_overlay_system);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Query;
    use pill_master_renderer_data::{CameraComponent, MeshRendererComponent};

    /// Counts the entities that draw a mesh, and the cameras.
    fn scene_counts(engine: &mut Engine) -> (usize, usize) {
        let drawn = Query::<&MeshRendererComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        let cameras = Query::<&CameraComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        (drawn, cameras)
    }

    /// The level is the pillars, the ground and every pill, seen by one camera.
    #[test]
    fn initialization_builds_the_level() {
        let mut engine = Engine::new();

        assert_eq!(init(&mut engine), 0);

        assert_eq!(scene_counts(&mut engine), (game::PILL_COUNT + 2, 1));
    }

    /// A hot reload runs `init` again; it must not build a second level.
    #[test]
    fn initializing_again_keeps_one_level() {
        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);

        assert_eq!(init(&mut engine), 0);

        assert_eq!(scene_counts(&mut engine), (game::PILL_COUNT + 2, 1));
    }
}

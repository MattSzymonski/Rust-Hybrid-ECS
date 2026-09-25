//! Current-ECS port of the original Italian Brainrot example.
//!
//! The scene displays three rotating Chimpanzini Bananini models using lit,
//! unlit, and posterized materials. The original model, texture, shader sources,
//! configuration, and showcase media are kept under `res` and `media`.

use pill_engine::{pill_project, Engine, PillComponent};
use serde::{Deserialize, Serialize};

mod asset_loading;
mod scene;
mod systems;

/// Selects every model that the rotation system animates.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(shared = "italian_brainrot::TagAlphaComponent", persistable)]
pub struct TagAlphaComponent;

/// Registers the renderer, loads the bundled assets, and creates the scene.
#[pill_project]
pub fn init(engine: &mut Engine) -> u32 {
    pill_master_renderer::register(engine);
    __pill_register_TagAlphaComponent(engine.world_mut());

    let assets = match asset_loading::load(engine.world_mut()) {
        Ok(assets) => assets,
        Err(error) => {
            eprintln!("[italian_brainrot] asset loading failed: {error}");
            return 1;
        }
    };

    if let Err(error) = scene::create(engine.world_mut(), assets) {
        eprintln!("[italian_brainrot] scene creation failed: {error}");
        return 1;
    }

    engine.register_system("italian_brainrot_rotation", systems::rotation_system);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::Query;
    use pill_master_renderer::{CameraComponent, MeshRendererComponent};

    #[test]
    fn initialization_creates_the_original_three_model_scene() {
        let mut engine = Engine::new();
        assert_eq!(init(&mut engine), 0);

        let model_count = Query::<&MeshRendererComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        let camera_count = Query::<&CameraComponent>::new(engine.world_mut())
            .iter_mut()
            .count();
        assert_eq!(model_count, 3);
        assert_eq!(camera_count, 1);

        assert_eq!(init(&mut engine), 0);
        assert_eq!(
            Query::<&MeshRendererComponent>::new(engine.world_mut())
                .iter_mut()
                .count(),
            3
        );
    }
}

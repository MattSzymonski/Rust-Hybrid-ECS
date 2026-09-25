//! Creates the camera and the original three model variants.

use crate::{asset_loading::SceneAssets, TagAlphaComponent};
use pill_engine::{Query, World};
use pill_master_renderer::{CameraComponent, MeshRendererComponent, TransformComponent};

pub(crate) fn create(world: &mut World, assets: SceneAssets) -> Result<(), String> {
    if Query::<&CameraComponent>::new(world)
        .iter_mut()
        .next()
        .is_none()
    {
        world
            .create_entity()
            .with(CameraComponent::default())
            .with(TransformComponent {
                translation: [0.0, 0.0, 5.0],
                ..Default::default()
            })
            .build()
            .map_err(|error| error.to_string())?;
    }

    if Query::<&TagAlphaComponent>::new(world)
        .iter_mut()
        .next()
        .is_some()
    {
        return Ok(());
    }

    for (translation, material) in [
        ([-1.25, 0.0, 0.0], assets.lit),
        ([1.25, 0.0, 0.0], assets.unlit),
        ([0.0, 0.0, 1.5], assets.cartoon),
    ] {
        world
            .create_entity()
            .with(TransformComponent {
                translation,
                ..Default::default()
            })
            .with(
                MeshRendererComponent::builder()
                    .mesh(&assets.mesh)
                    .material(&material)
                    .build(),
            )
            .with(TagAlphaComponent)
            .build()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

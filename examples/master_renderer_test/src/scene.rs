//! Creates the camera and the helmet.

use crate::{asset_loading::SceneAssets, TagHelmet};
use pill_engine::{Query, World};
use pill_master_renderer::{CameraComponent, MeshRendererComponent, TransformComponent};

/// Adds the camera if the world has none, then the helmet if it has none.
///
/// Both halves bail when their entity already exists, so a re-run after a reload
/// leaves the live scene alone instead of stacking a second helmet in it.
///
/// # Errors
///
/// Returns the engine's entity-creation error as text.
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
                translation: [0.0, 0.0, 2.5],
                ..Default::default()
            })
            .build()
            .map_err(|error| error.to_string())?;
    }

    if Query::<&TagHelmet>::new(world).iter_mut().next().is_some() {
        return Ok(());
    }

    // The sample is modelled at real-world scale (a helmet is about 0.4 m), so
    // it is scaled up to fill a camera that stands 2.5 units away.
    world
        .create_entity()
        .with(TransformComponent {
            translation: [0.0, 0.0, 0.0],
            scale: [3.0, 3.0, 3.0],
            ..Default::default()
        })
        .with(
            MeshRendererComponent::builder()
                .mesh(&assets.mesh)
                .material(&assets.material)
                .build(),
        )
        .with(TagHelmet)
        .build()
        .map_err(|error| error.to_string())?;
    Ok(())
}

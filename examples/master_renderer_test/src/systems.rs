//! The helmet's spin.

use crate::TagHelmet;
use pill_engine::{pill_hot, Query, Res, SystemError, Time};
use pill_master_renderer::TransformComponent;

/// One full turn every eight seconds, applied per frame so the speed does not
/// depend on the frame rate.
const TURN_RADIANS_PER_SECOND: f32 = std::f32::consts::TAU / 8.0;

/// Rotates the helmet around its local Y axis.
#[pill_hot]
pub(crate) fn rotation_system(
    time: Res<Time>,
    mut helmets: Query<(&mut TransformComponent, &TagHelmet)>,
) -> Result<(), SystemError> {
    let Some(time) = time.get() else {
        return Ok(());
    };
    let step = glam::Quat::from_rotation_y(TURN_RADIANS_PER_SECOND * time.delta_seconds());
    for (mut transform, _) in helmets.iter_mut() {
        let current = glam::Quat::from_array(transform.rotation);
        let current = if current.is_finite() && current.length_squared() > 1.0e-8 {
            current.normalize()
        } else {
            glam::Quat::IDENTITY
        };
        transform.rotation = (step * current).normalize().to_array();
    }
    Ok(())
}

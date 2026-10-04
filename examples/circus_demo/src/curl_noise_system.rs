//! Moves every pill through an animated curl noise field.
//!
//! # Responsibilities
//!
//! - Sample the curl of a noise vector potential at each pill, pull the pill
//!   towards the middle of the field, damp and clamp its velocity, and keep it
//!   inside the field's box ([`curl_noise_system`]).
//!
//! # Design
//!
//! Bridson, Hourihan and Nordenstam's curl noise ("Curl-Noise for Procedural
//! Fluid Flow", SIGGRAPH 2007): take a vector potential Ψ built from three
//! decorrelated noise fields and move along its curl ∇ × Ψ, which is
//! divergence free, so the pills swirl without bunching into sinks. Each pill
//! depends only on its own state, so the query runs in parallel.

// External crates
use glam::Vec3;
use pill_engine::{pill_hot, Query, Res, SystemError, Time};
use pill_master_renderer_data::TransformComponent;

// Current crate
use crate::game::{CurlNoiseComponent, DemoState};

/// The box the pills are kept inside.
const BOX_MINIMUM: Vec3 = Vec3::new(-120.0, -50.0, -120.0);
const BOX_MAXIMUM: Vec3 = Vec3::new(120.0, 140.0, 120.0);

/// The point the pills are pulled towards. The original demo computed it as
/// the box's corner sum over twelve, which puts it at (0, 7.5, 0); the look is
/// tuned to that point.
const ATTRACTION_CENTER: Vec3 = Vec3::new(
    (BOX_MINIMUM.x + BOX_MAXIMUM.x) / 12.0,
    (BOX_MINIMUM.y + BOX_MAXIMUM.y) / 12.0,
    (BOX_MINIMUM.z + BOX_MAXIMUM.z) / 12.0,
);

/// Fastest a pill may move, in world units per second.
const MAXIMUM_SPEED: f32 = 20.0;

/// How fast the noise field scrolls, per second, along every axis.
const FIELD_SCROLL_SPEED: f32 = 0.4;

/// Pseudo random value in `[0, 1)` for a lattice point.
fn hash(point: Vec3) -> f32 {
    let value = (point.x * 127.1 + point.y * 311.7 + point.z * 74.7).sin();
    (value * 43_758.547).fract()
}

/// The eight lattice hashes around one cell of the noise lattice.
///
/// Hashing is the expensive part of the noise (each hash is a `sin`), and the
/// curl samples six points within a few thousandths of each other, which
/// nearly always share a cell. Keeping a cell's hashes lets every sample in it
/// reuse them instead of hashing the same eight corners again.
#[derive(Clone, Copy)]
struct LatticeCell {
    /// The cell's lowest corner: the sample point rounded down.
    origin: Vec3,
    /// Corner hashes indexed by `x + 2y + 4z`, each axis 0 or 1.
    corners: [f32; 8],
}

impl LatticeCell {
    /// Hashes the eight corners of the cell `origin` names.
    fn at(origin: Vec3) -> Self {
        let corner = |x: f32, y: f32, z: f32| hash(origin + Vec3::new(x, y, z));
        Self {
            origin,
            corners: [
                corner(0.0, 0.0, 0.0),
                corner(1.0, 0.0, 0.0),
                corner(0.0, 1.0, 0.0),
                corner(1.0, 1.0, 0.0),
                corner(0.0, 0.0, 1.0),
                corner(1.0, 0.0, 1.0),
                corner(0.0, 1.0, 1.0),
                corner(1.0, 1.0, 1.0),
            ],
        }
    }

    /// Smooth value noise at `point`, which lies in this cell: the corner
    /// hashes blended with a smoothstep in each axis.
    fn noise(&self, point: Vec3) -> f32 {
        let offset = point - self.origin;
        let blend = offset * offset * (Vec3::splat(3.0) - 2.0 * offset);
        let lerp = |from: f32, to: f32, amount: f32| from + (to - from) * amount;
        let corners = &self.corners;

        let bottom_front = lerp(corners[0], corners[1], blend.x);
        let top_front = lerp(corners[2], corners[3], blend.x);
        let bottom_back = lerp(corners[4], corners[5], blend.x);
        let top_back = lerp(corners[6], corners[7], blend.x);

        let front = lerp(bottom_front, top_front, blend.y);
        let back = lerp(bottom_back, top_back, blend.y);
        lerp(front, back, blend.z)
    }
}

/// Offsets that decorrelate the three noise fields of the vector potential Ψ.
const FIELD_OFFSETS: [Vec3; 3] = [
    Vec3::ZERO,
    Vec3::new(31.416, 0.0, 0.0),
    Vec3::new(0.0, 67.254, 0.0),
];

/// The vector potential Ψ at `point`: three noise fields, offset to
/// decorrelate them. `cells` holds a cell per field from a nearby point; a
/// field whose point lies in it reuses its hashes, any other hashes its own.
fn potential(point: Vec3, cells: &[LatticeCell; 3]) -> Vec3 {
    let field = |index: usize| {
        let field_point = point + FIELD_OFFSETS[index];
        let origin = field_point.floor();
        if origin == cells[index].origin {
            cells[index].noise(field_point)
        } else {
            LatticeCell::at(origin).noise(field_point)
        }
    };
    Vec3::new(field(0), field(1), field(2))
}

/// The curl of Ψ at `point`, by central differences, with the field scrolled
/// by `time`:
/// ∇ × Ψ = (∂ψ3/∂y - ∂ψ2/∂z, ∂ψ1/∂z - ∂ψ3/∂x, ∂ψ2/∂x - ∂ψ1/∂y).
fn curl_noise(point: Vec3, time: f32, epsilon: f32, scale: f32) -> Vec3 {
    let animated = point * scale + Vec3::splat(time * FIELD_SCROLL_SPEED);
    // The cell of each field around the centre point. The six samples are
    // within `epsilon` of it, so they nearly always fall in these cells and
    // the whole curl hashes 24 corners instead of 144.
    let cells = FIELD_OFFSETS.map(|offset| LatticeCell::at((animated + offset).floor()));

    // The derivative of every component of Ψ along one axis.
    let derivative = |axis: Vec3| {
        (potential(animated + axis * epsilon, &cells)
            - potential(animated - axis * epsilon, &cells))
            / (2.0 * epsilon)
    };
    let along_x = derivative(Vec3::X);
    let along_y = derivative(Vec3::Y);
    let along_z = derivative(Vec3::Z);

    Vec3::new(
        along_y.z - along_z.y,
        along_z.x - along_x.z,
        along_x.y - along_y.x,
    )
}

/// The next position and velocity of one pill.
fn step_pill(
    position: Vec3,
    velocity: Vec3,
    curl_strength: f32,
    demo_state: &DemoState,
    time: f32,
    delta_time: f32,
) -> (Vec3, Vec3) {
    let curl = curl_noise(
        position,
        time,
        demo_state.curl_epsilon,
        demo_state.curl_scale,
    );
    let mut force = curl * curl_strength;

    // A constant strength pull towards the middle of the field.
    let to_center = ATTRACTION_CENTER - position;
    let distance_to_center = to_center.length();
    if distance_to_center > 0.1 {
        force += to_center * (demo_state.curl_attraction / distance_to_center);
    }

    // The original demo keeps a third of the previous velocity before damping.
    let velocity = (velocity / 3.0 + force * delta_time) * demo_state.curl_damping;
    let velocity = velocity.clamp_length_max(MAXIMUM_SPEED);

    let position = (position + velocity * delta_time).clamp(BOX_MINIMUM, BOX_MAXIMUM);
    (position, velocity)
}

/// Moves every pill one frame through the field, in parallel.
#[pill_hot]
pub(crate) fn curl_noise_system(
    time: Res<Time>,
    demo_state: Res<DemoState>,
    mut pills: Query<(&mut TransformComponent, &mut CurlNoiseComponent)>,
) -> Result<(), SystemError> {
    let (Some(time), Some(demo_state)) = (time.get(), demo_state.get()) else {
        return Ok(());
    };
    let delta_time = time.delta_seconds();
    let elapsed = time.elapsed_seconds();
    let demo_state: &DemoState = demo_state;

    pills
        .par_iter_mut()
        .label("circus_demo::curl_noise")
        .for_each(|(mut transform, mut pill)| {
            let (position, velocity) = step_pill(
                Vec3::from_array(transform.translation),
                Vec3::from_array(pill.velocity),
                pill.curl_strength,
                demo_state,
                elapsed,
                delta_time,
            );
            transform.translation = position.to_array();
            pill.velocity = velocity.to_array();
        });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pills never leave the box, however hard the field pushes.
    #[test]
    fn a_pill_stays_inside_the_box() {
        let demo_state = DemoState::default();
        let mut position = BOX_MAXIMUM - Vec3::splat(0.01);
        let mut velocity = Vec3::splat(1_000.0);
        for frame in 0..600 {
            (position, velocity) = step_pill(
                position,
                velocity,
                120.0,
                &demo_state,
                frame as f32 / 60.0,
                1.0 / 60.0,
            );
            assert!(position.cmpge(BOX_MINIMUM).all() && position.cmple(BOX_MAXIMUM).all());
            assert!(velocity.length() <= MAXIMUM_SPEED + 1.0e-3);
        }
    }

    /// The curl of a potential is divergence free; finite differences only
    /// approximate that, so the check is loose.
    #[test]
    fn the_field_has_almost_no_divergence() {
        let epsilon = 0.01;
        let point = Vec3::new(1.3, -0.7, 2.1);
        let field = |offset: Vec3| curl_noise(point + offset, 0.0, epsilon, 1.0);
        let step = 0.01;
        let divergence = (field(Vec3::X * step).x - field(-Vec3::X * step).x
            + field(Vec3::Y * step).y
            - field(-Vec3::Y * step).y
            + field(Vec3::Z * step).z
            - field(-Vec3::Z * step).z)
            / (2.0 * step);
        let magnitude = field(Vec3::ZERO).length().max(1.0);
        assert!(divergence.abs() < magnitude, "divergence {divergence}");
    }

    /// The noise as it was first written: every sample hashes its own eight
    /// corners. Kept as the reference the shared-cell version must match.
    mod reference {
        use super::*;

        fn noise3d(point: Vec3) -> f32 {
            let cell = point.floor();
            let offset = point - cell;
            let blend = offset * offset * (Vec3::splat(3.0) - 2.0 * offset);

            let corner = |x: f32, y: f32, z: f32| hash(cell + Vec3::new(x, y, z));
            let lerp = |from: f32, to: f32, amount: f32| from + (to - from) * amount;

            let bottom_front = lerp(corner(0.0, 0.0, 0.0), corner(1.0, 0.0, 0.0), blend.x);
            let top_front = lerp(corner(0.0, 1.0, 0.0), corner(1.0, 1.0, 0.0), blend.x);
            let bottom_back = lerp(corner(0.0, 0.0, 1.0), corner(1.0, 0.0, 1.0), blend.x);
            let top_back = lerp(corner(0.0, 1.0, 1.0), corner(1.0, 1.0, 1.0), blend.x);

            let front = lerp(bottom_front, top_front, blend.y);
            let back = lerp(bottom_back, top_back, blend.y);
            lerp(front, back, blend.z)
        }

        fn potential(point: Vec3) -> Vec3 {
            Vec3::new(
                noise3d(point),
                noise3d(point + Vec3::new(31.416, 0.0, 0.0)),
                noise3d(point + Vec3::new(0.0, 67.254, 0.0)),
            )
        }

        /// The curl as the original computed it, sample by sample.
        pub(super) fn curl_noise(point: Vec3, time: f32, epsilon: f32, scale: f32) -> Vec3 {
            let animated = point * scale + Vec3::splat(time * FIELD_SCROLL_SPEED);
            let derivative = |axis: Vec3| {
                (potential(animated + axis * epsilon) - potential(animated - axis * epsilon))
                    / (2.0 * epsilon)
            };
            let along_x = derivative(Vec3::X);
            let along_y = derivative(Vec3::Y);
            let along_z = derivative(Vec3::Z);
            Vec3::new(
                along_y.z - along_z.y,
                along_z.x - along_x.z,
                along_x.y - along_y.x,
            )
        }
    }

    /// Sharing a cell's hashes between samples changes no result: not for
    /// points well inside a cell, nor for ones a sample's step carries into
    /// the next cell, at the tuned epsilon or a much larger one.
    #[test]
    fn shared_cell_hashes_give_exactly_the_original_field() {
        let demo_state = DemoState::default();
        let scale = demo_state.curl_scale;
        let mut checked = 0;
        for index in 0..4_000 {
            // A spread of points across the box, at a spread of times.
            let fraction = index as f32 / 4_000.0;
            let point = BOX_MINIMUM
                + (BOX_MAXIMUM - BOX_MINIMUM)
                    * Vec3::new(
                        (fraction * 7.13).fract(),
                        (fraction * 3.71).fract(),
                        (fraction * 5.37).fract(),
                    );
            let time = fraction * 90.0;
            for epsilon in [demo_state.curl_epsilon, 0.25] {
                assert_eq!(
                    curl_noise(point, time, epsilon, scale),
                    reference::curl_noise(point, time, epsilon, scale),
                    "point {point:?}, time {time}, epsilon {epsilon}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 8_000);
    }

    /// A point just below a cell boundary: its +epsilon sample lands in the
    /// next cell, which has to hash its own corners.
    #[test]
    fn a_sample_across_a_cell_boundary_matches_the_original() {
        let epsilon = DemoState::default().curl_epsilon;
        // With scale 1 and time 0 the noise point is the point itself.
        for boundary_point in [
            Vec3::new(4.0 - epsilon * 0.5, 0.3, 0.6),
            Vec3::new(0.2, -2.0 - epsilon * 0.5, 0.7),
            Vec3::new(0.5, 0.5, 9.0 - epsilon * 0.5),
        ] {
            assert_eq!(
                curl_noise(boundary_point, 0.0, epsilon, 1.0),
                reference::curl_noise(boundary_point, 0.0, epsilon, 1.0)
            );
        }
    }
}

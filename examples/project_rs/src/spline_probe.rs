//! Reports spline visibility and deterministic samples for reload diagnostics.

use crate::physics::ball_spawn_state;
use crate::settings::BALL_COUNT;
use pill_core::math::Vector3f;
use pill_engine::*;
use pill_spline::Spline;
use std::time::{Duration, Instant};

/// Timestamps the last spline probe report so the cadence is wall-clock.
pub(crate) struct SplineProbeState {
    pub(crate) last_report: Instant,
}

impl Resource for SplineProbeState {}

/// How often the probe reports, on the wall clock.
///
/// The demo runs uncapped, so a per-frame report would drown the log and a
/// frame-count interval would make the cadence - and every suite waiting on a
/// report - depend on the machine's frame rate. Wall-clock time is what a
/// human reading the log observes, and what the waits can bound.
const SPLINE_REPORT_INTERVAL: Duration = Duration::from_millis(250);

/// Reports how many splines the project can see, and samples the curve.
///
/// The count is what reveals whether a separately loaded copy of `pill_spline`
/// shares the component type: `Spline` is `#[pill(shared)]`, so the module
/// DLL's copy and the project's are one column, and this line keeps saying
/// `1 spline(s)` instead of each artifact seeding a curve of its own.
///
/// # Errors
///
/// Returns [`SystemError::MissingResource`] when the resource is absent, which
/// means the project module and host disagree about initialization.
pub(crate) fn spline_probe_system(
    mut state: ResMut<SplineProbeState>,
    mut splines: Query<&mut Spline>,
) -> Result<(), SystemError> {
    let Some(mut state) = state.get_mut() else {
        return Err(SystemError::MissingResource {
            name: String::from("SplineProbeState"),
        });
    };
    if state.last_report.elapsed() < SPLINE_REPORT_INTERVAL {
        return Ok(());
    }
    state.last_report = Instant::now();

    let mut visible_spline_count = 0;
    for spline in splines.iter_mut() {
        visible_spline_count += 1;
        let _ = spline;
    }

    // The sampled point comes from a reference spline over the spawn geometry,
    // not from the live curve: the live control points are the ball centres,
    // which move every frame, and an integration suite cannot wait for a moving
    // number. The spawn geometry is fixed, so this value depends only on the
    // linked module's math - which is what the cascade suites watch change.
    let mut spawn_points = [Vector3f::ZERO; BALL_COUNT];
    for (index, point) in spawn_points.iter_mut().enumerate() {
        let ball = ball_spawn_state(index);
        *point = Vector3f::new(ball.position_x, ball.position_y, 0.0);
    }
    let reference = Spline::from_points(&spawn_points);
    let midpoint = reference.get_location_at(0.5);
    let color = reference.get_color_a();
    // Printed rather than logged through `tracing`: the project links its own
    // copy of `pill_core`, so its tracing dispatcher has no subscriber and log
    // lines emitted here never reach the host's telemetry.
    println!(
        "[project] 12xxsees {visible_spline_count} spline(s), midpoint ({:.1}, {:.1}), colorx {:.2}",
        midpoint.x, midpoint.y, color
    );

    Ok(())
}

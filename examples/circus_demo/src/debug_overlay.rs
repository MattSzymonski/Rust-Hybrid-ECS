//! The demo's debug overlay: the field's bounds, the world axes and the
//! tuning values, drawn through the engine's `DebugDraw`.
//!
//! # Responsibilities
//!
//! - Draw the volume the pills spawn in, the axes at the origin and labels
//!   naming them, every frame ([`debug_overlay_system`]).
//! - Toggle debug drawing at runtime with F3, in any build.

// External crates
use pill_engine::{pill_hot, DebugDraw, DebugStyle, Input, KeyCode, Res, ResMut, SystemError};

// Current crate
use crate::game::DemoState;

/// The key that switches debug drawing on and off.
const TOGGLE_KEY: KeyCode = KeyCode::F3;

/// The volume pills spawn in: its center and half its size on each axis.
const FIELD_CENTER: [f32; 3] = [0.0, 20.0, 0.0];
const FIELD_HALF_EXTENTS: [f32; 3] = [20.0, 20.0, 20.0];

/// Length of each drawn world axis.
const AXIS_LENGTH: f32 = 3.0;

/// Draws the field's bounds, the world axes and the tuning values, and
/// toggles debug drawing with F3.
#[pill_hot]
pub(crate) fn debug_overlay_system(
    input: Res<Input>,
    mut debug_draw: ResMut<DebugDraw>,
    demo_state: Res<DemoState>,
) -> Result<(), SystemError> {
    // A build without the resource has nothing to draw into.
    let Some(mut debug_draw) = debug_draw.get_mut() else {
        return Ok(());
    };
    if input
        .get()
        .is_some_and(|input| input.key_pressed(TOGGLE_KEY))
    {
        let enabled = !debug_draw.is_enabled();
        debug_draw.set_enabled(enabled);
    }
    if !debug_draw.is_enabled() {
        return Ok(());
    }

    debug_draw.aabb(
        FIELD_CENTER,
        FIELD_HALF_EXTENTS,
        DebugStyle::colored([0.3, 0.8, 1.0, 1.0]),
    );
    // The axes show through everything, so they stay findable inside the
    // swarm.
    let on_top = |color| DebugStyle {
        depth_test: false,
        ..DebugStyle::colored(color)
    };
    debug_draw
        .arrow(
            [0.0; 3],
            [AXIS_LENGTH, 0.0, 0.0],
            on_top([1.0, 0.2, 0.2, 1.0]),
        )
        .arrow(
            [0.0; 3],
            [0.0, AXIS_LENGTH, 0.0],
            on_top([0.2, 1.0, 0.2, 1.0]),
        )
        .arrow(
            [0.0; 3],
            [0.0, 0.0, AXIS_LENGTH],
            on_top([0.3, 0.5, 1.0, 1.0]),
        )
        .label_world([0.0, 0.0, 0.0], "origin", on_top([1.0; 4]))
        .label_world(
            [
                FIELD_CENTER[0] - FIELD_HALF_EXTENTS[0],
                FIELD_CENTER[1] + FIELD_HALF_EXTENTS[1],
                FIELD_CENTER[2] - FIELD_HALF_EXTENTS[2],
            ],
            "curl noise field",
            on_top([0.3, 0.8, 1.0, 1.0]),
        );

    let status = match demo_state.get() {
        Some(state) => format!(
            "circus demo - F3: debug draw\ncurl scale {:.4}  attraction {:.2}\ndamping {:.4}  epsilon {:.4}",
            state.curl_scale, state.curl_attraction, state.curl_damping, state.curl_epsilon
        ),
        None => String::from("circus demo - F3: debug draw"),
    };
    debug_draw.label_screen(16.0, 16.0, &status, on_top([1.0, 0.9, 0.4, 1.0]));
    Ok(())
}

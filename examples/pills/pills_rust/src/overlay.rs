//! The demo's two overlays: the egui tuning panel, drawn through the engine's
//! game UI, and the debug drawing.
//!
//! # Responsibilities
//!
//! - [`tuning_overlay_system`]: one egui pass per frame - the FPS readout, the
//!   tuning values and the button that resets them.
//! - [`debug_overlay_system`]: the field's box, the world axes and a status
//!   label, toggled with F3 in any build.
//!
//! # Design
//!
//! Both are ordinary project code, so an edit to either live-patches the
//! running demo. They are deliberately self-contained - no `crate::` imports
//! and no file-level constants - because a generated fast patch replays a hot
//! file's imports into a synthetic crate that links this rlib.
//!
//! The egui pass is built into the [`GameUi`] resource rather than into a fresh
//! context, which is what makes the panel survive a project reload: the
//! context's fonts, its widget state and its textures live in a shared library,
//! so the reloaded project finds them exactly as they were. [`DemoState`] is
//! the one writer of the tuning values; the panel displays them and the
//! reset button restores the defaults.

// External crates
use pill_core::math::Vector2f;
use pill_engine::{
    pill_hot, DebugDraw, DebugStyle, Input, KeyCode, Res, ResMut, SystemError, Time,
};
// The egui this engine pins, named through the crate that owns it: a project
// never depends on egui directly, so every artifact shares the one copy.
use pill_project_ui::{egui, GameUi, InputFrame, UiFrameSlot, PRIMARY_SURFACE};

// Current crate
use pills_rust::{DemoState, PILL_COUNT};

/// Draws the tuning panel for this frame.
#[pill_hot]
pub(crate) fn tuning_overlay_system(
    mut game_ui: ResMut<GameUi>,
    input: Res<Input>,
    mut slot: ResMut<UiFrameSlot>,
    time: Res<Time>,
    mut state: ResMut<DemoState>,
) -> Result<(), SystemError> {
    let (Some(mut game_ui), Some(input), Some(mut slot), Some(time), Some(mut state)) = (
        game_ui.get_mut(),
        input.get(),
        slot.get_mut(),
        time.get(),
        state.get_mut(),
    ) else {
        return Ok(());
    };

    // The engine's input snapshot, captured before the pass: `run` needs the UI
    // resource mutably, so the snapshot is taken first and the borrow of the
    // engine's input ends with it.
    let input_frame = InputFrame::capture(input);
    // The tokens the panel's widgets paint from, cloned out of the resource
    // because the pass borrows the UI mutably. A reload installs the theme
    // again, so the clone is this frame's.
    let theme = game_ui.theme().clone();

    // The panel lays out against its own size and sits in the window's top-left
    // corner, so it needs no window-size input; the renderer applies the
    // monitor's scale factor, which is why 1.0 is the honest value here.
    game_ui.run(
        &mut slot,
        &input_frame,
        Vector2f::new(320.0, 260.0),
        1.0,
        PRIMARY_SURFACE,
        |ui| {
            egui::Window::new("Pills")
                .default_pos(egui::pos2(12.0, 12.0))
                .resizable(false)
                .collapsible(false)
                .show(ui.ctx(), |window| {
                    window.label(format!(
                        "{:.0} FPS | {} pills",
                        1.0 / time.delta_seconds().max(1e-6),
                        PILL_COUNT
                    ));
                    window.separator();
                    window.collapsing("Curl noise", |section| {
                        section.label(format!("Scale {:.4}", state.curl_scale));
                        section.label(format!("Attraction {:.0}", state.curl_attraction));
                        section.label(format!("Damping {:.2}", state.curl_damping));
                        section.label(format!("Epsilon {:.4}", state.curl_epsilon));
                    });
                    window.separator();
                    window.label("O/P scale  L/K attraction  M/N damping  V/B epsilon");
                    if window
                        .add(
                            pill_project_ui::kit::ChunkyButton::new("Reset curls", &theme)
                                .accent(true),
                        )
                        .clicked()
                    {
                        *state = DemoState::default();
                    }
                });
        },
    );
    Ok(())
}

/// Draws the field's bounds, the world axes and the tuning values, and toggles
/// debug drawing with F3.
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
        .is_some_and(|input| input.key_pressed(KeyCode::F3))
    {
        let enabled = !debug_draw.is_enabled();
        debug_draw.set_enabled(enabled);
    }
    if !debug_draw.is_enabled() {
        return Ok(());
    }

    // The volume the pills spawn in.
    let (field_center, field_half_extents) = ([0.0, 20.0, 0.0], [20.0, 20.0, 20.0]);
    debug_draw.aabb(
        field_center,
        field_half_extents,
        DebugStyle::colored([0.3, 0.8, 1.0, 1.0]),
    );

    // The axes show through everything, so they stay findable inside the swarm.
    let on_top = |color| DebugStyle {
        depth_test: false,
        ..DebugStyle::colored(color)
    };
    debug_draw
        .arrow([0.0; 3], [3.0, 0.0, 0.0], on_top([1.0, 0.2, 0.2, 1.0]))
        .arrow([0.0; 3], [0.0, 3.0, 0.0], on_top([0.2, 1.0, 0.2, 1.0]))
        .arrow([0.0; 3], [0.0, 0.0, 3.0], on_top([0.3, 0.5, 1.0, 1.0]))
        .label_world([0.0; 3], "origin", on_top([1.0; 4]))
        .label_world(
            [
                field_center[0] - field_half_extents[0],
                field_center[1] + field_half_extents[1],
                field_center[2] - field_half_extents[2],
            ],
            "curl noise field",
            on_top([0.3, 0.8, 1.0, 1.0]),
        );

    if let Some(state) = demo_state.get() {
        let status = format!(
            "pills - F3: debug draw\ncurl scale {:.4}  attraction {:.2}\ndamping {:.4}  epsilon {:.4}",
            state.curl_scale, state.curl_attraction, state.curl_damping, state.curl_epsilon
        );
        debug_draw.label_screen(16.0, 16.0, &status, on_top([1.0, 0.9, 0.4, 1.0]));
    }
    Ok(())
}

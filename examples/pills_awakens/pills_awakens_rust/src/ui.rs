//! The tuning overlay: the demo's in-game egui UI, drawn through the engine.
//!
//! # Responsibilities
//!
//! - [`tuning_overlay_system`]: one egui pass per frame - the FPS readout and
//!   the curl-noise tuning values, as a panel in the window's top-left corner.
//!
//! # Design
//!
//! The overlay is ordinary project code: a `#[pill_hot]` system, so an edit to
//! this function live-patches the running demo. It is self-contained on
//! purpose: no `crate::` imports and no file-level constants. A fast patch
//! copies only the function into a small generated crate that repeats this
//! file's imports, so either shape would fail to compile there and turn a
//! quick patch into a full reload.
//!
//! The pass is built into the [`GameUi`] resource rather than into a fresh
//! context. That is what makes the overlay survive a project reload. The
//! context's fonts, its widget state (collapsed sections, focus) and its
//! textures live in a shared library, so the reloaded project finds them
//! exactly as they were. The demo's setup installs that resource and registers
//! its type; this system draws into it.
//!
//! The tuning values live in [`DemoState`]. The demo's own input system changes
//! them from the tuning keys, and the overlay displays them and resets them to
//! their defaults when the player presses Reset curls.
//!
//! The frame reaches the renderer as `pill_ui_frame` data - a draw list and a
//! texture set - so nothing in the rendering path names egui.

// External crates
use pill_core::math::Vector2f;
use pill_engine::{pill_hot, Input, Res, ResMut, SystemError, Time};
// The egui version this engine pins, named through the crate that owns it. A
// project never depends on egui directly, so every artifact shares the one
// copy.
use pill_project_ui::{egui, GameUi, InputFrame, UiFrameSlot, PRIMARY_SURFACE};

// Current crate
// The crate's own items, named through its package name rather than `crate::`.
// A generated hot patch repeats this file's imports in a separate crate, where
// only `pills_awakens_rust::...` resolves; that crate links this project's
// library.
use pills_awakens_rust::{DemoState, PILL_COUNT};

/// Draws the tuning overlay for this frame.
#[pill_hot]
pub(crate) fn tuning_overlay_system(
    mut game_ui: ResMut<GameUi>,
    input: Res<Input>,
    mut ui_frame_slot: ResMut<UiFrameSlot>,
    time: Res<Time>,
    mut demo_state: ResMut<DemoState>,
) -> Result<(), SystemError> {
    let (Some(mut game_ui), Some(input), Some(mut ui_frame_slot), Some(time), Some(mut demo_state)) = (
        game_ui.get_mut(),
        input.get(),
        ui_frame_slot.get_mut(),
        time.get(),
        demo_state.get_mut(),
    ) else {
        return Ok(());
    };

    // Step 1: Take the engine's input snapshot. `run` needs the UI resource
    // mutably, so the snapshot is taken first, and the borrow of the engine's
    // input ends here.
    let input_frame = InputFrame::capture(input);

    // Step 2: Clone the theme tokens the panel's widgets paint from. They are
    // cloned out of the resource because `run` needs the UI mutably, and this
    // copy is used only in this pass.
    let theme = game_ui.theme().clone();

    // Step 3: Run the pass. The overlay lays out in its own 320x260 area and
    // sits in the window's top-left corner, so it needs no window-size input.
    // A scale factor of 1.0 makes one logical pixel one physical pixel, so the
    // panel is not enlarged for a high-DPI monitor.
    game_ui.run(
        &mut ui_frame_slot,
        &input_frame,
        Vector2f::new(320.0, 260.0),
        1.0,
        PRIMARY_SURFACE,
        |ui| {
            egui::Window::new("Pills Awakens")
                .default_pos(egui::pos2(12.0, 12.0))
                .resizable(false)
                .collapsible(false)
                .show(ui.ctx(), |window_ui| {
                    window_ui.label(format!(
                        "{:.0} FPS | {} pills",
                        1.0 / time.delta_seconds().max(1e-6),
                        PILL_COUNT
                    ));
                    window_ui.label(format!("{:.0}s uptime", time.elapsed_seconds()));
                    window_ui.separator();
                    window_ui.collapsing("Curl noise", |section| {
                        section.label(format!("Scale {:.4}", demo_state.curl_scale));
                        section.label(format!("Attraction {:.0}", demo_state.curl_attraction));
                        section.label(format!("Damping {:.2}", demo_state.curl_damping));
                        section.label(format!("Epsilon {:.4}", demo_state.curl_epsilon));
                    });
                    window_ui.separator();
                    // The brutalist chunky button: a flat face on a hard
                    // shadow, and the face moves onto the shadow while the
                    // button is held. One of each variant - the accent one
                    // acts, the selected one marks the tuning as live, and the
                    // disabled one shows the inert look.
                    window_ui.horizontal(|row| {
                        if row
                            .add(
                                pill_project_ui::kit::ChunkyButton::new("Reset curls", &theme)
                                    .accent(true),
                            )
                            .clicked()
                        {
                            let default_demo_state = DemoState::default();
                            demo_state.curl_scale = default_demo_state.curl_scale;
                            demo_state.curl_attraction = default_demo_state.curl_attraction;
                            demo_state.curl_damping = default_demo_state.curl_damping;
                            demo_state.curl_epsilon = default_demo_state.curl_epsilon;
                        }
                        row.add(
                            pill_project_ui::kit::ChunkyButton::new("Tuning on", &theme)
                                .selected(true),
                        );
                        row.add_enabled(
                            false,
                            pill_project_ui::kit::ChunkyButton::new("Locked", &theme),
                        );
                    });
                    window_ui.label("Tune: O/P, K/L, M/N, B/V");
                });
        },
    );
    Ok(())
}

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
//! this function live-patches the running demo. It is deliberately
//! self-contained - no `crate::` imports and no file-level constants - because
//! a generated fast patch replays a hot file's imports into a synthetic crate
//! and copies only the function: either shape would fail to compile there and
//! cost the edit a full reload.
//!
//! The pass is built into the [`GameUi`] resource rather than into a fresh
//! context, which is what makes the overlay survive a project reload: the
//! context's fonts, its widget state (collapsed sections, focus) and its
//! textures live in a shared library, so the reloaded project finds them
//! exactly as they were. `init` installs that resource and registers its type;
//! this system only draws.
//!
//! What the UI reads and what the player tunes are separate: the values live in
//! [`DemoState`], which the demo's own input system changes from the tuning
//! keys, and the overlay displays them. That keeps one writer for the tuning
//! values and lets the overlay take them immutably.
//!
//! The frame reaches the renderer as `pill_ui_frame` data - a draw list and a
//! texture set - so nothing in the rendering path names egui (W-0028 D1).

// External crates
use pill_core::math::Vector2f;
use pill_engine::{pill_hot, Input, Res, ResMut, SystemError, Time};
// The egui this engine pins, named through the crate that owns it: a project
// never depends on egui directly, so every artifact shares the one copy.
use pill_project_ui::{egui, GameUi, InputFrame, UiFrameSlot, PRIMARY_SURFACE};

// The crate's own items, named through its package name rather than `crate::`:
// a generated hot patch replays this file's imports into a synthetic crate,
// where only `circus_demo::...` resolves (it links this rlib).
use circus_demo::{DemoState, PILL_COUNT};

/// Draw the tuning overlay for this frame.
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

    // The tokens the panel's widgets paint from: cloned out of the resource,
    // because the pass itself borrows the UI mutably. A project reload installs
    // the theme again, so the clone is this frame's.
    let theme = game_ui.theme().clone();

    // The overlay lays out against its own size and sits in the window's
    // top-left corner, so it needs no window-size input; the monitor's scale
    // factor is applied by the renderer, which is why 1.0 is the honest value
    // here.
    game_ui.run(
        &mut slot,
        &input_frame,
        Vector2f::new(320.0, 260.0),
        1.0,
        PRIMARY_SURFACE,
        |ui| {
            egui::Window::new("Circus Demo")
                .default_pos(egui::pos2(12.0, 12.0))
                .resizable(false)
                .collapsible(false)
                .show(ui.ctx(), |window| {
                    window.label(format!(
                        "{:.0} FPS | {} pills",
                        1.0 / time.delta_seconds().max(1e-6),
                        PILL_COUNT
                    ));
                    window.label(format!("{:.0}s uptime", time.elapsed_seconds()));
                    window.separator();
                    window.collapsing("Curl noise", |section| {
                        section.label(format!("Scale {:.4}", state.curl_scale));
                        section.label(format!("Attraction {:.0}", state.curl_attraction));
                        section.label(format!("Damping {:.2}", state.curl_damping));
                        section.label(format!("Epsilon {:.4}", state.curl_epsilon));
                    });
                    window.separator();
                    // The brutalist recipe's own button (W-0127 Stage 8): a
                    // face on a hard shadow it drops onto while held. One of
                    // each face - the accent one acts, the selected one marks
                    // the tuning as live, and the disabled one shows the
                    // inert look.
                    window.horizontal(|row| {
                        if row
                            .add(
                                pill_project_ui::kit::ChunkyButton::new("Reset curls", &theme)
                                    .accent(true),
                            )
                            .clicked()
                        {
                            let defaults = DemoState::default();
                            state.curl_scale = defaults.curl_scale;
                            state.curl_attraction = defaults.curl_attraction;
                            state.curl_damping = defaults.curl_damping;
                            state.curl_epsilon = defaults.curl_epsilon;
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
                    window.label("Tune: O/P, K/L, M/N, B/V");
                });
        },
    );
    Ok(())
}

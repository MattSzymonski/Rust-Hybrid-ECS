//! The tuning overlay: the demo's `pill_ui` panel, styled by a `.css` asset.
//!
//! # Responsibilities
//!
//! - [`tuning_overlay_system`]: draw the FPS readout and the curl-noise
//!   tuning values as a top-left overlay, and keep its stylesheet level with
//!   the asset store - an edited `res/ui.css` restyles the running demo,
//!   because the dev host's asset watcher reimports it and the next sync
//!   re-parses it.
//!
//! # Design
//!
//! The overlay is ordinary project code: a `#[pill_hot]` system, so an edit
//! to this function live-patches the running demo. It is deliberately
//! self-contained - no `crate::` imports and no file-level constants - because
//! a generated fast patch replays a hot file's imports into a synthetic crate
//! and copies only the function: either shape would fail to compile there and
//! cost the edit a full reload. Everything the UI remembers between frames -
//! scroll, focus, text editing - lives in the [`UiContext`] world resource,
//! which survives a project reload; the tuning values themselves live in
//! [`DemoState`], a persisted resource. The overlay lays out against its own
//! size and sits at the surface's top-left corner, so it needs no window-size
//! input.

// External crates
use pill_core::math::Vector2f;
use pill_engine::{pill_hot, AssetManager, Input, Res, ResMut, SystemError, Time};
use pill_ui::UiContext;

// The crate's own items, named through its package name rather than `crate::`:
// a generated hot patch replays this file's imports into a synthetic crate,
// where only `circus_demo::...` resolves (it links this rlib).
use circus_demo::{DemoState, PILL_COUNT};

/// Draw the tuning overlay and keep its stylesheet fresh.
#[pill_hot]
pub(crate) fn tuning_overlay_system(
    mut ui: ResMut<UiContext>,
    input: Res<Input>,
    mut assets: ResMut<AssetManager>,
    time: Res<Time>,
    state: Res<DemoState>,
) -> Result<(), SystemError> {
    let (Some(mut ui), Some(input), Some(mut assets), Some(time), Some(state)) = (
        ui.get_mut(),
        input.get(),
        assets.get_mut(),
        time.get(),
        state.get(),
    ) else {
        return Ok(());
    };

    // The stylesheet and the overlay's own size are literals rather than
    // file-level constants, so the whole body stays copyable by a fast patch.
    ui.sync_stylesheet(&mut assets, "ui.css");
    ui.begin_frame(input, Vector2f::new(300.0, 250.0), 1.0);
    ui.draw(0, |panel| {
        panel.heading("Circus Demo");
        panel.label(&format!(
            "{:.0} FPS | {} pills",
            1.0 / time.delta_seconds().max(1e-6),
            PILL_COUNT
        ));
        panel.label(&format!("{:.0}s uptime", time.elapsed_seconds()));
        panel.separator();
        panel.collapsing("Curl noise", true, |panel| {
            panel.slider("Scale", state.curl_scale, 0.0..=1.0);
            panel.slider("Attraction", state.curl_attraction, 0.0..=400.0);
            panel.slider("Damping", state.curl_damping, 0.0..=10.0);
            panel.drag_value("Epsilon", state.curl_epsilon, 0.0001);
            panel.label("Tune: O/P, K/L, M/N, B/V");
        });
    });
    Ok(())
}

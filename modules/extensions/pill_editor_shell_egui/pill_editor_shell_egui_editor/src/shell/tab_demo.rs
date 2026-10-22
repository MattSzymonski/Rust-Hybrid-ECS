//! The shell module's own editor panel (W-0038 Stage 11).
//!
//! # Responsibilities
//!
//! - [`demo_panel`]: the reference a module's panel is written against - it
//!   reads only [`PanelContext`] and draws with egui, so nothing of this
//!   crate's state outlives the frame that drew it.
//! - The deliberate overshoot behind `PILL_TEST_PANEL_SLOW`: a body that
//!   costs more than [`PANEL_BUDGET_MICROS`], which is how the egui suite
//!   proves the tab's badge and the budget toast.
//!
//! # Design
//!
//! The panel is registered by this crate's `register` through
//! [`PanelRegistrations`], and the executable binds the request to a liveness
//! marker that dies with this module's image. It holds no state of its own: a
//! panel that kept a counter would need somewhere to keep it that survives a
//! reload, and a world resource is the only such place - the egui context is
//! never one (see the reload-safety contract in `pill_ui_egui`).

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// The environment variable that makes the panel deliberately slow.
pub const PANEL_SLOW_ENV: &str = "PILL_TEST_PANEL_SLOW";

/// How many times the budget the slow posture burns.
const SLOW_MULTIPLE: f32 = 3.0;

/// The module's demo panel.
///
/// Public because the function inventory names it by path, and a private `fn`
/// would fail the build of the generated wrapper crate.
pub fn demo_panel(ui: &mut egui::Ui, context: &mut PanelContext<'_>) {
    ui.vertical(|ui| {
        ui.heading("Demo Panel");
        ui.label("Registered by the egui shell module itself.");
        ui.separator();
        ui.label(format!("frame rate: {:.1}", context.frames_per_second));
        match context.selected_entity {
            Some(entity) => ui.label(format!("selected entity: {entity}")),
            None => ui.label("selected entity: none"),
        };
        ui.separator();
        ui.label(
            "A module panel draws with egui and reads only plain data: the \
             dock keeps its tab, and a module that unloads reads ",
        );
        ui.monospace("module not loaded");
    });
    if slow_requested() {
        burn_micros(PANEL_BUDGET_MICROS * SLOW_MULTIPLE);
    }
}

/// Whether `PILL_TEST_PANEL_SLOW` asks for the over-budget body.
///
/// Public because the function inventory names every function by path.
pub fn slow_requested() -> bool {
    std::env::var(PANEL_SLOW_ENV)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

/// Spend `micros` of wall clock doing nothing: the budget's worst case.
///
/// Public because the function inventory names every function by path.
pub fn burn_micros(micros: f32) {
    let started = pill_core::platform::Instant::now();
    let budget = std::time::Duration::from_micros(micros as u64);
    while started.elapsed() < budget {
        std::hint::spin_loop();
    }
}

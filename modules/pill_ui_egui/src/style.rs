//! The editor's egui look: one dark theme close to the approved style.
//!
//! # Responsibilities
//!
//! - [`apply`] installs the theme on a `Context`; the egui shell calls it
//!   once per generation at registration.
//!
//! Styling is egui `Style`/`Visuals` data, never CSS (plan decision E5); the
//! palette follows the editor's approved dark look.

// External crates
use egui::{Color32, Context, Stroke, Theme, ThemePreference};

/// Install the editor's dark theme and spacing on `context`.
pub fn apply(context: &Context) {
    context.set_theme(ThemePreference::Dark);
    context.style_mut_of(Theme::Dark, |style| {
        let visuals = &mut style.visuals;
        visuals.panel_fill = Color32::from_rgb(28, 28, 32);
        visuals.window_fill = Color32::from_rgb(32, 32, 37);
        visuals.extreme_bg_color = Color32::from_rgb(20, 20, 24);
        visuals.faint_bg_color = Color32::from_rgb(38, 38, 44);
        visuals.selection.bg_fill = Color32::from_rgb(52, 84, 132);
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(58, 58, 66));
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    });
}

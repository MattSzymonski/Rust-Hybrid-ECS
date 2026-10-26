//! Scoped style variants for stock egui widgets.
//!
//! # Responsibilities
//!
//! - [`primary`](crate::variants::primary), [`secondary`](crate::variants::secondary)
//!   and [`danger`](crate::variants::danger): scoped widget visuals and button
//!   padding from the theme's tokens, for a block the caller wraps in
//!   `ui.scope`.
//!
//! # Design
//!
//! A variant mutates the `Ui` it is given and nothing else, so
//! `ui.scope(|ui| { variants::danger(ui, &theme); ... })` restores the theme's
//! look when the block ends. `Button::fill` would freeze every state
//! (`research §6`); changing the scoped `Style` keeps hover and press
//! behaviour.

// External crates
use egui::{Color32, Ui};

// Current crate
use pill_ui_theme::{Rgba, Theme};

/// Apply the `primary` variant: the accent fill.
pub fn primary(ui: &mut Ui, theme: &Theme) {
    apply(
        ui,
        theme,
        theme.palette.accent,
        theme.palette.accent_hot,
        theme.palette.accent_hot,
    );
}

/// Apply the `secondary` variant: a raised neutral fill.
pub fn secondary(ui: &mut Ui, theme: &Theme) {
    apply(
        ui,
        theme,
        theme.palette.panel_raised,
        theme.palette.line,
        theme.palette.panel,
    );
}

/// Apply the `danger` variant: a danger fill that strengthens on interaction.
pub fn danger(ui: &mut Ui, theme: &Theme) {
    apply(
        ui,
        theme,
        theme.palette.danger.with_alpha_factor(0.35),
        theme.palette.danger,
        theme.palette.danger,
    );
}

/// Set the three interactive fills and the theme's button padding on `ui`.
fn apply(ui: &mut Ui, theme: &Theme, inactive: Rgba, hovered: Rgba, active: Rgba) {
    let widgets = &mut ui.visuals_mut().widgets;
    widgets.inactive.weak_bg_fill = color(inactive);
    widgets.inactive.bg_fill = color(inactive);
    widgets.hovered.weak_bg_fill = color(hovered);
    widgets.hovered.bg_fill = color(hovered);
    widgets.active.weak_bg_fill = color(active);
    widgets.active.bg_fill = color(active);
    widgets.hovered.bg_stroke.color = color(hovered);
    widgets.active.bg_stroke.color = color(active);
    let [x, y] = theme.spacing.button_padding;
    ui.spacing_mut().button_padding = egui::vec2(x, y);
}

/// An egui colour from a theme colour.
fn color(value: Rgba) -> Color32 {
    let [red, green, blue, alpha] = value.0;
    Color32::from_rgba_unmultiplied(red, green, blue, alpha)
}

#[cfg(test)]
mod tests {
    use egui::{Context, RawInput};

    use super::{danger, primary, secondary};
    use pill_ui_theme::Theme;

    #[test]
    fn the_variants_set_distinct_fills() {
        let theme = Theme::brutalist();
        let context = Context::default();
        let mut output = context.run_ui(RawInput::default(), |ui| {
            primary(ui, &theme);
            let primary_fill = ui.visuals().widgets.inactive.weak_bg_fill;
            secondary(ui, &theme);
            let secondary_fill = ui.visuals().widgets.inactive.weak_bg_fill;
            danger(ui, &theme);
            let danger_fill = ui.visuals().widgets.inactive.weak_bg_fill;
            assert_ne!(primary_fill, secondary_fill);
            assert_ne!(secondary_fill, danger_fill);
            assert_ne!(primary_fill, danger_fill);
        });
        output.textures_delta.clear();
    }

    #[test]
    fn the_padding_comes_from_the_theme() {
        let theme = Theme::brutalist();
        let context = Context::default();
        let mut output = context.run_ui(RawInput::default(), |ui| {
            primary(ui, &theme);
            let [x, y] = theme.spacing.button_padding;
            assert_eq!(ui.spacing().button_padding, egui::vec2(x, y));
        });
        output.textures_delta.clear();
    }
}

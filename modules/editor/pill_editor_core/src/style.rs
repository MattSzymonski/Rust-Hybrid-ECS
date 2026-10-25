//! The editor's egui look: one flat, compact dark theme.
//!
//! # Responsibilities
//!
//! - [`apply`]: installs the theme on a `Context`; the egui shell calls it
//!   once per generation at registration.
//! - [`dock_style`]: the `egui_dock` styling (flat tabs merged with their
//!   bodies, thin separators) built from the theme, for the shell's
//!   `DockArea`.
//! - The named colors the shell needs beyond egui's own visuals (the
//!   viewport overlay's chips and controls).
//!
//! # Design
//!
//! Styling is egui `Style`/`Visuals` data, never CSS (plan decision E5).
//! The palette is a low-contrast dark gray - a darker frame and tab strip,
//! slightly lighter panels, one muted border - with a single green accent
//! reserved for status wording; everything stays in this module so the
//! shell never hardcodes a color of its own.

// External crates
use egui::style::TextStyle;
use egui::{Color32, Context, CornerRadius, FontId, Stroke, Theme, ThemePreference};

// =============================================================================
// Palette
// =============================================================================

/// The window frame and tab-strip background, a step darker than the
/// panels.
pub const FRAME_FILL: Color32 = Color32::from_rgb(28, 29, 31);

/// The panels' and tab bodies' background.
pub const PANEL_FILL: Color32 = Color32::from_rgb(42, 43, 46);

/// Inputs and wells: a step darker than the panels.
pub const WELL_FILL: Color32 = Color32::from_rgb(22, 23, 25);

/// The border between panels and around controls.
pub const BORDER: Color32 = Color32::from_rgb(61, 63, 66);

/// Main text.
pub const TEXT: Color32 = Color32::from_rgb(198, 200, 203);

/// Emphasized text (active tab titles, hovered labels).
pub const TEXT_STRONG: Color32 = Color32::from_rgb(232, 233, 235);

/// Muted text (inactive tab titles, secondary labels).
pub const TEXT_MUTED: Color32 = Color32::from_rgb(142, 144, 148);

/// The green status accent; the shell reserves it for status wording.
pub const ACCENT: Color32 = Color32::from_rgb(122, 194, 82);

/// The viewport overlay's chip fill (near-opaque, so the scene stays
/// readable under it) and its border.
pub const OVERLAY_FILL: Color32 = Color32::from_rgba_premultiplied(21, 22, 24, 226);
pub const OVERLAY_BORDER: Color32 = Color32::from_rgb(66, 68, 72);

/// The fill of flat controls sitting above the viewport (the example
/// button).
pub const CONTROL_FILL: Color32 = Color32::from_rgb(47, 49, 52);

/// The selected-row highlight (lists, selectable labels).
pub const SELECTION_FILL: Color32 = Color32::from_rgb(56, 73, 94);

// =============================================================================
// Applying the theme
// =============================================================================

/// Install the editor's dark theme and spacing on `context`.
pub fn apply(context: &Context) {
    // The icon fonts ride the theme: the shell draws the chrome's icons in
    // the families this registers.
    crate::icons::install(context);
    context.set_theme(ThemePreference::Dark);
    context.style_mut_of(Theme::Dark, |style| {
        let visuals = &mut style.visuals;
        visuals.panel_fill = PANEL_FILL;
        visuals.window_fill = PANEL_FILL;
        visuals.window_stroke = Stroke::new(1.0, BORDER);
        visuals.window_corner_radius = CornerRadius::same(6);
        visuals.window_shadow = egui::epaint::Shadow {
            offset: [0, 6],
            blur: 18,
            spread: 0,
            color: Color32::from_black_alpha(140),
        };
        visuals.extreme_bg_color = WELL_FILL;
        visuals.faint_bg_color = Color32::from_rgb(35, 36, 39);
        visuals.selection.bg_fill = SELECTION_FILL;
        visuals.selection.stroke = Stroke::new(1.0, TEXT_STRONG);
        visuals.hyperlink_color = ACCENT;
        visuals.slider_trailing_fill = true;
        // Flat widgets: fills carry the state, the stroke stays subtle.
        let widgets = &mut visuals.widgets;
        widgets.noninteractive.bg_fill = PANEL_FILL;
        widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        widgets.inactive.bg_fill = Color32::from_rgb(52, 54, 57);
        widgets.inactive.bg_stroke = Stroke::NONE;
        widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        widgets.hovered.bg_fill = Color32::from_rgb(62, 64, 68);
        widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(84, 86, 90));
        widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
        widgets.active.bg_fill = Color32::from_rgb(70, 73, 77);
        widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(94, 97, 101));
        widgets.active.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
        widgets.open.bg_fill = Color32::from_rgb(62, 64, 68);
        widgets.open.bg_stroke = Stroke::new(1.0, Color32::from_rgb(84, 86, 90));
        widgets.open.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
        for widget in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
            &mut widgets.open,
        ] {
            widget.corner_radius = CornerRadius::same(4);
        }
        // Compact metrics, so panels hold denser rows like the reference.
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        style.spacing.interact_size = egui::vec2(24.0, 20.0);
        style.spacing.scroll = egui::style::ScrollStyle::floating();
        style.text_styles = [
            (TextStyle::Heading, FontId::proportional(15.0)),
            (TextStyle::Body, FontId::proportional(13.0)),
            (TextStyle::Button, FontId::proportional(13.0)),
            (TextStyle::Monospace, FontId::monospace(12.0)),
            (TextStyle::Small, FontId::proportional(11.0)),
        ]
        .into();
    });
}

// =============================================================================
// The dock's style
// =============================================================================

/// The `egui_dock` styling for the shell's `DockArea`, derived from
/// `egui_style` and overridden to the editor's look: flat tabs that merge
/// with their active bodies, a dark tab strip, and thin separators between
/// panels.
pub fn dock_style(egui_style: &egui::Style) -> egui_dock::Style {
    let mut style = egui_dock::Style::from_egui(egui_style);
    style.main_surface_border_stroke = Stroke::NONE;
    style.dock_area_padding = None;
    // The tab strip: a darker band; the active tab takes the body's fill,
    // so it reads as one surface with its panel.
    style.tab_bar.bg_fill = FRAME_FILL;
    style.tab_bar.height = 26.0;
    style.tab_bar.corner_radius = CornerRadius::ZERO;
    style.tab_bar.hline_color = BORDER;
    style.tab_bar.inner_margin = egui::Margin::symmetric(6, 3);
    style.tab.active = tab_faces(PANEL_FILL, TEXT_STRONG);
    style.tab.focused = tab_faces(PANEL_FILL, TEXT_STRONG);
    style.tab.active_with_kb_focus = tab_faces(PANEL_FILL, TEXT_STRONG);
    style.tab.inactive = tab_faces(Color32::TRANSPARENT, TEXT_MUTED);
    style.tab.inactive_with_kb_focus = tab_faces(Color32::TRANSPARENT, TEXT_MUTED);
    style.tab.hovered = tab_faces(Color32::from_rgb(33, 34, 36), TEXT);
    style.tab.focused_with_kb_focus = tab_faces(Color32::from_rgb(33, 34, 36), TEXT);
    style.tab.hline_below_active_tab_name = false;
    style.tab.spacing = 0.0;
    style.tab.tab_body.bg_fill = PANEL_FILL;
    style.tab.tab_body.inner_margin = egui::Margin::same(6);
    style.tab.tab_body.corner_radius = CornerRadius::ZERO;
    style.tab.tab_body.stroke = Stroke::NONE;
    // Panel boundaries: one thin line, with a grab band several times its
    // width so the splitter is easy to catch (W-0049), brightened while
    // interacted with.
    style.separator.width = 1.0;
    style.separator.extra_interact_width = 8.0;
    style.separator.color_idle = BORDER;
    style.separator.color_hovered = Color32::from_rgb(96, 99, 104);
    style.separator.color_dragged = Color32::from_rgb(126, 130, 136);
    // The drag-and-drop preview uses the selection tint.
    style.overlay.selection_color = SELECTION_FILL;
    // Icon-ish buttons: muted, brightening on hover.
    let buttons = &mut style.buttons;
    buttons.add_tab_align = egui_dock::TabAddAlign::Left;
    for (color, active) in [
        (
            &mut buttons.close_tab_color,
            &mut buttons.close_tab_active_color,
        ),
        (
            &mut buttons.add_tab_color,
            &mut buttons.add_tab_active_color,
        ),
        (
            &mut buttons.close_all_tabs_color,
            &mut buttons.close_all_tabs_active_color,
        ),
        (
            &mut buttons.collapse_tabs_color,
            &mut buttons.collapse_tabs_active_color,
        ),
        (
            &mut buttons.minimize_window_color,
            &mut buttons.minimize_window_active_color,
        ),
    ] {
        *color = TEXT_MUTED;
        *active = TEXT_STRONG;
    }
    style
}

/// One `egui_dock` tab face: square corners and no outline, so the fill
/// alone separates the states.
fn tab_faces(bg_fill: Color32, text_color: Color32) -> egui_dock::TabInteractionStyle {
    egui_dock::TabInteractionStyle {
        bg_fill,
        text_color,
        corner_radius: CornerRadius::ZERO,
        outline_color: Color32::TRANSPARENT,
    }
}

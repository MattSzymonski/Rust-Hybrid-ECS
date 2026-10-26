//! The token-to-egui translator and the font installer.
//!
//! # Responsibilities
//!
//! - [`build_style`]: a complete [`egui::Style`] from a
//!   [`Theme`](pill_ui_theme::Theme).
//! - [`font_definitions`]: egui's defaults, the theme's fonts, the caller's
//!   built-in families behind them, and every family name the styles use
//!   registered.
//! - [`install`]: fonts first (only when their signature changed, reported by
//!   its return), then the style written to both egui themes, then the dark
//!   theme pinned.
//! - [`FontSignature`]: the caller-held record that makes `set_fonts` run only
//!   when the fonts changed.
//!
//! # Design
//!
//! Every entry point is a plain function compiled into the caller's dylib, so
//! no closure created by a reloadable image ends up inside an egui `Style`.
//! The kit holds no statics: the font signature, the font bytes and the theme
//! all belong to the caller. Fonts are installed before the style, because a
//! style that names an unregistered family panics on the next pass
//! (`research §4.2`).

// Standard library
use std::hash::{Hash, Hasher};
use std::sync::Arc;

// External crates
use egui::epaint::Shadow;
use egui::style::{ScrollStyle, Selection};
use egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin,
    Stroke as EguiStroke, TextStyle, Theme as EguiTheme, ThemePreference, Vec2,
};

// Current crate
use pill_ui_theme::{FontFamilyName, Rgba, Stroke, Theme, WidgetStyle};

/// The family name a theme's `display` font is registered under.
pub const DISPLAY_FAMILY: &str = "display";

/// The font-definition key a theme's body font is registered under.
pub const THEME_BODY_KEY: &str = "theme.body";
/// The font-definition key a theme's display font is registered under.
pub const THEME_DISPLAY_KEY: &str = "theme.display";
/// The font-definition key a theme's monospace font is registered under.
pub const THEME_MONOSPACE_KEY: &str = "theme.monospace";

/// Whether `name` is one of the theme's own font keys.
///
/// A caller inserting its built-in families uses this to keep the theme's font
/// in front: the theme font is what a layer asked for, the caller's is the
/// fallback behind it.
pub fn is_theme_font(name: &str) -> bool {
    name.starts_with("theme.")
}

/// The font bytes a theme layer resolved.
///
/// The kit installs owned copies: the game copies them out of the asset store,
/// the editor's user layer out of the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeFonts {
    /// The body font.
    pub body: Option<Vec<u8>>,
    /// The display font.
    pub display: Option<Vec<u8>>,
    /// The monospace font.
    pub monospace: Option<Vec<u8>>,
}

/// A caller-held record of the font set last installed.
///
/// A new signature (or a fresh one, for a new context) makes [`install`] call
/// `set_fonts`; an unchanged one skips it, because epaint compares the full TTF
/// data on every call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FontSignature(Option<u64>);

impl FontSignature {
    /// The signature of `fonts`.
    pub fn of(fonts: &ThemeFonts) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for bytes in [&fonts.body, &fonts.display, &fonts.monospace] {
            match bytes {
                Some(bytes) => {
                    1u8.hash(&mut hasher);
                    bytes.hash(&mut hasher);
                }
                None => 0u8.hash(&mut hasher),
            }
        }
        Self(Some(hasher.finish()))
    }
}

impl std::fmt::Display for FontSignature {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(hash) => write!(formatter, "{hash:016x}"),
            None => formatter.write_str("none"),
        }
    }
}

/// Build egui's style from `theme`, starting from `base`.
///
/// Every field the theme owns is overwritten; the rest of `base` is kept.
pub fn build_style(theme: &Theme, base: &egui::Style) -> egui::Style {
    let mut style = base.clone();
    let visuals = &mut style.visuals;
    visuals.dark_mode = theme.dark_mode;
    // Per-state `fg_stroke` carries the text colours; the override flattens
    // them (`research §1`).
    visuals.override_text_color = None;
    visuals.panel_fill = color(theme.palette.panel);
    visuals.window_fill = color(theme.palette.panel);
    visuals.window_stroke = stroke(theme.shape.window_stroke);
    visuals.window_corner_radius = CornerRadius::same(theme.shape.window_corner_radius);
    visuals.menu_corner_radius = CornerRadius::same(theme.shape.menu_corner_radius);
    visuals.window_shadow = shadow(theme.shape.window_shadow);
    visuals.popup_shadow = shadow(theme.shape.popup_shadow);
    visuals.extreme_bg_color = color(theme.palette.well);
    visuals.faint_bg_color = color(theme.palette.panel_raised);
    visuals.code_bg_color = color(theme.palette.well);
    visuals.selection = Selection {
        bg_fill: color(theme.palette.selection),
        stroke: stroke(theme.shape.selection_stroke),
    };
    visuals.hyperlink_color = color(theme.palette.accent_hot);
    visuals.warn_fg_color = color(theme.palette.warning);
    visuals.error_fg_color = color(theme.palette.danger);
    visuals.slider_trailing_fill = true;
    visuals.widgets.noninteractive = widget_visuals(theme.widgets.noninteractive);
    visuals.widgets.inactive = widget_visuals(theme.widgets.inactive);
    visuals.widgets.hovered = widget_visuals(theme.widgets.hovered);
    visuals.widgets.active = widget_visuals(theme.widgets.active);
    visuals.widgets.open = widget_visuals(theme.widgets.open);

    let spacing = &mut style.spacing;
    spacing.item_spacing = vec2(theme.spacing.item_spacing);
    spacing.button_padding = vec2(theme.spacing.button_padding);
    spacing.interact_size = vec2(theme.spacing.interact_size);
    spacing.window_margin = margin(theme.spacing.window_margin);
    spacing.menu_margin = margin(theme.spacing.menu_margin);
    spacing.indent = theme.spacing.indent;
    spacing.slider_width = theme.spacing.slider_width;
    spacing.slider_rail_height = theme.spacing.slider_rail_height;
    spacing.icon_width = theme.spacing.icon_width;
    spacing.icon_width_inner = theme.spacing.icon_width_inner;
    spacing.icon_spacing = theme.spacing.icon_spacing;
    spacing.menu_width = theme.spacing.menu_width;
    spacing.tooltip_width = theme.spacing.tooltip_width;
    spacing.combo_width = theme.spacing.combo_width;
    spacing.combo_height = theme.spacing.combo_height;
    spacing.text_edit_width = theme.spacing.text_edit_width;
    spacing.menu_spacing = theme.spacing.menu_spacing;
    // A floating bar is the solid one at its floating width, so the theme owns
    // both the width and the posture.
    spacing.scroll = if theme.spacing.scroll_floating {
        ScrollStyle {
            bar_width: theme.spacing.scroll_bar_width,
            ..ScrollStyle::floating()
        }
    } else {
        ScrollStyle {
            bar_width: theme.spacing.scroll_bar_width,
            ..ScrollStyle::solid()
        }
    };

    let mut text_styles = style.text_styles.clone();
    for (name, text_style) in &theme.text_styles {
        text_styles.insert(
            text_style_key(name),
            FontId::new(text_style.size, text_style_family(text_style.family)),
        );
    }
    style.text_styles = text_styles;
    style
}

/// Build the font definitions for `fonts` and the caller's built-ins.
///
/// Order per family: the theme font first, then the caller's built-ins, then
/// egui's defaults. The theme's fonts go in *before* `builtin` runs, so a
/// family the caller registers by copying the proportional list (an icon font's
/// fallback chain) carries the theme's body font too.
///
/// The named `display` family always exists, so a style may name it even when
/// the theme ships no display font; it falls back to the family as the caller
/// left it.
pub fn font_definitions(
    fonts: &ThemeFonts,
    builtin: impl FnOnce(&mut FontDefinitions),
) -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    if let Some(bytes) = &fonts.body {
        definitions.font_data.insert(
            THEME_BODY_KEY.to_owned(),
            Arc::new(FontData::from_owned(bytes.clone())),
        );
        definitions
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, THEME_BODY_KEY.to_owned());
    }
    if let Some(bytes) = &fonts.monospace {
        definitions.font_data.insert(
            THEME_MONOSPACE_KEY.to_owned(),
            Arc::new(FontData::from_owned(bytes.clone())),
        );
        definitions
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, THEME_MONOSPACE_KEY.to_owned());
    }
    builtin(&mut definitions);

    let proportional = definitions
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut display_family = Vec::new();
    if let Some(bytes) = &fonts.display {
        definitions.font_data.insert(
            THEME_DISPLAY_KEY.to_owned(),
            Arc::new(FontData::from_owned(bytes.clone())),
        );
        display_family.push(THEME_DISPLAY_KEY.to_owned());
    }
    display_family.extend(proportional);
    definitions
        .families
        .insert(FontFamily::Name(DISPLAY_FAMILY.into()), display_family);
    definitions
}

/// Install `theme` on `context`: fonts first, then the style to both egui
/// themes, then the dark theme pinned. `builtin` appends the caller's own
/// families (the editor's Inter, JetBrains Mono and Phosphor). It runs only
/// when the fonts are installed. `signature` belongs to the context; pass a
/// fresh one for a new context.
///
/// Returns whether `set_fonts` ran, so the caller can say so once per install.
pub fn install(
    context: &Context,
    theme: &Theme,
    fonts: &ThemeFonts,
    builtin: impl FnOnce(&mut FontDefinitions),
    signature: &mut FontSignature,
) -> bool {
    let next = FontSignature::of(fonts);
    let fonts_installed = *signature != next;
    if fonts_installed {
        context.set_fonts(font_definitions(fonts, builtin));
        *signature = next;
    }
    let style = build_style(theme, &egui::Style::default());
    context.set_style_of(EguiTheme::Dark, style.clone());
    context.set_style_of(EguiTheme::Light, style);
    context.set_theme(ThemePreference::Dark);
    fonts_installed
}

/// The egui text-style key for a theme's named style.
fn text_style_key(name: &str) -> TextStyle {
    match name {
        "small" => TextStyle::Small,
        "body" => TextStyle::Body,
        "button" => TextStyle::Button,
        "monospace" => TextStyle::Monospace,
        "heading" => TextStyle::Heading,
        other => TextStyle::Name(other.into()),
    }
}

/// The egui family for a theme family.
fn text_style_family(family: FontFamilyName) -> FontFamily {
    match family {
        FontFamilyName::Proportional => FontFamily::Proportional,
        FontFamilyName::Monospace => FontFamily::Monospace,
        FontFamilyName::Display => FontFamily::Name(DISPLAY_FAMILY.into()),
    }
}

/// An egui colour from a theme colour (straight sRGB alpha).
fn color(value: Rgba) -> Color32 {
    let [red, green, blue, alpha] = value.0;
    Color32::from_rgba_unmultiplied(red, green, blue, alpha)
}

/// An egui stroke from a theme stroke.
fn stroke(value: Stroke) -> EguiStroke {
    EguiStroke::new(value.width, color(value.color))
}

/// An egui widget look from a theme widget look.
fn widget_visuals(value: WidgetStyle) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill: color(value.bg_fill),
        weak_bg_fill: color(value.weak_bg_fill),
        bg_stroke: stroke(value.bg_stroke),
        corner_radius: CornerRadius::same(value.corner_radius),
        fg_stroke: stroke(value.fg_stroke),
        expansion: value.expansion,
    }
}

/// An egui shadow from a theme shadow (integer fields in egui 0.36).
fn shadow(value: pill_ui_theme::Shadow) -> Shadow {
    Shadow {
        offset: value.offset,
        blur: value.blur,
        spread: value.spread,
        color: color(value.color),
    }
}

/// An egui vector from a theme pair.
fn vec2(value: [f32; 2]) -> Vec2 {
    Vec2::new(value[0], value[1])
}

/// An egui margin from one theme number.
fn margin(value: f32) -> Margin {
    Margin::same(value.round().clamp(0.0, 127.0) as i8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{RawInput, ThemePreference};
    use pill_ui_theme::{Spacing, Theme, ThemeOverrides};

    fn themed(overrides: &str) -> Theme {
        Theme::fallback()
            .merged_with(&ThemeOverrides::from_json_str(overrides).unwrap())
            .unwrap()
    }

    #[test]
    fn every_widget_state_maps_to_its_token() {
        let theme = themed(
            r##"{
                "widgets": {
                    "inactive": {"weak_bg_fill": "#112233"},
                    "hovered": {"bg_stroke": {"width": 3.0, "color": "#445566"}},
                    "active": {"fg_stroke": {"width": 2.0, "color": "#778899"}}
                }
            }"##,
        );
        let style = build_style(&theme, &egui::Style::default());
        assert_eq!(
            style.visuals.widgets.inactive.weak_bg_fill,
            color(Rgba::rgb(0x11, 0x22, 0x33))
        );
        assert_eq!(style.visuals.widgets.hovered.bg_stroke.width, 3.0);
        assert_eq!(
            style.visuals.widgets.hovered.bg_stroke.color,
            color(Rgba::rgb(0x44, 0x55, 0x66))
        );
        assert_eq!(style.visuals.widgets.active.fg_stroke.width, 2.0);
        assert_eq!(
            style.visuals.widgets.active.fg_stroke.color,
            color(Rgba::rgb(0x77, 0x88, 0x99))
        );
    }

    #[test]
    fn override_text_color_stays_none() {
        let style = build_style(&Theme::brutalist(), &egui::Style::default());
        assert!(style.visuals.override_text_color.is_none());
    }

    #[test]
    fn dark_mode_follows_the_token() {
        let theme = themed(r#"{"dark_mode": false}"#);
        let style = build_style(&theme, &egui::Style::default());
        assert!(!style.visuals.dark_mode);
    }

    #[test]
    fn switching_to_the_light_theme_still_yields_the_theme_visuals() {
        let theme = Theme::brutalist();
        let context = Context::default();
        let mut signature = FontSignature::default();
        install(
            &context,
            &theme,
            &ThemeFonts::default(),
            |_| {},
            &mut signature,
        );
        context.set_theme(ThemePreference::Light);
        let style = context.global_style();
        assert_eq!(style.visuals.panel_fill, color(theme.palette.panel));
    }

    #[test]
    fn the_display_family_is_always_registered() {
        let theme = Theme::brutalist();
        let definitions = font_definitions(&ThemeFonts::default(), |_| {});
        let display = definitions
            .families
            .get(&FontFamily::Name(DISPLAY_FAMILY.into()))
            .expect("the display family");
        assert!(!display.is_empty());

        let context = Context::default();
        let mut signature = FontSignature::default();
        install(
            &context,
            &theme,
            &ThemeFonts::default(),
            |_| {},
            &mut signature,
        );
        // A pass that names the family must not panic.
        let mut output = context.run_ui(RawInput::default(), |ui| {
            let text =
                egui::RichText::new("display").family(FontFamily::Name(DISPLAY_FAMILY.into()));
            ui.label(text);
        });
        output.textures_delta.clear();
    }

    #[test]
    fn an_unchanged_font_signature_does_not_install_fonts_again() {
        let context = Context::default();
        let theme = Theme::fallback();
        let mut signature = FontSignature::default();
        let mut calls = 0;
        install(
            &context,
            &theme,
            &ThemeFonts::default(),
            |_| calls += 1,
            &mut signature,
        );
        assert_eq!(calls, 1);
        install(
            &context,
            &theme,
            &ThemeFonts::default(),
            |_| calls += 1,
            &mut signature,
        );
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_theme_font_goes_first_in_its_family() {
        let fonts = ThemeFonts {
            body: Some(vec![0, 1, 2, 3]),
            ..ThemeFonts::default()
        };
        let definitions = font_definitions(&fonts, |_| {});
        let proportional = definitions
            .families
            .get(&FontFamily::Proportional)
            .expect("the proportional family");
        assert_eq!(proportional.first().map(String::as_str), Some("theme.body"));
    }

    /// The theme's fonts go in before the caller's, so a family the built-in
    /// callback copies the proportional list into carries them too - an icon
    /// family draws a label beside its icon in the theme's body font.
    #[test]
    fn a_built_in_family_built_after_the_theme_font_carries_it() {
        let fonts = ThemeFonts {
            body: Some(vec![0, 1, 2, 3]),
            display: Some(vec![4, 5, 6, 7]),
            ..ThemeFonts::default()
        };
        let definitions = font_definitions(&fonts, |definitions| {
            // The shape an icon installer uses: its own font first, then the
            // proportional family as it stands.
            let fallbacks = definitions
                .families
                .get(&FontFamily::Proportional)
                .cloned()
                .unwrap_or_default();
            definitions.font_data.insert(
                "builtin.icon".to_owned(),
                Arc::new(FontData::from_static(&[9, 9])),
            );
            let mut names = fallbacks;
            names.push("builtin.icon".to_owned());
            definitions
                .families
                .insert(FontFamily::Name("icons".into()), names);
        });

        let icons = definitions
            .families
            .get(&FontFamily::Name("icons".into()))
            .expect("the built-in family");
        assert_eq!(icons.first().map(String::as_str), Some(THEME_BODY_KEY));
        assert_eq!(icons.last().map(String::as_str), Some("builtin.icon"));
        // The display family leads with its own face and falls back to the
        // body font the caller's families were built on.
        let display = definitions
            .families
            .get(&FontFamily::Name(DISPLAY_FAMILY.into()))
            .expect("the display family");
        assert_eq!(display.first().map(String::as_str), Some(THEME_DISPLAY_KEY));
        assert_eq!(display.get(1).map(String::as_str), Some(THEME_BODY_KEY));
    }

    /// `install` reports whether it ran `set_fonts`, which is what both UIs log
    /// and what keeps an unchanged font set from being re-parsed.
    #[test]
    fn install_reports_whether_the_fonts_were_installed() {
        let context = Context::default();
        let theme = Theme::fallback();
        let mut signature = FontSignature::default();
        let fonts = ThemeFonts {
            body: Some(vec![0, 1, 2, 3]),
            ..ThemeFonts::default()
        };
        assert!(install(&context, &theme, &fonts, |_| {}, &mut signature));
        assert!(!install(&context, &theme, &fonts, |_| {}, &mut signature));
        let other = ThemeFonts {
            body: Some(vec![3, 2, 1, 0]),
            ..ThemeFonts::default()
        };
        assert!(install(&context, &theme, &other, |_| {}, &mut signature));
        assert_ne!(signature.to_string(), "none");
    }

    #[test]
    fn the_light_and_dark_styles_are_the_same_theme() {
        let theme = Theme::brutalist();
        let context = Context::default();
        let mut signature = FontSignature::default();
        install(
            &context,
            &theme,
            &ThemeFonts::default(),
            |_| {},
            &mut signature,
        );
        assert_eq!(
            context.global_style().visuals.panel_fill,
            color(theme.palette.panel)
        );
        context.set_theme(ThemePreference::Light);
        assert_eq!(
            context.global_style().visuals.panel_fill,
            color(theme.palette.panel)
        );
        context.set_theme(ThemePreference::Dark);
        assert_eq!(
            context.global_style().visuals.panel_fill,
            color(theme.palette.panel)
        );
    }

    /// The theme's spacing reaches egui.
    #[test]
    fn spacing_maps_to_egui() {
        let theme = Theme::brutalist();
        let style = build_style(&theme, &egui::Style::default());
        let Spacing {
            item_spacing,
            button_padding,
            interact_size,
            ..
        } = theme.spacing;
        assert_eq!(style.spacing.item_spacing, vec2(item_spacing));
        assert_eq!(style.spacing.button_padding, vec2(button_padding));
        assert_eq!(style.spacing.interact_size, vec2(interact_size));
    }

    #[test]
    fn text_styles_map_by_name_and_family() {
        let style = build_style(&Theme::brutalist(), &egui::Style::default());
        let heading = style.text_styles.get(&TextStyle::Heading).unwrap();
        assert_eq!(heading.size, 22.0);
        assert_eq!(heading.family, FontFamily::Proportional);
        let monospace = style.text_styles.get(&TextStyle::Monospace).unwrap();
        assert_eq!(monospace.family, FontFamily::Monospace);
    }
}

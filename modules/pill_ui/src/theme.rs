//! The built-in default theme: the tokens widgets are painted with.
//!
//! # Responsibilities
//!
//! - [`Theme`]: every colour and metric the built-in widget painting uses
//!   ([`Theme::color_tokens`] lists the colours for the UI Gallery).
//! - The built-in default (plan decision D7): the approved Stage 10 design
//!   (W-0004 v4) in code; `assets/default.css` states the same tokens in the
//!   CSS subset and wins wherever a rule is more specific.
//!
//! # Design
//!
//! Plain data, no logic: a stylesheet maps its declarations onto these
//! fields, and painting reads them. Metrics are logical pixels; the backend
//! scales them with the monitor's scale factor.

// Current crate
use crate::draw::Color;

// =============================================================================
// Theme
// =============================================================================

/// The colours and metrics the default widget painting uses.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Regular text size in logical pixels.
    pub font_size: f32,
    /// Heading text size in logical pixels.
    pub heading_size: f32,
    /// Line height for regular text.
    pub line_height: f32,
    /// The height of an interactive row (buttons, fields, rows).
    pub item_height: f32,
    /// Vertical and horizontal spacing between widgets.
    pub spacing: f32,
    /// Padding inside containers and around widget content.
    pub padding: f32,
    /// Indentation per nesting level (tree rows).
    pub indent: f32,
    /// The window and scene-surround background.
    pub window: Color,
    /// A panel body.
    pub panel: Color,
    /// A raised strip: menu bar, tab strips, headers, wells' borders.
    pub raised: Color,
    /// A hovered row, tab or chip.
    pub hover: Color,
    /// A pressed surface, the open menu, hovers that press.
    pub active: Color,
    /// A sunken inset (text fields, tracks).
    pub inset: Color,
    /// Hairline borders, dividers and separators.
    pub border: Color,
    /// Control borders and popup edges.
    pub border_strong: Color,
    /// Regular text.
    pub text: Color,
    /// Secondary text: labels and values.
    pub text_secondary: Color,
    /// Muted text: metadata, markers and badges.
    pub text_muted: Color,
    /// Disabled text and marks.
    pub text_disabled: Color,
    /// The item highlight: selection, focus rings, errors.
    pub highlight: Color,
    /// The highlight's wash: selected rows and hovered menu items.
    pub highlight_wash: Color,
    /// A default button's fill.
    pub button: Color,
    /// A hovered button's fill.
    pub button_hover: Color,
    /// A pressed button's fill.
    pub button_active: Color,
    /// A checked checkbox's fill (its mark is drawn in `window`).
    pub check: Color,
    /// A slider's empty track.
    pub track: Color,
    /// A slider's filled track.
    pub track_fill: Color,
    /// A slider's knob.
    pub knob: Color,
    /// A scrollbar's thumb.
    pub scrollbar_thumb: Color,
    /// The strip a scrollbar's thumb slides in.
    pub scrollbar_track: Color,
    /// The scrollbar strip's thickness in logical pixels.
    pub scrollbar_width: f32,
    /// Border width for focused and outlined widgets.
    pub border_width: f32,

    /// Corner radius for small parts: checkbox boxes, slider tracks.
    pub radius_tiny: f32,

    /// Corner radius for controls: buttons, input wells, combo boxes.
    pub radius_small: f32,

    /// Corner radius for panels and containers.
    pub radius_medium: f32,

    /// Corner radius for popups and menus.
    pub radius_large: f32,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            // 13/15 px, 18 px lines: the approved type scale (--fs-md, --fs-lg).
            font_size: 13.0,
            heading_size: 15.0,
            line_height: 18.0,
            item_height: 24.0,
            spacing: 6.0,
            padding: 8.0,
            indent: 14.0,
            // The same values `assets/default.css` states, exactly; the two
            // are the approved Stage 10 design (W-0004 v4) in code and in
            // the subset (D7).
            window: Color::rgba(20.0 / 255.0, 20.0 / 255.0, 20.0 / 255.0, 1.0),
            panel: Color::rgba(25.0 / 255.0, 25.0 / 255.0, 25.0 / 255.0, 1.0),
            raised: Color::rgba(31.0 / 255.0, 31.0 / 255.0, 31.0 / 255.0, 1.0),
            hover: Color::rgba(40.0 / 255.0, 40.0 / 255.0, 40.0 / 255.0, 1.0),
            active: Color::rgba(50.0 / 255.0, 50.0 / 255.0, 50.0 / 255.0, 1.0),
            inset: Color::rgba(14.0 / 255.0, 14.0 / 255.0, 14.0 / 255.0, 1.0),
            border: Color::rgba(38.0 / 255.0, 38.0 / 255.0, 38.0 / 255.0, 1.0),
            border_strong: Color::rgba(56.0 / 255.0, 56.0 / 255.0, 56.0 / 255.0, 1.0),
            text: Color::rgba(230.0 / 255.0, 230.0 / 255.0, 230.0 / 255.0, 1.0),
            text_secondary: Color::rgba(158.0 / 255.0, 158.0 / 255.0, 158.0 / 255.0, 1.0),
            text_muted: Color::rgba(116.0 / 255.0, 116.0 / 255.0, 116.0 / 255.0, 1.0),
            text_disabled: Color::rgba(78.0 / 255.0, 78.0 / 255.0, 78.0 / 255.0, 1.0),
            highlight: Color::rgba(240.0 / 255.0, 68.0 / 255.0, 68.0 / 255.0, 1.0),
            highlight_wash: Color::rgba(240.0 / 255.0, 68.0 / 255.0, 68.0 / 255.0, 0.16),
            button: Color::rgba(42.0 / 255.0, 42.0 / 255.0, 42.0 / 255.0, 1.0),
            button_hover: Color::rgba(51.0 / 255.0, 51.0 / 255.0, 51.0 / 255.0, 1.0),
            button_active: Color::rgba(61.0 / 255.0, 61.0 / 255.0, 61.0 / 255.0, 1.0),
            check: Color::rgba(176.0 / 255.0, 176.0 / 255.0, 176.0 / 255.0, 1.0),
            track: Color::rgba(51.0 / 255.0, 51.0 / 255.0, 51.0 / 255.0, 1.0),
            track_fill: Color::rgba(143.0 / 255.0, 143.0 / 255.0, 143.0 / 255.0, 1.0),
            knob: Color::rgba(196.0 / 255.0, 196.0 / 255.0, 196.0 / 255.0, 1.0),
            scrollbar_thumb: Color::rgba(94.0 / 255.0, 94.0 / 255.0, 94.0 / 255.0, 1.0),
            scrollbar_track: Color::rgba(25.0 / 255.0, 25.0 / 255.0, 25.0 / 255.0, 1.0),
            scrollbar_width: 9.0,
            border_width: 1.0,
            radius_tiny: 2.0,
            radius_small: 3.0,
            radius_medium: 5.0,
            radius_large: 6.0,
        }
    }
}

impl Theme {
    /// Every colour token, by name, for the UI Gallery's token sheet.
    pub fn color_tokens(&self) -> Vec<(&'static str, Color)> {
        vec![
            ("window", self.window),
            ("panel", self.panel),
            ("raised", self.raised),
            ("hover", self.hover),
            ("active", self.active),
            ("inset", self.inset),
            ("border", self.border),
            ("border_strong", self.border_strong),
            ("text", self.text),
            ("text_secondary", self.text_secondary),
            ("text_muted", self.text_muted),
            ("text_disabled", self.text_disabled),
            ("highlight", self.highlight),
            ("highlight_wash", self.highlight_wash),
            ("button", self.button),
            ("button_hover", self.button_hover),
            ("button_active", self.button_active),
            ("check", self.check),
            ("track", self.track),
            ("track_fill", self.track_fill),
            ("knob", self.knob),
            ("scrollbar_thumb", self.scrollbar_thumb),
            ("scrollbar_track", self.scrollbar_track),
        ]
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_theme_is_coherent() {
        let theme = Theme::default();
        assert!(theme.item_height >= theme.line_height);
        assert!(theme.padding > 0.0 && theme.spacing > 0.0);
        assert_eq!(theme.color_tokens().len(), 23);
        // Every colour is visible against the background it sits on.
        assert!(theme.text.a > 0.9);
        assert!(theme.highlight.a > 0.9);
    }
}

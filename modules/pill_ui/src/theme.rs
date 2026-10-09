//! The built-in default theme: the tokens widgets are painted with.
//!
//! # Responsibilities
//!
//! - [`Theme`]: every colour and metric the built-in widget painting uses
//!   ([`Theme::color_tokens`] lists the colours for the UI Gallery).
//! - The built-in default until Stage 9's stylesheets replace it (plan
//!   decision D7): values here are the fallback when no stylesheet loads.
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
    /// The window/panel background.
    pub background: Color,
    /// A raised surface (buttons, headers, fields).
    pub surface: Color,
    /// A sunken inset (text fields, tracks).
    pub inset: Color,
    /// Regular text.
    pub text: Color,
    /// Disabled text and secondary labels.
    pub text_disabled: Color,
    /// The interactive accent (active buttons, knobs, selections).
    pub accent: Color,
    /// A muted accent (hover, filled slider tracks).
    pub accent_muted: Color,
    /// Borders and separators.
    pub border: Color,
    /// Border width for focused and outlined widgets.
    pub border_width: f32,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            font_size: 14.0,
            heading_size: 18.0,
            line_height: 18.0,
            item_height: 24.0,
            spacing: 6.0,
            padding: 8.0,
            indent: 14.0,
            background: Color::rgba(0.12, 0.13, 0.15, 1.0),
            surface: Color::rgba(0.19, 0.20, 0.23, 1.0),
            inset: Color::rgba(0.08, 0.09, 0.10, 1.0),
            text: Color::rgba(0.87, 0.88, 0.90, 1.0),
            text_disabled: Color::rgba(0.45, 0.46, 0.50, 1.0),
            accent: Color::rgba(0.26, 0.52, 0.85, 1.0),
            accent_muted: Color::rgba(0.20, 0.36, 0.58, 1.0),
            border: Color::rgba(0.32, 0.34, 0.38, 1.0),
            border_width: 1.0,
        }
    }
}

impl Theme {
    /// Every colour token, by name, for the UI Gallery's token sheet.
    pub fn color_tokens(&self) -> [(&'static str, Color); 8] {
        [
            ("background", self.background),
            ("surface", self.surface),
            ("inset", self.inset),
            ("text", self.text),
            ("text_disabled", self.text_disabled),
            ("accent", self.accent),
            ("accent_muted", self.accent_muted),
            ("border", self.border),
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
        assert_eq!(theme.color_tokens().len(), 8);
        // Every colour is visible against the background it sits on.
        assert!(theme.text.a > 0.9);
        assert!(theme.accent.a > 0.9);
    }
}

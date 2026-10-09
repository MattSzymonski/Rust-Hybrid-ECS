//! Component scanning and typed values for the CSS subset.
//!
//! # Responsibilities
//!
//! - [`components`]: splits a declaration's raw value into top-level
//!   components (whitespace-separated words, function calls and comma
//!   groups kept whole).
//! - Typed parsing for every value the subset supports: colours, lengths,
//!   sizes, keywords and the side shorthands (`1px 2px`), so a declaration
//!   is validated once at load time.
//!
//! # Design
//!
//! Values arrive as raw text (the parser records slices, not token streams),
//! so everything here takes `&str` and is deliberately strict: a value the
//! subset does not understand is reported by the caller with its file
//! position, never guessed at. Percentages are kept as such where they are
//! meaningful (`width: 50%`), resolved against the parent where they are
//! not (`padding`), and refused where the model has no room for them.

// Standard library
use std::fmt;

// Current crate
use crate::draw::Color;

// =============================================================================
// Component scanning
// =============================================================================

/// One top-level component of a declaration value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    /// Lowercased text of the component, function calls included.
    pub text: String,
    /// The component as written, case kept (font families are
    /// case-sensitive names).
    pub source: String,
}

impl Component {
    /// Whether the component is a `name(...)` function call.
    pub fn function(&self) -> Option<(&str, &str)> {
        let open = self.text.find('(')?;
        let name = &self.text[..open];
        let inner = self.text[open + 1..].strip_suffix(')')?;
        Some((name, inner))
    }
}

/// Split `raw` into top-level components: whitespace separates, commas are
/// separators of their own, and parentheses keep their contents together.
pub fn components(raw: &str) -> Vec<Component> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;

    let push = |current: &mut String, out: &mut Vec<Component>| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            out.push(Component {
                text: trimmed.to_ascii_lowercase(),
                source: trimmed.to_string(),
            });
        }
        current.clear();
    };

    for character in raw.chars() {
        match quote {
            Some(open) => {
                current.push(character);
                if character == open {
                    quote = None;
                }
            }
            None => match character {
                '"' | '\'' => {
                    quote = Some(character);
                    current.push(character);
                }
                '(' => {
                    depth += 1;
                    current.push(character);
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    current.push(character);
                }
                ',' if depth == 0 => {
                    push(&mut current, &mut out);
                    out.push(Component {
                        text: String::from(","),
                        source: String::from(","),
                    });
                }
                character if character.is_whitespace() && depth == 0 => {
                    push(&mut current, &mut out);
                }
                _ => current.push(character),
            },
        }
    }
    push(&mut current, &mut out);
    out
}

/// The components with comma separators dropped.
pub fn words(raw: &str) -> Vec<Component> {
    components(raw)
        .into_iter()
        .filter(|component| component.text != ",")
        .collect()
}

// =============================================================================
// Sides
// =============================================================================

/// One value per side, in CSS order.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Sides<T> {
    /// The top side.
    pub top: T,
    /// The right side.
    pub right: T,
    /// The bottom side.
    pub bottom: T,
    /// The left side.
    pub left: T,
}

impl<T: Copy> Sides<T> {
    /// All sides the same.
    pub fn all(value: T) -> Sides<T> {
        Sides {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }

    /// Every side through `f`.
    pub fn map<U>(self, f: impl Fn(T) -> U) -> Sides<U> {
        Sides {
            top: f(self.top),
            right: f(self.right),
            bottom: f(self.bottom),
            left: f(self.left),
        }
    }
}

/// Build [`Sides`] from one to four values, as CSS shorthands do.
pub fn sides_from<T: Copy>(values: &[T]) -> Option<Sides<T>> {
    match values {
        [only] => Some(Sides::all(*only)),
        [vertical, horizontal] => Some(Sides {
            top: *vertical,
            right: *horizontal,
            bottom: *vertical,
            left: *horizontal,
        }),
        [top, horizontal, bottom] => Some(Sides {
            top: *top,
            right: *horizontal,
            bottom: *bottom,
            left: *horizontal,
        }),
        [top, right, bottom, left] => Some(Sides {
            top: *top,
            right: *right,
            bottom: *bottom,
            left: *left,
        }),
        _ => None,
    }
}

// =============================================================================
// Lengths and sizes
// =============================================================================

/// A length as CSS writes it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Length {
    /// Pixels (the subset's only unit; a bare number is pixels too).
    Px(f32),
    /// A percentage of the containing block.
    Percent(f32),
    /// The `auto` keyword: let the layout decide.
    Auto,
}

impl Length {
    /// The length in pixels when it is one, for places with no percentage
    /// context.
    pub fn px(self) -> Option<f32> {
        match self {
            Length::Px(value) => Some(value),
            _ => None,
        }
    }
}

/// A width or height as CSS writes it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Size {
    /// A length: pixels, a percentage, or `auto`.
    Length(Length),
    /// The content's size, whatever the context offers.
    MaxContent,
    /// The smallest size the content can take.
    MinContent,
}

/// Parse one length: `12px`, `0`, `50%` or `auto`.
pub fn parse_length(text: &str) -> Option<Length> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("auto") {
        return Some(Length::Auto);
    }
    if let Some(percent) = text.strip_suffix('%') {
        return percent.trim().parse::<f32>().ok().map(Length::Percent);
    }
    if let Some(px) = text.strip_suffix("px") {
        return px.trim().parse::<f32>().ok().map(Length::Px);
    }
    // A bare number is pixels: the subset has no other unit and the
    // alternative is refusing every `margin: 0`.
    text.parse::<f32>().ok().map(Length::Px)
}

/// Parse one width or height, including the content keywords.
pub fn parse_size(text: &str) -> Option<Size> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("max-content") {
        return Some(Size::MaxContent);
    }
    if text.eq_ignore_ascii_case("min-content") {
        return Some(Size::MinContent);
    }
    parse_length(text).map(Size::Length)
}

// =============================================================================
// Colours
// =============================================================================

/// Parse a colour: `#rgb`, `#rrggbb`, `rgb(r, g, b)`, `rgba(r, g, b, a)`,
/// `transparent`, `currentcolor` or a small set of named colours.
pub fn parse_color(text: &str) -> Option<Color> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    let lower = text.to_ascii_lowercase();
    if let Some(inner) = lower.strip_prefix("rgb(").and_then(|v| v.strip_suffix(')')) {
        return parse_rgb_function(inner, false);
    }
    if let Some(inner) = lower
        .strip_prefix("rgba(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return parse_rgb_function(inner, true);
    }
    named_color(&lower)
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    let expanded = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 | 8 => hex.to_string(),
        _ => return None,
    };
    let byte = |index: usize| u8::from_str_radix(&expanded[index * 2..index * 2 + 2], 16).ok();
    let (r, g, b) = (byte(0)?, byte(1)?, byte(2)?);
    let a = if expanded.len() == 8 { byte(3)? } else { 255 };
    Some(Color::rgba(
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ))
}

fn parse_rgb_function(inner: &str, with_alpha: bool) -> Option<Color> {
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != if with_alpha { 4 } else { 3 } {
        return None;
    }
    let channel = |text: &str| -> Option<f32> {
        if let Some(percent) = text.strip_suffix('%') {
            Some(percent.trim().parse::<f32>().ok()? / 100.0)
        } else {
            Some(text.parse::<f32>().ok()? / 255.0)
        }
    };
    let r = channel(parts[0])?;
    let g = channel(parts[1])?;
    let b = channel(parts[2])?;
    let a = if with_alpha {
        parts[3].parse::<f32>().ok()?
    } else {
        1.0
    };
    Some(Color::rgba(r, g, b, a))
}

fn named_color(name: &str) -> Option<Color> {
    let rgba = match name {
        "transparent" => (0.0, 0.0, 0.0, 0.0),
        "white" => (1.0, 1.0, 1.0, 1.0),
        "black" => (0.0, 0.0, 0.0, 1.0),
        "red" => (1.0, 0.0, 0.0, 1.0),
        "green" => (0.0, 0.5, 0.0, 1.0),
        "blue" => (0.0, 0.0, 1.0, 1.0),
        "gray" | "grey" => (0.5, 0.5, 0.5, 1.0),
        "silver" => (0.75, 0.75, 0.75, 1.0),
        "orange" => (1.0, 0.65, 0.0, 1.0),
        "yellow" => (1.0, 1.0, 0.0, 1.0),
        "purple" => (0.5, 0.0, 0.5, 1.0),
        _ => return None,
    };
    Some(Color::rgba(rgba.0, rgba.1, rgba.2, rgba.3))
}

// =============================================================================
// Keywords
// =============================================================================

/// A number (opacity and friends), clamped into `range`.
pub fn parse_number(text: &str, range: std::ops::RangeInclusive<f32>) -> Option<f32> {
    let value = text.trim().parse::<f32>().ok()?;
    value
        .is_finite()
        .then_some(value.clamp(*range.start(), *range.end()))
}

// =============================================================================
// Display formatting (for reports)
// =============================================================================

impl fmt::Display for Length {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Length::Px(value) => write!(formatter, "{value}px"),
            Length::Percent(value) => write!(formatter, "{value}%"),
            Length::Auto => write!(formatter, "auto"),
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_split_at_top_level_only() {
        let parts = components("1px solid rgb(64, 64, 64)");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2].text, "rgb(64, 64, 64)");
        assert_eq!(parts[2].function(), Some(("rgb", "64, 64, 64")));

        let comma = components("0 6px 18px rgba(0, 0, 0, 0.42)");
        assert_eq!(comma.len(), 4);
        assert_eq!(comma[3].function().unwrap().0, "rgba");
        assert_eq!(
            words("Arial, sans-serif")
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            vec!["arial", "sans-serif"]
        );
    }

    #[test]
    fn lengths_parse_in_every_supported_shape() {
        assert_eq!(parse_length("12px"), Some(Length::Px(12.0)));
        assert_eq!(parse_length("0"), Some(Length::Px(0.0)));
        assert_eq!(parse_length("-2px"), Some(Length::Px(-2.0)));
        assert_eq!(parse_length("50%"), Some(Length::Percent(50.0)));
        assert_eq!(parse_length("auto"), Some(Length::Auto));
        assert_eq!(parse_length("1em"), None);
        assert_eq!(parse_size("max-content"), Some(Size::MaxContent));
    }

    #[test]
    fn colours_parse_in_every_supported_shape() {
        assert_eq!(parse_color("#fff"), Some(Color::WHITE));
        assert_eq!(
            parse_color("#102030"),
            Some(Color::rgba(
                0x10 as f32 / 255.0,
                0x20 as f32 / 255.0,
                0x30 as f32 / 255.0,
                1.0
            ))
        );
        let rgba = parse_color("rgba(255, 0, 0, 0.5)").unwrap();
        assert_eq!((rgba.r, rgba.g, rgba.b, rgba.a), (1.0, 0.0, 0.0, 0.5));
        let rgb = parse_color("rgb(0%, 50%, 100%)").unwrap();
        assert!((rgb.b - 1.0).abs() < f32::EPSILON);
        assert_eq!(
            parse_color("transparent"),
            Some(Color::rgba(0.0, 0.0, 0.0, 0.0))
        );
        assert_eq!(parse_color("chartreuse"), None);
    }

    #[test]
    fn sides_follow_the_css_shorthand() {
        assert_eq!(sides_from(&[1.0]), Some(Sides::all(1.0)));
        let two = sides_from(&[1.0, 2.0]).unwrap();
        assert_eq!(
            (two.top, two.right, two.bottom, two.left),
            (1.0, 2.0, 1.0, 2.0)
        );
        let four = sides_from(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!(
            (four.top, four.right, four.bottom, four.left),
            (1.0, 2.0, 3.0, 4.0)
        );
        assert_eq!(sides_from::<f32>(&[]), None);
    }

    #[test]
    fn numbers_clamp_into_range() {
        assert_eq!(parse_number("0.5", 0.0..=1.0), Some(0.5));
        assert_eq!(parse_number("2", 0.0..=1.0), Some(1.0));
        assert_eq!(parse_number("nope", 0.0..=1.0), None);
    }
}

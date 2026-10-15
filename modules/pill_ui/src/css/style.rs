//! The computed style a node ends up with, and the cascade that fills it.
//!
//! # Responsibilities
//!
//! - [`ComputedStyle`]: what styling decided for one node - paint properties
//!   (colours, fonts, outlines), layout properties (sizes, padding, flex)
//!   and the shape and overlay ones (radius, shadow, z-index).
//!   `None` means "not set by CSS; the built-in theme applies".
//! - [`apply`]: applies one matched declaration to a computed style;
//!   [`apply_inherit`]: the `inherit` keyword; [`inherit`]: copies the
//!   inheritable properties from a parent.
//!
//! # Design
//!
//! Styling never changes the theme: it only overrides fields. That is what
//! lets a UI run with no stylesheet (the whole Stage 8 behaviour), with the
//! default stylesheet (which re-states the theme) or with an editor
//! stylesheet, through exactly one code path (plan decision D7).

// Current crate
use crate::draw::Color;

use super::parse::{
    AlignKind, BorderSide, BoxSizingKind, Declaration, DisplayKind, FlexDirectionKind, JustifyKind,
    LineHeight, OverflowKind, PositionKind, Shadow, TextAlign, TextOverflow, WhiteSpace,
};
use super::value::{Length, Sides, Size};

/// What styling decided for one node; `None` fields fall back to the theme.
#[derive(Clone, Debug, PartialEq)]
pub struct ComputedStyle {
    /// The node's fill.
    pub background: Option<Color>,
    /// The text colour.
    pub color: Option<Color>,
    /// The node's opacity, multiplied down its subtree when painting.
    pub opacity: Option<f32>,
    /// The font size in pixels.
    pub font_size: Option<f32>,
    /// The line height.
    pub line_height: Option<LineHeight>,
    /// The font family.
    pub font_family: Option<String>,
    /// The font weight (parsed; the bundled font has one instance).
    pub font_weight: Option<u16>,
    /// Horizontal text placement.
    pub text_align: Option<TextAlign>,
    /// Text wrapping.
    pub white_space: Option<WhiteSpace>,
    /// What overflowing text does.
    pub text_overflow: Option<TextOverflow>,
    /// Padding, in pixels.
    pub padding: Option<Sides<f32>>,
    /// Margin.
    pub margin: Option<Sides<Length>>,
    /// Per-side border strokes; a side is unset until written.
    pub border: Sides<BorderSide>,
    /// Corner radius, kept for the rounded shader.
    pub border_radius: Option<f32>,
    /// The shadow to paint behind the node; an inset shadow is kept for the
    /// dock layout's left rules and not drawn yet.
    pub box_shadow: Option<Shadow>,
    /// The outline stroke: width and colour.
    pub outline: Option<(f32, Color)>,
    /// The outline's offset; negative pulls the stroke inside.
    pub outline_offset: Option<f32>,
    /// How widths count padding and border.
    pub box_sizing: Option<BoxSizingKind>,
    /// The layout display.
    pub display: Option<DisplayKind>,
    /// The main axis of a flex container.
    pub flex_direction: Option<FlexDirectionKind>,
    /// How much free space the node takes.
    pub flex_grow: Option<f32>,
    /// How much the node shrinks.
    pub flex_shrink: Option<f32>,
    /// The node's starting size on the main axis.
    pub flex_basis: Option<Size>,
    /// Cross-axis alignment of the container's items.
    pub align_items: Option<AlignKind>,
    /// Main-axis distribution of the container's items.
    pub justify_content: Option<JustifyKind>,
    /// Row and column gaps, in pixels.
    pub gap: Option<(f32, f32)>,
    /// Width.
    pub width: Option<Size>,
    /// Height.
    pub height: Option<Size>,
    /// Minimum width.
    pub min_width: Option<Size>,
    /// Minimum height.
    pub min_height: Option<Size>,
    /// Maximum width.
    pub max_width: Option<Size>,
    /// Maximum height.
    pub max_height: Option<Size>,
    /// How the node is positioned.
    pub position: Option<PositionKind>,
    /// Offsets from the edges.
    pub inset: Sides<Option<Length>>,
    /// Horizontal and vertical overflow behaviour.
    pub overflow: Option<(OverflowKind, OverflowKind)>,
    /// Stacking order, kept for the overlay layer.
    pub z_index: Option<i32>,
    /// The cursor name, kept for the platform layer.
    pub cursor: Option<String>,
    /// Whether the node takes the pointer at all.
    pub pointer_events: Option<bool>,
    /// Whether the node's text is selectable.
    pub user_select: Option<bool>,
    /// A scroll area's bar strip thickness in pixels; layout fills the
    /// theme's default in when nothing set it, so measuring, the drag math
    /// and painting all use one number.
    pub scrollbar_width: Option<f32>,
    /// A scrollbar's thumb colour.
    pub scrollbar_thumb: Option<Color>,
    /// The strip a scrollbar's thumb slides in.
    pub scrollbar_track: Option<Color>,

    /// The opacity after ancestors multiplied in; filled in while walking.
    pub opacity_effective: f32,
    /// Whether an ancestor (or the node) is `display: none`; filled in
    /// while walking.
    pub hidden: bool,
}

impl Default for ComputedStyle {
    fn default() -> ComputedStyle {
        ComputedStyle {
            background: None,
            color: None,
            opacity: None,
            font_size: None,
            line_height: None,
            font_family: None,
            font_weight: None,
            text_align: None,
            white_space: None,
            text_overflow: None,
            padding: None,
            margin: None,
            border: Sides::all(BorderSide::default()),
            border_radius: None,
            box_shadow: None,
            outline: None,
            outline_offset: None,
            box_sizing: None,
            display: None,
            flex_direction: None,
            flex_grow: None,
            flex_shrink: None,
            flex_basis: None,
            align_items: None,
            justify_content: None,
            gap: None,
            width: None,
            height: None,
            min_width: None,
            min_height: None,
            max_width: None,
            max_height: None,
            position: None,
            inset: Sides::all(None),
            overflow: None,
            z_index: None,
            cursor: None,
            pointer_events: None,
            user_select: None,
            scrollbar_width: None,
            scrollbar_thumb: None,
            scrollbar_track: None,
            opacity_effective: 1.0,
            hidden: false,
        }
    }
}

/// Apply one typed declaration.
pub fn apply(style: &mut ComputedStyle, declaration: &Declaration) {
    match declaration {
        Declaration::Background(color) => style.background = Some(*color),
        Declaration::Color(color) => style.color = Some(*color),
        Declaration::Opacity(value) => style.opacity = Some(*value),
        Declaration::FontSize(size) => style.font_size = Some(*size),
        Declaration::LineHeight(height) => style.line_height = Some(*height),
        Declaration::FontFamily(family) => style.font_family = Some(family.clone()),
        Declaration::FontWeight(weight) => style.font_weight = Some(*weight),
        Declaration::Font(spec) => {
            if let Some(size) = spec.size {
                style.font_size = Some(size);
            }
            if let Some(height) = spec.line_height {
                style.line_height = Some(height);
            }
            if let Some(weight) = spec.weight {
                style.font_weight = Some(weight);
            }
            if let Some(family) = &spec.family {
                style.font_family = Some(family.clone());
            }
        }
        Declaration::TextAlign(align) => style.text_align = Some(*align),
        Declaration::WhiteSpace(space) => style.white_space = Some(*space),
        Declaration::TextOverflow(overflow) => style.text_overflow = Some(*overflow),
        Declaration::Padding(sides) => merge_padding(style, sides),
        Declaration::Margin(sides) => merge_margin(style, sides),
        Declaration::Border(sides) => merge_border(style, sides),
        Declaration::BorderRadius(radius) => style.border_radius = Some(*radius),
        Declaration::BoxShadow(shadow) => style.box_shadow = Some(*shadow),
        Declaration::Outline { width, color } => style.outline = Some((*width, *color)),
        Declaration::OutlineOffset(offset) => style.outline_offset = Some(*offset),
        Declaration::BoxSizing(sizing) => style.box_sizing = Some(*sizing),
        Declaration::Display(display) => style.display = Some(*display),
        Declaration::FlexDirection(direction) => style.flex_direction = Some(*direction),
        Declaration::FlexGrow(grow) => style.flex_grow = Some(*grow),
        Declaration::FlexShrink(shrink) => style.flex_shrink = Some(*shrink),
        Declaration::FlexBasis(basis) => style.flex_basis = Some(*basis),
        Declaration::AlignItems(align) => style.align_items = Some(*align),
        Declaration::JustifyContent(justify) => style.justify_content = Some(*justify),
        Declaration::Gap { row, column } => {
            let current = style.gap.unwrap_or((0.0, 0.0));
            style.gap = Some((row.unwrap_or(current.0), column.unwrap_or(current.1)));
        }
        Declaration::Width(size) => style.width = Some(*size),
        Declaration::Height(size) => style.height = Some(*size),
        Declaration::MinWidth(size) => style.min_width = Some(*size),
        Declaration::MinHeight(size) => style.min_height = Some(*size),
        Declaration::MaxWidth(size) => style.max_width = Some(*size),
        Declaration::MaxHeight(size) => style.max_height = Some(*size),
        Declaration::Position(position) => style.position = Some(*position),
        Declaration::Inset(sides) => merge_inset(style, sides),
        Declaration::Overflow { x, y } => {
            let current = style
                .overflow
                .unwrap_or((OverflowKind::Visible, OverflowKind::Visible));
            style.overflow = Some((x.unwrap_or(current.0), y.unwrap_or(current.1)));
        }
        Declaration::ZIndex(z) => style.z_index = Some(*z),
        Declaration::Cursor(cursor) => style.cursor = Some(cursor.clone()),
        Declaration::PointerEvents(events) => style.pointer_events = Some(*events),
        Declaration::UserSelect(select) => style.user_select = Some(*select),
        Declaration::ScrollbarWidth(width) => style.scrollbar_width = Some(*width),
        Declaration::ScrollbarThumb(color) => style.scrollbar_thumb = Some(*color),
        Declaration::ScrollbarTrack(color) => style.scrollbar_track = Some(*color),
    }
}

/// Apply `inherit` for a property written by its name: the parent's computed
/// field is copied (a value of `None` still inherits nothing, which is the
/// same as the theme default on both sides).
pub fn apply_inherit(style: &mut ComputedStyle, name: &str, parent: &ComputedStyle) {
    match name {
        "color" => style.color = parent.color,
        "font" | "font-family" => style.font_family = parent.font_family.clone(),
        "font-size" => style.font_size = parent.font_size,
        "font-weight" => style.font_weight = parent.font_weight,
        "line-height" => style.line_height = parent.line_height,
        "text-align" => style.text_align = parent.text_align,
        "white-space" => style.white_space = parent.white_space,
        "text-overflow" => style.text_overflow = parent.text_overflow,
        "cursor" => style.cursor = parent.cursor.clone(),
        "user-select" => style.user_select = parent.user_select,
        "pointer-events" => style.pointer_events = parent.pointer_events,
        "background" | "background-color" => style.background = parent.background,
        "opacity" => style.opacity = parent.opacity,
        "padding" => style.padding = parent.padding,
        "margin" => style.margin = parent.margin,
        "border" => style.border = parent.border,
        "width" => style.width = parent.width,
        "height" => style.height = parent.height,
        "flex-direction" => style.flex_direction = parent.flex_direction,
        "align-items" => style.align_items = parent.align_items,
        "justify-content" => style.justify_content = parent.justify_content,
        // Everything else has no inheritable form in the subset; the
        // keyword is a no-op rather than an error, as CSS allows it on
        // every property.
        _ => {}
    }
}

/// Copy the inheritable properties from `parent` into a fresh style.
pub fn inherit(parent: &ComputedStyle, style: &mut ComputedStyle) {
    if style.color.is_none() {
        style.color = parent.color;
    }
    if style.font_size.is_none() {
        style.font_size = parent.font_size;
    }
    if style.line_height.is_none() {
        style.line_height = parent.line_height;
    }
    if style.font_family.is_none() {
        style.font_family = parent.font_family.clone();
    }
    if style.font_weight.is_none() {
        style.font_weight = parent.font_weight;
    }
    if style.text_align.is_none() {
        style.text_align = parent.text_align;
    }
    if style.white_space.is_none() {
        style.white_space = parent.white_space;
    }
    if style.text_overflow.is_none() {
        style.text_overflow = parent.text_overflow;
    }
    if style.cursor.is_none() {
        style.cursor = parent.cursor.clone();
    }
    if style.user_select.is_none() {
        style.user_select = parent.user_select;
    }
    if style.pointer_events.is_none() {
        style.pointer_events = parent.pointer_events;
    }
}

/// Overlay a shorthand's named sides onto the computed padding; unnamed
/// sides keep their value (the theme default starts as zero everywhere).
fn merge_padding(style: &mut ComputedStyle, sides: &Sides<Option<f32>>) {
    let mut merged = style.padding.unwrap_or(Sides::all(0.0));
    if let Some(value) = sides.top {
        merged.top = value;
    }
    if let Some(value) = sides.right {
        merged.right = value;
    }
    if let Some(value) = sides.bottom {
        merged.bottom = value;
    }
    if let Some(value) = sides.left {
        merged.left = value;
    }
    style.padding = Some(merged);
}

/// The margin twin of [`merge_padding`].
fn merge_margin(style: &mut ComputedStyle, sides: &Sides<Option<Length>>) {
    let mut merged = style.margin.unwrap_or(Sides::all(Length::Px(0.0)));
    if let Some(value) = sides.top {
        merged.top = value;
    }
    if let Some(value) = sides.right {
        merged.right = value;
    }
    if let Some(value) = sides.bottom {
        merged.bottom = value;
    }
    if let Some(value) = sides.left {
        merged.left = value;
    }
    style.margin = Some(merged);
}

fn merge_border(style: &mut ComputedStyle, sides: &Sides<BorderSide>) {
    let mut merged = style.border;
    for (target, incoming) in [
        (&mut merged.top, sides.top),
        (&mut merged.right, sides.right),
        (&mut merged.bottom, sides.bottom),
        (&mut merged.left, sides.left),
    ] {
        if incoming.width.is_some() {
            target.width = incoming.width;
        }
        if incoming.color.is_some() {
            target.color = incoming.color;
        }
    }
    style.border = merged;
}

fn merge_inset(style: &mut ComputedStyle, sides: &Sides<Option<Length>>) {
    let mut merged = style.inset;
    for (target, incoming) in [
        (&mut merged.top, sides.top),
        (&mut merged.right, sides.right),
        (&mut merged.bottom, sides.bottom),
        (&mut merged.left, sides.left),
    ] {
        if incoming.is_some() {
            *target = incoming;
        }
    }
    style.inset = merged;
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_override_fields() {
        let mut style = ComputedStyle::default();
        apply(&mut style, &Declaration::Color(Color::WHITE));
        apply(&mut style, &Declaration::FontSize(12.0));
        assert_eq!(style.color, Some(Color::WHITE));
        assert_eq!(style.font_size, Some(12.0));
        assert_eq!(style.background, None);
    }

    #[test]
    fn borders_merge_side_by_side() {
        let mut style = ComputedStyle::default();
        apply(
            &mut style,
            &Declaration::Border(Sides::all(BorderSide {
                width: Some(1.0),
                color: Some(Color::BLACK),
            })),
        );
        let mut bottom = Sides::all(BorderSide::default());
        bottom.bottom = BorderSide {
            width: Some(2.0),
            color: None,
        };
        apply(&mut style, &Declaration::Border(bottom));
        assert_eq!(style.border.top.width, Some(1.0));
        assert_eq!(style.border.bottom.width, Some(2.0));
        assert_eq!(style.border.bottom.color, Some(Color::BLACK));
    }

    #[test]
    fn inheritance_fills_unset_inheritable_fields_only() {
        let parent = ComputedStyle {
            color: Some(Color::WHITE),
            font_size: Some(12.0),
            background: Some(Color::BLACK),
            ..ComputedStyle::default()
        };

        let mut child = ComputedStyle {
            font_size: Some(16.0),
            ..ComputedStyle::default()
        };
        inherit(&parent, &mut child);
        assert_eq!(child.color, Some(Color::WHITE));
        assert_eq!(child.font_size, Some(16.0), "the child's own value wins");
        assert_eq!(child.background, None, "background is not inherited");

        apply_inherit(&mut child, "background", &parent);
        assert_eq!(child.background, Some(Color::BLACK));
    }

    #[test]
    fn gap_and_overflow_compose() {
        let mut style = ComputedStyle::default();
        apply(
            &mut style,
            &Declaration::Gap {
                row: Some(6.0),
                column: None,
            },
        );
        apply(
            &mut style,
            &Declaration::Gap {
                row: None,
                column: Some(8.0),
            },
        );
        assert_eq!(style.gap, Some((6.0, 8.0)));

        apply(
            &mut style,
            &Declaration::Overflow {
                x: Some(OverflowKind::Hidden),
                y: Some(OverflowKind::Auto),
            },
        );
        assert_eq!(
            style.overflow,
            Some((OverflowKind::Hidden, OverflowKind::Auto))
        );
    }
}

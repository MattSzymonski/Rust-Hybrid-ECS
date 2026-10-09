//! Painting a laid-out panel tree into a draw list.
//!
//! # Responsibilities
//!
//! - [`paint`]: walks a [`PanelTree`] and its [`PanelLayout`] together and
//!   appends every widget's rectangles and text to a [`DrawList`], keeping
//!   scroll-area clips balanced.
//! - The built-in default look: flat fills, 1px strokes and text, in the
//!   theme's colours; hover, press, focus, selection and disabled states
//!   are all visible.
//!
//! # Design
//!
//! Painting never decides anything the builder did not: hover, focus and
//! press come from the [`UiState`] the frame advanced, geometry from the
//! layout. The layout arrays are parallel to the tree's preorder, so one
//! walk with an index cursor drives both. Every glyph goes through
//! [`TextSystem`], which rasterizes into the shared [`Atlas`]; the list
//! itself only ever holds plain vertices, indices and commands, so any
//! backend can draw it.

// External crates
use pill_core::math::{Vector2f, Vector3f};

// Current crate
use crate::atlas::Atlas;
use crate::css::ComputedStyle;
use crate::draw::{Color, DrawList, Rect};
use crate::layout::{text_style, PanelLayout};
use crate::panel::{node_enabled, PanelNode, PanelTree};
use crate::state::UiState;
use crate::text::{TextStyle, TextSystem};
use crate::theme::Theme;

/// Width of a drag field's value box at the right of its rectangle.
const VALUE_BOX_WIDTH: f32 = 80.0;

// =============================================================================
// paint
// =============================================================================

/// Draw `tree` into `list` using the rectangles `layout` produced.
///
/// `state` supplies the pointer, the focus, the active drag and the text
/// fields' cursors; `text` and `atlas` receive the frame's glyphs.
pub fn paint(
    tree: &PanelTree,
    layout: &PanelLayout,
    theme: &Theme,
    state: &UiState,
    text: &mut TextSystem,
    atlas: &mut Atlas,
    list: &mut DrawList,
) {
    let mut nodes = Vec::new();
    collect(&tree.root, &mut nodes);

    let mut active_clip: Option<Rect> = None;
    for (index, node) in nodes.iter().enumerate() {
        let Some(rect) = layout.rects.get(index).copied() else {
            continue;
        };
        // Scroll-area clips change only at their boundaries; keep the draw
        // list's clip stack in step with the layout's.
        let node_clip = layout.clips.get(index).copied().flatten();
        if node_clip != active_clip {
            if active_clip.is_some() {
                list.pop_clip();
            }
            if let Some(clip) = node_clip {
                list.push_clip(clip);
            }
            active_clip = node_clip;
        }
        let css = &layout.styles[index];
        if css.hidden {
            // `display: none`: neither the node nor its subtree draws.
            continue;
        }
        draw_node(node, rect, theme, state, text, atlas, list, css);
    }
    if active_clip.is_some() {
        list.pop_clip();
    }
}

/// Collect every node in preorder, the order layout stored rectangles in.
fn collect<'a>(node: &'a PanelNode, out: &mut Vec<&'a PanelNode>) {
    out.push(node);
    for child in node.children() {
        collect(child, out);
    }
}

/// What the computed style asks painting to change: fills, text colours,
/// fonts, borders, outlines and the effective opacity.
#[derive(Copy, Clone, Debug)]
struct Look<'a> {
    /// The node's computed style.
    css: &'a ComputedStyle,
    /// The opacity after ancestors multiplied in.
    fade: f32,
}

impl<'a> Look<'a> {
    /// A look over one node's computed style.
    fn of(css: &'a ComputedStyle) -> Look<'a> {
        Look {
            css,
            fade: css.opacity_effective,
        }
    }

    /// A colour with the look's opacity folded into its alpha.
    fn fade(&self, color: Color) -> Color {
        if self.fade >= 1.0 {
            return color;
        }
        Color::rgba(color.r, color.g, color.b, color.a * self.fade)
    }

    /// The text colour: the computed one if set, else `base`.
    fn text(&self, base: Color) -> Color {
        self.fade(self.css.color.unwrap_or(base))
    }

    /// A default fill, overridden by `background` when the sheet set one.
    fn fill(&self, base: Color) -> Color {
        self.fade(self.css.background.unwrap_or(base))
    }

    /// The node's corner radius: the computed one when the sheet set it,
    /// else the widget's built-in `default`.
    fn radius(&self, default: f32) -> f32 {
        self.css.border_radius.unwrap_or(default)
    }

    /// The border a fill-and-border widget draws: the sheet's uniform border
    /// when all four sides agree, else the theme's ring in `default`.
    fn uniform_border(&self, theme: &Theme, default: Color) -> (f32, Color) {
        let sides = [
            self.css.border.top,
            self.css.border.right,
            self.css.border.bottom,
            self.css.border.left,
        ];
        let widths = sides.map(|side| side.width.unwrap_or(0.0));
        if widths[0] > 0.0 && widths.iter().all(|width| *width == widths[0]) {
            let color = sides
                .iter()
                .find_map(|side| side.color)
                .unwrap_or(theme.border);
            return (widths[0], color);
        }
        (theme.border_width, default)
    }

    /// A widget's fill with its border: the border as an outer rounded fill,
    /// the fill inside it. A zero radius draws square - the old
    /// fill-then-stroke pair, pixel for pixel.
    fn fill_rounded(
        &self,
        list: &mut DrawList,
        rect: Rect,
        radius: f32,
        width: f32,
        border: Color,
        fill: Color,
    ) {
        let radius = radius.max(0.0);
        if width <= 0.0 {
            list.push_rounded_fill(rect, fill, radius);
            return;
        }
        list.push_rounded_fill(rect, border, radius);
        let inner = Rect::new(
            rect.min + Vector2f::new(width, width),
            rect.max - Vector2f::new(width, width),
        );
        list.push_rounded_fill(inner, fill, (radius - width).max(0.0));
    }

    /// The text metrics the node paints with.
    fn style(&self, theme: &Theme, node: &PanelNode) -> TextStyle {
        text_style(node, theme, self.css)
    }

    /// The text metrics a widget's own labels paint with.
    fn label_style(&self, theme: &Theme) -> TextStyle {
        let font_size = self.css.font_size.unwrap_or(theme.font_size);
        let line_height = match self.css.line_height {
            Some(crate::css::LineHeight::Px(px)) => px,
            Some(crate::css::LineHeight::Factor(factor)) => factor * font_size,
            None => theme.line_height,
        };
        TextStyle {
            font_size,
            line_height: line_height.max(font_size),
            family: self.css.font_family.clone(),
        }
    }
}

/// How one widget is reacting this frame.
#[derive(Copy, Clone, Debug, Default)]
struct State {
    /// The pointer is over it.
    hovered: bool,
    /// The pointer is holding it.
    active: bool,
    /// It holds the keyboard focus.
    focused: bool,
    /// The caller disabled it.
    enabled: bool,
}

fn widget_state(node: &PanelNode, rect: Rect, state: &UiState) -> State {
    let id = node.id();
    let enabled = node_enabled(node);
    let hovered = enabled
        && id.is_some()
        && state
            .input
            .pointer()
            .map(|pointer| rect.contains(pointer))
            .unwrap_or(false);
    State {
        hovered,
        active: id.is_some() && state.interaction.active == id,
        focused: id.is_some() && state.interaction.focus == id,
        enabled,
    }
}

// =============================================================================
// Per-widget drawing
// =============================================================================

#[allow(clippy::too_many_arguments)]
fn draw_node(
    node: &PanelNode,
    rect: Rect,
    theme: &Theme,
    state: &UiState,
    text: &mut TextSystem,
    atlas: &mut Atlas,
    list: &mut DrawList,
    css: &ComputedStyle,
) {
    let interaction = widget_state(node, rect, state);
    let look = Look::of(css);

    // A `box-shadow` paints behind the node's own drawing; an inset shadow
    // is kept for later layers (the dock layout's left rules draw their own
    // rectangles). The blur's half extent approximates CSS's gaussian
    // spread: solid under the node, fading outward over `blur / 2` pixels.
    if let Some(shadow) = css.box_shadow {
        if !shadow.inset && shadow.color.a > 0.0 {
            let expansion = (shadow.blur * 0.5).max(0.0);
            let offset = Vector2f::new(shadow.offset_x, shadow.offset_y);
            let grow = Vector2f::new(expansion, expansion);
            let shadow_rect = Rect::new(rect.min + offset - grow, rect.max + offset + grow);
            let radius = css.border_radius.unwrap_or(0.0);
            list.push_soft_fill(
                shadow_rect,
                look.fade(shadow.color),
                radius + expansion,
                expansion,
            );
        }
    }
    match node {
        PanelNode::Row { .. } | PanelNode::Column { .. } | PanelNode::ScrollArea { .. } => {
            // Containers have no built-in fill; a stylesheet may give them
            // one, which is how panel backgrounds are drawn.
            if let Some(background) = css.background {
                list.push_rounded_fill(
                    rect,
                    look.fade(background),
                    css.border_radius.unwrap_or(0.0),
                );
            }
        }
        PanelNode::VectorField { .. } => {}
        PanelNode::Label { text: label } => {
            let style = look.style(theme, node);
            draw_aligned(
                list,
                atlas,
                text,
                label,
                rect,
                theme,
                &style,
                look.text(theme.text),
                css.text_align,
                css.white_space == Some(crate::css::WhiteSpace::Nowrap),
                css.text_overflow == Some(crate::css::TextOverflow::Ellipsis),
            );
        }
        PanelNode::Heading { text: label } => {
            let style = look.style(theme, node);
            draw_aligned(
                list,
                atlas,
                text,
                label,
                rect,
                theme,
                &style,
                look.text(theme.text),
                css.text_align,
                css.white_space == Some(crate::css::WhiteSpace::Nowrap),
                css.text_overflow == Some(crate::css::TextOverflow::Ellipsis),
            );
        }
        PanelNode::Separator => {
            list.push_fill(rect, look.fill(theme.border));
        }
        PanelNode::Image {
            texture,
            uv_min,
            uv_max,
            color,
            ..
        } => {
            list.push_quad(*texture, rect, *uv_min, *uv_max, look.fade(*color));
        }
        PanelNode::Button { label, .. } => {
            draw_button(list, atlas, text, rect, theme, label, interaction, &look)
        }
        PanelNode::Checkbox { label, checked, .. } => draw_checkbox(
            list,
            atlas,
            text,
            rect,
            theme,
            label,
            *checked,
            interaction,
            &look,
        ),
        PanelNode::Slider {
            label,
            value,
            min,
            max,
            ..
        } => draw_slider(
            list,
            atlas,
            text,
            rect,
            theme,
            label,
            *value,
            *min,
            *max,
            interaction,
        ),
        PanelNode::DragValue {
            label, value, step, ..
        } => draw_drag_value(
            list,
            atlas,
            text,
            rect,
            theme,
            label,
            *value,
            *step,
            interaction,
            &look,
        ),
        PanelNode::TextField {
            id, text: contents, ..
        } => draw_text_field(
            list,
            atlas,
            text,
            rect,
            theme,
            state,
            *id,
            contents,
            interaction,
            &look,
        ),
        PanelNode::ComboBox {
            selected, options, ..
        } => {
            let shown = options.get(*selected).map(String::as_str).unwrap_or("");
            draw_combo_box(list, atlas, text, rect, theme, shown, interaction, &look);
        }
        PanelNode::ColorField { color, .. } => {
            let swatch = Rect::new(
                Vector2f::new(rect.min.x, rect.min.y + 4.0),
                Vector2f::new(rect.min.x + 16.0, rect.max.y - 4.0),
            );
            list.push_fill(swatch, *color);
            stroke(list, swatch, theme.border_width, theme.border_strong);
        }
        PanelNode::Collapsing { title, open, .. } => {
            draw_collapsing(list, atlas, text, rect, theme, title, *open, interaction)
        }
        PanelNode::Table {
            headers,
            rows,
            selected,
            ..
        } => draw_table(
            list, atlas, text, rect, theme, headers, rows, *selected, state,
        ),
        PanelNode::TreeRow {
            label,
            open,
            has_children,
            selected,
            ..
        } => draw_tree_row(
            list,
            atlas,
            text,
            rect,
            theme,
            label,
            *open,
            *has_children,
            *selected,
            interaction,
        ),
    }

    // A stylesheet's `outline` draws over the node: the dock layout's
    // `:focus-visible` ring is exactly this.
    if let Some((width, color)) = css.outline {
        if width > 0.0 {
            let offset = css.outline_offset.unwrap_or(0.0);
            let outline_rect = Rect::new(
                Vector2f::new(rect.min.x - offset, rect.min.y - offset),
                Vector2f::new(rect.max.x + offset, rect.max.y + offset),
            );
            stroke(list, outline_rect, width, look.fade(color));
        }
    }
}

/// Draw a label or heading with the computed alignment; `nowrap` clips
/// instead of wrapping, and `ellipsis` cuts a too-wide single line to the
/// longest prefix that fits once an ellipsis is appended.
#[allow(clippy::too_many_arguments)]
fn draw_aligned(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    contents: &str,
    rect: Rect,
    _theme: &Theme,
    style: &TextStyle,
    color: Color,
    align: Option<crate::css::TextAlign>,
    nowrap: bool,
    ellipsis: bool,
) {
    let measured = text.measure(contents, style, None);
    let free = (rect.width() - measured.x).max(0.0);
    let x = rect.min.x
        + match align {
            Some(crate::css::TextAlign::Center) => free * 0.5,
            Some(crate::css::TextAlign::Right) => free,
            _ => 0.0,
        };
    let origin = Vector2f::new(x, rect.min.y);
    if nowrap {
        let drawn = if ellipsis {
            ellipsized(text, contents, style, rect.width())
        } else {
            contents.to_string()
        };
        list.push_clip(rect);
        draw_text(list, atlas, text, &drawn, origin, style, None, color);
        list.pop_clip();
    } else {
        draw_text(
            list,
            atlas,
            text,
            contents,
            origin,
            style,
            Some(rect.width()),
            color,
        );
    }
}

/// The longest prefix of `contents` that fits `width` once an ellipsis is
/// appended; `contents` unchanged when it already fits.
fn ellipsized(text: &mut TextSystem, contents: &str, style: &TextStyle, width: f32) -> String {
    /// The character the cut is marked with.
    const ELLIPSIS: &str = "…";

    if text.measure(contents, style, None).x <= width {
        return contents.to_string();
    }
    let budget = width - text.measure(ELLIPSIS, style, None).x;
    if budget <= 0.0 {
        return String::new();
    }
    let characters: Vec<char> = contents.chars().collect();
    let (mut low, mut high) = (0usize, characters.len());
    while low < high {
        let middle = (low + high).div_ceil(2);
        let candidate: String = characters[..middle].iter().collect();
        if text.measure(&candidate, style, None).x <= budget {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let mut fitted: String = characters[..low].iter().collect();
    fitted.push_str(ELLIPSIS);
    fitted
}

#[allow(clippy::too_many_arguments)]
fn draw_button(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    label: &str,
    interaction: State,
    look: &Look<'_>,
) {
    let background = if !interaction.enabled {
        look.fill(theme.inset)
    } else if interaction.active {
        look.fill(theme.button_active)
    } else if interaction.hovered {
        look.fill(theme.button_hover)
    } else {
        look.fill(theme.button)
    };
    let border = if interaction.focused {
        theme.highlight
    } else {
        theme.border_strong
    };
    let radius = look.radius(theme.radius_small);
    let (width, border) = look.uniform_border(theme, border);
    look.fill_rounded(list, rect, radius, width, border, background);
    let color = if interaction.enabled {
        look.text(theme.text)
    } else {
        look.text(theme.text_disabled)
    };
    let style = look.label_style(theme);
    let size = text.measure(label, &style, None);
    let origin = Vector2f::new(
        rect.min.x + (rect.width() - size.x) * 0.5,
        rect.min.y + (rect.height() - size.y) * 0.5,
    );
    draw_text(
        list,
        atlas,
        text,
        label,
        origin,
        &style,
        Some(rect.width()),
        color,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_checkbox(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    label: &str,
    checked: bool,
    interaction: State,
    look: &Look<'_>,
) {
    let box_size = 14.0;
    let top = rect.min.y + (rect.height() - box_size) * 0.5;
    let checkbox = Rect::from_min_size(
        Vector2f::new(rect.min.x, top),
        Vector2f::new(box_size, box_size),
    );
    let border = if interaction.focused {
        theme.highlight
    } else if checked {
        theme.check
    } else {
        theme.text_disabled
    };
    let radius = look.radius(theme.radius_tiny);
    let (width, border) = look.uniform_border(theme, border);
    look.fill_rounded(
        list,
        checkbox,
        radius,
        width,
        border,
        look.fill(theme.inset),
    );
    if checked {
        let mark = 6.0;
        let inset = (box_size - mark) * 0.5;
        list.push_fill(
            Rect::from_min_size(
                checkbox.min + Vector2f::new(inset, inset),
                Vector2f::new(mark, mark),
            ),
            if interaction.enabled {
                look.fade(theme.window)
            } else {
                theme.text_disabled
            },
        );
    }

    let color = if interaction.enabled {
        theme.text
    } else {
        theme.text_disabled
    };
    let style = label_style(theme);
    let origin = Vector2f::new(
        rect.min.x + box_size + theme.spacing * 0.5,
        rect.min.y + (rect.height() - theme.line_height) * 0.5,
    );
    draw_text(list, atlas, text, label, origin, &style, None, color);
}

#[allow(clippy::too_many_arguments)]
fn draw_slider(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    label: &str,
    value: f32,
    min: f32,
    max: f32,
    interaction: State,
) {
    let color = if interaction.enabled {
        theme.text_secondary
    } else {
        theme.text_disabled
    };
    let style = label_style(theme);
    let label_width = text.measure(label, &style, None).x;
    draw_text(
        list,
        atlas,
        text,
        label,
        Vector2f::new(
            rect.min.x,
            rect.min.y + (rect.height() - theme.line_height) * 0.5,
        ),
        &style,
        None,
        color,
    );

    let track_x0 = rect.min.x + label_width + theme.spacing;
    let track_x1 = rect.max.x;
    let mid = rect.min.y + rect.height() * 0.5;
    let track = Rect::new(
        Vector2f::new(track_x0, mid - 2.0),
        Vector2f::new(track_x1, mid + 2.0),
    );
    list.push_rounded_fill(track, theme.track, theme.radius_tiny);
    let fraction = if max > min {
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = Rect::new(
        track.min,
        Vector2f::new(track_x0 + track.width() * fraction, track.max.y),
    );
    list.push_rounded_fill(filled, theme.track_fill, theme.radius_tiny);

    let knob_x = track_x0 + track.width() * fraction;
    let knob = Rect::new(
        Vector2f::new(knob_x - 3.0, mid - 8.0),
        Vector2f::new(knob_x + 3.0, mid + 8.0),
    );
    let knob_color = if interaction.enabled {
        theme.knob
    } else {
        theme.text_disabled
    };
    let knob_border = if interaction.focused {
        theme.highlight
    } else {
        theme.track_fill
    };
    list.push_rounded_fill(knob, knob_border, theme.radius_tiny);
    let inner = Rect::new(
        knob.min + Vector2f::new(theme.border_width, theme.border_width),
        knob.max - Vector2f::new(theme.border_width, theme.border_width),
    );
    list.push_rounded_fill(
        inner,
        knob_color,
        (theme.radius_tiny - theme.border_width).max(0.0),
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_drag_value(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    label: &str,
    value: f32,
    step: f32,
    interaction: State,
    look: &Look<'_>,
) {
    let color = if interaction.enabled {
        theme.text
    } else {
        theme.text_disabled
    };
    let label_color = if interaction.enabled {
        theme.text_secondary
    } else {
        theme.text_disabled
    };
    let style = label_style(theme);
    draw_text(
        list,
        atlas,
        text,
        label,
        Vector2f::new(
            rect.min.x,
            rect.min.y + (rect.height() - theme.line_height) * 0.5,
        ),
        &style,
        None,
        label_color,
    );

    let box_x = (rect.max.x - VALUE_BOX_WIDTH).max(rect.min.x);
    let value_box = Rect::new(Vector2f::new(box_x, rect.min.y), rect.max);
    let background = if interaction.active {
        theme.active
    } else if interaction.hovered {
        theme.raised
    } else {
        theme.inset
    };
    list.push_fill(value_box, background);
    let border = if interaction.focused {
        theme.highlight
    } else {
        theme.border_strong
    };
    let radius = look.radius(theme.radius_small);
    let (width, border) = look.uniform_border(theme, border);
    look.fill_rounded(list, value_box, radius, width, border, background);

    let shown = format_value(value, step);
    let width = text.measure(&shown, &style, None).x;
    let origin = Vector2f::new(
        (value_box.max.x - 4.0 - width).max(value_box.min.x + 2.0),
        value_box.min.y + (value_box.height() - theme.line_height) * 0.5,
    );
    list.push_clip(value_box);
    draw_text(list, atlas, text, &shown, origin, &style, None, color);
    list.pop_clip();
}

/// An integer-looking value for whole steps, two decimals otherwise.
fn format_value(value: f32, step: f32) -> String {
    if step >= 1.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text_field(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    state: &UiState,
    id: crate::widget::WidgetId,
    contents: &str,
    interaction: State,
    look: &Look<'_>,
) {
    let border = if interaction.focused {
        theme.highlight
    } else {
        theme.border_strong
    };
    let radius = look.radius(theme.radius_small);
    let (width, border) = look.uniform_border(theme, border);
    look.fill_rounded(list, rect, radius, width, border, look.fill(theme.inset));

    let style = label_style(theme);
    let origin = Vector2f::new(
        rect.min.x + 4.0,
        rect.min.y + (rect.height() - theme.line_height) * 0.5,
    );
    let inner = Rect::new(
        Vector2f::new(rect.min.x + 4.0, rect.min.y + 2.0),
        Vector2f::new(rect.max.x - 4.0, rect.max.y - 2.0),
    );
    list.push_clip(inner);
    let color = if interaction.enabled {
        theme.text
    } else {
        theme.text_disabled
    };
    draw_text(list, atlas, text, contents, origin, &style, None, color);

    // Selection and caret, when the caller has given the field focus and an
    // editing state.
    if interaction.focused {
        if let Some(edit) = state.memory.get(&id).and_then(|memory| memory.text) {
            let (start, end) = edit.selection();
            let start = clamp_to_boundary(contents, start);
            let end = clamp_to_boundary(contents, end);
            let mut prefix = |byte: usize| text.measure(&contents[..byte], &style, None).x;
            if start != end {
                let selection = Rect::new(
                    Vector2f::new(origin.x + prefix(start), rect.min.y + 2.0),
                    Vector2f::new(origin.x + prefix(end), rect.max.y - 2.0),
                );
                list.push_fill(selection, theme.highlight_wash);
            }
            let caret_x = origin.x + prefix(clamp_to_boundary(contents, edit.cursor));
            let caret = Rect::new(
                Vector2f::new(caret_x, rect.min.y + 2.0),
                Vector2f::new(caret_x + 1.0, rect.max.y - 2.0),
            );
            list.push_fill(caret, theme.text);
        }
    }
    list.pop_clip();
}

/// `byte` clamped to the nearest char boundary at or before it.
fn clamp_to_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

#[allow(clippy::too_many_arguments)]
fn draw_combo_box(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    shown: &str,
    interaction: State,
    look: &Look<'_>,
) {
    let background = if interaction.active {
        theme.active
    } else if interaction.hovered {
        theme.raised
    } else {
        look.fill(theme.inset)
    };
    let border = if interaction.focused {
        theme.highlight
    } else {
        theme.border_strong
    };
    let radius = look.radius(theme.radius_small);
    let (width, border) = look.uniform_border(theme, border);
    look.fill_rounded(list, rect, radius, width, border, background);

    let style = label_style(theme);
    let color = if interaction.enabled {
        theme.text
    } else {
        theme.text_disabled
    };
    let origin = Vector2f::new(
        rect.min.x + 4.0,
        rect.min.y + (rect.height() - theme.line_height) * 0.5,
    );
    list.push_clip(rect);
    draw_text(list, atlas, text, shown, origin, &style, None, color);
    list.pop_clip();

    // A stepped marker stands in for the arrow glyph until the icon set
    // carries one.
    let marker_color = if interaction.enabled {
        theme.text_muted
    } else {
        theme.text_disabled
    };
    let mid = rect.min.y + rect.height() * 0.5;
    let x = rect.max.x - 12.0;
    for (index, width) in [8.0f32, 6.0, 4.0].iter().enumerate() {
        let y = mid - 3.0 + index as f32 * 2.0;
        list.push_fill(
            Rect::new(
                Vector2f::new(x + (8.0 - width) * 0.5, y),
                Vector2f::new(x + (8.0 + width) * 0.5, y + 2.0),
            ),
            marker_color,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_collapsing(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    title: &str,
    open: bool,
    interaction: State,
) {
    let header = Rect::new(
        rect.min,
        Vector2f::new(rect.max.x, rect.min.y + theme.item_height),
    );
    if interaction.active {
        list.push_fill(header, theme.active);
    } else if interaction.hovered {
        list.push_fill(header, theme.hover);
    }
    let marker = Rect::from_min_size(
        Vector2f::new(
            header.min.x + 2.0,
            header.min.y + (theme.item_height - 10.0) * 0.5,
        ),
        Vector2f::new(10.0, 10.0),
    );
    list.push_fill(marker, theme.inset);
    stroke(
        list,
        marker,
        theme.border_width,
        if interaction.focused {
            theme.highlight
        } else {
            theme.border_strong
        },
    );
    let inner = if open {
        Rect::from_min_size(
            marker.min + Vector2f::new(2.0, 2.0),
            Vector2f::new(6.0, 6.0),
        )
    } else {
        Rect::from_min_size(
            marker.min + Vector2f::new(2.0, 4.0),
            Vector2f::new(6.0, 2.0),
        )
    };
    list.push_fill(inner, theme.text_muted);

    let style = label_style(theme);
    let origin = Vector2f::new(
        header.min.x + 16.0,
        header.min.y + (theme.item_height - theme.line_height) * 0.5,
    );
    draw_text(list, atlas, text, title, origin, &style, None, theme.text);
}

#[allow(clippy::too_many_arguments)]
fn draw_table(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    headers: &[String],
    rows: &[Vec<String>],
    selected: Option<usize>,
    state: &UiState,
) {
    let style = label_style(theme);
    let row_height = theme.item_height;
    let columns = headers.len().max(1);
    let column_width = rect.width() / columns as f32;

    let header = Rect::new(rect.min, Vector2f::new(rect.max.x, rect.min.y + row_height));
    list.push_fill(header, theme.raised);
    for (column, title) in headers.iter().enumerate() {
        let cell = cell_rect(header, column, column_width);
        list.push_clip(cell);
        draw_text(
            list,
            atlas,
            text,
            title,
            Vector2f::new(cell.min.x + 4.0, cell.min.y + 3.0),
            &style,
            None,
            theme.text_muted,
        );
        list.pop_clip();
    }

    for (index, row) in rows.iter().enumerate() {
        let top = rect.min.y + row_height * (index as f32 + 1.0);
        let row_rect = Rect::new(
            Vector2f::new(rect.min.x, top),
            Vector2f::new(rect.max.x, top + row_height),
        );
        let hovered = state
            .input
            .pointer()
            .map(|pointer| row_rect.contains(pointer))
            .unwrap_or(false);
        if selected == Some(index) {
            list.push_rounded_fill(row_rect, theme.highlight_wash, theme.radius_small);
        } else if hovered {
            list.push_rounded_fill(row_rect, theme.hover, theme.radius_small);
        }
        for (column, cell_text) in row.iter().enumerate() {
            let cell = cell_rect(row_rect, column, column_width);
            list.push_clip(cell);
            draw_text(
                list,
                atlas,
                text,
                cell_text,
                Vector2f::new(cell.min.x + 4.0, cell.min.y + 3.0),
                &style,
                None,
                theme.text_secondary,
            );
            list.pop_clip();
        }
    }
}

/// The rectangle of column `column` within `row`.
fn cell_rect(row: Rect, column: usize, column_width: f32) -> Rect {
    let x = row.min.x + column_width * column as f32;
    Rect::new(
        Vector2f::new(x, row.min.y),
        Vector2f::new(x + column_width, row.max.y),
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_tree_row(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    rect: Rect,
    theme: &Theme,
    label: &str,
    open: bool,
    has_children: bool,
    selected: bool,
    interaction: State,
) {
    if selected {
        list.push_rounded_fill(rect, theme.highlight_wash, theme.radius_small);
        let rule = Rect::new(rect.min, Vector2f::new(rect.min.x + 2.0, rect.max.y));
        list.push_fill(rule, theme.highlight);
    } else if interaction.hovered {
        list.push_rounded_fill(rect, theme.hover, theme.radius_small);
    }
    let style = label_style(theme);
    let mut text_x = rect.min.x + 2.0;
    if has_children {
        let marker = Rect::from_min_size(
            Vector2f::new(text_x, rect.min.y + (rect.height() - 8.0) * 0.5),
            Vector2f::new(8.0, 8.0),
        );
        list.push_fill(marker, theme.inset);
        let inner = if open {
            Rect::from_min_size(
                marker.min + Vector2f::new(1.5, 1.5),
                Vector2f::new(5.0, 5.0),
            )
        } else {
            Rect::from_min_size(
                marker.min + Vector2f::new(3.0, 1.5),
                Vector2f::new(2.0, 5.0),
            )
        };
        list.push_fill(inner, theme.text_muted);
        text_x += 12.0;
    }
    let label_color = if selected {
        theme.text
    } else {
        theme.text_secondary
    };
    list.push_clip(rect);
    draw_text(
        list,
        atlas,
        text,
        label,
        Vector2f::new(
            text_x,
            rect.min.y + (rect.height() - theme.line_height) * 0.5,
        ),
        &style,
        None,
        label_color,
    );
    list.pop_clip();
}

// =============================================================================
// Small helpers
// =============================================================================

/// A one-pixel (or `width`-pixel) frame around `rect`, drawn as four fills.
fn stroke(list: &mut DrawList, rect: Rect, width: f32, color: Color) {
    let top = Rect::new(rect.min, Vector2f::new(rect.max.x, rect.min.y + width));
    let bottom = Rect::new(Vector2f::new(rect.min.x, rect.max.y - width), rect.max);
    let left = Rect::new(rect.min, Vector2f::new(rect.min.x + width, rect.max.y));
    let right = Rect::new(
        Vector2f::new(rect.max.x - width, rect.min.y),
        Vector2f::new(rect.max.x, rect.max.y),
    );
    for edge in [top, bottom, left, right] {
        list.push_fill(edge, color);
    }
}

fn label_style(theme: &Theme) -> TextStyle {
    TextStyle {
        font_size: theme.font_size,
        line_height: theme.line_height,
        family: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    list: &mut DrawList,
    atlas: &mut Atlas,
    text: &mut TextSystem,
    contents: &str,
    position: Vector2f,
    style: &TextStyle,
    max_width: Option<f32>,
    color: Color,
) {
    text.draw_text(list, atlas, contents, position, style, max_width, color);
}

/// A vector formatted component by component, for tests and callers that
/// want the field's text.
pub fn format_vector(value: Vector3f) -> String {
    format!("({:.2}, {:.2}, {:.2})", value.x, value.y, value.z)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawCommand, TextureId, UiVertex};
    use crate::layout::run_frame;
    use crate::panel::{PanelBuilder, TreeNode};
    use crate::widget::WidgetId;
    use pill_engine_core::{ButtonState, InputEvent, MouseButton};

    fn pointer_event(x: f32, y: f32) -> InputEvent {
        InputEvent::CursorMoved {
            position: Vector2f::new(x, y),
        }
    }

    fn click(state: ButtonState) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            state,
        }
    }

    /// One frame painted into a fresh draw list.
    fn painted_frame(
        state: &mut UiState,
        text: &mut TextSystem,
        events: &[InputEvent],
        panel: impl FnOnce(&mut PanelBuilder<'_>),
    ) -> DrawList {
        for event in events {
            state.input.apply(event);
        }
        state.input.begin_frame();
        let theme = Theme::default();
        let mut sheet = crate::css::Stylesheet::empty();
        let (output, layout) = run_frame(
            state,
            text,
            &theme,
            &mut sheet,
            Vector2f::new(400.0, 300.0),
            panel,
        );
        let mut atlas = Atlas::new(512, 512);
        let mut list = DrawList::new();
        paint(
            &output.tree,
            &layout,
            &theme,
            state,
            text,
            &mut atlas,
            &mut list,
        );
        list
    }

    fn build_every_widget(ui: &mut PanelBuilder<'_>) {
        ui.label("label");
        ui.heading("heading");
        ui.separator();
        ui.image("icon", TextureId::External(7), Vector2f::new(16.0, 16.0));
        ui.button("button");
        ui.checkbox("checkbox", true);
        ui.slider("slider", 0.25, 0.0..=1.0);
        ui.drag_value("drag", 3.0, 1.0);
        let mut field = String::from("edit me");
        ui.text_field("field", &mut field);
        ui.combo_box("combo", 0, &["one", "two"]);
        ui.color_field("color", Color::rgba(0.1, 0.2, 0.3, 1.0));
        ui.vector_field("vector", Vector3f::new(1.0, 2.0, 3.0));
        ui.collapsing("collapsing", true, |ui| {
            ui.label("inside");
        });
        ui.scroll_area("scroll", 60.0, |ui| {
            ui.label("one");
            ui.label("two");
        });
        ui.table("table", &["a", "b"], &[&["1", "2"], &["3", "4"]]);
        ui.tree(
            "tree",
            &[TreeNode::with_children(
                "root",
                vec![TreeNode::new("child")],
            )],
        );
    }

    #[test]
    fn a_narrow_nowrap_label_cuts_to_an_ellipsis() {
        let mut text = TextSystem::new();
        let style = TextStyle::default();
        let long = "a very long label that cannot possibly fit here";
        let fitted = ellipsized(&mut text, long, &style, 80.0);
        assert!(fitted.ends_with('…'), "the cut is marked: {fitted:?}");
        assert!(
            text.measure(&fitted, &style, None).x <= 80.0,
            "the fitted line stays inside the width: {fitted:?}"
        );
        assert!(fitted.len() < long.len());
        assert_eq!(ellipsized(&mut text, "small", &style, 300.0), "small");
    }

    #[test]
    fn a_sheet_radius_rounds_the_button_fill() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let mut sheet = crate::css::Stylesheet::parse("button { border-radius: 4px; }");
        assert!(sheet.reports().is_empty());
        state.input.begin_frame();
        let theme = Theme::default();
        let (output, layout) = run_frame(
            &mut state,
            &mut text,
            &theme,
            &mut sheet,
            Vector2f::new(200.0, 80.0),
            |ui| {
                ui.button("rounded");
            },
        );
        let mut atlas = Atlas::new(64, 64);
        let mut list = DrawList::new();
        paint(
            &output.tree,
            &layout,
            &theme,
            &state,
            &mut text,
            &mut atlas,
            &mut list,
        );
        assert!(
            list.vertices.iter().any(|vertex| vertex.shape[2] == 4.0),
            "the border fill carries the sheet's radius"
        );
        assert!(
            list.vertices.iter().any(|vertex| vertex.shape[2] == 3.0),
            "the inner fill carries the radius minus the border width"
        );
    }

    #[test]
    fn every_widget_kind_draws_headlessly() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let list = painted_frame(&mut state, &mut text, &[], build_every_widget);

        assert!(!list.is_empty(), "the frame drew something");
        assert!(
            list.vertices.len() > 100,
            "every widget contributed vertices, got {}",
            list.vertices.len()
        );
        let atlas_draws = list
            .commands
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    DrawCommand::Draw {
                        texture: TextureId::Atlas,
                        ..
                    }
                )
            })
            .count();
        assert!(atlas_draws > 0, "text was drawn from the glyph atlas");
        let external_draws = list
            .commands
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    DrawCommand::Draw {
                        texture: TextureId::External(7),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(external_draws, 1, "the image sampled its texture");
    }

    #[test]
    fn scroll_areas_push_and_pop_their_clip() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let list = painted_frame(&mut state, &mut text, &[], |ui| {
            ui.label("outside");
            ui.scroll_area("scroller", 60.0, |ui| {
                ui.label("inside");
            });
            ui.label("after");
        });
        let pushes = list
            .commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::PushClip(_)))
            .count();
        let pops = list
            .commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::PopClip))
            .count();
        assert_eq!(pushes, pops, "clips stay balanced");
        assert!(pushes >= 1, "the scroll area clipped its contents");
    }

    #[test]
    fn a_focused_text_field_draws_its_caret() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let id = WidgetId::from_name("field");
        let mut field = String::from("abc");

        // One frame to lay out, then focus without typing.
        painted_frame(&mut state, &mut text, &[], |ui| {
            let mut copy = field.clone();
            ui.text_field("field", &mut copy);
        });
        state.set_focus(id);
        state.memory.entry(id).or_default().text = Some(crate::widget::TextEditState {
            cursor: 1,
            anchor: 1,
        });
        let list = painted_frame(&mut state, &mut text, &[], |ui| {
            let mut copy = field.clone();
            ui.text_field("field", &mut copy);
        });

        // A caret is a thin solid rectangle the theme colours with the text
        // colour: one vertex set wider than tall, one pixel across.
        let caret = list.vertices.chunks_exact(4).any(|quad| {
            let width = (quad[1].position.x - quad[0].position.x).abs();
            let height = (quad[3].position.y - quad[0].position.y).abs();
            width <= 1.0 && height > 4.0 && quad[0].color == Theme::default().text
        });
        assert!(caret, "the focused field draws a caret");
        field.clear();
    }

    #[test]
    fn disabled_widgets_use_the_disabled_text_colour() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let list = painted_frame(&mut state, &mut text, &[], |ui| {
            ui.button_enabled("off", false);
            ui.button_enabled("on", true);
        });
        let theme = Theme::default();
        let has_disabled = list
            .vertices
            .iter()
            .any(|vertex| vertex.color == theme.text_disabled);
        let has_regular = list
            .vertices
            .iter()
            .any(|vertex| vertex.color == theme.text);
        assert!(has_disabled, "the disabled button's label is muted");
        assert!(has_regular, "the enabled button's label is regular");
    }

    #[test]
    fn a_press_changes_the_button_fill() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let id = WidgetId::from_name("Bake");
        painted_frame(&mut state, &mut text, &[], |ui| {
            ui.button("Bake");
        });
        let rect = state.hit_rects[&id];

        let list = painted_frame(
            &mut state,
            &mut text,
            &[
                pointer_event(rect.min.x + 2.0, rect.min.y + 2.0),
                click(ButtonState::Pressed),
            ],
            |ui| {
                ui.button("Bake");
            },
        );
        let held = Theme::default().button_active;
        let pressed_fill = list
            .vertices
            .chunks_exact(4)
            .any(|quad| quad[0].color == held);
        assert!(
            pressed_fill,
            "the held button is filled with the pressed fill"
        );
    }

    #[test]
    fn format_value_respects_the_step() {
        assert_eq!(format_value(3.4, 1.0), "3");
        assert_eq!(format_value(3.456, 0.1), "3.46");
        assert_eq!(
            format_vector(Vector3f::new(1.0, 2.5, -0.25)),
            "(1.00, 2.50, -0.25)"
        );
    }

    #[test]
    fn a_hovered_table_row_is_highlighted() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let id = WidgetId::from_name("table");
        painted_frame(&mut state, &mut text, &[], |ui| {
            ui.table("table", &["a"], &[&["1"], &["2"]]);
        });
        let rect = state.hit_rects[&id];
        let row_y = rect.min.y + Theme::default().item_height * 1.5;
        let list = painted_frame(
            &mut state,
            &mut text,
            &[pointer_event(rect.min.x + 5.0, row_y)],
            |ui| {
                ui.table("table", &["a"], &[&["1"], &["2"]]);
            },
        );
        let hover = Theme::default().hover;
        let highlighted = list
            .vertices
            .chunks_exact(4)
            .any(|quad: &[UiVertex]| quad[0].color == hover);
        assert!(highlighted, "the hovered row is highlighted");
    }

    #[test]
    fn a_stylesheet_restyles_the_ui() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let mut sheet = crate::css::Stylesheet::parse(
            "
            .quiet label { color: rgb(255, 0, 0); }
            .primary button { background: rgb(0, 255, 0); }
            ",
        );
        assert!(sheet.reports().is_empty(), "{:?}", sheet.reports());

        state.input.begin_frame();
        let theme = Theme::default();
        let (output, layout) = crate::layout::run_frame(
            &mut state,
            &mut text,
            &theme,
            &mut sheet,
            Vector2f::new(400.0, 300.0),
            |ui| {
                ui.class("quiet");
                ui.column(|ui| {
                    ui.label("styled label");
                });
                ui.class("primary");
                ui.column(|ui| {
                    ui.button("styled button");
                });
            },
        );
        let mut atlas = Atlas::new(2048, 2048);
        let mut list = DrawList::new();
        paint(
            &output.tree,
            &layout,
            &theme,
            &state,
            &mut text,
            &mut atlas,
            &mut list,
        );

        let red = Color::rgba(1.0, 0.0, 0.0, 1.0);
        let green = Color::rgba(0.0, 1.0, 0.0, 1.0);
        assert!(
            list.vertices.iter().any(|vertex| vertex.color == red),
            "the label's text colour came from the stylesheet"
        );
        assert!(
            list.vertices
                .chunks_exact(4)
                .any(|quad| quad[0].color == green),
            "the button's fill came from the stylesheet"
        );
    }

    #[test]
    fn display_none_paints_nothing_of_its_subtree() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let mut sheet = crate::css::Stylesheet::parse(".gone { display: none; }");
        state.input.begin_frame();
        let theme = Theme::default();
        let (output, layout) = crate::layout::run_frame(
            &mut state,
            &mut text,
            &theme,
            &mut sheet,
            Vector2f::new(400.0, 300.0),
            |ui| {
                ui.class("gone");
                ui.column(|ui| {
                    ui.label("never drawn");
                    ui.button("ghost");
                });
            },
        );
        assert!(
            layout.interactive.is_empty(),
            "hidden widgets take no input"
        );
        let mut atlas = Atlas::new(256, 256);
        let mut list = DrawList::new();
        paint(
            &output.tree,
            &layout,
            &theme,
            &state,
            &mut text,
            &mut atlas,
            &mut list,
        );
        assert!(list.vertices.is_empty(), "a hidden subtree paints nothing");
    }
}

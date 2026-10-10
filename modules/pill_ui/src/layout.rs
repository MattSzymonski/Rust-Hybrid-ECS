//! Laying a panel tree out: `taffy` styles in, rectangles out.
//!
//! # Responsibilities
//!
//! - [`compute`]: turns a [`PanelTree`] into a [`PanelLayout`] - one
//!   rectangle per node, in paint order, the clip each node sits under, and
//!   the hit rectangles the next frame interacts against.
//! - [`run_frame`]: one complete headless frame (build, lay out, store the
//!   hit rectangles), the driver tests and the shell share.
//! - Text measurement: label and heading sizes come from
//!   [`TextSystem::measure`], so a label wraps exactly as painting will draw
//!   it.
//!
//! # Design
//!
//! `taffy` never leaves this module: the panel tree maps onto a `taffy`
//! tree one-to-one in preorder, the layout is read back in the same order,
//! and everything downstream only sees rectangles. Scroll areas are plain
//! flex columns whose contents overflow; a post-pass shifts their children
//! by the stored offset and records the clip and the clipped hit rectangles,
//! which is what makes a scrolled-out widget stop reacting.

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_core::math::Vector2f;
use taffy::compute::compute_leaf_layout;
use taffy::prelude::{
    AvailableSpace, Dimension, Display, FlexDirection, JustifyContent, LengthPercentage,
    LengthPercentageAuto, NodeId, Size as TaffySize, Style, TaffyTree,
};
use taffy::{AlignItems, BoxSizing, Overflow, Point, Position, Rect as TaffyRect};

// Current crate
use crate::css::parse::{
    AlignKind, DisplayKind, FlexDirectionKind, JustifyKind, OverflowKind, PositionKind,
};
use crate::css::value::Length;
use crate::css::{ComputedStyle, NodeFacts, Stylesheet};
use crate::draw::Rect;
use crate::panel::{node_enabled, PanelBuilder, PanelNode, PanelOutput, PanelTree};
use crate::state::UiState;
use crate::text::{TextStyle, TextSystem};
use crate::theme::Theme;
use crate::widget::WidgetId;

/// The swatch zone at the left of a colour field.
const COLOR_SWATCH_WIDTH: f32 = 24.0;

// =============================================================================
// PanelLayout
// =============================================================================

/// One frame's geometry and styling: where every node ended up and what it
/// computed to.
#[derive(Clone, Debug, Default)]
pub struct PanelLayout {
    /// The root's size.
    pub size: Vector2f,
    /// One rectangle per node, in the tree's preorder (paint order).
    pub rects: Vec<Rect>,
    /// The clip each node sits under, parallel to `rects`.
    pub clips: Vec<Option<Rect>>,
    /// The interactive id of each node, parallel to `rects`, for
    /// introspection and tests.
    pub ids: Vec<Option<WidgetId>>,
    /// Each node's computed style, parallel to `rects`, with
    /// `opacity_effective` and `hidden` filled in while walking.
    pub styles: Vec<ComputedStyle>,
    /// Where each interactive widget can be hit, already clipped away from
    /// scrolled-out positions.
    pub interactive: BTreeMap<WidgetId, Rect>,
}

impl PanelLayout {
    /// Store this layout's hit rectangles as the state's, so the next frame
    /// interacts against them.
    pub fn store_interaction(&self, state: &mut UiState) {
        state.hit_rects = self.interactive.clone();
    }
}

// =============================================================================
// run_frame
// =============================================================================

/// Run one complete headless frame: build `panel`, lay it out, and store the
/// hit rectangles in `state`.
///
/// The caller has queued this frame's input events and called
/// [`UiInput::begin_frame`](crate::input::UiInput::begin_frame); what comes
/// back is the tree, its geometry and styles, and the widgets' events.
pub fn run_frame(
    state: &mut UiState,
    text: &mut TextSystem,
    theme: &Theme,
    sheet: &mut Stylesheet,
    available: Vector2f,
    panel: impl FnOnce(&mut PanelBuilder<'_>),
) -> (PanelOutput, PanelLayout) {
    let output = {
        let mut builder = PanelBuilder::new(state, theme);
        panel(&mut builder);
        builder.finish()
    };
    let layout = compute(&output.tree, text, theme, sheet, available, state);
    layout.store_interaction(state);
    (output, layout)
}

// =============================================================================
// compute
// =============================================================================

/// Lay `tree` out into at most `available` logical pixels, styled by
/// `sheet` (an empty stylesheet means the built-in theme's look).
///
/// Reads the stored scroll offsets from `state` (clamping them to the
/// content's real height) but does not write anything to it; call
/// [`PanelLayout::store_interaction`] afterwards to hand the next frame its
/// hit rectangles.
pub fn compute(
    tree: &PanelTree,
    text: &mut TextSystem,
    theme: &Theme,
    sheet: &mut Stylesheet,
    available: Vector2f,
    state: &UiState,
) -> PanelLayout {
    let styles = resolve_styles(tree, sheet, state);
    let mut taffy: TaffyTree<TextMeasure> = TaffyTree::new();
    let mut order = Vec::new();
    let mut cursor = 0usize;
    let root = build_node(
        &mut taffy,
        &tree.root,
        theme,
        &styles,
        &mut cursor,
        0.0,
        true,
        &mut order,
    );

    taffy
        .compute_layout_with_measure(
            root,
            TaffySize {
                width: AvailableSpace::Definite(available.x),
                height: AvailableSpace::Definite(available.y),
            },
            |known, _node, context, style| {
                // Style sizes resolve first; the measure closure only sizes
                // text leaves, treating every other leaf as empty content.
                compute_leaf_layout(
                    known,
                    style,
                    |_, value| value,
                    |known_dimensions, space| {
                        let Some(context) = context else {
                            return TaffySize {
                                width: 0.0,
                                height: 0.0,
                            };
                        };
                        let wrap = if context.wrap {
                            known_dimensions.width.or(match space.width {
                                AvailableSpace::Definite(width) => Some(width),
                                _ => None,
                            })
                        } else {
                            None
                        };
                        let measured = text.measure(&context.text, &context.style, wrap);
                        TaffySize {
                            width: measured.x,
                            height: measured.y,
                        }
                    },
                )
            },
        )
        .expect("panel trees always lay out");

    let mut layout = PanelLayout {
        styles,
        ..PanelLayout::default()
    };
    let mut cursor = 0usize;
    let root_rect = walk(
        &taffy,
        &tree.root,
        &order,
        &mut cursor,
        Vector2f::ZERO,
        Vector2f::ZERO,
        None,
        state,
        &mut layout,
    );
    layout.size = Vector2f::new(root_rect.width(), root_rect.height());
    layout
}

/// Walk the panel tree and the `taffy` tree together, in the preorder the
/// build recorded, collecting each node's absolute rectangle.
#[allow(clippy::too_many_arguments)]
fn walk(
    taffy: &TaffyTree<TextMeasure>,
    node: &PanelNode,
    order: &[NodeId],
    cursor: &mut usize,
    parent_origin: Vector2f,
    shift: Vector2f,
    clip: Option<Rect>,
    state: &UiState,
    layout: &mut PanelLayout,
) -> Rect {
    let node_id = order[*cursor];
    *cursor += 1;
    let index = *cursor - 1;
    let (hidden, takes_pointer, clips_children) = {
        let css = &layout.styles[index];
        (
            css.hidden,
            css.pointer_events != Some(false),
            clips_overflow(css),
        )
    };
    let geometry = taffy.layout(node_id).expect("laid out above");
    let local = Vector2f::new(geometry.location.x, geometry.location.y);
    let rect = Rect::from_min_size(
        parent_origin + local + shift,
        Vector2f::new(geometry.size.width, geometry.size.height),
    );
    layout.rects.push(rect);
    layout.clips.push(clip);
    layout.ids.push(node.id());
    if let Some(id) = node.id() {
        let hit = match clip {
            Some(clip) => rect.intersection(&clip),
            None => Some(rect),
        };
        if (!hidden && takes_pointer).then_some(()).is_some() {
            if let Some(hit) = hit {
                layout.interactive.insert(id, hit);
            }
        }
    }

    // Children: CSS `overflow` clips a subtree to the node's rectangle; a
    // scroll area also shifts its contents by the stored offset.
    let own_clip = if clips_children {
        match clip {
            Some(clip) => rect.intersection(&clip),
            None => Some(rect),
        }
    } else {
        clip
    };
    match node {
        PanelNode::ScrollArea { id, .. } => {
            // A scroll area always clips its contents, whether or not the
            // stylesheet says so; children first, at their natural
            // positions, so the content's real height tells the offset how
            // far it may go.
            let scroll_clip = match own_clip {
                Some(clip) => Some(clip),
                None => match clip {
                    Some(clip) => rect.intersection(&clip),
                    None => Some(rect),
                },
            };
            let start = layout.rects.len();
            for child in node.children() {
                walk(
                    taffy,
                    child,
                    order,
                    cursor,
                    parent_origin + local,
                    shift,
                    scroll_clip,
                    state,
                    layout,
                );
            }
            let mut content_bottom = rect.min.y;
            for child_rect in &layout.rects[start..] {
                content_bottom = content_bottom.max(child_rect.max.y);
            }
            let max_offset = (content_bottom - rect.min.y - rect.height()).max(0.0);
            let offset = state
                .memory
                .get(id)
                .map(|memory| memory.scroll)
                .unwrap_or(0.0)
                .clamp(0.0, max_offset);
            if offset != 0.0 {
                for index in start..layout.rects.len() {
                    layout.rects[index].min.y -= offset;
                    layout.rects[index].max.y -= offset;
                    // Hit rectangles were stored at natural positions;
                    // store them again where the shift left the widget.
                    if let Some(id) = layout.ids[index] {
                        let moved = layout.rects[index];
                        let hit = match layout.clips[index] {
                            Some(clip) => moved.intersection(&clip),
                            None => Some(moved),
                        };
                        match hit {
                            Some(hit) => layout.interactive.insert(id, hit),
                            None => layout.interactive.remove(&id),
                        };
                    }
                }
            }
        }
        _ => {
            for child in node.children() {
                walk(
                    taffy,
                    child,
                    order,
                    cursor,
                    parent_origin + local,
                    shift,
                    own_clip,
                    state,
                    layout,
                );
            }
        }
    }
    rect
}

/// Whether a node's CSS clips its children.
fn clips_overflow(css: &ComputedStyle) -> bool {
    match css.overflow {
        Some((x, y)) => x != OverflowKind::Visible || y != OverflowKind::Visible,
        None => false,
    }
}

/// Resolve every node's computed style, preorder, with inheritance and the
/// effective opacity and hidden flags filled in.
fn resolve_styles(tree: &PanelTree, sheet: &mut Stylesheet, state: &UiState) -> Vec<ComputedStyle> {
    let mut out = Vec::with_capacity(tree.meta.len());
    let mut chain: Vec<NodeFacts<'_>> = Vec::new();
    let mut parents: Vec<ComputedStyle> = Vec::new();
    let mut cursor = 0usize;
    resolve_walk(
        &tree.root,
        &tree.meta,
        &mut cursor,
        &mut chain,
        &mut parents,
        sheet,
        state,
        &mut out,
    );
    out
}

/// One step of [`resolve_styles`].
#[allow(clippy::too_many_arguments)]
fn resolve_walk<'t>(
    node: &PanelNode,
    meta: &'t [crate::panel::NodeMeta],
    cursor: &mut usize,
    chain: &mut Vec<NodeFacts<'t>>,
    parents: &mut Vec<ComputedStyle>,
    sheet: &mut Stylesheet,
    state: &UiState,
    out: &mut Vec<ComputedStyle>,
) {
    let facts_meta = &meta[*cursor];
    *cursor += 1;
    let id = node.id();
    let enabled = node_enabled(node);
    let hovered = enabled
        && id
            .zip(state.input.pointer())
            .map(|(id, pointer)| {
                state
                    .hit_rects
                    .get(&id)
                    .map(|rect| rect.contains(pointer))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
    let facts = NodeFacts {
        type_name: facts_meta.type_name,
        classes: &facts_meta.classes,
        id_name: facts_meta.name.as_deref(),
        hovered,
        active: id.is_some() && state.interaction.active == id,
        focused: id.is_some() && state.interaction.focus == id,
        disabled: !enabled,
    };
    chain.push(facts);
    let key = crate::css::key_for(chain);
    let parent = parents.last().cloned().unwrap_or_default();
    let mut style = sheet.resolve(chain, key, &parent);
    style.opacity_effective = parent.opacity_effective * style.opacity.unwrap_or(1.0);
    style.hidden = parent.hidden || style.display == Some(DisplayKind::None);
    out.push(style.clone());
    parents.push(style);
    for child in node.children() {
        resolve_walk(child, meta, cursor, chain, parents, sheet, state, out);
    }
    parents.pop();
    chain.pop();
}

/// Build the `taffy` node for `node`, recording node ids in preorder in
/// `order`; `styles` and `cursor` follow the same preorder for the
/// computed styles.
#[allow(clippy::too_many_arguments)]
fn build_node(
    taffy: &mut TaffyTree<TextMeasure>,
    node: &PanelNode,
    theme: &Theme,
    styles: &[ComputedStyle],
    cursor: &mut usize,
    extra_indent: f32,
    is_root: bool,
    order: &mut Vec<NodeId>,
) -> NodeId {
    let css = &styles[*cursor];
    *cursor += 1;
    let style = node_style(node, theme, extra_indent, is_root, css);

    match node {
        PanelNode::Label { text } | PanelNode::Heading { text } => {
            let context = TextMeasure {
                text: text.clone(),
                style: text_style(node, theme, css),
                wrap: css.white_space != Some(crate::css::WhiteSpace::Nowrap),
            };
            let id = taffy
                .new_leaf_with_context(style, context)
                .expect("leaf creation");
            order.push(id);
            id
        }
        _ => {
            let id = taffy.new_leaf(style).expect("leaf creation");
            order.push(id);
            let child_indent = match node {
                PanelNode::Collapsing { .. } => theme.indent,
                _ => 0.0,
            };
            for child in node.children() {
                let child_id = build_node(
                    taffy,
                    child,
                    theme,
                    styles,
                    cursor,
                    child_indent,
                    false,
                    order,
                );
                taffy.add_child(id, child_id).expect("child insertion");
            }
            id
        }
    }
}

/// The text style a text node measures with: the theme's metrics, overridden
/// by the node's computed style. Painting reuses it, so text paints exactly
/// as it measured.
pub(crate) fn text_style(node: &PanelNode, theme: &Theme, css: &ComputedStyle) -> TextStyle {
    let base_size = match node {
        PanelNode::Heading { .. } => theme.heading_size,
        _ => theme.font_size,
    };
    let base_line = match node {
        PanelNode::Heading { .. } => theme.heading_size * 1.3,
        _ => theme.line_height,
    };
    let font_size = css.font_size.unwrap_or(base_size);
    let line_height = match css.line_height {
        Some(crate::css::LineHeight::Px(px)) => px,
        Some(crate::css::LineHeight::Factor(factor)) => factor * font_size,
        None => base_line,
    };
    TextStyle {
        font_size,
        line_height: line_height.max(font_size),
        family: css.font_family.clone(),
    }
}

/// The `taffy` style of one node: fixed heights for widgets, `auto` for
/// text, flex for containers - the built-in look, with the node's computed
/// style over it.
fn node_style(
    node: &PanelNode,
    theme: &Theme,
    extra_indent: f32,
    is_root: bool,
    css: &ComputedStyle,
) -> Style {
    let mut style = Style {
        // Widgets keep their sizes: content overflows a scroll area rather
        // than shrinking to fit it.
        flex_shrink: 0.0,
        gap: TaffySize {
            width: LengthPercentage::length(theme.spacing),
            height: LengthPercentage::length(theme.spacing),
        },
        ..Style::default()
    };
    if extra_indent != 0.0 {
        style.margin.left = LengthPercentageAuto::length(extra_indent);
    }
    if is_root {
        style.size.width = Dimension::percent(1.0);
    }

    let item = Dimension::length(theme.item_height);
    match node {
        PanelNode::Row { .. } => {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Row;
            style.align_items = Some(AlignItems::CENTER);
        }
        PanelNode::Column { .. } => {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Column;
            style.align_items = Some(AlignItems::STRETCH);
        }
        PanelNode::Label { .. } | PanelNode::Heading { .. } => {}
        PanelNode::Separator => {
            style.size.height = Dimension::length(theme.border_width);
        }
        PanelNode::Image { size, .. } => {
            style.size.width = Dimension::length(size.x);
            style.size.height = Dimension::length(size.y);
        }
        PanelNode::Button { .. } => {
            style.size.height = item;
            style.min_size.width = LengthPercentageAuto::length(40.0);
        }
        PanelNode::Checkbox { .. } => {
            style.size.height = item;
            style.min_size.width = LengthPercentageAuto::length(90.0);
        }
        PanelNode::Slider { .. } => {
            style.size.height = item;
            style.min_size.width = LengthPercentageAuto::length(120.0);
        }
        PanelNode::DragValue { .. } => {
            style.size.height = item;
            style.min_size.width = LengthPercentageAuto::length(64.0);
        }
        PanelNode::TextField { .. } => {
            style.size.height = item;
            style.min_size.width = LengthPercentageAuto::length(80.0);
        }
        PanelNode::ComboBox { .. } => {
            style.size.height = item;
            style.min_size.width = LengthPercentageAuto::length(80.0);
        }
        PanelNode::ColorField { .. } => {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Row;
            style.align_items = Some(AlignItems::CENTER);
            style.size.height = item;
            style.padding.left = LengthPercentage::length(COLOR_SWATCH_WIDTH);
        }
        PanelNode::VectorField { .. } => {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Row;
            style.align_items = Some(AlignItems::CENTER);
            style.size.height = item;
        }
        PanelNode::Collapsing { .. } => {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Column;
            style.align_items = Some(AlignItems::STRETCH);
            // The header paints in the top padding.
            style.padding.top = LengthPercentage::length(theme.item_height);
        }
        PanelNode::ScrollArea { height, .. } => {
            style.display = Display::Flex;
            style.flex_direction = FlexDirection::Column;
            style.align_items = Some(AlignItems::STRETCH);
            style.overflow = Point {
                x: Overflow::Visible,
                y: Overflow::Clip,
            };
            if let Some(height) = height {
                style.size.height = Dimension::length(*height);
            }
        }
        PanelNode::Table { rows, .. } => {
            style.size.height = Dimension::length(theme.item_height * (rows.len() + 1) as f32);
        }
        PanelNode::TreeRow { depth, .. } => {
            style.size.height = item;
            style.margin.left =
                LengthPercentageAuto::length(*depth as f32 * theme.indent + extra_indent);
        }
    }

    // The computed style's overrides; anything it does not set keeps the
    // built-in value above.
    if let Some(display) = css.display {
        style.display = match display {
            DisplayKind::Flex => Display::Flex,
            DisplayKind::Block => Display::Block,
            DisplayKind::None => Display::None,
        };
    }
    if let Some(direction) = css.flex_direction {
        style.flex_direction = match direction {
            FlexDirectionKind::Row => FlexDirection::Row,
            FlexDirectionKind::RowReverse => FlexDirection::RowReverse,
            FlexDirectionKind::Column => FlexDirection::Column,
            FlexDirectionKind::ColumnReverse => FlexDirection::ColumnReverse,
        };
    }
    if let Some(grow) = css.flex_grow {
        style.flex_grow = grow;
    }
    if let Some(shrink) = css.flex_shrink {
        style.flex_shrink = shrink;
    }
    if let Some(basis) = css.flex_basis {
        style.flex_basis = dimension_of(basis);
    }
    if let Some(align) = css.align_items {
        style.align_items = Some(match align {
            AlignKind::Stretch => AlignItems::STRETCH,
            AlignKind::Center => AlignItems::CENTER,
            AlignKind::FlexStart => AlignItems::FLEX_START,
            AlignKind::FlexEnd => AlignItems::FLEX_END,
        });
    }
    if let Some(justify) = css.justify_content {
        style.justify_content = Some(match justify {
            JustifyKind::Start => JustifyContent::START,
            JustifyKind::End => JustifyContent::END,
            JustifyKind::Center => JustifyContent::CENTER,
            JustifyKind::SpaceBetween => JustifyContent::SPACE_BETWEEN,
        });
    }
    if let Some((row, column)) = css.gap {
        style.gap = TaffySize {
            width: LengthPercentage::length(column),
            height: LengthPercentage::length(row),
        };
    }
    if let Some(width) = css.width {
        style.size.width = dimension_of(width);
    }
    if let Some(height) = css.height {
        style.size.height = dimension_of(height);
    }
    if let Some(min_width) = css.min_width {
        style.min_size.width = min_max_of(min_width);
    }
    if let Some(min_height) = css.min_height {
        style.min_size.height = min_max_of(min_height);
    }
    if let Some(max_width) = css.max_width {
        style.max_size.width = min_max_of(max_width);
    }
    if let Some(max_height) = css.max_height {
        style.max_size.height = min_max_of(max_height);
    }
    if let Some(padding) = css.padding {
        style.padding = TaffyRect {
            left: LengthPercentage::length(padding.left),
            right: LengthPercentage::length(padding.right),
            top: LengthPercentage::length(padding.top),
            bottom: LengthPercentage::length(padding.bottom),
        };
    }
    if let Some(margin) = css.margin {
        style.margin = TaffyRect {
            left: auto_length_of(margin.left),
            right: auto_length_of(margin.right),
            top: auto_length_of(margin.top),
            bottom: auto_length_of(margin.bottom),
        };
    }
    let border = css.border;
    for (side, target) in [
        (border.top, &mut style.border.top),
        (border.right, &mut style.border.right),
        (border.bottom, &mut style.border.bottom),
        (border.left, &mut style.border.left),
    ] {
        if let Some(width) = side.width {
            *target = LengthPercentage::length(width);
        }
    }
    if let Some(sizing) = css.box_sizing {
        style.box_sizing = match sizing {
            crate::css::BoxSizingKind::BorderBox => BoxSizing::BorderBox,
            crate::css::BoxSizingKind::ContentBox => BoxSizing::ContentBox,
        };
    }
    if let Some(position) = css.position {
        style.position = match position {
            PositionKind::Relative => Position::Relative,
            PositionKind::Absolute | PositionKind::Fixed => Position::Absolute,
        };
    }
    let inset = css.inset;
    style.inset = TaffyRect {
        left: auto_length_of(inset.left.unwrap_or(Length::Auto)),
        right: auto_length_of(inset.right.unwrap_or(Length::Auto)),
        top: auto_length_of(inset.top.unwrap_or(Length::Auto)),
        bottom: auto_length_of(inset.bottom.unwrap_or(Length::Auto)),
    };
    if let Some((x, y)) = css.overflow {
        let convert = |kind: OverflowKind| match kind {
            OverflowKind::Visible => Overflow::Visible,
            OverflowKind::Hidden => Overflow::Hidden,
            OverflowKind::Auto | OverflowKind::Scroll => Overflow::Scroll,
            OverflowKind::Clip => Overflow::Clip,
        };
        style.overflow = Point {
            x: convert(x),
            y: convert(y),
        };
    }
    style
}

/// A CSS size as a `taffy` dimension.
fn dimension_of(size: crate::css::Size) -> Dimension {
    match size {
        crate::css::Size::Length(Length::Px(value)) => Dimension::length(value),
        crate::css::Size::Length(Length::Percent(value)) => Dimension::percent(value / 100.0),
        crate::css::Size::Length(Length::Auto) => Dimension::auto(),
        crate::css::Size::MaxContent => Dimension::max_content(),
        crate::css::Size::MinContent => Dimension::min_content(),
    }
}

/// A CSS length as a `taffy` length that may be `auto`.
fn auto_length_of(length: Length) -> LengthPercentageAuto {
    match length {
        Length::Px(value) => LengthPercentageAuto::length(value),
        Length::Percent(value) => LengthPercentageAuto::percent(value / 100.0),
        Length::Auto => LengthPercentageAuto::auto(),
    }
}

/// A CSS min/max size as a `taffy` length; content keywords become `auto`,
/// which keeps the built-in behaviour.
fn min_max_of(size: crate::css::Size) -> LengthPercentageAuto {
    match size {
        crate::css::Size::Length(length) => auto_length_of(length),
        crate::css::Size::MaxContent | crate::css::Size::MinContent => LengthPercentageAuto::auto(),
    }
}

/// What the measure function needs to size a text leaf.
struct TextMeasure {
    /// The text to measure.
    text: String,
    /// The style to measure it with.
    style: TextStyle,
    /// Whether the text wraps at the available width.
    wrap: bool,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::Stylesheet;
    use crate::draw::TextureId;
    use crate::panel::TreeNode;
    use crate::widget::WidgetId;
    use pill_engine_core::{ButtonState, InputEvent, MouseButton};

    fn text_system() -> TextSystem {
        TextSystem::new()
    }

    fn ctrl(x: f32, y: f32) -> Vector2f {
        Vector2f::new(x, y)
    }

    fn pointer(x: f32, y: f32) -> InputEvent {
        InputEvent::CursorMoved {
            position: ctrl(x, y),
        }
    }

    fn click(state: ButtonState) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            state,
        }
    }

    /// Run one frame with real layout and store the hit rectangles, the way
    /// the shell does.
    fn frame(
        state: &mut UiState,
        text: &mut TextSystem,
        events: &[InputEvent],
        panel: impl FnOnce(&mut PanelBuilder<'_>),
    ) -> (PanelOutput, PanelLayout) {
        for event in events {
            state.input.apply(event);
        }
        state.input.begin_frame();
        let theme = Theme::default();
        let mut sheet = Stylesheet::empty();
        let (output, layout) =
            run_frame(state, text, &theme, &mut sheet, ctrl(400.0, 300.0), panel);
        state.end_frame();
        (output, layout)
    }

    #[test]
    fn a_clicked_button_activates_next_frame() {
        let mut state = UiState::new();
        let mut text = text_system();
        let id = WidgetId::from_name("Bake");
        let mut activated = true;

        let (_, layout) = frame(&mut state, &mut text, &[], |ui| {
            activated = ui.button("Bake");
        });
        assert!(!activated);
        let rect = state.hit_rects[&id];
        assert!(rect.width() > 0.0 && rect.height() > 0.0);
        assert!(layout.interactive.contains_key(&id));

        let (_, layout) = frame(
            &mut state,
            &mut text,
            &[
                pointer(rect.min.x + 2.0, rect.min.y + 2.0),
                click(ButtonState::Pressed),
            ],
            |ui| {
                activated = ui.button("Bake");
            },
        );
        assert!(!activated, "the press alone does not activate");
        assert!(layout.interactive.contains_key(&id));

        let (output, _) = frame(
            &mut state,
            &mut text,
            &[click(ButtonState::Released)],
            |ui| {
                activated = ui.button("Bake");
            },
        );
        assert!(activated, "the click activates on the next frame");
        assert_eq!(output.inputs, vec![crate::panel::PanelInput::Activated(id)]);
    }

    #[test]
    fn a_release_still_clicks_when_another_panel_builds_first() {
        // The editor shell builds several panels in one frame. The release
        // that completes a click must survive the first panel's build: its
        // finish may not end the drag the later panel still needs.
        let mut state = UiState::new();
        let mut text = text_system();
        let id = WidgetId::from_name("Bake");
        let mut activated = true;

        fn two_panels(
            state: &mut UiState,
            text: &mut TextSystem,
            events: &[InputEvent],
            panel: impl FnOnce(&mut PanelBuilder<'_>),
        ) -> PanelOutput {
            for event in events {
                state.input.apply(event);
            }
            state.input.begin_frame();
            // The shell's pattern: every panel builds against the previous
            // frame's map, so the first (unrelated) panel's empty map must
            // not hide the second one's button.
            let theme = Theme::default();
            let mut sheet = Stylesheet::empty();
            let previous = state.hit_rects().clone();
            state.set_hit_rects(previous.clone());
            let _ = run_frame(state, text, &theme, &mut sheet, ctrl(400.0, 300.0), |ui| {
                ui.label("an unrelated first panel");
            });
            state.set_hit_rects(previous);
            let (output, _layout) =
                run_frame(state, text, &theme, &mut sheet, ctrl(400.0, 300.0), panel);
            state.end_frame();
            output
        }

        two_panels(&mut state, &mut text, &[], |ui| {
            activated = ui.button("Bake");
        });
        assert!(!activated);
        let rect = state.hit_rects[&id];

        two_panels(
            &mut state,
            &mut text,
            &[
                pointer(rect.min.x + 2.0, rect.min.y + 2.0),
                click(ButtonState::Pressed),
            ],
            |ui| {
                activated = ui.button("Bake");
            },
        );
        assert!(!activated, "the press alone does not activate");

        let output = two_panels(
            &mut state,
            &mut text,
            &[click(ButtonState::Released)],
            |ui| {
                activated = ui.button("Bake");
            },
        );
        assert!(activated, "the second panel turns the release into a click");
        assert_eq!(output.inputs, vec![crate::panel::PanelInput::Activated(id)]);
    }

    #[test]
    fn every_widget_kind_gets_a_nonzero_rectangle() {
        let mut state = UiState::new();
        let mut text = text_system();

        fn build_all(ui: &mut PanelBuilder<'_>) {
            ui.label("label");
            ui.heading("heading");
            ui.separator();
            ui.image("icon", TextureId::External(1), Vector2f::new(24.0, 24.0));
            ui.button("button");
            ui.checkbox("checkbox", true);
            ui.slider("slider", 0.5, 0.0..=1.0);
            ui.drag_value("drag", 1.0, 0.1);
            let mut field_text = String::from("text");
            ui.text_field("field", &mut field_text);
            ui.combo_box("combo", 0, &["one", "two"]);
            ui.color_field("color", crate::draw::Color::WHITE);
            ui.vector_field("vector", pill_core::math::Vector3f::ONE);
            ui.collapsing("collapsing", true, |ui| {
                ui.label("inside");
            });
            ui.scroll_area("scroll", 60.0, |ui| {
                ui.label("one");
                ui.label("two");
            });
            ui.table("table", &["a"], &[&["1"]]);
            ui.tree("tree", &[TreeNode::new("row")]);
        }

        let (output, layout) = frame(&mut state, &mut text, &[], build_all);
        assert!(layout.size.x > 0.0 && layout.size.y > 0.0);

        // Every node got a rectangle in preorder.
        let node_count = count_nodes(&output.tree.root);
        assert_eq!(layout.rects.len(), node_count);
        assert_eq!(layout.clips.len(), node_count);
        for rect in &layout.rects {
            assert!(rect.max.x >= rect.min.x && rect.max.y >= rect.min.y);
        }

        // Every interactive widget can be hit somewhere. The tree view's
        // surface is its rows, so the row id is what carries a rectangle.
        for id in [
            WidgetId::from_name("button"),
            WidgetId::from_name("checkbox"),
            WidgetId::from_name("slider"),
            WidgetId::from_name("drag"),
            WidgetId::from_name("field"),
            WidgetId::from_name("combo"),
            WidgetId::from_name("color"),
            WidgetId::from_name("vector"),
            WidgetId::from_name("collapsing"),
            WidgetId::from_name("scroll"),
            WidgetId::from_name("table"),
            WidgetId::named(WidgetId::from_name("tree"), "0 row"),
        ] {
            assert!(
                layout.interactive.contains_key(&id),
                "{id:?} has a hit rectangle"
            );
        }
    }

    fn count_nodes(node: &PanelNode) -> usize {
        1 + node.children().iter().map(count_nodes).sum::<usize>()
    }

    #[test]
    fn a_scrolled_out_widget_loses_its_hit_rectangle() {
        let mut state = UiState::new();
        let mut text = text_system();
        let scroller = WidgetId::from_name("scroller");
        // Widgets inside a scroll area derive their ids from it, so their
        // state stays put when the panel around them moves.
        let first = WidgetId::named(scroller, "a");
        let middle_id = WidgetId::named(scroller, "b");
        let last = WidgetId::named(scroller, "c");
        let build = |ui: &mut PanelBuilder<'_>| {
            ui.scroll_area("scroller", 30.0, |ui| {
                ui.drag_value("a", 0.0, 0.1);
                ui.drag_value("b", 0.0, 0.1);
                ui.drag_value("c", 0.0, 0.1);
            });
        };

        // Unscrolled, the middle field is cut off by the viewport: its hit
        // rectangle is the visible part, not the whole widget.
        let (_, layout) = frame(&mut state, &mut text, &[], build);
        let middle = layout.interactive[&middle_id];
        assert!(middle.height() < Theme::default().item_height);

        // Scrolled to the end, the first field is entirely above the
        // viewport and stops being interactive.
        state.memory.entry(scroller).or_default().scroll = 100_000.0;
        let (_, layout) = frame(&mut state, &mut text, &[], build);
        assert!(!layout.interactive.contains_key(&first));
        assert!(layout.interactive.contains_key(&last));
    }

    #[test]
    fn text_leaves_measure_to_their_content_height() {
        let mut state = UiState::new();
        let mut text = text_system();
        let (output, layout) = frame(&mut state, &mut text, &[], |ui| {
            ui.label("a single line");
        });
        let label_height = match output.tree.root {
            PanelNode::Column { ref children } => {
                assert_eq!(children.len(), 1);
                layout.rects[1].height()
            }
            _ => panic!("panels start as columns"),
        };
        assert!(
            (label_height - Theme::default().line_height).abs() < 4.0,
            "a label is one line tall, got {label_height}"
        );
    }

    #[test]
    fn scroll_offsets_are_clamped_to_the_content() {
        let mut state = UiState::new();
        let mut text = text_system();
        let id = WidgetId::from_name("scroller");
        let build = |ui: &mut PanelBuilder<'_>| {
            ui.scroll_area("scroller", 40.0, |ui| {
                for index in 0..12 {
                    ui.label(&format!("line {index}"));
                }
            });
        };
        frame(&mut state, &mut text, &[], build);
        // Ask for far more scrolling than exists; layout must clamp it when
        // it uses the offset, without touching the stored value.
        state.memory.entry(id).or_default().scroll = 100_000.0;
        let (_, layout) = frame(&mut state, &mut text, &[], build);
        assert!(layout.size.y > 0.0);
        assert_eq!(state.memory[&id].scroll, 100_000.0);
    }
}

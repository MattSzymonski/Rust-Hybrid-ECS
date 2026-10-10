//! What a panel wants drawn, as plain data.
//!
//! # Responsibilities
//!
//! - Defines the draw vocabulary a backend consumes: [`DrawList`] with
//!   vertices, indices, clip rectangles and draw commands, plus the value
//!   types those carry ([`UiVertex`], [`Color`], [`Rect`], [`TextureId`]).
//! - Batches consecutive quads that share a texture and no clip change into
//!   one command, so a run of glyph quads is one draw.
//!
//! # Design
//!
//! The list is append-only within a frame and cleared between frames.
//! Positions are logical pixels with the origin at the UI's top-left corner
//! and y growing down; the backend applies the monitor's scale factor when it
//! draws. Clips nest: [`DrawList::push_clip`] intersects the given rectangle
//! with the current clip, and every draw between a push and its matching
//! [`DrawList::pop_clip`] belongs clipped to it.

// Standard library
use std::ops::Range;

// External crates
use pill_core::math::Vector2f;

// =============================================================================
// Color
// =============================================================================

/// A straight-alpha colour in sRGB, one `f32` per channel in `0.0..=1.0`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Color {
    /// Red channel.
    pub r: f32,
    /// Green channel.
    pub g: f32,
    /// Blue channel.
    pub b: f32,
    /// Alpha channel; `0.0` is fully transparent.
    pub a: f32,
}

impl Color {
    /// Fully transparent black.
    pub const TRANSPARENT: Color = Color::rgba(0.0, 0.0, 0.0, 0.0);
    /// Opaque white.
    pub const WHITE: Color = Color::rgba(1.0, 1.0, 1.0, 1.0);
    /// Opaque black.
    pub const BLACK: Color = Color::rgba(0.0, 0.0, 0.0, 1.0);

    /// A colour from straight alpha components in `0.0..=1.0`.
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Color {
        Color { r, g, b, a }
    }

    /// A colour from 8-bit sRGB components.
    pub fn from_rgba8(rgba: [u8; 4]) -> Color {
        Color {
            r: rgba[0] as f32 / 255.0,
            g: rgba[1] as f32 / 255.0,
            b: rgba[2] as f32 / 255.0,
            a: rgba[3] as f32 / 255.0,
        }
    }
}

impl Default for Color {
    fn default() -> Color {
        Color::WHITE
    }
}

// =============================================================================
// Rect
// =============================================================================

/// An axis-aligned rectangle in logical pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Rect {
    /// The corner with the smaller x and y.
    pub min: Vector2f,
    /// The corner with the larger x and y.
    pub max: Vector2f,
}

impl Rect {
    /// A rectangle from its two corners, whatever their order.
    pub fn new(min: Vector2f, max: Vector2f) -> Rect {
        Rect {
            min: Vector2f::new(min.x.min(max.x), min.y.min(max.y)),
            max: Vector2f::new(min.x.max(max.x), min.y.max(max.y)),
        }
    }

    /// A rectangle from its top-left corner and its size.
    pub fn from_min_size(min: Vector2f, size: Vector2f) -> Rect {
        Rect {
            min,
            max: min + size,
        }
    }

    /// The width, along x.
    pub fn width(&self) -> f32 {
        self.max.x - self.min.x
    }

    /// The height, along y.
    pub fn height(&self) -> f32 {
        self.max.y - self.min.y
    }

    /// Whether `point` lies inside, edges included.
    pub fn contains(&self, point: Vector2f) -> bool {
        point.x >= self.min.x
            && point.x <= self.max.x
            && point.y >= self.min.y
            && point.y <= self.max.y
    }

    /// The overlap of two rectangles, or `None` when they do not overlap.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let min = Vector2f::new(self.min.x.max(other.min.x), self.min.y.max(other.min.y));
        let max = Vector2f::new(self.max.x.min(other.max.x), self.max.y.min(other.max.y));
        (min.x <= max.x && min.y <= max.y).then_some(Rect { min, max })
    }
}

// =============================================================================
// Texture and vertex types
// =============================================================================

/// Which texture a draw samples.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TextureId {
    /// The glyph and icon atlas ([`crate::atlas::Atlas`]).
    Atlas,
    /// No texture: the vertex colour fills the shape.
    Solid,
    /// A texture the caller registered with the backend under this number.
    External(u64),
}

/// One vertex: a position in logical pixels, a texture coordinate in
/// `0.0..=1.0`, a colour multiplied into the sampled texel, and the shape
/// data the rounded-corner shader needs.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct UiVertex {
    /// Position in logical pixels, y down.
    pub position: Vector2f,
    /// Texture coordinate; the texture's top-left corner is `(0.0, 0.0)`.
    pub uv: Vector2f,
    /// The vertex colour.
    pub color: Color,
    /// Position within the quad, `(0, 0)` at its top-left corner: the space
    /// a backend evaluates rounded corners and soft edges in.
    pub local: Vector2f,
    /// The quad's shape: `[half_width, half_height, corner_radius,
    /// edge_softness]`. Zero radius and softness is a plain rectangle.
    pub shape: [f32; 4],
}

/// One ordered draw operation.
#[derive(Clone, Debug, PartialEq)]
pub enum DrawCommand {
    /// Clip everything after this to `rect` intersected with the current
    /// clip.
    PushClip(Rect),
    /// Return to the clip that was active before the matching push.
    PopClip,
    /// Draw the given index range from `texture`.
    Draw {
        /// The texture to sample.
        texture: TextureId,
        /// The first index into [`DrawList::indices`].
        first_index: u32,
        /// How many indices to draw.
        index_count: u32,
    },
}

// =============================================================================
// DrawList
// =============================================================================

/// A frame's draw work: vertices, indices, and the commands that consume
/// them, in order.
///
/// Cleared and rebuilt every frame. The list never owns GPU resources; it is
/// plain data any backend can read.
#[derive(Clone, Debug, Default)]
pub struct DrawList {
    /// The frame's vertices, referenced by index.
    pub vertices: Vec<UiVertex>,
    /// The frame's indices, in draw order.
    pub indices: Vec<u32>,
    /// Clip changes and draws, in submission order.
    pub commands: Vec<DrawCommand>,
    /// How many clips are currently open; keeps pushes and pops balanced.
    clip_depth: u32,
}

impl DrawList {
    /// An empty list, ready for a frame.
    pub fn new() -> DrawList {
        DrawList::default()
    }

    /// Whether nothing would be drawn: no vertices and no clip changes.
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty() && self.commands.is_empty()
    }

    /// Forget the previous frame's work, keeping the allocations.
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
        self.commands.clear();
        self.clip_depth = 0;
    }

    /// Clip every following draw to `rect` until the matching
    /// [`Self::pop_clip`].
    ///
    /// The backend intersects this with whatever clip is already active.
    pub fn push_clip(&mut self, rect: Rect) {
        self.clip_depth += 1;
        self.commands.push(DrawCommand::PushClip(rect));
    }

    /// Close the most recent [`Self::push_clip`].
    pub fn pop_clip(&mut self) {
        debug_assert!(self.clip_depth > 0, "pop_clip without a matching push_clip");
        self.clip_depth = self.clip_depth.saturating_sub(1);
        self.commands.push(DrawCommand::PopClip);
    }

    /// A solid-coloured rectangle.
    pub fn push_fill(&mut self, rect: Rect, color: Color) {
        self.push_rounded_fill(rect, color, 0.0);
    }

    /// A solid-coloured rectangle with rounded corners.
    ///
    /// The rounding is shape data, not extra geometry: a backend with the
    /// rounded shader draws it directly. The radius is clamped to the
    /// rectangle's half extent, beyond which it is a capsule.
    pub fn push_rounded_fill(&mut self, rect: Rect, color: Color, corner_radius: f32) {
        let shape = self.shape(rect, corner_radius, 0.0);
        self.push_shaped_quad(
            TextureId::Solid,
            rect,
            Vector2f::ZERO,
            Vector2f::ZERO,
            color,
            shape,
        );
    }

    /// A solid rectangle whose edge fades over `softness` pixels: full
    /// colour `softness` inside the boundary, nothing at and beyond it - the
    /// primitive behind `box-shadow`. `rect` is the visible outer extent.
    pub fn push_soft_fill(&mut self, rect: Rect, color: Color, corner_radius: f32, softness: f32) {
        let shape = self.shape(rect, corner_radius, softness);
        self.push_shaped_quad(
            TextureId::Solid,
            rect,
            Vector2f::ZERO,
            Vector2f::ZERO,
            color,
            shape,
        );
    }

    /// The shape array for a quad: half extents, clamped radius, softness.
    fn shape(&self, rect: Rect, corner_radius: f32, softness: f32) -> [f32; 4] {
        let half_width = rect.width() * 0.5;
        let half_height = rect.height() * 0.5;
        let radius = corner_radius.clamp(0.0, half_width.min(half_height));
        [half_width, half_height, radius, softness.max(0.0)]
    }

    /// A rectangle textured from `rect`'s corner-to-corner UV range.
    ///
    /// Vertices are appended in top-left, top-right, bottom-right,
    /// bottom-left order, so `uv_min` and `uv_max` map to the corners named
    /// by their coordinates whatever the texture's orientation.
    pub fn push_quad(
        &mut self,
        texture: TextureId,
        rect: Rect,
        uv_min: Vector2f,
        uv_max: Vector2f,
        color: Color,
    ) {
        let shape = self.shape(rect, 0.0, 0.0);
        self.push_shaped_quad(texture, rect, uv_min, uv_max, color, shape);
    }

    /// The one vertex writer: appends the quad's four vertices (top-left,
    /// top-right, bottom-right, bottom-left), its indices and the draw
    /// command.
    fn push_shaped_quad(
        &mut self,
        texture: TextureId,
        rect: Rect,
        uv_min: Vector2f,
        uv_max: Vector2f,
        color: Color,
        shape: [f32; 4],
    ) {
        let width = rect.width();
        let height = rect.height();
        let base = self.vertices.len() as u32;
        self.vertices.push(UiVertex {
            position: Vector2f::new(rect.min.x, rect.min.y),
            uv: Vector2f::new(uv_min.x, uv_min.y),
            color,
            local: Vector2f::new(0.0, 0.0),
            shape,
        });
        self.vertices.push(UiVertex {
            position: Vector2f::new(rect.max.x, rect.min.y),
            uv: Vector2f::new(uv_max.x, uv_min.y),
            color,
            local: Vector2f::new(width, 0.0),
            shape,
        });
        self.vertices.push(UiVertex {
            position: Vector2f::new(rect.max.x, rect.max.y),
            uv: Vector2f::new(uv_max.x, uv_max.y),
            color,
            local: Vector2f::new(width, height),
            shape,
        });
        self.vertices.push(UiVertex {
            position: Vector2f::new(rect.min.x, rect.max.y),
            uv: Vector2f::new(uv_min.x, uv_max.y),
            color,
            local: Vector2f::new(0.0, height),
            shape,
        });

        let first_index = self.indices.len() as u32;
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        self.append_draw(texture, first_index, 6);
    }

    /// Extend the previous draw when it is the same texture and the index
    /// range is contiguous, else start a new one.
    fn append_draw(&mut self, texture: TextureId, first_index: u32, index_count: u32) {
        if let Some(DrawCommand::Draw {
            texture: previous,
            first_index: previous_first,
            index_count: previous_count,
        }) = self.commands.last_mut()
        {
            if *previous == texture && previous_first.saturating_add(*previous_count) == first_index
            {
                *previous_count += index_count;
                return;
            }
        }
        self.commands.push(DrawCommand::Draw {
            texture,
            first_index,
            index_count,
        });
    }

    /// The index range of the `command`-th [`DrawCommand::Draw`], for
    /// introspection and tests.
    pub fn draw_range(&self, command: usize) -> Option<Range<u32>> {
        let mut seen = 0usize;
        for entry in &self.commands {
            if let DrawCommand::Draw {
                first_index,
                index_count,
                ..
            } = entry
            {
                if seen == command {
                    return Some(*first_index..first_index + index_count);
                }
                seen += 1;
            }
        }
        None
    }

    /// Shift every vertex and clip rectangle by `offset`.
    ///
    /// A list is painted at the origin; a caller that puts it into a
    /// sub-region - an editor panel at its dock rectangle - translates it
    /// after painting instead of painting in a translated space. Draw
    /// commands carry no positions, so only the vertices and the push-clip
    /// rectangles move.
    pub fn translate(&mut self, offset: Vector2f) {
        for vertex in &mut self.vertices {
            vertex.position += offset;
        }
        for command in &mut self.commands {
            if let DrawCommand::PushClip(rect) = command {
                rect.min += offset;
                rect.max += offset;
            }
        }
    }

    /// Append `other`'s work after this list's own, renumbering vertex
    /// indices and draw ranges.
    ///
    /// `other` must have its clips balanced (every push matched by a pop),
    /// which every painter produced by [`crate::paint::paint`] is; an
    /// unbalanced list would leave this list's clip state wrong.
    pub fn append(&mut self, other: DrawList) {
        debug_assert!(
            other.clip_depth == 0,
            "append needs a list with balanced clips"
        );
        let vertex_base = self.vertices.len() as u32;
        let index_base = self.indices.len() as u32;
        self.vertices.extend(other.vertices);
        self.indices
            .extend(other.indices.into_iter().map(|index| index + vertex_base));
        for command in other.commands {
            self.commands.push(match command {
                DrawCommand::Draw {
                    texture,
                    first_index,
                    index_count,
                } => DrawCommand::Draw {
                    texture,
                    first_index: first_index + index_base,
                    index_count,
                },
                DrawCommand::PushClip(rect) => DrawCommand::PushClip(rect),
                DrawCommand::PopClip => DrawCommand::PopClip,
            });
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn quad_rect(min_x: f32, min_y: f32, size: f32) -> Rect {
        Rect::from_min_size(Vector2f::new(min_x, min_y), Vector2f::new(size, size))
    }

    #[test]
    fn a_quad_adds_four_vertices_and_six_indices() {
        let mut list = DrawList::new();
        list.push_fill(quad_rect(0.0, 0.0, 10.0), Color::WHITE);
        assert_eq!(list.vertices.len(), 4);
        assert_eq!(list.indices, [0, 1, 2, 0, 2, 3]);
        assert_eq!(
            list.commands,
            [DrawCommand::Draw {
                texture: TextureId::Solid,
                first_index: 0,
                index_count: 6,
            }]
        );
        assert!(!list.is_empty());
    }

    #[test]
    fn consecutive_quads_of_one_texture_share_a_command() {
        let mut list = DrawList::new();
        list.push_quad(
            TextureId::Atlas,
            quad_rect(0.0, 0.0, 8.0),
            Vector2f::ZERO,
            Vector2f::new(0.1, 0.1),
            Color::WHITE,
        );
        list.push_quad(
            TextureId::Atlas,
            quad_rect(8.0, 0.0, 8.0),
            Vector2f::ZERO,
            Vector2f::new(0.1, 0.1),
            Color::WHITE,
        );
        assert_eq!(list.vertices.len(), 8);
        assert_eq!(list.commands.len(), 1);
        assert_eq!(list.draw_range(0), Some(0..12));
    }

    #[test]
    fn a_texture_change_or_a_clip_change_starts_a_new_command() {
        let mut list = DrawList::new();
        list.push_fill(quad_rect(0.0, 0.0, 8.0), Color::WHITE);
        list.push_quad(
            TextureId::External(7),
            quad_rect(8.0, 0.0, 8.0),
            Vector2f::ZERO,
            Vector2f::new(1.0, 1.0),
            Color::WHITE,
        );
        list.push_clip(quad_rect(0.0, 0.0, 100.0));
        list.push_quad(
            TextureId::External(7),
            quad_rect(16.0, 0.0, 8.0),
            Vector2f::ZERO,
            Vector2f::new(1.0, 1.0),
            Color::WHITE,
        );
        assert_eq!(list.commands.len(), 4);
        assert_eq!(list.draw_range(0), Some(0..6));
        assert_eq!(list.draw_range(1), Some(6..12));
        assert_eq!(list.draw_range(2), Some(12..18));
        assert!(matches!(list.commands[2], DrawCommand::PushClip(_)));
    }

    #[test]
    fn clip_pushes_and_pops_stay_balanced_across_clear() {
        let mut list = DrawList::new();
        list.push_clip(quad_rect(0.0, 0.0, 10.0));
        list.pop_clip();
        list.clear();
        assert!(list.is_empty());
        assert_eq!(list.commands.len(), 0);
        // A fresh frame can push again without tripping the balance check.
        list.push_clip(quad_rect(0.0, 0.0, 10.0));
        list.pop_clip();
    }

    #[test]
    fn rectangles_normalize_and_contain() {
        let rect = Rect::new(Vector2f::new(10.0, 10.0), Vector2f::new(0.0, 0.0));
        assert_eq!(rect.min, Vector2f::ZERO);
        assert_eq!(rect.max, Vector2f::new(10.0, 10.0));
        assert!(rect.contains(Vector2f::new(5.0, 5.0)));
        assert!(rect.contains(Vector2f::new(0.0, 0.0)));
        assert!(!rect.contains(Vector2f::new(10.5, 5.0)));

        let other = Rect::from_min_size(Vector2f::new(5.0, 5.0), Vector2f::new(10.0, 10.0));
        assert_eq!(
            rect.intersection(&other),
            Some(Rect::from_min_size(
                Vector2f::new(5.0, 5.0),
                Vector2f::new(5.0, 5.0)
            ))
        );
        let apart = Rect::from_min_size(Vector2f::new(50.0, 50.0), Vector2f::new(1.0, 1.0));
        assert_eq!(rect.intersection(&apart), None);
    }

    #[test]
    fn translate_shifts_vertices_and_clip_rectangles() {
        let mut list = DrawList::new();
        let rect = quad_rect(0.0, 0.0, 10.0);
        list.push_clip(rect);
        list.push_fill(rect, Color::WHITE);
        list.pop_clip();
        list.translate(Vector2f::new(100.0, 50.0));
        for vertex in &list.vertices {
            assert!(vertex.position.x >= 100.0);
            assert!(vertex.position.y >= 50.0);
        }
        let Some(DrawCommand::PushClip(clip)) = list.commands.first() else {
            panic!("the push-clip survives the shift");
        };
        assert_eq!(clip.min, Vector2f::new(100.0, 50.0));
        assert_eq!(clip.max, Vector2f::new(110.0, 60.0));
    }

    #[test]
    fn append_renumbers_indices_and_draw_ranges() {
        let mut first = DrawList::new();
        first.push_fill(quad_rect(0.0, 0.0, 10.0), Color::WHITE);
        let mut second = DrawList::new();
        second.push_fill(quad_rect(20.0, 0.0, 10.0), Color::BLACK);
        let second_vertices = second.vertices.len() as u32;
        first.append(second);
        // Both quads' indices now address the merged vertex array.
        assert_eq!(first.vertices.len(), 8);
        assert_eq!(first.indices.len(), 12);
        for index in &first.indices {
            assert!(*index < first.vertices.len() as u32);
        }
        // The second quad's draw range starts after the first quad's.
        assert_eq!(first.draw_range(1), Some(6..(6 + 6)));
        assert_eq!(second_vertices, 4);
    }
}

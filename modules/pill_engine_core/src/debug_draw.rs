//! Immediate debug drawing: lines, shapes and text labels any system can draw,
//! and the gizmo handle geometry an editor tool draws on top.
//!
//! # Responsibilities
//!
//! - Hold what systems asked to see this frame, or for a while
//!   ([`DebugDraw`], a world resource): lines, shapes tessellated into lines,
//!   world- and screen-anchored text labels, and gizmo handle lines and
//!   triangles tagged with a handle id.
//! - Age the items and hand the renderer one frame's worth as plain data
//!   ([`DebugDraw::collect_frame`] returning a [`DebugFrame`]).
//! - Switch it all off: while disabled every drawing call returns at once and
//!   nothing reaches the renderer.
//!
//! # Design
//!
//! The resource is renderer-neutral plain data in the engine core, so a game,
//! an extension or an editor crate can draw without depending on a renderer,
//! and every renderer implementing the contract draws the same frame. Shapes
//! are lines by the time they leave here: a renderer needs only a line, a
//! triangle and a text primitive. Text is owned (`String`), never borrowed from
//! the image of the module that drew it, so a reload cannot leave a label
//! pointing into unmapped memory.

// Current crate
use crate::resource::Resource;

/// A colour, linear RGBA in `0..=1`.
pub type DebugColor = [f32; 4];

/// How long one drawn item stays.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum DebugLifetime {
    /// Drawn in the next frame only - the immediate-mode default.
    #[default]
    OneFrame,
    /// Drawn every frame for this many seconds.
    Seconds(f32),
}

/// How one drawn item looks and lasts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DebugStyle {
    /// Its colour.
    pub color: DebugColor,
    /// Whether scene geometry in front hides it (`true`), or it shows through
    /// everything (`false`).
    pub depth_test: bool,
    /// How long it stays.
    pub lifetime: DebugLifetime,
}

impl Default for DebugStyle {
    fn default() -> Self {
        Self {
            color: [1.0, 1.0, 1.0, 1.0],
            depth_test: true,
            lifetime: DebugLifetime::OneFrame,
        }
    }
}

impl DebugStyle {
    /// The default style in `color`.
    pub fn colored(color: DebugColor) -> Self {
        Self {
            color,
            ..Self::default()
        }
    }
}

/// Where a text label is placed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LabelAnchor {
    /// A world position, projected by each surface's camera.
    World([f32; 3]),
    /// Physical pixels from a surface's top-left corner.
    Screen {
        /// Pixels from the left edge.
        x: f32,
        /// Pixels from the top edge.
        y: f32,
    },
}

/// One line segment of a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DebugLine {
    /// Where it starts, in world space.
    pub start: [f32; 3],
    /// Where it ends, in world space.
    pub end: [f32; 3],
    /// Its colour.
    pub color: DebugColor,
    /// Whether scene geometry hides it.
    pub depth_test: bool,
}

/// One text label of a frame.
#[derive(Debug, Clone, PartialEq)]
pub struct DebugLabel {
    /// Where it is placed.
    pub anchor: LabelAnchor,
    /// What it says; printable ASCII is drawn, anything else as `?`.
    pub text: String,
    /// Its colour.
    pub color: DebugColor,
}

/// One gizmo line, drawn on top of everything and pickable by its handle id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GizmoLine {
    /// Where it starts, in world space.
    pub start: [f32; 3],
    /// Where it ends, in world space.
    pub end: [f32; 3],
    /// Its colour.
    pub color: DebugColor,
    /// The id a pick of it reports (`1..=0x7FFF_FFFF`; `0` is not pickable).
    pub handle: u32,
}

/// One gizmo triangle, drawn on top of everything and pickable by its handle
/// id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GizmoTriangle {
    /// Its corners, in world space.
    pub corners: [[f32; 3]; 3],
    /// Its colour.
    pub color: DebugColor,
    /// The id a pick of it reports (`1..=0x7FFF_FFFF`; `0` is not pickable).
    pub handle: u32,
}

/// Everything debug-drawn for one frame, as the renderer receives it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DebugFrame {
    /// Line segments, shapes included.
    pub lines: Vec<DebugLine>,
    /// Text labels.
    pub labels: Vec<DebugLabel>,
    /// Gizmo handle lines.
    pub gizmo_lines: Vec<GizmoLine>,
    /// Gizmo handle triangles.
    pub gizmo_triangles: Vec<GizmoTriangle>,
}

impl DebugFrame {
    /// Whether there is nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
            && self.labels.is_empty()
            && self.gizmo_lines.is_empty()
            && self.gizmo_triangles.is_empty()
    }
}

/// One item with the lifetime it has left.
#[derive(Debug, Clone)]
struct Timed<T> {
    item: T,
    lifetime: DebugLifetime,
}

/// Segments a circle is drawn with.
const CIRCLE_SEGMENTS: usize = 32;

/// The debug drawing of a world: a resource systems draw into and the renderer
/// collects once a frame.
///
/// Created enabled in a development host and an editor; a shipping or web
/// build creates it disabled unless the project's settings turn it on
/// (`debug_draw: true`), and [`Self::set_enabled`] switches it at runtime.
#[derive(Debug, Clone)]
pub struct DebugDraw {
    enabled: bool,
    lines: Vec<Timed<DebugLine>>,
    labels: Vec<Timed<DebugLabel>>,
    gizmo_lines: Vec<GizmoLine>,
    gizmo_triangles: Vec<GizmoTriangle>,
}

impl Default for DebugDraw {
    /// Enabled: the development default.
    fn default() -> Self {
        Self::new(true)
    }
}

impl Resource for DebugDraw {
    fn shared_name() -> Option<&'static str> {
        Some("pill_engine::debug_draw::DebugDraw")
    }
}

impl DebugDraw {
    /// An empty debug drawing, enabled or not.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            lines: Vec::new(),
            labels: Vec::new(),
            gizmo_lines: Vec::new(),
            gizmo_triangles: Vec::new(),
        }
    }

    /// Whether drawing calls record anything.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Switch debug drawing on or off; switching it off also forgets every
    /// item still waiting to be drawn.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.clear();
        }
    }

    /// Forget every item.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.labels.clear();
        self.gizmo_lines.clear();
        self.gizmo_triangles.clear();
    }

    /// A line segment from `start` to `end`.
    pub fn line(&mut self, start: [f32; 3], end: [f32; 3], style: DebugStyle) -> &mut Self {
        if self.enabled {
            self.lines.push(Timed {
                item: DebugLine {
                    start,
                    end,
                    color: style.color,
                    depth_test: style.depth_test,
                },
                lifetime: style.lifetime,
            });
        }
        self
    }

    /// An axis-aligned box around `center`, `half_extents` from it on each axis.
    pub fn aabb(
        &mut self,
        center: [f32; 3],
        half_extents: [f32; 3],
        style: DebugStyle,
    ) -> &mut Self {
        if !self.enabled {
            return self;
        }
        let corner = |x: f32, y: f32, z: f32| {
            [
                center[0] + x * half_extents[0],
                center[1] + y * half_extents[1],
                center[2] + z * half_extents[2],
            ]
        };
        let corners = [
            corner(-1.0, -1.0, -1.0),
            corner(1.0, -1.0, -1.0),
            corner(1.0, 1.0, -1.0),
            corner(-1.0, 1.0, -1.0),
            corner(-1.0, -1.0, 1.0),
            corner(1.0, -1.0, 1.0),
            corner(1.0, 1.0, 1.0),
            corner(-1.0, 1.0, 1.0),
        ];
        // The near face, the far face, and the four edges joining them.
        for (from, to) in [
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 0),
            (4, 5),
            (5, 6),
            (6, 7),
            (7, 4),
            (0, 4),
            (1, 5),
            (2, 6),
            (3, 7),
        ] {
            self.line(corners[from], corners[to], style);
        }
        self
    }

    /// A circle around `center` in the plane whose normal is `normal`.
    pub fn circle(
        &mut self,
        center: [f32; 3],
        normal: [f32; 3],
        radius: f32,
        style: DebugStyle,
    ) -> &mut Self {
        if !self.enabled {
            return self;
        }
        let (axis_u, axis_v) = plane_axes(normal);
        let point = |index: usize| {
            let angle = index as f32 / CIRCLE_SEGMENTS as f32 * std::f32::consts::TAU;
            let (sin, cos) = angle.sin_cos();
            [
                center[0] + radius * (cos * axis_u[0] + sin * axis_v[0]),
                center[1] + radius * (cos * axis_u[1] + sin * axis_v[1]),
                center[2] + radius * (cos * axis_u[2] + sin * axis_v[2]),
            ]
        };
        for index in 0..CIRCLE_SEGMENTS {
            self.line(point(index), point(index + 1), style);
        }
        self
    }

    /// A sphere around `center`: its three great circles.
    pub fn sphere(&mut self, center: [f32; 3], radius: f32, style: DebugStyle) -> &mut Self {
        for normal in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] {
            self.circle(center, normal, radius, style);
        }
        self
    }

    /// An arrow from `start` to `end`, with a head a fifth of its length.
    pub fn arrow(&mut self, start: [f32; 3], end: [f32; 3], style: DebugStyle) -> &mut Self {
        if !self.enabled {
            return self;
        }
        self.line(start, end, style);
        let direction = subtract(end, start);
        let length = length(direction);
        if length <= f32::EPSILON {
            return self;
        }
        let forward = scale(direction, 1.0 / length);
        let (side, up) = plane_axes(forward);
        let head = length * 0.2;
        let back = subtract(end, scale(forward, head));
        for offset in [side, scale(side, -1.0), up, scale(up, -1.0)] {
            self.line(end, add(back, scale(offset, head * 0.5)), style);
        }
        self
    }

    /// A text label at a world position.
    pub fn label_world(&mut self, position: [f32; 3], text: &str, style: DebugStyle) -> &mut Self {
        self.label(LabelAnchor::World(position), text, style)
    }

    /// A text label at physical pixels from a surface's top-left corner.
    pub fn label_screen(&mut self, x: f32, y: f32, text: &str, style: DebugStyle) -> &mut Self {
        self.label(LabelAnchor::Screen { x, y }, text, style)
    }

    /// A text label at `anchor`.
    pub fn label(&mut self, anchor: LabelAnchor, text: &str, style: DebugStyle) -> &mut Self {
        if self.enabled {
            self.labels.push(Timed {
                item: DebugLabel {
                    anchor,
                    text: text.to_owned(),
                    color: style.color,
                },
                lifetime: style.lifetime,
            });
        }
        self
    }

    /// A gizmo line, drawn on top for the next frame and pickable by `handle`.
    pub fn gizmo_line(
        &mut self,
        start: [f32; 3],
        end: [f32; 3],
        color: DebugColor,
        handle: u32,
    ) -> &mut Self {
        if self.enabled {
            self.gizmo_lines.push(GizmoLine {
                start,
                end,
                color,
                handle,
            });
        }
        self
    }

    /// A gizmo triangle, drawn on top for the next frame and pickable by
    /// `handle`.
    pub fn gizmo_triangle(
        &mut self,
        corners: [[f32; 3]; 3],
        color: DebugColor,
        handle: u32,
    ) -> &mut Self {
        if self.enabled {
            self.gizmo_triangles.push(GizmoTriangle {
                corners,
                color,
                handle,
            });
        }
        self
    }

    /// Hand over one frame's worth of items and age the rest by
    /// `delta_seconds`.
    ///
    /// One-frame items and gizmo geometry are taken; timed items are copied
    /// and kept until their time runs out. A disabled drawing hands over an
    /// empty frame.
    pub fn collect_frame(&mut self, delta_seconds: f32) -> DebugFrame {
        if !self.enabled {
            return DebugFrame::default();
        }
        let frame = DebugFrame {
            lines: self.lines.iter().map(|timed| timed.item).collect(),
            labels: self.labels.iter().map(|timed| timed.item.clone()).collect(),
            gizmo_lines: std::mem::take(&mut self.gizmo_lines),
            gizmo_triangles: std::mem::take(&mut self.gizmo_triangles),
        };
        age(&mut self.lines, delta_seconds);
        age(&mut self.labels, delta_seconds);
        frame
    }
}

/// Drop the items whose time ran out and count the rest down.
fn age<T>(items: &mut Vec<Timed<T>>, delta_seconds: f32) {
    items.retain_mut(|timed| match &mut timed.lifetime {
        DebugLifetime::OneFrame => false,
        DebugLifetime::Seconds(remaining) => {
            *remaining -= delta_seconds;
            *remaining > 0.0
        }
    });
}

/// Two unit axes spanning the plane whose normal is `normal`.
fn plane_axes(normal: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let normal_length = length(normal);
    let normal = if normal_length > f32::EPSILON {
        scale(normal, 1.0 / normal_length)
    } else {
        [0.0, 1.0, 0.0]
    };
    // Any vector not parallel to the normal gives the first axis.
    let helper = if normal[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let axis_u = normalize(cross(normal, helper));
    let axis_v = cross(normal, axis_u);
    (axis_u, axis_v)
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn subtract(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: [f32; 3], factor: f32) -> [f32; 3] {
    [a[0] * factor, a[1] * factor, a[2] * factor]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn length(a: [f32; 3]) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let length = length(a);
    if length > f32::EPSILON {
        scale(a, 1.0 / length)
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disabled drawing records nothing and hands over nothing.
    #[test]
    fn a_disabled_drawing_records_nothing() {
        let mut draw = DebugDraw::new(false);
        draw.line([0.0; 3], [1.0; 3], DebugStyle::default())
            .label_world([0.0; 3], "x", DebugStyle::default())
            .gizmo_line([0.0; 3], [1.0; 3], [1.0; 4], 1);
        assert!(draw.collect_frame(0.016).is_empty());
    }

    /// One-frame items are drawn once; timed ones until their time runs out.
    #[test]
    fn items_live_as_long_as_their_lifetime() {
        let mut draw = DebugDraw::new(true);
        draw.line([0.0; 3], [1.0; 3], DebugStyle::default());
        draw.line(
            [0.0; 3],
            [2.0; 3],
            DebugStyle {
                lifetime: DebugLifetime::Seconds(0.05),
                ..DebugStyle::default()
            },
        );
        assert_eq!(draw.collect_frame(0.02).lines.len(), 2);
        assert_eq!(draw.collect_frame(0.02).lines.len(), 1);
        assert_eq!(draw.collect_frame(0.02).lines.len(), 1);
        assert!(draw.collect_frame(0.02).lines.is_empty());
    }

    /// Shapes leave as lines: a box is twelve edges, a sphere three circles.
    #[test]
    fn shapes_are_tessellated_into_lines() {
        let mut draw = DebugDraw::new(true);
        draw.aabb([0.0; 3], [1.0; 3], DebugStyle::default());
        assert_eq!(draw.collect_frame(0.0).lines.len(), 12);
        draw.sphere([0.0; 3], 1.0, DebugStyle::default());
        assert_eq!(draw.collect_frame(0.0).lines.len(), 3 * CIRCLE_SEGMENTS);
        draw.arrow([0.0; 3], [0.0, 0.0, 1.0], DebugStyle::default());
        assert_eq!(draw.collect_frame(0.0).lines.len(), 5);
    }

    /// Every point of a circle lies at its radius from its center.
    #[test]
    fn a_circle_keeps_its_radius() {
        let mut draw = DebugDraw::new(true);
        draw.circle([1.0, 2.0, 3.0], [0.3, 0.9, 0.1], 2.0, DebugStyle::default());
        for line in draw.collect_frame(0.0).lines {
            let distance = length(subtract(line.start, [1.0, 2.0, 3.0]));
            assert!((distance - 2.0).abs() < 1e-4, "{distance}");
        }
    }

    /// Switching off forgets what was waiting; gizmo geometry is one frame.
    #[test]
    fn switching_off_forgets_waiting_items() {
        let mut draw = DebugDraw::new(true);
        draw.gizmo_triangle([[0.0; 3]; 3], [1.0; 4], 7);
        let frame = draw.collect_frame(0.0);
        assert_eq!(frame.gizmo_triangles[0].handle, 7);
        assert!(draw.collect_frame(0.0).gizmo_triangles.is_empty());
        draw.line(
            [0.0; 3],
            [1.0; 3],
            DebugStyle {
                lifetime: DebugLifetime::Seconds(10.0),
                ..DebugStyle::default()
            },
        );
        draw.set_enabled(false);
        draw.set_enabled(true);
        assert!(draw.collect_frame(0.0).is_empty());
    }
}

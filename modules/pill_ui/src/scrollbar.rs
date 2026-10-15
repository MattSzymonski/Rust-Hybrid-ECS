//! Scrollbar geometry and hit math for scroll areas.
//!
//! # Responsibilities
//!
//! - `ScrollMetrics`: one scroll area's viewport, content extent, current
//!   offset and the largest offset it may reach, as layout measured them.
//! - `Axis`: which of the two scrollbars a value belongs to.
//! - `geometry`: where a scrollbar's track and thumb sit for the metrics.
//! - `scroll_step` / `thumb_offset`: the drag and keyboard math.
//!
//! # Design
//!
//! Layout registers the track and thumb as hit targets, the builder reads
//! them back a frame later to drive the drag, and painting draws them; all
//! three must agree on the same rectangles, so the arithmetic lives here
//! once. Everything is plain data: the metrics a frame computes could cross
//! a reload boundary without dangling.

// External crates
use pill_core::math::Vector2f;

// Current crate
use crate::draw::Rect;

// =============================================================================
// Constants
// =============================================================================

/// The default scrollbar strip width in logical pixels.
pub const DEFAULT_WIDTH: f32 = 9.0;

/// The shortest a thumb may get, so a huge document still has something to
/// grab in logical pixels.
pub const MIN_THUMB_LENGTH: f32 = 24.0;

// =============================================================================
// Axis
// =============================================================================

/// Which scrollbar of an area a value belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Axis {
    /// The vertical bar, on the right edge.
    Vertical,
    /// The horizontal bar, on the bottom edge.
    Horizontal,
}

impl Axis {
    /// Both axes, vertical first.
    pub const BOTH: [Axis; 2] = [Axis::Vertical, Axis::Horizontal];

    /// The component of `size` along this axis.
    pub fn length(self, size: Vector2f) -> f32 {
        match self {
            Axis::Vertical => size.y,
            Axis::Horizontal => size.x,
        }
    }

    /// The extent of `rect` along this axis.
    pub fn rect_length(self, rect: Rect) -> f32 {
        match self {
            Axis::Vertical => rect.height(),
            Axis::Horizontal => rect.width(),
        }
    }

    /// The component of `point` along this axis.
    pub fn position(self, point: Vector2f) -> f32 {
        match self {
            Axis::Vertical => point.y,
            Axis::Horizontal => point.x,
        }
    }

    /// The suffix the axis' scrollbar widgets are named with.
    pub fn suffix(self) -> &'static str {
        match self {
            Axis::Vertical => "y",
            Axis::Horizontal => "x",
        }
    }

    /// Whether this axis is the horizontal one.
    pub fn is_horizontal(self) -> bool {
        self == Axis::Horizontal
    }
}

// =============================================================================
// ScrollMetrics
// =============================================================================

/// One scroll area's geometry for the frame it was laid out in.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScrollMetrics {
    /// The visible rectangle of the area.
    pub viewport: Rect,
    /// The content's extent (width and height) before clipping.
    pub content: Vector2f,
    /// The applied offset (the stored scroll, clamped).
    pub offset: Vector2f,
    /// The largest offset each axis may reach; zero when it fits.
    pub max_offset: Vector2f,
}

impl ScrollMetrics {
    /// The largest offset of `axis`.
    pub fn max(&self, axis: Axis) -> f32 {
        axis.length(self.max_offset)
    }

    /// The applied offset of `axis`.
    pub fn offset(&self, axis: Axis) -> f32 {
        axis.length(self.offset)
    }

    /// The visible length of `axis`.
    pub fn visible(&self, axis: Axis) -> f32 {
        axis.rect_length(self.viewport)
    }

    /// Whether `axis` overflows and gets a scrollbar.
    pub fn overflows(&self, axis: Axis) -> bool {
        self.max(axis) > 0.0
    }
}

// =============================================================================
// Geometry
// =============================================================================

/// The track and thumb rectangles of one scrollbar.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScrollBarGeometry {
    /// The strip the thumb slides in, spanning the viewport's edge.
    pub track: Rect,
    /// The draggable thumb.
    pub thumb: Rect,
}

/// Where `axis`'s scrollbar sits for `metrics`, or `None` when the content
/// fits and no bar shows.
///
/// `width` is the strip's thickness in logical pixels; the vertical bar hugs
/// the viewport's right edge, the horizontal one its bottom edge.
pub fn geometry(metrics: &ScrollMetrics, axis: Axis, width: f32) -> Option<ScrollBarGeometry> {
    if !metrics.overflows(axis) {
        return None;
    }
    let viewport = metrics.viewport;
    let width = width.max(1.0);
    let track = match axis {
        Axis::Vertical => Rect::new(
            Vector2f::new(viewport.max.x - width, viewport.min.y),
            Vector2f::new(viewport.max.x, viewport.max.y),
        ),
        Axis::Horizontal => Rect::new(
            Vector2f::new(viewport.min.x, viewport.max.y - width),
            Vector2f::new(viewport.max.x, viewport.max.y),
        ),
    };
    let track_length = axis.rect_length(track);
    let content_length = axis.length(metrics.content);
    let visible = metrics.visible(axis);
    let thumb_length = (track_length * (visible / content_length).clamp(0.0, 1.0))
        .clamp(MIN_THUMB_LENGTH.min(track_length), track_length);
    let travel = (track_length - thumb_length).max(0.0);
    let ratio = if metrics.max(axis) > 0.0 {
        (metrics.offset(axis) / metrics.max(axis)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let thumb = match axis {
        Axis::Vertical => Rect::new(
            Vector2f::new(track.min.x, track.min.y + ratio * travel),
            Vector2f::new(track.max.x, track.min.y + ratio * travel + thumb_length),
        ),
        Axis::Horizontal => Rect::new(
            Vector2f::new(track.min.x + ratio * travel, track.min.y),
            Vector2f::new(track.min.x + ratio * travel + thumb_length, track.max.y),
        ),
    };
    Some(ScrollBarGeometry { track, thumb })
}

/// The scroll offset a drag from `grab_offset` puts the thumb at, given the
/// pointer has moved to `pointer` along `axis`.
///
/// `grab_pointer` is where the pointer was when the drag began;
/// `grab_offset` is the offset then. The result is clamped to
/// `0..=metrics.max(axis)`.
pub fn thumb_offset(
    metrics: &ScrollMetrics,
    axis: Axis,
    pointer: f32,
    grab_pointer: f32,
    grab_offset: f32,
) -> f32 {
    let Some(geometry) = geometry(metrics, axis, DEFAULT_WIDTH) else {
        return grab_offset;
    };
    let travel = axis.rect_length(geometry.track) - axis.rect_length(geometry.thumb);
    if travel <= 0.0 {
        return 0.0;
    }
    let per_pixel = metrics.max(axis) / travel;
    (grab_offset + (pointer - grab_pointer) * per_pixel).clamp(0.0, metrics.max(axis))
}

/// The offset that centers the thumb on `pointer` (a click on the track).
pub fn track_offset(metrics: &ScrollMetrics, axis: Axis, pointer: f32) -> f32 {
    let Some(geometry) = geometry(metrics, axis, DEFAULT_WIDTH) else {
        return 0.0;
    };
    let track_length = axis.rect_length(geometry.track);
    let thumb_length = axis.rect_length(geometry.thumb);
    let travel = (track_length - thumb_length).max(0.0);
    if travel <= 0.0 {
        return 0.0;
    }
    let position =
        (pointer - axis.position(geometry.track.min) - thumb_length / 2.0).clamp(0.0, travel);
    (position / travel * metrics.max(axis)).clamp(0.0, metrics.max(axis))
}

/// One keyboard scroll step along `axis`: a page, less one line of overlap so
/// the reader does not lose their place.
pub fn scroll_step(metrics: &ScrollMetrics, axis: Axis) -> f32 {
    (metrics.visible(axis) - crate::panel::SCROLL_LINE_HEIGHT).max(crate::panel::SCROLL_LINE_HEIGHT)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(viewport: Rect, content: Vector2f, offset: Vector2f) -> ScrollMetrics {
        ScrollMetrics {
            viewport,
            content,
            offset,
            max_offset: Vector2f::new(
                (content.x - viewport.width()).max(0.0),
                (content.y - viewport.height()).max(0.0),
            ),
        }
    }

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect::from_min_size(Vector2f::new(x, y), Vector2f::new(width, height))
    }

    #[test]
    fn a_fitting_area_has_no_scrollbar() {
        let area = metrics(
            rect(0.0, 0.0, 100.0, 80.0),
            Vector2f::new(100.0, 80.0),
            Vector2f::ZERO,
        );
        assert!(geometry(&area, Axis::Vertical, 8.0).is_none());
        assert!(geometry(&area, Axis::Horizontal, 8.0).is_none());
    }

    #[test]
    fn the_vertical_thumb_spans_proportionally_and_slides_with_the_offset() {
        let area = metrics(
            rect(10.0, 20.0, 100.0, 80.0),
            Vector2f::new(100.0, 160.0),
            Vector2f::ZERO,
        );
        let top = geometry(&area, Axis::Vertical, 8.0).unwrap();
        assert_eq!(top.track, rect(102.0, 20.0, 8.0, 80.0));
        assert_eq!(top.thumb, rect(102.0, 20.0, 8.0, 40.0));

        // At the end of the content the thumb sits at the track's bottom.
        let area = metrics(
            rect(10.0, 20.0, 100.0, 80.0),
            Vector2f::new(100.0, 160.0),
            Vector2f::new(0.0, 80.0),
        );
        let bottom = geometry(&area, Axis::Vertical, 8.0).unwrap();
        assert_eq!(bottom.thumb, rect(102.0, 60.0, 8.0, 40.0));
    }

    #[test]
    fn a_tiny_thumb_stops_at_the_minimum_length() {
        let area = metrics(
            rect(0.0, 0.0, 100.0, 100.0),
            Vector2f::new(100.0, 10_000.0),
            Vector2f::ZERO,
        );
        let bar = geometry(&area, Axis::Vertical, 8.0).unwrap();
        assert_eq!(bar.thumb.height(), MIN_THUMB_LENGTH);
    }

    #[test]
    fn the_horizontal_bar_hugs_the_bottom_edge() {
        let area = metrics(
            rect(0.0, 0.0, 80.0, 50.0),
            Vector2f::new(240.0, 50.0),
            Vector2f::ZERO,
        );
        let bar = geometry(&area, Axis::Horizontal, 8.0).unwrap();
        assert_eq!(bar.track, rect(0.0, 42.0, 80.0, 8.0));
        // 240 wide in an 80 viewport: a third of the track, at the left.
        assert!((bar.thumb.width() - 80.0 / 3.0).abs() < 0.01);
        assert_eq!(bar.thumb.min.x, 0.0);
    }

    #[test]
    fn dragging_maps_pointer_motion_onto_the_scroll_range() {
        let area = metrics(
            rect(0.0, 0.0, 100.0, 80.0),
            Vector2f::new(100.0, 160.0),
            Vector2f::new(0.0, 40.0),
        );
        // Thumb travel is 40 px over a scroll range of 80: 2 px per pixel.
        let moved = thumb_offset(&area, Axis::Vertical, 55.0, 50.0, 40.0);
        assert!((moved - 50.0).abs() < 0.01);
        // The result clamps to the range.
        assert_eq!(thumb_offset(&area, Axis::Vertical, 500.0, 50.0, 40.0), 80.0);
        assert_eq!(thumb_offset(&area, Axis::Vertical, -500.0, 50.0, 40.0), 0.0);
    }

    #[test]
    fn a_track_click_centers_the_thumb() {
        let area = metrics(
            rect(0.0, 0.0, 100.0, 80.0),
            Vector2f::new(100.0, 160.0),
            Vector2f::ZERO,
        );
        // Clicking the track's middle (y = 40) centers the 40 px thumb
        // there: its top lands at 20, half of the 40 px travel, half the
        // scroll range.
        let offset = track_offset(&area, Axis::Vertical, 40.0);
        assert!((offset - 40.0).abs() < 0.01);
        assert_eq!(track_offset(&area, Axis::Vertical, -100.0), 0.0);
        assert_eq!(track_offset(&area, Axis::Vertical, 1000.0), 80.0);
    }
}

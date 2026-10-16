//! Where a popup opens: the flip and slide that keep it inside the monitor's
//! work area (Stage 21, D23).
//!
//! # Responsibilities
//!
//! - [`WorkArea`]: the usable screen rectangle of the monitor a window is on,
//!   in physical pixels, with the taskbar and the docked appbars removed.
//! - [`place_popup`]: turn the anchor a shell asked for into the position the
//!   popup window actually opens at, flipping it to the other side of the
//!   anchor and sliding it back inside the work area when it would not fit.
//!
//! # Design
//!
//! The arithmetic is platform-free so it can be tested without a monitor: the
//! platform layer (Stage 21's Windows half, Stage 31's Linux half) only
//! supplies the [`WorkArea`] and opens the window at the position this module
//! returns. A popup is anchored at its top-left, so a flip moves it by its own
//! size, and the result is always inside the work area unless the popup is
//! larger than the area itself.

/// A monitor's usable rectangle in physical pixels.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WorkArea {
    /// The left edge, in screen coordinates.
    pub x: i32,
    /// The top edge, in screen coordinates.
    pub y: i32,
    /// The width in pixels.
    pub width: u32,
    /// The height in pixels.
    pub height: u32,
}

impl WorkArea {
    /// A work area from its position and size.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> WorkArea {
        WorkArea {
            x,
            y,
            width,
            height,
        }
    }

    /// The right edge, exclusive.
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    /// The bottom edge, exclusive.
    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }
}

/// Where a popup opens, and how it got there.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PopupPlacement {
    /// The popup window's top-left, in physical screen pixels.
    pub origin: (i32, i32),
    /// Whether the popup was flipped to the other side of its anchor because
    /// it would have crossed the work area's right or bottom edge.
    pub flipped: bool,
    /// Whether the popup had to slide back inside the work area because it
    /// did not fit on either side of the anchor.
    pub slid: bool,
}

/// Place a popup of `size` at `anchor`, flipped and slid inside `work`.
///
/// `anchor` is where the popup's top-left was asked for - for a menu, the
/// top-left of the item that opened it, in physical screen pixels. The popup
/// grows to the right and down while it fits; when it would cross an edge it
/// is flipped to the other side of the anchor (its edge on the anchor's
/// edge), and when it fits on neither side it slides back inside.
pub fn place_popup(anchor: (i32, i32), size: (u32, u32), work: WorkArea) -> PopupPlacement {
    let width = size.0 as i32;
    let height = size.1 as i32;
    let mut flipped = false;
    let mut slid = false;
    // Step 1: horizontally, prefer the anchor's right, flip to its left, then
    // slide inside.
    let x = if anchor.0 + width <= work.right() {
        anchor.0
    } else if anchor.0 - width >= work.x {
        flipped = true;
        anchor.0 - width
    } else {
        slid = true;
        (work.right() - width).max(work.x)
    };
    // Step 2: the same downwards, so a menu near the bottom of the screen
    // opens upwards.
    let y = if anchor.1 + height <= work.bottom() {
        anchor.1
    } else if anchor.1 - height >= work.y {
        flipped = true;
        anchor.1 - height
    } else {
        slid = true;
        (work.bottom() - height).max(work.y)
    };
    PopupPlacement {
        origin: (x, y),
        flipped,
        slid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920x1080 work area at the origin.
    fn work() -> WorkArea {
        WorkArea::new(0, 0, 1920, 1080)
    }

    #[test]
    fn a_popup_that_fits_opens_at_its_anchor() {
        let placement = place_popup((100, 100), (200, 300), work());
        assert_eq!(placement.origin, (100, 100));
        assert!(!placement.flipped);
        assert!(!placement.slid);
    }

    #[test]
    fn a_popup_past_the_right_edge_flips_to_the_left_of_its_anchor() {
        let placement = place_popup((1850, 100), (200, 300), work());
        assert_eq!(placement.origin, (1650, 100));
        assert!(placement.flipped);
        assert!(!placement.slid);
    }

    #[test]
    fn a_popup_past_the_bottom_edge_flips_above_its_anchor() {
        let placement = place_popup((100, 1050), (200, 300), work());
        assert_eq!(placement.origin, (100, 750));
        assert!(placement.flipped);
        assert!(!placement.slid);
    }

    #[test]
    fn a_popup_that_fits_on_neither_side_slides_inside() {
        // A popup wider than the room on either side of the anchor.
        let placement = place_popup((1000, 100), (1500, 300), work());
        assert_eq!(placement.origin.0, 420);
        assert!(placement.slid);
        assert!(!placement.flipped);
    }

    #[test]
    fn a_popup_larger_than_the_work_area_stays_at_its_upper_left() {
        let placement = place_popup((500, 500), (2400, 1400), work());
        assert_eq!(placement.origin, (0, 0));
        assert!(placement.slid);
    }

    #[test]
    fn the_work_area_offset_is_honoured() {
        // A second monitor to the right of the primary one.
        let second = WorkArea::new(1920, 0, 1280, 1024);
        let placement = place_popup((3180, 100), (200, 300), second);
        assert_eq!(placement.origin, (2980, 100));
        assert!(placement.flipped);
        let inside = place_popup((2000, 100), (200, 300), second);
        assert_eq!(inside.origin, (2000, 100));
    }
}

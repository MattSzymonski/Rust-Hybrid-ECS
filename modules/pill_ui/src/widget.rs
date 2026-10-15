//! Widget identity and the plain-data state a widget keeps between frames.
//!
//! # Responsibilities
//!
//! - [`WidgetId`]: the stable name a widget's state is keyed by, derived from
//!   the widget's name and its position in the panel.
//! - [`WidgetMemory`]: everything one widget remembers between frames - text
//!   cursor, scroll offset, disclosure state, list selection.
//! - [`TextEditState`]: a text field's cursor and selection, and the editing
//!   operations on them.
//!
//! # Design
//!
//! Ids are hashes of (parent, name). The same call sequence produces the same
//! ids, so state survives the tree being rebuilt every frame; renaming or
//! reordering a widget starts its state fresh, which is the standard
//! immediate-mode contract. `TextEditState` keeps byte offsets and repairs
//! them against the string on use, so a caller that replaces a field's text
//! wholesale cannot leave a dangling cursor.

// Standard library
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// =============================================================================
// WidgetId
// =============================================================================

/// A widget's stable identity within a UI session.
///
/// Two widgets share state exactly when their ids are equal. Derive ids from
/// a name and the build position ([`WidgetId::named`], or the automatic
/// derivation every builder call performs); use [`WidgetId::from_raw`] only
/// when an external system (a scene entity, a project item) already owns the
/// identity.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WidgetId(u64);

impl WidgetId {
    /// The root the panel's ids hang off.
    pub const ROOT: WidgetId = WidgetId(0);

    /// A top-level id from a name.
    pub fn from_name(name: &str) -> WidgetId {
        WidgetId::named(WidgetId::ROOT, name)
    }

    /// The id of the widget called `name` inside `parent`.
    pub fn named(parent: WidgetId, name: &str) -> WidgetId {
        let mut hasher = DefaultHasher::new();
        parent.0.hash(&mut hasher);
        name.hash(&mut hasher);
        WidgetId(hasher.finish())
    }

    /// An id from an external system's own number.
    pub fn from_raw(value: u64) -> WidgetId {
        WidgetId(value)
    }

    /// The id as a plain number, for passing to systems that do not know
    /// about widgets.
    pub fn raw(self) -> u64 {
        self.0
    }
}

// =============================================================================
// Widget memory
// =============================================================================

/// The state one widget keeps between frames.
///
/// One struct with optional fields rather than a map per widget kind: a
/// widget uses at most a couple of these, and a single map keeps the state
/// easy to inspect and to carry across a reload. Fields are empty until the
/// widget kind that owns them first needs one.
#[derive(Clone, Debug, Default)]
pub struct WidgetMemory {
    /// Text editing state, for text fields.
    pub text: Option<TextEditState>,
    /// Vertical scroll offset in logical pixels, for scroll areas.
    pub scroll: f32,
    /// Horizontal scroll offset in logical pixels, for scroll areas.
    pub scroll_x: f32,
    /// The scrollbar drag in progress, if any.
    pub scroll_grab: Option<ScrollGrab>,
    /// Whether a collapsing section - or one tree row - is open.
    pub open: Option<bool>,
    /// The selected row, for tables and trees.
    pub selection: Option<usize>,
}

/// A scrollbar thumb drag in progress on a scroll area.
///
/// Plain data: the drag is just where the pointer grabbed the thumb and what
/// the offset was then, so the pointer's later positions map onto the scroll
/// range without the thumb having to remember pixel distances.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScrollGrab {
    /// The bar being dragged.
    pub axis: crate::scrollbar::Axis,
    /// The pointer coordinate along the bar when the drag began.
    pub pointer: f32,
    /// The scroll offset of that axis when the drag began.
    pub scroll: f32,
}

// =============================================================================
// Text editing
// =============================================================================

/// A text field's cursor and selection, as byte offsets into the field's
/// string.
///
/// `cursor` is where the caret sits; `anchor` is where the selection began.
/// They are equal when nothing is selected. Both are clamped to char
/// boundaries before every use, so text replaced from outside never requires
/// the caller to touch this state.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TextEditState {
    /// The caret position in bytes.
    pub cursor: usize,
    /// The selection anchor in bytes; equal to `cursor` when nothing is
    /// selected.
    pub anchor: usize,
}

impl TextEditState {
    /// State with the caret at the end of `text`.
    pub fn at_end(text: &str) -> TextEditState {
        let end = text.len();
        TextEditState {
            cursor: end,
            anchor: end,
        }
    }

    /// Whether anything is selected.
    pub fn has_selection(&self) -> bool {
        self.cursor != self.anchor
    }

    /// The selected byte range, ordered, empty when nothing is selected.
    pub fn selection(&self) -> (usize, usize) {
        if self.cursor <= self.anchor {
            (self.cursor, self.anchor)
        } else {
            (self.anchor, self.cursor)
        }
    }

    /// The selected text, if any.
    pub fn selected_text<'a>(&self, text: &'a str) -> Option<&'a str> {
        let (start, end) = self.selection();
        if start == end {
            return None;
        }
        text.get(start..end)
    }

    /// Move both ends to `byte`, clamped to a char boundary.
    pub fn move_to(&mut self, text: &str, byte: usize) {
        let byte = clamp_to_boundary(text, byte);
        self.cursor = byte;
        self.anchor = byte;
    }

    /// Move the caret one character left or right, keeping the anchor when
    /// `extend` is set and collapsing the selection to the other end when
    /// not.
    pub fn move_cursor(&mut self, text: &str, forward: bool, extend: bool) {
        let (start, end) = self.selection();
        let target = if !extend && start != end {
            // Without extension, a move collapses the selection to the end
            // it is heading away from.
            if forward {
                end
            } else {
                start
            }
        } else {
            step_boundary(text, self.cursor, forward)
        };
        self.cursor = target;
        if !extend {
            self.anchor = target;
        }
    }

    /// Move the caret to the start or end of the text, extending the
    /// selection when asked.
    pub fn move_to_edge(&mut self, text: &str, to_end: bool, extend: bool) {
        let target = if to_end { text.len() } else { 0 };
        self.cursor = target;
        if !extend {
            self.anchor = target;
        }
    }

    /// Replace the selection - or insert at the caret - with `inserted`.
    pub fn insert(&mut self, text: &mut String, inserted: &str) {
        let (start, end) = self.selection();
        let (start, end) = (clamp_to_boundary(text, start), clamp_to_boundary(text, end));
        text.replace_range(start..end, inserted);
        self.cursor = start + inserted.len();
        self.anchor = self.cursor;
    }

    /// Delete the selection, or the character before the caret.
    pub fn backspace(&mut self, text: &mut String) {
        if self.delete_selection(text) {
            return;
        }
        let cursor = clamp_to_boundary(text, self.cursor);
        if cursor == 0 {
            self.move_to(text, cursor);
            return;
        }
        let start = step_boundary(text, cursor, false);
        text.replace_range(start..cursor, "");
        self.move_to(text, start);
    }

    /// Delete the selection, or the character after the caret.
    pub fn delete(&mut self, text: &mut String) {
        if self.delete_selection(text) {
            return;
        }
        let cursor = clamp_to_boundary(text, self.cursor);
        let end = step_boundary(text, cursor, true);
        text.replace_range(cursor..end, "");
        self.move_to(text, cursor);
    }

    /// Delete the selection when there is one; report whether it did.
    fn delete_selection(&mut self, text: &mut String) -> bool {
        let (start, end) = self.selection();
        if start == end {
            return false;
        }
        let (start, end) = (clamp_to_boundary(text, start), clamp_to_boundary(text, end));
        text.replace_range(start..end, "");
        self.move_to(text, start);
        true
    }

    /// Select everything in `text`.
    pub fn select_all(&mut self, text: &str) {
        self.anchor = 0;
        self.cursor = text.len();
    }
}

/// `byte` clamped to the nearest char boundary at or before it.
fn clamp_to_boundary(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// The char boundary one character before or after `byte`.
fn step_boundary(text: &str, byte: usize, forward: bool) -> usize {
    let byte = clamp_to_boundary(text, byte);
    if forward {
        text[byte..]
            .chars()
            .next()
            .map(|c| byte + c.len_utf8())
            .unwrap_or(byte)
    } else {
        text[..byte]
            .chars()
            .last()
            .map(|c| byte - c.len_utf8())
            .unwrap_or(byte)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = WidgetId::from_name("Bake");
        let b = WidgetId::from_name("Bake");
        let c = WidgetId::from_name("Cancel");
        assert_eq!(a, b);
        assert_ne!(a, c);

        let child = WidgetId::named(a, "row 0");
        let other_parent = WidgetId::named(c, "row 0");
        assert_ne!(child, other_parent, "same name under different parents");
        assert_ne!(WidgetId::ROOT, a);
    }

    #[test]
    fn text_edits_insert_at_the_cursor() {
        let mut text = String::from("hello world");
        let mut edit = TextEditState::at_end(&text);
        edit.cursor = 5;
        edit.anchor = 5;
        edit.insert(&mut text, ",");
        assert_eq!(text, "hello, world");
        assert_eq!(edit.cursor, 6);

        edit.backspace(&mut text);
        assert_eq!(text, "hello world");
        edit.delete(&mut text);
        assert_eq!(text, "helloworld");
    }

    #[test]
    fn text_edits_replace_the_selection() {
        let source = "hello world";
        let mut text = String::from(source);
        let mut edit = TextEditState {
            cursor: 11,
            anchor: 6,
        };
        assert_eq!(edit.selected_text(&text), Some("world"));
        edit.insert(&mut text, "there");
        assert_eq!(text, "hello there");
        assert!(!edit.has_selection());

        edit.select_all(&text);
        edit.backspace(&mut text);
        assert_eq!(text, "");
        assert_eq!(edit.cursor, 0);
    }

    #[test]
    fn text_edits_respect_char_boundaries() {
        let mut text = String::from("héllo");
        // Byte 3 is the boundary between `é` (bytes 1..3) and `l`.
        let mut edit = TextEditState {
            cursor: 3,
            anchor: 3,
        };
        edit.insert(&mut text, "x");
        assert_eq!(text, "héxllo");
        edit.backspace(&mut text);
        assert_eq!(text, "héllo");

        // A cursor from a stale string snaps onto a char boundary and
        // never panics.
        let mut stale = TextEditState {
            cursor: 99,
            anchor: 99,
        };
        stale.delete(&mut text);
        assert_eq!(text, "héllo");
        stale.insert(&mut text, "!");
        assert_eq!(text, "héllo!");
    }

    #[test]
    fn cursor_movement_collapses_selections() {
        let text = "abc def";
        let mut edit = TextEditState {
            cursor: 3,
            anchor: 7,
        };
        edit.move_cursor(text, false, false);
        assert_eq!(edit.cursor, 3, "left collapses to the selection start");
        assert!(!edit.has_selection());

        let mut extending = TextEditState {
            cursor: 3,
            anchor: 3,
        };
        extending.move_cursor(text, true, true);
        assert_eq!(extending.selection(), (3, 4));
        extending.move_to_edge(text, true, true);
        assert_eq!(extending.selection(), (3, 7));
    }
}

//! One RGBA image holding every rasterized glyph and icon.
//!
//! # Responsibilities
//!
//! - [`Atlas`]: an RGBA8 image with a shelf allocator, a keyed entry
//!   registry, page growth and least-recently-used eviction
//!   ([`Atlas::reserve`]), row writes ([`Atlas::write_region`]), and the
//!   bookkeeping a backend needs to upload only what changed
//!   ([`Atlas::take_updated_regions`]).
//! - `Entry`: one cached image's place, use stamp and group tag.
//! - [`Region`]: a rectangle of the image in texels.
//!
//! # Design
//!
//! Entries pack left to right in the current shelf, a full shelf opens the
//! next, and a texel of padding keeps a sampled neighbour from bleeding in.
//! Every entry is registered under its owner's key: [`Atlas::reserve`]
//! hands back the existing region or places a new one, growing the image in
//! pages (each side doubles, up to the cap [`Atlas::with_limit`] sets)
//! before it evicts anything - growth re-creates a backend's texture, while
//! eviction never moves a surviving entry's pixels. At the cap the least
//! recently used entry is evicted until the newcomer fits, and the space it
//! leaves is reused directly, so a cached image that stays visible keeps
//! its place ([`Atlas::touch`] keeps it recent) and an evicted one is
//! simply rasterized again by its owner on the next frame. An entry that
//! still cannot fit comes back as `None`: the caller reports the refusal,
//! the atlas never drops a picture silently. Frontends hand the backend the
//! image once and the dirty regions per frame; a grown image arrives as one
//! whole-image region, so the backend recreates its texture from it.
//!
//! Growth changes the image's size, which re-scales every UV already in a
//! draw list; callers therefore grow between frames where they can
//! ([`Atlas::ensure_headroom`], run at the frame boundary), reserve every
//! glyph of one text before pushing its quads, and accept that a single
//! frame which outgrows a whole page on its own can hold a stale quad - the
//! next frame is correct.

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_core::math::Vector2f;

// The update data moved to `pill_draw` (W-0032): the renderer contract
// carries it in `RenderFrame.ui`, so the UI library and every renderer
// share one definition without the contract depending on this crate.
pub use pill_draw::atlas::{AtlasRegionUpdate, AtlasUpdate, Region};

// =============================================================================
// Atlas
// =============================================================================

/// Texels of padding between entries, against sampling bleed.
const PADDING: u32 = 1;

/// The side length of a default atlas: 2048 texels, 16 MiB of RGBA.
pub const DEFAULT_LENGTH: u32 = 2048;

/// The largest a default atlas grows to: 4096 texels, 64 MiB of RGBA.
/// A full atlas at this size evicts instead of growing.
pub const MAX_LENGTH: u32 = 4096;

/// One cached image's place in the atlas.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Where the image's pixels are.
    pub region: Region,
    /// The touch stamp; the least recently used entry is evicted first.
    pub used_at: u64,
    /// The owner's group number: a rasterization scale or a revision.
    /// [`Atlas::evict_tag`] drops one group in a single call.
    pub tag: u64,
}

/// The shared glyph and icon image.
#[derive(Clone, Debug)]
pub struct Atlas {
    /// Image width in texels.
    width: u32,
    /// Image height in texels.
    height: u32,
    /// The side length growth stops at.
    max_length: u32,
    /// RGBA8 pixels, row-major, row stride `width * 4`.
    pixels: Vec<u8>,
    /// Height of the shelf currently being filled, padding included.
    shelf_height: u32,
    /// Left edge of the free space in the current shelf.
    cursor_x: u32,
    /// Top edge of the current shelf.
    cursor_y: u32,
    /// Slots freed by eviction, reusable as they are.
    freed: Vec<Region>,
    /// Every live entry, by its owner's key.
    entries: BTreeMap<u64, Entry>,
    /// The monotonic stamp [`Self::touch`] and [`Self::reserve`] hand out.
    tick: u64,
    /// Regions written since the last [`Self::take_updated_regions`].
    dirty: Vec<Region>,
}

impl Default for Atlas {
    fn default() -> Atlas {
        Atlas::new(DEFAULT_LENGTH, DEFAULT_LENGTH)
    }
}

impl Atlas {
    /// An empty atlas of the given size, every texel transparent; it grows
    /// in pages up to [`MAX_LENGTH`] on each side as entries arrive.
    pub fn new(width: u32, height: u32) -> Atlas {
        Atlas::with_limit(width, height, MAX_LENGTH)
    }

    /// An empty atlas that grows no further than `max_length` per side.
    ///
    /// The cap makes eviction reachable on small targets and in tests; a
    /// production frontend uses [`Self::new`].
    pub fn with_limit(width: u32, height: u32, max_length: u32) -> Atlas {
        let max_length = max_length.max(width).max(height);
        Atlas {
            width,
            height,
            max_length,
            pixels: vec![0; (width as usize) * (height as usize) * 4],
            shelf_height: 0,
            cursor_x: 0,
            cursor_y: 0,
            freed: Vec::new(),
            entries: BTreeMap::new(),
            tick: 0,
            dirty: Vec::new(),
        }
    }

    /// Image width in texels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Image height in texels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The RGBA8 image, row-major.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The region of `key`'s entry, if it is still placed. Owners rasterize
    /// again when it is not: a full atlas evicted it.
    pub fn region_of(&self, key: u64) -> Option<Region> {
        self.entries.get(&key).map(|entry| entry.region)
    }

    /// Record that `key`'s entry was just used, so it is the last to be
    /// evicted; back comes its region, or `None` when it is gone and the
    /// owner has to rasterize it again.
    pub fn touch(&mut self, key: u64) -> Option<Region> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(&key)?;
        entry.used_at = tick;
        Some(entry.region)
    }

    /// The number of live entries.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Reserve `width` by `height` texels for `key`, growing the image and
    /// evicting the least recently used entries as needed; `None` only when
    /// the entry cannot fit even in an empty atlas at its cap.
    ///
    /// An existing entry for `key` keeps its place and tag and is merely
    /// touched, so an owner may call this every frame.
    pub fn reserve(&mut self, key: u64, width: u32, height: u32, tag: u64) -> Option<Region> {
        if let Some(region) = self.touch(key) {
            return Some(region);
        }
        if width == 0 || height == 0 || width > self.max_length || height > self.max_length {
            return None;
        }
        loop {
            if let Some(region) = self.allocate(width, height) {
                let used_at = self.next_tick();
                self.entries.insert(
                    key,
                    Entry {
                        region,
                        used_at,
                        tag,
                    },
                );
                return Some(region);
            }
            if self.grow() {
                continue;
            }
            // At the cap: free the least recently used entry and try the
            // space it leaves; a full atlas with nothing to evict refuses.
            self.evict_lru()?;
        }
    }

    /// Grow a page when the shelves are three-quarters full, so growth
    /// happens between frames rather than mid-paint.
    ///
    /// A growth re-scales every UV, and quads already in a frame's draw
    /// list would keep the old ones; callers run this right before taking
    /// the frame's update. `false` when there is nothing to do - or the
    /// cap is reached, where the next frame evicts instead.
    pub fn ensure_headroom(&mut self) -> bool {
        let used = self.cursor_y + self.shelf_height;
        if used * 4 < self.height * 3 {
            return false;
        }
        self.grow()
    }

    /// Evict every entry tagged `tag`, returning how many went; their
    /// regions become reusable immediately. Owners use this to drop a whole
    /// rasterization scale in one call (plan decision D30).
    pub fn evict_tag(&mut self, tag: u64) -> usize {
        let victims: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.tag == tag)
            .map(|(key, _)| *key)
            .collect();
        for key in &victims {
            self.remove(*key);
        }
        victims.len()
    }

    /// Reserve a `width` by `height` region without registering an entry:
    /// a slot freed by eviction is used first, then the current shelf.
    pub fn allocate(&mut self, width: u32, height: u32) -> Option<Region> {
        if width == 0 || height == 0 || width > self.width || height > self.height {
            return None;
        }
        if let Some(index) = self
            .freed
            .iter()
            .position(|slot| slot.width >= width && slot.height >= height)
        {
            let slot = self.freed.swap_remove(index);
            let region = Region {
                x: slot.x,
                y: slot.y,
                width,
                height,
            };
            self.carve(slot, region);
            return Some(region);
        }
        if let Some(region) = self.allocate_shelf(width, height) {
            return Some(region);
        }
        // Eviction can leave the freed list fragmented into slots none of
        // which fits the request - even with every entry evicted. With
        // nothing registered, the whole image is free again, so the
        // shelves start over and the fragments go with them.
        if self.entries.is_empty() && !self.freed.is_empty() {
            self.freed.clear();
            self.shelf_height = 0;
            self.cursor_x = 0;
            self.cursor_y = 0;
            return self.allocate_shelf(width, height);
        }
        None
    }

    /// The shelf half of [`Self::allocate`].
    fn allocate_shelf(&mut self, width: u32, height: u32) -> Option<Region> {
        // A row that cannot hold the entry starts the next shelf.
        if self.cursor_x + width > self.width {
            self.cursor_y += self.shelf_height + PADDING;
            self.cursor_x = 0;
            self.shelf_height = 0;
        }
        if self.cursor_y + height > self.height {
            return None;
        }
        let region = Region {
            x: self.cursor_x,
            y: self.cursor_y,
            width,
            height,
        };
        self.cursor_x += width + PADDING;
        self.shelf_height = self.shelf_height.max(height + PADDING);
        Some(region)
    }

    /// Cut `used` out of a freed `slot`, returning its right and bottom
    /// strips to the reusable list when each can hold anything again.
    fn carve(&mut self, slot: Region, used: Region) {
        let right = used.x + used.width + PADDING;
        if slot.x + slot.width > right {
            self.freed.push(Region {
                x: right,
                y: slot.y,
                width: slot.x + slot.width - right,
                height: used.height,
            });
        }
        let bottom = used.y + used.height + PADDING;
        if slot.y + slot.height > bottom {
            self.freed.push(Region {
                x: slot.x,
                y: bottom,
                width: slot.width,
                height: slot.y + slot.height - bottom,
            });
        }
    }

    /// Double each side within the cap, keeping every entry and every pixel
    /// where they are; `false` at the cap.
    ///
    /// The whole image counts as changed afterwards: a backend recreates its
    /// texture for the new size and uploads the image in one go.
    fn grow(&mut self) -> bool {
        let width = (self.width * 2).min(self.max_length);
        let height = (self.height * 2).min(self.max_length);
        if width == self.width && height == self.height {
            return false;
        }
        let old_stride = self.width as usize * 4;
        let new_stride = width as usize * 4;
        let mut pixels = vec![0; new_stride * height as usize];
        for row in 0..self.height as usize {
            pixels[row * new_stride..row * new_stride + old_stride]
                .copy_from_slice(&self.pixels[row * old_stride..row * old_stride + old_stride]);
        }
        self.pixels = pixels;
        self.width = width;
        self.height = height;
        self.dirty.clear();
        self.dirty.push(Region {
            x: 0,
            y: 0,
            width,
            height,
        });
        true
    }

    /// Evict the least recently used entry, back comes its key; `None` when
    /// nothing is registered.
    fn evict_lru(&mut self) -> Option<u64> {
        let victim = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.used_at)
            .map(|(key, _)| *key)?;
        self.remove(victim);
        Some(victim)
    }

    /// Drop `key`'s entry and return its slot - the placement plus the
    /// padding reserved to its right and below - to the reusable list.
    fn remove(&mut self, key: u64) -> Option<Entry> {
        let entry = self.entries.remove(&key)?;
        let max_x = (entry.region.x + entry.region.width + PADDING).min(self.width);
        let max_y = (entry.region.y + entry.region.height + PADDING).min(self.height);
        self.freed.push(Region {
            x: entry.region.x,
            y: entry.region.y,
            width: max_x - entry.region.x,
            height: max_y - entry.region.y,
        });
        Some(entry)
    }

    /// The next monotonic touch stamp.
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Copy one RGBA8 image (`width * height * 4` bytes, row-major, straight
    /// alpha) into `region`, and record it as changed.
    ///
    /// `region` must come from [`Self::allocate`]; a mismatch between the
    /// region and the data is a programming error, so debug builds panic.
    pub fn write_region(&mut self, region: Region, rgba: &[u8]) {
        debug_assert_eq!(
            rgba.len(),
            (region.width as usize) * (region.height as usize) * 4,
            "atlas write data does not match its region"
        );
        debug_assert!(
            region.x + region.width <= self.width && region.y + region.height <= self.height
        );
        let row_bytes = (region.width as usize) * 4;
        for row in 0..region.height as usize {
            let destination =
                ((region.y as usize) + row) * (self.width as usize) * 4 + (region.x as usize) * 4;
            let source = row * row_bytes;
            self.pixels[destination..destination + row_bytes]
                .copy_from_slice(&rgba[source..source + row_bytes]);
        }
        self.dirty.push(region);
    }

    /// The regions written since the previous call; drains the record.
    pub fn take_updated_regions(&mut self) -> Vec<Region> {
        std::mem::take(&mut self.dirty)
    }

    /// Everything written since the previous call, pixels included, as owned
    /// data; `None` when nothing changed.
    ///
    /// The same work [`Self::take_updated_regions`] describes, with the pixels
    /// attached so the value can cross an artifact boundary - the draw lists a
    /// game's UI builds travel to the renderer without the atlas itself.
    pub fn take_update(&mut self) -> Option<AtlasUpdate> {
        let regions = self.take_updated_regions();
        if regions.is_empty() {
            return None;
        }
        let stride = self.width as usize * 4;
        let updates = regions
            .into_iter()
            .map(|region| {
                let row_bytes = region.width as usize * 4;
                let mut pixels = Vec::with_capacity(row_bytes * region.height as usize);
                for row in 0..region.height as usize {
                    let start = (region.y as usize + row) * stride + region.x as usize * 4;
                    pixels.extend_from_slice(&self.pixels[start..start + row_bytes]);
                }
                AtlasRegionUpdate { region, pixels }
            })
            .collect();
        Some(AtlasUpdate {
            width: self.width,
            height: self.height,
            regions: updates,
        })
    }

    /// The corner-to-corner texture coordinates of `region`, normalized to
    /// `0.0..=1.0`.
    pub fn uv_rect(&self, region: Region) -> (Vector2f, Vector2f) {
        let min = Vector2f::new(
            region.x as f32 / self.width as f32,
            region.y as f32 / self.height as f32,
        );
        let max = Vector2f::new(
            (region.x + region.width) as f32 / self.width as f32,
            (region.y + region.height) as f32 / self.height as f32,
        );
        (min, max)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_pack_left_to_right_and_wrap_to_a_new_shelf() {
        let mut atlas = Atlas::new(16, 16);
        let first = atlas.allocate(4, 4).unwrap();
        let second = atlas.allocate(4, 4).unwrap();
        let third = atlas.allocate(4, 4).unwrap();
        let fourth = atlas.allocate(4, 4).unwrap();
        assert_eq!(
            first,
            Region {
                x: 0,
                y: 0,
                width: 4,
                height: 4
            }
        );
        assert_eq!(
            second,
            Region {
                x: 5,
                y: 0,
                width: 4,
                height: 4
            }
        );
        assert_eq!(
            third,
            Region {
                x: 10,
                y: 0,
                width: 4,
                height: 4
            }
        );
        // 15 + 4 does not fit the first shelf, so this entry opens the next
        // one: shelf height (4 + padding) plus the inter-shelf padding row.
        assert_eq!(
            fourth,
            Region {
                x: 0,
                y: 6,
                width: 4,
                height: 4
            }
        );
    }

    #[test]
    fn a_full_atlas_refuses_and_an_oversized_entry_is_refused() {
        let mut atlas = Atlas::new(8, 8);
        assert!(atlas.allocate(16, 4).is_none());
        assert!(atlas.allocate(4, 16).is_none());
        assert!(atlas.allocate(0, 4).is_none());
        assert!(atlas.allocate(8, 8).is_some());
        // The first shelf filled the whole image.
        assert!(atlas.allocate(1, 1).is_none());
    }

    /// Fill `region` with one colour, so a test can watch it survive.
    fn write_marker(atlas: &mut Atlas, region: Region, value: u8) {
        let rgba = vec![value; region.width as usize * region.height as usize * 4];
        atlas.write_region(region, &rgba);
    }

    #[test]
    fn reserving_grows_the_atlas_in_pages_and_keeps_the_pixels() {
        let mut atlas = Atlas::with_limit(8, 8, 16);
        let first = atlas.reserve(1, 4, 4, 0).unwrap();
        write_marker(&mut atlas, first, 7);
        atlas.take_updated_regions();

        // A second 4x4 entry no longer fits the second shelf of an 8x8
        // image, so the atlas doubles once, to its cap, and places it.
        let second = atlas.reserve(2, 4, 4, 0).unwrap();
        assert_eq!((atlas.width(), atlas.height()), (16, 16));
        assert_eq!(
            second,
            Region {
                x: 0,
                y: 6,
                width: 4,
                height: 4
            }
        );
        assert_eq!(atlas.region_of(1), Some(first));
        // The first entry's pixels came through the growth untouched.
        let offset = (first.y as usize * atlas.width() as usize + first.x as usize) * 4;
        assert_eq!(atlas.pixels()[offset], 7);
        // The whole image counts as changed once, so a backend recreates
        // its texture from the one region.
        assert_eq!(
            atlas.take_updated_regions(),
            [Region {
                x: 0,
                y: 0,
                width: 16,
                height: 16
            }]
        );
    }

    #[test]
    fn headroom_growth_happens_between_frames_before_the_shelves_fill() {
        let mut atlas = Atlas::with_limit(16, 16, 64);
        assert!(!atlas.ensure_headroom(), "an empty atlas has room");
        for key in 1..=6 {
            atlas.reserve(key, 4, 4, 0).unwrap();
        }
        assert!(!atlas.ensure_headroom(), "half full is not worth a page");

        for key in 7..=9 {
            atlas.reserve(key, 4, 4, 0).unwrap();
        }
        assert!(atlas.ensure_headroom(), "past three quarters grows");
        assert_eq!(atlas.width(), 32);

        // At the cap the growth stops; the next frame evicts instead.
        let mut small = Atlas::with_limit(16, 16, 16);
        for key in 1..=9 {
            small.reserve(key, 4, 4, 0).unwrap();
        }
        assert!(!small.ensure_headroom(), "the cap stops headroom growth");
        assert_eq!(small.width(), 16);
    }

    #[test]
    fn a_full_atlas_at_its_cap_evicts_the_least_recently_used_entry() {
        // Nine 4x4 entries are what a 16x16 image holds with padding.
        let mut atlas = Atlas::with_limit(16, 16, 16);
        for key in 1..=9 {
            atlas.reserve(key, 4, 4, 0).unwrap();
        }
        assert_eq!(atlas.width(), 16, "the cap stops growth");

        // The second entry is the oldest one; touching the first keeps it.
        atlas.touch(1);
        atlas.reserve(10, 4, 4, 0).unwrap();
        assert_eq!(atlas.region_of(2), None, "the oldest entry went");
        assert!(atlas.region_of(1).is_some(), "a touched entry survives");
        assert!(atlas.region_of(10).is_some());
        assert_eq!(atlas.entry_count(), 9);
        // The newcomer took the evicted entry's slot: eviction neither
        // grows the image nor moves a surviving entry.
        assert_eq!(atlas.width(), 16);
    }

    #[test]
    fn an_entry_bigger_than_the_cap_is_refused() {
        let mut atlas = Atlas::with_limit(16, 16, 16);
        assert!(atlas.reserve(1, 32, 4, 0).is_none());
        assert!(atlas.reserve(1, 4, 32, 0).is_none());
        assert!(atlas.reserve(1, 0, 4, 0).is_none());
    }

    #[test]
    fn an_emptied_atlas_relays_its_shelves() {
        let mut atlas = Atlas::with_limit(16, 16, 16);
        for key in 1..=9 {
            atlas.reserve(key, 4, 4, 0).unwrap();
        }
        atlas.touch(9);
        // A wide entry no 4x4 slot descendant can ever fit: reserve evicts
        // every entry, the fragments fit nothing, and the shelves start
        // over on the empty image.
        let region = atlas.reserve(10, 12, 4, 0).unwrap();
        assert_eq!(
            region,
            Region {
                x: 0,
                y: 0,
                width: 12,
                height: 4
            }
        );
        assert_eq!(atlas.entry_count(), 1);
    }

    #[test]
    fn evicting_a_tag_frees_its_slots_for_reuse() {
        let mut atlas = Atlas::with_limit(16, 16, 16);
        for key in 1..=4 {
            atlas.reserve(key, 4, 4, 1).unwrap();
        }
        for key in 5..=8 {
            atlas.reserve(key, 4, 4, 2).unwrap();
        }
        assert_eq!(atlas.evict_tag(1), 4);
        assert_eq!(atlas.region_of(1), None);
        assert!(atlas.region_of(5).is_some(), "the other tag stays");

        // What the evicted scale left behind serves four new entries
        // without the image growing.
        for key in 9..=12 {
            atlas.reserve(key, 4, 4, 3).unwrap();
        }
        assert_eq!(atlas.width(), 16);
    }

    #[test]
    fn a_write_records_one_region_once() {
        let mut atlas = Atlas::new(8, 8);
        let region = atlas.allocate(2, 1).unwrap();
        let pixels = [10u8, 20, 30, 40, 50, 60, 70, 80];
        atlas.write_region(region, &pixels);
        assert_eq!(atlas.take_updated_regions(), [region]);
        assert!(atlas.take_updated_regions().is_empty());
        // The pixels landed where the region says.
        let offset = (region.y * atlas.width() + region.x) as usize * 4;
        assert_eq!(&atlas.pixels()[offset..offset + 8], &pixels);
    }

    #[test]
    fn texture_coordinates_normalize_the_region() {
        let atlas = Atlas::new(100, 50);
        let (min, max) = atlas.uv_rect(Region {
            x: 25,
            y: 10,
            width: 25,
            height: 10,
        });
        assert_eq!(min, Vector2f::new(0.25, 0.2));
        assert_eq!(max, Vector2f::new(0.5, 0.4));
    }
}

//! The texture mirror: egui's delta stream, kept as whole images a rebuilt
//! painter can upload from.
//!
//! # Responsibilities
//!
//! - [`TextureMirror`]: one image per egui texture, patched in place by the
//!   deltas each pass reports, and a revision per image.
//! - [`TextureMirror::apply`]: fold a pass's [`TexturesDelta`] into the
//!   mirror, remembering what changed and what was freed.
//! - [`TextureMirror::carry`]: turn what changed into the frame's
//!   [`UiTextureSet`], as whole images.
//!
//! # Design
//!
//! egui hands out whole images for new textures and *patches* for the ones it
//! keeps growing - the font atlas is patched for every new glyph. A painter
//! that was rebuilt mid-run cannot be brought up to date from a patch, so the
//! producer keeps the full image itself and carries it whenever the texture
//! changed (W-0028 D3). The mirror is that
//! copy: one `Vec<u8>` per texture, patched by the deltas, read out as the
//! frame's uploads.
//!
//! The bytes are straight-alpha sRGB RGBA8, because that is what
//! `pill_ui_frame::Color` and the painter's shader expect; egui's premultiplied
//! `Color32` pixels are unpremultiplied on the way in.

// Standard library
use std::collections::HashMap;
use std::sync::Arc;

// External crates
use egui::{ImageData, TextureId as EguiTextureId, TexturesDelta};
use pill_ui_frame::{TextureId, UiTextureFilter, UiTextureSet};

// Current crate
use crate::convert::external_id;

// =============================================================================
// TextureMirror
// =============================================================================

/// One texture's current image, as the painter would need it from nothing.
#[derive(Clone, Debug)]
struct MirroredTexture {
    width: u32,
    height: u32,
    filter: UiTextureFilter,
    revision: u64,
    /// Straight-alpha sRGB RGBA8, `width * height * 4` bytes.
    pixels: Vec<u8>,
}

/// Every texture egui handed this UI, kept whole.
#[derive(Default)]
pub struct TextureMirror {
    textures: HashMap<EguiTextureId, MirroredTexture>,
    /// The textures whose content changed since the last [`Self::carry`].
    dirty: Vec<EguiTextureId>,
    /// The textures egui freed since the last [`Self::carry`].
    freed: Vec<EguiTextureId>,
    /// The newest revision handed out; per texture, revisions increase by one
    /// on every change.
    next_revision: u64,
}

impl TextureMirror {
    /// An empty mirror.
    pub fn new() -> TextureMirror {
        TextureMirror::default()
    }

    /// How many textures it holds.
    pub fn len(&self) -> usize {
        self.textures.len()
    }

    /// Whether it holds no textures.
    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }

    /// The revision `texture` is at, when it is mirrored.
    pub fn revision(&self, texture: EguiTextureId) -> Option<u64> {
        self.textures.get(&texture).map(|entry| entry.revision)
    }

    /// Fold one pass's texture deltas into the mirror.
    ///
    /// A delta with no position replaces the whole image (a new texture, or a
    /// grown one); a delta with a position patches the image in place. A patch
    /// for a texture the mirror has never seen cannot be placed, so it is
    /// reported and dropped - egui always sends the whole image first.
    pub fn apply(&mut self, delta: &TexturesDelta) {
        for (texture_id, deltas) in &delta.set {
            for patch in deltas {
                let ImageData::Color(image) = &patch.image;
                let pixels = unpremultiplied_rgba8(&image.pixels);
                match patch.pos {
                    None => {
                        self.next_revision += 1;
                        self.textures.insert(
                            *texture_id,
                            MirroredTexture {
                                width: image.size[0] as u32,
                                height: image.size[1] as u32,
                                filter: filter_of(&patch.options),
                                revision: self.next_revision,
                                pixels,
                            },
                        );
                    }
                    Some([x, y]) => {
                        let Some(entry) = self.textures.get_mut(texture_id) else {
                            pill_core::warn!(
                                target: pill_core::telemetry::telemetry_target::UI,
                                "[ui] a texture patch arrived before its whole image"
                            );
                            continue;
                        };
                        let (patch_width, patch_height) =
                            (image.size[0] as u32, image.size[1] as u32);
                        if x as u32 + patch_width > entry.width
                            || y as u32 + patch_height > entry.height
                        {
                            pill_core::warn!(
                                target: pill_core::telemetry::telemetry_target::UI,
                                "[ui] a texture patch fell outside the mirrored image"
                            );
                            continue;
                        }
                        let stride = entry.width as usize * 4;
                        for row in 0..patch_height as usize {
                            let to = (y + row) * stride + x * 4;
                            let from = row * patch_width as usize * 4;
                            let end = from + patch_width as usize * 4;
                            entry.pixels[to..to + patch_width as usize * 4]
                                .copy_from_slice(&pixels[from..end]);
                        }
                        self.next_revision += 1;
                        entry.revision = self.next_revision;
                    }
                }
                if !self.dirty.contains(texture_id) {
                    self.dirty.push(*texture_id);
                }
            }
        }
        for texture_id in &delta.free {
            self.textures.remove(texture_id);
            self.dirty.retain(|id| id != texture_id);
            if !self.freed.contains(texture_id) {
                self.freed.push(*texture_id);
            }
        }
    }

    /// Append what changed to `set` as whole-image uploads, and what egui
    /// freed as frees; then forget both lists.
    pub fn carry(&mut self, set: &mut UiTextureSet) {
        for texture_id in self.dirty.drain(..) {
            let Some(entry) = self.textures.get(&texture_id) else {
                continue;
            };
            let pixels: Arc<[u8]> = Arc::from(entry.pixels.as_slice());
            set.push_full(
                TextureId(external_id(texture_id)),
                entry.width,
                entry.height,
                entry.revision,
                entry.filter,
                pixels,
            );
        }
        for texture_id in self.freed.drain(..) {
            set.push_free(TextureId(external_id(texture_id)));
        }
    }
}

/// egui's premultiplied sRGB pixels as straight-alpha RGBA8.
fn unpremultiplied_rgba8(pixels: &[egui::Color32]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for pixel in pixels {
        rgba.extend_from_slice(&pixel.to_srgba_unmultiplied());
    }
    rgba
}

/// The filter mode `options` asks for.
fn filter_of(options: &egui::TextureOptions) -> UiTextureFilter {
    if options.magnification == egui::TextureFilter::Nearest {
        UiTextureFilter::Nearest
    } else {
        UiTextureFilter::Linear
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::ImageDelta;
    use egui::{Color32, ColorImage};

    /// A whole-image delta of `side x side` opaque pixels of `color`.
    fn whole(side: usize, color: Color32) -> ImageDelta {
        ImageDelta::full(
            ColorImage {
                size: [side, side],
                source_size: egui::vec2(side as f32, side as f32),
                pixels: vec![color; side * side],
            },
            egui::TextureOptions::NEAREST,
        )
    }

    /// A patch delta of `side x side` at `pos`.
    fn patch(side: usize, pos: [usize; 2], color: Color32) -> ImageDelta {
        ImageDelta {
            image: ImageData::Color(Arc::new(ColorImage {
                size: [side, side],
                source_size: egui::vec2(side as f32, side as f32),
                pixels: vec![color; side * side],
            })),
            options: egui::TextureOptions::NEAREST,
            pos: Some(pos),
        }
    }

    /// A delta carrying one whole image for `texture`.
    fn whole_image(texture: EguiTextureId, side: usize, color: Color32) -> TexturesDelta {
        let mut delta = TexturesDelta::default();
        delta.push(texture, whole(side, color));
        delta
    }

    /// A delta carrying one patch for `texture`.
    fn patch_of(
        texture: EguiTextureId,
        side: usize,
        pos: [usize; 2],
        color: Color32,
    ) -> TexturesDelta {
        let mut delta = TexturesDelta::default();
        delta.push(texture, patch(side, pos, color));
        delta
    }

    #[test]
    fn a_whole_image_is_carried_whole() {
        let mut mirror = TextureMirror::new();
        let mut delta = whole_image(EguiTextureId::Managed(0), 2, Color32::RED);
        mirror.apply(&delta);
        delta.clear();
        let mut set = UiTextureSet::new();
        mirror.carry(&mut set);
        assert_eq!(set.textures.len(), 1);
        let texture = &set.textures[0];
        assert_eq!(
            texture.id,
            TextureId(external_id(EguiTextureId::Managed(0)))
        );
        assert_eq!((texture.width, texture.height), (2, 2));
        assert_eq!(texture.pixels.len(), 16);
        assert_eq!(texture.filter, UiTextureFilter::Nearest);

        // The same mirror, carried again with nothing changed: nothing.
        let mut empty = UiTextureSet::new();
        mirror.carry(&mut empty);
        assert!(empty.is_empty());
    }

    #[test]
    fn a_patch_updates_the_mirrored_image_in_place() {
        let mut mirror = TextureMirror::new();
        let mut delta = whole_image(EguiTextureId::Managed(0), 4, Color32::BLACK);
        mirror.apply(&delta);
        delta.clear();
        let mut first = UiTextureSet::new();
        mirror.carry(&mut first);
        let revision = first.textures[0].revision;

        let mut patch_delta = patch_of(EguiTextureId::Managed(0), 2, [1, 1], Color32::WHITE);
        mirror.apply(&patch_delta);
        patch_delta.clear();
        let mut second = UiTextureSet::new();
        mirror.carry(&mut second);
        let texture = &second.textures[0];
        assert!(texture.revision > revision, "the patch bumps the revision");
        // A patched texture still carries its whole image.
        let full = &texture.pixels;
        // The patch landed at (1, 1): row 1, columns 1 and 2 are white.
        let at = |x: usize, y: usize| full[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].to_vec();
        assert_eq!(at(1, 1), vec![255, 255, 255, 255]);
        assert_eq!(at(2, 1), vec![255, 255, 255, 255]);
        assert_eq!(at(0, 0), vec![0, 0, 0, 255]);
        assert_eq!(at(1, 0), vec![0, 0, 0, 255]);
    }

    #[test]
    fn a_free_is_carried_and_forgets_the_image() {
        let mut mirror = TextureMirror::new();
        let mut delta = whole_image(EguiTextureId::User(3), 1, Color32::WHITE);
        mirror.apply(&delta);
        delta.clear();
        let mut carried = UiTextureSet::new();
        mirror.carry(&mut carried);

        let mut freed = TexturesDelta::default();
        freed.free(EguiTextureId::User(3));
        mirror.apply(&freed);
        freed.clear();
        assert!(mirror.is_empty());
        let mut set = UiTextureSet::new();
        mirror.carry(&mut set);
        assert_eq!(set.frees, vec![TextureId(3)]);
        assert!(set.textures.is_empty());
    }

    #[test]
    fn a_patch_before_its_whole_image_is_dropped() {
        let mut mirror = TextureMirror::new();
        let mut delta = patch_of(EguiTextureId::Managed(1), 1, [0, 0], Color32::WHITE);
        mirror.apply(&delta);
        delta.clear();
        assert!(mirror.is_empty());
        let mut set = UiTextureSet::new();
        mirror.carry(&mut set);
        assert!(set.is_empty());
    }
}

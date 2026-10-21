//! The versioned texture set a UI frame carries, so a backend that was
//! rebuilt mid-run re-uploads what it lacks instead of drawing nothing.
//!
//! # Responsibilities
//!
//! - [`UiTextureSet`]: every texture a frame's draw lists sample, each with a
//!   monotonically increasing revision, plus the textures to free.
//! - [`UiTexture`]: one texture's size, sampling and this revision's uploads,
//!   as owned plain data.
//! - [`UiTextureUpload`]: the pixels of either the whole image or a patch of
//!   it.
//!
//! # Design
//!
//! The revision is the reload rule (W-0028 D3). A painter keeps
//! `texture -> revision` for what it has applied and uploads whenever the
//! carried revision is not the applied one, so a painter that was rebuilt -
//! and therefore remembers nothing - uploads everything the set carries. That
//! only works if the set can *rebuild* a texture, never just patch it: a
//! revision's uploads therefore include the whole image whenever the texture
//! changed, and a patch is carried solely so a live painter can skip the
//! bytes for regions it already has. The producer pays one full-image copy
//! per changed texture per frame; nothing is replayed from history, so no
//! back-channel and no cadence is involved.
//!
//! Every buffer is an [`Arc`] allocated by the producing side's own code and
//! handed over as plain data: a project reload may drop the producer while a
//! renderer still holds the frame, so the bytes must be owned by the data,
//! not borrowed from the producer's atlas.

// Standard library
use std::sync::Arc;

// Current crate
use crate::atlas::Region;
use crate::draw::TextureId;

// =============================================================================
// UiTextureFilter
// =============================================================================

/// How a backend samples a UI texture.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum UiTextureFilter {
    /// One texel per pixel, sharp edges: glyph atlases and pixel art.
    #[default]
    Nearest,
    /// Smooth in between: photo-like textures and scaled drawings.
    Linear,
}

// =============================================================================
// UiTextureUpload
// =============================================================================

/// The pixels of one upload: the whole image, or a patch of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiTextureUpload {
    /// Where the pixels belong; `None` replaces the whole image.
    pub region: Option<Region>,
    /// Tightly packed straight-alpha sRGB RGBA8 rows, sized by `region`.
    pub pixels: Arc<[u8]>,
}

impl UiTextureUpload {
    /// Whether the pixels are the whole image rather than a patch.
    pub fn is_full(&self) -> bool {
        self.region.is_none()
    }
}

// =============================================================================
// UiTexture
// =============================================================================

/// One texture a frame's draw lists sample, at one revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiTexture {
    /// Which draws sample it.
    pub id: TextureId,
    /// The full image's width in texels, patched or not.
    pub width: u32,
    /// The full image's height in texels, patched or not.
    pub height: u32,
    /// The content revision this entry carries; it increases whenever the
    /// texture's content changes.
    pub revision: u64,
    /// This revision's uploads, in whatever order the producer wrote them.
    pub uploads: Vec<UiTextureUpload>,
    /// How the texture is sampled.
    pub filter: UiTextureFilter,
}

impl UiTexture {
    /// The pixels that rebuild the texture from nothing: the whole image when
    /// this revision carries it.
    ///
    /// A painter that has never applied this texture needs exactly this; the
    /// patches beside it are for a painter that already has the rest.
    pub fn full_image(&self) -> Option<&[u8]> {
        self.uploads
            .iter()
            .find(|upload| upload.is_full())
            .map(|upload| upload.pixels.as_ref())
    }

    /// The uploads that are patches, in order.
    pub fn patches(&self) -> impl Iterator<Item = (Region, &[u8])> {
        self.uploads
            .iter()
            .filter_map(|upload| upload.region.map(|region| (region, upload.pixels.as_ref())))
    }
}

// =============================================================================
// UiTextureSet
// =============================================================================

/// A frame's textures: what to carry to the backend, and what to free.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiTextureSet {
    /// The textures this frame's draws use, one entry per texture.
    pub textures: Vec<UiTexture>,
    /// Textures the producer no longer uses; a backend drops them.
    pub frees: Vec<TextureId>,
}

impl UiTextureSet {
    /// An empty set.
    pub fn new() -> UiTextureSet {
        UiTextureSet::default()
    }

    /// Whether there is nothing to upload and nothing to free.
    pub fn is_empty(&self) -> bool {
        self.textures.is_empty() && self.frees.is_empty()
    }

    /// Forget the previous frame's set, keeping the allocations.
    pub fn clear(&mut self) {
        self.textures.clear();
        self.frees.clear();
    }

    /// The revision carried for `id`, if the set names it.
    pub fn revision_of(&self, id: TextureId) -> Option<u64> {
        self.textures
            .iter()
            .find(|texture| texture.id == id)
            .map(|texture| texture.revision)
    }

    /// Add the whole image of `id` at `revision`.
    ///
    /// `pixels` is straight-alpha sRGB RGBA8, `width * height * 4` bytes; a
    /// shorter or longer slice is a producer bug the backend would otherwise
    /// read past, so it is refused here rather than at the upload.
    pub fn push_full(
        &mut self,
        id: TextureId,
        width: u32,
        height: u32,
        revision: u64,
        filter: UiTextureFilter,
        pixels: Arc<[u8]>,
    ) {
        assert_eq!(
            pixels.len(),
            width as usize * height as usize * 4,
            "a whole-texture upload needs exactly width * height * 4 bytes"
        );
        self.textures.push(UiTexture {
            id,
            width,
            height,
            revision,
            uploads: vec![UiTextureUpload {
                region: None,
                pixels,
            }],
            filter,
        });
    }

    /// Add a patch of `id` at `revision`, for a texture the backend already
    /// holds.
    ///
    /// The patch's pixels are `region.width * region.height * 4` bytes.
    pub fn push_patch(
        &mut self,
        id: TextureId,
        width: u32,
        height: u32,
        revision: u64,
        filter: UiTextureFilter,
        region: Region,
        pixels: Arc<[u8]>,
    ) {
        assert_eq!(
            pixels.len(),
            region.texels() * 4,
            "a patch upload needs exactly width * height * 4 bytes"
        );
        self.textures.push(UiTexture {
            id,
            width,
            height,
            revision,
            uploads: vec![UiTextureUpload {
                region: Some(region),
                pixels,
            }],
            filter,
        });
    }

    /// Free `id` in every backend that applied the set.
    pub fn push_free(&mut self, id: TextureId) {
        self.frees.push(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole-image upload of `side x side` opaque white texels.
    fn full_white(id: TextureId, side: u32, revision: u64) -> UiTextureSet {
        let pixels = vec![255u8; side as usize * side as usize * 4];
        let mut set = UiTextureSet::new();
        set.push_full(
            id,
            side,
            side,
            revision,
            UiTextureFilter::Nearest,
            Arc::from(pixels),
        );
        set
    }

    #[test]
    fn a_full_upload_is_the_rebuild_source() {
        let set = full_white(TextureId::Atlas, 2, 7);
        let texture = &set.textures[0];
        assert_eq!(texture.full_image().map(<[u8]>::len), Some(16));
        assert_eq!(texture.patches().count(), 0);
        assert_eq!(set.revision_of(TextureId::Atlas), Some(7));
        assert_eq!(set.revision_of(TextureId::External(1)), None);
    }

    #[test]
    fn a_patch_is_not_a_rebuild_source() {
        let mut set = UiTextureSet::new();
        set.push_patch(
            TextureId::External(4),
            8,
            8,
            3,
            UiTextureFilter::Linear,
            Region {
                x: 2,
                y: 3,
                width: 2,
                height: 2,
            },
            Arc::from(vec![9u8; 16]),
        );
        let texture = &set.textures[0];
        assert!(texture.full_image().is_none());
        let patches: Vec<_> = texture.patches().collect();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0.texels(), 4);
        assert_eq!(patches[0].1.len(), 16);
    }

    #[test]
    fn a_wrongly_sized_upload_is_refused() {
        let outcome = std::panic::catch_unwind(|| {
            let mut set = UiTextureSet::new();
            set.push_full(
                TextureId::Atlas,
                2,
                2,
                1,
                UiTextureFilter::Nearest,
                Arc::from(vec![0u8; 8]),
            );
        });
        assert!(outcome.is_err(), "a 2x2 image is 16 bytes, not 8");
    }

    #[test]
    fn freeing_a_texture_empties_the_set_only_with_no_uploads() {
        let mut set = UiTextureSet::new();
        set.push_free(TextureId::External(2));
        assert!(!set.is_empty());
        assert!(set.textures.is_empty());
        set.clear();
        assert!(set.is_empty());
    }
}

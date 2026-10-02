//! Texture assets and their color interpretation.
//!
//! # Responsibilities
//!
//! - Carry the pixels the renderer uploads: RGBA bytes, their dimensions, and
//!   the name the asset is known by.
//! - Say how those pixels are meant to be read ([`TextureType`]): as colour,
//!   as a normal map, or as depth that only a shader declares.
//! - Build textures two ways, by decoding an image file ([`Texture::new`])
//!   and from a buffer the caller already holds ([`Texture::from_rgba`]).
//! - Load through a metadata file ([`ImportedAsset`]): the
//!   [`TextureImportSettings`] in `<image>.meta` say how to read the pixels.
//!
//! # Design
//!
//! A texture here is plain data with no GPU state: the renderer keeps the
//! texture, view and sampler beside it and rebuilds them when the asset's
//! version moves. The [`TextureType`] travels with the pixels for the same
//! reason the pixels travel at all - the bytes alone do not say how to read
//! them, so the type decides the format the renderer uploads as and how a
//! shader may sample the result.

// External crates
use pill_engine::{Asset, AssetLoadError, AssetLoadResult, AssetLoader, ImportedAsset};
use serde::{Deserialize, Serialize};

/// How a texture's pixels are meant to be read.
///
/// The type travels with the pixels because the bytes alone do not say how to
/// interpret them: it decides the format the pixels upload as and the binding
/// a shader receives for a texture slot, which is what keeps colour, normal
/// and depth data from being read the wrong way.
///
/// Serialized by variant name (`"Color"`, `"Normal"`) in a texture's metadata
/// file, so renaming a variant changes the on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextureType {
    /// A colour image: albedo, UI, or any texture sampled for its values.
    /// Uploaded as sRGB, so the hardware converts it to linear light before a
    /// shader reads it.
    Color,
    /// A normal map, whose bytes are directions rather than colours.
    /// Uploaded as linear data, so they reach a shader exactly as stored.
    Normal,
    /// A depth buffer, sampled by a pass that reconstructs position or measures
    /// distance.
    ///
    /// Not a kind of asset: no file decodes to one, and the renderer refuses a
    /// texture asset that claims to be one. It exists so a shader can say that a
    /// slot reads depth, which is what decides the binding wgpu will accept -
    /// a depth texture cannot be bound where a filterable colour texture is
    /// expected.
    Depth,
}

/// Decoded image data under a name, ready for the renderer to upload.
///
/// Plain data rather than a GPU resource: the renderer holds the matching
/// texture, view and sampler, and re-uploads them when the asset's version
/// moves, so a game edits the texture it holds instead of GPU state.
#[derive(Clone, Debug)]
pub struct Texture {
    /// Label used in logs, profiling and error messages.
    pub name: String,
    /// Pixels in RGBA order, four bytes per texel. The constructors check the
    /// length against the dimensions before a texture exists.
    pub rgba: Vec<u8>,
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// How a shader should read the pixels.
    pub texture_type: TextureType,
}

impl Texture {
    /// Builds a texture by decoding an image file.
    ///
    /// The loader supplies the bytes and the texture keeps its name, so a
    /// decode failure is reported against the asset being loaded rather than
    /// an anonymous buffer. Whatever format the file carries, the pixels are
    /// converted to RGBA8 before they are stored.
    ///
    /// # Errors
    ///
    /// Returns whatever the loader reports when the file is missing or
    /// unreadable, [`AssetLoadError::Decode`] when the bytes are not a
    /// decodable image, and (through [`Self::from_rgba`]) a decode error when
    /// the image's dimensions do not match its pixel buffer.
    pub fn new(
        name: impl Into<String>,
        texture_type: TextureType,
        loader: AssetLoader,
    ) -> AssetLoadResult<Self> {
        let bytes = loader.load()?;
        Self::decode(name.into(), texture_type, &bytes)
    }

    /// Decodes an encoded image (PNG or JPEG) into an RGBA8 texture.
    ///
    /// The one decode path, shared by [`Self::new`] and the metadata import.
    fn decode(name: String, texture_type: TextureType, bytes: &[u8]) -> AssetLoadResult<Self> {
        let image = image::load_from_memory(bytes).map_err(|error| AssetLoadError::Decode {
            label: name.clone(),
            detail: error.to_string(),
        })?;
        let image = image.to_rgba8();
        let (width, height) = image.dimensions();
        Self::from_rgba(name, texture_type, image.into_raw(), width, height)
    }

    /// Wraps an RGBA byte buffer as a texture, checked against the declared
    /// dimensions.
    ///
    /// This is the constructor for pixels that were made rather than loaded -
    /// a procedural image, a single-colour stand-in - where no file exists to
    /// decode.
    ///
    /// # Errors
    ///
    /// Returns [`AssetLoadError::Decode`] when the buffer's length is not
    /// `width * height * 4`. The GPU upload checks the same thing later, but it
    /// can only report the mismatch as an upload failure; catching it here
    /// names the texture while its own numbers are still at hand.
    pub fn from_rgba(
        name: impl Into<String>,
        texture_type: TextureType,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    ) -> AssetLoadResult<Self> {
        let name = name.into();
        let expected_bytes = width as usize * height as usize * 4;
        if rgba.len() != expected_bytes {
            return Err(AssetLoadError::Decode {
                label: name,
                detail: format!(
                    "declares {width}x{height} but carries {} bytes of RGBA data (expected {expected_bytes})",
                    rgba.len()
                ),
            });
        }
        Ok(Self {
            name,
            rgba,
            width,
            height,
            texture_type,
        })
    }
}

// Shared across binaries: the data module, the GPU module and every project
// compile their own copy of this crate, each with its own `TypeId`. The pinned
// name makes them one asset column (see `Asset::shared_name`); keep it
// verbatim when moving the type.
impl Asset for Texture {
    fn shared_name() -> Option<&'static str> {
        Some("pill_master_renderer::assets::Texture")
    }
}

/// How to read an image file as a texture: what its `.meta` file holds.
///
/// Every field is part of the metadata file format, so a rename breaks
/// existing files; `#[serde(default)]` lets a file written before a field
/// existed still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextureImportSettings {
    /// How a shader reads the pixels. `Depth` is refused on import: no image
    /// file holds depth.
    pub texture_type: TextureType,
}

impl Default for TextureImportSettings {
    fn default() -> Self {
        Self {
            texture_type: TextureType::Color,
        }
    }
}

// The metadata type name defaults to the shared name above, so every binary's
// copy of `Texture` reads the same files.
impl ImportedAsset for Texture {
    type ImportSettings = TextureImportSettings;
    // The formats this crate's `image` features decode.
    const SOURCE_EXTENSIONS: &'static [&'static str] = &["png", "jpg", "jpeg"];

    fn import(
        name: &str,
        source_bytes: &[u8],
        settings: &TextureImportSettings,
    ) -> AssetLoadResult<Self> {
        // The renderer refuses a depth texture asset anyway; refusing it here
        // names the file whose metadata asked for it.
        if settings.texture_type == TextureType::Depth {
            return Err(AssetLoadError::Decode {
                label: name.to_owned(),
                detail:
                    "`Depth` is not a texture asset type; an image file holds color or normal data"
                        .to_owned(),
            });
        }
        Self::decode(name.to_owned(), settings.texture_type, source_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 PNG, encoded in the test so no fixture file is needed.
    fn one_pixel_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn import_reads_the_texture_type_from_the_settings() {
        let settings = TextureImportSettings {
            texture_type: TextureType::Normal,
        };
        let texture = Texture::import("textures/n.png", &one_pixel_png(), &settings).unwrap();
        assert_eq!(texture.texture_type, TextureType::Normal);
        assert_eq!(texture.name, "textures/n.png");
        assert_eq!(texture.rgba, [10, 20, 30, 255]);
    }

    #[test]
    fn import_refuses_depth() {
        let settings = TextureImportSettings {
            texture_type: TextureType::Depth,
        };
        assert!(matches!(
            Texture::import("d.png", &one_pixel_png(), &settings),
            Err(AssetLoadError::Decode { .. })
        ));
    }

    /// The on-disk form: the variant name, and the default for a missing field.
    #[test]
    fn settings_serialize_by_variant_name() {
        let settings = TextureImportSettings {
            texture_type: TextureType::Normal,
        };
        assert_eq!(
            serde_json::to_string(&settings).unwrap(),
            r#"{"texture_type":"Normal"}"#
        );
        let defaulted: TextureImportSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaulted.texture_type, TextureType::Color);
    }
}

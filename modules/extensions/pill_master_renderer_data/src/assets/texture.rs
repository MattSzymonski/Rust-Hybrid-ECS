//! Texture assets and their color interpretation.
//!
//! # Responsibilities
//!
//! - Carry the texels the renderer uploads: their bytes, their dimensions, and
//!   the name the asset is known by.
//! - Say how those texels are meant to be read ([`TextureType`]): as colour,
//!   as a normal map, as depth that only a shader declares, or as an
//!   environment - an equirectangular panorama or a cubemap - for a skybox.
//! - Build textures by decoding an image file ([`Texture::new`]), from an RGBA8
//!   buffer the caller already holds ([`Texture::from_rgba`]), and from linear
//!   float texels ([`Texture::from_rgba_f32`]).
//! - Project an equirectangular panorama onto the six faces of a cubemap, with
//!   the same direction convention the skybox shaders sample by.
//! - Load through a metadata file ([`ImportedAsset`]): the
//!   [`TextureImportSettings`] in `<image>.meta` say how to read the texels.
//!
//! # Design
//!
//! A texture here is plain data with no GPU state: the renderer keeps the
//! texture, view and sampler beside it and rebuilds them when the asset's
//! version moves. The [`TextureType`] travels with the texels for the same
//! reason the texels travel at all - the bytes alone do not say how to read
//! them, so the type decides their layout, the format the renderer uploads as,
//! and how a shader may sample the result.
//!
//! Environment textures are high dynamic range: a sky's sun is many times
//! brighter than its clouds, and clamping it to one byte per channel would
//! flatten it. They are kept as half floats, the widest format every backend
//! can filter, and a cubemap is projected from a panorama once, at import,
//! rather than every frame.

// Standard library
use std::f32::consts::PI;

// External crates
use glam::Vec3;
use pill_engine::{Asset, AssetLoadError, AssetLoadResult, AssetLoader, ImportedAsset};
use serde::{Deserialize, Serialize};

/// How a texture's texels are meant to be read.
///
/// The type travels with the texels because the bytes alone do not say how to
/// interpret them: it decides their layout and the format they upload as, and
/// the binding a shader receives for a texture slot, which is what keeps
/// colour, normal, depth and environment data from being read the wrong way.
///
/// Serialized by variant name (`"Color"`, `"Normal"`, `"Equirect"`,
/// `"Cubemap"`) in a texture's metadata file, so renaming a variant changes the
/// on-disk format.
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
    /// An equirectangular panorama: longitude across, latitude down, the whole
    /// sphere of directions around a point in one 2:1 image.
    ///
    /// High dynamic range: stored and uploaded as linear half floats, so a
    /// Radiance `.hdr` sky keeps the range above 1.0 that its sun lives in. A
    /// shader samples it by turning a direction into a uv
    /// ([`equirect_uv`]).
    Equirect,
    /// A cubemap: six square faces around a point, in the order `+X`, `-X`,
    /// `+Y`, `-Y`, `+Z`, `-Z`, sampled by direction.
    ///
    /// Imported from an equirectangular panorama, projected onto the faces at
    /// import ([`project_equirect_to_cubemap`]); high dynamic range like
    /// [`Self::Equirect`]. A cubemap samples evenly in every direction, where a
    /// panorama crowds its texels at the poles.
    Cubemap,
}

impl TextureType {
    /// Bytes one texel takes: four 8-bit channels, or four half-float channels
    /// for an environment texture.
    pub fn bytes_per_texel(self) -> usize {
        match self {
            TextureType::Color | TextureType::Normal | TextureType::Depth => 4,
            TextureType::Equirect | TextureType::Cubemap => 8,
        }
    }

    /// Images the texture holds: six faces for a cubemap, one otherwise.
    pub fn layer_count(self) -> u32 {
        match self {
            TextureType::Cubemap => 6,
            _ => 1,
        }
    }

    /// Whether the texels are linear half floats rather than 8-bit channels.
    pub fn is_high_dynamic_range(self) -> bool {
        matches!(self, TextureType::Equirect | TextureType::Cubemap)
    }
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
    /// Texels in RGBA order, laid out as [`Self::texture_type`] says: four
    /// bytes per texel for colour and normal maps, four little-endian half
    /// floats for an environment, and for a cubemap the six faces one after
    /// another. The constructors check the length against the dimensions before
    /// a texture exists.
    pub rgba: Vec<u8>,
    /// Width in texels; for a cubemap, of each face.
    pub width: u32,
    /// Height in texels; for a cubemap, of each face.
    pub height: u32,
    /// How a shader should read the texels.
    pub texture_type: TextureType,
}

impl Texture {
    /// Builds a texture by decoding an image file.
    ///
    /// The loader supplies the bytes and the texture keeps its name, so a
    /// decode failure is reported against the asset being loaded rather than
    /// an anonymous buffer. Colour and normal maps are converted to RGBA8; an
    /// environment is converted to linear half floats, and a cubemap is
    /// projected from the image as a panorama.
    ///
    /// # Errors
    ///
    /// Returns whatever the loader reports when the file is missing or
    /// unreadable, [`AssetLoadError::Decode`] when the bytes are not a
    /// decodable image, and a decode error when the image's dimensions do not
    /// match its texels.
    pub fn new(
        name: impl Into<String>,
        texture_type: TextureType,
        loader: AssetLoader,
    ) -> AssetLoadResult<Self> {
        let bytes = loader.load()?;
        Self::decode(name.into(), texture_type, &bytes)
    }

    /// Decodes an encoded image (PNG, JPEG or Radiance HDR) into a texture of
    /// `texture_type`.
    ///
    /// The one decode path, shared by [`Self::new`] and the metadata import.
    fn decode(name: String, texture_type: TextureType, bytes: &[u8]) -> AssetLoadResult<Self> {
        let image = image::load_from_memory(bytes).map_err(|error| AssetLoadError::Decode {
            label: name.clone(),
            detail: error.to_string(),
        })?;
        if !texture_type.is_high_dynamic_range() {
            let image = image.to_rgba8();
            let (width, height) = image.dimensions();
            return Self::from_rgba(name, texture_type, image.into_raw(), width, height);
        }

        // An 8-bit image stores sRGB-encoded values, and an environment is read
        // as linear light: decoding one without converting would brighten every
        // midtone. A float image (Radiance HDR) is linear already.
        let is_float_source = matches!(
            image.color(),
            image::ColorType::Rgb32F | image::ColorType::Rgba32F
        );
        let image = image.to_rgba32f();
        let (width, height) = image.dimensions();
        let mut texels = image.into_raw();
        if !is_float_source {
            for texel in texels.chunks_exact_mut(4) {
                for channel in &mut texel[..3] {
                    *channel = srgb_to_linear(*channel);
                }
            }
        }
        Self::from_rgba_f32(name, texture_type, &texels, width, height)
    }

    /// Wraps a texel buffer as a texture, checked against the declared
    /// dimensions.
    ///
    /// This is the constructor for texels that were made rather than loaded -
    /// a procedural image, a single-colour stand-in - where no file exists to
    /// decode. The buffer is laid out as [`Self::rgba`] describes for
    /// `texture_type`.
    ///
    /// # Errors
    ///
    /// Returns [`AssetLoadError::Decode`] when the buffer's length is not what
    /// the dimensions and type call for, or when a cubemap's faces are not
    /// square. The GPU upload checks the length later, but it can only report a
    /// mismatch as an upload failure; catching it here names the texture while
    /// its own numbers are still at hand.
    pub fn from_rgba(
        name: impl Into<String>,
        texture_type: TextureType,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    ) -> AssetLoadResult<Self> {
        let name = name.into();
        let expected_bytes = expected_byte_count(texture_type, width, height);
        if rgba.len() != expected_bytes {
            return Err(AssetLoadError::Decode {
                label: name,
                detail: format!(
                    "declares {width}x{height} {texture_type:?} but carries {} bytes (expected {expected_bytes})",
                    rgba.len()
                ),
            });
        }
        if texture_type == TextureType::Cubemap && width != height {
            return Err(AssetLoadError::Decode {
                label: name,
                detail: format!("a cubemap's faces are square, but this one is {width}x{height}"),
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

    /// Builds an environment texture from linear RGBA float texels of an
    /// equirectangular panorama.
    ///
    /// An [`TextureType::Equirect`] texture keeps the panorama as it is; a
    /// [`TextureType::Cubemap`] is projected from it, with faces a quarter of the
    /// panorama's width - the size at which a face and the panorama hold the
    /// same detail around the horizon.
    ///
    /// # Errors
    ///
    /// Returns [`AssetLoadError::Decode`] when `texture_type` is not an
    /// environment, or when `rgba` does not hold `width * height` texels.
    pub fn from_rgba_f32(
        name: impl Into<String>,
        texture_type: TextureType,
        rgba: &[f32],
        width: u32,
        height: u32,
    ) -> AssetLoadResult<Self> {
        let name = name.into();
        if !texture_type.is_high_dynamic_range() {
            return Err(AssetLoadError::Decode {
                label: name,
                detail: format!("float texels make an Equirect or a Cubemap, not {texture_type:?}"),
            });
        }
        let expected_floats = width as usize * height as usize * 4;
        if rgba.len() != expected_floats {
            return Err(AssetLoadError::Decode {
                label: name,
                detail: format!(
                    "declares a {width}x{height} panorama but carries {} floats (expected {expected_floats})",
                    rgba.len()
                ),
            });
        }

        let (texels, width, height) = match texture_type {
            TextureType::Cubemap => {
                let face_size = (width / 4).max(1);
                let faces = project_equirect_to_cubemap(rgba, width, height, face_size);
                (faces, face_size, face_size)
            }
            _ => (rgba.to_vec(), width, height),
        };
        let bytes = texels
            .iter()
            .flat_map(|value| half::f16::from_f32(*value).to_le_bytes())
            .collect();
        Self::from_rgba(name, texture_type, bytes, width, height)
    }
}

/// The byte length a texture of this type and size holds.
fn expected_byte_count(texture_type: TextureType, width: u32, height: u32) -> usize {
    width as usize
        * height as usize
        * texture_type.bytes_per_texel()
        * texture_type.layer_count() as usize
}

/// Converts one sRGB-encoded channel in `[0, 1]` to linear light.
fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// The equirectangular uv a direction samples, in `[0, 1]`.
///
/// Looking down `-Z` is the middle of the panorama (`u = 0.5`), `+X` is three
/// quarters across, and straight up is the top row. The skybox shaders use the
/// same formula, so a cubemap projected here and the panorama it came from show
/// the same sky.
pub fn equirect_uv(direction: Vec3) -> (f32, f32) {
    let direction = direction.normalize_or(Vec3::NEG_Z);
    let u = 0.5 + direction.x.atan2(-direction.z) / (2.0 * PI);
    let v = 0.5 - direction.y.clamp(-1.0, 1.0).asin() / PI;
    (u, v)
}

/// The direction a texel of a cubemap face looks along.
///
/// `face` is the layer index (`+X`, `-X`, `+Y`, `-Y`, `+Z`, `-Z`), and `s` and
/// `t` run from -1 to 1 across the face, left to right and top to bottom. This
/// is the cube face table GPUs sample by, so a texel written here is the one a
/// shader reads for that direction.
pub fn cube_face_direction(face: usize, s: f32, t: f32) -> Vec3 {
    let direction = match face {
        0 => Vec3::new(1.0, -t, -s),
        1 => Vec3::new(-1.0, -t, s),
        2 => Vec3::new(s, 1.0, t),
        3 => Vec3::new(s, -1.0, -t),
        4 => Vec3::new(s, -t, 1.0),
        _ => Vec3::new(-s, -t, -1.0),
    };
    direction.normalize()
}

/// Projects an equirectangular panorama onto the six faces of a cubemap.
///
/// Takes linear RGBA float texels of a `width` x `height` panorama and returns
/// the faces, `face_size` square each, one after another in layer order. Each
/// face texel samples the panorama bilinearly along its own direction: wrapping
/// across the panorama's seam, clamped at its poles.
pub fn project_equirect_to_cubemap(
    panorama: &[f32],
    width: u32,
    height: u32,
    face_size: u32,
) -> Vec<f32> {
    let face_size = face_size.max(1);
    let mut faces = Vec::with_capacity(6 * face_size as usize * face_size as usize * 4);
    for face in 0..6 {
        for row in 0..face_size {
            for column in 0..face_size {
                // Texel centres, mapped onto -1..1 across the face.
                let s = 2.0 * (column as f32 + 0.5) / face_size as f32 - 1.0;
                let t = 2.0 * (row as f32 + 0.5) / face_size as f32 - 1.0;
                let (u, v) = equirect_uv(cube_face_direction(face, s, t));
                faces.extend_from_slice(&sample_bilinear(panorama, width, height, u, v));
            }
        }
    }
    faces
}

/// Reads a panorama at `(u, v)` with bilinear filtering: `u` wraps around the
/// seam, `v` clamps at the poles.
fn sample_bilinear(panorama: &[f32], width: u32, height: u32, u: f32, v: f32) -> [f32; 4] {
    let x = u * width as f32 - 0.5;
    let y = (v * height as f32 - 0.5).clamp(0.0, (height - 1) as f32);
    let x_floor = x.floor();
    let y_floor = y.floor();
    let x_blend = x - x_floor;
    let y_blend = y - y_floor;

    let column = |offset: i64| (x_floor as i64 + offset).rem_euclid(width as i64) as usize;
    let row = |offset: i64| (y_floor as i64 + offset).clamp(0, height as i64 - 1) as usize;
    let texel = |column: usize, row: usize| {
        let start = (row * width as usize + column) * 4;
        &panorama[start..start + 4]
    };

    let mut result = [0.0; 4];
    for (channel, value) in result.iter_mut().enumerate() {
        let top = texel(column(0), row(0))[channel] * (1.0 - x_blend)
            + texel(column(1), row(0))[channel] * x_blend;
        let bottom = texel(column(0), row(1))[channel] * (1.0 - x_blend)
            + texel(column(1), row(1))[channel] * x_blend;
        *value = top * (1.0 - y_blend) + bottom * y_blend;
    }
    result
}

/// The pinned shared name of [`Texture`]; see the comment on its `Asset` impl.
const TEXTURE_SHARED_NAME: &str = "pill_master_renderer::assets::Texture";

// Shared across binaries: the data module, the GPU module and every project
// compile their own copy of this crate, each with its own `TypeId`. The pinned
// name makes them one asset column (see `Asset::shared_name`); keep it
// verbatim when moving the type.
impl Asset for Texture {
    fn shared_name() -> Option<&'static str> {
        Some(TEXTURE_SHARED_NAME)
    }

    fn shared_identity() -> Option<u128> {
        // A `const`, so the name is hashed at compile time. The default hashes
        // it on every call, and every `AssetManager` lookup makes that call:
        // the renderer does it several times per drawn entity, every frame.
        const IDENTITY: u128 =
            pill_engine::component::shared_component_identity(TEXTURE_SHARED_NAME);
        Some(IDENTITY)
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
    /// How a shader reads the texels. `Depth` is refused on import: no image
    /// file holds depth. `Equirect` and `Cubemap` read the image as a
    /// panorama, typically a Radiance `.hdr` sky.
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
    const SOURCE_EXTENSIONS: &'static [&'static str] = &["png", "jpg", "jpeg", "hdr"];

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
                    "`Depth` is not a texture asset type; an image file holds color, normal or environment data"
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

    /// A panorama whose every texel stores the direction it looks along, so a
    /// projected face can be checked against the direction of its own texel.
    fn direction_panorama(width: u32, height: u32) -> Vec<f32> {
        let mut texels = Vec::new();
        for row in 0..height {
            for column in 0..width {
                let u = (column as f32 + 0.5) / width as f32;
                let v = (row as f32 + 0.5) / height as f32;
                // The inverse of `equirect_uv`.
                let longitude = (u - 0.5) * 2.0 * PI;
                let latitude = (0.5 - v) * PI;
                texels.extend_from_slice(&[
                    latitude.cos() * longitude.sin(),
                    latitude.sin(),
                    -latitude.cos() * longitude.cos(),
                    1.0,
                ]);
            }
        }
        texels
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
            texture_type: TextureType::Cubemap,
        };
        assert_eq!(
            serde_json::to_string(&settings).unwrap(),
            r#"{"texture_type":"Cubemap"}"#
        );
        let defaulted: TextureImportSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaulted.texture_type, TextureType::Color);
    }

    /// An 8-bit image read as an environment is linearized and stored as half
    /// floats: sRGB 255 is linear 1.0, and the alpha channel is left alone.
    #[test]
    fn an_eight_bit_image_imports_as_a_linear_equirect() {
        let settings = TextureImportSettings {
            texture_type: TextureType::Equirect,
        };
        let texture = Texture::import("sky.png", &one_pixel_png(), &settings).unwrap();

        assert_eq!(texture.rgba.len(), 8);
        let channel = |index: usize| {
            half::f16::from_le_bytes([texture.rgba[index * 2], texture.rgba[index * 2 + 1]])
                .to_f32()
        };
        assert!((channel(0) - srgb_to_linear(10.0 / 255.0)).abs() < 1.0e-3);
        assert!((channel(3) - 1.0).abs() < 1.0e-3);
    }

    /// A Radiance file keeps values above 1.0, which is why skies are HDR.
    #[test]
    fn a_radiance_file_keeps_its_range() {
        let mut bytes = Vec::new();
        image::codecs::hdr::HdrEncoder::new(&mut bytes)
            .encode(&[image::Rgb([8.0f32, 0.5, 0.25]); 2], 2, 1)
            .unwrap();
        let settings = TextureImportSettings {
            texture_type: TextureType::Equirect,
        };

        let texture = Texture::import("sky.hdr", &bytes, &settings).unwrap();

        let red = half::f16::from_le_bytes([texture.rgba[0], texture.rgba[1]]).to_f32();
        assert!((red - 8.0).abs() < 0.1, "red {red}");
    }

    /// A cubemap from a 2:1 panorama has six faces a quarter as wide, and
    /// every face texel holds the direction it looks along.
    #[test]
    fn a_projected_cubemap_looks_where_its_texels_point() {
        let (width, height) = (256, 128);
        let texture = Texture::from_rgba_f32(
            "sky",
            TextureType::Cubemap,
            &direction_panorama(width, height),
            width,
            height,
        )
        .unwrap();
        assert_eq!((texture.width, texture.height), (64, 64));
        assert_eq!(texture.rgba.len(), 6 * 64 * 64 * 8);

        let faces =
            project_equirect_to_cubemap(&direction_panorama(width, height), width, height, 8);
        for face in 0..6 {
            for row in 0..8 {
                for column in 0..8 {
                    let s = 2.0 * (column as f32 + 0.5) / 8.0 - 1.0;
                    let t = 2.0 * (row as f32 + 0.5) / 8.0 - 1.0;
                    let start = ((face * 8 + row) * 8 + column) * 4;
                    let stored = Vec3::new(faces[start], faces[start + 1], faces[start + 2]);
                    let expected = cube_face_direction(face, s, t);
                    assert!(
                        stored.normalize().dot(expected) > 0.99,
                        "face {face} texel ({column}, {row}): {stored} against {expected}"
                    );
                }
            }
        }
    }

    /// Where the panorama's landmarks are: forward (`-Z`) in the middle, up on
    /// the top row.
    #[test]
    fn the_equirect_convention_matches_the_shaders() {
        assert_eq!(equirect_uv(Vec3::NEG_Z), (0.5, 0.5));
        let (u, _) = equirect_uv(Vec3::X);
        assert!((u - 0.75).abs() < 1.0e-6);
        let (_, v) = equirect_uv(Vec3::Y);
        assert!(v.abs() < 1.0e-6);
    }

    #[test]
    fn a_cubemap_with_unsquare_faces_is_refused() {
        // Two by one texels, eight bytes each, six faces: the right length for
        // the declared size, so only the shape is wrong.
        let bytes = vec![0; 2 * 8 * 6];
        assert!(Texture::from_rgba("sky", TextureType::Cubemap, bytes, 2, 1).is_err());
    }
}

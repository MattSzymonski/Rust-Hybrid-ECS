//! Texture assets and their color interpretation.

use pill_engine::{Asset, AssetLoadError, AssetLoadResult, AssetLoader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextureType {
    Color,
    Normal,
}

#[derive(Clone, Debug)]
pub struct Texture {
    pub name: String,
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub texture_type: TextureType,
}

impl Texture {
    pub fn new(
        name: impl Into<String>,
        texture_type: TextureType,
        loader: AssetLoader,
    ) -> AssetLoadResult<Self> {
        let name = name.into();
        let bytes = loader.load()?;
        let image = image::load_from_memory(&bytes).map_err(|error| AssetLoadError::Decode {
            label: name.clone(),
            detail: error.to_string(),
        })?;
        let image = image.to_rgba8();
        let (width, height) = image.dimensions();
        Ok(Self::from_rgba(
            name,
            texture_type,
            image.into_raw(),
            width,
            height,
        ))
    }

    pub fn from_rgba(
        name: impl Into<String>,
        texture_type: TextureType,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            name: name.into(),
            rgba,
            width,
            height,
            texture_type,
        }
    }
}

impl Asset for Texture {}

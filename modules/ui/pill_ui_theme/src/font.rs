//! The `.ttf`/`.otf` font a theme layer names (W-0127 Stage 7).
//!
//! # Responsibilities
//!
//! - [`FontAsset`]: the file's bytes as an asset, so a font a theme names is
//!   imported, versioned and watched like every other asset.
//! - [`FontAsset::bytes`]: the copy a UI hands to egui's `set_fonts`.
//! - [`FONT_SHARED_NAME`]: the pinned asset identity across binaries.
//!
//! # Design
//!
//! The bytes say everything the asset needs, so the import settings are empty
//! and the document is the file. The type exists rather than a project reading
//! the file itself so that an edited font moves the store's content version:
//! that is what makes both UIs re-install their fonts without a restart. The
//! asset never borrows from the store - the bytes are owned - so nothing a
//! reloadable image owns outlives it.

// External crates
use serde::{Deserialize, Serialize};
use trait_type_map::impl_trait_accessible;

// Current crate
use pill_engine_core::{Asset, AssetLoadError, AssetLoadResult, ImportedAsset};

/// The pinned shared name of [`FontAsset`].
pub const FONT_SHARED_NAME: &str = "pill_ui_theme::FontAsset";

/// A font file a theme layer named, as its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontAsset {
    /// The file's bytes, as shipped.
    pub bytes: Vec<u8>,
}

impl Asset for FontAsset {
    fn shared_name() -> Option<&'static str> {
        Some(FONT_SHARED_NAME)
    }
}

// The trait object the registry stores; every asset type registers one.
impl_trait_accessible!(dyn Asset; FontAsset);

/// How a font file is read, as stored in its `.meta` file.
///
/// Empty: a font's bytes say everything about it. It exists so a font file
/// gets a metadata file, and with it a guid that survives a rename.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontImportSettings {}

impl ImportedAsset for FontAsset {
    type ImportSettings = FontImportSettings;
    const SOURCE_EXTENSIONS: &'static [&'static str] = &["ttf", "otf"];

    /// Take the file's bytes.
    ///
    /// An empty file is refused here rather than at `set_fonts`: epaint
    /// panics on empty font data, and a refusal names the file instead.
    ///
    /// # Errors
    ///
    /// [`AssetLoadError::Decode`] when the file holds no bytes.
    fn import(
        name: &str,
        source_bytes: &[u8],
        _settings: &FontImportSettings,
    ) -> AssetLoadResult<Self> {
        if source_bytes.is_empty() {
            return Err(AssetLoadError::Decode {
                label: name.to_owned(),
                detail: "the file is empty".to_owned(),
            });
        }
        Ok(Self {
            bytes: source_bytes.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_font_asset_is_the_files_bytes() {
        let font = FontAsset::import("fonts/a.ttf", b"\x00\x01\x00\x00ttf", &Default::default())
            .expect("the bytes are a font");
        assert_eq!(font.bytes, b"\x00\x01\x00\x00ttf");
        assert_eq!(FontAsset::shared_name(), Some(FONT_SHARED_NAME));
    }

    #[test]
    fn an_empty_file_is_refused_with_its_name() {
        let error = FontAsset::import("fonts/empty.ttf", b"", &Default::default())
            .expect_err("an empty font is refused");
        assert!(error.to_string().contains("fonts/empty.ttf"), "{error}");
    }
}

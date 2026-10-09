//! The `.css` asset type: a stylesheet as an ordinary project file.
//!
//! # Responsibilities
//!
//! - [`StylesheetAsset`]: the bytes of a `.css` file, imported through the
//!   world's import registry like any other asset ([`ImportedAsset`]), with
//!   metadata files, guids and rename-following for free.
//! - [`StylesheetAsset::parse`]: the bytes as a
//!   [`Stylesheet`](crate::css::Stylesheet); parsing stays lazy, so an
//!   imported-but-unused sheet costs nothing.
//!
//! # Design
//!
//! The asset is deliberately thin - path, bytes, parse on demand. The
//! importer claims the `css` extension the first time a consumer registers
//! the type (the editor shell does, Stage 13; the dev probe reads files by
//! path instead), so the engine's asset watcher hot-reloads stylesheets the
//! same way it reloads any imported file, and a reload is a new
//! [`StylesheetAsset`] whose parse gives a fresh cache.

// Standard library
use std::path::PathBuf;
use std::sync::Arc;

// External crates
use pill_engine::{Asset, AssetLoadError, AssetLoadResult, ImportedAsset};
use serde::{Deserialize, Serialize};

/// The bytes of one stylesheet file.
pub struct StylesheetAsset {
    /// The path the bytes were read from, for diagnostics and reloading.
    path: PathBuf,
    /// The file's text, exactly as it was on disk.
    source: Arc<str>,
}

impl Asset for StylesheetAsset {}

trait_type_map::impl_trait_accessible!(dyn Asset; StylesheetAsset);

impl std::fmt::Debug for StylesheetAsset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StylesheetAsset")
            .field("path", &self.path)
            .field("bytes", &self.source.len())
            .finish()
    }
}

impl StylesheetAsset {
    /// The stylesheet's source text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The path the stylesheet was imported from.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Parse the source into a [`Stylesheet`](crate::css::Stylesheet);
    /// refusals land in its reports.
    pub fn parse(&self) -> crate::css::Stylesheet {
        crate::css::Stylesheet::parse(&self.source)
    }
}

/// How a stylesheet file is imported, as stored in its `.meta` file.
///
/// Empty for now: a stylesheet's text says everything. It exists so `css`
/// files get metadata files, and with them a guid that survives a rename.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StylesheetImportSettings {}

impl ImportedAsset for StylesheetAsset {
    type ImportSettings = StylesheetImportSettings;
    const SOURCE_EXTENSIONS: &'static [&'static str] = &["css"];

    // The type name is an interface: files must keep matching if the type
    // moves.
    fn metadata_type_name() -> &'static str {
        "pill_ui::Stylesheet"
    }

    fn import(
        name: &str,
        source_bytes: &[u8],
        _settings: &StylesheetImportSettings,
    ) -> AssetLoadResult<Self> {
        let source = std::str::from_utf8(source_bytes).map_err(|error| AssetLoadError::Decode {
            label: name.to_owned(),
            detail: error.to_string(),
        })?;
        Ok(StylesheetAsset {
            path: PathBuf::from(name),
            source: Arc::from(source),
        })
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_satisfies_the_import_contract() {
        fn assert_asset<T: Asset>() {}
        assert_asset::<StylesheetAsset>();

        assert_eq!(StylesheetAsset::SOURCE_EXTENSIONS, &["css"]);
        assert_eq!(StylesheetAsset::metadata_type_name(), "pill_ui::Stylesheet");
    }

    #[test]
    fn importing_parses_a_stylesheet_lazily() {
        let asset = StylesheetAsset::import(
            "theme.css",
            b".panel { background: rgb(1, 2, 3); }",
            &StylesheetImportSettings::default(),
        )
        .expect("valid UTF-8");
        assert_eq!(asset.path().to_str(), Some("theme.css"));
        let sheet = asset.parse();
        assert!(sheet.reports().is_empty(), "{:?}", sheet.reports());
        assert_eq!(sheet.rule_count(), 1);
    }

    #[test]
    fn non_utf8_bytes_are_refused_by_name() {
        let error = StylesheetAsset::import("bad.css", &[0xff, 0xfe], &Default::default())
            .expect_err("not text");
        let message = error.to_string();
        assert!(message.contains("bad.css"), "{message}");
    }

    #[test]
    fn the_world_registers_the_css_extension() {
        use pill_engine::world::World;

        let mut world = World::new();
        world.register_imported_asset::<StylesheetAsset>();
        let registry = world
            .get_resource::<pill_engine::asset_import_registry::ImportRegistry>()
            .expect("the registry is a world resource");
        assert_eq!(
            registry.type_for_extension("css"),
            Some("pill_ui::Stylesheet")
        );
    }
}

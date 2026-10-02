//! A reference to an asset that survives being saved: [`AssetReference`].
//!
//! # Responsibilities
//!
//! - Name an asset by its guid in data written to disk, serialized as the
//!   guid's 32 hexadecimal digits (or `null` when unset).
//! - Resolve that guid to the live [`Handle`] when the data is loaded, and to
//!   [`Handle::INVALID`] when no asset has it, reporting each missing guid once.
//!
//! # Design
//!
//! A [`Handle`] is a slot index and a generation: exact and cheap at runtime,
//! meaningless in the next run, where the same asset lands in whatever slot it
//! gets. Hot-reload persistence depends on the handle's `(index, generation)`
//! serde form within one run, so that form stays as it is. Data written for a
//! later run - a scene, a material file - holds an `AssetReference` instead and
//! resolves it once loaded.
//!
//! An unresolved reference is a missing asset, not a broken program: it
//! resolves to [`Handle::INVALID`], which the renderer already draws with its
//! fallbacks. It is logged once per guid rather than once per frame.

// Standard library
use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::{Mutex, OnceLock};

// External crates
use trait_type_map::TraitAccessible;

// Current crate
use crate::asset::{Asset, AssetGuid, AssetManager, Handle};

/// A saved reference to an asset of type `T`, by guid.
///
/// Serializes as the guid in hex, or `null` when unset; see the module docs
/// for when to use it rather than a [`Handle`].
pub struct AssetReference<T: Asset> {
    guid: Option<AssetGuid>,
    _type: PhantomData<fn() -> T>,
}

impl<T: Asset> AssetReference<T> {
    /// A reference to the asset with `guid`.
    pub const fn new(guid: AssetGuid) -> Self {
        Self {
            guid: Some(guid),
            _type: PhantomData,
        }
    }

    /// A reference to no asset, which resolves to [`Handle::INVALID`] without
    /// a report: an intentionally empty slot rather than a missing asset.
    pub const fn unset() -> Self {
        Self {
            guid: None,
            _type: PhantomData,
        }
    }

    /// The referenced guid, or `None` when unset.
    pub fn guid(&self) -> Option<AssetGuid> {
        self.guid
    }

    /// Whether the reference names an asset.
    pub fn is_set(&self) -> bool {
        self.guid.is_some()
    }

    /// A reference to the asset `handle` addresses, by its guid; `None` when
    /// the handle is stale or its asset has no guid.
    pub fn from_handle(assets: &AssetManager, handle: Handle<T>) -> Option<Self>
    where
        T: TraitAccessible<dyn Asset>,
    {
        assets.guid_of(handle).map(Self::new)
    }

    /// The live handle of the referenced asset, or `None` when the reference is
    /// unset or no asset has its guid. Reports nothing.
    pub fn try_resolve(&self, assets: &AssetManager) -> Option<Handle<T>>
    where
        T: TraitAccessible<dyn Asset>,
    {
        assets.handle_by_guid::<T>(self.guid?)
    }

    /// The live handle of the referenced asset, or [`Handle::INVALID`].
    ///
    /// A set reference that resolves to nothing is logged as a warning the
    /// first time its guid is seen in this process; an unset one is not.
    pub fn resolve(&self, assets: &AssetManager) -> Handle<T>
    where
        T: TraitAccessible<dyn Asset>,
    {
        if let Some(handle) = self.try_resolve(assets) {
            return handle;
        }
        if let Some(guid) = self.guid {
            if first_report_of(guid) {
                pill_core::warn!(
                    "asset reference {guid} ({}) resolves to no loaded asset; using the invalid handle",
                    std::any::type_name::<T>()
                );
            }
        }
        Handle::INVALID
    }
}

/// Whether `guid` is being reported as unresolved for the first time in this
/// process; records it, so later calls return `false`.
fn first_report_of(guid: AssetGuid) -> bool {
    static REPORTED: OnceLock<Mutex<HashSet<AssetGuid>>> = OnceLock::new();
    REPORTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(guid)
}

// Written out by hand: a derive would require `T` to implement each trait,
// and a reference stores no `T`.

impl<T: Asset> Clone for AssetReference<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Asset> Copy for AssetReference<T> {}

impl<T: Asset> PartialEq for AssetReference<T> {
    fn eq(&self, other: &Self) -> bool {
        self.guid == other.guid
    }
}

impl<T: Asset> Eq for AssetReference<T> {}

impl<T: Asset> Default for AssetReference<T> {
    fn default() -> Self {
        Self::unset()
    }
}

impl<T: Asset> std::fmt::Debug for AssetReference<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.guid {
            Some(guid) => write!(formatter, "AssetReference({guid})"),
            None => formatter.write_str("AssetReference(unset)"),
        }
    }
}

impl<T: Asset> serde::Serialize for AssetReference<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.guid.serialize(serializer)
    }
}

impl<'de, T: Asset> serde::Deserialize<'de> for AssetReference<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            guid: Option::<AssetGuid>::deserialize(deserializer)?,
            _type: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trait_type_map::impl_trait_accessible;

    #[derive(Debug, PartialEq)]
    struct Picture(&'static str);
    impl Asset for Picture {}
    impl_trait_accessible!(dyn Asset; Picture);

    const GUID: &str = "000000000000000000000000000000ab";

    #[test]
    fn a_reference_serializes_as_its_hex_guid_and_back() {
        let reference = AssetReference::<Picture>::new(AssetGuid::parse(GUID).unwrap());

        let json = serde_json::to_string(&reference).unwrap();
        assert_eq!(json, format!("\"{GUID}\""));
        let read: AssetReference<Picture> = serde_json::from_str(&json).unwrap();
        assert_eq!(read, reference);
    }

    #[test]
    fn an_unset_reference_serializes_as_null() {
        let json = serde_json::to_string(&AssetReference::<Picture>::unset()).unwrap();
        assert_eq!(json, "null");
        let read: AssetReference<Picture> = serde_json::from_str("null").unwrap();
        assert!(!read.is_set());
    }

    #[test]
    fn a_malformed_guid_is_a_deserialization_error() {
        assert!(serde_json::from_str::<AssetReference<Picture>>("\"not a guid\"").is_err());
    }

    #[test]
    fn a_reference_resolves_to_the_live_handle_of_its_guid() {
        let mut assets = AssetManager::new();
        let guid = AssetGuid::parse(GUID).unwrap();
        let handle = assets
            .add_named_with_guid("a.png", guid, Picture("a"))
            .unwrap();

        let reference = AssetReference::<Picture>::from_handle(&assets, handle).unwrap();
        assert_eq!(reference.guid(), Some(guid));
        assert_eq!(reference.resolve(&assets), handle);
        assert_eq!(assets.get(reference.resolve(&assets)), Some(&Picture("a")));
    }

    #[test]
    fn a_missing_or_unset_reference_resolves_to_the_invalid_handle() {
        let assets = AssetManager::new();
        let missing = AssetReference::<Picture>::new(
            AssetGuid::parse("0000000000000000000000000000beef").unwrap(),
        );

        assert_eq!(missing.resolve(&assets), Handle::INVALID);
        assert_eq!(
            AssetReference::<Picture>::unset().resolve(&assets),
            Handle::INVALID
        );
        assert_eq!(missing.try_resolve(&assets), None);
    }

    #[test]
    fn a_missing_guid_is_reported_once() {
        let guid = AssetGuid::parse("0000000000000000000000000000cafe").unwrap();
        assert!(first_report_of(guid));
        assert!(!first_report_of(guid));
        assert!(!first_report_of(guid));
    }
}

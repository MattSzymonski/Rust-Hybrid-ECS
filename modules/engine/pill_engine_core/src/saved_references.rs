//! The saved form of references: the shape a document written for a later run
//! stores instead of a [`crate::asset::Handle`] or an [`crate::entity::Entity`].
//!
//! # Responsibilities
//!
//! - [`SavedAssetReference`]: the guid that authorizes a reference, a
//!   non-authoritative path hint beside it, and the sub-asset key the format
//!   carries.
//! - [`saved_reference`]: one field value as the form a saved document holds -
//!   an asset's guid or an entity's stable id.
//! - [`restore_asset_reference`]: the guid back to a field value, loading the
//!   asset on demand and falling back to the path hint when the guid is
//!   unknown.
//! - [`SavedEntityLinks`]: entity references collected while a document is
//!   read, resolved once every entity of it exists.
//!
//! # Design
//!
//! A [`crate::asset::Handle`] is a slot index and a generation: exact within
//! one run, and meaningless in the next, where the same asset lands in whatever
//! slot it gets. Hot-reload persistence depends on that in-run form, so it keeps the
//! handle's own serde and never comes through here. Data meant for a later run
//! - a scene, a material, project data - holds the saved form instead and
//! resolves it when it is read.
//!
//! The rule is a pair of functions over [`FieldValue`] rather than a serde
//! context, which could not do the job: it has no [`AssetManager`] to resolve a
//! guid against, and a `Handle` cannot hold an unresolved guid because it is
//! `#[repr(C)]` and `Copy`. Assets therefore resolve on demand, and entities
//! once the whole document exists.
//!
//! An unresolvable reference is a missing asset, not a broken program: it is
//! reported once per guid and resolves to [`crate::asset::Handle::INVALID`],
//! which the renderer's caches answer with their defaults.

// Standard library
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

// External crates
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// Current crate
use crate::asset::{AssetGuid, AssetManager};
use crate::asset_database::AssetDatabase;
use crate::asset_import_registry::ImportRegistry;
use crate::asset_metadata::MetadataPolicy;
use crate::component_field::FieldValue;
use crate::entity::{Entity, EntityId};
use crate::world::World;

// =============================================================================
// The saved asset reference
// =============================================================================

/// A saved reference to one asset.
///
/// The guid is what identifies the asset; `path` is a hint kept beside it for
/// a reader whose database no longer knows the guid (a file copied into
/// another project keeps working). `sub_asset` is the key a reference to part
/// of a file names - written only when set, so a document that uses none is no
/// larger for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedAssetReference {
    /// The guid of the referenced asset.
    pub guid: AssetGuid,
    /// The asset's path when the writer knew it, as a non-authoritative hint.
    pub path: Option<String>,
    /// The sub-asset key inside the file; no loader resolves it.
    pub sub_asset: Option<String>,
}

impl SavedAssetReference {
    /// A reference to the asset with `guid`, with no hint and no sub-asset.
    pub const fn new(guid: AssetGuid) -> Self {
        Self {
            guid,
            path: None,
            sub_asset: None,
        }
    }

    /// The same reference carrying the path hint `path`.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// The same reference naming `key` inside the file.
    #[must_use]
    pub fn with_sub_asset(mut self, key: impl Into<String>) -> Self {
        self.sub_asset = Some(key.into());
        self
    }
}

/// The object form's fields, as they are written and read.
#[derive(Serialize, Deserialize)]
struct SavedAssetReferenceFields {
    /// The 32-hex guid.
    guid: AssetGuid,
    /// The path hint, when the writer knew one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// The sub-asset key, when one is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sub_asset: Option<String>,
}

impl Serialize for SavedAssetReference {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SavedAssetReferenceFields {
            guid: self.guid,
            path: self.path.clone(),
            sub_asset: self.sub_asset.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SavedAssetReference {
    /// Reads the object form and the bare hex string a file that carries no
    /// hint holds.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Either spelling, told apart by the value's shape.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            /// The current form.
            Object(SavedAssetReferenceFields),
            /// The guid alone.
            Bare(AssetGuid),
        }
        Ok(match Stored::deserialize(deserializer)? {
            Stored::Object(fields) => Self {
                guid: fields.guid,
                path: fields.path,
                sub_asset: fields.sub_asset,
            },
            Stored::Bare(guid) => Self::new(guid),
        })
    }
}

// =============================================================================
// One reference field's saved value
// =============================================================================

/// One reference field's value in a saved document.
///
/// An asset reference is an object (its guid, hint and sub-asset key); an
/// entity reference is the entity's stable id as 32 hex digits. The two cannot
/// be confused on the way back in: only the asset form is an object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SavedReference {
    /// A reference to an asset.
    Asset(SavedAssetReference),
    /// A reference to an entity, by its stable id.
    Entity(EntityId),
}

impl<'de> Deserialize<'de> for SavedReference {
    /// An object is an asset reference; a string is an entity's id. A bare
    /// string is never read as an asset here: this build always spells an asset
    /// reference as an object, and the guid-only string a `.material` file
    /// holds is read by [`AssetReference`](crate::AssetReference) itself.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ReferenceVisitor;
        impl<'de> serde::de::Visitor<'de> for ReferenceVisitor {
            type Value = SavedReference;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an asset reference object or an entity id")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                let reference = SavedAssetReference::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(SavedReference::Asset(reference))
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
                let id = EntityId::parse(text).ok_or_else(|| {
                    E::custom(format!("`{text}` is not an entity id (32 hex digits)"))
                })?;
                Ok(SavedReference::Entity(id))
            }
        }
        deserializer.deserialize_any(ReferenceVisitor)
    }
}

/// The saved form of one field value, or `None` for a value that references
/// nothing: an unset handle, the null entity, or a field that is not a
/// reference at all.
///
/// The world is what turns a stored handle into an [`EntityId`]; the asset
/// manager and its registry are what turn a handle into the guid and the path
/// an asset is known by. A handle whose asset has no guid cannot be saved, and
/// reads as nothing here rather than as a reference that would later resolve
/// to the wrong asset.
pub fn saved_reference(
    world: &World,
    assets: &AssetManager,
    registry: &ImportRegistry,
    value: &FieldValue,
) -> Option<SavedReference> {
    match value {
        FieldValue::Asset {
            asset_type,
            index,
            generation,
        } => {
            if (*index, *generation) == (u32::MAX, u32::MAX) {
                return None;
            }
            let type_name = asset_type.as_deref()?;
            let identity = registry.asset_identity(assets, type_name, *index, *generation)?;
            let guid = identity.guid?;
            let reference = match identity.name {
                Some(name) => SavedAssetReference::new(guid).with_path(name),
                None => SavedAssetReference::new(guid),
            };
            Some(SavedReference::Asset(reference))
        }
        FieldValue::Entity { id, generation } => {
            let entity = Entity::from_raw(*id, *generation);
            if entity.is_null() {
                return None;
            }
            world.entity_id(entity).map(SavedReference::Entity)
        }
        _ => None,
    }
}

// =============================================================================
// Resolving a saved asset back
// =============================================================================

/// Resolve `saved` to the field value an asset reference field holds, loading
/// the asset on demand.
///
/// The guid is tried first, through the project database. A guid nothing knows
/// falls back to the path hint, which keeps a reference working after its asset
/// was copied into another project that assigned it a different guid. That
/// fallback is reported once per guid, as is a reference that resolves to
/// nothing, which comes back invalid so one missing asset does not fail the
/// document that names it.
///
/// The caller writes the value: the manager this loads through and the world
/// the field lives in cannot be borrowed at the same time.
pub fn restore_asset_reference(
    assets: &mut AssetManager,
    database: &AssetDatabase,
    registry: &ImportRegistry,
    saved: &SavedAssetReference,
) -> FieldValue {
    let loaded =
        match registry.load_by_guid(assets, database, saved.guid, MetadataPolicy::ReadIfPresent) {
            Ok(outcome) => Some(outcome),
            Err(guid_error) => match saved.path.as_deref() {
                Some(path) => match registry.load_by_path(
                    assets,
                    Path::new(path),
                    MetadataPolicy::ReadIfPresent,
                ) {
                    Ok(outcome) => {
                        report_once(
                            saved.guid,
                            &format!("is unknown here; resolved by its path hint `{path}` instead"),
                        );
                        Some(outcome)
                    }
                    Err(path_error) => {
                        report_once(
                            saved.guid,
                            &format!(
                                "resolves to no asset ({guid_error}; the path hint `{path}`: \
                             {path_error}); using the invalid handle"
                            ),
                        );
                        None
                    }
                },
                None => {
                    report_once(
                        saved.guid,
                        &format!(
                            "resolves to no asset ({guid_error}) and carries no path hint; \
                         using the invalid handle"
                        ),
                    );
                    None
                }
            },
        };
    match loaded {
        Some(outcome) => FieldValue::Asset {
            asset_type: Some(outcome.type_name),
            index: outcome.index,
            generation: outcome.generation,
        },
        None => FieldValue::Asset {
            asset_type: None,
            index: u32::MAX,
            generation: u32::MAX,
        },
    }
}

/// Report `guid` as a reference problem, once per process.
fn report_once(guid: AssetGuid, reason: &str) {
    static REPORTED: OnceLock<Mutex<HashSet<AssetGuid>>> = OnceLock::new();
    let first = REPORTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(guid);
    if first {
        pill_core::warn!("saved asset reference {guid} {reason}");
    }
}

// =============================================================================
// Deferred entity references
// =============================================================================

/// One entity reference a document names, waiting for its entity to exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedEntityLink {
    /// The entity holding the field.
    pub entity: Entity,
    /// The component's registered name.
    pub component: String,
    /// The field's name.
    pub field: String,
    /// The id the document named.
    pub id: EntityId,
}

/// The entity references a document holds, resolved in one pass.
///
/// A document is read in two steps because a reference may name an entity that
/// comes later in it: every entity is created first, then the ids are resolved
/// against the world that now holds them all. A link whose id resolves to no
/// entity names something that was deleted before the document was written, so
/// it comes back unresolved rather than being silently cleared.
#[derive(Debug, Default, Clone)]
pub struct SavedEntityLinks {
    /// The links still waiting for their entity.
    links: Vec<SavedEntityLink>,
}

impl SavedEntityLinks {
    /// Record that `entity`'s `component.field` names the entity `id`.
    pub fn push(&mut self, entity: Entity, component: &str, field: &str, id: EntityId) {
        self.links.push(SavedEntityLink {
            entity,
            component: component.to_owned(),
            field: field.to_owned(),
            id,
        });
    }

    /// How many links are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.links.len()
    }

    /// Whether no link is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// Write every link into its field; returns how many were written and the
    /// links whose id named no entity.
    ///
    /// A field that cannot be written counts as unresolved and comes back in
    /// the second half, because the document named a reference the world could
    /// not take.
    pub fn resolve(self, world: &mut World) -> (usize, Vec<SavedEntityLink>) {
        let mut resolved = 0;
        let mut unresolved = Vec::new();
        for link in self.links {
            let Some(target) = world.entity_by_id(link.id) else {
                unresolved.push(link);
                continue;
            };
            let value = FieldValue::Entity {
                id: target.id(),
                generation: target.generation(),
            };
            match world.write_component_field(link.entity, &link.component, &link.field, value) {
                Ok(()) => resolved += 1,
                Err(_) => unresolved.push(link),
            }
        }
        (resolved, unresolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{Asset, AssetLoadResult, Handle};
    use crate::asset_database::AssetDatabase;
    use crate::asset_metadata::ImportedAsset;
    use crate::asset_store::{mount_directory, mounted_directory_test_lock};
    use crate::component_registry::ComponentFieldDescriptor;
    use std::borrow::Cow;
    use std::path::PathBuf;
    use trait_type_map::impl_trait_accessible;

    /// The test asset: a text file's contents.
    #[derive(Debug)]
    struct Text(String);
    impl Asset for Text {}
    impl_trait_accessible!(dyn Asset; Text);

    impl Text {
        /// The text the file held.
        fn text(&self) -> &str {
            &self.0
        }
    }

    impl ImportedAsset for Text {
        type ImportSettings = ();
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["txt"];

        fn metadata_type_name() -> &'static str {
            "pill_engine::saved_references::tests::Text"
        }

        fn import(_name: &str, bytes: &[u8], _settings: &()) -> AssetLoadResult<Self> {
            Ok(Self(String::from_utf8_lossy(bytes).into_owned()))
        }
    }

    /// A component holding one asset handle and one entity reference.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct Refs {
        asset: Handle<Text>,
        target: Entity,
    }
    impl crate::component::Component for Refs {}

    /// Its layout, tagged the way the derive tags the two fields.
    static REFS_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "asset",
            type_tag: "struct:Handle<Text>",
            offset: 0,
            size: std::mem::size_of::<Handle<Text>>(),
            align: 4,
            element_count: 0,
            decorators: Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "target",
            type_tag: "entity",
            offset: 8,
            size: std::mem::size_of::<Entity>(),
            align: 8,
            element_count: 0,
            decorators: Cow::Borrowed(&[]),
        },
    ];

    const NOTE_GUID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";

    /// A scratch `res` directory, mounted while the test holds the lock, and a
    /// world with the text type and the reference component registered.
    struct Scratch {
        root: PathBuf,
        world: World,
        _mounted: std::sync::MutexGuard<'static, ()>,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let mounted = mounted_directory_test_lock();
            let root = std::env::temp_dir().join(format!(
                "pill-saved-references-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            mount_directory(&root);
            let mut world = World::new();
            world.register_imported_asset::<Text>();
            world.register_component_with_layout::<Refs>(REFS_FIELDS);
            // The derive records this for a real component; the layout here is
            // written by hand, so the field's asset type is recorded by hand
            // too.
            world.record_asset_field_type(
                crate::component::ComponentId::of::<Refs>(),
                "asset",
                Text::metadata_type_name(),
            );
            Self {
                root,
                world,
                _mounted: mounted,
            }
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        /// A source at `relative` whose sidecar carries `guid`.
        fn text(&self, relative: &str, guid: &str) {
            self.write(relative, "hello");
            self.write(
                &format!("{relative}.meta"),
                &format!(
                    r#"{{"format_version": 1, "asset_type": "pill_engine::saved_references::tests::Text", "guid": "{guid}", "settings": null}}"#
                ),
            );
        }

        fn registry(&self) -> ImportRegistry {
            self.world.get_resource::<ImportRegistry>().unwrap().clone()
        }

        fn database(&self) -> AssetDatabase {
            AssetDatabase::from_directory(&self.root, &self.registry())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn refs_component() -> String {
        crate::component::ComponentRegistry::registered_name::<Refs>()
    }

    fn guid(text: &str) -> AssetGuid {
        AssetGuid::parse(text).expect("the test's guid is 32 hexadecimal digits")
    }

    /// A component holding a handle and an entity reference round-trips through
    /// the saved form into a fresh world, where it names the same asset and the
    /// same entity through new handles.
    #[test]
    fn references_round_trip_into_a_fresh_world() {
        // Step 1: the first world, with the asset loaded and the fields set
        // from code. The scratch directory is released before the second one
        // is mounted, because a mounted directory is a process-wide lock.
        let (saved_asset, saved_entity) = {
            let mut first = Scratch::new("round-trip-first");
            first.text("notes/a.txt", NOTE_GUID);
            let registry = first.registry();
            let database = first.database();
            let world = &mut first.world;
            let loaded = {
                let assets = world.get_resource_mut::<AssetManager>().unwrap();
                registry
                    .load_by_guid(
                        assets,
                        &database,
                        guid(NOTE_GUID),
                        MetadataPolicy::ReadIfPresent,
                    )
                    .unwrap()
            };
            {
                let assets = world.get_resource::<AssetManager>().unwrap();
                let text = assets
                    .get(Handle::<Text>::from_raw(loaded.index, loaded.generation))
                    .expect("the note loaded");
                assert_eq!(text.text(), "hello");
            }
            let target = world.create_entity().build().unwrap();
            let holder = world
                .create_entity()
                .with(Refs {
                    asset: Handle::from_raw(loaded.index, loaded.generation),
                    target,
                })
                .build()
                .unwrap();

            let assets = world.get_resource::<AssetManager>().unwrap();
            let asset_value = world
                .read_component_field(holder, &refs_component(), "asset")
                .unwrap();
            let entity_value = world
                .read_component_field(holder, &refs_component(), "target")
                .unwrap();
            let saved_asset = match saved_reference(world, assets, &registry, &asset_value) {
                Some(SavedReference::Asset(reference)) => reference,
                other => panic!("the handle saved as {other:?}"),
            };
            let saved_entity = match saved_reference(world, assets, &registry, &entity_value) {
                Some(SavedReference::Entity(id)) => id,
                other => panic!("the entity saved as {other:?}"),
            };
            assert_eq!(saved_asset.guid, guid(NOTE_GUID));
            assert_eq!(saved_asset.path.as_deref(), Some("notes/a.txt"));
            (saved_asset, saved_entity)
        };

        // Step 2: a fresh world with its own manager. The asset loads on
        // demand and the entity is created before the link is resolved.
        let mut second = Scratch::new("round-trip-second");
        second.text("notes/a.txt", NOTE_GUID);
        let registry = second.registry();
        let database = second.database();
        let world = &mut second.world;
        // The document carries the entity's stable id, so the loader builds the
        // entity under that id: that is what makes the link resolvable.
        let new_target = world.create_entity().with_id(saved_entity).build().unwrap();
        let new_holder = world
            .create_entity()
            .with(Refs {
                asset: Handle::INVALID,
                target: Entity::NULL,
            })
            .build()
            .unwrap();
        let restored = {
            let assets = world.get_resource_mut::<AssetManager>().unwrap();
            restore_asset_reference(assets, &database, &registry, &saved_asset)
        };
        world
            .write_component_field(new_holder, &refs_component(), "asset", restored)
            .unwrap();
        let mut links = SavedEntityLinks::default();
        links.push(new_holder, &refs_component(), "target", saved_entity);
        let (resolved, unresolved) = links.resolve(world);
        assert_eq!((resolved, unresolved.len()), (1, 0));

        let refreshed: Refs = *world.get_component::<Refs>(new_holder).unwrap();
        let assets = world.get_resource::<AssetManager>().unwrap();
        assert_eq!(assets.guid_of(refreshed.asset), Some(saved_asset.guid));
        assert_eq!(
            assets.get(refreshed.asset).map(Text::text),
            Some("hello"),
            "the reloaded handle addresses the note"
        );
        assert_eq!(world.entity_id(refreshed.target), Some(saved_entity));
        assert_eq!(refreshed.target, new_target);
    }

    /// A guid nothing knows falls back to the path hint, which loads the file
    /// under the guid its own metadata carries.
    #[test]
    fn an_unknown_guid_resolves_by_its_path_hint() {
        let mut scratch = Scratch::new("path-fallback");
        scratch.text("notes/moved.txt", "11111111111111111111111111111111");
        let registry = scratch.registry();
        let database = scratch.database();
        let world = &mut scratch.world;
        let holder = world
            .create_entity()
            .with(Refs {
                asset: Handle::INVALID,
                target: Entity::NULL,
            })
            .build()
            .unwrap();

        let stale = SavedAssetReference::new(guid("99999999999999999999999999999999"))
            .with_path("notes/moved.txt");
        let restored = {
            let assets = world.get_resource_mut::<AssetManager>().unwrap();
            restore_asset_reference(assets, &database, &registry, &stale)
        };
        world
            .write_component_field(holder, &refs_component(), "asset", restored)
            .unwrap();

        let refreshed: Refs = *world.get_component::<Refs>(holder).unwrap();
        let assets = world.get_resource::<AssetManager>().unwrap();
        assert_eq!(
            assets.guid_of(refreshed.asset),
            Some(guid("11111111111111111111111111111111")),
            "the file's own guid wins over the document's unknown one"
        );
    }

    /// A reference with neither a known guid nor a resolving path restores as
    /// the invalid handle instead of failing, and is reported once.
    #[test]
    fn a_reference_to_nothing_restores_as_the_invalid_handle() {
        let mut scratch = Scratch::new("missing");
        let registry = scratch.registry();
        let database = scratch.database();
        let world = &mut scratch.world;
        let missing = SavedAssetReference::new(guid("88888888888888888888888888888888"));
        let restored = {
            let assets = world.get_resource_mut::<AssetManager>().unwrap();
            restore_asset_reference(assets, &database, &registry, &missing)
        };
        assert_eq!(
            restored,
            FieldValue::Asset {
                asset_type: None,
                index: u32::MAX,
                generation: u32::MAX,
            }
        );
    }

    /// The stored forms: the object with a hint, the object without one, and
    /// the bare hex an older file holds.
    #[test]
    fn the_saved_asset_reference_reads_both_spellings() {
        let with_hint = SavedAssetReference::new(guid(NOTE_GUID)).with_path("notes/a.txt");
        let json = serde_json::to_string(&with_hint).unwrap();
        assert_eq!(
            json,
            format!(r#"{{"guid":"{NOTE_GUID}","path":"notes/a.txt"}}"#)
        );
        assert_eq!(
            serde_json::from_str::<SavedAssetReference>(&json).unwrap(),
            with_hint
        );

        let bare = serde_json::to_string(&SavedAssetReference::new(guid(NOTE_GUID))).unwrap();
        assert_eq!(bare, format!(r#"{{"guid":"{NOTE_GUID}"}}"#));
        assert_eq!(
            serde_json::from_str::<SavedAssetReference>(&format!("\"{NOTE_GUID}\"")).unwrap(),
            SavedAssetReference::new(guid(NOTE_GUID))
        );
    }

    /// A saved entity reference is the stable id, and reads back as one.
    #[test]
    fn a_saved_entity_reference_is_its_stable_id() {
        let id = EntityId::from_name("hero");
        let saved = SavedReference::Entity(id);
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(
            serde_json::from_str::<SavedReference>(&json).unwrap(),
            saved
        );
    }
}

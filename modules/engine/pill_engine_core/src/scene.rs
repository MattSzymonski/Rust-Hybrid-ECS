//! A scene: the entities of a world, their persistable components and their
//! references, written as one standalone asset in `res`.
//!
//! # Responsibilities
//!
//! - [`SceneDocument`]: what a `.scene` file holds - one entry per entity, its
//!   stable [`EntityId`], and its components by registered type name, each a
//!   map of field name to the value the field layer reads.
//! - [`save_scene`]: a world's entities as that document, with every reference
//!   in the saved form.
//! - [`load_scene`]: the document back into a world - entities created under
//!   the ids it carries, components re-attached by name, asset references
//!   loaded on demand and entity references resolved once every entity exists.
//! - [`Scene`]: the asset type itself, so a scene can be named by a handle and
//!   loaded by its `.scene` extension.
//!
//! # Design
//!
//! The document is addressed by the field layer's names (Stage 2), not by the
//! component types' own serde: a handle field then saves as its guid and an
//! entity field as its stable id, which is the whole point of the scene
//! existing (Stages 5 and 7). A component whose own serde writes a handle as a
//! slot pair cannot be written straight into a file meant for a later run, so
//! the field layer - which knows what a reference *is* - is the writer.
//!
//! A field the field layer can only summarise (a `vec:` container) is not
//! written; [`SceneSave::skipped_fields`] names each one, so a lossy save says
//! so rather than passing for a complete one. A field it can locate but not
//! interpret (a `struct:` with no known layout) is written as its raw bytes,
//! because a captured opaque payload writes back verbatim into the same
//! layout.
//!
//! Loading is three passes: the components a reference lives in must exist
//! before the field is written, and an entity reference may name an entity
//! that comes later in the file. So: create every entity, attach every
//! component, then write every field.

// Standard library
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

// External crates
use serde::{Deserialize, Serialize};
use trait_type_map::impl_trait_accessible;

// Current crate
use crate::asset::{Asset, AssetLoadResult, AssetManager};
use crate::asset_database::AssetDatabase;
use crate::asset_import_registry::ImportRegistry;
use crate::asset_standalone::StandaloneAsset;
use crate::commands::ByteComponentAdder;
use crate::component_field::{is_handle_tag, FieldValue};
use crate::engine::Engine;
use crate::entity::{Entity, EntityId};
use crate::saved_references::{
    restore_asset_reference, saved_reference, SavedAssetReference, SavedEntityLinks, SavedReference,
};
use crate::world::World;

// =============================================================================
// The document
// =============================================================================

/// One entity in a scene document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneEntity {
    /// The entity's stable id; a reference to it in the same document names
    /// this id.
    pub id: EntityId,
    /// Its components by registered type name, each a map of field name to the
    /// value the field layer reads.
    #[serde(default)]
    pub components: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

/// The body of a `.scene` file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneDocument {
    /// Every entity the scene holds.
    pub entities: Vec<SceneEntity>,
}

/// A scene asset: the document it was loaded from, so code and other data can
/// name it with a `Handle<Scene>`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scene {
    /// The document the file held.
    document: SceneDocument,
}

impl Scene {
    /// The document this scene was loaded from.
    #[must_use]
    pub fn document(&self) -> &SceneDocument {
        &self.document
    }
}

/// The pinned shared name of [`Scene`]: the key the import registry files the
/// type under, and the type a `Handle<Scene>` field records.
const SCENE_SHARED_NAME: &str = "pill_engine::scene::Scene";

// Shared across binaries: a project, the host and every extension compile their
// own copy of the engine, each with its own `TypeId`. The pinned name makes
// them one asset column; keep it verbatim when moving the type.
impl Asset for Scene {
    fn shared_name() -> Option<&'static str> {
        Some(SCENE_SHARED_NAME)
    }
}

impl_trait_accessible!(dyn Asset; Scene);

impl StandaloneAsset for Scene {
    type Document = SceneDocument;

    const FILE_EXTENSION: &'static str = "scene";

    fn metadata_type_name() -> &'static str {
        SCENE_SHARED_NAME
    }

    fn from_document(
        _name: &str,
        document: SceneDocument,
        _assets: &AssetManager,
    ) -> AssetLoadResult<Self> {
        Ok(Self { document })
    }
}

// =============================================================================
// Saving
// =============================================================================

/// What [`save_scene`] wrote, and what it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SceneSave {
    /// One entry per field the field layer could not write, by
    /// `<entity>: <component>.<field>`.
    pub skipped_fields: Vec<String>,
    /// How many components were written.
    pub components: usize,
}

/// Write a world's entities into a scene document.
///
/// `entities` names the entities to write; `None` writes every live entity.
/// A component is written when the field layer can read it, which is the same
/// condition that lets it be written back. Persistability is deliberately not
/// the gate: that is a serde registration, and a component holding an `Entity`
/// - the very thing a scene exists to carry - cannot have one.
pub fn save_scene(
    world: &World,
    assets: &AssetManager,
    registry: &ImportRegistry,
    entities: Option<&[Entity]>,
) -> (SceneDocument, SceneSave) {
    let entities: Vec<Entity> = match entities {
        Some(chosen) => chosen.to_vec(),
        None => world
            .entity_rows()
            .into_iter()
            .map(|row| row.entity)
            .collect(),
    };
    let mut document = SceneDocument::default();
    let mut report = SceneSave::default();
    for entity in entities {
        let Some(id) = world.entity_id(entity) else {
            continue;
        };
        let mut components = BTreeMap::new();
        for name in world.entity_component_names(entity).unwrap_or_default() {
            let Some(component_id) = world.resolve_entity_component_id(entity, &name) else {
                continue;
            };
            let Some(layout) = world.component_field_layout(component_id) else {
                continue;
            };
            let values = match world.read_component_fields(entity, &name) {
                Ok(values) => values,
                Err(_) => continue,
            };
            let mut fields = BTreeMap::new();
            for (descriptor, value) in layout.iter().zip(values.iter()) {
                match saved_field(world, assets, registry, value) {
                    Some(encoded) => {
                        fields.insert(descriptor.name.to_string(), encoded);
                    }
                    None => report
                        .skipped_fields
                        .push(format!("{id}: {name}.{}", descriptor.name)),
                }
            }
            report.components += 1;
            components.insert(name, fields);
        }
        document.entities.push(SceneEntity { id, components });
    }
    (document, report)
}

/// One field value as a scene document holds it, or `None` for a value the
/// field layer can only summarise (a heap container) or a float JSON has no
/// number for.
///
/// `Text` is `None` for the same reason `List` is: a heap-backed field is not
/// writable through the field layer, so a value written here could never be
/// restored - and a document that carries one lies about what a load will do.
fn saved_field(
    world: &World,
    assets: &AssetManager,
    registry: &ImportRegistry,
    value: &FieldValue,
) -> Option<serde_json::Value> {
    /// A float JSON can represent; `None` for NaN and the infinities.
    fn number(value: f64) -> Option<serde_json::Value> {
        serde_json::Number::from_f64(value).map(serde_json::Value::Number)
    }
    match value {
        FieldValue::F32(value) => number(f64::from(*value)),
        FieldValue::F64(value) => number(*value),
        FieldValue::I8(value) => Some(serde_json::json!(value)),
        FieldValue::I16(value) => Some(serde_json::json!(value)),
        FieldValue::I32(value) => Some(serde_json::json!(value)),
        FieldValue::I64(value) => Some(serde_json::json!(value)),
        FieldValue::U8(value) => Some(serde_json::json!(value)),
        FieldValue::U16(value) => Some(serde_json::json!(value)),
        FieldValue::U32(value) => Some(serde_json::json!(value)),
        FieldValue::U64(value) => Some(serde_json::json!(value)),
        FieldValue::Usize(value) => Some(serde_json::json!(value)),
        FieldValue::Isize(value) => Some(serde_json::json!(value)),
        FieldValue::Bool(value) => Some(serde_json::json!(value)),
        FieldValue::Array(elements) => {
            let mut encoded = Vec::with_capacity(elements.len());
            for element in elements {
                encoded.push(saved_field(world, assets, registry, element)?);
            }
            Some(serde_json::Value::Array(encoded))
        }
        FieldValue::Opaque { type_tag, bytes } => Some(serde_json::json!({
            "opaque": hex_of(bytes),
            "tag": type_tag,
        })),
        FieldValue::Text(_) | FieldValue::List { .. } => None,
        FieldValue::Asset { .. } | FieldValue::Entity { .. } => {
            match saved_reference(world, assets, registry, value) {
                Some(SavedReference::Asset(reference)) => serde_json::to_value(reference).ok(),
                Some(SavedReference::Entity(id)) => Some(serde_json::json!(id.to_string())),
                // An unset reference writes as `null`, which is how a cleared
                // field reads back; a set one the writer cannot name (an asset
                // without a guid) is a loss, and says so.
                None if !is_set_reference(value) => Some(serde_json::Value::Null),
                None => None,
            }
        }
    }
}

/// Whether `value` names something, for the two reference variants.
fn is_set_reference(value: &FieldValue) -> bool {
    match value {
        FieldValue::Asset {
            index, generation, ..
        } => (*index, *generation) != (u32::MAX, u32::MAX),
        FieldValue::Entity { id, generation } => !Entity::from_raw(*id, *generation).is_null(),
        _ => false,
    }
}

/// `bytes` as lowercase hex, the form an opaque payload is written in.
fn hex_of(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into the string cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The bytes `text` spells as hex, or `None` for anything else.
fn bytes_of(text: &str) -> Option<Vec<u8>> {
    let digits = text.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks_exact(2) {
        let pair = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(bytes)
}

// =============================================================================
// Loading
// =============================================================================

/// What [`load_scene`] made of a document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SceneLoad {
    /// Entities created.
    pub entities: usize,
    /// Components attached.
    pub components: usize,
    /// Component type names the world does not know, or cannot attach.
    pub unknown_components: Vec<String>,
    /// References naming an entity the document did not create.
    pub unresolved_references: usize,
    /// Fields the document named that could not be written, by
    /// `<entity>: <component>.<field>`.
    pub skipped_fields: Vec<String>,
}

/// Instantiate a scene document into a world.
///
/// The entities are created under the ids the document carries, so a reference
/// between them resolves. A component the world does not know is reported and
/// skipped; so is a field the layout no longer has or refuses.
pub fn load_scene(
    engine: &mut Engine,
    database: &AssetDatabase,
    document: &SceneDocument,
) -> SceneLoad {
    let mut report = SceneLoad::default();
    let registry = engine
        .world()
        .get_resource::<ImportRegistry>()
        .cloned()
        .unwrap_or_default();
    let mut created: Vec<(Entity, &SceneEntity)> = Vec::with_capacity(document.entities.len());
    let mut seen = HashSet::with_capacity(document.entities.len());
    for scene_entity in &document.entities {
        if !seen.insert(scene_entity.id) {
            report
                .skipped_fields
                .push(format!("{}: the id repeats in this scene", scene_entity.id));
            continue;
        }
        match engine
            .world_mut()
            .create_entity()
            .with_id(scene_entity.id)
            .build()
        {
            Ok(entity) => {
                report.entities += 1;
                created.push((entity, scene_entity));
            }
            Err(_) => report
                .skipped_fields
                .push(format!("{}: the id is already taken", scene_entity.id)),
        }
    }

    // Step 1: attach every component, so its fields can be written into it.
    let mut fresh: HashSet<(Entity, String)> = HashSet::new();
    for (entity, scene_entity) in &created {
        for name in scene_entity.components.keys() {
            match attach_component(engine, *entity, name) {
                Ok(attached) => {
                    report.components += 1;
                    if attached {
                        fresh.insert((*entity, name.clone()));
                    }
                }
                Err(()) => report.unknown_components.push(name.clone()),
            }
        }
    }

    // Step 2: write every field; entity references wait for their entity.
    let mut links = SavedEntityLinks::default();
    for (entity, scene_entity) in &created {
        for (name, fields) in &scene_entity.components {
            // The tags are copied out first: the field loop below writes
            // through `engine.world_mut()`, which cannot run while the layout
            // borrows the world.
            let tags: BTreeMap<String, &'static str> = {
                let world = engine.world();
                let Some(component_id) = world.resolve_entity_component_id(*entity, name) else {
                    continue;
                };
                let Some(layout) = world.component_field_layout(component_id) else {
                    continue;
                };
                layout
                    .iter()
                    .map(|row| (row.name.to_string(), row.type_tag))
                    .collect()
            };
            // A freshly attached component is a zero-filled image, and a zeroed
            // reference names slot 0 - a real entity or asset. Every reference
            // the document does not name is therefore cleared explicitly.
            if fresh.contains(&(*entity, name.clone())) {
                for (field, tag) in &tags {
                    if fields.contains_key(field.as_str()) {
                        continue;
                    }
                    if !is_handle_tag(tag) && tag != &"entity" {
                        continue;
                    }
                    if engine
                        .world_mut()
                        .write_component_field(*entity, name, field, cleared_field(tag))
                        .is_err()
                    {
                        report
                            .skipped_fields
                            .push(format!("{entity}: {name}.{field} was refused"));
                    }
                }
            }
            for (field, encoded) in fields {
                let Some(tag) = tags.get(field.as_str()).copied() else {
                    report
                        .skipped_fields
                        .push(format!("{entity}: {name}.{field} is not in the layout"));
                    continue;
                };
                let value = match decoded_field(tag, encoded) {
                    Some(Decoded::Value(value)) => value,
                    Some(Decoded::Asset(saved)) => {
                        let assets = engine
                            .world_mut()
                            .get_resource_mut::<AssetManager>()
                            .expect("a world with an asset manager");
                        restore_asset_reference(assets, database, &registry, &saved)
                    }
                    Some(Decoded::Entity(id)) => {
                        links.push(*entity, name, field, id);
                        continue;
                    }
                    Some(Decoded::Cleared) => cleared_field(tag),
                    None => {
                        report
                            .skipped_fields
                            .push(format!("{entity}: {name}.{field} does not read"));
                        continue;
                    }
                };
                if engine
                    .world_mut()
                    .write_component_field(*entity, name, field, value)
                    .is_err()
                {
                    report
                        .skipped_fields
                        .push(format!("{entity}: {name}.{field} was refused"));
                }
            }
        }
    }

    // Step 3: the entity references, now that every entity of the document
    // exists.
    let (_, unresolved) = links.resolve(engine.world_mut());
    report.unresolved_references = unresolved.len();
    report
}

/// Attach `name` with its defaults to `entity`, reporting whether the image was
/// freshly attached; `Err(())` when the world does not know the type or cannot
/// build an image for it.
fn attach_component(engine: &mut Engine, entity: Entity, name: &str) -> Result<bool, ()> {
    if engine
        .world()
        .resolve_entity_component_id(entity, name)
        .is_some()
    {
        // Already attached: the document listed it twice, or the entity was
        // created with it. Its fields are written all the same, but its
        // references are not the zero-filled ones, so they are left alone.
        return Ok(false);
    }
    let bytes = engine
        .world()
        .build_component_image(name, &[])
        .map_err(|_| ())?;
    let component_id = engine
        .world()
        .resolve_component_id_by_name_any(name)
        .map_err(|_| ())?
        .ok_or(())?;
    engine.queue_deferred_commands(move |_world, queue| {
        if component_id.is_native_storage() {
            queue.add_component_adder_to_entity(
                entity,
                Box::new(ByteComponentAdder::new(component_id, bytes)),
            );
        } else {
            queue.add_descriptor_component_to_entity(entity, component_id, bytes);
        }
    });
    engine.flush_deferred_commands().map_err(|_| ())?;
    Ok(true)
}

/// What one document value means for the field it is written into.
enum Decoded {
    /// A value to write as it is.
    Value(FieldValue),
    /// An asset reference to resolve against the world's assets.
    Asset(SavedAssetReference),
    /// An entity reference to resolve after every entity exists.
    Entity(EntityId),
    /// `null` in a reference field: the cleared handle.
    Cleared,
}

/// Decode one document value for the field tagged `tag`.
fn decoded_field(tag: &'static str, encoded: &serde_json::Value) -> Option<Decoded> {
    if encoded.is_null() {
        return (is_handle_tag(tag) || tag == "entity").then_some(Decoded::Cleared);
    }
    if is_handle_tag(tag) {
        let saved: SavedAssetReference = serde_json::from_value(encoded.clone()).ok()?;
        return Some(Decoded::Asset(saved));
    }
    if tag == "entity" {
        return Some(Decoded::Entity(EntityId::parse(encoded.as_str()?)?));
    }
    if let Some(element_tag) = tag.strip_prefix("array:") {
        // The element tag is a prefix of the field's own `'static` tag, so the
        // same decoder applies; a handle or `entity` element is not reached,
        // because the field layer reads such an array as opaque bytes.
        let elements = encoded.as_array()?;
        let mut values = Vec::with_capacity(elements.len());
        for element in elements {
            match decoded_field(element_tag, element)? {
                Decoded::Value(value) => values.push(value),
                _ => return None,
            }
        }
        return Some(Decoded::Value(FieldValue::Array(values)));
    }
    let value = match tag {
        "f32" => FieldValue::F32(encoded.as_f64()? as f32),
        "f64" => FieldValue::F64(encoded.as_f64()?),
        "i8" => FieldValue::I8(encoded.as_i64()? as i8),
        "i16" => FieldValue::I16(encoded.as_i64()? as i16),
        "i32" => FieldValue::I32(encoded.as_i64()? as i32),
        "i64" => FieldValue::I64(encoded.as_i64()?),
        "u8" => FieldValue::U8(encoded.as_u64()? as u8),
        "u16" => FieldValue::U16(encoded.as_u64()? as u16),
        "u32" => FieldValue::U32(encoded.as_u64()? as u32),
        "u64" => FieldValue::U64(encoded.as_u64()?),
        "usize" => FieldValue::Usize(encoded.as_u64()? as usize),
        "isize" => FieldValue::Isize(encoded.as_i64()? as isize),
        "bool" => FieldValue::Bool(encoded.as_bool()?),
        "string" => FieldValue::Text(encoded.as_str()?.to_owned()),
        _ => return Some(Decoded::Value(opaque_field(tag, encoded)?)),
    };
    Some(Decoded::Value(value))
}

/// The raw bytes an opaque field was written from, tagged with the layout's own
/// `'static` tag: a document cannot introduce a tag the layout does not have.
fn opaque_field(tag: &'static str, encoded: &serde_json::Value) -> Option<FieldValue> {
    let object = encoded.as_object()?;
    if object.get("tag")?.as_str()? != tag {
        return None;
    }
    Some(FieldValue::Opaque {
        type_tag: tag,
        bytes: bytes_of(object.get("opaque")?.as_str()?)?,
    })
}

/// The value a `null` written for a reference field restores.
fn cleared_field(tag: &str) -> FieldValue {
    if tag == "entity" {
        FieldValue::Entity {
            id: Entity::NULL.id(),
            generation: Entity::NULL.generation(),
        }
    } else {
        FieldValue::Asset {
            asset_type: None,
            index: u32::MAX,
            generation: u32::MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AssetGuid, AssetLoadResult, Handle};
    use crate::asset_metadata::{ImportedAsset, MetadataPolicy};
    use crate::asset_store::{mount_directory, mounted_directory_test_lock};
    use crate::component::Component;
    use crate::component::ComponentRegistry;
    use crate::component_registry::ComponentFieldDescriptor;
    use std::borrow::Cow;
    use std::path::{Path, PathBuf};

    /// The test asset: a note's text.
    #[derive(Debug)]
    struct Note(String);
    impl Asset for Note {}
    impl_trait_accessible!(dyn Asset; Note);

    impl ImportedAsset for Note {
        type ImportSettings = ();
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["note"];

        fn metadata_type_name() -> &'static str {
            "pill_engine::scene::tests::Note"
        }

        fn import(_name: &str, bytes: &[u8], _settings: &()) -> AssetLoadResult<Self> {
            Ok(Self(String::from_utf8_lossy(bytes).into_owned()))
        }
    }

    /// A component with a scalar, an asset reference and an entity reference,
    /// which is the shape a scene has to carry.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct Actor {
        speed: f32,
        note: Handle<Note>,
        target: Entity,
    }
    impl Component for Actor {}

    /// Its layout, tagged the way the derive tags the three fields.
    static ACTOR_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "speed",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "note",
            type_tag: "struct:Handle<Note>",
            offset: 4,
            size: std::mem::size_of::<Handle<Note>>(),
            align: 4,
            element_count: 0,
            decorators: Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "target",
            type_tag: "entity",
            offset: 16,
            size: std::mem::size_of::<Entity>(),
            align: 8,
            element_count: 0,
            decorators: Cow::Borrowed(&[]),
        },
    ];

    const NOTE_GUID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";

    /// A scratch `res` directory, mounted while the test holds the lock, and an
    /// engine with the note type and the actor component registered.
    struct Scratch {
        root: PathBuf,
        engine: Engine,
        _mounted: std::sync::MutexGuard<'static, ()>,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let mounted = mounted_directory_test_lock();
            let root =
                std::env::temp_dir().join(format!("pill-scene-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            mount_directory(&root);
            let mut engine = Engine::new();
            let world = engine.world_mut();
            world.register_imported_asset::<Note>();
            world.register_standalone_asset::<Scene>();
            world.register_component_with_layout::<Actor>(ACTOR_FIELDS);
            world.record_asset_field_type(
                crate::component::ComponentId::of::<Actor>(),
                "note",
                Note::metadata_type_name(),
            );
            Self {
                root,
                engine,
                _mounted: mounted,
            }
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        /// A note at `relative` whose sidecar carries `guid`.
        fn note(&self, relative: &str, guid: &str) {
            self.write(relative, "a note");
            self.write(
                &format!("{relative}.meta"),
                &format!(
                    r#"{{"format_version": 1, "asset_type": "pill_engine::scene::tests::Note", "guid": "{guid}", "settings": null}}"#
                ),
            );
        }

        fn registry(&self) -> ImportRegistry {
            self.engine
                .world()
                .get_resource::<ImportRegistry>()
                .unwrap()
                .clone()
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

    fn actor_component() -> String {
        ComponentRegistry::registered_name::<Actor>()
    }

    /// AC1 and AC3: a world saves to a `.scene` file with a fresh guid, and
    /// loading that file into an empty engine brings back the same components
    /// and references - the asset by guid, the entity by stable id.
    #[test]
    fn a_scene_round_trips_through_a_file() {
        // Step 1: the world that saves.
        let (reference_guid, bytes, target_id) = {
            let mut scratch = Scratch::new("round-trip");
            scratch.note("notes/a.note", NOTE_GUID);
            let registry = scratch.registry();
            let database = scratch.database();
            let world = scratch.engine.world_mut();
            let loaded = {
                let assets = world.get_resource_mut::<AssetManager>().unwrap();
                registry
                    .load_by_guid(
                        assets,
                        &database,
                        AssetGuid::parse(NOTE_GUID).unwrap(),
                        MetadataPolicy::ReadIfPresent,
                    )
                    .unwrap()
            };
            let target_id = EntityId::from_name("hero-target");
            let target = world.create_entity().with_id(target_id).build().unwrap();
            world
                .create_entity()
                .with_id(EntityId::from_name("hero"))
                .with(Actor {
                    speed: 2.5,
                    note: Handle::from_raw(loaded.index, loaded.generation),
                    target,
                })
                .build()
                .unwrap();

            let assets = world.get_resource::<AssetManager>().unwrap();
            let (document, report) = save_scene(world, assets, &registry, None);
            assert!(
                report.skipped_fields.is_empty(),
                "{:?}",
                report.skipped_fields
            );
            assert_eq!(report.components, 1, "only the actor's component");
            assert_eq!(document.entities.len(), 2);

            // The document itself: a scalar, an asset reference by guid and an
            // entity reference by stable id.
            let actor = document
                .entities
                .iter()
                .find(|entity| entity.components.contains_key(&actor_component()))
                .expect("the actor is in the document");
            let fields = &actor.components[&actor_component()];
            assert_eq!(fields["speed"], serde_json::json!(2.5));
            assert_eq!(
                fields["note"],
                serde_json::json!({ "guid": NOTE_GUID, "path": "notes/a.note" })
            );
            assert_eq!(fields["target"], serde_json::json!(target_id.to_string()));

            // Step 2: the file, written where a project keeps its scenes.
            let guid = AssetGuid::random().expect("a fresh guid");
            let bytes = crate::asset_standalone::render_standalone::<Scene>(guid, &document)
                .expect("the scene renders");
            assert!(!document.entities.is_empty());
            (guid, bytes, target_id)
        };

        // Step 3: an empty engine with the same project reads the file back,
        // through the registry that knows the extension.
        let mut scratch = Scratch::new("load");
        scratch.note("notes/a.note", NOTE_GUID);
        scratch.write("scenes/a.scene", &String::from_utf8(bytes).unwrap());
        let database = scratch.database();
        // Relative, because an asset's name is its path below the project's
        // asset directory.
        let path = Path::new("scenes/a.scene");
        let scene = {
            let assets = scratch
                .engine
                .world_mut()
                .get_resource_mut::<AssetManager>()
                .unwrap();
            let imported = assets
                .import_standalone::<Scene>(&path)
                .expect("the scene file loads");
            assert_eq!(
                imported.guid, reference_guid,
                "the file keeps the guid it was saved with"
            );
            assets
                .get(imported.handle)
                .expect("the scene asset is live")
                .clone()
        };
        let report = load_scene(&mut scratch.engine, &database, scene.document());
        assert_eq!(report.entities, 2);
        assert_eq!(report.components, 1);
        assert!(
            report.unknown_components.is_empty(),
            "{:?}",
            report.unknown_components
        );
        assert!(
            report.skipped_fields.is_empty(),
            "{:?}",
            report.skipped_fields
        );
        assert_eq!(report.unresolved_references, 0);

        let world = scratch.engine.world_mut();
        let hero = world
            .entity_by_id(EntityId::from_name("hero"))
            .expect("the hero exists");
        let actor = *world
            .get_component::<Actor>(hero)
            .expect("the actor component");
        assert_eq!(actor.speed, 2.5);
        let assets = world.get_resource::<AssetManager>().unwrap();
        assert_eq!(
            assets.guid_of(actor.note),
            Some(AssetGuid::parse(NOTE_GUID).unwrap()),
            "the asset reference resolves to the same file"
        );
        assert_eq!(
            assets.get(actor.note).map(|note| note.0.as_str()),
            Some("a note"),
            "and the asset it names is the one that was loaded"
        );
        assert_eq!(world.entity_id(actor.target), Some(target_id));
    }

    /// Whatever `save_scene` writes, `load_scene` restores.
    ///
    /// A heap-backed field is read-only through the field layer - its buffer is
    /// reached through the type's own accessors, not through raw row bytes - so
    /// the saver must leave one out. It used to write a `string` as JSON while
    /// the writer refused the same value back, so the file claimed a value no
    /// load could restore, and `vec:` fields were dropped without a word.
    #[test]
    fn a_saved_field_is_one_the_loader_accepts() {
        static LABELED_FIELDS: &[ComponentFieldDescriptor] = &[
            ComponentFieldDescriptor {
                name: "label",
                type_tag: "string",
                offset: 0,
                size: std::mem::size_of::<String>(),
                align: std::mem::align_of::<String>(),
                element_count: 0,
                decorators: Cow::Borrowed(&[]),
            },
            ComponentFieldDescriptor {
                name: "points",
                type_tag: "vec:u32",
                offset: std::mem::size_of::<String>(),
                size: std::mem::size_of::<Vec<u32>>(),
                align: std::mem::align_of::<Vec<u32>>(),
                element_count: 0,
                decorators: Cow::Borrowed(&[]),
            },
        ];

        #[repr(C)]
        #[derive(Debug)]
        struct Labeled {
            label: String,
            points: Vec<u32>,
        }
        impl Component for Labeled {}

        // Step 1: a world holding both heap kinds saves.
        let (document, save) = {
            let mut scratch = Scratch::new("heap-save");
            scratch
                .engine
                .world_mut()
                .register_component_with_layout::<Labeled>(LABELED_FIELDS);
            let registry = scratch.registry();
            let world = scratch.engine.world_mut();
            world
                .create_entity()
                .with(Labeled {
                    label: "a note's title".to_string(),
                    points: vec![1, 2, 3],
                })
                .build()
                .expect("entity builds");
            let assets = world.get_resource::<AssetManager>().unwrap();
            save_scene(world, assets, &registry, None)
        };

        // Step 2: the document carries no field a load would have to refuse,
        // and both omissions are reported rather than silent.
        let component_fields =
            &document.entities[0].components[&ComponentRegistry::registered_name::<Labeled>()];
        assert!(
            component_fields.is_empty(),
            "a heap-backed field is never written: {component_fields:?}"
        );
        for field in ["label", "points"] {
            assert!(
                save.skipped_fields
                    .iter()
                    .any(|skipped| skipped.ends_with(&format!(".{field}"))),
                "{field} is reported as skipped: {:?}",
                save.skipped_fields
            );
        }
    }

    /// AC2: a component the world does not know is reported, and the rest of
    /// the entity still loads.
    #[test]
    fn an_unknown_component_is_reported_and_the_rest_loads() {
        let mut scratch = Scratch::new("unknown");
        let id = EntityId::from_name("hero");
        let mut fields = BTreeMap::new();
        fields.insert("speed".to_string(), serde_json::json!(1.25));
        let mut components = BTreeMap::new();
        components.insert(actor_component(), fields);
        components.insert(
            "demo::Missing".to_string(),
            BTreeMap::from([("x".to_string(), serde_json::json!(1.0))]),
        );
        let document = SceneDocument {
            entities: vec![SceneEntity { id, components }],
        };

        let report = load_scene(&mut scratch.engine, &AssetDatabase::new(), &document);
        assert_eq!(report.entities, 1);
        assert_eq!(report.components, 1, "the known component attaches");
        assert_eq!(report.unknown_components, vec!["demo::Missing".to_string()]);

        let world = scratch.engine.world_mut();
        let hero = world.entity_by_id(id).expect("the entity exists");
        let actor = world
            .get_component::<Actor>(hero)
            .expect("the actor exists");
        assert_eq!(actor.speed, 1.25);
        assert_eq!(actor.target, Entity::NULL, "an absent reference is unset");
        assert_eq!(actor.note, Handle::default(), "and so is an absent handle");
    }
}

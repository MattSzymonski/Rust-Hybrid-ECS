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
//! The document is addressed by the field layer's names, not by the component
//! types' own serde: a handle field then saves as its guid and an entity field
//! as its stable id. A component whose own serde writes a handle as a slot pair
//! cannot be written straight into a file meant for a later run, so the field
//! layer - which knows what a reference *is* - is the writer.
//!
//! A field the field layer can only summarise (a `vec:` container, a `string`)
//! is not written; [`SceneSave::skipped_fields`] names each one, so a lossy
//! save says so rather than passing for a complete one. A field it can locate
//! but not interpret (a `struct:` with no known layout) is written as its raw
//! bytes, which write back verbatim into the same layout. A whole component it
//! cannot reach - no layout, or a row it cannot read - is left out and named in
//! [`SceneSave::skipped_components`], which is the caller's to report.
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
use crate::commands::{ByteComponentAdder, CommandError};
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

// Shared identity: a type two binaries each compile has one `TypeId` per
// binary, so the pinned name is what makes them one asset column - and what a
// saved `Handle<Scene>` field records. Keep it verbatim when moving the type.
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
    /// One entry per component the save left out, by `<entity>: <component>`
    /// and the reason. A component is written through the field layer, so one
    /// with no layout of its own has no fields to write - the document then
    /// holds the entity without it, which is a loss the caller has to see.
    pub skipped_components: Vec<String>,
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
                report
                    .skipped_components
                    .push(format!("{id}: {name} is not registered on this entity"));
                continue;
            };
            let Some(layout) = world.component_field_layout(component_id) else {
                report
                    .skipped_components
                    .push(format!("{id}: {name} has no field layout"));
                continue;
            };
            let values = match world.read_component_fields(entity, &name) {
                Ok(values) => values,
                Err(_) => {
                    report
                        .skipped_components
                        .push(format!("{id}: {name} cannot be read"));
                    continue;
                }
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
        FieldValue::Asset { .. } | FieldValue::Entity { .. } => !value.references_nothing(),
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
///
/// Each bucket admits one kind of loss:
///
/// - [`unknown_components`](Self::unknown_components): the world knows no
///   component of that name, so nothing could be attached.
/// - [`failed_components`](Self::failed_components): the world knows the type
///   but could not attach it - the reason is the field layer's or a command's
///   own message.
/// - [`skipped_entities`](Self::skipped_entities): a document entry was dropped
///   because its id repeats or is already taken.
/// - [`skipped_fields`](Self::skipped_fields): one field of an attached
///   component could not be written, by `<entity>: <component>.<field>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SceneLoad {
    /// Entities created.
    pub entities: usize,
    /// Components attached.
    pub components: usize,
    /// Component type names the world does not know.
    pub unknown_components: Vec<String>,
    /// Components that failed to attach although the type is registered, each
    /// entry naming the entity, the component and the reason.
    pub failed_components: Vec<String>,
    /// References naming an entity the document did not create.
    pub unresolved_references: usize,
    /// Document entries whose entity was not created, with the reason.
    pub skipped_entities: Vec<String>,
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
                .skipped_entities
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
                .skipped_entities
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
                Err(AttachError::UnknownType) => report.unknown_components.push(name.clone()),
                Err(failure) => report
                    .failed_components
                    .push(format!("{entity}: {name} could not attach: {failure}")),
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
                    if let Err(error) = engine.world_mut().write_component_field(
                        *entity,
                        name,
                        field,
                        cleared_field(tag),
                    ) {
                        report
                            .skipped_fields
                            .push(format!("{entity}: {name}.{field} was refused: {error}"));
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
                    Ok(Decoded::Value(value)) => value,
                    Ok(Decoded::Asset(saved)) => {
                        let assets = engine
                            .world_mut()
                            .get_resource_mut::<AssetManager>()
                            .expect("a world with an asset manager");
                        restore_asset_reference(assets, database, &registry, &saved)
                    }
                    Ok(Decoded::Entity(id)) => {
                        links.push(*entity, name, field, id);
                        continue;
                    }
                    Ok(Decoded::Cleared) => cleared_field(tag),
                    Err(reason) => {
                        report
                            .skipped_fields
                            .push(format!("{entity}: {name}.{field} {reason}"));
                        continue;
                    }
                };
                if let Err(error) = engine
                    .world_mut()
                    .write_component_field(*entity, name, field, value)
                {
                    report
                        .skipped_fields
                        .push(format!("{entity}: {name}.{field} was refused: {error}"));
                }
            }
        }
    }

    // Step 3: the entity references, once every entity of the document exists.
    let (_, unresolved) = links.resolve(engine.world_mut());
    report.unresolved_references = unresolved.len();
    report
}

/// Why a scene document's component could not be attached.
#[derive(Debug)]
enum AttachError {
    /// The world knows no component of that name.
    UnknownType,
    /// The world knows the type but could not attach it; the message is the
    /// field layer's or a command's own reason.
    Failed(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownType => formatter.write_str("the world knows no component of this name"),
            Self::Failed(reason) => formatter.write_str(reason),
        }
    }
}

/// Attach `name` with its defaults to `entity`, reporting whether the image was
/// freshly attached.
///
/// # Errors
///
/// [`AttachError::UnknownType`] when the world knows no component of that
/// name; [`AttachError::Failed`] when it knows the type but the image cannot be
/// built or the add command is refused, carrying that layer's own reason.
fn attach_component(engine: &mut Engine, entity: Entity, name: &str) -> Result<bool, AttachError> {
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
    // The type is resolved first so "unknown" and "known but unbuildable" stay
    // separate answers: only the first is an unknown component.
    let component_id = engine
        .world()
        .resolve_component_id_by_name_any(name)
        .map_err(|_| AttachError::UnknownType)?
        .ok_or(AttachError::UnknownType)?;
    let bytes = engine
        .world()
        .build_component_image(name, &[])
        .map_err(|error| AttachError::Failed(error.to_string()))?;
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
    engine
        .flush_deferred_commands()
        .map_err(|errors| AttachError::Failed(join_command_errors(errors)))?;
    Ok(true)
}

/// Renders a failed command batch as one reason string.
fn join_command_errors(errors: Vec<CommandError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
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
///
/// `Err` carries the reason the value cannot be written, which the load report
/// names next to the field: a shape a field cannot read, or a number that does
/// not fit the field's type.
fn decoded_field(tag: &'static str, encoded: &serde_json::Value) -> Result<Decoded, String> {
    if encoded.is_null() {
        return if is_handle_tag(tag) || tag == "entity" {
            Ok(Decoded::Cleared)
        } else {
            Err("does not read".to_owned())
        };
    }
    if is_handle_tag(tag) {
        let saved: SavedAssetReference =
            serde_json::from_value(encoded.clone()).map_err(|_| "does not read".to_owned())?;
        return Ok(Decoded::Asset(saved));
    }
    if tag == "entity" {
        return encoded
            .as_str()
            .and_then(EntityId::parse)
            .map(Decoded::Entity)
            .ok_or_else(|| "does not read".to_owned());
    }
    if let Some(element_tag) = tag.strip_prefix("array:") {
        // The element tag is a prefix of the field's own `'static` tag, so the
        // same decoder applies; a handle or `entity` element is not reached,
        // because the field layer reads such an array as opaque bytes.
        let elements = encoded
            .as_array()
            .ok_or_else(|| "does not read".to_owned())?;
        let mut values = Vec::with_capacity(elements.len());
        for element in elements {
            match decoded_field(element_tag, element)? {
                Decoded::Value(value) => values.push(value),
                _ => return Err("does not read".to_owned()),
            }
        }
        return Ok(Decoded::Value(FieldValue::Array(values)));
    }
    let value = match tag {
        "f32" => FieldValue::F32(narrowed_f32(tag, encoded)?),
        "f64" => FieldValue::F64(encoded.as_f64().ok_or_else(|| "does not read".to_owned())?),
        "i8" => FieldValue::I8(narrowed_signed(tag, encoded)?),
        "i16" => FieldValue::I16(narrowed_signed(tag, encoded)?),
        "i32" => FieldValue::I32(narrowed_signed(tag, encoded)?),
        "i64" => FieldValue::I64(encoded.as_i64().ok_or_else(|| "does not read".to_owned())?),
        "u8" => FieldValue::U8(narrowed_unsigned(tag, encoded)?),
        "u16" => FieldValue::U16(narrowed_unsigned(tag, encoded)?),
        "u32" => FieldValue::U32(narrowed_unsigned(tag, encoded)?),
        "u64" => FieldValue::U64(encoded.as_u64().ok_or_else(|| "does not read".to_owned())?),
        "usize" => FieldValue::Usize(narrowed_unsigned(tag, encoded)?),
        "isize" => FieldValue::Isize(narrowed_signed(tag, encoded)?),
        "bool" => FieldValue::Bool(
            encoded
                .as_bool()
                .ok_or_else(|| "does not read".to_owned())?,
        ),
        "string" => FieldValue::Text(
            encoded
                .as_str()
                .ok_or_else(|| "does not read".to_owned())?
                .to_owned(),
        ),
        _ => {
            return Ok(Decoded::Value(
                opaque_field(tag, encoded).ok_or_else(|| "does not read".to_owned())?,
            ))
        }
    };
    Ok(Decoded::Value(value))
}

/// A whole number narrowed to a signed field's own type.
///
/// A value that does not fit is refused rather than cast: `as i8` would wrap 300
/// into 44 and leave the world holding a number the document never said. The
/// report names the refused value.
fn narrowed_signed<T: TryFrom<i64>>(
    tag: &'static str,
    encoded: &serde_json::Value,
) -> Result<T, String> {
    let number = encoded.as_i64().ok_or_else(|| "does not read".to_owned())?;
    T::try_from(number).map_err(|_| format!("{encoded} does not fit a `{tag}`"))
}

/// A whole number narrowed to an unsigned field's own type, refusing a
/// negative one and one that does not fit.
fn narrowed_unsigned<T: TryFrom<u64>>(
    tag: &'static str,
    encoded: &serde_json::Value,
) -> Result<T, String> {
    let number = encoded
        .as_u64()
        .ok_or_else(|| format!("{encoded} does not fit a `{tag}`"))?;
    T::try_from(number).map_err(|_| format!("{encoded} does not fit a `{tag}`"))
}

/// A number narrowed to an `f32`, refusing the one cast that changes its kind:
/// a finite value whose magnitude exceeds `f32::MAX` became `inf`. Rounding
/// within range is what the type is for, so it is not a refusal.
fn narrowed_f32(tag: &'static str, encoded: &serde_json::Value) -> Result<f32, String> {
    let number = encoded.as_f64().ok_or_else(|| "does not read".to_owned())?;
    let narrowed = number as f32;
    if number.is_finite() && !narrowed.is_finite() {
        return Err(format!("{encoded} does not fit a `{tag}`"));
    }
    Ok(narrowed)
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
        FieldValue::unset_asset()
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

    /// A world saves to a `.scene` file with a fresh guid, and loading that file
    /// into an empty engine brings back the same components and references -
    /// the asset by guid, the entity by stable id.
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
                .import_standalone::<Scene>(path)
                .expect("the scene file loads");
            assert_eq!(
                imported.guid,
                Some(reference_guid),
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
    /// the saver leaves one out and reports it. Writing a `string` would put a
    /// value in the file that no load could restore.
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

    /// A component the world knows but has no field layout for is not written,
    /// and the report says so instead of dropping it.
    ///
    /// The field layer is what writes a scene, so a component without a layout
    /// has no fields to write; keeping quiet about it would put gameplay state
    /// silently out of the file.
    #[test]
    fn a_component_without_a_layout_is_reported_as_not_written() {
        #[repr(C)]
        #[derive(Debug, Clone, Copy)]
        struct Plain {
            value: u32,
        }
        impl Component for Plain {}

        let mut scratch = Scratch::new("no-layout");
        let world = scratch.engine.world_mut();
        world.register_component::<Plain>();
        let entity = world
            .create_entity()
            .with(Actor {
                speed: 1.0,
                note: Handle::default(),
                target: Entity::NULL,
            })
            .with(Plain { value: 7 })
            .build()
            .expect("entity builds");
        assert_eq!(
            world.entity_component_names(entity).unwrap().len(),
            2,
            "the entity carries both components"
        );
        let id = world.entity_id(entity).expect("the entity has a stable id");
        let registry = scratch.registry();
        let (document, report) = {
            let world = scratch.engine.world();
            let assets = world.get_resource::<AssetManager>().unwrap();
            save_scene(world, assets, &registry, None)
        };

        // The document holds the component the field layer can write, and only
        // that one.
        let written = &document.entities[0].components;
        assert_eq!(report.components, 1, "one of the two is written");
        assert!(written.contains_key(&actor_component()), "{written:?}");

        // The other one is reported, by entity and name, with the reason.
        let plain = ComponentRegistry::registered_name::<Plain>();
        assert_eq!(
            report.skipped_components,
            vec![format!("{id}: {plain} has no field layout")],
            "the component the save left out is named"
        );
    }

    /// A component the world does not know is reported, and the rest of the
    /// entity still loads.
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

    /// A component the world knows but cannot build an image for is reported
    /// in its own bucket with the reason, not as an unknown type.
    ///
    /// The two failures mean different things to whoever reads the report: an
    /// unknown name is a missing registration, while an unbuildable image is a
    /// registered type the field layer cannot create - a heap-backed or
    /// layout-less component.
    #[test]
    fn a_registered_component_that_cannot_be_attached_is_reported_with_its_reason() {
        #[repr(C)]
        #[derive(Debug, Clone, Copy)]
        struct Layoutless {
            value: u32,
        }
        impl Component for Layoutless {}

        let mut scratch = Scratch::new("unbuildable");
        scratch
            .engine
            .world_mut()
            .register_component::<Layoutless>();
        let id = EntityId::from_name("hero");
        let document = SceneDocument {
            entities: vec![SceneEntity {
                id,
                components: BTreeMap::from([(
                    ComponentRegistry::registered_name::<Layoutless>(),
                    BTreeMap::from([("value".to_string(), serde_json::json!(3))]),
                )]),
            }],
        };

        let report = load_scene(&mut scratch.engine, &AssetDatabase::new(), &document);

        assert!(
            report.unknown_components.is_empty(),
            "the type is registered, so it is not unknown: {:?}",
            report.unknown_components
        );
        assert_eq!(
            report.failed_components.len(),
            1,
            "one component failed to attach: {:?}",
            report.failed_components
        );
        let said = &report.failed_components[0];
        assert!(
            said.contains("could not attach") && said.contains("no registered field layout"),
            "the entry carries the field layer's reason: {said}"
        );
        assert_eq!(report.components, 0);
    }

    /// A document that repeats an entity id reports the dropped entry as an
    /// entity, not as a field.
    #[test]
    fn a_repeated_entity_id_is_reported_as_a_skipped_entity() {
        let mut scratch = Scratch::new("repeated-id");
        let id = EntityId::from_name("hero");
        let components = BTreeMap::from([(
            actor_component(),
            BTreeMap::from([("speed".to_string(), serde_json::json!(1.0))]),
        )]);
        let document = SceneDocument {
            entities: vec![
                SceneEntity {
                    id,
                    components: components.clone(),
                },
                SceneEntity { id, components },
            ],
        };

        let report = load_scene(&mut scratch.engine, &AssetDatabase::new(), &document);

        assert_eq!(report.entities, 1, "only the first entry creates an entity");
        assert_eq!(
            report.skipped_entities,
            vec![format!("{id}: the id repeats in this scene")]
        );
        assert!(
            report.skipped_fields.is_empty(),
            "nothing was lost at field level: {:?}",
            report.skipped_fields
        );
    }

    /// A document value that does not fit its field's type is refused and
    /// reported, instead of being narrowed into a different number.
    ///
    /// Casting would wrap `"amount": 300` to 44 and turn `1e40` into `inf` for
    /// an `f32`, leaving the world holding numbers the document never said.
    #[test]
    fn a_value_that_does_not_fit_its_field_is_reported() {
        #[repr(C)]
        #[derive(Debug, Clone, Copy, Default)]
        struct Damage {
            amount: u8,
            scale: f32,
        }
        impl Component for Damage {}

        static DAMAGE_FIELDS: &[ComponentFieldDescriptor] = &[
            ComponentFieldDescriptor {
                name: "amount",
                type_tag: "u8",
                offset: 0,
                size: 1,
                align: 1,
                element_count: 0,
                decorators: Cow::Borrowed(&[]),
            },
            ComponentFieldDescriptor {
                name: "scale",
                type_tag: "f32",
                offset: 4,
                size: 4,
                align: 4,
                element_count: 0,
                decorators: Cow::Borrowed(&[]),
            },
        ];

        let mut scratch = Scratch::new("narrow");
        scratch
            .engine
            .world_mut()
            .register_component_with_layout::<Damage>(DAMAGE_FIELDS);
        let id = EntityId::from_name("hero");
        let name = ComponentRegistry::registered_name::<Damage>();
        let mut fields = BTreeMap::new();
        fields.insert("amount".to_string(), serde_json::json!(300));
        fields.insert("scale".to_string(), serde_json::json!(1.0e40));
        let document = SceneDocument {
            entities: vec![SceneEntity {
                id,
                components: BTreeMap::from([(name.clone(), fields)]),
            }],
        };

        let report = load_scene(&mut scratch.engine, &AssetDatabase::new(), &document);
        assert_eq!(report.components, 1, "the component still attaches");
        assert_eq!(
            report.skipped_fields.len(),
            2,
            "both out-of-range values are reported: {:?}",
            report.skipped_fields
        );
        for (field, said) in [("amount", "300"), ("scale", "1e+40")] {
            let entry = report
                .skipped_fields
                .iter()
                .find(|skipped| skipped.contains(&format!("{name}.{field} ")))
                .unwrap_or_else(|| panic!("{field} is reported: {:?}", report.skipped_fields));
            assert!(
                entry.contains(said) && entry.contains("does not fit"),
                "{field} is reported with the value and the reason: {entry}"
            );
        }

        // Refused, not narrowed: the component keeps the zero its image was
        // built with, so nothing invents a number the document never said.
        let world = scratch.engine.world_mut();
        let hero = world.entity_by_id(id).expect("the entity exists");
        let damage = world.get_component::<Damage>(hero).expect("it attached");
        assert_eq!(damage.amount, 0, "300 did not wrap to 44");
        assert_eq!(damage.scale, 0.0, "1e40 did not become inf");
    }
}

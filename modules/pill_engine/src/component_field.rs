//! Generic, type-erased component field access for editor-style tools.
//!
//! # Responsibilities
//!
//! - Read one field (or every field) of a component on one entity as a
//!   [`FieldValue`], using the field layout registered with the component
//!   ([`crate::World::component_field_layout`]).
//! - Write one field (or one fixed-array element) back into the live column,
//!   stamping that row's `changed` tick so `Changed<T>` systems observe the
//!   edit next frame.
//!
//! # Design
//!
//! Components are stored in erased columns with no `dyn Any` downcast, so the
//! only generic way to reach an arbitrary component's value is the field
//! layout (`name`, `type_tag`, `offset`, `size`, `align`, `element_count`)
//! that `#[derive(PillComponent)]` / `#[derive(PillMirror)]` register. This
//! module is the safe wrapper over that data: it resolves a component through
//! **the entity's own archetype** (never through the registry-wide
//! name resolver, whose "highest bit" entry is not necessarily the generation
//! that created the data), validates every value against the layout, and never
//! hands a raw pointer to the caller.
//!
//! The [`FieldValue`] vocabulary mirrors the derive's closed `type_tag`
//! vocabulary one-to-one. `struct:` fields (and anything else the engine
//! cannot interpret) are surfaced as [`FieldValue::Opaque`] on read and are
//! never writable, so an inspector can show them instead of hiding them.

// Standard library
use std::mem::size_of;

// Current crate
use crate::archetype::ArchetypeId;
use crate::component::{ComponentId, Tick};
use crate::component_registry::ComponentFieldDescriptor;
use crate::entity::Entity;
use crate::world::World;

// =============================================================================
// Value vocabulary
// =============================================================================

/// One read or edited value in the engine's reflected vocabulary.
///
/// Mirrors the derive's `type_tag` vocabulary 1:1: the scalar primitives the
/// engine can describe, whole fixed-size arrays, and an `Opaque` escape hatch
/// for fields the engine can locate but not interpret (`struct:` tags whose
/// value type is not resolvable, and any tag outside the vocabulary). `Opaque`
/// carries the raw bytes so the UI can show them; it is never writable.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// A 32-bit IEEE-754 floating point value.
    F32(f32),
    /// A 64-bit IEEE-754 floating point value.
    F64(f64),
    /// A signed 8-bit integer.
    I8(i8),
    /// A signed 16-bit integer.
    I16(i16),
    /// A signed 32-bit integer.
    I32(i32),
    /// A signed 64-bit integer.
    I64(i64),
    /// An unsigned 8-bit integer.
    U8(u8),
    /// An unsigned 16-bit integer.
    U16(u16),
    /// An unsigned 32-bit integer.
    U32(u32),
    /// An unsigned 64-bit integer.
    U64(u64),
    /// A boolean, stored as a single Rust byte in the component image.
    Bool(bool),
    /// A pointer-width unsigned integer.
    Usize(usize),
    /// A pointer-width signed integer.
    Isize(isize),
    /// A whole fixed-size array field, one entry per element in layout order.
    Array(Vec<FieldValue>),
    /// A field the engine can locate but not interpret. Never writable.
    Opaque {
        /// The registered `type_tag` of the field (for example `struct:path`).
        type_tag: &'static str,
        /// The raw bytes of the field, copied out of the component row.
        bytes: Vec<u8>,
    },
}

impl FieldValue {
    /// The closed-vocabulary tag a value's variant corresponds to.
    ///
    /// Mirrors the `type_tag` strings the derive emits, so a mismatch can name
    /// both sides. `Array` and `Opaque` are not derive tags; they use the
    /// labels below for diagnostics.
    pub(crate) fn variant_tag(&self) -> &'static str {
        match self {
            Self::F32(_) => "f32",
            Self::F64(_) => "f64",
            Self::I8(_) => "i8",
            Self::I16(_) => "i16",
            Self::I32(_) => "i32",
            Self::I64(_) => "i64",
            Self::U8(_) => "u8",
            Self::U16(_) => "u16",
            Self::U32(_) => "u32",
            Self::U64(_) => "u64",
            Self::Bool(_) => "bool",
            Self::Usize(_) => "usize",
            Self::Isize(_) => "isize",
            Self::Array(_) => "array",
            Self::Opaque { .. } => "opaque",
        }
    }
}

// =============================================================================
// Errors
// =============================================================================

/// Why a generic field read or write was refused.
///
/// Every failure is all-or-nothing: nothing is written when any validation
/// fails, and the error names the offending entity/component/field so the
/// editor can surface it in a console rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentFieldError {
    /// The entity handle is not alive.
    EntityNotFound,
    /// The entity is alive but does not carry a component of that name.
    ComponentNotFound {
        /// The requested component type name.
        component: String,
    },
    /// The component is registered but has no registered field layout, so it
    /// cannot be inspected or edited generically.
    ComponentHasNoFieldLayout {
        /// The component type name.
        component: String,
    },
    /// The component has fields, but not the one named.
    FieldNotFound {
        /// The component type name.
        component: String,
        /// The requested field name.
        field: String,
    },
    /// The value variant does not match the field's registered `type_tag`.
    TypeMismatch {
        /// The field being edited.
        field: String,
        /// The tag the layout declares.
        expected: &'static str,
        /// The tag the caller supplied.
        found: &'static str,
    },
    /// A whole-array write supplied the wrong number of elements.
    ArrayLengthMismatch {
        /// The array field being edited.
        field: String,
        /// The element count the layout declares.
        expected: usize,
        /// The element count the caller supplied.
        found: usize,
    },
    /// An element write addressed an element outside the fixed array.
    ArrayIndexOutOfRange {
        /// The array field being edited.
        field: String,
        /// The requested index.
        index: usize,
        /// The element count the layout declares.
        length: usize,
    },
    /// The field's tag is outside the editable vocabulary (`struct:` and
    /// anything the derive does not emit a scalar tag for).
    UnsupportedField {
        /// The field being edited.
        field: String,
        /// Why it cannot be edited.
        reason: &'static str,
    },
}

impl std::fmt::Display for ComponentFieldError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.summary())
    }
}

impl std::error::Error for ComponentFieldError {}

impl ComponentFieldError {
    /// A short, stable diagnostic for console surfacing.
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::EntityNotFound => "entity is not alive".to_string(),
            Self::ComponentNotFound { component } => {
                format!("component `{component}` is not on this entity")
            }
            Self::ComponentHasNoFieldLayout { component } => {
                format!("component `{component}` has no registered field layout")
            }
            Self::FieldNotFound { component, field } => {
                format!("component `{component}` has no field `{field}`")
            }
            Self::TypeMismatch {
                field,
                expected,
                found,
            } => format!("field `{field}` expects `{expected}` but received `{found}`"),
            Self::ArrayLengthMismatch {
                field,
                expected,
                found,
            } => format!("field `{field}` expects {expected} elements but received {found}"),
            Self::ArrayIndexOutOfRange {
                field,
                index,
                length,
            } => format!("field `{field}` index {index} is out of range (length {length})"),
            Self::UnsupportedField { field, reason } => {
                format!("field `{field}` is not editable: {reason}")
            }
        }
    }
}

// =============================================================================
// Tag vocabulary helpers
// =============================================================================

/// The size in bytes of one scalar whose tag is in the closed vocabulary.
fn scalar_size(tag: &str) -> Option<usize> {
    Some(match tag {
        "f32" => 4,
        "f64" => 8,
        "i8" => 1,
        "u8" => 1,
        "i16" => 2,
        "u16" => 2,
        "i32" => 4,
        "u32" => 4,
        "i64" => 8,
        "u64" => 8,
        "bool" => 1,
        "usize" => size_of::<usize>(),
        "isize" => size_of::<isize>(),
        _ => return None,
    })
}

/// Whether a tag names one of the scalar primitives.
fn is_scalar_tag(tag: &str) -> bool {
    scalar_size(tag).is_some()
}

/// Encode one scalar value into `output`, returning whether the tag matched.
fn encode_scalar(tag: &str, value: &FieldValue, output: &mut Vec<u8>) -> bool {
    match (tag, value) {
        ("f32", FieldValue::F32(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("f64", FieldValue::F64(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("i8", FieldValue::I8(v)) => output.push(*v as u8),
        ("i16", FieldValue::I16(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("i32", FieldValue::I32(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("i64", FieldValue::I64(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("u8", FieldValue::U8(v)) => output.push(*v),
        ("u16", FieldValue::U16(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("u32", FieldValue::U32(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("u64", FieldValue::U64(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("bool", FieldValue::Bool(v)) => output.push(u8::from(*v)),
        ("usize", FieldValue::Usize(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        ("isize", FieldValue::Isize(v)) => output.extend_from_slice(&v.to_ne_bytes()),
        _ => return false,
    }
    true
}

/// Decode one scalar from a byte buffer at `offset`, or `None` when the tag is
/// not a scalar.
fn decode_scalar(bytes: &[u8], offset: usize, tag: &str) -> Option<FieldValue> {
    fn read<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
        bytes.get(offset..offset + N)?.try_into().ok()
    }
    Some(match tag {
        "f32" => FieldValue::F32(f32::from_ne_bytes(read::<4>(bytes, offset)?)),
        "f64" => FieldValue::F64(f64::from_ne_bytes(read::<8>(bytes, offset)?)),
        "i8" => FieldValue::I8(*bytes.get(offset)? as i8),
        "i16" => FieldValue::I16(i16::from_ne_bytes(read::<2>(bytes, offset)?)),
        "i32" => FieldValue::I32(i32::from_ne_bytes(read::<4>(bytes, offset)?)),
        "i64" => FieldValue::I64(i64::from_ne_bytes(read::<8>(bytes, offset)?)),
        "u8" => FieldValue::U8(*bytes.get(offset)?),
        "u16" => FieldValue::U16(u16::from_ne_bytes(read::<2>(bytes, offset)?)),
        "u32" => FieldValue::U32(u32::from_ne_bytes(read::<4>(bytes, offset)?)),
        "u64" => FieldValue::U64(u64::from_ne_bytes(read::<8>(bytes, offset)?)),
        "bool" => FieldValue::Bool(bytes.get(offset).copied()? != 0),
        "usize" => FieldValue::Usize(usize::from_ne_bytes(read::<8>(bytes, offset)?)),
        "isize" => FieldValue::Isize(isize::from_ne_bytes(read::<8>(bytes, offset)?)),
        _ => return None,
    })
}

/// The number of bytes one fixed-array element occupies, from the descriptor.
fn array_element_size(descriptor: &ComponentFieldDescriptor) -> Option<usize> {
    if descriptor.element_count == 0 {
        return None;
    }
    Some(descriptor.size / descriptor.element_count)
}

/// Decode one field (scalar, whole array, or opaque) from a row image.
fn decode_field(bytes: &[u8], descriptor: &ComponentFieldDescriptor) -> FieldValue {
    // The layout's tag is `&'static` (compile-time or leaked runtime string),
    // so the stripped inner tag is `&'static` too.
    let Some(inner_tag) = descriptor.type_tag.strip_prefix("array:") else {
        return decode_tagged(
            bytes,
            descriptor.offset,
            descriptor.type_tag,
            descriptor.size,
        );
    };
    let Some(element_size) = array_element_size(descriptor) else {
        return FieldValue::Array(Vec::new());
    };
    let mut elements = Vec::with_capacity(descriptor.element_count);
    for element_index in 0..descriptor.element_count {
        let element_offset = descriptor.offset + element_index * element_size;
        elements.push(decode_tagged(
            bytes,
            element_offset,
            inner_tag,
            element_size,
        ));
    }
    FieldValue::Array(elements)
}

/// Decode one tag-sized value: a scalar when the tag is scalar, otherwise an
/// opaque byte copy of `size` bytes.
///
/// `tag` is `'static` because every tag an `Opaque` value can carry comes from
/// a registered field layout (compile-time static data or a `Box::leak`ed
/// runtime string).
fn decode_tagged(bytes: &[u8], offset: usize, tag: &'static str, size: usize) -> FieldValue {
    if let Some(value) = decode_scalar(bytes, offset, tag) {
        return value;
    }
    let mut raw = vec![0u8; size];
    if let Some(source) = bytes.get(offset..offset + size) {
        raw.copy_from_slice(source);
    }
    FieldValue::Opaque {
        type_tag: tag,
        bytes: raw,
    }
}

// =============================================================================
// Row access
// =============================================================================

impl World {
    /// The archetype id and row of a live entity, for component access.
    fn entity_row_location(&self, entity: Entity) -> Option<(ArchetypeId, usize)> {
        let location = self.entity_locations.get(&entity)?;
        Some((location.archetype_id, location.index_in_archetype))
    }

    /// Copy one entity's component row for `component_id` into an owned buffer.
    ///
    /// Works for native (Rust) and dynamic (foreign-language) columns alike.
    /// The caller owns the returned bytes, so no borrow outlives this call.
    fn copy_component_row_bytes(
        &self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
    ) -> Option<Vec<u8>> {
        let archetype = self.archetypes.get(&archetype_id)?;
        match component_id {
            ComponentId::Native(type_id) => {
                let column = archetype.component_storages.get_trait_storage(type_id)?;
                if row >= column.len() {
                    return None;
                }
                let element_size = column.elem_size();
                let mut buffer = vec![0u8; element_size];
                // SAFETY: the row is within the column (`row < len`) and the
                // buffer is exactly one element long; nothing mutates the
                // column while this shared borrow is alive.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        column.raw_ptr().add(row * element_size),
                        buffer.as_mut_ptr(),
                        element_size,
                    );
                }
                Some(buffer)
            }
            ComponentId::Dynamic(_) => archetype
                .dynamic_component_storages
                .get(&component_id)?
                .bytes(row)
                .map(<[u8]>::to_vec),
        }
    }

    /// Resolve a component's field layout as an owned list of descriptors.
    fn owned_field_layout(
        &self,
        component_id: ComponentId,
        component: &str,
    ) -> Result<Vec<ComponentFieldDescriptor>, ComponentFieldError> {
        let layout = self.component_field_layout(component_id).ok_or(
            ComponentFieldError::ComponentHasNoFieldLayout {
                component: component.to_string(),
            },
        )?;
        if layout.is_empty() {
            return Err(ComponentFieldError::ComponentHasNoFieldLayout {
                component: component.to_string(),
            });
        }
        Ok(layout.to_vec())
    }
}

// =============================================================================
// Reads
// =============================================================================

impl World {
    /// Read one field of one component on one entity.
    ///
    /// The component is resolved through the entity's own archetype (see
    /// [`Self::resolve_entity_component_id`]), so the returned value always
    /// describes data the entity really holds. Reads never mutate and never
    /// stamp a change tick.
    pub fn read_component_field(
        &self,
        entity: Entity,
        component_name: &str,
        field_name: &str,
    ) -> Result<FieldValue, ComponentFieldError> {
        // Step 1: Locate the entity and its component within its archetype.
        let (archetype_id, row) = self
            .entity_row_location(entity)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let component_id = self
            .resolve_entity_component_id(entity, component_name)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;

        // Step 2: Copy the layout and locate the requested field.
        let layout = self.owned_field_layout(component_id, component_name)?;
        let descriptor = layout
            .iter()
            .find(|descriptor| descriptor.name == field_name)
            .ok_or(ComponentFieldError::FieldNotFound {
                component: component_name.to_string(),
                field: field_name.to_string(),
            })?;

        // Step 3: Copy the row and decode the single field.
        let row_bytes = self
            .copy_component_row_bytes(archetype_id, row, component_id)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        Ok(decode_field(&row_bytes, descriptor))
    }

    /// Read every field of one component in layout order, for the inspector.
    ///
    /// Cheaper than N [`Self::read_component_field`] calls: the row is located
    /// and copied once.
    pub fn read_component_fields(
        &self,
        entity: Entity,
        component_name: &str,
    ) -> Result<Vec<FieldValue>, ComponentFieldError> {
        // Step 1: Locate the entity and its component within its archetype.
        let (archetype_id, row) = self
            .entity_row_location(entity)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let component_id = self
            .resolve_entity_component_id(entity, component_name)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;

        // Step 2: Copy the layout and the whole row once.
        let layout = self.owned_field_layout(component_id, component_name)?;
        let row_bytes = self
            .copy_component_row_bytes(archetype_id, row, component_id)
            .ok_or(ComponentFieldError::EntityNotFound)?;

        // Step 3: Decode every field from the one row image.
        Ok(layout
            .iter()
            .map(|descriptor| decode_field(&row_bytes, descriptor))
            .collect())
    }
}

// =============================================================================
// Writes
// =============================================================================

/// A validated byte payload and where in the component row it belongs.
struct FieldWrite {
    /// Byte offset of the payload within the component row.
    row_offset: usize,
    /// The payload bytes (a scalar, one array element, or a whole array).
    bytes: Vec<u8>,
}

/// Encode a validated whole-field write for a descriptor.
fn encode_field_write(
    descriptor: &ComponentFieldDescriptor,
    field: &str,
    value: &FieldValue,
) -> Result<FieldWrite, ComponentFieldError> {
    let tag = descriptor.type_tag;
    // A `struct:` field, or any other tag the engine cannot interpret, is
    // never writable through the generic path.
    if !is_scalar_tag(tag) && !tag.starts_with("array:") {
        return Err(ComponentFieldError::UnsupportedField {
            field: field.to_string(),
            reason: "the field's type is outside the editable vocabulary",
        });
    }
    if matches!(value, FieldValue::Opaque { .. }) {
        return Err(ComponentFieldError::TypeMismatch {
            field: field.to_string(),
            expected: tag,
            found: value.variant_tag(),
        });
    }
    if let Some(inner_tag) = tag.strip_prefix("array:") {
        let FieldValue::Array(values) = value else {
            return Err(ComponentFieldError::TypeMismatch {
                field: field.to_string(),
                expected: tag,
                found: value.variant_tag(),
            });
        };
        if !is_scalar_tag(inner_tag) {
            return Err(ComponentFieldError::UnsupportedField {
                field: field.to_string(),
                reason: "array elements are not scalar primitives",
            });
        }
        if values.len() != descriptor.element_count {
            return Err(ComponentFieldError::ArrayLengthMismatch {
                field: field.to_string(),
                expected: descriptor.element_count,
                found: values.len(),
            });
        }
        let mut bytes = Vec::with_capacity(descriptor.size);
        for element in values {
            if !encode_scalar(inner_tag, element, &mut bytes) {
                return Err(ComponentFieldError::TypeMismatch {
                    field: field.to_string(),
                    expected: inner_tag,
                    found: element.variant_tag(),
                });
            }
        }
        return Ok(FieldWrite {
            row_offset: descriptor.offset,
            bytes,
        });
    }

    // A scalar field: the value must carry the exact scalar tag.
    let mut bytes = Vec::new();
    if encode_scalar(tag, value, &mut bytes) {
        Ok(FieldWrite {
            row_offset: descriptor.offset,
            bytes,
        })
    } else {
        Err(ComponentFieldError::TypeMismatch {
            field: field.to_string(),
            expected: tag,
            found: value.variant_tag(),
        })
    }
}

impl World {
    /// Write one field of a live component.
    ///
    /// Resolves the component through the entity's own archetype, validates the
    /// value against the registered field layout, writes it through the column,
    /// and stamps that row's `ComponentTicks::changed` so `Changed<T>` systems
    /// observe the edit on their next run. This is the difference from
    /// `get_component_mut` (`world.rs:1224`), which does not stamp. Nothing is
    /// written when any validation fails.
    pub fn write_component_field(
        &mut self,
        entity: Entity,
        component_name: &str,
        field_name: &str,
        value: FieldValue,
    ) -> Result<(), ComponentFieldError> {
        // Step 1: Resolve, copy the layout, and find the field.
        let (archetype_id, row) = self
            .entity_row_location(entity)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let component_id = self
            .resolve_entity_component_id(entity, component_name)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;
        let layout = self.owned_field_layout(component_id, component_name)?;
        let descriptor = layout
            .iter()
            .find(|descriptor| descriptor.name == field_name)
            .ok_or(ComponentFieldError::FieldNotFound {
                component: component_name.to_string(),
                field: field_name.to_string(),
            })?;

        // Step 2: Validate and encode the payload BEFORE touching any column,
        // so a refused edit leaves the world untouched.
        let write = encode_field_write(descriptor, field_name, &value)?;

        // Step 3: Write the payload and stamp the row's changed tick, each in
        // its own archetype borrow so column and tick lookups never overlap.
        //
        // The world tick is bumped first so the stamped `changed` value is
        // strictly newer than every system's `last_run` baseline. Edits are
        // applied between frames, when the tick is otherwise frozen; without
        // the bump a `Changed<T>` filter would compare the new value against
        // an equal baseline and never observe the edit.
        let current_tick = self.increment_change_tick();
        self.apply_row_write(
            archetype_id,
            row,
            component_id,
            write.row_offset,
            &write.bytes,
        )?;
        self.stamp_row_changed(archetype_id, row, component_id, current_tick)
    }

    /// Build a zero-initialised component image for a registered type name,
    /// then write the supplied field values over it.
    ///
    /// Used by the editor to create entities or add components from field
    /// values without knowing the concrete Rust type. Creation has no entity
    /// yet, so the component is resolved through the registry (this is the one
    /// editor path where the registry-wide name resolver is correct).
    /// Zero-initialisation is only valid for components with a non-empty
    /// registered field layout, which this enforces.
    pub fn build_component_image(
        &self,
        component_name: &str,
        fields: &[(String, FieldValue)],
    ) -> Result<Vec<u8>, ComponentFieldError> {
        let component_id = self
            .resolve_component_id_by_name_any(component_name)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;
        let layout = self.owned_field_layout(component_id, component_name)?;
        let (size, _) = self.component_layout(component_id).ok_or(
            ComponentFieldError::ComponentHasNoFieldLayout {
                component: component_name.to_string(),
            },
        )?;
        let mut image = vec![0u8; size];
        for (field_name, value) in fields {
            let descriptor = layout
                .iter()
                .find(|descriptor| descriptor.name == *field_name)
                .ok_or(ComponentFieldError::FieldNotFound {
                    component: component_name.to_string(),
                    field: field_name.clone(),
                })?;
            let write = encode_field_write(descriptor, field_name, value)?;
            let target = image
                .get_mut(write.row_offset..write.row_offset + write.bytes.len())
                .ok_or(ComponentFieldError::UnsupportedField {
                    field: field_name.clone(),
                    reason: "write extends past the component image",
                })?;
            target.copy_from_slice(&write.bytes);
        }
        Ok(image)
    }

    /// Write one element of a fixed-size array field.
    pub fn write_component_array_element(
        &mut self,
        entity: Entity,
        component_name: &str,
        field_name: &str,
        index: usize,
        value: FieldValue,
    ) -> Result<(), ComponentFieldError> {
        // Step 1: Resolve, copy the layout, and find the field.
        let (archetype_id, row) = self
            .entity_row_location(entity)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let component_id = self
            .resolve_entity_component_id(entity, component_name)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;
        let layout = self.owned_field_layout(component_id, component_name)?;
        let descriptor = layout
            .iter()
            .find(|descriptor| descriptor.name == field_name)
            .ok_or(ComponentFieldError::FieldNotFound {
                component: component_name.to_string(),
                field: field_name.to_string(),
            })?;

        // Step 2: The field must be a fixed array of scalar primitives.
        let Some(inner_tag) = descriptor.type_tag.strip_prefix("array:") else {
            return Err(ComponentFieldError::TypeMismatch {
                field: field_name.to_string(),
                expected: descriptor.type_tag,
                found: value.variant_tag(),
            });
        };
        if !is_scalar_tag(inner_tag) {
            return Err(ComponentFieldError::UnsupportedField {
                field: field_name.to_string(),
                reason: "array elements are not scalar primitives",
            });
        }
        if index >= descriptor.element_count {
            return Err(ComponentFieldError::ArrayIndexOutOfRange {
                field: field_name.to_string(),
                index,
                length: descriptor.element_count,
            });
        }
        let element_size = scalar_size(inner_tag).ok_or(ComponentFieldError::UnsupportedField {
            field: field_name.to_string(),
            reason: "array element size is unknown",
        })?;
        let mut bytes = Vec::with_capacity(element_size);
        if !encode_scalar(inner_tag, &value, &mut bytes) {
            return Err(ComponentFieldError::TypeMismatch {
                field: field_name.to_string(),
                expected: inner_tag,
                found: value.variant_tag(),
            });
        }

        // Step 3: Write the element and stamp the row's changed tick. The tick
        // is bumped first for the same reason the whole-field write bumps it:
        // a between-frame edit must be visible to `Changed<T>` filters.
        let row_offset = descriptor.offset + index * element_size;
        let current_tick = self.increment_change_tick();
        self.apply_row_write(archetype_id, row, component_id, row_offset, &bytes)?;
        self.stamp_row_changed(archetype_id, row, component_id, current_tick)
    }

    /// Overwrite `bytes.len()` bytes at `row_offset` of one component row.
    fn apply_row_write(
        &mut self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        row_offset: usize,
        bytes: &[u8],
    ) -> Result<(), ComponentFieldError> {
        match component_id {
            ComponentId::Native(type_id) => {
                let archetype = self
                    .archetypes
                    .get_mut(&archetype_id)
                    .ok_or(ComponentFieldError::EntityNotFound)?;
                let element_size;
                let destination;
                {
                    let column = archetype
                        .component_storages
                        .get_trait_storage_mut(type_id)
                        .ok_or(ComponentFieldError::ComponentNotFound {
                            component: String::new(),
                        })?;
                    if row >= column.len() {
                        return Err(ComponentFieldError::EntityNotFound);
                    }
                    element_size = column.elem_size();
                    // SAFETY: `row < column.len()` was checked above, so the
                    // pointer stays inside the column's allocation, and the
                    // column borrow keeps that allocation alive through this
                    // statement.
                    destination = unsafe { column.as_mut_ptr().add(row * element_size) };
                }
                if row_offset + bytes.len() > element_size {
                    return Err(ComponentFieldError::UnsupportedField {
                        field: String::new(),
                        reason: "write extends past the component row",
                    });
                }
                // SAFETY: the destination row is initialized (`row < len`) and
                // the payload fits within it (checked above); the column is not
                // reallocated while this borrow is held.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        destination.add(row_offset),
                        bytes.len(),
                    );
                }
                Ok(())
            }
            ComponentId::Dynamic(_) => {
                let archetype = self
                    .archetypes
                    .get_mut(&archetype_id)
                    .ok_or(ComponentFieldError::EntityNotFound)?;
                let element_size;
                let mut row_bytes;
                {
                    let column = archetype
                        .dynamic_component_storages
                        .get_mut(&component_id)
                        .ok_or(ComponentFieldError::ComponentNotFound {
                            component: String::new(),
                        })?;
                    element_size = column.element_size();
                    if row >= column.len() {
                        return Err(ComponentFieldError::EntityNotFound);
                    }
                    row_bytes = column
                        .bytes(row)
                        .ok_or(ComponentFieldError::EntityNotFound)?
                        .to_vec();
                }
                if row_offset + bytes.len() > element_size {
                    return Err(ComponentFieldError::UnsupportedField {
                        field: String::new(),
                        reason: "write extends past the component row",
                    });
                }
                let target = row_bytes
                    .get_mut(row_offset..row_offset + bytes.len())
                    .ok_or(ComponentFieldError::UnsupportedField {
                        field: String::new(),
                        reason: "write extends past the component row",
                    })?;
                target.copy_from_slice(bytes);
                let archetype = self
                    .archetypes
                    .get_mut(&archetype_id)
                    .ok_or(ComponentFieldError::EntityNotFound)?;
                let column = archetype
                    .dynamic_component_storages
                    .get_mut(&component_id)
                    .ok_or(ComponentFieldError::ComponentNotFound {
                        component: String::new(),
                    })?;
                column
                    .set_bytes(row, &row_bytes)
                    .map_err(|_| ComponentFieldError::EntityNotFound)
            }
        }
    }

    /// Stamp one row's `changed` tick so `Changed<T>` systems react.
    fn stamp_row_changed(
        &mut self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        tick: Tick,
    ) -> Result<(), ComponentFieldError> {
        let archetype = self
            .archetypes
            .get_mut(&archetype_id)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let ticks = archetype.component_ticks.get_mut(&component_id).ok_or(
            ComponentFieldError::ComponentNotFound {
                component: String::new(),
            },
        )?;
        let row_ticks = ticks
            .get_mut(row)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        row_ticks.set_changed(tick);
        Ok(())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;

    /// `repr(C)` pins the field offsets the hand-written layout below
    /// describes; a plain `repr(Rust)` struct may reorder fields.
    #[repr(C)]
    #[derive(Debug, Clone, PartialEq)]
    struct SampleComponent {
        a_float: f32,
        an_int: u32,
        a_bool: bool,
        an_array: [i16; 3],
    }
    impl Component for SampleComponent {}
    trait_type_map::impl_trait_accessible!(dyn Component; SampleComponent);

    // A hand-rolled registration mirroring what #[derive(PillComponent)] emits.
    static SAMPLE_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "a_float",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        },
        ComponentFieldDescriptor {
            name: "an_int",
            type_tag: "u32",
            offset: 4,
            size: 4,
            align: 4,
            element_count: 0,
        },
        ComponentFieldDescriptor {
            name: "a_bool",
            type_tag: "bool",
            offset: 8,
            size: 1,
            align: 1,
            element_count: 0,
        },
        // In `#[repr(C)]` the `[i16; 3]` follows the `bool` at offset 8 and
        // only needs 2-byte alignment, so it starts at offset 10.
        ComponentFieldDescriptor {
            name: "an_array",
            type_tag: "array:i16",
            offset: 10,
            size: 6,
            align: 2,
            element_count: 3,
        },
    ];

    fn test_world() -> World {
        let mut world = World::new();
        world.register_component_with_layout::<SampleComponent>(SAMPLE_FIELDS);
        world
    }

    fn sample_value() -> SampleComponent {
        SampleComponent {
            a_float: 1.5,
            an_int: 7,
            a_bool: true,
            an_array: [1, 2, 3],
        }
    }

    #[test]
    fn round_trip_every_scalar_field() {
        let mut world = test_world();
        let entity = world
            .create_entity()
            .with(sample_value())
            .build()
            .expect("entity builds");

        assert_eq!(
            world.read_component_field(
                entity,
                "pill_engine::component_field::tests::SampleComponent",
                "a_float"
            ),
            Ok(FieldValue::F32(1.5))
        );
        assert_eq!(
            world.read_component_field(
                entity,
                "pill_engine::component_field::tests::SampleComponent",
                "an_int"
            ),
            Ok(FieldValue::U32(7))
        );
        assert_eq!(
            world.read_component_field(
                entity,
                "pill_engine::component_field::tests::SampleComponent",
                "a_bool"
            ),
            Ok(FieldValue::Bool(true))
        );
        assert_eq!(
            world.read_component_field(
                entity,
                "pill_engine::component_field::tests::SampleComponent",
                "an_array"
            ),
            Ok(FieldValue::Array(vec![
                FieldValue::I16(1),
                FieldValue::I16(2),
                FieldValue::I16(3),
            ]))
        );

        world
            .write_component_field(
                entity,
                "pill_engine::component_field::tests::SampleComponent",
                "a_float",
                FieldValue::F32(9.25),
            )
            .expect("write succeeds");
        world
            .write_component_array_element(
                entity,
                "pill_engine::component_field::tests::SampleComponent",
                "an_array",
                1,
                FieldValue::I16(-5),
            )
            .expect("element write succeeds");

        let component = world
            .get_component::<SampleComponent>(entity)
            .expect("component still attached");
        assert_eq!(component.a_float, 9.25);
        assert_eq!(component.an_array, [1, -5, 3]);
    }

    #[test]
    fn refusing_errors_leave_state_untouched() {
        let mut world = test_world();
        let entity = world
            .create_entity()
            .with(sample_value())
            .build()
            .expect("entity builds");
        let name = "pill_engine::component_field::tests::SampleComponent";

        assert_eq!(
            world.write_component_field(entity, name, "an_int", FieldValue::F32(1.0)),
            Err(ComponentFieldError::TypeMismatch {
                field: "an_int".to_string(),
                expected: "u32",
                found: "f32",
            })
        );
        assert_eq!(
            world.write_component_field(entity, name, "an_array", FieldValue::Array(vec![])),
            Err(ComponentFieldError::ArrayLengthMismatch {
                field: "an_array".to_string(),
                expected: 3,
                found: 0,
            })
        );
        assert_eq!(
            world.write_component_array_element(entity, name, "an_array", 5, FieldValue::I16(1)),
            Err(ComponentFieldError::ArrayIndexOutOfRange {
                field: "an_array".to_string(),
                index: 5,
                length: 3,
            })
        );

        // Nothing above was written.
        let component = world.get_component::<SampleComponent>(entity).unwrap();
        assert_eq!(component.an_int, 7);
        assert_eq!(component.an_array, [1, 2, 3]);
    }

    #[test]
    fn dead_entity_and_unknown_component_and_field() {
        let mut world = test_world();
        let entity = world
            .create_entity()
            .with(sample_value())
            .build()
            .expect("entity builds");
        let name = "pill_engine::component_field::tests::SampleComponent";

        let _ = world.destroy_entity(entity);
        assert_eq!(
            world.read_component_field(entity, name, "a_float"),
            Err(ComponentFieldError::EntityNotFound)
        );

        let entity = world
            .create_entity()
            .with(sample_value())
            .build()
            .expect("entity builds");
        assert_eq!(
            world.read_component_field(entity, "missing::Type", "a_float"),
            Err(ComponentFieldError::ComponentNotFound {
                component: "missing::Type".to_string(),
            })
        );
        assert_eq!(
            world.read_component_field(entity, name, "missing_field"),
            Err(ComponentFieldError::FieldNotFound {
                component: name.to_string(),
                field: "missing_field".to_string(),
            })
        );
    }

    #[test]
    fn reads_do_not_stamp_but_writes_do() {
        let mut world = test_world();
        let entity = world
            .create_entity()
            .with(sample_value())
            .build()
            .expect("entity builds");
        let name = "pill_engine::component_field::tests::SampleComponent";
        let before = world.change_tick();

        let _ = world.read_component_field(entity, name, "a_float").unwrap();
        let ticks = &world.archetypes.values().next().unwrap().component_ticks;
        let component_id = ComponentId::of::<SampleComponent>();
        let row_ticks = ticks.get(&component_id).unwrap()[0];
        assert!(!row_ticks.changed.is_newer_than(before, world.change_tick()));

        world
            .write_component_field(entity, name, "a_float", FieldValue::F32(2.0))
            .expect("write succeeds");
        let ticks = &world.archetypes.values().next().unwrap().component_ticks;
        let row_ticks = ticks.get(&component_id).unwrap()[0];
        assert!(row_ticks.changed.is_newer_than(before, world.change_tick()));
    }

    #[test]
    fn changed_filter_observes_a_field_edit() {
        use crate::query::{Changed, Query};

        let mut world = test_world();
        let entity = world
            .create_entity()
            .with(sample_value())
            .build()
            .expect("entity builds");
        let name = "pill_engine::component_field::tests::SampleComponent";

        // Step 1: set the baseline so the creation-time `added` tick is past.
        world.set_system_last_run(world.change_tick());

        // Step 2: edit one field through the generic API; this must stamp the
        // row's `changed` tick.
        world
            .write_component_field(entity, name, "an_int", FieldValue::U32(42))
            .expect("write succeeds");

        // Step 3: a Changed<T> filter must yield exactly the edited entity.
        let mut query =
            Query::<(crate::entity::Entity,), Changed<SampleComponent>>::new(&mut world);
        let hits: Vec<crate::entity::Entity> = query.iter_mut().map(|(e,)| e).collect();
        assert_eq!(hits, vec![entity]);
    }
}

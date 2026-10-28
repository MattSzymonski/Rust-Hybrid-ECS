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
//! field layout (`name`, `type_tag`, `offset`, `size`, `align`,
//! `element_count`) that `#[derive(PillComponent)]` / `#[derive(PillMirror)]`
//! register is the only generic way to reach an arbitrary component's value.
//! This module is the safe wrapper over that data: it resolves a component
//! through **the entity's own archetype**, the one place that names the
//! generation which created the data (a registry-wide name lookup can see
//! several registrations under one name), validates every value against the
//! layout, and never hands a raw pointer to the caller.
//!
//! The [`FieldValue`] vocabulary covers the derive's closed `type_tag`
//! vocabulary. `struct:` fields (and anything else the engine cannot
//! interpret) are surfaced as [`FieldValue::Opaque`] on read and are never
//! edited by value, so an inspector can show them instead of hiding them; a
//! restore hands the captured bytes back exactly as they were read. A nested
//! `struct:` field whose type has a known layout never gets that far: the
//! world flattens it into dotted leaf rows at registration
//! ([`World::register_component_with_layout`]), so the tag survives only when
//! no layout was resolvable.
//!
//! # Row byte convention
//!
//! A row's bytes are native-endian, including an asset handle's two `u32`s
//! (index, then generation) and an entity reference's slot and generation. The
//! C# mirror reads the same row from the same process, so no byte order has to
//! cross a machine boundary; a field that ever does would need its own stated
//! convention here.

// Standard library
use std::mem::size_of;
use std::sync::OnceLock;

// Current crate
use crate::archetype::{ArchetypeId, ComponentColumn};
use crate::component::{ComponentId, Tick};
use crate::component_registry::{ComponentFieldDescriptor, FieldDecorator};
use crate::entity::Entity;
use crate::world::World;

// =============================================================================
// Value vocabulary
// =============================================================================

/// One read or edited value in the engine's reflected vocabulary.
///
/// Covers the derive's `type_tag` vocabulary: the scalar primitives the engine
/// can describe, whole fixed-size arrays, heap-backed containers (`List` and
/// `Text`, summarized rather than materialized), handle and entity references,
/// and an `Opaque` escape hatch for fields the engine can locate but not
/// interpret (`struct:` tags whose value type is not resolvable, and any tag
/// outside the vocabulary). `Opaque` carries the raw bytes so the UI can show
/// them; no edit can introduce one, but a captured payload whose tag and size
/// match the field's own descriptor writes back verbatim, which is how a
/// restore re-creates a destroyed row. `List` and `Text` are read-only through
/// this path — writes go through the container field's generated accessors,
/// which reach the live buffer instead of rebuilding the row.
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
    /// A heap-backed sequence field (`vec:<element_tag>`), summarized by its
    /// element count rather than materialized: a component `Vec` may hold
    /// millions of elements, and an inspector showing the length is what makes
    /// it useful. Read-only here; use the field's accessors to edit elements.
    List {
        /// Element type tag from the closed vocabulary (`f32`, `struct:path`).
        element_tag: &'static str,
        /// Number of live elements in the buffer right now.
        element_count: usize,
    },
    /// A heap-backed UTF-8 string field (`string`), decoded for display.
    /// Read-only here; use the field's accessors to replace its contents.
    Text(String),
    /// A reference to an asset: a `Handle<T>` field, as its slot index and
    /// generation, with the asset type the field was registered with.
    ///
    /// Writable like a scalar; a write naming another asset type than the
    /// field's is refused. `asset_type` is the type's registered name (the
    /// one `.meta` files and the import registry use), `None` when the field
    /// carries no record of it. A cleared reference is
    /// [`Handle::INVALID`](crate::Handle::INVALID)'s pair, `u32::MAX` twice.
    Asset {
        /// The registered name of the referenced asset's type.
        asset_type: Option<String>,
        /// The handle's slot index.
        index: u32,
        /// The handle's generation.
        generation: u32,
    },
    /// A reference to another entity: an `Entity` field, as the slot id and
    /// generation the handle carries.
    ///
    /// Writable like a scalar. The pair is the *handle*, so it resolves only
    /// in the world that issued it; what a reference saved for a later run
    /// carries is the entity's [`EntityId`](crate::EntityId), which the writer
    /// looks up and the loader resolves back to a handle.
    Entity {
        /// The referenced entity's slot id.
        id: u64,
        /// The slot's generation, which tells a live entity from a recycled
        /// slot's previous occupant.
        generation: u32,
    },
    /// A field the engine can locate but not interpret. Never writable.
    Opaque {
        /// The registered `type_tag` of the field (for example `struct:path`).
        type_tag: &'static str,
        /// The raw bytes of the field, copied out of the component row.
        bytes: Vec<u8>,
    },
}

impl FieldValue {
    /// The cleared-reference value: [`Handle::INVALID`](crate::Handle::INVALID)'s
    /// pair, with no recorded asset type.
    ///
    /// The field layer's one spelling of "this reference points at nothing",
    /// so a saver, a restorer and a clearer agree without re-deriving it.
    #[must_use]
    pub fn unset_asset() -> Self {
        Self::Asset {
            asset_type: None,
            index: crate::asset::INVALID_ASSET_INDEX,
            generation: crate::asset::INVALID_ASSET_GENERATION,
        }
    }

    /// Whether this value is a reference that points at nothing.
    ///
    /// True for a cleared asset reference ([`Self::unset_asset`]) whatever
    /// wrote it, and for the null entity handle.
    #[must_use]
    pub fn references_nothing(&self) -> bool {
        match self {
            Self::Asset {
                index, generation, ..
            } => {
                (*index, *generation)
                    == (
                        crate::asset::INVALID_ASSET_INDEX,
                        crate::asset::INVALID_ASSET_GENERATION,
                    )
            }
            Self::Entity { id, generation } => crate::Entity::from_raw(*id, *generation).is_null(),
            _ => false,
        }
    }

    /// The closed-vocabulary tag a value's variant corresponds to.
    ///
    /// Mirrors the `type_tag` strings the derive emits, so a mismatch can name
    /// both sides. `Array` and `Opaque` are not derive tags; they use the
    /// labels below for diagnostics.
    pub(crate) fn variant_tag(&self) -> &'static str {
        match self {
            Self::F32(_) => ScalarKind::F32.tag(),
            Self::F64(_) => ScalarKind::F64.tag(),
            Self::I8(_) => ScalarKind::I8.tag(),
            Self::I16(_) => ScalarKind::I16.tag(),
            Self::I32(_) => ScalarKind::I32.tag(),
            Self::I64(_) => ScalarKind::I64.tag(),
            Self::U8(_) => ScalarKind::U8.tag(),
            Self::U16(_) => ScalarKind::U16.tag(),
            Self::U32(_) => ScalarKind::U32.tag(),
            Self::U64(_) => ScalarKind::U64.tag(),
            Self::Bool(_) => ScalarKind::Bool.tag(),
            Self::Usize(_) => ScalarKind::Usize.tag(),
            Self::Isize(_) => ScalarKind::Isize.tag(),
            Self::Array(_) => "array",
            Self::List { .. } => "list",
            Self::Text(_) => "text",
            Self::Asset { .. } => "asset",
            Self::Entity { .. } => "entity",
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
    /// The component was resolved through a live entity, but its column or row
    /// is missing from the archetype.
    ///
    /// This is a registration/storage desync, not a dead handle: the entity
    /// exists and it carries the component's id, so the storage the id names
    /// should be there.
    ComponentStorageMissing {
        /// The component type name.
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
    /// An asset reference write named another asset type than the field's.
    AssetTypeMismatch {
        /// The field being edited.
        field: String,
        /// The asset type the field references.
        expected: String,
        /// The asset type the caller supplied.
        found: String,
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

/// Whether a field's type tag names a heap-backed container.
///
/// `vec:<tag>` and `dynbuf:<tag>` describe a `(pointer, length, capacity)`
/// header and `string` the same bytes with UTF-8 meaning; all three are
/// decoded by following a pointer the row is expected to own. A descriptor row
/// is raw bytes with no Rust value in it, so a layout claiming one of these
/// tags on such a row promises a header that is not there.
pub(crate) fn is_container_tag(type_tag: &str) -> bool {
    type_tag == "string" || type_tag.starts_with("vec:") || type_tag.starts_with("dynbuf:")
}

impl ComponentFieldError {
    /// A short, stable diagnostic for console surfacing.
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::EntityNotFound => "entity is not alive".to_string(),
            Self::ComponentNotFound { component } => {
                format!("component `{component}` is not on this entity")
            }
            Self::ComponentStorageMissing { component } => {
                format!("component `{component}` has no storage column or row in the archetype")
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
            Self::AssetTypeMismatch {
                field,
                expected,
                found,
            } => format!("field `{field}` references a `{expected}` but received a `{found}`"),
            Self::UnsupportedField { field, reason } => {
                format!("field `{field}` is not editable: {reason}")
            }
        }
    }
}

// =============================================================================
// Tag vocabulary helpers
// =============================================================================

/// Whether a type tag is a `Handle<T>` field's (`struct:Handle<Mesh>`, or
/// `struct:Handle` when the derive could not read the type argument).
pub(crate) fn is_handle_tag(tag: &str) -> bool {
    tag == "struct:Handle" || tag.starts_with("struct:Handle<")
}

/// Whether a type tag is an `Entity` field's.
pub(crate) fn is_entity_tag(tag: &str) -> bool {
    tag == "entity"
}

/// The byte size of a handle: its `u32` index and `u32` generation.
const HANDLE_SIZE: usize = 8;

/// The byte size of an entity reference: the `u64` slot id, the `u32`
/// generation, and the padding `#[repr(C)]` puts after them.
const ENTITY_REFERENCE_SIZE: usize = size_of::<Entity>();

/// The scalar primitives the field vocabulary names.
///
/// One entry per tag, so the size table, both codecs and the derive's tag
/// strings cannot drift apart: adding a scalar here is what makes it readable
/// and writable, and the rest of this file asks the kind instead of repeating
/// the tag strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarKind {
    /// A 32-bit float.
    F32,
    /// A 64-bit float.
    F64,
    /// An 8-bit signed integer.
    I8,
    /// A 16-bit signed integer.
    I16,
    /// A 32-bit signed integer.
    I32,
    /// A 64-bit signed integer.
    I64,
    /// An 8-bit unsigned integer.
    U8,
    /// A 16-bit unsigned integer.
    U16,
    /// A 32-bit unsigned integer.
    U32,
    /// A 64-bit unsigned integer.
    U64,
    /// A boolean, one byte.
    Bool,
    /// A pointer-sized unsigned integer.
    Usize,
    /// A pointer-sized signed integer.
    Isize,
}

impl ScalarKind {
    /// The kind a tag names, or `None` when the tag is not a scalar.
    fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "f32" => Self::F32,
            "f64" => Self::F64,
            "i8" => Self::I8,
            "i16" => Self::I16,
            "i32" => Self::I32,
            "i64" => Self::I64,
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "bool" => Self::Bool,
            "usize" => Self::Usize,
            "isize" => Self::Isize,
            _ => return None,
        })
    }

    /// The tag this kind is named by, as the derive emits it.
    fn tag(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::Bool => "bool",
            Self::Usize => "usize",
            Self::Isize => "isize",
        }
    }

    /// The size of one value in bytes.
    fn size(self) -> usize {
        match self {
            Self::F32 | Self::I32 | Self::U32 => 4,
            Self::F64 | Self::I64 | Self::U64 => 8,
            Self::I8 | Self::U8 | Self::Bool => 1,
            Self::I16 | Self::U16 => 2,
            // Pointer-sized: 4 bytes on wasm32, 8 on 64-bit targets.
            Self::Usize => size_of::<usize>(),
            Self::Isize => size_of::<isize>(),
        }
    }

    /// Append one value, reporting whether it is of this kind.
    fn encode(self, value: &FieldValue, output: &mut Vec<u8>) -> bool {
        match (self, value) {
            (Self::F32, FieldValue::F32(value)) => output.extend_from_slice(&value.to_ne_bytes()),
            (Self::F64, FieldValue::F64(value)) => output.extend_from_slice(&value.to_ne_bytes()),
            (Self::I8, FieldValue::I8(value)) => output.push(*value as u8),
            (Self::I16, FieldValue::I16(value)) => output.extend_from_slice(&value.to_ne_bytes()),
            (Self::I32, FieldValue::I32(value)) => output.extend_from_slice(&value.to_ne_bytes()),
            (Self::I64, FieldValue::I64(value)) => output.extend_from_slice(&value.to_ne_bytes()),
            (Self::U8, FieldValue::U8(value)) => output.push(*value),
            (Self::U16, FieldValue::U16(value)) => output.extend_from_slice(&value.to_ne_bytes()),
            (Self::U32, FieldValue::U32(value)) => output.extend_from_slice(&value.to_ne_bytes()),
            (Self::U64, FieldValue::U64(value)) => output.extend_from_slice(&value.to_ne_bytes()),
            (Self::Bool, FieldValue::Bool(value)) => output.push(u8::from(*value)),
            (Self::Usize, FieldValue::Usize(value)) => {
                output.extend_from_slice(&value.to_ne_bytes())
            }
            (Self::Isize, FieldValue::Isize(value)) => {
                output.extend_from_slice(&value.to_ne_bytes())
            }
            _ => return false,
        }
        true
    }

    /// Decode one value at `offset`, or `None` when the buffer is too short.
    fn decode(self, bytes: &[u8], offset: usize) -> Option<FieldValue> {
        fn read<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
            bytes.get(offset..offset + N)?.try_into().ok()
        }
        Some(match self {
            Self::F32 => FieldValue::F32(f32::from_ne_bytes(read::<4>(bytes, offset)?)),
            Self::F64 => FieldValue::F64(f64::from_ne_bytes(read::<8>(bytes, offset)?)),
            Self::I8 => FieldValue::I8(*bytes.get(offset)? as i8),
            Self::I16 => FieldValue::I16(i16::from_ne_bytes(read::<2>(bytes, offset)?)),
            Self::I32 => FieldValue::I32(i32::from_ne_bytes(read::<4>(bytes, offset)?)),
            Self::I64 => FieldValue::I64(i64::from_ne_bytes(read::<8>(bytes, offset)?)),
            Self::U8 => FieldValue::U8(*bytes.get(offset)?),
            Self::U16 => FieldValue::U16(u16::from_ne_bytes(read::<2>(bytes, offset)?)),
            Self::U32 => FieldValue::U32(u32::from_ne_bytes(read::<4>(bytes, offset)?)),
            Self::U64 => FieldValue::U64(u64::from_ne_bytes(read::<8>(bytes, offset)?)),
            Self::Bool => FieldValue::Bool(bytes.get(offset).copied()? != 0),
            Self::Usize => FieldValue::Usize(usize::from_ne_bytes(read::<{ size_of::<usize>() }>(
                bytes, offset,
            )?)),
            Self::Isize => FieldValue::Isize(isize::from_ne_bytes(read::<{ size_of::<isize>() }>(
                bytes, offset,
            )?)),
        })
    }
}

/// The size in bytes of one scalar whose tag is in the closed vocabulary.
fn scalar_size(tag: &str) -> Option<usize> {
    ScalarKind::from_tag(tag).map(ScalarKind::size)
}

/// Whether a tag names one of the scalar primitives.
fn is_scalar_tag(tag: &str) -> bool {
    ScalarKind::from_tag(tag).is_some()
}

/// Encode one scalar value into `output`, returning whether the tag matched.
fn encode_scalar(tag: &str, value: &FieldValue, output: &mut Vec<u8>) -> bool {
    ScalarKind::from_tag(tag).is_some_and(|kind| kind.encode(value, output))
}

/// Decode one scalar from a byte buffer at `offset`, or `None` when the tag is
/// not a scalar.
fn decode_scalar(bytes: &[u8], offset: usize, tag: &str) -> Option<FieldValue> {
    ScalarKind::from_tag(tag)?.decode(bytes, offset)
}

/// The number of bytes one fixed-array element occupies, from the descriptor.
fn array_element_size(descriptor: &ComponentFieldDescriptor) -> Option<usize> {
    if descriptor.element_count == 0 {
        return None;
    }
    Some(descriptor.size / descriptor.element_count)
}

/// Read one machine word out of a row image.
fn header_word(bytes: &[u8], at: usize) -> Option<usize> {
    Some(usize::from_ne_bytes(
        bytes.get(at..at + size_of::<usize>())?.try_into().ok()?,
    ))
}

/// Byte offsets of the pointer and the length inside a `Vec`/`String` header.
///
/// The standard library gives `Vec` - and `String`, a `Vec<u8>` newtype - no
/// `repr(C)`, so their field order is the compiler's to choose and it is not
/// pointer-first on this build. The offsets are measured once from a live value
/// rather than assumed: an assumed word is either the length, dereferenced as
/// an address, or the capacity, read as a count.
fn std_container_offsets() -> (usize, usize) {
    static OFFSETS: OnceLock<(usize, usize)> = OnceLock::new();
    *OFFSETS.get_or_init(|| {
        // Pointer, capacity and length all differ, so each word is
        // recognisable: 12 bytes held in a 64-byte allocation.
        let mut text = String::with_capacity(64);
        text.push_str("header probe");
        let words = header_words_of(&text);
        let pointer_index = words
            .iter()
            .position(|&word| word == text.as_ptr() as usize)
            .expect("the pointer is one of a string's three words");
        let length_index = words
            .iter()
            .position(|&word| word == text.len())
            .expect("the length is one of a string's three words");
        assert_ne!(
            pointer_index, length_index,
            "a string's pointer and length must occupy different words"
        );
        (
            pointer_index * size_of::<usize>(),
            length_index * size_of::<usize>(),
        )
    })
}

/// The three header words of a `String`, in the order the compiler laid them
/// out.
fn header_words_of(text: &String) -> [usize; 3] {
    assert_eq!(
        size_of::<String>(),
        3 * size_of::<usize>(),
        "a string header is three words; the field layer reads it as one"
    );
    // SAFETY: the size was just checked, so three words cover the value
    // exactly, and `transmute_copy` reads it without moving it.
    unsafe { std::mem::transmute_copy(text) }
}

/// Read the `(pointer, length)` header of a std heap container field.
///
/// `vec:` and `string` tags reach here, and only from a row copied out of a
/// live column. Fixed-array fields keep going through
/// `element_count`/`element_size` instead.
fn container_header(bytes: &[u8], offset: usize) -> Option<(*const u8, usize)> {
    let (pointer_at, length_at) = std_container_offsets();
    let pointer = header_word(bytes, offset + pointer_at)? as *const u8;
    let length = header_word(bytes, offset + length_at)?;
    Some((pointer, length))
}

/// Read the `(pointer, length)` header of an engine-owned native buffer field.
///
/// `DynamicBuffer` pins that order with `repr(C)` - the C# mirror reads the same
/// three words straight out of the row - so unlike [`container_header`] this one
/// can name the offsets.
fn native_buffer_header(bytes: &[u8], offset: usize) -> Option<(*const u8, usize)> {
    let pointer = header_word(bytes, offset)? as *const u8;
    let length = header_word(bytes, offset + size_of::<usize>())?;
    Some((pointer, length))
}

/// Decode one field (scalar, whole array, heap container, or opaque) from a
/// row image.
fn decode_field(bytes: &[u8], descriptor: &ComponentFieldDescriptor) -> FieldValue {
    // Heap-owning containers are summarized rather than materialized. Both are
    // read-only through this path; writes go through the field's generated
    // accessors, which reach the live buffer in place.
    //
    // The two kinds read their count from different headers: a `vec:` field is
    // a std `Vec`, whose word order the compiler chooses (see
    // [`std_container_offsets`]), and a `dynbuf:` field is a `DynamicBuffer`,
    // whose `repr(C)` order the engine owns.
    if let Some(element_tag) = descriptor.type_tag.strip_prefix("vec:") {
        let element_count = container_header(bytes, descriptor.offset)
            .map(|(_, length)| length)
            .unwrap_or(0);
        return FieldValue::List {
            element_tag,
            element_count,
        };
    }
    if let Some(element_tag) = descriptor.type_tag.strip_prefix("dynbuf:") {
        let element_count = native_buffer_header(bytes, descriptor.offset)
            .map(|(_, length)| length)
            .unwrap_or(0);
        return FieldValue::List {
            element_tag,
            element_count,
        };
    }
    if descriptor.type_tag == "string" {
        let text = match container_header(bytes, descriptor.offset) {
            // A zero-length string may carry the dangling-but-aligned pointer
            // `String` uses for its empty state; reading nothing from it is
            // fine, and the null check keeps that explicit.
            Some((pointer, length)) if !pointer.is_null() => {
                // SAFETY: the header was copied out of a live component row,
                // so the buffer it describes is live for this call too.
                let slice = unsafe { std::slice::from_raw_parts(pointer, length) };
                String::from_utf8_lossy(slice).into_owned()
            }
            _ => String::new(),
        };
        return FieldValue::Text(text);
    }
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

/// Format a copied component row for native debugger helpers.
///
/// This accepts a byte slice rather than a component type, so it works for
/// Rust, renderer, and managed descriptor columns alike when paired with the
/// registered field layout.
pub fn format_component_debug(fields: &[ComponentFieldDescriptor], bytes: &[u8]) -> String {
    fields
        .iter()
        .map(|field| format!("{}={:?}", field.name, decode_field(bytes, field)))
        .collect::<Vec<_>>()
        .join(", ")
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
    // A handle is two `u32`s; the asset type is the world's record of the
    // field, filled in by the caller that knows the component.
    if is_handle_tag(tag) && size == HANDLE_SIZE {
        let index = decode_scalar(bytes, offset, "u32");
        let generation = decode_scalar(bytes, offset + 4, "u32");
        if let (Some(FieldValue::U32(index)), Some(FieldValue::U32(generation))) =
            (index, generation)
        {
            return FieldValue::Asset {
                asset_type: None,
                index,
                generation,
            };
        }
    }
    // An entity reference is a slot id and a generation, in the handle's
    // `#[repr(C)]` order; the trailing padding is not read.
    if is_entity_tag(tag) && size == ENTITY_REFERENCE_SIZE {
        let id = decode_scalar(bytes, offset, "u64");
        let generation = decode_scalar(bytes, offset + 8, "u32");
        if let (Some(FieldValue::U64(id)), Some(FieldValue::U32(generation))) = (id, generation) {
            return FieldValue::Entity { id, generation };
        }
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

/// One field's location: the entity's row, its component and the descriptor.
///
/// What every single-field entry point needs before it can read or write, so
/// the resolution rule and its error payloads live in
/// [`World::resolve_field`] rather than in each entry point.
struct ResolvedField {
    /// The archetype holding the entity.
    archetype_id: ArchetypeId,
    /// The entity's row within that archetype.
    row: usize,
    /// The component carried by that entity.
    component_id: ComponentId,
    /// The requested field's descriptor.
    descriptor: ComponentFieldDescriptor,
}

impl World {
    /// The archetype id and row of a live entity, for component access.
    fn entity_row_location(&self, entity: Entity) -> Option<(ArchetypeId, usize)> {
        let location = self.entity_locations.get(&entity)?;
        Some((location.archetype_id, location.index_in_archetype))
    }

    /// The registered field layout of a component, refusing an empty one.
    ///
    /// Borrowed rather than copied: the field entry points read one or every
    /// descriptor out of it, and the inspector calls them once per row per
    /// frame.
    fn field_layout(
        &self,
        component_id: ComponentId,
        component: &str,
    ) -> Result<&[ComponentFieldDescriptor], ComponentFieldError> {
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
        Ok(layout)
    }

    /// Locate one field of one component on one entity.
    ///
    /// The one place entity, component, layout and field are resolved, so a
    /// new entry point cannot invent its own error path: every miss reports the
    /// same variant with the same payload.
    fn resolve_field(
        &self,
        entity: Entity,
        component: &str,
        field: &str,
    ) -> Result<ResolvedField, ComponentFieldError> {
        let (archetype_id, row) = self
            .entity_row_location(entity)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let component_id = self.resolve_entity_component_id(entity, component).ok_or(
            ComponentFieldError::ComponentNotFound {
                component: component.to_string(),
            },
        )?;
        let descriptor = self
            .field_layout(component_id, component)?
            .iter()
            .find(|descriptor| descriptor.name == field)
            .ok_or(ComponentFieldError::FieldNotFound {
                component: component.to_string(),
                field: field.to_string(),
            })?;
        Ok(ResolvedField {
            archetype_id,
            row,
            component_id,
            descriptor: descriptor.clone(),
        })
    }

    /// One component column of one archetype, for a row that exists.
    ///
    /// `component_name` only feeds the error path: the caller resolved the
    /// component through the entity, so a missing column or a row past its end
    /// is a storage desync and reports the component the caller named.
    fn row_column(
        &self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
    ) -> Result<&ComponentColumn, ComponentFieldError> {
        let column = self
            .archetypes
            .get(&archetype_id)
            .ok_or(ComponentFieldError::EntityNotFound)?
            .component_storages
            .get(component_id)
            .ok_or(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            })?;
        if row >= column.len() {
            return Err(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            });
        }
        Ok(column)
    }

    /// [`Self::row_column`]'s mutable twin, for the write lanes.
    fn row_column_mut(
        &mut self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
    ) -> Result<&mut ComponentColumn, ComponentFieldError> {
        let column = self
            .archetypes
            .get_mut(&archetype_id)
            .ok_or(ComponentFieldError::EntityNotFound)?
            .component_storages
            .get_mut(component_id)
            .ok_or(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            })?;
        if row >= column.len() {
            return Err(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            });
        }
        Ok(column)
    }

    /// Copy one entity's component row for `component_id` into an owned buffer.
    ///
    /// Works for native (Rust) and descriptor (foreign-language) columns alike.
    /// The caller owns the returned bytes, so no borrow outlives this call.
    fn copy_component_row_bytes(
        &self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
    ) -> Result<Vec<u8>, ComponentFieldError> {
        let column = self.row_column(archetype_id, row, component_id, component_name)?;
        if !component_id.is_native_storage() {
            return column.bytes(row).map(<[u8]>::to_vec).ok_or(
                ComponentFieldError::ComponentStorageMissing {
                    component: component_name.to_string(),
                },
            );
        }
        let element_size = column.elem_size();
        let mut buffer = vec![0u8; element_size];
        // SAFETY: the row is within the column (`row < len`, checked by the
        // lookup) and the buffer is exactly one element long; nothing mutates
        // the column while this shared borrow is alive.
        unsafe {
            std::ptr::copy_nonoverlapping(
                column.raw_ptr().add(row * element_size),
                buffer.as_mut_ptr(),
                element_size,
            );
        }
        Ok(buffer)
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
        let ResolvedField {
            archetype_id,
            row,
            component_id,
            descriptor,
        } = self.resolve_field(entity, component_name, field_name)?;

        // Copy the row and decode the single field. A copy that comes back
        // missing after both lookups succeeded is a storage desync, not a dead
        // handle, so it reports the component by name.
        let row_bytes =
            self.copy_component_row_bytes(archetype_id, row, component_id, component_name)?;
        Ok(self.with_asset_type(
            component_id,
            &descriptor,
            decode_field(&row_bytes, &descriptor),
        ))
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
        // The entity and component half of `resolve_field`; this one reads every
        // descriptor, so it keeps the borrowed layout instead of one clone.
        let (archetype_id, row) = self
            .entity_row_location(entity)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let component_id = self
            .resolve_entity_component_id(entity, component_name)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;

        // Copy the layout and the whole row once.
        let layout = self.field_layout(component_id, component_name)?;
        let row_bytes =
            self.copy_component_row_bytes(archetype_id, row, component_id, component_name)?;

        // Decode every field from the one row image.
        Ok(layout
            .iter()
            .map(|descriptor| {
                self.with_asset_type(
                    component_id,
                    descriptor,
                    decode_field(&row_bytes, descriptor),
                )
            })
            .collect())
    }

    /// `value` with the asset type a handle field was registered with, when
    /// it is one; any other value unchanged.
    fn with_asset_type(
        &self,
        component_id: ComponentId,
        descriptor: &ComponentFieldDescriptor,
        value: FieldValue,
    ) -> FieldValue {
        match value {
            FieldValue::Asset {
                asset_type: None,
                index,
                generation,
            } => FieldValue::Asset {
                asset_type: self
                    .asset_field_type(component_id, descriptor.name)
                    .map(str::to_owned),
                index,
                generation,
            },
            other => other,
        }
    }

    /// Record that `component`'s handle field `field` references assets of the
    /// type registered as `asset_type`.
    ///
    /// Called by `#[derive(PillComponent)]`'s generated registration for each
    /// `Handle<T>` field, with `T`'s shared name, and by a foreign-language
    /// backend for its components. The name is copied: the caller's string
    /// may live in a module that a reload unmaps.
    pub fn record_asset_field_type(
        &mut self,
        component: ComponentId,
        field: &str,
        asset_type: &str,
    ) {
        self.asset_field_types
            .entry(component)
            .or_default()
            .insert(field.to_owned(), asset_type.to_owned());
    }

    /// Refuse an asset value whose type is not the one `field` was recorded
    /// with; a value or a field without a recorded type passes.
    fn check_asset_type(
        &self,
        component: ComponentId,
        field: &str,
        value: &FieldValue,
    ) -> Result<(), ComponentFieldError> {
        let FieldValue::Asset {
            asset_type: Some(found),
            ..
        } = value
        else {
            return Ok(());
        };
        match self.asset_field_type(component, field) {
            Some(expected) if expected != found => Err(ComponentFieldError::AssetTypeMismatch {
                field: field.to_owned(),
                expected: expected.to_owned(),
                found: found.clone(),
            }),
            _ => Ok(()),
        }
    }

    /// The asset type `component`'s handle field `field` references, if
    /// recorded.
    pub fn asset_field_type(&self, component: ComponentId, field: &str) -> Option<&str> {
        self.asset_field_types
            .get(&component)?
            .get(field)
            .map(String::as_str)
    }
}

// =============================================================================
// Nested field expansion
// =============================================================================

/// How many elements of an `array:struct:` field expand into leaf rows.
///
/// A control-point array of a few dozen entries becomes a few dozen rows,
/// which is what an editor wants to show; a fixed-capacity buffer of thousands
/// would only bloat every layout, so the rest stays reachable through the
/// field's own accessors.
pub const MAX_EXPANDED_ELEMENTS: usize = 64;

impl World {
    /// Record a component's editor field layout, expanding nested struct
    /// fields into dotted leaf rows.
    ///
    /// A `struct:<path>` field whose type has a known layout - another
    /// component registered with one, or a `#[derive(PillMirror)]` value type -
    /// becomes one row per leaf, named `<field>.<leaf>` with the offsets
    /// composed. Resolution sees the layouts registered so far, so the nested
    /// type registers first.
    ///
    /// The declared slice is kept beside the flattened view, because a
    /// generated mirror describes declarations, not leaf rows. Nothing to
    /// expand borrows the caller's `'static` slice for both; only an actual
    /// expansion copies into an owned editor layout.
    pub(crate) fn record_component_field_layout(
        &mut self,
        component_id: ComponentId,
        fields: &'static [ComponentFieldDescriptor],
    ) {
        let layout = match self.expanded_field_layout(fields) {
            Some(expanded) => crate::world::ComponentFieldLayout::from_expanded(fields, expanded),
            None => crate::world::ComponentFieldLayout::from_static(fields),
        };
        self.component_field_layouts.insert(component_id, layout);
    }

    /// Expand `struct:` fields into dotted leaf rows where their type's layout
    /// is known; `None` when every field already stands on its own.
    fn expanded_field_layout(
        &self,
        fields: &[ComponentFieldDescriptor],
    ) -> Option<Vec<ComponentFieldDescriptor>> {
        let mut expanded = Vec::new();
        let mut changed = false;
        self.expand_fields_into(fields, "", 0, &[], &mut expanded, &mut changed);
        changed.then_some(expanded)
    }

    /// Flatten one level of `fields` into `out`, recursing into every
    /// `struct:` field whose path resolves to a known layout.
    ///
    /// `prefix` is the dotted path of the struct being flattened ("" at the
    /// top level), `base_offset` is where that struct starts in the component
    /// row, and `inherited` is the grouping decorators (`Color`, `Vector`) the
    /// enclosing struct fields declared - the editor's row model groups leaves
    /// by them, so they have to reach the rows the leaves produce. A struct
    /// cannot contain itself without a heap indirection, and heap fields are
    /// not `struct:`-tagged, so the recursion terminates.
    fn expand_fields_into(
        &self,
        fields: &[ComponentFieldDescriptor],
        prefix: &str,
        base_offset: usize,
        inherited: &[FieldDecorator],
        out: &mut Vec<ComponentFieldDescriptor>,
        changed: &mut bool,
    ) {
        for field in fields {
            // An array of structs (`array:struct:<path>`) becomes one dotted
            // row per leaf per element - `points[0].x` - so the editor's generic
            // field API addresses a single control point the same way it
            // addresses a plain float. The element count is
            // the field's own; a cap keeps a huge array from flooding a layout.
            if let Some(path) = field.type_tag.strip_prefix("array:struct:") {
                if let Some(nested) = self.resolve_nested_layout(path) {
                    let elements = field.element_count.min(MAX_EXPANDED_ELEMENTS);
                    // A zero-length array has no stride to compute; every
                    // other count divides the field's whole size evenly.
                    let stride = field.size.checked_div(field.element_count).unwrap_or(0);
                    if elements > 0 {
                        *changed = true;
                        for index in 0..elements {
                            let element = if prefix.is_empty() {
                                format!("{}[{}]", field.name, index)
                            } else {
                                format!("{}.{}[{}]", prefix, field.name, index)
                            };
                            self.expand_fields_into(
                                &nested,
                                &element,
                                base_offset + field.offset + index * stride,
                                &grouping_decorators(&field.decorators),
                                out,
                                changed,
                            );
                        }
                        continue;
                    }
                }
            }
            let nested = field
                .type_tag
                .strip_prefix("struct:")
                .and_then(|path| self.resolve_nested_layout(path));
            if let Some(nested) = nested {
                *changed = true;
                let name = if prefix.is_empty() {
                    field.name.to_string()
                } else {
                    format!("{prefix}.{}", field.name)
                };
                let group = grouping_decorators(&field.decorators);
                self.expand_fields_into(
                    &nested,
                    &name,
                    base_offset + field.offset,
                    &group,
                    out,
                    changed,
                );
            } else {
                let name = if prefix.is_empty() {
                    field.name
                } else {
                    intern_nested_field_name(format!("{prefix}.{}", field.name))
                };
                let decorators = if inherited.is_empty() {
                    field.decorators.clone()
                } else {
                    inherited_decorators(inherited, field.decorators.as_ref())
                };
                out.push(ComponentFieldDescriptor {
                    name,
                    type_tag: field.type_tag,
                    offset: base_offset + field.offset,
                    size: field.size,
                    align: field.align,
                    element_count: field.element_count,
                    decorators,
                });
            }
        }
    }

    /// The field layout a `struct:<path>` tag names, if one is known.
    ///
    /// The tag is the path as written at the field site, which is usually an
    /// imported short name (`Color`), so an exact match is tried first and a
    /// unique `::<path>` suffix match second. Candidates are the world's
    /// registered component layouts and the registering DLL's
    /// `#[derive(PillMirror)]` value types, reached through the installed
    /// [`ValueTypeLayoutResolver`](crate::component_registry::ValueTypeLayoutResolver).
    /// An ambiguous suffix resolves to `None` - an opaque field is readable, a
    /// wrong one is not.
    fn resolve_nested_layout(&self, path: &str) -> Option<Vec<ComponentFieldDescriptor>> {
        // Registered components, through the rule both candidate sources share.
        let component_names = self
            .component_field_layouts
            .keys()
            .filter_map(|component_id| self.component_registry.get_name(component_id));
        if let Some(name) = crate::component_registry::resolve_unique_name(path, component_names) {
            for (component_id, layout) in &self.component_field_layouts {
                if self.component_registry.get_name(component_id) == Some(name) {
                    return Some(layout.fields().to_vec());
                }
            }
        }

        // The registering DLL's value types, through the resolver it installed;
        // it applies the same rule.
        let resolver = self.value_type_layout_resolver?;
        resolver(path).map(<[ComponentFieldDescriptor]>::to_vec)
    }

    /// Install `resolver` as the way value-type layouts are found while
    /// component layouts are recorded; returns the previous one so the caller
    /// can restore it.
    ///
    /// Called by the facade's `register_all_components` around its loop, with
    /// the resolver compiled into the registering DLL. Not meant for other
    /// callers.
    #[doc(hidden)]
    pub fn replace_value_type_layout_resolver(
        &mut self,
        resolver: Option<crate::component_registry::ValueTypeLayoutResolver>,
    ) -> Option<crate::component_registry::ValueTypeLayoutResolver> {
        std::mem::replace(&mut self.value_type_layout_resolver, resolver)
    }
}

/// The grouping decorators a struct field declares, which its flattened leaves
/// inherit.
///
/// `Color` and `Vector` describe the group of leaves a struct expands into, so
/// they have to travel with the leaves. Every other decorator presents the
/// single value its field holds - a header above it, a tooltip on it - and is
/// dropped together with the field that held the struct.
fn grouping_decorators(decorators: &[FieldDecorator]) -> Vec<FieldDecorator> {
    decorators
        .iter()
        .filter(|decorator| matches!(decorator, FieldDecorator::Color | FieldDecorator::Vector))
        .cloned()
        .collect()
}

/// The decorators one flattened leaf carries: what the struct fields above it
/// inherited, then what the leaf declared itself, without repeating one twice.
fn inherited_decorators(
    inherited: &[FieldDecorator],
    own: &[FieldDecorator],
) -> std::borrow::Cow<'static, [FieldDecorator]> {
    let mut merged = inherited.to_vec();
    for decorator in own {
        if !merged.contains(decorator) {
            merged.push(decorator.clone());
        }
    }
    std::borrow::Cow::Owned(merged)
}

/// Intern a synthesized nested field name ("color.r", "inner.a.b") into
/// `&'static str`.
///
/// Descriptors store names as `&'static str` and expansion builds them at
/// registration, so each distinct dotted name is leaked once and reused for
/// the process lifetime. The set is bounded by the distinct field paths a
/// process ever registers, so a hot reload re-registering the same components
/// does not grow it.
fn intern_nested_field_name(name: String) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static NESTED_FIELD_NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let names = NESTED_FIELD_NAMES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut names = names
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(&existing) = names.get(name.as_str()) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.into_boxed_str());
    names.insert(leaked);
    leaked
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
    // A heap-backed container is never writable through the generic path: its
    // buffer is reached through the field's accessors, which edit the live
    // container in place rather than replacing it, so an editor that showed a
    // stale image cannot overwrite the real one here. The predicate is the
    // crate's one spelling of "heap-backed", so a container tag added there
    // cannot leave a writable raw handle behind in a document.
    if is_container_tag(tag) {
        return Err(ComponentFieldError::UnsupportedField {
            field: field.to_string(),
            reason: "heap-backed fields are read-only here; edit them through the field accessors",
        });
    }
    // A captured payload writes back byte-faithfully: the bytes came out of
    // this exact field's column, and the value's tag plus its exact size pin
    // them to it. This is the route a restore takes (an editor re-creating a
    // destroyed entity, or re-attaching a removed component); a value of any
    // other kind still walks the editable rules below.
    if let FieldValue::Opaque { type_tag, bytes } = value {
        if *type_tag == descriptor.type_tag && bytes.len() == descriptor.size {
            return Ok(FieldWrite {
                row_offset: descriptor.offset,
                bytes: bytes.clone(),
            });
        }
        return Err(ComponentFieldError::TypeMismatch {
            field: field.to_string(),
            expected: tag,
            found: value.variant_tag(),
        });
    }
    // A handle field takes an asset value: its index and generation, encoded
    // like every other `u32` in a row (see the module's row byte convention).
    // The asset type is checked by the caller, which knows the field's record
    // (`World::write_component_field`).
    if let FieldValue::Asset {
        index, generation, ..
    } = value
    {
        if !is_handle_tag(tag) || descriptor.size != HANDLE_SIZE {
            return Err(ComponentFieldError::TypeMismatch {
                field: field.to_string(),
                expected: tag,
                found: value.variant_tag(),
            });
        }
        let mut bytes = Vec::with_capacity(HANDLE_SIZE);
        encode_scalar("u32", &FieldValue::U32(*index), &mut bytes);
        encode_scalar("u32", &FieldValue::U32(*generation), &mut bytes);
        return Ok(FieldWrite {
            row_offset: descriptor.offset,
            bytes,
        });
    }
    // A handle field is editable, but only by an asset value.
    if is_handle_tag(tag) {
        return Err(ComponentFieldError::TypeMismatch {
            field: field.to_string(),
            expected: tag,
            found: value.variant_tag(),
        });
    }
    // An entity reference takes an entity value: the slot id and generation,
    // then the padding a `#[repr(C)]` handle leaves after them, written as
    // zero so a row byte never carries a stale value.
    if is_entity_tag(tag) {
        let FieldValue::Entity { id, generation } = value else {
            return Err(ComponentFieldError::TypeMismatch {
                field: field.to_string(),
                expected: tag,
                found: value.variant_tag(),
            });
        };
        if descriptor.size != ENTITY_REFERENCE_SIZE {
            return Err(ComponentFieldError::TypeMismatch {
                field: field.to_string(),
                expected: tag,
                found: value.variant_tag(),
            });
        }
        let mut bytes = Vec::with_capacity(ENTITY_REFERENCE_SIZE);
        bytes.extend_from_slice(&id.to_ne_bytes());
        bytes.extend_from_slice(&generation.to_ne_bytes());
        bytes.resize(ENTITY_REFERENCE_SIZE, 0);
        return Ok(FieldWrite {
            row_offset: descriptor.offset,
            bytes,
        });
    }
    // A `struct:` field, or any other tag the engine cannot interpret, is
    // never writable through the generic path.
    if !is_scalar_tag(tag) && !tag.starts_with("array:") {
        return Err(ComponentFieldError::UnsupportedField {
            field: field.to_string(),
            reason: "the field's type is outside the editable vocabulary",
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
    /// observe the edit on their next run. [`Self::get_component_mut`] hands out
    /// the same column slot without that stamp. Nothing is written when any
    /// validation fails.
    pub fn write_component_field(
        &mut self,
        entity: Entity,
        component_name: &str,
        field_name: &str,
        value: FieldValue,
    ) -> Result<(), ComponentFieldError> {
        // Resolve, then validate and encode the payload BEFORE touching any
        // column, so a refused edit leaves the world untouched.
        let ResolvedField {
            archetype_id,
            row,
            component_id,
            descriptor,
        } = self.resolve_field(entity, component_name, field_name)?;
        self.check_asset_type(component_id, field_name, &value)?;
        let write = encode_field_write(&descriptor, field_name, &value)?;

        // Write the payload and stamp the row's changed tick, each in its own
        // archetype borrow so column and tick lookups never overlap.
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
            component_name,
            write.row_offset,
            &write.bytes,
        )?;
        self.stamp_row_changed(
            archetype_id,
            row,
            component_id,
            component_name,
            current_tick,
        )
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
        // An ambiguous name means two registrations claim it, so there is no
        // one component to build an image for; it reads the same as "not
        // found" to the editor, which is the safe outcome either way.
        let component_id = self
            .resolve_component_id_by_name_any(component_name)
            .unwrap_or(None)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;
        let layout = self.field_layout(component_id, component_name)?;
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
            self.check_asset_type(component_id, field_name, value)?;
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
        let ResolvedField {
            archetype_id,
            row,
            component_id,
            descriptor,
        } = self.resolve_field(entity, component_name, field_name)?;

        // The field must be a fixed array of scalar primitives.
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

        // Write the element and stamp the row's changed tick. The tick is
        // bumped first for the same reason the whole-field write bumps it: a
        // between-frame edit must be visible to `Changed<T>` filters.
        let row_offset = descriptor.offset + index * element_size;
        let current_tick = self.increment_change_tick();
        self.apply_row_write(
            archetype_id,
            row,
            component_id,
            component_name,
            row_offset,
            &bytes,
        )?;
        self.stamp_row_changed(
            archetype_id,
            row,
            component_id,
            component_name,
            current_tick,
        )
    }

    /// Overwrite `bytes.len()` bytes at `row_offset` of one component row.
    ///
    /// `component_name` only feeds the error paths: the caller resolved the
    /// component through the entity, so a missing column or row here is a
    /// storage desync and reports the component the caller named.
    fn apply_row_write(
        &mut self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
        row_offset: usize,
        bytes: &[u8],
    ) -> Result<(), ComponentFieldError> {
        match component_id {
            _ if component_id.is_native_storage() => {
                let column =
                    self.row_column_mut(archetype_id, row, component_id, component_name)?;
                let element_size = column.elem_size();
                // SAFETY: the lookup checked `row < column.len()`, so the
                // pointer stays inside the column's allocation, and the column
                // borrow keeps that allocation alive through this statement.
                let destination = unsafe { column.as_mut_ptr().add(row * element_size) };
                // Checked arithmetic on the way to the write: a wrapping sum
                // would pass a caller-supplied offset near `usize::MAX` and
                // let the copy below run off the row.
                let Some(end) = row_offset
                    .checked_add(bytes.len())
                    .filter(|end| *end <= element_size)
                else {
                    return Err(ComponentFieldError::UnsupportedField {
                        field: String::new(),
                        reason: "write extends past the component row",
                    });
                };
                // SAFETY: the destination row is initialized (`row < len`) and
                // the payload fits within it (checked above); the column is not
                // reallocated while this borrow is held.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        destination.add(row_offset),
                        end - row_offset,
                    );
                }
                Ok(())
            }
            _ => {
                let mut row_bytes = self
                    .row_column_mut(archetype_id, row, component_id, component_name)?
                    .bytes(row)
                    .ok_or(ComponentFieldError::ComponentStorageMissing {
                        component: component_name.to_string(),
                    })?
                    .to_vec();
                let element_size = row_bytes.len();
                // The same checked arithmetic as the native branch: the row
                // slice below would otherwise be ranged with a wrapped sum.
                let Some(end) = row_offset
                    .checked_add(bytes.len())
                    .filter(|end| *end <= element_size)
                else {
                    return Err(ComponentFieldError::UnsupportedField {
                        field: String::new(),
                        reason: "write extends past the component row",
                    });
                };
                let target = row_bytes.get_mut(row_offset..end).ok_or(
                    ComponentFieldError::UnsupportedField {
                        field: String::new(),
                        reason: "write extends past the component row",
                    },
                )?;
                target.copy_from_slice(bytes);
                self.row_column_mut(archetype_id, row, component_id, component_name)?
                    .set_bytes(row, &row_bytes)
                    .map_err(|_| ComponentFieldError::ComponentStorageMissing {
                        component: component_name.to_string(),
                    })
            }
        }
    }

    /// Stamp one row's `changed` tick so `Changed<T>` systems react.
    fn stamp_row_changed(
        &mut self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
        tick: Tick,
    ) -> Result<(), ComponentFieldError> {
        let row_ticks = self
            .row_column_mut(archetype_id, row, component_id, component_name)?
            .row_ticks_mut(row)
            .ok_or(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            })?;
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

    /// A second registered type the sample entity's archetype never carries,
    /// used to drive the write path into its storage-missing arms.
    #[repr(C)]
    #[derive(Debug, Clone, PartialEq)]
    struct OtherComponent {
        value: u32,
    }
    impl Component for OtherComponent {}

    /// Inner half of the nested-layout components below.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct NestedInner {
        a: f32,
        b: f32,
    }
    impl Component for NestedInner {}

    /// A value type whose layout reaches components through the installed
    /// value-type resolver rather than through a component registration of
    /// its own. The facade's `register_all_components` installs one over its
    /// DLL's `#[derive(PillMirror)]` registry; this test installs its own.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct ProbeVector {
        x: f32,
        y: f32,
    }

    /// `ProbeVector`'s layout, as `#[derive(PillMirror)]` would declare it.
    static PROBE_VECTOR_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "x",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "y",
            type_tag: "f32",
            offset: 4,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
    ];

    /// The test's value-type resolver: knows `ProbeVector` only.
    fn probe_value_types(path: &str) -> Option<&'static [ComponentFieldDescriptor]> {
        (path == "ProbeVector").then_some(PROBE_VECTOR_FIELDS)
    }

    /// Carries one nested component field and one nested value-type field.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct NestedOuter {
        lead: f32,
        inner: NestedInner,
        probe: ProbeVector,
    }
    impl Component for NestedOuter {}

    // A hand-rolled registration mirroring what #[derive(PillComponent)] emits.
    static SAMPLE_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "a_float",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "an_int",
            type_tag: "u32",
            offset: 4,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "a_bool",
            type_tag: "bool",
            offset: 8,
            size: 1,
            align: 1,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
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
            decorators: std::borrow::Cow::Borrowed(&[]),
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

    static NESTED_INNER_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "a",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "b",
            type_tag: "f32",
            offset: 4,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
    ];

    static NESTED_OUTER_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "lead",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "inner",
            type_tag: "struct:NestedInner",
            offset: 4,
            size: 8,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "probe",
            type_tag: "struct:ProbeVector",
            offset: 12,
            size: 8,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
    ];

    /// A component carrying an array of value types: the shape a spline's
    /// control points have.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct PointArray {
        lead: f32,
        points: [ProbeVector; 2],
    }
    impl Component for PointArray {}

    static POINT_ARRAY_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "lead",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "points",
            type_tag: "array:struct:ProbeVector",
            offset: 4,
            size: 16,
            align: 4,
            element_count: 2,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
    ];

    /// An `array:struct:` field expands into one dotted row per leaf per
    /// element, with the element stride composed from the array's own size, and
    /// a single element's leaf reads and writes like any scalar field.
    #[test]
    fn struct_arrays_expand_into_editable_element_rows() {
        let mut world = World::new();
        world.replace_value_type_layout_resolver(Some(probe_value_types));
        world.register_component_with_layout::<PointArray>(POINT_ARRAY_FIELDS);
        world.replace_value_type_layout_resolver(None);

        let layout = world
            .component_field_layout(ComponentId::of::<PointArray>())
            .expect("PointArray registered with a layout");
        let names: Vec<&str> = layout.iter().map(|field| field.name).collect();
        assert_eq!(
            names,
            [
                "lead",
                "points[0].x",
                "points[0].y",
                "points[1].x",
                "points[1].y"
            ]
        );
        assert_eq!(layout[1].offset, 4, "first element starts after `lead`");
        assert_eq!(layout[3].offset, 12, "the second element is one stride on");

        let entity = world
            .create_entity()
            .with(PointArray {
                lead: 1.0,
                points: [
                    ProbeVector { x: 2.0, y: 3.0 },
                    ProbeVector { x: 4.0, y: 5.0 },
                ],
            })
            .build()
            .expect("entity builds");
        let component = world
            .entity_component_names(entity)
            .and_then(|names| names.into_iter().find(|name| name.ends_with("PointArray")))
            .expect("the entity carries PointArray");
        let read = world
            .read_component_field(entity, &component, "points[1].y")
            .expect("the element's leaf reads");
        assert!(matches!(read, FieldValue::F32(value) if value == 5.0));
        world
            .write_component_field(entity, &component, "points[0].x", FieldValue::F32(9.5))
            .expect("the element's leaf writes");
        let read = world
            .read_component_field(entity, &component, "points[0].x")
            .expect("the element's leaf reads back");
        assert!(matches!(read, FieldValue::F32(value) if value == 9.5));
    }

    fn nested_world() -> World {
        let mut world = World::new();
        world.replace_value_type_layout_resolver(Some(probe_value_types));
        world.register_component_with_layout::<NestedInner>(NESTED_INNER_FIELDS);
        world.register_component_with_layout::<NestedOuter>(NESTED_OUTER_FIELDS);
        world.replace_value_type_layout_resolver(None);
        world
    }

    /// Nested `struct:` fields expand into dotted leaf rows at registration,
    /// and the leaves read and write like any scalar field.
    #[test]
    fn nested_fields_expand_into_editable_rows() {
        let mut world = nested_world();

        let layout = world
            .component_field_layout(ComponentId::of::<NestedOuter>())
            .expect("NestedOuter registered with a layout");
        let names: Vec<&str> = layout.iter().map(|field| field.name).collect();
        assert_eq!(names, ["lead", "inner.a", "inner.b", "probe.x", "probe.y"]);
        assert_eq!(layout[1].offset, 4, "composed offsets for inner.a");
        assert_eq!(layout[2].offset, 8, "composed offsets for inner.b");
        assert_eq!(layout[3].offset, 12, "composed offsets for probe.x");

        let entity = world
            .create_entity()
            .with(NestedOuter {
                lead: 1.0,
                inner: NestedInner { a: 2.0, b: 3.0 },
                probe: ProbeVector { x: 4.0, y: 5.0 },
            })
            .build()
            .expect("entity builds");

        let name = "pill_engine::component_field::tests::NestedOuter";
        assert_eq!(
            world.read_component_field(entity, name, "inner.b").unwrap(),
            FieldValue::F32(3.0)
        );
        assert_eq!(
            world.read_component_field(entity, name, "probe.y").unwrap(),
            FieldValue::F32(5.0)
        );

        world
            .write_component_field(entity, name, "inner.b", FieldValue::F32(9.0))
            .expect("the expanded leaf is writable");
        assert_eq!(
            world.read_component_field(entity, name, "inner.b").unwrap(),
            FieldValue::F32(9.0)
        );
    }

    /// The declared view keeps the nesting the editor view flattens: a mirror
    /// is a struct declaration, and a leaf row names no field a struct has.
    #[test]
    fn the_declared_layout_keeps_nested_fields_whole() {
        let mut world = nested_world();
        world.replace_value_type_layout_resolver(Some(probe_value_types));
        world.register_component_with_layout::<PointArray>(POINT_ARRAY_FIELDS);
        world.replace_value_type_layout_resolver(None);

        let declared = world
            .component_declared_field_layout(ComponentId::of::<NestedOuter>())
            .expect("NestedOuter registered with a layout");
        let names: Vec<&str> = declared.iter().map(|field| field.name).collect();
        assert_eq!(names, ["lead", "inner", "probe"]);
        assert_eq!(declared[1].type_tag, "struct:NestedInner");
        assert_eq!(declared[1].offset, 4);
        assert_eq!(declared[1].size, 8, "the whole nested struct, not a leaf");

        // An array of structs stays one array field, so the mirror emits the
        // elements it declares rather than one struct field per leaf.
        let declared = world
            .component_declared_field_layout(ComponentId::of::<PointArray>())
            .expect("PointArray registered with a layout");
        let names: Vec<&str> = declared.iter().map(|field| field.name).collect();
        assert_eq!(names, ["lead", "points"]);
        assert_eq!(declared[1].type_tag, "array:struct:ProbeVector");
        assert_eq!(declared[1].element_count, 2);
    }

    /// A `struct:` field whose type has no known layout stays one opaque row
    /// instead of being guessed at.
    #[test]
    fn unresolvable_struct_fields_stay_opaque() {
        static FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
            name: "mystery",
            type_tag: "struct:NoSuchTypeExists",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        }];

        #[repr(C)]
        #[derive(Debug, Clone, Copy)]
        struct OpaqueHolder {
            mystery: u32,
        }
        impl Component for OpaqueHolder {}

        let mut world = World::new();
        world.register_component_with_layout::<OpaqueHolder>(FIELDS);
        let layout = world
            .component_field_layout(ComponentId::of::<OpaqueHolder>())
            .expect("OpaqueHolder registered with a layout");
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].name, "mystery");
        assert_eq!(layout[0].type_tag, "struct:NoSuchTypeExists");
    }

    /// A captured opaque payload round-trips: read it off a live row, write it
    /// back through both generic paths (a field write and a whole image), and
    /// the bytes match; a payload that does not pin to the field is refused.
    #[test]
    fn captured_opaque_payloads_round_trip_and_are_pinned_to_their_field() {
        static FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
            name: "mystery",
            type_tag: "struct:NoSuchTypeExists",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        }];

        #[repr(C)]
        #[derive(Debug, Clone, Copy)]
        struct OpaqueHolder {
            mystery: u32,
        }
        impl Component for OpaqueHolder {}

        let mut world = World::new();
        world.register_component_with_layout::<OpaqueHolder>(FIELDS);
        let name = crate::component::ComponentRegistry::registered_name::<OpaqueHolder>();

        let entity = world
            .create_entity()
            .with(OpaqueHolder {
                mystery: 0xAAAA_BBBB,
            })
            .build()
            .expect("entity builds");
        let captured = world
            .read_component_field(entity, &name, "mystery")
            .expect("the opaque field reads");
        let FieldValue::Opaque { type_tag, bytes } = captured.clone() else {
            panic!("a struct-tagged field must read as opaque, got {captured:?}");
        };
        assert_eq!(type_tag, "struct:NoSuchTypeExists");
        // The captured bytes are the row's own, so they carry the module's
        // native-endian convention rather than a fixed byte order.
        assert_eq!(bytes, 0xAAAA_BBBBu32.to_ne_bytes());

        // A field write of the captured payload lands the same bytes back.
        world
            .write_component_field(entity, &name, "mystery", captured.clone())
            .expect("the captured payload writes back");
        assert_eq!(
            world.read_component_field(entity, &name, "mystery"),
            Ok(captured.clone())
        );

        // A whole captured image builds too, which is the restore path.
        let image = world
            .build_component_image(&name, &[("mystery".to_string(), captured.clone())])
            .expect("the captured image builds");
        assert_eq!(image, 0xAAAA_BBBBu32.to_ne_bytes());

        // A payload whose size or tag does not pin to the field is refused.
        let short = FieldValue::Opaque {
            type_tag,
            bytes: vec![0, 1],
        };
        assert_eq!(
            world.write_component_field(entity, &name, "mystery", short.clone()),
            Err(ComponentFieldError::TypeMismatch {
                field: "mystery".to_string(),
                expected: "struct:NoSuchTypeExists",
                found: "opaque",
            })
        );
        let mistagged = FieldValue::Opaque {
            type_tag: "struct:SomethingElse",
            bytes: bytes.clone(),
        };
        assert_eq!(
            world.build_component_image(&name, &[("mystery".to_string(), mistagged)]),
            Err(ComponentFieldError::TypeMismatch {
                field: "mystery".to_string(),
                expected: "struct:NoSuchTypeExists",
                found: "opaque",
            })
        );
    }

    /// An opaque payload cannot be written over a heap container's header.
    ///
    /// `dynbuf:` names a `DynamicBuffer` header, and an opaque payload whose
    /// tag and size pin to the field would copy its bytes straight over the
    /// live pointer. Every container tag takes the accessor-only path instead.
    #[test]
    fn an_opaque_payload_cannot_overwrite_a_container_handle() {
        static FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
            name: "samples",
            type_tag: "dynbuf:f32",
            offset: 0,
            size: 3 * std::mem::size_of::<usize>(),
            align: std::mem::align_of::<usize>(),
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        }];

        #[repr(C)]
        #[derive(Debug, Clone, Copy)]
        struct DynHolder {
            /// A `DynamicBuffer` header: pointer, length, capacity.
            samples: [usize; 3],
        }
        impl Component for DynHolder {}

        let mut world = World::new();
        world.register_component_with_layout::<DynHolder>(FIELDS);
        let name = crate::component::ComponentRegistry::registered_name::<DynHolder>();
        let entity = world
            .create_entity()
            .with(DynHolder {
                samples: [0x1000, 4, 8],
            })
            .build()
            .expect("entity builds");

        // A payload a hand-written document could carry: tagged and sized to
        // pin exactly to this field, filled with bytes of its choosing.
        let forged = FieldValue::Opaque {
            type_tag: "dynbuf:f32",
            bytes: 0xDEAD_BEEF_u64.to_ne_bytes().repeat(3),
        };
        assert!(
            matches!(
                world.write_component_field(entity, &name, "samples", forged),
                Err(ComponentFieldError::UnsupportedField { .. })
            ),
            "a container field must refuse the generic write path"
        );

        // The live header is untouched.
        assert_eq!(
            world.read_component_field(entity, &name, "samples"),
            Ok(FieldValue::List {
                element_tag: "f32",
                element_count: 4,
            })
        );
    }

    /// A heap container field is read by following its real header.
    ///
    /// Neither a `String` nor a `Vec` puts its pointer in the first word of its
    /// header, so a decoder that assumes one either reports the length as a
    /// pointer - and dereferences it - or the capacity as an element count.
    #[test]
    fn heap_container_fields_read_through_their_real_header() {
        static FIELDS: &[ComponentFieldDescriptor] = &[
            ComponentFieldDescriptor {
                name: "label",
                type_tag: "string",
                offset: 0,
                size: std::mem::size_of::<String>(),
                align: std::mem::align_of::<String>(),
                element_count: 0,
                decorators: std::borrow::Cow::Borrowed(&[]),
            },
            ComponentFieldDescriptor {
                name: "points",
                type_tag: "vec:u32",
                offset: std::mem::size_of::<String>(),
                size: std::mem::size_of::<Vec<u32>>(),
                align: std::mem::align_of::<Vec<u32>>(),
                element_count: 0,
                decorators: std::borrow::Cow::Borrowed(&[]),
            },
        ];

        #[repr(C)]
        #[derive(Debug)]
        struct Holder {
            label: String,
            points: Vec<u32>,
        }
        impl Component for Holder {}

        // Three live elements in a sixteen-element allocation, so the count and
        // the capacity cannot be confused for one another.
        let mut points = Vec::with_capacity(16);
        points.extend([10_u32, 20, 30]);

        let mut world = World::new();
        world.register_component_with_layout::<Holder>(FIELDS);
        let name = crate::component::ComponentRegistry::registered_name::<Holder>();
        let entity = world
            .create_entity()
            .with(Holder {
                label: "hello".to_string(),
                points,
            })
            .build()
            .expect("entity builds");

        assert_eq!(
            world.read_component_field(entity, &name, "label"),
            Ok(FieldValue::Text("hello".to_string()))
        );
        assert_eq!(
            world.read_component_field(entity, &name, "points"),
            Ok(FieldValue::List {
                element_tag: "u32",
                element_count: 3,
            })
        );
    }

    /// The tag table, both codecs and the diagnostic names all agree.
    ///
    /// They used to be hand-copied matches, so a scalar could be sized but not
    /// decoded - or decoded but reported by a name nothing else knew. A tag is
    /// a vocabulary entry only if every one of them accepts it.
    #[test]
    fn every_scalar_tag_sizes_encodes_decodes_and_names_itself() {
        let values = [
            FieldValue::F32(1.5),
            FieldValue::F64(-2.25),
            FieldValue::I8(-3),
            FieldValue::I16(-300),
            FieldValue::I32(-70_000),
            FieldValue::I64(-5_000_000_000),
            FieldValue::U8(7),
            FieldValue::U16(600),
            FieldValue::U32(70_000),
            FieldValue::U64(5_000_000_000),
            FieldValue::Bool(true),
            FieldValue::Usize(12),
            FieldValue::Isize(-12),
        ];
        for value in values {
            let tag = value.variant_tag();
            let size = scalar_size(tag).unwrap_or_else(|| panic!("`{tag}` is a scalar tag"));
            assert!(is_scalar_tag(tag), "`{tag}` is a scalar tag");

            let mut bytes = Vec::new();
            assert!(encode_scalar(tag, &value, &mut bytes), "`{tag}` encodes");
            assert_eq!(bytes.len(), size, "`{tag}` encodes to its declared size");

            let decoded = decode_scalar(&bytes, 0, tag).expect("the encoded value decodes");
            assert_eq!(decoded, value, "`{tag}` survives its own round trip");
        }

        // A tag nothing claims stays outside the vocabulary rather than
        // half-known.
        assert_eq!(scalar_size("u128"), None);
        assert!(!is_scalar_tag("u128"));
        assert!(!encode_scalar("u128", &FieldValue::U8(1), &mut Vec::new()));
        assert_eq!(decode_scalar(&[0_u8; 16], 0, "u128"), None);
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

    /// Every single-field entry point misses the same way.
    ///
    /// They share one resolver, so a read and an array-element write have to
    /// report a dead entity, a missing component and a missing field with the
    /// same variant and payload; a new entry point that grows its own preamble
    /// fails here.
    #[test]
    fn every_entry_point_reports_the_same_misses() {
        let mut world = test_world();
        let entity = world
            .create_entity()
            .with(sample_value())
            .build()
            .expect("entity builds");
        let name = "pill_engine::component_field::tests::SampleComponent";

        // One probe per entry point, for each kind of miss.
        for (component, field) in [("missing::Type", "a_float"), (name, "missing_field")] {
            let read = world.read_component_field(entity, component, field).err();
            let write = world
                .write_component_field(entity, component, field, FieldValue::F32(0.0))
                .err();
            let element = world
                .write_component_array_element(entity, component, field, 0, FieldValue::I16(0))
                .err();
            assert_eq!(read, write, "read and write agree on `{field}`");
            assert_eq!(write, element, "the array write agrees on `{field}`");
        }
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

        let _ = world.destroy_entity(entity);
        assert_eq!(
            world.read_component_field(entity, name, "a_float"),
            Err(ComponentFieldError::EntityNotFound)
        );
        assert_eq!(
            world.write_component_field(entity, name, "a_float", FieldValue::F32(0.0)),
            Err(ComponentFieldError::EntityNotFound)
        );
        assert_eq!(
            world.write_component_array_element(entity, name, "an_array", 0, FieldValue::I16(0)),
            Err(ComponentFieldError::EntityNotFound)
        );
    }

    /// A storage miss names the component instead of blaming the entity.
    ///
    /// The entry points resolve the component through a live entity, so a
    /// column or row that is missing after that point is a registration
    /// desync; `EntityNotFound` sent callers looking for a dead handle that is
    /// right there.
    #[test]
    fn storage_missing_names_the_component() {
        // The summary names the component and cannot be read as a dead handle.
        let summary = ComponentFieldError::ComponentStorageMissing {
            component: "demo::Settings".to_string(),
        }
        .summary();
        assert!(summary.contains("demo::Settings"), "{summary}");
        assert!(!summary.contains("entity"), "{summary}");

        // And the write path reports it: the entity lives, but its archetype
        // holds no column for the registered component it does not carry.
        let mut world = test_world();
        world.register_component::<OtherComponent>();
        let entity = world
            .create_entity()
            .with(sample_value())
            .build()
            .expect("entity builds");
        let (archetype_id, row) = world.entity_row_location(entity).expect("entity is live");
        assert_eq!(
            world.apply_row_write(
                archetype_id,
                row,
                ComponentId::of::<OtherComponent>(),
                "OtherComponent",
                0,
                &[0u8; 4],
            ),
            Err(ComponentFieldError::ComponentStorageMissing {
                component: "OtherComponent".to_string(),
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

        let component_id = ComponentId::of::<SampleComponent>();
        let row_ticks_of = |world: &World| {
            *world
                .archetypes
                .values()
                .next()
                .unwrap()
                .component_storages
                .get(component_id)
                .unwrap()
                .row_ticks(0)
                .unwrap()
        };

        let _ = world.read_component_field(entity, name, "a_float").unwrap();
        let row_ticks = row_ticks_of(&world);
        assert!(!row_ticks.changed.is_newer_than(before, world.change_tick()));

        world
            .write_component_field(entity, name, "a_float", FieldValue::F32(2.0))
            .expect("write succeeds");
        let row_ticks = row_ticks_of(&world);
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

    /// A component holding two asset handles, laid out like the derive lays
    /// out `MeshRendererComponent`.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct RendererProbe {
        mesh: crate::asset::Handle<ProbeMesh>,
        material: crate::asset::Handle<ProbeMaterial>,
    }
    impl Component for RendererProbe {}

    /// The two asset types the probe references.
    struct ProbeMesh;
    impl crate::asset::Asset for ProbeMesh {}
    struct ProbeMaterial;
    impl crate::asset::Asset for ProbeMaterial {}

    static RENDERER_PROBE_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "mesh",
            type_tag: "struct:Handle<ProbeMesh>",
            offset: 0,
            size: 8,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "material",
            type_tag: "struct:Handle<ProbeMaterial>",
            offset: 8,
            size: 8,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
    ];

    /// A world with the probe registered and its two fields' asset types
    /// recorded, and one entity carrying it.
    fn renderer_probe_world() -> (World, crate::entity::Entity, String) {
        let mut world = World::new();
        world.register_component_with_layout::<RendererProbe>(RENDERER_PROBE_FIELDS);
        let component = ComponentId::of::<RendererProbe>();
        world.record_asset_field_type(component, "mesh", "probe::Mesh");
        world.record_asset_field_type(component, "material", "probe::Material");
        let entity = world
            .create_entity()
            .with(RendererProbe {
                mesh: crate::asset::Handle::from_raw(0, 0),
                material: crate::asset::Handle::from_raw(1, 0),
            })
            .build()
            .expect("entity builds");
        let name = crate::component::ComponentRegistry::registered_name::<RendererProbe>();
        (world, entity, name)
    }

    /// A handle field reads as an asset value naming its type and the handle,
    /// not as opaque bytes.
    #[test]
    fn a_handle_field_reads_as_a_typed_asset_reference() {
        let (world, entity, name) = renderer_probe_world();
        let fields = world.read_component_fields(entity, &name).unwrap();
        assert_eq!(
            fields,
            vec![
                FieldValue::Asset {
                    asset_type: Some("probe::Mesh".to_owned()),
                    index: 0,
                    generation: 0,
                },
                FieldValue::Asset {
                    asset_type: Some("probe::Material".to_owned()),
                    index: 1,
                    generation: 0,
                },
            ]
        );
    }

    /// Another handle of the field's type writes, stamps the row changed, and
    /// reads back; the invalid handle clears the reference.
    #[test]
    fn a_handle_field_writes_another_handle_and_clears() {
        let (mut world, entity, name) = renderer_probe_world();
        let other = FieldValue::Asset {
            asset_type: Some("probe::Mesh".to_owned()),
            index: 7,
            generation: 2,
        };
        world
            .write_component_field(entity, &name, "mesh", other.clone())
            .expect("a mesh handle writes into the mesh field");
        assert_eq!(
            world.read_component_field(entity, &name, "mesh").unwrap(),
            other
        );
        assert_eq!(
            world.get_component::<RendererProbe>(entity).unwrap().mesh,
            crate::asset::Handle::from_raw(7, 2)
        );

        let cleared = FieldValue::Asset {
            asset_type: Some("probe::Mesh".to_owned()),
            index: u32::MAX,
            generation: u32::MAX,
        };
        world
            .write_component_field(entity, &name, "mesh", cleared)
            .expect("the invalid handle clears the reference");
        assert_eq!(
            world.get_component::<RendererProbe>(entity).unwrap().mesh,
            crate::asset::Handle::INVALID
        );
    }

    /// A handle of another asset type is refused with a typed error, and a
    /// non-asset value is refused as a type mismatch; neither writes.
    #[test]
    fn a_handle_of_another_asset_type_is_refused() {
        let (mut world, entity, name) = renderer_probe_world();
        let material = FieldValue::Asset {
            asset_type: Some("probe::Material".to_owned()),
            index: 3,
            generation: 0,
        };
        let refused = world.write_component_field(entity, &name, "mesh", material);
        assert!(
            matches!(
                &refused,
                Err(ComponentFieldError::AssetTypeMismatch { expected, found, .. })
                    if expected == "probe::Mesh" && found == "probe::Material"
            ),
            "{refused:?}"
        );
        let scalar = world.write_component_field(entity, &name, "mesh", FieldValue::U32(5));
        assert!(
            matches!(scalar, Err(ComponentFieldError::TypeMismatch { .. })),
            "{scalar:?}"
        );
        assert_eq!(
            world.get_component::<RendererProbe>(entity).unwrap().mesh,
            crate::asset::Handle::from_raw(0, 0),
            "nothing was written"
        );
    }

    /// A component holding a reference to another entity, laid out like the
    /// derive lays out an `Entity` field.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct FollowProbe {
        target: crate::entity::Entity,
        weight: f32,
    }
    impl Component for FollowProbe {}

    static FOLLOW_PROBE_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "target",
            type_tag: "entity",
            offset: 0,
            size: size_of::<crate::entity::Entity>(),
            align: 8,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "weight",
            type_tag: "f32",
            offset: 16,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
    ];

    /// An entity field reads as an entity value naming the handle it holds,
    /// not as opaque bytes.
    #[test]
    fn an_entity_field_reads_as_an_entity_reference() {
        let mut world = World::new();
        world.register_component_with_layout::<FollowProbe>(FOLLOW_PROBE_FIELDS);
        let target = world.create_entity().build().expect("target builds");
        let follower = world
            .create_entity()
            .with(FollowProbe {
                target,
                weight: 0.5,
            })
            .build()
            .expect("follower builds");
        let name = crate::component::ComponentRegistry::registered_name::<FollowProbe>();

        assert_eq!(
            world
                .read_component_field(follower, &name, "target")
                .unwrap(),
            FieldValue::Entity {
                id: target.id(),
                generation: target.generation(),
            }
        );
    }

    /// Writing another handle re-points the reference and stamps the row
    /// changed; the following scalar keeps its bytes.
    #[test]
    fn an_entity_field_writes_another_reference() {
        let mut world = World::new();
        world.register_component_with_layout::<FollowProbe>(FOLLOW_PROBE_FIELDS);
        let first = world.create_entity().build().expect("first builds");
        let second = world.create_entity().build().expect("second builds");
        let follower = world
            .create_entity()
            .with(FollowProbe {
                target: first,
                weight: 0.5,
            })
            .build()
            .expect("follower builds");
        let name = crate::component::ComponentRegistry::registered_name::<FollowProbe>();

        let rewritten = FieldValue::Entity {
            id: second.id(),
            generation: second.generation(),
        };
        world
            .write_component_field(follower, &name, "target", rewritten)
            .expect("an entity value writes into the reference");
        assert_eq!(
            world.get_component::<FollowProbe>(follower).unwrap().target,
            second
        );
        assert_eq!(
            world.get_component::<FollowProbe>(follower).unwrap().weight,
            0.5,
            "the neighbouring field was not touched"
        );
    }

    /// A non-entity value is refused and nothing is written.
    #[test]
    fn an_entity_field_refuses_a_scalar() {
        let mut world = World::new();
        world.register_component_with_layout::<FollowProbe>(FOLLOW_PROBE_FIELDS);
        let target = world.create_entity().build().expect("target builds");
        let follower = world
            .create_entity()
            .with(FollowProbe {
                target,
                weight: 0.5,
            })
            .build()
            .expect("follower builds");
        let name = crate::component::ComponentRegistry::registered_name::<FollowProbe>();

        let refused = world.write_component_field(follower, &name, "target", FieldValue::U64(7));
        assert!(
            matches!(
                &refused,
                Err(ComponentFieldError::TypeMismatch { expected, found, .. })
                    if *expected == "entity" && *found == "u64"
            ),
            "{refused:?}"
        );
        assert_eq!(
            world.get_component::<FollowProbe>(follower).unwrap().target,
            target,
            "nothing was written"
        );
    }
}

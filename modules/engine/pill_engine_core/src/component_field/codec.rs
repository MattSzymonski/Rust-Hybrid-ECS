//! Scalar and container encode/decode for component field rows.
//!
//! # Responsibilities
//!
//! - Turn a row's bytes into a [`FieldValue`](super::FieldValue) and back, for
//!   every tag the derive emits: the scalar primitives, asset and entity
//!   handles, whole fixed arrays, and the heap containers read through their
//!   real header.
//! - Own the size facts a row is validated against - handle size, entity
//!   reference size, the array element size - so the reads and the writes
//!   agree on them.
//!
//! # Design
//!
//! A container field's bytes are a `Vec` or a `String`, whose layout std does
//! not pin: the header offsets are measured once at runtime
//! (`std_container_offsets`) and every read through them carries its `// SAFETY:`
//! argument. Rows are native-endian by the module's stated convention, so the
//! scalar paths are plain `from_ne_bytes`/`to_ne_bytes`.

// Standard library
use std::sync::OnceLock;

// Current crate
use super::row::FieldWrite;
use super::{is_container_tag, is_entity_tag, is_handle_tag, ComponentFieldError, FieldValue};
use crate::component_registry::ComponentFieldDescriptor;
use crate::entity::Entity;

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
pub(super) enum ScalarKind {
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
    pub(super) fn tag(self) -> &'static str {
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
pub(super) fn scalar_size(tag: &str) -> Option<usize> {
    ScalarKind::from_tag(tag).map(ScalarKind::size)
}

/// Whether a tag names one of the scalar primitives.
pub(super) fn is_scalar_tag(tag: &str) -> bool {
    ScalarKind::from_tag(tag).is_some()
}

/// Encode one scalar value into `output`, returning whether the tag matched.
pub(super) fn encode_scalar(tag: &str, value: &FieldValue, output: &mut Vec<u8>) -> bool {
    ScalarKind::from_tag(tag).is_some_and(|kind| kind.encode(value, output))
}

/// Decode one scalar from a byte buffer at `offset`, or `None` when the tag is
/// not a scalar.
pub(super) fn decode_scalar(bytes: &[u8], offset: usize, tag: &str) -> Option<FieldValue> {
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
pub(super) fn decode_field(bytes: &[u8], descriptor: &ComponentFieldDescriptor) -> FieldValue {
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

/// Encode a validated whole-field write for a descriptor.
pub(super) fn encode_field_write(
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

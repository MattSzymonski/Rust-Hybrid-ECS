//! Generic, type-erased component field access for editor-style tools.
//!
//! # Responsibilities
//!
//! - Own the field vocabulary an inspector speaks: [`FieldValue`], the
//!   [`ComponentFieldError`] every refusal reports, and the tag helpers that
//!   name a field's kind (`is_handle_tag`, `is_entity_tag`, `is_container_tag`).
//! - Keep the public paths this module has always had: `FieldValue` and
//!   `ComponentFieldError` are what the rest of the engine names, and
//!   `MAX_EXPANDED_ELEMENTS` / `format_component_debug` are re-exported from the
//!   files that own them.
//!
//! # Design
//!
//! Components are stored in erased columns with no `dyn Any` downcast, so the
//! field layout (`name`, `type_tag`, `offset`, `size`, `align`,
//! `element_count`) that `#[derive(PillComponent)]` / `#[derive(PillMirror)]`
//! register is the only generic way to reach an arbitrary component's value.
//! This module is the safe wrapper over that data, split by concern:
//!
//! - `mod.rs` (here) - the vocabulary, the errors and the tag helpers.
//! - `row.rs` - resolving a component's field to a live row, and reading or
//!   writing it. Every entry point resolves through **the entity's own
//!   archetype**, the one place that names the generation which created the
//!   data (a registry-wide name lookup can see several registrations under one
//!   name); nothing hands a raw pointer to the caller.
//! - `codec.rs` - turning a row's bytes into a [`FieldValue`] and back.
//! - `expand.rs` - flattening a nested `struct:` field into leaf rows at
//!   registration.
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
//! A tag is the derive's name for a field's type, not a type itself:
//! `f32`/`u64`/`bool` are scalars, `struct:Handle<Mesh>` an asset reference,
//! `entity` an entity reference, `array:<tag>` a fixed array and
//! `struct:<path>` anything the engine cannot interpret. Reading a tag is always
//! a string comparison against this closed set, never a type lookup.
//!
//! # Row byte convention
//!
//! A row's bytes are native-endian, including an asset handle's two `u32`s
//! (index, then generation) and an entity reference's slot and generation. The
//! C# mirror reads the same row from the same process, so no byte order has to
//! cross a machine boundary; a field that ever does would need its own stated
//! convention here.

// Module tree: the vocabulary, the errors and the tag helpers live here; the
// row layer, the codec and the registration-time expansion are files beside
// this one, each declared with the re-export that keeps its public path.
mod codec;
mod expand;
mod row;

use codec::ScalarKind;

pub use expand::MAX_EXPANDED_ELEMENTS;
pub use row::format_component_debug;

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

/// Whether a type tag is a `Handle<T>` field's (`struct:Handle<Mesh>`, or
/// `struct:Handle` when the derive could not read the type argument).
pub(crate) fn is_handle_tag(tag: &str) -> bool {
    tag == "struct:Handle" || tag.starts_with("struct:Handle<")
}

/// Whether a type tag is an `Entity` field's.
pub(crate) fn is_entity_tag(tag: &str) -> bool {
    tag == "entity"
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests;

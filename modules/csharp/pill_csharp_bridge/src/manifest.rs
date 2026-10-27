//! Managed component manifest schema, validation, and engine type mapping.
//!
//! # Responsibilities
//!
//! - Define the manifest entries a managed assembly submits: components and
//!   resources, with their identities, sizes and field trees.
//! - Validate field trees against the blittable vocabulary the byte-level
//!   column storage requires.
//! - Map managed field types onto the engine's field descriptors, interning
//!   the strings a manifest-declared component has no static to borrow from.
//!
//! # Design
//!
//! A manifest is the managed side's only description of a component - identity,
//! layout and fields - transferred as JSON bytes in the pending-manifest
//! payload. The storage it feeds is a raw byte buffer, so the validation here
//! is the enforcement behind that storage's `Send`/`Sync` claim: every field
//! must be a blittable value with no ownership and nothing to release.
//!
//! The types stay `Deserialize`-only: the host parses a manifest and never
//! writes one, so there is no serializer to keep in step with.

// Standard library
use std::collections::{HashMap, HashSet};

// External crates
use pill_core::error::CSharpError;
use pill_engine::archetype::{FieldPlan, LayoutField};
use pill_engine::component_registry::{ComponentFieldDescriptor, FieldDecorator};
use pill_engine::{ComponentId, Engine};
use serde::Deserialize;

// Current crate
use super::components::{stable_component_id, StableComponentId};
use super::resources::{ManagedResourceDeclaration, ResourceFieldLayout};

/// Maximum nesting depth accepted in a managed component field tree.
///
/// Real component layouts never exceed a handful of levels. The budget stays
/// below `serde_json`'s own parser recursion limit so this validation, not an
/// opaque parser error, rejects pathological manifests.
const MAX_FIELD_NESTING_DEPTH: usize = 32;

/// One accepted leaf type's whole meaning to the host.
///
/// The manifest vocabulary is read by four separate checks - blittability,
/// natural alignment, the engine type tag and default-literal parsing - and all
/// four read their answer from this table: a type is accepted exactly when it
/// has a row here, so the checks cannot disagree about what a type means.
struct BlittableLeaf {
    /// Canonical managed type name as the manifest writes it.
    type_name: &'static str,
    /// Natural alignment the managed side places the field at.
    alignment: usize,
    /// Engine type tag, or `None` for a type the engine cannot decode: the
    /// field keeps its bytes in storage but is left out of the registered
    /// editor layout.
    tag: Option<&'static str>,
    /// How a `default:` literal for this type parses.
    default_literal: DefaultLiteral,
}

/// How a leaf type's `default:` literal turns into native bytes.
#[derive(Clone, Copy)]
enum DefaultLiteral {
    /// `true` or `false`.
    Boolean,
    /// An integer or float literal of this exact width.
    Signed8,
    Signed16,
    Signed32,
    Signed64,
    Unsigned8,
    Unsigned16,
    Unsigned32,
    Unsigned64,
    Float32,
    Float64,
    /// No supported literal form. The pointer-width types land here, so a
    /// manifest declaring such a default is refused by name.
    Unsupported,
}

/// The managed blittable field vocabulary: one row per accepted leaf type.
///
/// This is the enforcement behind `ComponentColumn`'s `unsafe impl Send`/`Sync`
/// and its lack of drop glue. That storage is a raw byte buffer: rows are moved
/// with `ptr::copy` and the buffer is freed without running any destructor, so
/// every field must be a blittable value with no ownership, no interior
/// pointer, and nothing to release.
///
/// A nested value type is a shape, not a leaf: it has no row here, and
/// validation recurses into its own fields instead of looking it up.
const BLITTABLE_LEAVES: &[BlittableLeaf] = &[
    BlittableLeaf {
        type_name: "System.Byte",
        alignment: 1,
        tag: Some("u8"),
        default_literal: DefaultLiteral::Unsigned8,
    },
    BlittableLeaf {
        type_name: "System.SByte",
        alignment: 1,
        tag: Some("i8"),
        default_literal: DefaultLiteral::Signed8,
    },
    BlittableLeaf {
        type_name: "System.Int16",
        alignment: 2,
        tag: Some("i16"),
        default_literal: DefaultLiteral::Signed16,
    },
    BlittableLeaf {
        type_name: "System.UInt16",
        alignment: 2,
        tag: Some("u16"),
        default_literal: DefaultLiteral::Unsigned16,
    },
    BlittableLeaf {
        type_name: "System.Int32",
        alignment: 4,
        tag: Some("i32"),
        default_literal: DefaultLiteral::Signed32,
    },
    BlittableLeaf {
        type_name: "System.UInt32",
        alignment: 4,
        tag: Some("u32"),
        default_literal: DefaultLiteral::Unsigned32,
    },
    BlittableLeaf {
        type_name: "System.Int64",
        alignment: 8,
        tag: Some("i64"),
        default_literal: DefaultLiteral::Signed64,
    },
    BlittableLeaf {
        type_name: "System.UInt64",
        alignment: 8,
        tag: Some("u64"),
        default_literal: DefaultLiteral::Unsigned64,
    },
    // Pointer-width on the host, which is what the managed side marshals them
    // as; the engine only ever stores their bytes.
    BlittableLeaf {
        type_name: "System.IntPtr",
        alignment: std::mem::size_of::<usize>(),
        tag: None,
        default_literal: DefaultLiteral::Unsupported,
    },
    BlittableLeaf {
        type_name: "System.UIntPtr",
        alignment: std::mem::size_of::<usize>(),
        tag: None,
        default_literal: DefaultLiteral::Unsupported,
    },
    BlittableLeaf {
        type_name: "System.Single",
        alignment: 4,
        tag: Some("f32"),
        default_literal: DefaultLiteral::Float32,
    },
    BlittableLeaf {
        type_name: "System.Double",
        alignment: 8,
        tag: Some("f64"),
        default_literal: DefaultLiteral::Float64,
    },
    BlittableLeaf {
        type_name: "System.Boolean",
        alignment: 1,
        tag: Some("bool"),
        default_literal: DefaultLiteral::Boolean,
    },
    // A blittable UTF-16 code unit with the same size as the engine's `u16`;
    // exposing it that way keeps the field editable.
    BlittableLeaf {
        type_name: "System.Char",
        alignment: 2,
        tag: Some("u16"),
        default_literal: DefaultLiteral::Unsigned16,
    },
    // A reference to another entity: the runtime's `Entity`
    // handle, which is the Rust handle's mirror - a `u64` slot id and a `u32`
    // generation. Eight-byte aligned because of the `u64`, and with no
    // default: an unset reference is the world's null handle.
    BlittableLeaf {
        type_name: "Entity",
        alignment: 8,
        tag: Some("entity"),
        default_literal: DefaultLiteral::Unsupported,
    },
];

/// The nested-struct marker: a shape validation descends into, not a leaf.
const STRUCT_TYPE: &str = "struct";

/// The prefix of a `Handle<T>` field's type: `handle:` and the Rust type path
/// of `T`, as the C# mirror of `T` carries it.
const HANDLE_PREFIX: &str = "handle:";

/// A handle's size and alignment: two `u32`s, `index` then `generation`.
const HANDLE_SIZE: usize = 8;
const HANDLE_ALIGNMENT: usize = 4;

/// The Rust type path of a handle field's asset type, when the field is one.
pub(super) fn handle_asset_path(primitive_type: &str) -> Option<&str> {
    primitive_type
        .strip_prefix(HANDLE_PREFIX)
        .filter(|path| !path.is_empty())
}

/// The engine tag a handle field is recorded under: `struct:Handle<Mesh>`,
/// the last segment of the asset type's path, as the derive writes it.
fn handle_tag(asset_path: &str) -> &'static str {
    let short = asset_path.rsplit("::").next().unwrap_or(asset_path);
    intern(&format!("struct:Handle<{short}>"))
}

/// The vocabulary row for one managed type, when it is an accepted leaf.
fn blittable_leaf(primitive_type: &str) -> Option<&'static BlittableLeaf> {
    BLITTABLE_LEAVES
        .iter()
        .find(|leaf| leaf.type_name == primitive_type)
}

/// Whether a manifest may declare a field of this type: an accepted leaf, or
/// a nested value struct whose own fields are validated recursively against
/// this same vocabulary.
fn is_blittable(primitive_type: &str) -> bool {
    primitive_type == STRUCT_TYPE
        || blittable_leaf(primitive_type).is_some()
        || handle_asset_path(primitive_type).is_some()
}

/// Deserialized entry from the managed component manifest.
#[derive(Deserialize)]
pub(super) struct ManagedComponentManifest {
    /// Low 64 bits of the stable component identity.
    pub(super) stable_id_low: u64,
    /// High 64 bits of the stable component identity.
    pub(super) stable_id_high: u64,
    /// Canonical full name used to recompute and verify the identity.
    pub(super) full_name: String,
    /// Total byte size of the component layout.
    pub(super) size: usize,
    /// Required byte alignment of the component layout.
    pub(super) alignment: usize,
    /// Hash of the managed field schema used to match native mirrors.
    pub(super) schema_hash: u64,
    /// Whether the managed side expects a native engine binding.
    pub(super) shared: bool,
    /// What the entry declares: a component, or a resource.
    ///
    /// Defaulted so a payload that omits the tag parses as a list of
    /// components: an older generation's manifest stays readable during a
    /// reload that straddles the change.
    #[serde(default)]
    pub(super) kind: ManifestEntryKind,
    /// The names of live registrations this entry renames onto itself.
    ///
    /// A rename is expressed here rather than as a new entry: the host resolves
    /// each alias to the registration that answered to it and moves that
    /// registration's rows onto this entry, so the rename reads as a
    /// migration instead of a retirement plus an add. Defaulted for the same
    /// reason `kind` is.
    #[serde(default)]
    pub(super) aliases: Vec<String>,
    /// Top-level field descriptions of the component layout.
    pub(super) fields: Vec<ManagedFieldManifest>,
}

/// What one managed manifest entry declares.
///
/// Resources ride in the same array as components rather than a second
/// document, so one transfer and one registration transaction cover a whole
/// generation's declaration - a resource that registered while its project's
/// components were refused would be a half-applied manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ManifestEntryKind {
    /// Per-entity storage, registered as a column.
    #[default]
    Component,
    /// One value for the whole world, registered as a foreign resource.
    Resource,
}

/// Deserialized field entry within a managed component manifest.
#[derive(Deserialize)]
pub(super) struct ManagedFieldManifest {
    /// Field name as it appears in the managed schema.
    pub(super) name: String,
    /// Byte offset of the field within its containing struct.
    pub(super) offset: usize,
    /// Byte size of the field.
    pub(super) size: usize,
    /// Canonical managed type name of the field.
    pub(super) primitive_type: String,
    /// Literal a newly added or reset field starts from, instead of zero.
    #[serde(default)]
    pub(super) default: Option<String>,
    /// Presentation decorators the field declares, in declaration order.
    ///
    /// Defaulted so a payload that omits them parses as an undecorated field.
    #[serde(default)]
    pub(super) decorations: Vec<ManagedFieldDecorator>,
    /// Nested field descriptions when this field is a struct.
    pub(super) fields: Vec<ManagedFieldManifest>,
}

/// One presentation decorator a managed field declares.
///
/// Mirrors the editor's [`FieldDecorator`](pill_engine::component_registry::FieldDecorator)
/// vocabulary. The managed side writes one flat record for the whole
/// vocabulary, so unused payload members arrive as `null` and a variant reads
/// only the members its kind carries. [`validate_decorations`] refuses a
/// payload that cannot mean what it says; nothing here affects storage.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ManagedFieldDecorator {
    /// Unity's `[HideInInspector]`: the Inspector draws no row for the field.
    HideInInspector {},
    /// A separator line with text, drawn above the field.
    Header {
        /// Text the line shows.
        text: String,
    },
    /// A vertical gap above the field.
    Space {
        /// Height of the gap, in pixels.
        number: f32,
    },
    /// Hover text on the field's label.
    Tooltip {
        /// Text shown while the pointer rests on the label.
        text: String,
    },
    /// A slider over the range.
    Range {
        /// Lowest value the slider offers.
        min: f32,
        /// Highest value the slider offers.
        max: f32,
    },
    /// A multi-line text box.
    TextArea {
        /// Rows the box starts with.
        min_lines: usize,
        /// Rows the box grows to before it scrolls.
        max_lines: usize,
    },
    /// A fixed-height multi-line text box.
    Multiline {
        /// Rows the box shows.
        lines: usize,
    },
    /// A drag value that never goes below the minimum.
    Min {
        /// Lowest value the drag value accepts.
        number: f32,
    },
}

/// Reject sibling fields that share any byte range.
///
/// Conflicting interpretations of the same storage would corrupt data
/// silently, so overlaps and duplicated offsets are invalid layouts.
///
/// # Errors
///
/// Returns an error naming the first pair of sibling fields that overlap.
pub(super) fn validate_sibling_non_overlap(
    fields: &[ManagedFieldManifest],
    parent_name: &str,
) -> Result<(), String> {
    for (index, left) in fields.iter().enumerate() {
        let left_end = left.offset.saturating_add(left.size);
        for right in &fields[index + 1..] {
            let right_end = right.offset.saturating_add(right.size);
            if left.offset < right_end && right.offset < left_end {
                return Err(format!(
                    "managed fields {} and {} overlap inside {parent_name}",
                    left.name, right.name
                ));
            }
        }
    }
    Ok(())
}

/// Verify that a field and every nested field fit within the byte range of
/// the struct that directly contains it, and that sibling fields never
/// overlap.
///
/// The field tree is walked with an explicit worklist so deeply nested input
/// consumes heap rather than stack, and the depth budget rejects pathological
/// manifests before they cost real work.
///
/// # Errors
///
/// Returns an error when a field overflows its containing struct, names an
/// empty field or type, declares a type outside the [`BLITTABLE_LEAVES`]
/// vocabulary, sits at an offset that is not a multiple of its natural
/// alignment, or exceeds the maximum nesting depth.
pub(super) fn validate_field_manifest(
    field: &ManagedFieldManifest,
    parent_size: usize,
) -> Result<(), String> {
    // Each entry carries the field to inspect, the size of the struct that
    // directly contains it, and that branch's current nesting depth.
    let mut worklist = vec![(field, parent_size, 0_usize)];
    while let Some((field, parent_size, depth)) = worklist.pop() {
        let end = field
            .offset
            .checked_add(field.size)
            .ok_or("managed field range overflow")?;
        if field.name.is_empty() || field.primitive_type.is_empty() || end > parent_size {
            return Err("managed field lies outside its component layout".into());
        }
        // Reject anything that is not a blittable value type. `ComponentColumn`
        // copies rows as raw bytes and frees its buffer without running drop
        // glue, so a field owning a resource would be duplicated on move and
        // leaked on free - and sharing such a column across threads, which the
        // engine does, would be unsound.
        if !is_blittable(&field.primitive_type) {
            return Err(format!(
                "managed field {} has non-blittable type {}; descriptor components must contain only unmanaged value types",
                field.name, field.primitive_type
            ));
        }
        // A field placed off its natural alignment describes a packed or
        // hand-written layout. The analyzer refuses `StructLayout.Pack` on the
        // managed side (PILL0402), but the analyzer is a compile-time lint on
        // one project's source while this manifest is the runtime trust
        // boundary - a project built without the analyzer reference reaches the
        // host with nothing in between.
        //
        // The offset is relative to the containing struct, and a struct's own
        // offset is checked against its parent by the same rule, so a field is
        // correctly aligned within the component exactly when every step of
        // that chain is.
        // A handle is exactly the engine's `Handle<T>`: two `u32`s, aligned to
        // four, with no default (an unset reference is the invalid handle).
        if handle_asset_path(&field.primitive_type).is_some()
            && (field.size != HANDLE_SIZE
                || field.offset % HANDLE_ALIGNMENT != 0
                || field.default.is_some())
        {
            return Err(format!(
                "managed handle field {} must be {HANDLE_SIZE} bytes at a {HANDLE_ALIGNMENT}-byte boundary with no default",
                field.name
            ));
        }
        if let Some(leaf) = blittable_leaf(&field.primitive_type) {
            if field.offset % leaf.alignment != 0 {
                return Err(format!(
                    "managed field {} of type {} sits at offset {}, which is not a multiple of its {}-byte alignment",
                    field.name, field.primitive_type, field.offset, leaf.alignment
                ));
            }
        }
        // A default is for the leaves: a struct has no literal form, and a
        // nested default would be a second layout language to validate. The
        // literal is checked against the field's own type here, so a mismatch
        // is a named refusal rather than a marshalling surprise later.
        if let Some(literal) = &field.default {
            if !field.fields.is_empty() {
                return Err(format!(
                    "managed field {} declares a default, but defaults are only supported on primitive fields",
                    field.name
                ));
            }
            let bytes = parse_default_bytes(&field.primitive_type, literal)?;
            if bytes.len() != field.size {
                return Err(format!(
                    "managed field {} declares a default of {} byte(s) for a {}-byte field",
                    field.name,
                    bytes.len(),
                    field.size
                ));
            }
        }
        // The depth check runs after the field validates so the error always
        // names a well-formed field.
        validate_decorations(&field.decorations, &field.name)?;
        if depth >= MAX_FIELD_NESTING_DEPTH {
            return Err(format!(
                "managed field {} exceeds the maximum nesting depth of {MAX_FIELD_NESTING_DEPTH}",
                field.name
            ));
        }
        validate_sibling_non_overlap(&field.fields, &field.name)?;
        for nested in &field.fields {
            worklist.push((nested, field.size, depth + 1));
        }
    }
    Ok(())
}

/// Reject a presentation decorator that cannot mean what it says.
///
/// Presentation only, but a slider whose minimum is above its maximum, or a
/// text box with zero lines, is a declaration mistake the managed side cannot
/// see - Unity silently ignores bad ranges, and a silently ignored decorator
/// is worse than a refused manifest.
///
/// # Errors
///
/// Returns an error naming the field and the decorator that is out of range.
pub(super) fn validate_decorations(
    decorations: &[ManagedFieldDecorator],
    field_name: &str,
) -> Result<(), String> {
    for decoration in decorations {
        match decoration {
            ManagedFieldDecorator::HideInInspector {} | ManagedFieldDecorator::Header { .. } => {}
            ManagedFieldDecorator::Tooltip { .. } => {}
            ManagedFieldDecorator::Space { number } if !number.is_finite() || *number < 0.0 => {
                return Err(format!(
                    "managed field {field_name} declares a space of {number}, which is not a finite, non-negative height"
                ));
            }
            ManagedFieldDecorator::Range { min, max }
                if !min.is_finite() || !max.is_finite() || min > max =>
            {
                return Err(format!(
                    "managed field {field_name} declares the range {min}..={max}, which needs finite bounds with the minimum first"
                ));
            }
            ManagedFieldDecorator::TextArea {
                min_lines,
                max_lines,
            } if *min_lines == 0 || min_lines > max_lines => {
                return Err(format!(
                    "managed field {field_name} declares a text area of {min_lines}..{max_lines} lines, which needs at least one line with the minimum first"
                ));
            }
            ManagedFieldDecorator::Multiline { lines } if *lines == 0 => {
                return Err(format!(
                    "managed field {field_name} declares a multiline box of zero lines"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// The editor decorators a managed field declares, as the layout carries them.
///
/// Payload strings are owned here: a managed declaration's text arrives in the
/// parsed JSON, not in a static, so the descriptor owns it. An empty list stays
/// the borrowed empty slice every undecorated field shares.
pub(super) fn field_decorations(
    decorations: &[ManagedFieldDecorator],
) -> std::borrow::Cow<'static, [FieldDecorator]> {
    if decorations.is_empty() {
        return std::borrow::Cow::Borrowed(&[]);
    }
    std::borrow::Cow::Owned(
        decorations
            .iter()
            .map(|decoration| match decoration {
                ManagedFieldDecorator::HideInInspector {} => FieldDecorator::HideInInspector,
                ManagedFieldDecorator::Header { text } => {
                    FieldDecorator::Header(text.clone().into())
                }
                ManagedFieldDecorator::Space { number } => FieldDecorator::Space(*number),
                ManagedFieldDecorator::Tooltip { text } => {
                    FieldDecorator::Tooltip(text.clone().into())
                }
                ManagedFieldDecorator::Range { min, max } => FieldDecorator::Range {
                    min: *min,
                    max: *max,
                },
                ManagedFieldDecorator::TextArea {
                    min_lines,
                    max_lines,
                } => FieldDecorator::TextArea {
                    min_lines: *min_lines,
                    max_lines: *max_lines,
                },
                ManagedFieldDecorator::Multiline { lines } => FieldDecorator::Multiline(*lines),
                ManagedFieldDecorator::Min { number } => FieldDecorator::Min(*number),
            })
            .collect(),
    )
}

/// The engine-vocabulary tag one manifest field carries.
///
/// Mirrors what [`managed_field_layout`] records for the same field, so a plan
/// built from a manifest compares like with like. A leaf the engine cannot
/// decode (the pointer-width types) carries no tag and reads `unsupported`.
pub(super) fn manifest_field_tag(field: &ManagedFieldManifest) -> &'static str {
    if field.primitive_type == STRUCT_TYPE {
        return STRUCT_TYPE;
    }
    if let Some(asset_path) = handle_asset_path(&field.primitive_type) {
        return handle_tag(asset_path);
    }
    blittable_leaf(&field.primitive_type)
        .and_then(|leaf| leaf.tag)
        .unwrap_or("unsupported")
}

/// Parse one field default literal into native-endian bytes.
///
/// Deliberately literals only: an expression the AOT posture could not
/// evaluate would be a default that exists in the manifest and not in the
/// running world. The literal is checked against the field's own declared
/// type, so a mismatch is a named validation error rather than a marshalling
/// surprise later and the managed side is free to format its number however
/// the language does.
pub(super) fn parse_default_bytes(primitive_type: &str, literal: &str) -> Result<Vec<u8>, String> {
    let trimmed = literal.trim();
    let invalid = || format!("field default `{literal}` is not a valid {primitive_type} literal");
    let Some(leaf) = blittable_leaf(primitive_type) else {
        return Err(format!(
            "field default `{literal}` names a type with no supported literal form ({primitive_type})"
        ));
    };
    match leaf.default_literal {
        DefaultLiteral::Boolean => match trimmed {
            "true" => Ok(vec![1]),
            "false" => Ok(vec![0]),
            _ => Err(invalid()),
        },
        DefaultLiteral::Signed8 => trimmed
            .parse::<i8>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Signed16 => trimmed
            .parse::<i16>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Signed32 => trimmed
            .parse::<i32>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Signed64 => trimmed
            .parse::<i64>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        // UTF-16 code units parse as the engine's `u16` alongside the real
        // unsigned 16-bit type.
        DefaultLiteral::Unsigned8 => trimmed
            .parse::<u8>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Unsigned16 => trimmed
            .parse::<u16>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Unsigned32 => trimmed
            .parse::<u32>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Unsigned64 => trimmed
            .parse::<u64>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Float32 => trimmed
            .parse::<f32>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Float64 => trimmed
            .parse::<f64>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        DefaultLiteral::Unsupported => Err(format!(
            "field default `{literal}` names a type with no supported literal form ({primitive_type})"
        )),
    }
}

/// Reduce a field's type tag to what the migration plan should compare.
///
/// The registry records a nested struct as `struct:<owner>::<field>` for the
/// editor's benefit, while a manifest only knows that the field is a struct.
/// Comparing those verbatim would mark every nested field retyped - and would
/// do it again whenever a component is renamed, because the owner is part of
/// the tag. Both sides are folded to `struct`, so the plan asks the question it
/// actually means: is this field still a struct?
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) fn plan_tag(type_tag: &str) -> &str {
    if type_tag.starts_with("struct:") {
        "struct"
    } else {
        type_tag
    }
}

/// Strings handed to the engine as `&'static str`, deduplicated by content.
///
/// `ComponentFieldDescriptor` holds `&'static str` because the derive builds it
/// in an artifact's static data. A manifest-declared component has no statics
/// to borrow from, so its strings have to be given the same lifetime by hand.
///
/// A bare `Box::leak` per registration would grow with the reload count, since
/// the C# project re-registers its components on every reload. Interning bounds
/// the cost by the project's type set: a name a component keeps across a
/// hundred reloads is stored once.
static INTERNED_FIELD_STRINGS: std::sync::Mutex<Option<HashSet<&'static str>>> =
    std::sync::Mutex::new(None);

/// Return a `&'static str` equal to `value`, allocating only on first sight.
pub(super) fn intern(value: &str) -> &'static str {
    let mut guard = INTERNED_FIELD_STRINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let table = guard.get_or_insert_with(HashSet::new);
    if let Some(existing) = table.get(value) {
        return existing;
    }
    // First sight: this is the one allocation the string ever costs.
    let leaked: &'static str = Box::leak(value.to_owned().into_boxed_str());
    table.insert(leaked);
    leaked
}

/// Convert a managed component manifest's field tree into engine descriptors.
///
/// Primitive leaves map onto the engine's type-tag vocabulary so the editor
/// can decode and edit them. Nested `struct:` fields stay opaque: the engine
/// has no struct walking, so their bytes are visible but not interpretable.
/// Field names and struct tags are interned, so a component re-registered on
/// every reload costs its strings once rather than once per reload.
pub(super) fn managed_field_layout(
    component_name: &str,
    fields: &[ManagedFieldManifest],
) -> Vec<ComponentFieldDescriptor> {
    let mut layout = Vec::new();
    for field in fields {
        // A handle is a leaf the engine decodes as an asset reference; its
        // asset type is recorded beside the layout (`managed_asset_fields`).
        if let Some(asset_path) = handle_asset_path(&field.primitive_type) {
            layout.push(ComponentFieldDescriptor {
                name: intern(&field.name),
                type_tag: handle_tag(asset_path),
                offset: field.offset,
                size: field.size,
                align: HANDLE_ALIGNMENT,
                element_count: 0,
                decorators: field_decorations(&field.decorations),
            });
            continue;
        }
        if field.primitive_type == STRUCT_TYPE {
            layout.push(ComponentFieldDescriptor {
                name: intern(&field.name),
                type_tag: intern(&format!("struct:{component_name}::{}", field.name)),
                offset: field.offset,
                size: field.size,
                align: 1,
                element_count: 0,
                decorators: field_decorations(&field.decorations),
            });
            continue;
        }
        // A leaf the engine cannot decode keeps its bytes in storage but is
        // omitted from the registered layout.
        let Some(type_tag) = blittable_leaf(&field.primitive_type).and_then(|leaf| leaf.tag) else {
            continue;
        };
        layout.push(ComponentFieldDescriptor {
            name: intern(&field.name),
            type_tag,
            offset: field.offset,
            size: field.size,
            align: 1,
            element_count: 0,
            decorators: field_decorations(&field.decorations),
        });
    }
    layout
}

/// Render a registered field layout as one stable, parseable line so
/// integration suites can assert the managed manifest reached the engine
/// intact: `name@offset:size:type_tag` entries joined by `|`.
pub(super) fn format_field_layout_line(layout: &[ComponentFieldDescriptor]) -> String {
    layout
        .iter()
        .map(|field| {
            format!(
                "{}@{}:{}:{}",
                field.name, field.offset, field.size, field.type_tag
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Parse a managed manifest and check every entry against its own identity.
///
/// Every check here is a property of the manifest alone - identity, uniqueness,
/// and the shape of each layout - so startup and reload both run it before
/// either touches the world, and so both validate exactly the same thing.
pub(super) fn parse_and_validate_manifest(
    bytes: &[u8],
) -> Result<Vec<ManagedComponentManifest>, CSharpError> {
    // Step 1: Parse and validate every entry against canonical identities.
    let manifest: Vec<ManagedComponentManifest> = serde_json::from_slice(bytes)?;
    let mut seen = HashSet::new();
    let mut alias_owners: HashMap<&str, &str> = HashMap::new();
    for component in &manifest {
        let stable_id =
            StableComponentId::from_halves(component.stable_id_low, component.stable_id_high);
        if stable_component_id(&component.full_name) != stable_id {
            return Err(format!(
                "managed component {} has an ID that does not match its canonical full name",
                component.full_name
            )
            .into());
        }
        if !seen.insert(stable_id) {
            // Components and resources share one identity space, so this also
            // catches a resource whose declared name collides with a component's
            // type name - two entries that would answer to one slot.
            return Err(format!(
                "duplicate declaration {} in managed manifest",
                component.full_name
            )
            .into());
        }
        for alias in &component.aliases {
            // Resolution is one hop against live names only, so an alias that
            // is itself a declared name (or another entry's alias) has no
            // unambiguous predecessor to find. Refused here rather than
            // resolved by precedence, because the wrong choice would move a
            // live registration's rows.
            if alias.trim().is_empty() {
                return Err(format!(
                    "managed component {} declares an empty alias",
                    component.full_name
                )
                .into());
            }
            if manifest.iter().any(|other| other.full_name == *alias) {
                return Err(format!(
                    "managed component {} declares alias `{alias}`, which is a live declaration",
                    component.full_name
                )
                .into());
            }
            if let Some(owner) = alias_owners.insert(alias.as_str(), &component.full_name) {
                return Err(format!(
                    "alias `{alias}` is declared by both {owner} and {}",
                    component.full_name
                )
                .into());
            }
        }
        if component.size == 0
            || u32::try_from(component.size).is_err()
            || component.alignment == 0
            || !component.alignment.is_power_of_two()
            || std::alloc::Layout::from_size_align(component.size, component.alignment).is_err()
        {
            return Err(format!(
                "invalid layout for managed component {}",
                component.full_name
            )
            .into());
        }

        // Sibling fields of the component itself must not overlap either.
        validate_sibling_non_overlap(&component.fields, &component.full_name)?;
        for field in &component.fields {
            validate_field_manifest(field, component.size)?;
        }

        // A resource is a singleton, so "the host already binds this natively"
        // has nothing to mean for one: there is no column to bind and no native
        // mirror to validate against. Refused where it is declared rather than
        // silently ignored, so a mis-marked struct is a named error.
        if component.kind == ManifestEntryKind::Resource && component.shared {
            return Err(format!(
                "managed resource {} is marked shared; resources have no native binding",
                component.full_name
            )
            .into());
        }
    }
    Ok(manifest)
}

/// Split one parsed manifest into its component and resource halves.
///
/// Both halves went through the same identity, layout and field validation
/// above; only what they are registered as differs.
pub(super) fn split_manifest_kinds(
    manifest: Vec<ManagedComponentManifest>,
) -> (
    Vec<ManagedComponentManifest>,
    Vec<ManagedResourceDeclaration>,
) {
    let mut components = Vec::new();
    let mut resources = Vec::new();
    for entry in manifest {
        match entry.kind {
            ManifestEntryKind::Component => components.push(entry),
            ManifestEntryKind::Resource => {
                // The tags are folded through the same function the component
                // plan uses, so a resource migration compares the two sides of
                // its diff in one vocabulary rather than marking every nested
                // field retyped.
                let fields = entry
                    .fields
                    .iter()
                    .map(|field| ResourceFieldLayout {
                        name: field.name.clone(),
                        type_tag: manifest_field_tag(field).to_owned(),
                        offset: field.offset,
                        size: field.size,
                        default: field.default.as_ref().map(|literal| {
                            parse_default_bytes(&field.primitive_type, literal)
                                .expect("the field default was validated during parsing")
                        }),
                        decorations: field_decorations(&field.decorations).into_owned(),
                    })
                    .collect();
                resources.push(ManagedResourceDeclaration {
                    full_name: entry.full_name,
                    size: entry.size,
                    align: entry.alignment,
                    schema_hash: entry.schema_hash,
                    aliases: entry.aliases,
                    fields,
                });
            }
        }
    }
    (components, resources)
}

/// Build the byte plan from the layout a component has now to the one a
/// manifest asks for.
///
/// The old side comes from the world rather than from the previous manifest:
/// the field layout registered with a descriptor component is the layout its
/// columns actually use, which is the only thing a byte copy can be measured
/// against.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) fn build_field_plan(
    engine: &Engine,
    component_id: ComponentId,
    fields: &[ManagedFieldManifest],
) -> FieldPlan {
    let previous: Vec<LayoutField<'_>> = engine
        .world()
        .component_field_layout(component_id)
        .unwrap_or(&[])
        .iter()
        .map(|field| LayoutField {
            name: field.name,
            type_tag: plan_tag(field.type_tag),
            offset: field.offset,
            size: field.size,
        })
        .collect();
    // The manifest names a managed type; mapping it through the same function
    // `managed_field_layout` uses is what makes the two sides of the diff
    // comparable at all - otherwise every field would look retyped.
    let next: Vec<LayoutField<'_>> = fields
        .iter()
        .map(|field| LayoutField {
            name: field.name.as_str(),
            type_tag: manifest_field_tag(field),
            offset: field.offset,
            size: field.size,
        })
        .collect();
    let mut plan = FieldPlan::between(&previous, &next);
    // Declared defaults fill the fields the diff leaves empty - an added field
    // or one whose type changed. `set_default` is a no-op on a copied field,
    // which is what keeps a changed default from masking a carried value.
    for field in fields {
        let Some(literal) = &field.default else {
            continue;
        };
        let bytes = parse_default_bytes(&field.primitive_type, literal)
            .expect("the manifest default was validated before the plan was built");
        plan.set_default(field.offset, &bytes)
            .expect("the manifest default was validated against the planned field");
    }
    plan
}

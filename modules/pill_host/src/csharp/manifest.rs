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
use std::collections::HashSet;

// External crates
use pill_core::error::CSharpError;
use pill_engine::component_registry::ComponentFieldDescriptor;
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

/// Field types a dynamic component is allowed to contain.
///
/// This is the enforcement behind `DynamicColumn`'s `unsafe impl Send`/`Sync`
/// and its lack of drop glue. That storage is a raw byte buffer: rows are moved
/// with `ptr::copy` and the buffer is freed without running any destructor, so
/// every field must be a blittable value with no ownership, no interior
/// pointer, and nothing to release.
///
/// Before this list existed the only check on a field's type was that its name
/// was non-empty, so a manifest declaring a managed reference passed validation
/// and the resulting column was shared across threads on a promise nothing
/// verified.
///
/// `"struct"` denotes a nested value type; its own fields are validated
/// recursively against this same list, so allowing it does not open a hole.
const BLITTABLE_FIELD_TYPES: &[&str] = &[
    "System.Byte",
    "System.SByte",
    "System.Int16",
    "System.UInt16",
    "System.Int32",
    "System.UInt32",
    "System.Int64",
    "System.UInt64",
    "System.IntPtr",
    "System.UIntPtr",
    "System.Single",
    "System.Double",
    "System.Boolean",
    "System.Char",
    "struct",
];

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
    /// Defaulted so a manifest written before resources existed still parses as
    /// a list of components; the managed side always writes the tag now, and
    /// the default is what keeps an older generation's payload readable during
    /// a reload that straddles the change.
    #[serde(default)]
    pub(super) kind: ManifestEntryKind,
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
    /// Nested field descriptions when this field is a struct.
    pub(super) fields: Vec<ManagedFieldManifest>,
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
/// empty field or type, declares a type outside [`BLITTABLE_FIELD_TYPES`], or
/// exceeds the maximum nesting depth.
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
        // Reject anything that is not a blittable value type. `DynamicColumn`
        // copies rows as raw bytes and frees its buffer without running drop
        // glue, so a field owning a resource would be duplicated on move and
        // leaked on free - and sharing such a column across threads, which the
        // engine does, would be unsound.
        if !BLITTABLE_FIELD_TYPES.contains(&field.primitive_type.as_str()) {
            return Err(format!(
                "managed field {} has non-blittable type {}; dynamic components                  must contain only unmanaged value types",
                field.name, field.primitive_type
            ));
        }
        // The depth check runs after the field validates so the error always
        // names a well-formed field.
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

/// Map a managed primitive type onto the engine's field type-tag vocabulary.
///
/// Returns `None` for blittable types the engine cannot decode (the field is
/// then omitted from the registered layout but keeps its bytes in storage).
fn managed_primitive_tag(primitive_type: &str) -> Option<&'static str> {
    match primitive_type {
        "System.Byte" => Some("u8"),
        "System.SByte" => Some("i8"),
        "System.Int16" => Some("i16"),
        "System.UInt16" => Some("u16"),
        "System.Int32" => Some("i32"),
        "System.UInt32" => Some("u32"),
        "System.Int64" => Some("i64"),
        "System.UInt64" => Some("u64"),
        "System.Single" => Some("f32"),
        "System.Double" => Some("f64"),
        "System.Boolean" => Some("bool"),
        // `System.Char` is a blittable UTF-16 code unit with the same size as
        // the engine's `u16`; exposing it that way keeps the field editable.
        "System.Char" => Some("u16"),
        _ => None,
    }
}

/// The engine-vocabulary tag one manifest field carries.
///
/// Mirrors what [`managed_field_layout`] records for the same field, so a plan
/// built from a manifest compares like with like.
pub(super) fn manifest_field_tag(field: &ManagedFieldManifest) -> &'static str {
    if field.primitive_type == "struct" {
        return "struct";
    }
    managed_primitive_tag(&field.primitive_type).unwrap_or("unsupported")
}

/// Reduce a field's type tag to what the migration plan should compare.
///
/// The registry records a nested struct as `struct:<owner>::<field>` for the
/// editor's benefit, while a manifest only knows that the field is a struct.
/// Comparing those verbatim would mark every nested field retyped - and would
/// do it again whenever a component is renamed, because the owner is part of
/// the tag. Both sides are folded to `struct`, so the plan asks the question it
/// actually means: is this field still a struct?
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
/// Doing that with a bare `Box::leak` per registration is what this replaces:
/// the C# project re-registers its components on every reload, so the leak grew
/// with reload count rather than with the number of distinct names. Interning
/// bounds it by the project's type set, which is the intended cost - a name a
/// component keeps across a hundred reloads is stored once.
static INTERNED_FIELD_STRINGS: std::sync::Mutex<Option<HashSet<&'static str>>> =
    std::sync::Mutex::new(None);

/// Return a `&'static str` equal to `value`, allocating only on first sight.
fn intern(value: &str) -> &'static str {
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
        if field.primitive_type == "struct" {
            layout.push(ComponentFieldDescriptor {
                name: intern(&field.name),
                type_tag: intern(&format!("struct:{component_name}::{}", field.name)),
                offset: field.offset,
                size: field.size,
                align: 1,
                element_count: 0,
            });
            continue;
        }
        let Some(type_tag) = managed_primitive_tag(&field.primitive_type) else {
            continue;
        };
        layout.push(ComponentFieldDescriptor {
            name: intern(&field.name),
            type_tag,
            offset: field.offset,
            size: field.size,
            align: 1,
            element_count: 0,
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
/// Split out of `register_component_manifest` so the reload path validates
/// exactly what the startup path validates. Every check here is a property of
/// the manifest alone - identity, uniqueness, and the shape of each layout - so
/// both callers can run it before either touches the world.
pub(super) fn parse_and_validate_manifest(
    bytes: &[u8],
) -> Result<Vec<ManagedComponentManifest>, CSharpError> {
    // Step 1: Parse and validate every entry against canonical identities.
    let manifest: Vec<ManagedComponentManifest> = serde_json::from_slice(bytes)?;
    let mut seen = HashSet::new();
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
                    })
                    .collect();
                resources.push(ManagedResourceDeclaration {
                    full_name: entry.full_name,
                    size: entry.size,
                    align: entry.alignment,
                    schema_hash: entry.schema_hash,
                    fields,
                });
            }
        }
    }
    (components, resources)
}

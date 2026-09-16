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
use pill_engine::component_registry::ComponentFieldDescriptor;
use pill_engine::{ComponentId, Engine};
use serde::Deserialize;

// Current crate
use super::components::{
    check_binding_against_manifest, stable_component_id, BindingStore, ComponentBinding,
    RenameSource, StableComponentId,
};
use super::resources::{ManagedResourceDeclaration, ResourceFieldLayout};

/// Maximum nesting depth accepted in a managed component field tree.
///
/// Real component layouts never exceed a handful of levels. The budget stays
/// below `serde_json`'s own parser recursion limit so this validation, not an
/// opaque parser error, rejects pathological manifests.
const MAX_FIELD_NESTING_DEPTH: usize = 32;

/// Field types a descriptor component is allowed to contain.
///
/// This is the enforcement behind `ComponentColumn`'s `unsafe impl Send`/`Sync`
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
    /// Names this entry's type used to be declared under.
    ///
    /// A rename is expressed here rather than as a new entry: the host resolves
    /// each alias to the registration that answered to it and moves that
    /// registration's rows onto this entry, so the rename reads as a
    /// migration instead of a retirement plus an add. Defaulted for the same
    /// reason `kind` is - a manifest written before aliases existed still
    /// parses.
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
        // Reject anything that is not a blittable value type. `ComponentColumn`
        // copies rows as raw bytes and frees its buffer without running drop
        // glue, so a field owning a resource would be duplicated on move and
        // leaked on free - and sharing such a column across threads, which the
        // engine does, would be unsound.
        if !BLITTABLE_FIELD_TYPES.contains(&field.primitive_type.as_str()) {
            return Err(format!(
                "managed field {} has non-blittable type {}; descriptor components must contain only unmanaged value types",
                field.name, field.primitive_type
            ));
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
    match primitive_type {
        "System.Byte" => trimmed
            .parse::<u8>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.SByte" => trimmed
            .parse::<i8>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.Int16" => trimmed
            .parse::<i16>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.UInt16" | "System.Char" => trimmed
            .parse::<u16>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.Int32" => trimmed
            .parse::<i32>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.UInt32" => trimmed
            .parse::<u32>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.Int64" => trimmed
            .parse::<i64>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.UInt64" => trimmed
            .parse::<u64>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.Single" => trimmed
            .parse::<f32>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.Double" => trimmed
            .parse::<f64>()
            .map(|value| value.to_ne_bytes().to_vec())
            .map_err(|_| invalid()),
        "System.Boolean" => match trimmed {
            "true" => Ok(vec![1]),
            "false" => Ok(vec![0]),
            _ => Err(invalid()),
        },
        _ => Err(format!(
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

/// One manifest entry with everything the engine and the store can tell us
/// resolved up front.
///
/// The apply phase executes these in order and re-decides nothing, which is
/// what keeps the refusals out of the mutated state; what still goes wrong at
/// apply time is handled by the undo journal.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) enum PlannedManifestEntry {
    /// The binding table already agrees with the manifest.
    Settled,
    /// A descriptor component the manifest adds.
    Add {
        stable_id: StableComponentId,
        component: ManagedComponentManifest,
    },
    /// A descriptor component the manifest renamed: the entry reached a
    /// predecessor through one of its aliases, and the predecessor's rows have
    /// to move onto the successor's registration.
    Rename {
        stable_id: StableComponentId,
        component: ManagedComponentManifest,
        predecessor: RenameSource,
    },
    /// A descriptor component whose layout changed and whose rows must migrate.
    Migrate {
        stable_id: StableComponentId,
        component: ManagedComponentManifest,
        component_id: ComponentId,
        plan: FieldPlan,
        previous_binding: ComponentBinding,
        previous_fields: Vec<ComponentFieldDescriptor>,
    },
}

/// Resolve every manifest entry against the store and the engine.
///
/// The one refusal that lives here rather than in validation is the shared
/// entry with no native binding; `register_manifest_entry` repeats it as a
/// defensive check, but planning means it is raised before anything moves.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) fn plan_manifest(
    engine: &Engine,
    store: &BindingStore,
    manifest: Vec<ManagedComponentManifest>,
    renames: &HashMap<StableComponentId, RenameSource>,
) -> Result<Vec<PlannedManifestEntry>, CSharpError> {
    let mut planned = Vec::with_capacity(manifest.len());
    for component in manifest {
        let stable_id =
            StableComponentId::from_halves(component.stable_id_low, component.stable_id_high);
        let Some(binding) = store.read().get(&stable_id).copied() else {
            if component.shared {
                return Err(format!(
                    "managed shared component {} has no native engine binding",
                    component.full_name
                )
                .into());
            }
            // An alias that reached a predecessor turns this entry into a
            // rename; without one it is an ordinary addition.
            if let Some(predecessor) = renames.get(&stable_id) {
                planned.push(PlannedManifestEntry::Rename {
                    stable_id,
                    component,
                    predecessor: predecessor.clone(),
                });
            } else {
                planned.push(PlannedManifestEntry::Add {
                    stable_id,
                    component,
                });
            }
            continue;
        };

        // A successor's stable id is derived from its new name, so a store hit
        // here means the name was already registered while its aliases still
        // name an earlier registration. Refused rather than settled: settling
        // would strand the predecessor's rows in a binding nothing tracks.
        if renames.contains_key(&stable_id) {
            return Err(format!(
                "managed component {} is already registered, but its aliases name an earlier \
                 registration; a rename must declare a new name",
                component.full_name
            )
            .into());
        }

        let ComponentBinding::Managed {
            component_id,
            size,
            align,
            schema_hash,
        } = binding
        else {
            check_binding_against_manifest(binding, &component)?;
            planned.push(PlannedManifestEntry::Settled);
            continue;
        };

        // The same layout means nothing to do; anything else is a migration.
        if size == component.size
            && align == component.alignment
            && schema_hash == component.schema_hash
        {
            planned.push(PlannedManifestEntry::Settled);
            continue;
        }

        let plan = build_field_plan(engine, component_id, &component.fields);
        // The fields are captured as owned data here so the inverse plan needs
        // no engine borrow later, when the world is being mutated again.
        let previous_fields = engine
            .world()
            .component_field_layout(component_id)
            .unwrap_or(&[])
            .to_vec();
        planned.push(PlannedManifestEntry::Migrate {
            stable_id,
            component,
            component_id,
            plan,
            previous_binding: binding,
            previous_fields,
        });
    }
    Ok(planned)
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

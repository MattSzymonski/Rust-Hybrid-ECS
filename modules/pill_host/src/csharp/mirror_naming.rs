//! The rules that name a generated C# mirror's fields, shared by the mirror
//! generator and the binding check.
//!
//! # Responsibilities
//!
//! - Turn Rust field names and type tags into the C# declarations a generated
//!   mirror carries: PascalCase names, one numbered field per array element,
//!   no public field for a heap container.
//! - State a mirror's public field list as a signature both sides can produce:
//!   the generator from the native descriptors, the bridge from the managed
//!   manifest.
//!
//! # Design
//!
//! Compiled in every posture. Only a reloading host generates mirrors, but a
//! shipping host still binds them and still has to refuse a managed struct
//! whose fields disagree with the native component, so the naming rules cannot
//! live in the generator alone.

// External crates
use pill_engine::component_registry::ComponentFieldDescriptor;

/// The public fields a generated mirror declares for `fields`, as the text
/// `Name@offset:size|...` ordered by offset, or `None` when it cannot be
/// stated.
///
/// This is what lets a managed mirror be checked field by field against the
/// native component it binds to, without a schema hash both sides would have
/// to agree on: the names and offsets are exactly the ones
/// [`emit_typed_struct`] writes - PascalCase, one numbered field per array
/// element. A component with no field layout, or with a heap container (which
/// emits private handle fields rather than a public one), has no signature and
/// is checked by size and alignment only.
pub(super) fn generated_field_signature(fields: &[ComponentFieldDescriptor]) -> Option<String> {
    if fields.is_empty()
        || fields
            .iter()
            .any(|field| is_opaque_container_tag(field.type_tag))
    {
        return None;
    }
    let mut declared: Vec<(usize, String, usize)> = Vec::new();
    for field in fields {
        let (_, is_array) = split_array_tag(field.type_tag).ok()?;
        let pascal = snake_to_pascal(field.name);
        if is_array {
            if field.element_count == 0 || field.size % field.element_count != 0 {
                return None;
            }
            let element_size = field.size / field.element_count;
            for index in 0..field.element_count {
                declared.push((
                    field.offset + index * element_size,
                    format!("{pascal}{index}"),
                    element_size,
                ));
            }
        } else {
            declared.push((field.offset, pascal, field.size));
        }
    }
    Some(field_signature_text(declared))
}

/// Render `(offset, name, size)` triples as the signature text, by offset.
pub(super) fn field_signature_text(mut declared: Vec<(usize, String, usize)>) -> String {
    declared.sort();
    declared
        .into_iter()
        .map(|(offset, name, size)| format!("{name}@{offset}:{size}"))
        .collect::<Vec<_>>()
        .join("|")
}

/// Whether a tag names a Rust-owned container field, which carries no C# field
/// of its own: its pointer must never be exposed to managed code, so elements
/// are reached through the generated accessor members instead.
pub(super) fn is_opaque_container_tag(tag: &str) -> bool {
    tag.starts_with("vec:") || tag == "string"
}

/// Split an `array:<inner>` tag into its base tag and whether it is an array.
pub(super) fn split_array_tag(tag: &str) -> Result<(&str, bool), String> {
    match tag.strip_prefix("array:") {
        Some(inner) => Ok((inner, true)),
        None => Ok((tag, false)),
    }
}

/// Convert a snake_case Rust identifier to the PascalCase used in C# mirrors.
pub(super) fn snake_to_pascal(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut capitalize_next = true;
    for character in name.chars() {
        if character == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.extend(character.to_uppercase());
            capitalize_next = false;
        } else {
            result.push(character);
        }
    }
    result
}

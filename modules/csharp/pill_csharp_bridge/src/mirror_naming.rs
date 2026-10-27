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
/// to agree on: the names and offsets are exactly the ones the generator in
/// `pill_host` (`emit_typed_struct`) writes - PascalCase, one numbered field
/// per array element. A component with no field layout, or with a heap-owned
/// field (which emits private handle words rather than a public field), has no
/// signature and is checked by size and alignment only.
pub(super) fn generated_field_signature(fields: &[ComponentFieldDescriptor]) -> Option<String> {
    if fields.is_empty() || fields.iter().any(|field| is_heap_owned_tag(field.type_tag)) {
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
///
/// This is narrower than [`is_heap_owned_tag`] on purpose. An engine-owned
/// `dynbuf:` buffer carries three private header words in the mirror, so the
/// generator must not skip it the way it skips a `vec:` - but managed code
/// still reaches its contents through accessors.
pub fn is_opaque_container_tag(tag: &str) -> bool {
    tag.starts_with("vec:") || tag == "string"
}

/// Whether a tag names a field whose bytes are a header for storage owned
/// elsewhere: a `vec:`/`string` container the row owns, or an engine-owned
/// `dynbuf:` native buffer.
///
/// This is the engine's own container rule (`is_container_tag` in
/// `pill_engine_core::component_field`), restated because that copy is
/// crate-private. Both sides must agree: the check below reads the rule to
/// decide that a mirror's public field list cannot be stated at all, while the
/// generator reads it to decide which fields it emits differently from a plain
/// value.
pub fn is_heap_owned_tag(tag: &str) -> bool {
    tag == "string" || tag.starts_with("vec:") || tag.starts_with("dynbuf:")
}

/// Split an `array:<inner>` tag into its base tag and whether it is an array.
pub fn split_array_tag(tag: &str) -> Result<(&str, bool), String> {
    match tag.strip_prefix("array:") {
        Some(inner) => Ok((inner, true)),
        None => Ok((tag, false)),
    }
}

/// Convert a snake_case Rust identifier to the PascalCase used in C# mirrors.
pub fn snake_to_pascal(name: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(
        name: &'static str,
        type_tag: &'static str,
        offset: usize,
    ) -> ComponentFieldDescriptor {
        ComponentFieldDescriptor {
            name,
            type_tag,
            offset,
            size: 24,
            align: 8,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        }
    }

    /// A component with an engine-owned buffer field is checked by size and
    /// alignment: the generator hands managed code three private header words
    /// there, so no list of public field names describes the struct. A stated
    /// signature would refuse every such mirror at bind time.
    #[test]
    fn a_native_buffer_field_leaves_no_field_signature() {
        let fields = [
            descriptor("intensity", "f32", 0),
            descriptor("points", "dynbuf:f32", 8),
        ];
        assert_eq!(generated_field_signature(&fields), None);
    }

    #[test]
    fn a_plain_component_still_has_a_field_signature() {
        let fields = [
            descriptor("intensity", "f32", 0),
            descriptor("name_id", "u32", 4),
        ];
        assert_eq!(
            generated_field_signature(&fields).as_deref(),
            Some("Intensity@0:24|NameId@4:24")
        );
    }
}

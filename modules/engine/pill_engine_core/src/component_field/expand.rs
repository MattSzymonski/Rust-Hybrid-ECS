//! Registration-time expansion of nested component field layouts.
//!
//! # Responsibilities
//!
//! - Flatten a `struct:` field whose type has a known layout into one dotted
//!   leaf row per leaf, composing offsets as it descends.
//! - Keep the declared layout beside the flattened one, and own the resolver a
//!   later registration consults for a nested type.
//!
//! # Design
//!
//! Expanded field names are interned for the process: a layout holds
//! `&'static str` names and is registered once per type, so the interner hands
//! out one copy of each name and never frees it. A name that is never expanded
//! borrows the caller's own `'static` slice instead, so only real expansion
//! allocates.

// Current crate
use crate::component::ComponentId;
use crate::component_registry::{ComponentFieldDescriptor, FieldDecorator};
use crate::world::World;

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

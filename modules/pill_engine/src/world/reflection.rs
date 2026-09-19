//! Reflection and diagnostics: what the world can say about itself.
//!
//! # Responsibilities
//!
//! - Answers the field-layout questions an editor asks: which fields a
//!   component or resource declares, and where each one sits in a row.
//! - Enumerates the world for a tool rather than for a system: the entities
//!   that exist, the component names attached to each, and which types are
//!   registered at all.
//! - Renders the archetype graph and estimates the world's memory, for the
//!   diagnostics report and for `--dot`.
//!
//! # Design
//!
//! Everything here is read-mostly and addressed by name rather than by type:
//! the callers are the editor and the managed side, neither of which has the
//! Rust type in hand. Names are what survive a reload, so a name is the only
//! identity a tool can hold across one.

use super::*;

// =============================================================================
// World - Reflection and Diagnostics
// =============================================================================

impl World {
    /// Return the field layout a component was registered with.
    ///
    /// `None` for components registered without field metadata, which is how
    /// the C# codegen decides between a typed mirror and the ABI blob, and how
    /// the editor decides a component is not field-editable.
    pub fn component_field_layout(
        &self,
        component_id: ComponentId,
    ) -> Option<&[crate::component_registry::ComponentFieldDescriptor]> {
        self.component_field_layouts
            .get(&component_id)
            .map(ComponentFieldLayout::fields)
    }

    /// The declared field layout of a foreign resource, if one was recorded.
    ///
    /// The resource twin of [`Self::component_field_layout`]. `None` means the
    /// resource was registered without a layout, which is the ordinary state
    /// for a Rust resource: its fields are a Rust type's, not a manifest's.
    #[must_use]
    pub fn resource_field_layout(
        &self,
        resource_id: ResourceId,
    ) -> Option<&[crate::component_registry::ComponentFieldDescriptor]> {
        self.resource_field_layouts
            .get(&resource_id)
            .map(ComponentFieldLayout::fields)
    }

    /// Record the declared field layout of a foreign resource.
    ///
    /// Overwrites any previous layout for the same id, so a manifest reload
    /// replaces rather than accumulates - the same rule
    /// [`Self::register_component_descriptor_with_layout`] follows.
    ///
    /// **A relayout clears the stored layout**, so this has to be called again
    /// after one. That is deliberate: serving the previous generation's offsets
    /// over the migrated bytes would be worse than serving nothing, and a
    /// caller that forgets loses inspectability rather than correctness.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentFieldError::UnsupportedField`](crate::component_field::ComponentFieldError::UnsupportedField)
    /// for a descriptor that reaches past the resource's registered size or
    /// names a container tag. A foreign resource's bytes are as opaque as a
    /// descriptor row, so it needs both checks for the same reason: the bound
    /// keeps a reader inside the value, and the tag check keeps it from
    /// following a `(pointer, length)` pair the bytes never held.
    pub fn register_resource_field_layout(
        &mut self,
        resource_id: ResourceId,
        fields: Vec<crate::component_registry::ComponentFieldDescriptor>,
    ) -> Result<(), crate::component_field::ComponentFieldError> {
        let resource_size = self
            .resource_factories
            .get(&resource_id)
            .map(|ops| ops.size);
        for field in &fields {
            let Some(end) = field.offset.checked_add(field.size) else {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field's range overflows",
                    },
                );
            };
            if resource_size.is_some_and(|size| end > size) {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field extends past the resource's registered size",
                    },
                );
            }
            if crate::component_field::is_container_tag(field.type_tag) {
                return Err(crate::component_field::ComponentFieldError::UnsupportedField {
                    field: field.name.to_string(),
                    reason: "a container tag is read as a native pointer and length, which a foreign resource's bytes do not hold",
                });
            }
        }
        self.resource_field_layouts
            .insert(resource_id, ComponentFieldLayout::from_owned(fields));
        Ok(())
    }

    /// Every live entity with its component type names, sorted by entity id.
    ///
    /// Read-only and tick-neutral: nothing is mutated and no change tick is
    /// touched, so the editor can call this every refresh without disturbing
    /// the simulation. Component names are resolved per archetype once and
    /// shared across the entities of that archetype.
    pub fn entity_rows(&self) -> Vec<EntityRow> {
        // Step 1: Resolve each archetype's component names once; the string
        // work is per archetype rather than per entity.
        let mut archetype_names: HashMap<ArchetypeId, Vec<String>> =
            HashMap::with_capacity(self.archetypes.len());
        for (archetype_id, archetype) in &self.archetypes {
            let names: Vec<String> = archetype
                .component_types
                .iter()
                .filter_map(|component_id| {
                    self.component_registry
                        .get_name(component_id)
                        .map(str::to_string)
                })
                .collect();
            archetype_names.insert(*archetype_id, names);
        }

        // Step 2: Assemble one row per live entity, cloning the shared names.
        let mut rows: Vec<EntityRow> = self
            .entity_locations
            .iter()
            .map(|(entity, location)| EntityRow {
                entity: *entity,
                components: archetype_names
                    .get(&location.archetype_id)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();

        // Step 3: Deterministic ordering for the editor list.
        rows.sort_by_key(|row| row.entity.id());
        rows
    }

    /// Component type names attached to one entity, or `None` when it is dead.
    ///
    /// Names come from the entity's own archetype, never from a registry scan,
    /// so the result is authoritative for the generation that created the data.
    pub fn entity_component_names(&self, entity: Entity) -> Option<Vec<String>> {
        let location = self.entity_locations.get(&entity)?;
        let archetype = self.archetypes.get(&location.archetype_id)?;
        Some(
            archetype
                .component_types
                .iter()
                .filter_map(|component_id| {
                    self.component_registry
                        .get_name(component_id)
                        .map(str::to_string)
                })
                .collect(),
        )
    }

    /// Every registered component type: name plus current [`ComponentId`].
    ///
    /// Names are the stable cross-reload key; ids are per-generation and must
    /// not be cached across a reload. Used by the editor's add-component
    /// picker, not by the per-frame snapshot.
    pub fn registered_components(&self) -> Vec<(String, ComponentId)> {
        let mut components: Vec<(String, ComponentId)> = self
            .component_registry
            .registered_components()
            .map(|(component_id, _, name)| (name.to_string(), component_id))
            .collect();
        components.sort();
        components
    }

    /// Whether a component type is registered as persistable (schema-migrated
    /// across reloads). Used by the editor to mark such components.
    pub fn component_is_persistable(&self, component_id: ComponentId) -> bool {
        self.persist_inserters.contains_key(&component_id)
    }

    /// The component id an entity's archetype actually stores for a registered
    /// type name.
    ///
    /// Unlike [`Self::resolve_component_id_by_name_any`], which searches every
    /// generation the registry still remembers, this looks only at the columns
    /// the entity really has. That makes it correct across reloads even when a
    /// bit index was recycled, and it guarantees the returned id has a live
    /// column, a live tick vector, and a field layout belonging to the
    /// generation that created the data.
    pub fn resolve_entity_component_id(
        &self,
        entity: Entity,
        type_name: &str,
    ) -> Option<ComponentId> {
        let location = self.entity_locations.get(&entity)?;
        let archetype = self.archetypes.get(&location.archetype_id)?;
        archetype
            .component_types
            .iter()
            .copied()
            .find(|component_id| self.component_registry.get_name(component_id) == Some(type_name))
    }

    /// Return the byte size and alignment of a registered component's layout.
    ///
    /// Works for both native (Rust) and descriptor (foreign-language) components.
    /// Used by the C# backend to validate that a managed mirror struct has the
    /// same ABI layout as the component an optional module registered.
    pub fn component_layout(&self, component_id: ComponentId) -> Option<(usize, usize)> {
        match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Native(info)) => Some((info.size, info.align)),
            Some(StorageFactory::Descriptor(layout)) => Some((layout.size, layout.align)),
            None => None,
        }
    }

    /// Print information about all archetypes in the world
    ///
    /// This displays the component types and entity count for each archetype,
    /// useful for debugging and understanding the current state of the ECS.
    pub fn print_archetypes(&self) {
        println!(
            "\n=== World Archetypes (Total: {}) ===",
            self.archetypes.len()
        );
        for (_, archetype) in self.archetypes.iter() {
            archetype.print_info(&self.component_registry);
        }
        println!("Total entities: {}", self.entity_locations.len());
    }

    /// Estimate the total memory footprint of the world in bytes.
    ///
    /// Sums all archetype storage, entity location map, resources,
    /// and internal data structures.
    pub fn memory_estimate(&self) -> usize {
        let mut total = 0usize;

        // Archetype storage (component columns + entities + ticks)
        for archetype in self.archetypes.values() {
            total += archetype.memory_estimate(&self.component_registry);
        }

        // Entity location map: ~32 bytes per entry (Entity key + EntityLocation value + HashMap overhead)
        total += self.entity_locations.len() * 48;

        // Resources: approximate based on type count
        total += self.resources.len() * 128;

        // Free entity IDs
        total += self.free_entity_ids.capacity() * 12;

        // Storage factories and script data
        total += self.storage_factories.len() * 128;
        total += self.script_components.len() * 24;
        total += self.script_updaters.len() * 32;

        total
    }

    /// Generate a Graphviz DOT representation of the world for debugging.
    ///
    /// Useful for debugging archetype fragmentation and visualizing the
    /// relationship between component sets and entity counts. The output is
    /// a `digraph` with one node per archetype, labelled with its component
    /// names and entity count.
    ///
    /// # Example output
    ///
    /// ```dot
    /// digraph World {
    ///     rankdir=LR;
    ///     node [shape=record];
    ///     "arch_0" [label="Position, Velocity | 3 entities"];
    ///     "arch_1" [label="Position, Health | 1 entity"];
    /// }
    /// ```
    #[cold]
    pub fn to_dot_graph(&self) -> String {
        let mut dot = String::from("digraph World {\n    rankdir=LR;\n    node [shape=record];\n");

        for (_, archetype) in self.archetypes.iter() {
            let component_names: Vec<String> = archetype
                .component_types
                .iter()
                .map(|id| {
                    self.component_registry
                        .get_name(id)
                        .unwrap_or("?")
                        .to_string()
                })
                .collect();
            let label = format!(
                "{} | {} entit{}",
                component_names.join(", "),
                archetype.entities.len(),
                if archetype.entities.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            dot.push_str(&format!(
                "    \"arch_{:?}\" [label=\"{}\"];\n",
                archetype.id.0, label
            ));
        }

        dot.push_str("}\n");
        dot
    }
}

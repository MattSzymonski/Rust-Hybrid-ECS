//! Component snapshot and restore: the two directions of the data lane.
//!
//! # Responsibilities
//!
//! - Captures every persistable component row into a [`ComponentSnapshot`],
//!   walking archetypes and serializing through the registered per-type glue.
//! - Rebuilds the world from a snapshot after a reload: destroys the old
//!   entities, re-materializes rows by type name, and seeds change ticks.
//! - Resolves a type name to its single live component id, treating an
//!   unresolved or ambiguous name as the caller's safe fallback.
//!
//! # Design
//!
//! Restore destroys first and recreates from the snapshot, so stale `TypeId`s
//! cannot alias new registrations. Both lanes meet here: native rows come back
//! through their deserializer and inserter, descriptor rows are row images
//! already and are pushed straight into the column.

use super::*;

// =============================================================================
// World — Snapshot and Restore
// =============================================================================

impl World {
    /// Capture all persistable component data from every living entity.
    ///
    /// Iterates every archetype and every entity index, calling the
    /// registered `serialize` function for each persistable component type.
    /// Non-persistable components (those registered with plain
    /// `register_component`) are silently skipped.
    ///
    /// # Panics
    ///
    /// Panics if a persistable component's JSON serialization fails (should
    /// never happen for valid component types).
    pub fn snapshot_components(&self) -> ComponentSnapshot {
        let total_entities = self.entity_locations.len();
        let total_archetypes = self.archetypes.len();
        let persistable_type_count = self.persist_serializers.len();

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Snapshotting {} entities across {} archetypes ({} persistable component types)...",
            total_entities, total_archetypes, persistable_type_count,
        );

        let mut entries: Vec<Vec<(String, Vec<u8>)>> = Vec::with_capacity(total_entities);
        let mut total_components_serialized: usize = 0;
        let mut skipped_non_persistable: usize = 0;

        // Serialize every persistable component across all archetypes
        // and entities; non-persistable components are counted and skipped.
        for archetype in self.archetypes.values() {
            let entity_count = archetype.entities.len();
            if entity_count == 0 {
                continue;
            }

            for entity_index in 0..entity_count {
                let mut component_data: Vec<(String, Vec<u8>)> = Vec::new();

                for component_id in &archetype.component_types {
                    // A descriptor-only component has no serializer - there is
                    // no Rust type to have generated one - so it is serialized
                    // from its field layout instead. Every such component is
                    // persistable: the validated vocabulary is blittable, so
                    // there is nothing a row could hold that cannot be written
                    // down, and no attribute exists for opting out.
                    if !component_id.is_native_storage() {
                        if let Some(bytes) =
                            self.snapshot_descriptor_row(archetype, *component_id, entity_index)
                        {
                            let type_name = self
                                .component_registry
                                .get_name(component_id)
                                .unwrap_or("?")
                                .to_string();
                            component_data.push((type_name, bytes));
                            total_components_serialized += 1;
                        } else {
                            skipped_non_persistable += 1;
                        }
                        continue;
                    }
                    if let Some(&serialize_fn) = self.persist_serializers.get(component_id) {
                        let type_name = self
                            .component_registry
                            .get_name(component_id)
                            .unwrap_or("?")
                            .to_string();
                        let bytes = serialize_fn(&archetype.component_storages, entity_index);
                        let byte_len = bytes.len();
                        let name_for_log = type_name.clone();
                        component_data.push((type_name, bytes));
                        total_components_serialized += 1;

                        if total_components_serialized <= 5 {
                            debug!(
                                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                                "[persistence]   snapshot '{}' → {} bytes",
                                name_for_log, byte_len,
                            );
                        }
                    } else {
                        skipped_non_persistable += 1;
                    }
                }

                if !component_data.is_empty() {
                    entries.push(component_data);
                }
            }
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Snapshot complete: {} entities, {} components serialized ({} non-persistable skipped)",
            entries.len(),
            total_components_serialized,
            skipped_non_persistable,
        );

        ComponentSnapshot { entries }
    }

    /// Rebuild a descriptor-only component's row image from snapshot bytes.
    ///
    /// Returns `None` when the component has no field layout or the snapshot
    /// payload is not an object, which the caller counts as a failed restore
    /// for that component rather than for the whole entity.
    pub(super) fn descriptor_restore_image(
        &self,
        component_id: ComponentId,
        json: &[u8],
    ) -> Option<Vec<u8>> {
        let (size, _) = self.component_layout(component_id)?;
        let fields = self.component_field_layout(component_id)?;
        crate::component_field::descriptor_row_image(json, fields, size)
    }

    /// Serialize one descriptor-only component row for the snapshot.
    ///
    /// Returns `None` when the component has no registered field layout, which
    /// is the one shape that cannot be written down: an opaque blob whose bytes
    /// nothing describes would restore into a row nothing could place.
    pub(super) fn snapshot_descriptor_row(
        &self,
        archetype: &crate::archetype::Archetype,
        component_id: ComponentId,
        row: usize,
    ) -> Option<Vec<u8>> {
        let fields = self.component_field_layout(component_id)?;
        if fields.is_empty() {
            return None;
        }
        let row_bytes = archetype.component_storages.get(component_id)?.bytes(row)?;
        Some(crate::component_field::serialize_descriptor_row(
            row_bytes, fields,
        ))
    }

    /// Destroy all entities and recreate them from a snapshot.
    ///
    /// This is called after a hot-reload: the old component types have been
    /// replaced by new ones (with potentially different `TypeId`s), but the
    /// snapshot still holds data keyed by type **name**.  Matching is done
    /// by name - components whose type name is not found in the new
    /// registration are silently dropped (the component type was removed).
    ///
    /// Deserialization uses JSON, so field additions (serde fills
    /// `Default::default()`) and field removals (serde ignores unknown
    /// keys) are handled gracefully.  Incompatible changes (e.g. changing
    /// a field's type from `f32` to `String`) cause that component to be
    /// skipped with a warning.
    pub fn restore_from_snapshot(&mut self, snapshot: &ComponentSnapshot) {
        let snapshot_entity_count = snapshot.entries.len();
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Restoring {} entities from snapshot...",
            snapshot_entity_count,
        );

        // Step 1: Destroy all existing entities so stale TypeIds cannot
        // alias new registrations.
        let all_entity_ids: Vec<Entity> = self.entity_locations.keys().copied().collect();
        let destroyed_count = all_entity_ids.len();
        for entity in all_entity_ids {
            let _ = self.destroy_entity(entity);
        }
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence]   Destroyed {} old entities (stale TypeIds)",
            destroyed_count,
        );

        // Step 2: Recreate entities from snapshot data.
        let mut restored_entity_count: usize = 0;
        let mut restored_component_total: usize = 0;
        let mut skipped_type_removed: usize = 0;
        let mut skipped_deser_fail: usize = 0;
        let mut skipped_no_inserter: usize = 0;

        for (entry_idx, component_set) in snapshot.entries.iter().enumerate() {
            let mut restored_components: Vec<(ComponentId, RestoredComponent)> = Vec::new();
            let mut restored_component_ids: Vec<ComponentId> = Vec::new();

            for (type_name, bytes) in component_set {
                // A descriptor-only component is rebuilt from its own field
                // layout, by name, with anything the snapshot does not carry
                // left at its zero bytes.
                if let Some(component_id) = self
                    .resolve_component_id_by_name_any(type_name)
                    .unwrap_or(None)
                    .filter(|id| !id.is_native_storage())
                {
                    match self.descriptor_restore_image(component_id, bytes) {
                        Some(image) => {
                            restored_components
                                .push((component_id, RestoredComponent::Descriptor(image)));
                            restored_component_ids.push(component_id);
                        }
                        None => skipped_deser_fail += 1,
                    }
                    continue;
                }
                if let Some(&deserialize_fn) = self.persist_deserializers.get(type_name) {
                    if let Some(component) = deserialize_fn(bytes) {
                        if let Some(component_id) =
                            self.resolve_component_id_by_name_logged(type_name)
                        {
                            if entry_idx < 3 {
                                debug!(
                                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                                    "[persistence]   restore '{}' → ok ({} bytes)",
                                    type_name,
                                    bytes.len(),
                                );
                            }
                            restored_components
                                .push((component_id, RestoredComponent::Native(component)));
                            restored_component_ids.push(component_id);
                        } else {
                            skipped_no_inserter += 1;
                            if entry_idx < 3 {
                                debug!(
                                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                                    "[persistence]   restore '{}' → SKIP (no inserter)",
                                    type_name,
                                );
                            }
                        }
                    } else {
                        skipped_deser_fail += 1;
                        debug!(
                            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                            "[persistence]   restore '{}' → SKIP (deserialize failed)",
                            type_name,
                        );
                    }
                } else {
                    skipped_type_removed += 1;
                    if entry_idx < 3 {
                        debug!(
                            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                            "[persistence]   restore '{}' → SKIP (type removed)",
                            type_name,
                        );
                    }
                }
            }

            if restored_components.is_empty() {
                continue;
            }

            // Place the entity in the archetype that owns its exact set of
            // restored component types, inserting each restored component and
            // seeding change ticks.
            let entity = self.allocate_entity();
            restored_component_ids.sort();

            let archetype_id = self.get_or_create_archetype(restored_component_ids.clone());
            let current_tick = crate::component::Tick::new(self.change_tick);

            {
                let archetype = self
                    .archetypes
                    .get_mut(&archetype_id)
                    .expect("archetype must exist after get_or_create_archetype");
                let index = archetype.entities.len();
                archetype.entities.push(entity);

                for (component_id, component) in restored_components {
                    match component {
                        RestoredComponent::Native(component) => {
                            if let Some(&insert_fn) = self.persist_inserters.get(&component_id) {
                                insert_fn(&mut archetype.component_storages, component);
                                restored_component_total += 1;
                            }
                        }
                        // The descriptor lane needs no inserter: the row is
                        // already the bytes the column stores, so it is pushed
                        // straight in. `get_or_create_archetype` above built
                        // the column from the same registered layout, so the
                        // width always matches.
                        RestoredComponent::Descriptor(image) => {
                            if let Some(column) = archetype.component_storages.get_mut(component_id)
                            {
                                if column.push_bytes(&image).is_ok() {
                                    restored_component_total += 1;
                                }
                            }
                        }
                    }
                }

                for &component_id in &archetype.component_types {
                    archetype
                        .component_ticks
                        .entry(component_id)
                        .or_default()
                        .push(crate::component::ComponentTicks::new(current_tick));
                }

                self.entity_locations.insert(
                    entity,
                    crate::world::EntityLocation {
                        archetype_id,
                        index_in_archetype: index,
                    },
                );
            }

            restored_entity_count += 1;
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Restore complete: {} entities, {} components inserted",
            restored_entity_count, restored_component_total,
        );
        if skipped_type_removed > 0 {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]   {} components skipped (type removed from project)",
                skipped_type_removed,
            );
        }
        if skipped_deser_fail > 0 {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]   {} components skipped (deserialization failed)",
                skipped_deser_fail,
            );
        }
        if skipped_no_inserter > 0 {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]   {} components skipped (no inserter for TypeId)",
                skipped_no_inserter,
            );
        }
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence]   World now has {} entities in {} archetypes",
            self.entity_locations.len(),
            self.archetypes.len(),
        );
    }

    /// Look up the single [`ComponentId`] a component type name resolves to.
    ///
    /// After multiple hot-reloads the component registry can hold one entry
    /// per reload, each with a different `TypeId` but the same type name.
    /// What reduces those to one candidate is **eviction**: registering a new
    /// generation purges every same-name entry from `persist_inserters`
    /// (see [`Self::register_persistable_component`]), so the
    /// `persist_inserters` filter below leaves exactly one.
    ///
    /// The bit index is deliberately *not* used as a tiebreak. It is not a
    /// recency ordering: `ComponentRegistry::allocate_bit` hands out bits
    /// reclaimed by `remove` before advancing `next_bit`, so a later
    /// registration can receive a lower bit than an earlier one. Since
    /// eviction already guarantees uniqueness, more than one surviving
    /// candidate is a bug rather than something to break a tie on, and it is
    /// reported as [`WorldError::ComponentNameAmbiguous`].
    ///
    /// [`persist_registration_sequence`](Self::persist_registration_sequence)
    /// is the true chronological ordering, if a recency tiebreak is ever
    /// genuinely wanted.
    pub(super) fn resolve_component_id_by_name(
        &self,
        type_name: &str,
    ) -> Result<Option<ComponentId>, WorldError> {
        let mut candidates = self
            .component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .filter(|(id, _, _)| self.persist_inserters.contains_key(id))
            .map(|(id, _, _)| id);

        let Some(first) = candidates.next() else {
            return Ok(None);
        };
        // Any second candidate means eviction did not collapse the set, so
        // picking either one would silently bind half the rows to the wrong
        // column.
        let extra = candidates.count();
        if extra > 0 {
            return Err(WorldError::ComponentNameAmbiguous {
                type_name: type_name.to_string(),
                count: extra + 1,
            });
        }
        Ok(Some(first))
    }

    /// [`Self::resolve_component_id_by_name`] for callers that cannot return
    /// an error, degrading an ambiguous name to "unresolved".
    ///
    /// Every such caller already has a safe answer for an unresolved name -
    /// skip the row, omit the manifest entry, drop nothing - so reporting the
    /// ambiguity and taking that path is strictly better than the previous
    /// `max_by_key(bit)` tiebreak, which silently picked one of the candidates
    /// and could bind rows to the wrong column.
    pub(super) fn resolve_component_id_by_name_logged(
        &self,
        type_name: &str,
    ) -> Option<ComponentId> {
        match self.resolve_component_id_by_name(type_name) {
            Ok(component_id) => component_id,
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    type_name = %type_name,
                    error = %error,
                    "component type name is ambiguous; treating it as unresolved"
                );
                None
            }
        }
    }
}

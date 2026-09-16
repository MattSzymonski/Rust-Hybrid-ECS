//! Selective migration: rewriting only the component types a reload changed.
//!
//! # Responsibilities
//!
//! - Drives the selective pass: for every changed type name, reads the old
//!   value through the retiring generation's serializer and writes it back
//!   through the arriving generation's deserializer and inserter.
//! - Chooses the strategy per type: an in-place column swap when the id is
//!   unchanged, a full archetype remap when the reload moved the id.
//! - Reports what it migrated and what it had to skip, so the host can log
//!   the difference the way the full-restore path would have.
//!
//! # Design
//!
//! Everything here must run while the retiring generation's image is still
//! mapped: the serializer came from that artifact and is the only code that
//! can read the old bytes correctly. Entities the incoming generation spawned
//! during `init` are exempted from the old glue and round-tripped through the
//! current serializer instead.

use super::*;

// =============================================================================
// World — Selective Migration
// =============================================================================

impl World {
    /// Migrate only changed persistable components.
    ///
    /// For each changed type name, this uses the old serializer (captured before
    /// reload) and the new deserializer/inserter (registered by new project_init)
    /// to rewrite only the affected component columns.
    ///
    /// `pre_swap_entities` is the set of entities that existed before the
    /// incoming generation's `init` ran (see [`Self::capture_live_entities`]).
    /// Entities outside the set were spawned by the new generation: they
    /// already carry the current schema, so where a column rebuild touches
    /// them they are round-tripped through the current serializer rather than
    /// the retiring generation's. Pass `None` when every entity predates the
    /// migration (for example when restoring a snapshot).
    pub fn migrate_changed_persistable_components(
        &mut self,
        previous_metadata_by_name: &HashMap<String, PersistTypeMetadata>,
        changed_type_names: &HashSet<String>,
        pre_swap_entities: Option<&HashSet<Entity>>,
    ) -> SelectiveMigrationReport {
        let mut report = SelectiveMigrationReport::default();

        // Sort references into the names rather than cloning every string into
        // a heap `Vec<String>`: the ordering exists only for a deterministic
        // report, and the names are unique, so `sort_unstable` is fine. The
        // sort is skipped entirely when nothing will render the report.
        let mut sorted_changed_type_names: Vec<&str> =
            changed_type_names.iter().map(String::as_str).collect();
        if tracing::enabled!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            tracing::Level::INFO
        ) {
            sorted_changed_type_names.sort_unstable();
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Selective migration starting for {} component type(s)...",
            sorted_changed_type_names.len(),
        );

        for type_name in &sorted_changed_type_names {
            let Some(previous_metadata) = previous_metadata_by_name.get(*type_name) else {
                debug!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    "[persistence]   '{}' -> SKIP (missing previous metadata)",
                    type_name,
                );
                report.skipped_type_names.push((*type_name).to_string());
                continue;
            };

            let current_schema_hash = self
                .persist_schema_hashes
                .get(*type_name)
                .copied()
                .unwrap_or(0);
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]   '{}' -> migrating (schema {} -> {})",
                type_name, previous_metadata.schema_hash, current_schema_hash,
            );

            match self.migrate_single_component_type(
                type_name,
                previous_metadata,
                pre_swap_entities,
            ) {
                Ok(migrated_entity_count_for_type) => {
                    info!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        "[persistence]   '{}' -> OK ({} entities)",
                        type_name, migrated_entity_count_for_type,
                    );
                    report.migrated_type_count += 1;
                    report.migrated_entity_count += migrated_entity_count_for_type;
                }
                Err(error) => {
                    debug!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        "[persistence]   '{type_name}' -> SKIP ({error})",);
                    report.skipped_type_names.push((*type_name).to_string());
                }
            }
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "[persistence] Selective migration finished: {} type(s) migrated, {} entities touched, {} type(s) skipped.",
            report.migrated_type_count,
            report.migrated_entity_count,
            report.skipped_type_names.len(),
        );

        report
    }

    /// Migrate one persistable component type from its previous registration
    /// to the current one.
    ///
    /// Chooses between an in-place column swap (when the [`ComponentId`] is
    /// unchanged) and a full archetype remap (when the component id changed
    /// after a reload).
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::ComponentTypeUnregistered`] when the type
    /// name is no longer registered, [`PersistenceError::DeserializerMissing`]
    /// when no deserializer is registered for the type name, and
    /// [`PersistenceError::InserterMissing`] when no inserter is registered
    /// for the resolved component id.  Errors from the underlying in-place or
    /// cross-archetype migration propagate unchanged.
    pub(super) fn migrate_single_component_type(
        &mut self,
        type_name: &str,
        previous_metadata: &PersistTypeMetadata,
        pre_swap_entities: Option<&HashSet<Entity>>,
    ) -> Result<usize, PersistenceError> {
        let new_component_id = match self.resolve_component_id_by_name(type_name) {
            Ok(Some(component_id)) => component_id,
            Ok(None) => {
                return Err(PersistenceError::ComponentTypeUnregistered {
                    type_name: type_name.to_string(),
                })
            }
            Err(WorldError::ComponentNameAmbiguous { count, .. }) => {
                return Err(PersistenceError::ComponentTypeAmbiguous {
                    type_name: type_name.to_string(),
                    count,
                })
            }
            Err(_) => {
                return Err(PersistenceError::ComponentTypeUnregistered {
                    type_name: type_name.to_string(),
                })
            }
        };

        let Some(&deserialize_component) = self.persist_deserializers.get(type_name) else {
            return Err(PersistenceError::DeserializerMissing {
                type_name: type_name.to_string(),
            });
        };

        let Some(&insert_component) = self.persist_inserters.get(&new_component_id) else {
            return Err(PersistenceError::InserterMissing {
                type_name: type_name.to_string(),
            });
        };

        if previous_metadata.component_id == new_component_id {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]     strategy: in-place column swap for '{}'",
                type_name,
            );
            // The current serializer reads spawns' values with the layout the
            // new generation wrote them in; without it the rebuild would have
            // to misread them through the retiring generation's layout.
            let Some(&serialize_current_component) =
                self.persist_serializers.get(&new_component_id)
            else {
                return Err(PersistenceError::SerializerMissing {
                    type_name: type_name.to_string(),
                });
            };
            self.migrate_component_column_in_place(
                previous_metadata.component_id,
                previous_metadata.serializer,
                serialize_current_component,
                deserialize_component,
                insert_component,
                pre_swap_entities,
            )
        } else {
            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "[persistence]     strategy: archetype remap for '{}' (component id changed)",
                type_name,
            );
            self.migrate_component_across_archetypes(
                previous_metadata.component_id,
                new_component_id,
                previous_metadata.serializer,
                deserialize_component,
                insert_component,
            )
        }
    }

    /// Rewrite one component column in place inside every archetype that
    /// contains the old component id.
    ///
    /// Serializes each old value, deserializes it with the new schema
    /// (falling back to `{}` when the snapshot bytes no longer parse),
    /// removes the old storage column, recreates it through the registered
    /// storage factory, and inserts the migrated values.
    ///
    /// Entities the incoming generation spawned during `init` are part of the
    /// same archetype when the schema change kept the component's layout
    /// identical (a widened `Vec<f32>` element type, for example). Those
    /// already hold current-layout values, so they are serialized with
    /// `serialize_current_component` instead of the retiring generation's
    /// serializer, which would reinterpret their bytes under the old shape.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::DeserializationFailed`] when a value cannot
    /// be decoded with either the snapshot bytes or `{}`,
    /// [`PersistenceError::StorageRemovalFailed`] when the old storage column
    /// cannot be removed, [`PersistenceError::StorageFactoryMissing`] when no
    /// storage factory is registered, and
    /// [`PersistenceError::NativeStorageExpected`] when the registered
    /// factory is not a native storage factory.
    pub(super) fn migrate_component_column_in_place(
        &mut self,
        component_id: ComponentId,
        serialize_old_component: SerializeComponentFn,
        serialize_current_component: SerializeComponentFn,
        deserialize_new_component: DeserializeComponentFn,
        insert_new_component: InsertComponentFn,
        pre_swap_entities: Option<&HashSet<Entity>>,
    ) -> Result<usize, PersistenceError> {
        // Step 1: Collect the archetypes whose columns contain the old
        // component id.
        let archetype_ids: Vec<_> = self
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.component_types.contains(&component_id))
            .map(|(archetype_id, _)| *archetype_id)
            .collect();

        let mut migrated_entity_count: usize = 0;

        for archetype_id in archetype_ids {
            // Step 2: Serialize every value before the storage column is
            // removed. Entities spawn-swapped in by the new generation (they
            // are not in the pre-swap set) already hold current-layout values
            // and go through the current serializer.
            let serialized_components: Vec<Vec<u8>> = {
                let Some(archetype) = self.archetypes.get(&archetype_id) else {
                    continue;
                };

                (0..archetype.entities.len())
                    .map(|entity_index| {
                        let spawned_this_generation = pre_swap_entities.is_some_and(|entities| {
                            !entities.contains(&archetype.entities[entity_index])
                        });
                        if spawned_this_generation {
                            serialize_current_component(&archetype.component_storages, entity_index)
                        } else {
                            serialize_old_component(&archetype.component_storages, entity_index)
                        }
                    })
                    .collect()
            };

            // Step 3: Decode each value with the new schema, falling back to
            // an empty `{}` object when the snapshot bytes no longer parse.
            let mut migrated_components: Vec<Box<dyn Component>> =
                Vec::with_capacity(serialized_components.len());

            for bytes in &serialized_components {
                let component = deserialize_new_component(bytes)
                    .or_else(|| deserialize_new_component(b"{}"))
                    .ok_or(PersistenceError::DeserializationFailed { component_id })?;
                migrated_components.push(component);
            }

            // Step 4: Swap the storage column: remove the old one, recreate it
            // through the registered storage factory, and insert the migrated
            // values.
            let Some(archetype) = self.archetypes.get_mut(&archetype_id) else {
                continue;
            };

            if !component_id.is_native_storage() {
                return Err(PersistenceError::NativeStorageExpected { component_id });
            }
            // Drop the old column through the glue that wrote it. The re-home
            // pass already stamped the arriving generation's table onto every
            // column, and old-layout values must not be released by code that
            // never allocated them; `register_component_inner` captured the
            // retiring table when the incoming registration replaced it.
            if let Some(retiring_ops) = self.retired_native_storage_ops.get(&component_id) {
                if let Some(column) = archetype.component_storages.get_mut(component_id) {
                    column.refresh_ops(*retiring_ops);
                }
            }
            if archetype.component_storages.remove(component_id).is_none() {
                return Err(PersistenceError::StorageRemovalFailed { component_id });
            }

            let Some(factory) = self.storage_factories.get(&component_id) else {
                return Err(PersistenceError::StorageFactoryMissing { component_id });
            };
            let crate::archetype::StorageFactory::Native(info) = factory else {
                return Err(PersistenceError::NativeStorageExpected { component_id });
            };
            // Build the replacement column as a concrete erased column (no
            // trait-object vtable) so it stays valid across module unloads.
            archetype.component_storages.insert(
                component_id,
                crate::archetype::ComponentColumn::from_native_info(*info, 0)
                    .expect("a registered native layout must describe an allocation"),
            );

            for component in migrated_components {
                insert_new_component(&mut archetype.component_storages, component);
            }

            migrated_entity_count += serialized_components.len();
        }

        Ok(migrated_entity_count)
    }

    /// Move every entity from archetypes containing the old component id into
    /// archetypes containing the new component id.
    ///
    /// Serializes each old component value, deserializes it with the new
    /// schema (falling back to `{}`), copies the unchanged components through
    /// their registered copiers, and re-inserts the migrated component.  Used
    /// when a reload changes the [`ComponentId`] assigned to a type name.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::DeserializationFailed`] when a migrated
    /// value cannot be decoded with either the snapshot bytes or `{}`,
    /// [`PersistenceError::DestinationArchetypeMissing`] when the destination
    /// archetype cannot be created, and [`PersistenceError::CopierMissing`]
    /// when an unchanged component has no registered copier.
    pub(super) fn migrate_component_across_archetypes(
        &mut self,
        old_component_id: ComponentId,
        new_component_id: ComponentId,
        serialize_old_component: SerializeComponentFn,
        deserialize_new_component: DeserializeComponentFn,
        insert_new_component: InsertComponentFn,
    ) -> Result<usize, PersistenceError> {
        // Step 1: Locate every archetype that contains the old component id.
        let source_archetype_ids: Vec<_> = self
            .archetypes
            .iter()
            .filter(|(_, archetype)| archetype.component_types.contains(&old_component_id))
            .map(|(archetype_id, _)| *archetype_id)
            .collect();

        let mut migrated_entity_count: usize = 0;

        for source_archetype_id in source_archetype_ids {
            // Step 2: Remove the source archetype and compute its destination
            // component set with the old id replaced by the new one.
            let Some(source_archetype) = self.archetypes.remove(&source_archetype_id) else {
                continue;
            };

            let mut destination_component_ids: Vec<ComponentId> = source_archetype
                .component_types
                .iter()
                .map(|component_id| {
                    if *component_id == old_component_id {
                        new_component_id
                    } else {
                        *component_id
                    }
                })
                .collect();

            destination_component_ids.sort();
            destination_component_ids.dedup();

            let destination_archetype_id = self.get_or_create_archetype(destination_component_ids);

            // Step 3: Serialize the old component values and decode them with
            // the new schema, falling back to `{}`.
            let serialized_components: Vec<Vec<u8>> = (0..source_archetype.entities.len())
                .map(|entity_index| {
                    serialize_old_component(&source_archetype.component_storages, entity_index)
                })
                .collect();

            let mut migrated_components: Vec<Box<dyn Component>> =
                Vec::with_capacity(serialized_components.len());

            for bytes in &serialized_components {
                let component = deserialize_new_component(bytes)
                    .or_else(|| deserialize_new_component(b"{}"))
                    .ok_or(PersistenceError::DeserializationFailed {
                        component_id: old_component_id,
                    })?;
                migrated_components.push(component);
            }

            // Step 4: Move each entity into the destination archetype,
            // copying the unchanged components and inserting the migrated
            // one, then re-record its location and per-component ticks.
            let Some(destination_archetype) = self.archetypes.get_mut(&destination_archetype_id)
            else {
                return Err(PersistenceError::DestinationArchetypeMissing);
            };

            let current_tick = crate::component::Tick::new(self.change_tick);

            for (entity_index, component) in migrated_components.into_iter().enumerate() {
                let entity = source_archetype.entities[entity_index];
                let destination_index = destination_archetype.entities.len();
                destination_archetype.entities.push(entity);

                for source_component_id in &source_archetype.component_types {
                    if *source_component_id == old_component_id {
                        continue;
                    }

                    let Some(&copy_component) = self.component_copiers.get(source_component_id)
                    else {
                        return Err(PersistenceError::CopierMissing {
                            component_id: *source_component_id,
                        });
                    };

                    copy_component(
                        &source_archetype.component_storages,
                        &mut destination_archetype.component_storages,
                        entity_index,
                    );
                }

                insert_new_component(&mut destination_archetype.component_storages, component);

                for destination_component_id in &destination_archetype.component_types {
                    let tick = if *destination_component_id == new_component_id {
                        source_archetype
                            .component_ticks
                            .get(&old_component_id)
                            .and_then(|ticks| ticks.get(entity_index))
                            .copied()
                            .unwrap_or(crate::component::ComponentTicks::new(current_tick))
                    } else {
                        source_archetype
                            .component_ticks
                            .get(destination_component_id)
                            .and_then(|ticks| ticks.get(entity_index))
                            .copied()
                            .unwrap_or(crate::component::ComponentTicks::new(current_tick))
                    };

                    destination_archetype
                        .component_ticks
                        .entry(*destination_component_id)
                        .or_default()
                        .push(tick);
                }

                self.entity_locations.insert(
                    entity,
                    crate::world::EntityLocation {
                        archetype_id: destination_archetype_id,
                        index_in_archetype: destination_index,
                    },
                );
            }

            migrated_entity_count += source_archetype.entities.len();
        }

        // Step 5: Bump the archetype generation so cached query plans observe
        // the new archetype layout.
        self.archetype_generation = self.archetype_generation.wrapping_add(1);
        Ok(migrated_entity_count)
    }
}

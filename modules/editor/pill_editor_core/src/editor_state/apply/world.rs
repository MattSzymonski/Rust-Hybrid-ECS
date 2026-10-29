//! The world family's applier.
//!
//! The world edits: component fields, array elements, entity and
//! component lifecycle, and resource fields.
//!
//! # Responsibilities
//!
//! - Apply the world edits and answer each with the inverse the history
//!   records.
//! - Refuse a write the component registry cannot take, naming the field.
//!
//! # Design
//!
//! One `match` over the family's own variants, called from
//! [`EditorCommand::apply_one`](super::EditorCommand). Every arm ends in a
//! [`CommandOutcome`]: the inverse the history records, or `plain()` for a
//! command that cannot be undone.

use pill_engine::Engine;

use super::CommandOutcome;
use super::*;

/// Apply one world command.
pub(super) fn apply(
    engine: &mut Engine,
    command: &EditorCommand,
) -> Result<CommandOutcome, String> {
    match command {
        EditorCommand::CreateEntity { components } => {
            // Step 1: Validate and compose every component image before
            // touching the world, so a bad seed changes nothing.
            let mut native: Vec<Box<dyn pill_engine::commands::ComponentAdder>> = Vec::new();
            let mut descriptor_components = Vec::new();
            for seed in components {
                // Build the image first; this validates the layout rule.
                let bytes = engine
                    .world()
                    .build_component_image(&seed.type_name, &seed.fields)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                // An ambiguous name reports itself rather than resolving
                // to a guessed column, so the editor never writes bytes
                // into the wrong component's storage.
                let component_id = engine
                    .world()
                    .resolve_component_id_by_name_any(&seed.type_name)
                    .map_err(|error| format!("{command:?}: {error}"))?
                    .ok_or_else(|| {
                        format!("{command:?}: `{}` is not registered", seed.type_name)
                    })?;
                if component_id.is_native_storage() {
                    native.push(Box::new(pill_engine::commands::ByteComponentAdder::new(
                        component_id,
                        bytes,
                    )));
                } else {
                    descriptor_components.push((component_id, bytes));
                }
            }

            // Step 2: reserve the handle, then queue the creation. `Entity` is
            // a `Copy` value and `queue_deferred_commands` runs its closure
            // immediately, so the handle crosses by value and the inverse names
            // it directly - no shared cell, no lock, and no failure branch for
            // a write that cannot fail.
            let assigned = engine.world_mut().reserve_entity();
            engine.queue_deferred_commands(move |_world, queue| {
                queue.create_mixed_entity(assigned, native, descriptor_components);
            });
            engine
                .flush_deferred_commands()
                .map_err(|errors| format!("{command:?}: {errors:?}"))?;
            Ok(CommandOutcome::inverse(EditorCommand::DestroyEntity {
                entity: assigned,
            }))
        }

        EditorCommand::DestroyEntity { entity } => {
            // Capture the images before the destroy: they are the inverse.
            let seeds = super::EditorCommand::capture_seeds(engine, *entity)?;
            if engine.world_mut().destroy_entity(*entity) {
                Ok(CommandOutcome::inverse(EditorCommand::RestoreEntity {
                    previous: *entity,
                    components: seeds,
                }))
            } else {
                Err(format!("{command:?}: entity was already gone"))
            }
        }

        EditorCommand::AddComponent { entity, component } => {
            // Adding is only offered for components with a registered
            // field layout: the zero-initialisation rule says an image
            // without one cannot be safely zero-filled. Adding targets an
            // entity that does not carry the component yet; attaching it
            // twice is an error, not a no-op.
            if engine
                .world()
                .resolve_entity_component_id(*entity, component)
                .is_some()
            {
                return Err(format!(
                    "{command:?}: `{component}` is already on the entity"
                ));
            }
            let bytes = engine
                .world()
                .build_component_image(component, &[])
                .map_err(|error| format!("{command:?}: {error}"))?;
            let component_id = engine
                .world()
                .resolve_component_id_by_name_any(component)
                .map_err(|error| format!("{command:?}: {error}"))?
                .ok_or_else(|| format!("{command:?}: `{component}` is not registered"))?;
            engine.queue_deferred_commands(move |_world, queue| {
                if component_id.is_native_storage() {
                    queue.add_component_adder_to_entity(
                        *entity,
                        Box::new(pill_engine::commands::ByteComponentAdder::new(
                            component_id,
                            bytes,
                        )),
                    );
                } else {
                    queue.add_descriptor_component_to_entity(*entity, component_id, bytes);
                }
            });
            engine
                .flush_deferred_commands()
                .map_err(|errors| format!("{command:?}: {errors:?}"))?;
            Ok(CommandOutcome::inverse(EditorCommand::RemoveComponent {
                entity: *entity,
                component: component.clone(),
            }))
        }

        EditorCommand::RemoveComponent { entity, component } => {
            let component_id = engine
                .world()
                .resolve_entity_component_id(*entity, component)
                .ok_or_else(|| format!("{command:?}: `{component}` is not on the entity"))?;
            // Capture the values first: the inverse re-attaches the
            // component with them. A layout-less component cannot be
            // rebuilt from images, so its removal is not invertible.
            let names: Vec<String> = engine
                .world()
                .component_field_layout(component_id)
                .map(|layout| layout.iter().map(|d| d.name.to_string()).collect())
                .unwrap_or_default();
            let removed = if names.is_empty() {
                None
            } else {
                let values = engine
                    .world()
                    .read_component_fields(*entity, component)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                Some(
                    names
                        .into_iter()
                        .zip(values)
                        .collect::<Vec<(String, FieldValue)>>(),
                )
            };
            engine.queue_deferred_commands(move |_world, queue| {
                queue.remove_component_by_id(*entity, component_id);
            });
            engine
                .flush_deferred_commands()
                .map_err(|errors| format!("{command:?}: {errors:?}"))?;
            Ok(match removed {
                Some(fields) => CommandOutcome::inverse(EditorCommand::RestoreComponent {
                    entity: *entity,
                    component: component.clone(),
                    fields,
                }),
                None => CommandOutcome::plain(),
            })
        }

        EditorCommand::RestoreEntity {
            previous,
            components,
        } => {
            // Restores go through the creation path, which is what makes
            // the assigned handle observable; the inverse this produced
            // (a destroy of the fresh handle) is deliberately dropped: a
            // restore is already part of the history, never an entry.
            let create = EditorCommand::CreateEntity {
                components: components.clone(),
            };
            let created = super::EditorCommand::apply_one(engine, &create)?;
            let Some(EditorCommand::DestroyEntity { entity: assigned }) = created.inverse else {
                return Err(format!("{command:?}: the restore reserved no handle"));
            };
            Ok(CommandOutcome {
                inverse: None,
                remap: Some((*previous, assigned)),
            })
        }

        EditorCommand::RestoreComponent {
            entity,
            component,
            fields,
        } => {
            // Zero-initialised attach first, then the captured values, so
            // the restore reuses exactly the validation of the commands
            // the UI issues.
            let add = EditorCommand::AddComponent {
                entity: *entity,
                component: component.clone(),
            };
            super::EditorCommand::apply_one(engine, &add)?;
            for (field, value) in fields {
                let set = EditorCommand::SetField {
                    entity: *entity,
                    component: component.clone(),
                    field: field.clone(),
                    value: value.clone(),
                };
                super::EditorCommand::apply_one(engine, &set)?;
            }
            Ok(CommandOutcome::plain())
        }
        EditorCommand::SetField {
            entity,
            component,
            field,
            value,
        } => {
            // Read the old value first: it is what the inverse restores,
            // and a field that cannot be read cannot be written either.
            let previous = engine
                .world()
                .read_component_field(*entity, component, field)
                .map_err(|error| format!("{command:?}: {error}"))?;
            engine
                .world_mut()
                .write_component_field(*entity, component, field, value.clone())
                .map_err(|error| format!("{command:?}: {error}"))?;
            Ok(CommandOutcome::inverse(EditorCommand::SetField {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
                value: previous,
            }))
        }

        EditorCommand::SetFieldAsset {
            entity,
            component,
            field,
            reference,
        } => {
            let previous = engine
                .world()
                .read_component_field(*entity, component, field)
                .map_err(|error| format!("{command:?}: {error}"))?;
            let FieldValue::Asset { asset_type, .. } = &previous else {
                return Err(format!("{command:?}: `{field}` is not an asset reference"));
            };
            let value = match reference {
                None => FieldValue::Asset {
                    asset_type: asset_type.clone(),
                    index: u32::MAX,
                    generation: u32::MAX,
                },
                Some(reference) => resolve_asset_reference(engine, reference)
                    .map_err(|error| format!("{command:?}: {error}"))?,
            };
            engine
                .world_mut()
                .write_component_field(*entity, component, field, value)
                .map_err(|error| format!("{command:?}: {error}"))?;
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                component = component.as_str(),
                field = field.as_str(),
                asset = reference.as_deref().unwrap_or("none"),
                "[editor] asset reference set"
            );
            Ok(CommandOutcome::inverse(EditorCommand::SetField {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
                value: previous,
            }))
        }

        EditorCommand::SetFieldEntity {
            entity,
            component,
            field,
            reference,
        } => {
            let previous = engine
                .world()
                .read_component_field(*entity, component, field)
                .map_err(|error| format!("{command:?}: {error}"))?;
            if !matches!(previous, FieldValue::Entity { .. }) {
                return Err(format!("{command:?}: `{field}` is not an entity reference"));
            }
            let value = match reference {
                None => FieldValue::Entity {
                    id: Entity::NULL.id(),
                    generation: Entity::NULL.generation(),
                },
                Some(target) => {
                    // A reference to an entity that has gone since the row
                    // was drawn is refused here rather than written as a
                    // dangling handle: the row would show it missing, and
                    // the undo step would restore a reference to nothing.
                    if !engine.world().is_entity_valid(*target) {
                        return Err(format!(
                            "{command:?}: no live entity carries the reference {}",
                            EntityListEntry::display_of(*target)
                        ));
                    }
                    FieldValue::Entity {
                        id: target.id(),
                        generation: target.generation(),
                    }
                }
            };
            engine
                .world_mut()
                .write_component_field(*entity, component, field, value)
                .map_err(|error| format!("{command:?}: {error}"))?;
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                component = component.as_str(),
                field = field.as_str(),
                reference = reference
                    .map(EntityListEntry::display_of)
                    .unwrap_or_else(|| "none".to_owned()),
                "[editor] entity reference set"
            );
            Ok(CommandOutcome::inverse(EditorCommand::SetField {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
                value: previous,
            }))
        }

        EditorCommand::SetArrayElement {
            entity,
            component,
            field,
            index,
            value,
        } => {
            // The whole array reads back as one value; the old element is
            // the inverse's new value.
            let current = engine
                .world()
                .read_component_field(*entity, component, field)
                .map_err(|error| format!("{command:?}: {error}"))?;
            let FieldValue::Array(elements) = current else {
                return Err(format!("{command:?}: `{field}` is not an array field"));
            };
            let previous = elements
                .get(*index)
                .cloned()
                .ok_or_else(|| format!("{command:?}: element {index} is outside the array"))?;
            engine
                .world_mut()
                .write_component_array_element(*entity, component, field, *index, value.clone())
                .map_err(|error| format!("{command:?}: {error}"))?;
            Ok(CommandOutcome::inverse(EditorCommand::SetArrayElement {
                entity: *entity,
                component: component.clone(),
                field: field.clone(),
                index: *index,
                value: previous,
            }))
        }

        EditorCommand::SetResourceField {
            resource,
            field,
            value,
        } => {
            let world = engine.world_mut();
            if world.get_resource::<crate::gizmo::GizmoState>().is_none() {
                world.insert_resource(crate::gizmo::GizmoState::default());
            }
            let Some(state) = world.get_resource_mut::<crate::gizmo::GizmoState>() else {
                return Err(format!("{command:?}: the gizmo state could not be claimed"));
            };
            let previous =
                crate::gizmo::set_resource_field(&mut state.settings, resource, field, value)
                    .map_err(|message| format!("{command:?}: {message}"))?;
            Ok(CommandOutcome::inverse(EditorCommand::SetResourceField {
                resource: resource.clone(),
                field: field.clone(),
                value: previous,
            }))
        }

        other => unreachable!("{:?} is not a world command", other.family()),
    }
}

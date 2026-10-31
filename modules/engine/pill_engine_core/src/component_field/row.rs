//! Row access, the field reads and the field writes.
//!
//! # Responsibilities
//!
//! - Resolve a component's field to a live row: the entity's own archetype, its
//!   column, and the byte range a field occupies there.
//! - Read one field or every field of a row as [`FieldValue`](super::FieldValue),
//!   and write one back, stamping the row's `changed` tick so `Changed<T>`
//!   observes the edit next frame.
//! - Report every refusal as a [`ComponentFieldError`](super::ComponentFieldError)
//!   naming the component and the field, without handing a raw pointer out.
//!
//! # Design
//!
//! Every entry point resolves through the entity's own archetype, the one place
//! that names the generation which created the data; a registry-wide name
//! lookup can see several registrations under one name. Values are validated
//! against the registered layout before anything is written, and a refused
//! write leaves the row untouched.

// Current crate
use super::codec::{decode_field, encode_field_write, encode_scalar, is_scalar_tag, scalar_size};
use super::{ComponentFieldError, FieldValue};
use crate::archetype::{ArchetypeId, ComponentColumn};
use crate::component::{ComponentId, Tick};
use crate::component_registry::ComponentFieldDescriptor;
use crate::entity::Entity;
use crate::world::World;

/// Format a copied component row for native debugger helpers.
///
/// This accepts a byte slice rather than a component type, so it works for
/// Rust, renderer, and managed descriptor columns alike when paired with the
/// registered field layout.
pub fn format_component_debug(fields: &[ComponentFieldDescriptor], bytes: &[u8]) -> String {
    fields
        .iter()
        .map(|field| format!("{}={:?}", field.name, decode_field(bytes, field)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One field's location: the entity's row, its component and the descriptor.
///
/// What every single-field entry point needs before it can read or write, so
/// the resolution rule and its error payloads live in
/// [`World::resolve_field`] rather than in each entry point.
struct ResolvedField {
    /// The archetype holding the entity.
    archetype_id: ArchetypeId,
    /// The entity's row within that archetype.
    row: usize,
    /// The component carried by that entity.
    component_id: ComponentId,
    /// The requested field's descriptor.
    descriptor: ComponentFieldDescriptor,
}

impl World {
    /// The archetype id and row of a live entity, for component access.
    pub(super) fn entity_row_location(&self, entity: Entity) -> Option<(ArchetypeId, usize)> {
        let location = self.entity_locations.get(&entity)?;
        Some((location.archetype_id, location.index_in_archetype))
    }

    /// The registered field layout of a component, refusing an empty one.
    ///
    /// Borrowed rather than copied: the field entry points read one or every
    /// descriptor out of it, and the inspector calls them once per row per
    /// frame.
    fn field_layout(
        &self,
        component_id: ComponentId,
        component: &str,
    ) -> Result<&[ComponentFieldDescriptor], ComponentFieldError> {
        let layout = self.component_field_layout(component_id).ok_or(
            ComponentFieldError::ComponentHasNoFieldLayout {
                component: component.to_string(),
            },
        )?;
        if layout.is_empty() {
            return Err(ComponentFieldError::ComponentHasNoFieldLayout {
                component: component.to_string(),
            });
        }
        Ok(layout)
    }

    /// Locate one field of one component on one entity.
    ///
    /// The one place entity, component, layout and field are resolved, so a
    /// new entry point cannot invent its own error path: every miss reports the
    /// same variant with the same payload.
    fn resolve_field(
        &self,
        entity: Entity,
        component: &str,
        field: &str,
    ) -> Result<ResolvedField, ComponentFieldError> {
        let (archetype_id, row) = self
            .entity_row_location(entity)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let component_id = self.resolve_entity_component_id(entity, component).ok_or(
            ComponentFieldError::ComponentNotFound {
                component: component.to_string(),
            },
        )?;
        let descriptor = self
            .field_layout(component_id, component)?
            .iter()
            .find(|descriptor| descriptor.name == field)
            .ok_or(ComponentFieldError::FieldNotFound {
                component: component.to_string(),
                field: field.to_string(),
            })?;
        Ok(ResolvedField {
            archetype_id,
            row,
            component_id,
            descriptor: descriptor.clone(),
        })
    }

    /// One component column of one archetype, for a row that exists.
    ///
    /// `component_name` only feeds the error path: the caller resolved the
    /// component through the entity, so a missing column or a row past its end
    /// is a storage desync and reports the component the caller named.
    fn row_column(
        &self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
    ) -> Result<&ComponentColumn, ComponentFieldError> {
        let column = self
            .archetypes
            .get(&archetype_id)
            .ok_or(ComponentFieldError::EntityNotFound)?
            .component_storages
            .get(component_id)
            .ok_or(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            })?;
        if row >= column.len() {
            return Err(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            });
        }
        Ok(column)
    }

    /// [`Self::row_column`]'s mutable twin, for the write lanes.
    fn row_column_mut(
        &mut self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
    ) -> Result<&mut ComponentColumn, ComponentFieldError> {
        let column = self
            .archetypes
            .get_mut(&archetype_id)
            .ok_or(ComponentFieldError::EntityNotFound)?
            .component_storages
            .get_mut(component_id)
            .ok_or(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            })?;
        if row >= column.len() {
            return Err(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            });
        }
        Ok(column)
    }

    /// Copy one entity's component row for `component_id` into an owned buffer.
    ///
    /// Works for native (Rust) and descriptor (foreign-language) columns alike.
    /// The caller owns the returned bytes, so no borrow outlives this call.
    fn copy_component_row_bytes(
        &self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
    ) -> Result<Vec<u8>, ComponentFieldError> {
        let column = self.row_column(archetype_id, row, component_id, component_name)?;
        if !component_id.is_native_storage() {
            return column.bytes(row).map(<[u8]>::to_vec).ok_or(
                ComponentFieldError::ComponentStorageMissing {
                    component: component_name.to_string(),
                },
            );
        }
        let element_size = column.element_size();
        let mut buffer = vec![0u8; element_size];
        // SAFETY: the row is within the column (`row < len`, checked by the
        // lookup) and the buffer is exactly one element long; nothing mutates
        // the column while this shared borrow is alive.
        unsafe {
            std::ptr::copy_nonoverlapping(
                column.raw_ptr().add(row * element_size),
                buffer.as_mut_ptr(),
                element_size,
            );
        }
        Ok(buffer)
    }
}

impl World {
    /// Read one field of one component on one entity.
    ///
    /// The component is resolved through the entity's own archetype (see
    /// [`Self::resolve_entity_component_id`]), so the returned value always
    /// describes data the entity really holds. Reads never mutate and never
    /// stamp a change tick.
    pub fn read_component_field(
        &self,
        entity: Entity,
        component_name: &str,
        field_name: &str,
    ) -> Result<FieldValue, ComponentFieldError> {
        let ResolvedField {
            archetype_id,
            row,
            component_id,
            descriptor,
        } = self.resolve_field(entity, component_name, field_name)?;

        // Copy the row and decode the single field. A copy that comes back
        // missing after both lookups succeeded is a storage desync, not a dead
        // handle, so it reports the component by name.
        let row_bytes =
            self.copy_component_row_bytes(archetype_id, row, component_id, component_name)?;
        Ok(self.with_asset_type(
            component_id,
            &descriptor,
            decode_field(&row_bytes, &descriptor),
        ))
    }

    /// Read every field of one component in layout order, for the inspector.
    ///
    /// Cheaper than N [`Self::read_component_field`] calls: the row is located
    /// and copied once.
    pub fn read_component_fields(
        &self,
        entity: Entity,
        component_name: &str,
    ) -> Result<Vec<FieldValue>, ComponentFieldError> {
        // The entity and component half of `resolve_field`; this one reads every
        // descriptor, so it keeps the borrowed layout instead of one clone.
        let (archetype_id, row) = self
            .entity_row_location(entity)
            .ok_or(ComponentFieldError::EntityNotFound)?;
        let component_id = self
            .resolve_entity_component_id(entity, component_name)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;

        // Copy the layout and the whole row once.
        let layout = self.field_layout(component_id, component_name)?;
        let row_bytes =
            self.copy_component_row_bytes(archetype_id, row, component_id, component_name)?;

        // Decode every field from the one row image.
        Ok(layout
            .iter()
            .map(|descriptor| {
                self.with_asset_type(
                    component_id,
                    descriptor,
                    decode_field(&row_bytes, descriptor),
                )
            })
            .collect())
    }

    /// `value` with the asset type a handle field was registered with, when
    /// it is one; any other value unchanged.
    fn with_asset_type(
        &self,
        component_id: ComponentId,
        descriptor: &ComponentFieldDescriptor,
        value: FieldValue,
    ) -> FieldValue {
        match value {
            FieldValue::Asset {
                asset_type: None,
                index,
                generation,
            } => FieldValue::Asset {
                asset_type: self
                    .asset_field_type(component_id, descriptor.name)
                    .map(str::to_owned),
                index,
                generation,
            },
            other => other,
        }
    }

    /// Record that `component`'s handle field `field` references assets of the
    /// type registered as `asset_type`.
    ///
    /// Called by `#[derive(PillComponent)]`'s generated registration for each
    /// `Handle<T>` field, with `T`'s shared name, and by a foreign-language
    /// backend for its components. The name is copied: the caller's string
    /// may live in a module that a reload unmaps.
    pub fn record_asset_field_type(
        &mut self,
        component: ComponentId,
        field: &str,
        asset_type: &str,
    ) {
        self.asset_field_types
            .entry(component)
            .or_default()
            .insert(field.to_owned(), asset_type.to_owned());
    }

    /// Refuse an asset value whose type is not the one `field` was recorded
    /// with; a value or a field without a recorded type passes.
    fn check_asset_type(
        &self,
        component: ComponentId,
        field: &str,
        value: &FieldValue,
    ) -> Result<(), ComponentFieldError> {
        let FieldValue::Asset {
            asset_type: Some(found),
            ..
        } = value
        else {
            return Ok(());
        };
        match self.asset_field_type(component, field) {
            Some(expected) if expected != found => Err(ComponentFieldError::AssetTypeMismatch {
                field: field.to_owned(),
                expected: expected.to_owned(),
                found: found.clone(),
            }),
            _ => Ok(()),
        }
    }

    /// The asset type `component`'s handle field `field` references, if
    /// recorded.
    pub fn asset_field_type(&self, component: ComponentId, field: &str) -> Option<&str> {
        self.asset_field_types
            .get(&component)?
            .get(field)
            .map(String::as_str)
    }
}

/// The row one field write targets, and the names its refusals report.
pub(super) struct RowWriteTarget<'a> {
    /// Archetype holding the row.
    pub(super) archetype_id: ArchetypeId,
    /// Row within that archetype.
    pub(super) row: usize,
    /// Component whose column holds the field.
    pub(super) component_id: ComponentId,
    /// Component name the error paths report.
    pub(super) component_name: &'a str,
    /// Field name the error paths report.
    pub(super) field_name: &'a str,
}

/// A validated byte payload and where in the component row it belongs.
pub(super) struct FieldWrite {
    /// Byte offset of the payload within the component row.
    pub(super) row_offset: usize,
    /// The payload bytes (a scalar, one array element, or a whole array).
    pub(super) bytes: Vec<u8>,
}

impl World {
    /// Write one field of a live component.
    ///
    /// Resolves the component through the entity's own archetype, validates the
    /// value against the registered field layout, writes it through the column,
    /// and stamps that row's `ComponentTicks::changed` so `Changed<T>` systems
    /// observe the edit on their next run. [`Self::get_component_mut`] hands out
    /// the same column slot without that stamp. Nothing is written when any
    /// validation fails.
    pub fn write_component_field(
        &mut self,
        entity: Entity,
        component_name: &str,
        field_name: &str,
        value: FieldValue,
    ) -> Result<(), ComponentFieldError> {
        // Resolve, then validate and encode the payload BEFORE touching any
        // column, so a refused edit leaves the world untouched.
        let ResolvedField {
            archetype_id,
            row,
            component_id,
            descriptor,
        } = self.resolve_field(entity, component_name, field_name)?;
        self.check_asset_type(component_id, field_name, &value)?;
        let write = encode_field_write(&descriptor, field_name, &value)?;

        // Write the payload and stamp the row's changed tick, each in its own
        // archetype borrow so column and tick lookups never overlap.
        //
        // The world tick is bumped first so the stamped `changed` value is
        // strictly newer than every system's `last_run` baseline. Edits are
        // applied between frames, when the tick is otherwise frozen; without
        // the bump a `Changed<T>` filter would compare the new value against
        // an equal baseline and never observe the edit.
        let current_tick = self.increment_change_tick();
        self.apply_row_write(
            RowWriteTarget {
                archetype_id,
                row,
                component_id,
                component_name,
                field_name,
            },
            write.row_offset,
            &write.bytes,
        )?;
        self.stamp_row_changed(
            archetype_id,
            row,
            component_id,
            component_name,
            current_tick,
        )
    }

    /// Build a zero-initialised component image for a registered type name,
    /// then write the supplied field values over it.
    ///
    /// Used by the editor to create entities or add components from field
    /// values without knowing the concrete Rust type. Creation has no entity
    /// yet, so the component is resolved through the registry (this is the one
    /// editor path where the registry-wide name resolver is correct).
    /// Zero-initialisation is only valid for components with a non-empty
    /// registered field layout, which this enforces.
    pub fn build_component_image(
        &self,
        component_name: &str,
        fields: &[(String, FieldValue)],
    ) -> Result<Vec<u8>, ComponentFieldError> {
        // An ambiguous name means two registrations claim it, so there is no
        // one component to build an image for; it reads the same as "not
        // found" to the editor, which is the safe outcome either way.
        let component_id = self
            .resolve_component_id_by_name_any(component_name)
            .unwrap_or(None)
            .ok_or(ComponentFieldError::ComponentNotFound {
                component: component_name.to_string(),
            })?;
        let layout = self.field_layout(component_id, component_name)?;
        let (size, _) = self.component_layout(component_id).ok_or(
            ComponentFieldError::ComponentHasNoFieldLayout {
                component: component_name.to_string(),
            },
        )?;
        let mut image = vec![0u8; size];
        for (field_name, value) in fields {
            let descriptor = layout
                .iter()
                .find(|descriptor| descriptor.name == *field_name)
                .ok_or(ComponentFieldError::FieldNotFound {
                    component: component_name.to_string(),
                    field: field_name.clone(),
                })?;
            self.check_asset_type(component_id, field_name, value)?;
            let write = encode_field_write(descriptor, field_name, value)?;
            let target = image
                .get_mut(write.row_offset..write.row_offset + write.bytes.len())
                .ok_or(ComponentFieldError::UnsupportedField {
                    field: field_name.clone(),
                    reason: "write extends past the component image",
                })?;
            target.copy_from_slice(&write.bytes);
        }
        Ok(image)
    }

    /// Write one element of a fixed-size array field.
    pub fn write_component_array_element(
        &mut self,
        entity: Entity,
        component_name: &str,
        field_name: &str,
        index: usize,
        value: FieldValue,
    ) -> Result<(), ComponentFieldError> {
        let ResolvedField {
            archetype_id,
            row,
            component_id,
            descriptor,
        } = self.resolve_field(entity, component_name, field_name)?;

        // The field must be a fixed array of scalar primitives.
        let Some(inner_tag) = descriptor.type_tag.strip_prefix("array:") else {
            return Err(ComponentFieldError::TypeMismatch {
                field: field_name.to_string(),
                expected: descriptor.type_tag,
                found: value.variant_tag(),
            });
        };
        if !is_scalar_tag(inner_tag) {
            return Err(ComponentFieldError::UnsupportedField {
                field: field_name.to_string(),
                reason: "array elements are not scalar primitives",
            });
        }
        if index >= descriptor.element_count {
            return Err(ComponentFieldError::ArrayIndexOutOfRange {
                field: field_name.to_string(),
                index,
                length: descriptor.element_count,
            });
        }
        let element_size = scalar_size(inner_tag).ok_or(ComponentFieldError::UnsupportedField {
            field: field_name.to_string(),
            reason: "array element size is unknown",
        })?;
        let mut bytes = Vec::with_capacity(element_size);
        if !encode_scalar(inner_tag, &value, &mut bytes) {
            return Err(ComponentFieldError::TypeMismatch {
                field: field_name.to_string(),
                expected: inner_tag,
                found: value.variant_tag(),
            });
        }

        // Write the element and stamp the row's changed tick. The tick is
        // bumped first for the same reason the whole-field write bumps it: a
        // between-frame edit must be visible to `Changed<T>` filters.
        let row_offset = descriptor.offset + index * element_size;
        let current_tick = self.increment_change_tick();
        self.apply_row_write(
            RowWriteTarget {
                archetype_id,
                row,
                component_id,
                component_name,
                field_name,
            },
            row_offset,
            &bytes,
        )?;
        self.stamp_row_changed(
            archetype_id,
            row,
            component_id,
            component_name,
            current_tick,
        )
    }

    /// Overwrite `bytes.len()` bytes at `row_offset` of one component row.
    ///
    /// `target.component_name` only feeds the error paths: the caller resolved
    /// the component through the entity, so a missing column or row here is a
    /// storage desync and reports the component the caller named.
    pub(super) fn apply_row_write(
        &mut self,
        target: RowWriteTarget<'_>,
        row_offset: usize,
        bytes: &[u8],
    ) -> Result<(), ComponentFieldError> {
        let RowWriteTarget {
            archetype_id,
            row,
            component_id,
            component_name,
            field_name,
        } = target;
        // One construction for every "the payload does not fit the row" refusal
        // in this function, so the message always names the field.
        let past_the_row = || ComponentFieldError::UnsupportedField {
            field: field_name.to_string(),
            reason: "write extends past the component row",
        };
        match component_id {
            _ if component_id.is_native_storage() => {
                let column =
                    self.row_column_mut(archetype_id, row, component_id, component_name)?;
                let element_size = column.element_size();
                // SAFETY: the lookup checked `row < column.len()`, so the
                // pointer stays inside the column's allocation, and the column
                // borrow keeps that allocation alive through this statement.
                let destination = unsafe { column.as_mut_ptr().add(row * element_size) };
                // Checked arithmetic on the way to the write: a wrapping sum
                // would pass a caller-supplied offset near `usize::MAX` and
                // let the copy below run off the row.
                let Some(end) = row_offset
                    .checked_add(bytes.len())
                    .filter(|end| *end <= element_size)
                else {
                    return Err(past_the_row());
                };
                // SAFETY: the destination row is initialized (`row < len`) and
                // the payload fits within it (checked above); the column is not
                // reallocated while this borrow is held.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        destination.add(row_offset),
                        end - row_offset,
                    );
                }
                Ok(())
            }
            _ => {
                let mut row_bytes = self
                    .row_column_mut(archetype_id, row, component_id, component_name)?
                    .bytes(row)
                    .ok_or(ComponentFieldError::ComponentStorageMissing {
                        component: component_name.to_string(),
                    })?
                    .to_vec();
                let element_size = row_bytes.len();
                // The same checked arithmetic as the native branch: the row
                // slice below would otherwise be ranged with a wrapped sum.
                let Some(end) = row_offset
                    .checked_add(bytes.len())
                    .filter(|end| *end <= element_size)
                else {
                    return Err(past_the_row());
                };
                let target = row_bytes
                    .get_mut(row_offset..end)
                    .ok_or_else(past_the_row)?;

                target.copy_from_slice(bytes);
                self.row_column_mut(archetype_id, row, component_id, component_name)?
                    .set_bytes(row, &row_bytes)
                    .map_err(|_| ComponentFieldError::ComponentStorageMissing {
                        component: component_name.to_string(),
                    })
            }
        }
    }

    /// Stamp one row's `changed` tick so `Changed<T>` systems react.
    fn stamp_row_changed(
        &mut self,
        archetype_id: ArchetypeId,
        row: usize,
        component_id: ComponentId,
        component_name: &str,
        tick: Tick,
    ) -> Result<(), ComponentFieldError> {
        let row_ticks = self
            .row_column_mut(archetype_id, row, component_id, component_name)?
            .row_ticks_mut(row)
            .ok_or(ComponentFieldError::ComponentStorageMissing {
                component: component_name.to_string(),
            })?;
        row_ticks.set_changed(tick);
        Ok(())
    }
}

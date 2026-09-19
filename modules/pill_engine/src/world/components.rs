//! Component registration, storage access, and per-entity attachment.
//!
//! # Responsibilities
//!
//! - Registers component types in all three lanes - a Rust type, a type shared
//!   between binaries, and a layout described at runtime by a manifest - and
//!   keeps the per-type storage descriptions a reload replaces.
//! - Reshapes and re-homes live columns across a reload: a relayout rewrites a
//!   column's rows into a new shape, a remap moves them onto a successor
//!   registration, and the re-homing pass re-points every column's function
//!   table at code that is still mapped.
//! - Reads and writes one entity's component, and adds or removes one, which
//!   migrates the entity between archetypes.
//!
//! # Design
//!
//! Registration is data rather than code: a component's storage description is
//! a layout plus a function table, never a closure monomorphized into the
//! artifact that registered it, so a column outlives the generation that filled
//! it and the table is replaced on reload. The three lanes differ only in what
//! vouches for a row's meaning - an exact `TypeId`, a name-derived identity, or
//! nothing but a validated layout - and share one column type underneath.

use super::*;

// =============================================================================
// World - Components
// =============================================================================

impl World {
    /// Reserve capacity for at least `additional` instances of component `T`
    /// in every archetype that currently contains `T`.
    ///
    /// This is a hint - the ECS may reserve more or less than requested
    /// depending on archetype distribution. Call after registering components
    /// and after creating archetypes you intend to populate.
    pub fn reserve_components<T>(&mut self, additional: usize)
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        for archetype in self.archetypes.values_mut() {
            if archetype.component_types.contains(&component_id) {
                let storage = archetype.component_storages.column_of_mut::<T>();
                storage.reserve::<T>(additional);
            }
        }
    }

    /// Return one component chunk together with its parallel change-tick column.
    ///
    /// Language bindings use this form when exposing writable component data.
    /// Both slices have identical lengths and row `i` in `ticks` describes row
    /// `i` in `components`.
    pub fn component_chunk_with_ticks_mut<T>(
        &mut self,
        chunk_index: usize,
    ) -> Option<(ArchetypeId, &mut [T], &mut [ComponentTicks])>
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let (components, ticks) = archetype
            .component_storages
            .column_of_mut::<T>()
            .rows_and_ticks_mut::<T>();
        Some((archetype_id, components, ticks))
    }

    /// Return one component chunk from one already-known archetype, with its
    /// parallel change-tick column.
    ///
    /// The archetype-scoped twin of [`Self::component_chunk_with_ticks_mut`]:
    /// language bindings that hold an archetype identity (from a driver chunk)
    /// resolve the remaining query terms directly instead of scanning chunk
    /// indices until the archetypes match.
    pub fn component_chunk_with_ticks_mut_in_archetype<T>(
        &mut self,
        archetype_id: ArchetypeId,
    ) -> Option<(ArchetypeId, &mut [T], &mut [ComponentTicks])>
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        if !archetype.component_types.contains(&component_id) {
            return None;
        }
        let (components, ticks) = archetype
            .component_storages
            .column_of_mut::<T>()
            .rows_and_ticks_mut::<T>();
        Some((archetype_id, components, ticks))
    }

    /// Register a component type with the World
    ///
    /// This must be called for each component type before it can be used.
    ///
    /// A component needs no `Clone`: an archetype move carries its row bitwise,
    /// so a type that cannot sensibly be cloned - a handle, a guard, anything
    /// whose duplication means something - is registrable like any other.
    pub fn register_component<T>(&mut self)
    where
        T: Component,
    {
        self.register_component_inner::<T>(&[]);
    }

    /// Shared registration body for the layout-less and layout-carrying entry
    /// points.
    ///
    /// `fields` is the compile-time field layout when the caller has one. It
    /// is forwarded to the registry, which records it so a second registration
    /// of the same component can be checked against the first - a diagnostic
    /// for an ordinary component, and the soundness check for a shared one.
    ///
    /// # Shared components
    ///
    /// A component that declares [`Component::shared_name`] resolves to the
    /// same [`ComponentId`] in every binary that links it, so the second
    /// binary's registration finds the bit already taken and **binds to the
    /// existing column** rather than allocating a second one: the registry
    /// reports `AlreadyPresent`, no new bit is consumed, and entities spawned
    /// from either binary land in the same archetype. Nothing here special-
    /// cases that - it falls out of the id being equal - which is why there is
    /// no window in which two binaries share a column but not a mask bit.
    pub(crate) fn register_component_inner<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "register component",
            [(
                "Component type being registered: {}",
                std::any::type_name::<T>()
            )]
        );
        let component_id = ComponentId::of::<T>();
        let type_name = crate::component::ComponentRegistry::registered_name::<T>();

        // Register component (bit index + name + layout)
        // `register_bit_with_layout` rather than `register`: the world does not
        // act on whether the type was already present, and re-registration is
        // normal here because a hot reload re-runs every `init`.
        let bit = match self
            .component_registry
            .register_bit_with_layout::<T>(fields)
        {
            Ok(bit) => bit,
            Err(error) => {
                // Neither the 128-type ceiling nor a shared-layout
                // disagreement is a programming error - both are outcomes of
                // what the user's binaries declare - so they are reported as
                // first-class diagnostics and recorded for the init entry
                // point instead of panicking. The caller (project/module init)
                // fails the reload transactionally when the error is drained
                // by `component_registry::register_all_components`.
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    type_name = %type_name,
                    error = %error,
                    remaining = self.component_registry.available_slots(),
                    "component registration failed"
                );
                self.record_registration_error(error);
                return;
            }
        };
        // Warn while headroom is still available but getting thin, so
        // exhaustion is visible before it is fatal.
        let remaining = self.component_registry.available_slots();
        if remaining <= REGISTRATION_HEADROOM_WARNING_THRESHOLD {
            warn!(
                target: pill_core::telemetry::telemetry_target::ECS,
                remaining,
                type_name = %type_name,
                "component-type headroom is low; the 128-type ceiling is approaching"
            );
        }
        let _ = bit;

        // Record the registration chronologically so the host can enumerate
        // which types one module's init registered at all (plain or
        // persistable), which is how a type dropped from a reloaded module is
        // told apart from one merely downgraded to a plain component. The id is
        // recorded beside the name because a rebuilt image gets a fresh
        // `TypeId` per name: comparing ids is what tells a failed generation's
        // leftovers apart from a rollback generation's re-registration.
        self.component_registration_log.push((
            type_name,
            component_id,
            self.component_registration_sequence,
        ));
        self.component_registration_sequence = self.component_registration_sequence.wrapping_add(1);

        // Register the storage factory as plain DATA (type id, layout, and a
        // per-type function table) instead of a closure that would be
        // monomorphized into this generation's DLL. The engine builds the
        // actual column in `Archetype::new` as a concrete `ComponentColumn`
        // with no trait-object vtable, and re-homes its function table on
        // every reload, so columns survive DLL unloads.
        //
        // A shared component's column is built with `of_shared`, so its
        // element-type check compares layout instead of `TypeId`. That is the
        // point where the compiler stops vouching for the type and the
        // registry's layout check above takes over: the second binary's `T` is
        // a different `TypeId` for the same type, and only the recorded size,
        // alignment and schema hash establish that it really is.
        //
        // Re-registering replaces the factory, so the function table points at
        // the most recently loaded generation - the same last-writer-wins rule
        // the reload path already relies on, with `rehome_native_columns`
        // re-pointing live columns at it afterwards.
        let storage_info = if T::shared_name().is_some() {
            crate::archetype::NativeColumnInfo::of::<T>(true)
        } else {
            crate::archetype::NativeColumnInfo::of::<T>(false)
        };
        // Keep the table that is about to be replaced: the migration that
        // consumes old-layout columns must drop them through the glue that
        // wrote them, and by then this registration's table is the one stamped
        // on every column - see `migrate_component_column_in_place`.
        if let Some(StorageFactory::Native(previous)) = self.storage_factories.get(&component_id) {
            self.retired_native_storage_ops
                .insert(component_id, previous.ops);
        }
        self.storage_factories
            .insert(component_id, StorageFactory::Native(storage_info));
    }

    /// Drain the first registration failure recorded by
    /// [`Self::register_component`], if any.
    ///
    /// The artifact-wide registration loop calls this after running every
    /// descriptor so the generated `init` can fail the reload with a non-zero
    /// status when the component-type ceiling was hit — a diagnosable startup
    /// failure rather than a silent half-registration.
    pub fn take_registration_error(&mut self) -> Option<WorldError> {
        self.registration_error.take()
    }

    /// Record a registration failure raised outside `register_component`, so
    /// the artifact-wide registration loop fails the reload the same way it
    /// does for the component-type ceiling.
    ///
    /// The first failure wins: later registrations in the same pass are often
    /// knock-on effects of the first, and the original is the diagnosable one.
    pub(crate) fn record_registration_error(&mut self, error: WorldError) {
        if self.registration_error.is_none() {
            self.registration_error = Some(error);
        }
    }

    /// Register a component together with its compile-time field layout, so
    /// the C# mirror codegen can emit a typed struct. Components registered
    /// without field metadata (hand-registered, descriptor, or unit types) keep
    /// the opaque ABI-blob mirror.
    pub fn register_component_with_layout<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component,
    {
        self.register_component_inner::<T>(fields);
        self.component_field_layouts.insert(
            ComponentId::of::<T>(),
            ComponentFieldLayout::from_static(fields),
        );
    }

    /// Record a runtime-described layout for a descriptor component.
    ///
    /// Overwrites any previous layout for the same id, so a manifest reload
    /// replaces rather than accumulates. Used by the C# backend so managed
    /// components become field-inspectable in the editor.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentFieldError::UnsupportedField`] for a descriptor that
    /// reaches past the component's registered size or names a container tag.
    /// A descriptor row is raw bytes with no Rust value in it, so the accessors
    /// have to be able to trust both: the bound keeps them inside the row, and
    /// the tag check keeps them from following a `(pointer, length)` pair the
    /// row never held.
    pub fn register_component_descriptor_with_layout(
        &mut self,
        component_id: ComponentId,
        fields: Vec<crate::component_registry::ComponentFieldDescriptor>,
    ) -> Result<(), crate::component_field::ComponentFieldError> {
        let component_size = self.component_registry.get_size(&component_id);
        for field in &fields {
            let Some(end) = field.offset.checked_add(field.size) else {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field's range overflows",
                    },
                );
            };
            if component_size.is_some_and(|size| end > size) {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field extends past the component's registered size",
                    },
                );
            }
            if !component_id.is_native_storage()
                && crate::component_field::is_container_tag(field.type_tag)
            {
                return Err(crate::component_field::ComponentFieldError::UnsupportedField {
                    field: field.name.to_string(),
                    reason: "a container tag is read as a native pointer and length, which a descriptor row does not hold",
                });
            }
        }
        self.component_field_layouts
            .insert(component_id, ComponentFieldLayout::from_owned(fields));
        Ok(())
    }

    /// Re-home every native column's per-type function table.
    ///
    /// Called by the host after each generation's `init` and before migration.
    /// Columns store function pointers into the DLL that created them; when
    /// that DLL is evicted from the reload graveyard the pointers dangle. This
    /// pass refreshes each column from the latest factory registered for its
    /// component id, so:
    ///
    /// - unchanged types point at the freshly loaded generation (still mapped),
    /// - schema-changed types point at the previous generation (still mapped
    ///   while the migration consumes and drops their columns),
    /// - type-erased (foreign-language) columns are untouched.
    pub fn rehome_native_columns(&mut self) {
        // Step 1: Snapshot the current per-type function tables first so the
        // immutable borrow of `storage_factories` cannot conflict with the
        // mutable borrow of `archetypes` below.
        let factory_ops: HashMap<ComponentId, crate::archetype::ColumnOps> = self
            .storage_factories
            .iter()
            .filter_map(|(component_id, factory)| match factory {
                StorageFactory::Native(info) => Some((*component_id, info.ops)),
                StorageFactory::Descriptor(_) => None,
            })
            .collect();

        // Step 2: Refresh every column whose component id has a native factory.
        for archetype in self.archetypes.values_mut() {
            for &component_id in &archetype.component_types {
                let Some(&ops) = factory_ops.get(&component_id) else {
                    continue;
                };
                if let Some(column) = archetype.component_storages.get_mut(component_id) {
                    column.refresh_ops(ops);
                }
            }
        }
    }

    /// Native columns the world still stores although no factory describes
    /// their component id.
    ///
    /// A column outlives its registration when a type is re-registered under a
    /// fresh id - the rebuilt image's "same name, fresh `TypeId`" case - and
    /// the old id is never swept: the archetype keeps the id in its mask and
    /// the column in its storage, but no factory is left to describe the type.
    /// The host's reload path uses this as the gate on unmapping a retired
    /// image, because a column's drop table lives in the image that produced
    /// it.
    pub fn columns_without_factory(&self) -> usize {
        self.archetypes
            .values()
            .flat_map(|archetype| archetype.component_types.iter())
            .filter(|component_id| {
                component_id.is_native_storage()
                    && !self.storage_factories.contains_key(component_id)
            })
            .count()
    }

    /// Drop every native column whose component id has no factory left.
    ///
    /// The id-keyed counterpart of `drop_forgotten_components`: the name-keyed
    /// sweep cannot reach these, because the name resolves to whatever
    /// registration claimed it last while the stale column keeps the id the
    /// previous generation minted. Each affected component is removed from
    /// every entity that carries it, which empties the archetypes holding it
    /// and drops their columns through the table that is still mapped while
    /// this runs. Returns the number of orphaned component ids swept.
    pub fn drop_columns_without_factory(&mut self) -> usize {
        // Step 1: Collect the orphaned ids across every archetype. Deduplicated
        // by hand: the list is a handful of entries at most, and a `HashSet`
        // would hide the ordering.
        let mut orphaned_ids: Vec<ComponentId> = Vec::new();
        for archetype in self.archetypes.values() {
            for &component_id in &archetype.component_types {
                if component_id.is_native_storage()
                    && !self.storage_factories.contains_key(&component_id)
                    && !orphaned_ids.contains(&component_id)
                {
                    orphaned_ids.push(component_id);
                }
            }
        }

        // Step 2: Remove each orphaned component from every entity carrying
        // it. The entities are collected up front so the mutable borrows
        // during removal never overlap the iteration, exactly as in
        // `drop_forgotten_components`.
        for component_id in &orphaned_ids {
            let entities: Vec<Entity> = self
                .entity_locations
                .iter()
                .filter(|(_, location)| {
                    self.archetypes
                        .get(&location.archetype_id)
                        .is_some_and(|archetype| archetype.component_types.contains(component_id))
                })
                .map(|(entity, _)| *entity)
                .collect();
            for entity in entities {
                let _ = self.remove_component_by_id(entity, *component_id);
            }
        }
        orphaned_ids.len()
    }

    /// Register an unmanaged component described by an external language.
    ///
    /// Re-registering an identical layout and name for the same `stable_id`
    /// is idempotent and returns the existing [`ComponentId`].
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorStableIdZero`] for a zero `stable_id`,
    /// [`WorldError::DescriptorSizeZero`] for a zero `size`,
    /// [`WorldError::DescriptorAlignmentInvalid`] for a zero or non-power-of-two
    /// `align`, [`WorldError::DescriptorLayoutInvalid`] for an oversized layout,
    /// [`WorldError::DescriptorAlreadyRegistered`] when the `stable_id` is
    /// already taken by a different layout or name, and
    /// [`WorldError::ComponentTypeLimitExceeded`] when the registry is full.
    ///
    /// `blittability` is the caller's evidence that every field of the
    /// component is a blittable value type. It is required, not optional:
    /// the storage built from this layout copies rows with `ptr::copy`, frees
    /// them without running element destructors, and is shared across threads,
    /// so there is no check this function could run itself that would make an
    /// owning layout safe. [`Blittability`] documents what each constructor
    /// proves.
    pub fn register_component_descriptor(
        &mut self,
        stable_id: u128,
        name: impl Into<String>,
        size: usize,
        align: usize,
        schema_hash: u64,
        blittability: Blittability,
    ) -> Result<ComponentId, WorldError> {
        if stable_id == 0 {
            return Err(WorldError::DescriptorStableIdZero);
        }
        // The same three checks a relayout runs, so a layout this refuses can
        // never be one the storage would accept later.
        validate_component_layout(size, align)?;
        let name = name.into();
        let component_id = ComponentId::descriptor(stable_id);
        if let Some(existing) = self.storage_factories.get(&component_id) {
            return match existing {
                StorageFactory::Descriptor(layout)
                    if layout.size == size
                        && layout.align == align
                        && layout.schema_hash == schema_hash
                        && self.component_registry.get_name(&component_id) == Some(&name) =>
                {
                    Ok(component_id)
                }
                _ => Err(WorldError::DescriptorAlreadyRegistered),
            };
        }
        // A name already claimed by a live column belongs to a different
        // component, not to an earlier generation of this one: a manifest
        // reload derives `stable_id` from the same full name, so it lands on
        // the idempotent path above rather than here. Registering a second
        // component under that name would make every later lookup by name
        // ambiguous, and managed code binds by name, so the collision is
        // reported now instead of producing a binding to the wrong column.
        if let Some((existing_id, live_rows)) = self.live_component_with_name(&name, component_id) {
            return Err(WorldError::ComponentNameCollision {
                type_name: name,
                existing_id,
                incoming_id: component_id,
                live_rows,
            });
        }

        // The registry reports the 128-type ceiling as a typed error (with the
        // offending name and current count) rather than panicking; propagate it.
        self.component_registry
            .register_descriptor(stable_id, name, size)?;
        self.storage_factories.insert(
            component_id,
            StorageFactory::Descriptor(ColumnLayout::new(size, align, schema_hash, blittability)?),
        );
        Ok(component_id)
    }

    /// Replace a descriptor component's layout, migrating every stored row.
    ///
    /// The one way a descriptor component's storage shape may change after
    /// registration. The component keeps its id, its registry bit and its
    /// archetype membership - a fresh registration would allocate a new bit
    /// index, and that index is baked into archetype masks and scheduled access
    /// masks - so only the columns' element layout and the recorded size move.
    ///
    /// `plan` says where each byte of a new row comes from
    /// ([`FieldPlan::between`] builds one from two field lists). Anything
    /// it does not cover is left zero. Row order and change ticks are
    /// preserved, so entity locations stay valid and no system observes a
    /// spurious `Added`.
    ///
    /// The new layout and the plan are checked before the first column is
    /// touched, so a rejected call leaves the world exactly as it was.
    ///
    /// Returns the number of rows migrated.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorSizeZero`],
    /// [`WorldError::DescriptorAlignmentInvalid`] or
    /// [`WorldError::DescriptorLayoutInvalid`] when the new layout cannot describe
    /// storage, [`WorldError::DescriptorComponentNotRegistered`] when the id is not
    /// a registered descriptor component, [`WorldError::DescriptorStorageMissing`]
    /// when an archetype lists the component without a column,
    /// [`WorldError::ComponentColumnLayoutMismatch`] when a column's element size
    /// disagrees with the registered layout, and [`WorldError::DescriptorRowInvalid`]
    /// when a planned field falls outside a row of either layout.
    pub fn relayout_descriptor_component(
        &mut self,
        component_id: ComponentId,
        size: usize,
        align: usize,
        schema_hash: u64,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        validate_component_layout(size, align)?;

        // The previous layout is what the plan's source offsets are measured
        // against, so it has to be the one the columns are actually using.
        let Some(StorageFactory::Descriptor(previous)) = self.storage_factories.get(&component_id)
        else {
            return Err(WorldError::DescriptorComponentNotRegistered { id: component_id });
        };
        let previous_size = previous.size;
        plan.validate(previous_size, size)?;

        // Every archetype holding the component, including the ones with no
        // rows: their columns must carry the new element layout so the next
        // entity added to them is stored at the new shape.
        // A relayout keeps the witness and the release hook: the shape
        // changes, the promise about what a row owns does not.
        let layout = ColumnLayout {
            size,
            align,
            schema_hash,
            blittability: previous.blittability,
        };

        // Step 1: Verify every column before touching one. The plan and the
        // factory were checked above; what remains is the possibility that a
        // column disagrees with the factory - the desync a partially applied
        // relayout used to produce - so the whole set is checked first and the
        // report names the column that disagrees.
        let mut targets = Vec::new();
        for (archetype_id, archetype) in &self.archetypes {
            if !archetype.component_types.contains(&component_id) {
                continue;
            }
            let Some(column) = archetype.component_storages.get(component_id) else {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id,
                    archetype_id: *archetype_id,
                });
            };
            if column.element_size() != previous_size {
                return Err(WorldError::ComponentColumnLayoutMismatch {
                    component_id,
                    archetype_id: *archetype_id,
                    expected: previous_size,
                    actual: column.element_size(),
                });
            }
            targets.push(*archetype_id);
        }

        // Step 2: Migrate. Every column here was verified a moment ago and
        // nothing in between reshapes the map, so the lookups cannot miss.
        let mut migrated = 0;
        for archetype_id in targets {
            let archetype = self
                .archetypes
                .get_mut(&archetype_id)
                .expect("the verification pass listed only present archetypes");
            let column = archetype
                .component_storages
                .get_mut(component_id)
                .expect("the verification pass checked the column is present");
            migrated += column.relayout_validated(layout.clone(), plan, previous_size);
        }

        // Publish the new layout to everyone who reads it back: the storage
        // factory the chunk accessors consult, and the registry copy the
        // diagnostics and later registrations compare against. Both records
        // move together, or `get_layout` keeps reporting the placeholder
        // alignment registration started with.
        self.component_registry
            .update_descriptor_layout(&component_id, &layout);
        self.storage_factories
            .insert(component_id, StorageFactory::Descriptor(layout));
        Ok(migrated)
    }

    /// Move every row of one descriptor component into another and retire the
    /// source registration.
    ///
    /// The rename half of the managed manifest's storage story: a successor
    /// registration derives its id from its own name, so rows cannot stay put
    /// the way a relayout keeps them. Each row is reshaped through `plan` - the
    /// same [`FieldPlan`] a relayout uses, measured from the source's field
    /// layout to the successor's - and added to the successor before it is
    /// removed from the source, so an entity whose only component this is
    /// survives instead of being destroyed when the removal empties it.
    ///
    /// The source registration is retired once every row has moved: its name
    /// claim, registry entry and storage factory go, and its bit is released
    /// when no archetype with live rows still references it. A second remap out
    /// of the source is therefore impossible, which is what the caller wants:
    /// the source no longer exists.
    ///
    /// Returns the number of rows moved.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorRemapSelf`] when both ids are equal,
    /// [`WorldError::DescriptorComponentNotRegistered`] when either id is not a
    /// registered descriptor component,
    /// [`WorldError::DescriptorComponentAlreadyPresent`] when the successor
    /// already carries rows (a fresh registration has none; merging two row
    /// sets would make the plan's source side wrong for half its input),
    /// [`WorldError::DescriptorRowInvalid`] when the plan does not fit the two
    /// layouts, and [`WorldError::DescriptorStorageMissing`] when an archetype
    /// lists the source without a column.
    pub fn remap_descriptor_component(
        &mut self,
        old_component_id: ComponentId,
        new_component_id: ComponentId,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        if old_component_id == new_component_id {
            return Err(WorldError::DescriptorRemapSelf);
        }
        let old_size = match self.storage_factories.get(&old_component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => {
                return Err(WorldError::DescriptorComponentNotRegistered {
                    id: old_component_id,
                })
            }
        };
        let new_size = match self.storage_factories.get(&new_component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => {
                return Err(WorldError::DescriptorComponentNotRegistered {
                    id: new_component_id,
                })
            }
        };
        plan.validate(old_size, new_size)?;
        if self.live_row_count(new_component_id) != 0 {
            return Err(WorldError::DescriptorComponentAlreadyPresent);
        }

        // Step 1: Read and reshape every row before the first mutation, so a
        // row the plan cannot describe fails before anything has moved. The
        // plan was validated against the registered sizes, so the slices in
        // here are in bounds.
        let mut moves: Vec<(Entity, Vec<u8>)> = Vec::new();
        for (archetype_id, archetype) in &self.archetypes {
            if !archetype.component_types.contains(&old_component_id) {
                continue;
            }
            let Some(column) = archetype.component_storages.get(old_component_id) else {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id: old_component_id,
                    archetype_id: *archetype_id,
                });
            };
            for (row, entity) in archetype.entities.iter().enumerate() {
                let old_bytes = column.bytes(row).ok_or(WorldError::DescriptorRowInvalid)?;
                let mut new_bytes = vec![0_u8; new_size];
                for planned in plan.fields() {
                    let FieldSource::OldOffset(source_offset) = planned.source else {
                        continue;
                    };
                    new_bytes[planned.offset..planned.offset + planned.bytes]
                        .copy_from_slice(&old_bytes[source_offset..source_offset + planned.bytes]);
                }
                moves.push((*entity, new_bytes));
            }
        }

        // Step 2: Move every row. Add first, remove second: the two columns
        // coexist for a moment and the entity never loses its last component.
        let mut moved_rows = 0;
        for (entity, new_bytes) in moves {
            self.add_descriptor_component(entity, new_component_id, &new_bytes)?;
            self.remove_component_by_id(entity, old_component_id)
                .map_err(|_| WorldError::DescriptorComponentMissing)?;
            moved_rows += 1;
        }

        // Step 3: The source holds no rows now; retire it so its name can be
        // declared again and its bit returns to the pool. Every archetype that
        // carried it was emptied by Step 2, which is the precondition the
        // retirement's bit rule checks.
        self.retire_component_registration(old_component_id);
        Ok(moved_rows)
    }

    /// Return a raw descriptor component column for language bindings.
    pub fn descriptor_component_chunk_mut(
        &mut self,
        component_id: ComponentId,
        chunk_index: usize,
    ) -> Option<(ArchetypeId, *mut u8, usize, &mut [ComponentTicks])> {
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let (data, len, ticks) = archetype
            .component_storages
            .get_mut(component_id)?
            .raw_rows_and_ticks_mut();
        Some((archetype_id, data, len, ticks))
    }

    /// Return a raw native component column for language bindings.
    ///
    /// The native twin of [`Self::descriptor_component_chunk_mut`]: returns the
    /// contiguous row buffer of a native (Rust-registered) component as raw
    /// bytes, so the C# backend can expose components that an optional module
    /// registered without naming their concrete Rust type. Only native
    /// components are served; descriptor components must use
    /// [`Self::descriptor_component_chunk_mut`] instead.
    /// The returned pointer is only valid for the active managed-system
    /// invocation and must not be retained beyond it.
    pub fn native_component_chunk_mut(
        &mut self,
        component_id: ComponentId,
        chunk_index: usize,
    ) -> Option<(ArchetypeId, *mut u8, usize, usize, &mut [ComponentTicks])> {
        if !component_id.is_native_storage() {
            return None;
        }
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let column = archetype.component_storages.get_mut(component_id)?;
        let element_size = column.elem_size();
        let (data, len, ticks) = column.raw_rows_and_ticks_mut();
        Some((archetype_id, data, len, element_size, ticks))
    }

    /// Return a raw descriptor component column from one already-known archetype.
    ///
    /// The archetype-scoped twin of [`Self::descriptor_component_chunk_mut`].
    pub fn descriptor_component_chunk_in_archetype(
        &mut self,
        component_id: ComponentId,
        archetype_id: ArchetypeId,
    ) -> Option<(ArchetypeId, *mut u8, usize, &mut [ComponentTicks])> {
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        let (data, len, ticks) = archetype
            .component_storages
            .get_mut(component_id)?
            .raw_rows_and_ticks_mut();
        Some((archetype_id, data, len, ticks))
    }

    /// Return a raw native component column from one already-known archetype.
    ///
    /// The archetype-scoped twin of [`Self::native_component_chunk_mut`].
    pub fn native_component_chunk_in_archetype(
        &mut self,
        component_id: ComponentId,
        archetype_id: ArchetypeId,
    ) -> Option<(ArchetypeId, *mut u8, usize, usize, &mut [ComponentTicks])> {
        if !component_id.is_native_storage() {
            return None;
        }
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        if !archetype.component_types.contains(&component_id) {
            return None;
        }
        let column = archetype.component_storages.get_mut(component_id)?;
        let element_size = column.elem_size();
        let (data, len, ticks) = column.raw_rows_and_ticks_mut();
        Some((archetype_id, data, len, element_size, ticks))
    }

    /// Total number of rows every archetype currently stores for one
    /// component id.
    ///
    /// Zero means the component is registered but nothing is using it, which
    /// is what a superseded hot-reload generation looks like once its entities
    /// have migrated away. A non-zero count means the column is still live, so
    /// its registration cannot be treated as stale and discarded.
    pub fn live_row_count(&self, component_id: ComponentId) -> usize {
        self.archetypes
            .values()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .map(|archetype| archetype.entities.len())
            .sum()
    }

    /// Whether any archetype still lists this component id.
    ///
    /// The host's failed-generation cleanup uses this to assert, in debug
    /// builds, that a stranded id is really gone - rows, columns and tables -
    /// before the image that defined it is retired from the graveyard.
    pub fn any_archetype_lists_component(&self, component_id: ComponentId) -> bool {
        self.archetypes
            .values()
            .any(|archetype| archetype.component_types.contains(&component_id))
    }

    /// The first registered component other than `excluding` that claims
    /// `type_name` and still holds rows, with that row count.
    ///
    /// Both collision guards ask the same question: is a same-name
    /// registration a superseded generation or a live peer? Live rows answer
    /// part of it - a peer's rows are the ones that would be lost - but they do
    /// not separate the two cases on their own, because a generation being
    /// retired still holds its rows while its replacement registers. The host
    /// therefore announces the retiring generation's names before init (see
    /// [`Self::supersede_persist_registrations`]); this lookup supplies the
    /// candidate the announcement is judged against.
    pub(crate) fn live_component_with_name(
        &self,
        type_name: &str,
        excluding: ComponentId,
    ) -> Option<(ComponentId, usize)> {
        self.component_registry
            .registered_components()
            .filter(|(id, _, name)| *name == type_name && *id != excluding)
            .map(|(id, _, _)| (id, self.live_row_count(id)))
            .find(|(_, live_rows)| *live_rows > 0)
    }

    /// Every registered component that claims `type_name`, in registration
    /// order.
    ///
    /// Unlike [`Self::live_component_with_name`] this neither requires live
    /// rows nor excludes an id. A rebuilt image usually gets the compiler's id
    /// back for an unchanged type, so a superseding registration's predecessor
    /// is often the entry *with* the incoming id, and older generations can
    /// still be listed under the same name - every candidate has to be
    /// considered, not one of them.
    pub(crate) fn component_ids_with_name(&self, type_name: &str) -> Vec<ComponentId> {
        self.component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .map(|(id, _, _)| id)
            .collect()
    }

    /// Resolve a component ID from its registered type name, without the
    /// persistable-only filter.
    ///
    /// Used by the C# backend to map an optional module's exposed component
    /// name (e.g. `pill_spline::Spline`) to its native [`ComponentId`] so a
    /// byte-level binding can be created without naming the concrete type.
    ///
    /// Unlike the persistable resolver, this has no `persist_inserters` filter
    /// to collapse the candidate set, so a name claimed by two registrations
    /// arrives here with both still visible. It used to break that tie with
    /// `max_by_key(bit)`, which is not a recency ordering - `allocate_bit`
    /// reissues bits reclaimed by `remove` before advancing `next_bit`, so the
    /// highest bit can belong to the older registration. Binding managed code
    /// to the wrong column that way is silent, so an ambiguous name is now
    /// reported as [`WorldError::ComponentNameAmbiguous`] instead.
    ///
    /// A component declared with a shared identity cannot reach this state:
    /// every binary that links it computes the same [`ComponentId`], so there
    /// is only ever one registration to find.
    pub fn resolve_component_id_by_name_any(
        &self,
        type_name: &str,
    ) -> Result<Option<ComponentId>, WorldError> {
        let mut candidates = self
            .component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .map(|(id, _, _)| id);

        let Some(first) = candidates.next() else {
            return Ok(None);
        };
        let extra = candidates.count();
        if extra > 0 {
            return Err(WorldError::ComponentNameAmbiguous {
                type_name: type_name.to_string(),
                count: extra + 1,
            });
        }
        Ok(Some(first))
    }

    /// Read one runtime-defined component as its raw manifest bytes.
    pub fn descriptor_component_bytes(
        &self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Option<&[u8]> {
        let location = self.entity_locations.get(&entity)?;
        self.archetypes
            .get(&location.archetype_id)?
            .component_storages
            .get(component_id)?
            .bytes(location.index_in_archetype)
    }

    /// Get immutable reference to a component on an entity
    ///
    /// Returns None if the entity doesn't exist or doesn't have the component.
    pub fn get_component<T>(&self, entity: Entity) -> Option<&T>
    where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "get component",
            [(
                "Target entity: {:?}, Component type: {}",
                entity,
                std::any::type_name::<T>()
            )]
        );
        // Get component bit for O(1) archetype check
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Get entity location
        let location = self.entity_locations.get(&entity)?;

        // Get archetype
        let archetype = self.archetypes.get(&location.archetype_id)?;

        // Check if archetype has this component type (O(1) bitmask check)
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Get component from storage
        Some(
            archetype
                .component_storages
                .column_of::<T>()
                .get::<T>(location.index_in_archetype),
        )
    }

    /// Get mutable reference to a component on an entity
    ///
    /// Returns None if the entity doesn't exist or doesn't have the component.
    pub fn get_component_mut<T>(&mut self, entity: Entity) -> Option<&mut T>
    where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "get component mut",
            [(
                "Target entity: {:?}, Component type (mutable): {}",
                entity,
                std::any::type_name::<T>()
            )]
        );
        // Get component bit for O(1) archetype check
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Get entity location
        let location = self.entity_locations.get(&entity)?;
        let archetype_id = location.archetype_id;
        let index = location.index_in_archetype;

        // Get archetype
        let archetype = self.archetypes.get_mut(&archetype_id)?;

        // Check if archetype has this component type (O(1) bitmask check)
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Get component from storage
        Some(
            archetype
                .component_storages
                .column_of_mut::<T>()
                .get_mut::<T>(index),
        )
    }

    /// Get raw mutable pointer to a component on an entity
    ///
    /// This is used by ScriptContext to avoid aliasing issues when a script
    /// accesses components of its own type. By returning a raw pointer instead
    /// of `&mut T`, we opt out of Rust's noalias optimization.
    ///
    /// Returns None if the entity doesn't exist or doesn't have the component.
    pub(crate) fn get_component_ptr_mut<T>(&mut self, entity: Entity) -> Option<*mut T>
    where
        T: Component,
    {
        // Get component bit for O(1) archetype check
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Get entity location
        let location = self.entity_locations.get(&entity)?;
        let archetype_id = location.archetype_id;
        let index = location.index_in_archetype;

        // Get archetype
        let archetype = self.archetypes.get_mut(&archetype_id)?;

        // Check if archetype has this component type (O(1) bitmask check)
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Get raw pointer to component - avoids creating intermediate &mut
        let storage = archetype.component_storages.column_of_mut::<T>();
        Some(storage.get_mut::<T>(index) as *mut T)
    }

    /// Remove a component from an entity, moving it to a new archetype.
    ///
    /// If the entity's last component is removed, the entity is destroyed
    /// instead of migrated to an empty archetype.
    ///
    /// # Errors
    ///
    /// Returns [`RemoveComponentError::EntityNotFound`] if the entity does
    /// not exist, and [`RemoveComponentError::ComponentNotFound`] if the
    /// entity does not carry the component `T`.
    pub fn remove_component<T: Component>(
        &mut self,
        entity: Entity,
    ) -> Result<(), RemoveComponentError> {
        self.remove_component_by_id(entity, ComponentId::of::<T>())
    }

    /// Remove a component identified by [`ComponentId`] from one entity.
    ///
    /// This is the non-generic core of [`Self::remove_component`]; the host
    /// uses it when dropping data for a type it can no longer name statically
    /// (a component a reloaded module stopped registering).
    pub(crate) fn remove_component_by_id(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), RemoveComponentError> {
        let _zone =
            crate::profile_scope!("remove component", [("Entity being mutated: {:?}", entity)]);

        // Step 1: Validate - the entity must exist and currently carry the
        // component.
        let location = match self.entity_locations.get(&entity) {
            Some(loc) => *loc,
            None => return Err(RemoveComponentError::EntityNotFound),
        };

        let old_archetype = match self.archetypes.get(&location.archetype_id) {
            Some(arch) => arch,
            None => return Err(RemoveComponentError::EntityNotFound),
        };

        // Check if entity has this component
        if !old_archetype.component_types.contains(&component_id) {
            return Err(RemoveComponentError::ComponentNotFound);
        }

        // Step 2: Build the destination component set without T. If no
        // components remain, destroy the entity instead of migrating it to
        // an empty archetype.
        let new_component_ids: Vec<ComponentId> = old_archetype
            .component_types
            .iter()
            .filter(|&id| *id != component_id)
            .cloned()
            .collect();

        // If no components left, destroy the entity instead.
        // The entity may already be gone; that's fine - we just need to
        // stop trying to migrate it.
        if new_component_ids.is_empty() {
            let _ = self.destroy_entity(entity);
            return Ok(());
        }

        // Step 3: Migrate the entity to the new archetype. Every surviving
        // component is carried by the move itself; the removed one is left
        // behind in the source archetype and released there.
        self.move_entity_to_archetype(entity, new_component_ids, |_| Ok(()))
            .unwrap_or_else(|error| {
                // The entity and its archetype were validated above, so a
                // migration failure here is an internal-invariant break, not a
                // user error. Name the entity and the failure so the report is
                // startable; the descriptor paths propagate the same failure as
                // a typed error instead, because their inputs come from outside
                // the engine.
                panic!("internal invariant broken while migrating entity {entity:?}: {error}")
            });

        Ok(())
    }

    /// Add a component to an existing entity, moving it to a new archetype.
    ///
    /// Existing components are preserved during the migration.
    ///
    /// # Errors
    ///
    /// Returns [`AddComponentError::EntityNotFound`] if the entity does not
    /// exist, and [`AddComponentError::ComponentAlreadyExists`] if the entity
    /// already carries the component `T`.
    pub fn add_component<T>(
        &mut self,
        entity: Entity,
        component: T,
    ) -> Result<(), AddComponentError>
    where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "add component",
            [(
                "Target entity: {:?}, Component type being added: {}",
                entity,
                std::any::type_name::<T>()
            )]
        );
        let component_id = ComponentId::of::<T>();

        // Step 1: Validate - the entity must exist and must not already
        // carry T.
        let location = match self.entity_locations.get(&entity) {
            Some(loc) => *loc,
            None => return Err(AddComponentError::EntityNotFound),
        };

        let old_archetype = match self.archetypes.get(&location.archetype_id) {
            Some(arch) => arch,
            None => return Err(AddComponentError::EntityNotFound),
        };

        // Check if entity already has this component
        if old_archetype.component_types.contains(&component_id) {
            return Err(AddComponentError::ComponentAlreadyExists);
        }

        // Step 2: Build the destination component set with T appended.
        let mut new_component_ids = Vec::with_capacity(old_archetype.component_types.len() + 1);
        new_component_ids.extend_from_slice(&old_archetype.component_types);
        new_component_ids.push(component_id);
        new_component_ids.sort();

        // Step 3: Migrate the entity. Existing components are carried by the
        // move; the only thing left to write is the component being added.
        self.move_entity_to_archetype(entity, new_component_ids, |new_storage| {
            new_storage.column_of_mut::<T>().push::<T>(component);
            Ok(())
        })
        .unwrap_or_else(|error| {
            // As in `remove_component_by_id`: the entity and its archetype
            // were validated above, so this failure is an internal-invariant
            // break, not a user error.
            panic!("internal invariant broken while migrating entity {entity:?}: {error}")
        });

        Ok(())
    }

    /// Add a runtime-defined component and migrate the entity's other columns.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] if the entity does not exist,
    /// [`WorldError::DescriptorComponentAlreadyPresent`] if the entity already
    /// carries the component, [`WorldError::DescriptorComponentNotRegistered`]
    /// if the component was never registered, and
    /// [`WorldError::DescriptorByteLengthMismatch`] if `bytes` does not match
    /// the registered layout size.
    pub(crate) fn add_descriptor_component(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        let Some(old_archetype) = self.archetypes.get(&location.archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: location.archetype_id,
            });
        };
        if old_archetype.component_types.contains(&component_id) {
            return Err(WorldError::DescriptorComponentAlreadyPresent);
        }
        let expected_size = match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => return Err(WorldError::DescriptorComponentNotRegistered { id: component_id }),
        };
        if bytes.len() != expected_size {
            return Err(WorldError::DescriptorByteLengthMismatch { id: component_id });
        }
        let mut new_ids = old_archetype.component_types.clone();
        new_ids.push(component_id);
        new_ids.sort();
        // The migration carries every existing column; the arriving component
        // is the one column the destination has and the source does not, so it
        // is written by the attach step rather than patched in afterwards.
        self.move_entity_to_archetype(entity, new_ids, |new_storage| {
            // The destination archetype was built from `new_ids`, which names
            // this component, so the column is there; the refusal covers a
            // registry/storage desync rather than a reachable state.
            let Some(column) = new_storage.get_mut(component_id) else {
                return Err(WorldError::DescriptorComponentMissing);
            };
            column.push_bytes(bytes)
        })
    }

    /// Replace the bytes of an existing runtime-defined component row.
    pub(crate) fn set_descriptor_component_bytes(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        self.archetypes
            .get_mut(&location.archetype_id)
            .and_then(|archetype| archetype.component_storages.get_mut(component_id))
            .ok_or(WorldError::DescriptorComponentMissing)?
            .set_bytes(location.index_in_archetype, bytes)
    }

    /// Add a zero-initialized runtime-defined component.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorComponentNotRegistered`] if the component
    /// was never registered, plus any error reported by
    /// [`add_descriptor_component`](Self::add_descriptor_component).
    pub fn add_descriptor_component_default(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), WorldError> {
        let size = match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => return Err(WorldError::DescriptorComponentNotRegistered { id: component_id }),
        };
        self.add_descriptor_component(entity, component_id, &vec![0; size])
    }

    /// Remove a runtime-defined component while preserving every other column.
    ///
    /// If the entity's last component is removed, the entity is destroyed
    /// instead of migrated to an empty archetype.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] if the entity does not exist,
    /// and [`WorldError::DescriptorComponentMissing`] if the entity does not
    /// carry the component.
    pub fn remove_descriptor_component(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), WorldError> {
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        let Some(old_archetype) = self.archetypes.get(&location.archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: location.archetype_id,
            });
        };
        if !old_archetype.component_storages.contains(component_id) {
            return Err(WorldError::DescriptorComponentMissing);
        }
        let new_ids: Vec<_> = old_archetype
            .component_types
            .iter()
            .copied()
            .filter(|id| *id != component_id)
            .collect();
        if new_ids.is_empty() {
            let _ = self.destroy_entity(entity);
            return Ok(());
        }
        self.move_entity_to_archetype(entity, new_ids, |_| Ok(()))
    }

    /// Read-only view of the component registry.
    ///
    /// Exposed for the same reason as [`World::archetypes_iter`]: resolving a
    /// component column by stable type name and size rather than by a `TypeId`,
    /// which differs between the host and a hot-loaded DLL.
    #[inline]
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.component_registry
    }
}

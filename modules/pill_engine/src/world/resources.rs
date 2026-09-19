//! Resource storage: registration, claims, foreign values and re-homing.
//!
//! # Responsibilities
//!
//! - Stores one value per resource type, native or foreign, and hands it out
//!   through `Res<T>` and `ResMut<T>`.
//! - Arbitrates the shared names two binaries can both claim, so one type owns
//!   a name and the other binds to it rather than shadowing it.
//! - Re-homes a live resource's function table across a reload and retires the
//!   claims a retiring generation leaves behind.
//!
//! # Design
//!
//! A resource is a singleton, so there is no archetype and no row: the store is
//! a map from id to an erased box that carries a replaceable function table
//! rather than a trait object's vtable. That is the same discipline component
//! columns follow and for the same reason - a retiring artifact's destructor
//! must never be the one that runs.
//!
//! A foreign resource is the same store with no Rust type behind it: bytes plus
//! a declared layout, which is what lets managed code own a resource the host
//! has never heard of.

use super::*;

// =============================================================================
// World - Resource Management
// =============================================================================

impl World {
    /// Number of resources the world holds.
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Insert a resource (singleton data not attached to any entity)
    ///
    /// Resources are global state such as time, input, configuration, etc.
    /// If a resource of this type already exists, it is replaced.
    ///
    /// A shared name claimed by another type is the exception: the claim is
    /// refused, its error recorded for the caller's drain, and **nothing is
    /// stored**. The generation that asked for it is one init away from being
    /// discarded, and storing its value would replace a live resource with a
    /// shape no reader can accept - or, when the layouts happen to agree,
    /// quietly hand one type's bytes to the other.
    pub fn insert_resource<T: Resource>(&mut self, resource: T) {
        let _zone = crate::profile_scope!(
            "insert resource",
            [(
                "Resource type being inserted: {}",
                std::any::type_name::<T>()
            )]
        );
        let id = ResourceId::of::<T>();
        if !self.claim_shared_resource_name::<T>() {
            return;
        }
        let tick = Tick::new(self.change_tick);
        self.note_resource_registration(id);
        self.resources.insert(id, ErasedResource::new(resource));
        self.resource_ticks.insert(id, ComponentTicks::new(tick));
        // Record the table too, so a later reload can re-home this value even
        // if the next generation never inserts it again.
        self.resource_factories
            .insert(id, ErasedResourceOps::of::<T>());
    }

    /// Get immutable reference to a resource
    pub fn get_resource<T: Resource>(&self) -> Option<&T> {
        let _zone = crate::profile_scope!(
            "get resource",
            [(
                "Resource type being accessed (immutable): {}",
                std::any::type_name::<T>()
            )]
        );
        self.resources
            .get(&ResourceId::of::<T>())
            .and_then(ErasedResource::get::<T>)
    }

    /// Get mutable reference to a resource.
    ///
    /// Prefer [`get_resource_mut_tracked`] for system-parameter usage so
    /// that change-detection ticks are automatically bumped on mutation.
    pub fn get_resource_mut<T: Resource>(&mut self) -> Option<&mut T> {
        self.resources
            .get_mut(&ResourceId::of::<T>())
            .and_then(ErasedResource::get_mut::<T>)
    }

    /// Get mutable, change-tracking access to a resource.
    ///
    /// Returns a [`Mut<'_, T>`] that wraps both the resource value and its
    /// [`ComponentTicks`]. Mutating through `DerefMut` automatically bumps
    /// `ticks.changed` to the current world tick, exactly like mutable
    /// component queries do.
    ///
    /// This is used by [`ResMut`](crate::query::ResMut) so that systems
    /// can later detect resource changes via tick inspection.
    ///
    /// # Panics (debug only)
    ///
    /// Panics if this resource was already fetched mutably during the
    /// current frame - indicates a scheduler bug where two systems
    /// obtained concurrent `&mut` access to the same resource.
    pub fn get_resource_mut_tracked<T: Resource>(&mut self) -> Option<Mut<'_, T>> {
        let _zone = crate::profile_scope!(
            "get resource mut tracked",
            [(
                "Resource type being accessed (mutable, tracked): {}",
                std::any::type_name::<T>()
            )]
        );
        let id = ResourceId::of::<T>();
        let value: &mut T = self
            .resources
            .get_mut(&id)
            .and_then(ErasedResource::get_mut::<T>)?;
        let ticks: &mut ComponentTicks = self.resource_ticks.get_mut(&id).unwrap_or_else(|| {
            // `insert_resource` writes `resources` and `resource_ticks`
            // together, so a resource present in one and absent from the
            // other is an internal-invariant break. Naming the resource
            // keeps the diagnosis startable.
            panic!(
                "resource {id:?} exists in resources but has no change-tick \
                     column; resource_ticks has fallen out of sync with resources"
            )
        });
        let this_run = Tick::new(self.change_tick);
        Some(Mut::new(value, ticks, this_run))
    }

    /// Declare that this artifact owns the resource type `T`, without
    /// inserting a value.
    ///
    /// The resource equivalent of `register_component`, and needed for the
    /// same reason: a resource's stored drop function points into the artifact
    /// that inserted the value, and a reloaded generation that wants to keep
    /// the existing value never calls `insert_resource`, so nothing else would
    /// contribute a table pointing at code that is still mapped.
    ///
    /// Call it from a module's `init` for every resource type the module owns.
    /// Idempotent - a reload re-runs `init`, which is the point.
    ///
    /// A refused claim records its error and leaves the stored table alone, so
    /// a generation that disagrees about a name's owner or shape cannot
    /// re-point the live resource at its own code before its init fails.
    pub fn register_resource<T: Resource>(&mut self) {
        self.register_resource_claiming::<T>();
    }

    /// [`Self::register_resource`], reporting whether the claim was accepted.
    ///
    /// The public call swallows a refusal because its callers have nothing to
    /// do with one - the error is recorded and the init that raised it fails.
    /// The persistable registration does have something to do with it: a
    /// refused generation must not go on to install a serializer that reads the
    /// live value through code that is one init away from being discarded.
    pub(crate) fn register_resource_claiming<T: Resource>(&mut self) -> bool {
        let id = ResourceId::of::<T>();
        if !self.claim_shared_resource_name::<T>() {
            return false;
        }
        self.resource_factories
            .insert(id, ErasedResourceOps::of::<T>());
        self.note_resource_registration(id);
        true
    }

    /// Declare a resource defined by another language, without storing a value.
    ///
    /// The foreign-language counterpart of [`Self::register_resource`], for a
    /// resource type this process has no Rust definition of. Identity is the
    /// declared name, hashed exactly as [`Resource::shared_name`] hashes a Rust
    /// type's, so a managed declaration and a Rust type that write down the same
    /// string are one resource - the scheduler included, because resource access
    /// sets are id sets.
    ///
    /// The payload is blittable bytes: `size` and `align` describe it, and
    /// [`Self::insert_foreign_resource_bytes`] stores it. Nothing here runs a
    /// destructor, so a foreign value outlives whatever assembly produced it.
    ///
    /// `declaring_type` is the declaring language's own name for the type (the
    /// managed full name, for the C# path). It is recorded, and compared only
    /// against another Rust declaration - see [`Self::claim_shared_resource`].
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceNameEmpty`] for an empty name,
    /// [`WorldError::ForeignResourceLayoutInvalid`] for a layout that cannot
    /// describe an allocation, [`WorldError::SharedResourceNameConflict`] when
    /// two Rust types disagree about the name,
    /// [`WorldError::SharedResourceLayoutMismatch`] when the declared layout
    /// disagrees with the one already claimed, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is
    /// stored under the id.
    pub fn register_foreign_resource(
        &mut self,
        name: &str,
        declaring_type: &str,
        size: usize,
        align: usize,
        schema_hash: u64,
    ) -> Result<ResourceId, WorldError> {
        if name.is_empty() {
            return Err(WorldError::ForeignResourceNameEmpty);
        }
        if !is_valid_foreign_layout(size, align) {
            return Err(WorldError::ForeignResourceLayoutInvalid { size, align });
        }
        let id = ResourceId::Shared(crate::component::shared_component_identity(name));
        if let Some(error) = self.claim_shared_resource(
            id,
            name,
            declaring_type,
            size,
            align,
            true,
            Some(schema_hash),
        ) {
            return Err(error);
        }
        // A stored Rust value fixes what is under the id - its destructor is
        // bound to its type - so a foreign declaration cannot take the id over.
        // `relayout_foreign_resource` refuses the same state; refusing here too
        // means the disagreement never reaches `rehome_resources`.
        if self
            .resources
            .get(&id)
            .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id });
        }
        self.resource_factories
            .insert(id, ErasedResourceOps::foreign(size, align, schema_hash));
        self.note_resource_registration(id);
        Ok(id)
    }

    /// Store a value for a shared resource, from another language's bytes.
    ///
    /// Replaces a foreign payload under the id; the change tick is stamped,
    /// because a value appearing is a change. A Rust value is never replaced:
    /// its destructor is bound to its type, so admitting foreign bytes would
    /// drop it through code that never allocated it. That refusal is
    /// [`WorldError::ForeignResourceFactoryIsNative`].
    ///
    /// The id has to name a registered *shared* resource, one whose identity is
    /// a declared name. A Rust type's private resource is identified by its
    /// `TypeId`, which no other language can name, so there is nothing here to
    /// address.
    ///
    /// # Errors
    ///
    /// [`WorldError::SharedResourceNotRegistered`] when the id is not a
    /// registered shared resource, [`WorldError::ForeignResourceFactoryIsNative`]
    /// when the id's table is a Rust type's, and
    /// [`WorldError::ForeignResourceBytesMismatch`] when the payload is not
    /// exactly the registered size.
    pub fn insert_foreign_resource_bytes(
        &mut self,
        id: ResourceId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        let Some(ops) = self.shared_resource_ops(id) else {
            return Err(WorldError::SharedResourceNotRegistered { id });
        };
        if !ops.foreign {
            return Err(WorldError::ForeignResourceFactoryIsNative { id });
        }
        if bytes.len() != ops.size {
            return Err(WorldError::ForeignResourceBytesMismatch {
                id,
                expected: ops.size,
                actual: bytes.len(),
            });
        }
        let tick = Tick::new(self.change_tick);
        self.resources.insert(
            id,
            ErasedResource::new_foreign(bytes, ops.align, ops.schema_hash.unwrap_or(0)),
        );
        self.resource_ticks.insert(id, ComponentTicks::new(tick));
        self.note_resource_registration(id);
        Ok(())
    }

    /// The stored bytes of a shared resource, for another language to read.
    ///
    /// `None` when the id is not a registered shared resource, when nothing is
    /// stored under it yet, or when the stored value is a Rust one: bytes are
    /// for foreign payloads, whose layout is the whole of what they are.
    pub fn foreign_resource_bytes(&self, id: ResourceId) -> Option<&[u8]> {
        self.foreign_resource_ops(id)?;
        self.resources
            .get(&id)
            .filter(|resource| resource.is_foreign())
            .map(ErasedResource::bytes)
    }

    /// The stored bytes of a shared resource, mutably, with its ticks.
    ///
    /// The `changed` tick moves as the view is handed out rather than on the
    /// first write: a caller holding the bytes has no wrapper that could notice
    /// a mutation, so the conservative reading is that the value changed when
    /// the borrow was taken.
    ///
    /// A caller writing here is responsible for writing a value the stored type
    /// accepts, exactly as a generated mirror is for the component rows it
    /// writes in place.
    ///
    /// A Rust value refuses, as it does for [`Self::foreign_resource_bytes`],
    /// and the refusal costs nothing: the `changed` tick is not stamped for a
    /// view that is never handed out.
    pub fn foreign_resource_bytes_mut(
        &mut self,
        id: ResourceId,
    ) -> Option<(&mut [u8], &mut ComponentTicks)> {
        self.foreign_resource_ops(id)?;
        if !self.resources.get(&id)?.is_foreign() {
            return None;
        }
        let changed = Tick::new(self.change_tick);
        let ticks = self.resource_ticks.get_mut(&id)?;
        ticks.set_changed(changed);
        let bytes = self.resources.get_mut(&id)?.bytes_mut();
        Some((bytes, ticks))
    }

    /// The declared layout of a foreign resource: size, alignment, schema hash.
    ///
    /// What a host compares against the next generation's manifest before it
    /// decides whether a reload needs a migration at all.
    pub fn foreign_resource_layout(&self, id: ResourceId) -> Option<(usize, usize, u64)> {
        let ops = self.resource_factories.get(&id).copied()?;
        if !ops.foreign {
            return None;
        }
        Some((ops.size, ops.align, ops.schema_hash?))
    }

    /// Replace a foreign resource's declared layout, migrating its value.
    ///
    /// The resource twin of [`Self::relayout_descriptor_component`], and much
    /// smaller for the same reason a resource is smaller than a column: one
    /// value, no archetypes, no rows to keep in step. `plan` comes from the same
    /// two field lists the component path uses
    /// ([`FieldPlan::between`]), and anything it does not cover is left
    /// zero.
    ///
    /// Only a foreign payload is rewritten. A Rust value stored under the
    /// declaration refuses: its shape *is* its type, so a declaration that
    /// disagrees with it is a conflict to report rather than a migration to
    /// perform.
    ///
    /// Returns 1 when a value was rewritten and 0 when only the declaration
    /// moved, which is the case for a resource nobody has stored yet.
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceNotRegistered`] when the id is not a
    /// registered foreign declaration,
    /// [`WorldError::ForeignResourceLayoutInvalid`] for a layout that cannot
    /// describe an allocation,
    /// [`WorldError::ForeignResourcePlanOutOfBounds`] when the plan does not fit
    /// the old or the new payload, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is stored
    /// under the id.
    pub fn relayout_foreign_resource(
        &mut self,
        id: ResourceId,
        size: usize,
        align: usize,
        schema_hash: u64,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        if !self
            .resource_factories
            .get(&id)
            .is_some_and(|ops| ops.foreign)
        {
            return Err(WorldError::ForeignResourceNotRegistered { id });
        }
        if !is_valid_foreign_layout(size, align) {
            return Err(WorldError::ForeignResourceLayoutInvalid { size, align });
        }
        if self
            .resources
            .get(&id)
            .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id });
        }

        let next = ErasedResourceOps::foreign(size, align, schema_hash);
        // The claim is the second record of this resource's shape, and the one
        // the next declaration is checked against, so it moves with the
        // factory on both paths. Left behind, it made the migrated shape
        // unre-declarable and let a stale declaration revert the factory.
        if let Some(claim) = self.shared_resource_claims.get_mut(&id) {
            claim.size = size;
            claim.align = align;
            claim.schema_hash = Some(schema_hash);
        }
        let Some(value) = self.resources.get_mut(&id) else {
            self.resource_factories.insert(id, next);
            self.resource_field_layouts.remove(&id);
            return Ok(0);
        };
        value
            .migrate_bytes(size, align, plan)
            .map_err(|_error| WorldError::ForeignResourcePlanOutOfBounds { id })?;
        self.resource_factories.insert(id, next);
        // The stored layout described the shape that just moved, so it is
        // dropped rather than left to be served over the migrated bytes. The
        // declarer re-registers it, exactly as it does for a component.
        self.resource_field_layouts.remove(&id);
        Ok(1)
    }

    /// Move a foreign resource's value onto another id and retire the source
    /// declaration.
    ///
    /// The resource twin of [`Self::remap_descriptor_component`] and the
    /// rename half of the managed manifest's resource story: a successor
    /// declaration derives its id from its own name, so the stored value is
    /// reshaped through `plan` - measured from the source's field list to the
    /// successor's - written under the successor, and the source is then
    /// dropped. Any payload already under the successor is replaced; a fresh
    /// declaration's zero seed is the expected case, and the moved value is
    /// the only meaningful one there.
    ///
    /// The source's claims move to the successor wholesale, so the source's
    /// value is released here only when no other subject still declares the
    /// old name, and the successor ends up protected exactly as the source
    /// was.
    ///
    /// Returns 1 when a value was moved and 0 when the source had none stored,
    /// which is the case for a resource nobody has written yet.
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceRemapSelf`] when both ids are equal,
    /// [`WorldError::ForeignResourceNotRegistered`] when either id is not a
    /// registered foreign declaration, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is
    /// stored under either id.
    pub fn remap_foreign_resource(
        &mut self,
        old_id: ResourceId,
        new_id: ResourceId,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        if old_id == new_id {
            return Err(WorldError::ForeignResourceRemapSelf);
        }
        let Some((old_size, _, _)) = self.foreign_resource_layout(old_id) else {
            return Err(WorldError::ForeignResourceNotRegistered { id: old_id });
        };
        let Some((new_size, _, _)) = self.foreign_resource_layout(new_id) else {
            return Err(WorldError::ForeignResourceNotRegistered { id: new_id });
        };
        if self
            .resources
            .get(&old_id)
            .is_some_and(|value| !value.is_foreign())
            || self
                .resources
                .get(&new_id)
                .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id: old_id });
        }

        // Step 1: Read and reshape the stored value before anything moves.
        // `FieldPlan::validate` speaks descriptor rows, so the bounds are
        // checked here and reported with the resource's own variant.
        let mut moved = 0;
        if let Some(old_bytes) = self.foreign_resource_bytes(old_id).map(<[u8]>::to_vec) {
            let mut new_bytes = vec![0_u8; new_size];
            for planned in plan.fields() {
                let FieldSource::OldOffset(source_offset) = planned.source else {
                    continue;
                };
                if planned.offset + planned.bytes > new_size
                    || source_offset + planned.bytes > old_size
                {
                    return Err(WorldError::ForeignResourcePlanOutOfBounds { id: old_id });
                }
                new_bytes[planned.offset..planned.offset + planned.bytes]
                    .copy_from_slice(&old_bytes[source_offset..source_offset + planned.bytes]);
            }
            // Step 2: Write it under the successor. The insert stamps the
            // change tick, so a reader observes the move as the change it is.
            self.insert_foreign_resource_bytes(new_id, &new_bytes)?;
            moved = 1;
        }

        // Step 3: Move the claims and drop the source. The claims are taken
        // wholesale rather than released one by one: whoever declared the old
        // name still needs the resource under its new one.
        let claims = self.resource_claim_counts.remove(&old_id).unwrap_or(0);
        if claims > 0 {
            *self.resource_claim_counts.entry(new_id).or_insert(0) += claims;
        }
        self.drop_resources(&[old_id]);
        Ok(moved)
    }

    /// The per-type table of a registered shared resource, if the id names one.
    ///
    /// Shared identity is the gate: a private resource's id is a `TypeId`, so
    /// no other language can name it and no byte view of it exists to hand out.
    /// The `?` consumes the identity itself - absent means private.
    fn shared_resource_ops(&self, id: ResourceId) -> Option<ErasedResourceOps> {
        id.shared_identity()?;
        self.resource_factories.get(&id).copied()
    }

    /// The per-type table of a registered shared resource, when that table
    /// belongs to foreign bytes.
    ///
    /// The byte accessors go through this one. `shared_resource_ops` answers
    /// "is this a shared id at all", which the foreign insert needs in order to
    /// tell a private id from one whose table is a Rust type's; a byte view, by
    /// contrast, is only meaningful for a payload with no Rust type anywhere,
    /// so both the table and the box have to say so.
    fn foreign_resource_ops(&self, id: ResourceId) -> Option<ErasedResourceOps> {
        self.shared_resource_ops(id).filter(|ops| ops.foreign)
    }

    /// Stamp one resource registration, keeping one entry per id.
    ///
    /// The stamp is the id's most recent registration, not its first. The host
    /// diffs [`Self::resource_ids_registered_since`] after an artifact's `init`
    /// to decide which resources that generation still claims, and a
    /// generation re-claims its resources by re-registering them - the
    /// documented replace path of `insert_resource`. Keeping only the first
    /// stamp made every reload read its own resources as retired and drop them
    /// out from under the systems that were still running. The map stays
    /// proportional to distinct resources, and the diff reports each id once.
    fn note_resource_registration(&mut self, id: ResourceId) {
        self.resource_registration_stamps
            .insert(id, self.resource_registration_sequence);
        self.resource_registration_sequence = self.resource_registration_sequence.wrapping_add(1);
    }

    /// Sequence marker to capture before an artifact's `init`, so the resource
    /// types that `init` registers can be enumerated afterwards.
    ///
    /// Mirrors [`Self::component_registration_sequence`].
    pub fn resource_registration_sequence(&self) -> u64 {
        self.resource_registration_sequence
    }

    /// Resource ids registered at or after `sequence`.
    ///
    /// The comparison is `>=`: a registration reads the current value and then
    /// increments it, so one happening immediately after the capture carries
    /// exactly the captured value.
    pub fn resource_ids_registered_since(&self, sequence: u64) -> Vec<ResourceId> {
        let mut ids: Vec<ResourceId> = self
            .resource_registration_stamps
            .iter()
            .filter(|(_, stamped)| **stamped >= sequence)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Names declared by the shared resources this world holds a claim for,
    /// sorted and deduplicated.
    ///
    /// The resource counterpart of the component registry's shared-identity
    /// list, and what the ECS report prints. A shared resource's whole point is
    /// that no `TypeId` names it, so the declared name is all there is to
    /// report.
    #[must_use]
    pub fn shared_resource_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .shared_resource_claims
            .values()
            .map(|claim| claim.shared_name.clone())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Check and record which type owns a shared resource name.
    ///
    /// Returns whether the caller may go on to register: `true` for an ordinary
    /// resource, whose id *is* its `TypeId`, so a second claim on one id is
    /// necessarily the same type, and for a shared claim that is accepted;
    /// `false` when the claim was refused and its error recorded.
    ///
    /// A refusal has to stop the caller from touching what is stored, not just
    /// fail the init later: the reload transaction rolls the generation back by
    /// re-running the previous `init`, which re-registers what it owned rather
    /// than restoring values, so a value written under a refused claim would
    /// outlive the generation that wrote it - in a shape nothing can read.
    ///
    /// For a shared resource the id is derived from a written-down name, so
    /// two unrelated types can reach it. The discriminator is the type's own
    /// name - two copies of one type agree on it, two different types do not -
    /// and only the final path segment is compared, because the module path
    /// differs between artifacts while the type's name does not. This is the
    /// same rule `ComponentRegistry` applies to shared components.
    fn claim_shared_resource_name<T: Resource>(&mut self) -> bool {
        let Some(shared_name) = T::shared_name() else {
            return true;
        };
        match self.claim_shared_resource(
            ResourceId::of::<T>(),
            shared_name,
            crate::component::ComponentRegistry::declaring_type_name::<T>(),
            std::mem::size_of::<T>(),
            std::mem::align_of::<T>(),
            false,
            <T as Resource>::shared_schema_hash(),
        ) {
            None => true,
            Some(error) => {
                // Recorded here rather than in the core: the Rust path is the
                // one whose refusal has to fail the init that raised it. A
                // foreign declaration's caller gets the error back and acts on
                // it itself.
                self.record_registration_error(error);
                false
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn claim_shared_resource(
        &mut self,
        id: ResourceId,
        shared_name: &str,
        declaring_type: &str,
        size: usize,
        align: usize,
        foreign: bool,
        schema_hash: Option<u64>,
    ) -> Option<WorldError> {
        let incoming = SharedResourceClaim {
            shared_name: shared_name.to_string(),
            declaring_type: declaring_type.to_string(),
            foreign,
            size,
            align,
            schema_hash,
        };
        if let Some(existing) = self.shared_resource_claims.get(&id) {
            // The id is a hash of the name, so two different names sharing it
            // are an identity collision: one slot would answer to both. The
            // recorded string is what tells the two apart.
            if existing.shared_name != shared_name {
                let error = WorldError::SharedResourceIdentityCollision {
                    shared_name: shared_name.to_string(),
                    existing_name: existing.shared_name.clone(),
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_name = %existing.shared_name,
                    error = %error,
                    "two shared resource names hash to one identity"
                );
                return Some(error);
            }
            if !existing.foreign
                && !incoming.foreign
                && existing.declaring_type != incoming.declaring_type
            {
                // Logged as well as returned, exactly as `register_component`
                // logs its own failures: the log line is what names the cause
                // to whoever reads the host output.
                let error = WorldError::SharedResourceNameConflict {
                    shared_name: shared_name.to_string(),
                    existing_type: existing.declaring_type.clone(),
                    incoming_type: incoming.declaring_type.clone(),
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_type = %existing.declaring_type,
                    incoming_type = %incoming.declaring_type,
                    error = %error,
                    "shared resource name claimed by two different types"
                );
                return Some(error);
            }
            if existing.size != incoming.size || existing.align != incoming.align {
                let error = WorldError::SharedResourceLayoutMismatch {
                    shared_name: shared_name.to_string(),
                    existing_size: existing.size,
                    existing_align: existing.align,
                    incoming_size: incoming.size,
                    incoming_align: incoming.align,
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_size = existing.size,
                    existing_align = existing.align,
                    incoming_size = incoming.size,
                    incoming_align = incoming.align,
                    error = %error,
                    "shared resource registered with two different layouts"
                );
                return Some(error);
            }
            // Both sides carrying a hash means both declared a field shape, and
            // equal size/align is exactly the case a hash catches: {u32, u32}
            // and {f32, f32} agree on layout and not on fields.
            if let (Some(existing_hash), Some(incoming_hash)) =
                (existing.schema_hash, incoming.schema_hash)
            {
                if existing_hash != incoming_hash {
                    let error = WorldError::SharedResourceSchemaMismatch {
                        shared_name: shared_name.to_string(),
                        existing_hash,
                        incoming_hash,
                    };
                    error!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        shared_name = %shared_name,
                        error = %error,
                        "shared resource registered with two different field shapes"
                    );
                    return Some(error);
                }
            }
        }
        match self.shared_resource_claims.get(&id) {
            None => {
                self.shared_resource_claims.insert(id, incoming);
            }
            // A foreign declaration does not overwrite the record: keeping a
            // Rust declarer's name is what lets a later Rust arrival still be
            // compared against one. A Rust arrival to a foreign claim does take
            // it, because from then on there is a Rust name worth comparing.
            Some(existing) if existing.foreign && !incoming.foreign => {
                self.shared_resource_claims.insert(id, incoming);
            }
            Some(_) => {}
        }
        None
    }

    /// Re-home every stored resource's per-type function table.
    ///
    /// The twin of [`Self::rehome_native_columns`], called at the same point
    /// in the reload transaction and for the same reason: a resource holds a
    /// drop function belonging to whichever artifact inserted it, and that
    /// artifact's image is about to be evicted from the reload graveyard.
    /// Refreshing from the latest registered table, while every image is still
    /// mapped, keeps the drop valid afterwards.
    ///
    /// A resource whose type no artifact has registered in this generation is
    /// left alone - there is nothing newer to point it at. That case is a
    /// retired owner's resource, and [`Self::drop_resources`] is what releases
    /// it, while the retiring image is still mapped.
    ///
    /// A foreign box is left alone as well, for a different reason: its table
    /// drops nothing and points at this crate's code, which outlives every
    /// artifact. Refreshing it from the factories would let a Rust type that
    /// shares the name hand its own destructor to bytes it does not own.
    ///
    /// A table that claims foreign ownership over a Rust value is left alone
    /// too. Registration paths refuse that state, but a stale table could in
    /// principle reach it, and the table would then drop nothing: the value
    /// would leak silently. Keeping its own destructor is the lesser cost, and
    /// the disagreement is reported rather than swallowed.
    pub fn rehome_resources(&mut self) {
        for (id, resource) in &mut self.resources {
            if resource.is_foreign() {
                continue;
            }
            if let Some(&ops) = self.resource_factories.get(id) {
                if ops.foreign {
                    error!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        resource = ?id,
                        "a Rust resource is declared foreign; keeping its own drop"
                    );
                    continue;
                }
                resource.refresh_ops(ops);
            }
        }
    }

    /// Drop the named resources, releasing their values.
    ///
    /// The resource twin of
    /// [`drop_forgotten_components`](Self::drop_forgotten_components), and it
    /// carries the same timing requirement: **call it while the artifact that
    /// inserted the value is still mapped.** A resource holds a drop function
    /// belonging to that artifact, so dropping it after the image is evicted
    /// calls through a dangling pointer.
    ///
    /// The host uses it when a reloaded module stops registering a resource
    /// type it previously owned - compare
    /// [`resource_ids_registered_since`](Self::resource_ids_registered_since)
    /// across the two generations to find those ids.
    ///
    /// An id another subject still claims through
    /// [`retain_resource_claims`](Self::retain_resource_claims) is skipped: the
    /// value's drop function belongs to the generation that inserted it, and
    /// the subject retiring now is not necessarily that one.
    ///
    /// Returns how many were actually present and dropped.
    pub fn drop_resources(&mut self, ids: &[ResourceId]) -> usize {
        let mut dropped = 0;
        for id in ids {
            // Another subject's claim keeps the value alive.
            if self
                .resource_claim_counts
                .get(id)
                .is_some_and(|count| *count > 0)
            {
                continue;
            }
            if self.resources.remove(id).is_some() {
                dropped += 1;
            }
            self.resource_ticks.remove(id);
            self.resource_factories.remove(id);
            self.resource_field_layouts.remove(id);
            self.shared_resource_claims.remove(id);
            // The id is unregistered now, so a later registration is a fresh
            // one and has to be visible to the next "registered since" diff.
            self.resource_registration_stamps.remove(id);
        }
        dropped
    }

    /// Record one claim per id on a resource this subject registered.
    ///
    /// The count is what [`drop_resources`](Self::drop_resources) consults
    /// before releasing an id one subject retired: a shared name can be
    /// registered from several subjects, and only the last of them to let go
    /// should destroy the value.
    pub fn retain_resource_claims(&mut self, ids: &[ResourceId]) {
        for id in ids {
            *self.resource_claim_counts.entry(*id).or_insert(0) += 1;
        }
    }

    /// Drop one claim per id, releasing a value once its last claim goes.
    ///
    /// A claim an id never had is a no-op, and a release that takes the count
    /// to zero removes the entry, so a later `retain` starts a fresh count.
    pub fn release_resource_claims(&mut self, ids: &[ResourceId]) {
        for id in ids {
            if let Some(count) = self.resource_claim_counts.get_mut(id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.resource_claim_counts.remove(id);
                }
            }
        }
    }

    /// Remove a resource and return it if it existed.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::SharedResourceHoldsAnotherType`] when the id holds
    /// a value this `T` cannot be read as - the case a shared, name-derived id
    /// makes reachable. The take is attempted first and the box is put back on
    /// refusal, so nothing about the stored resource is touched: not its value,
    /// its ticks, its factory or its claim.
    pub fn remove_resource<T: Resource>(&mut self) -> Result<Option<T>, WorldError> {
        let id = ResourceId::of::<T>();
        let Some(erased) = self.resources.remove(&id) else {
            return Ok(None);
        };
        let value = match erased.take::<T>() {
            Ok(value) => value,
            Err(erased) => {
                // The comment above used to promise this and the code did the
                // opposite: `and_then(..ok())` dropped the returned box, taking
                // the value, its ticks, its factory and its claim with it.
                self.resources.insert(id, erased);
                return Err(WorldError::SharedResourceHoldsAnotherType {
                    id,
                    requested_type: std::any::type_name::<T>(),
                });
            }
        };
        self.resource_ticks.remove(&id);
        self.resource_factories.remove(&id);
        self.shared_resource_claims.remove(&id);
        self.resource_registration_stamps.remove(&id);
        Ok(Some(value))
    }

    /// Check whether a resource exists *and* can be read as `T`.
    ///
    /// The same question [`Self::get_resource`] answers: for a shared id a
    /// differently-shaped value under the name is not a `T`, so existence and
    /// readability agree instead of one reporting the id and the other the
    /// readable shape. [`Self::resource_holder`] answers the shape question for
    /// a caller that needs to know what is stored.
    #[must_use]
    pub fn has_resource<T: Resource>(&self) -> bool {
        self.resources
            .get(&ResourceId::of::<T>())
            .is_some_and(|erased| erased.holds::<T>())
    }

    /// The stored box under `T`'s resource id, whatever shape it holds.
    ///
    /// For a caller whose id exists but whose type does not read: the box
    /// answers `holds::<T>()` and `has_shared_identity()`. `None` means the id
    /// has no resource at all.
    #[must_use]
    pub fn resource_holder<T: Resource>(&self) -> Option<&crate::resource::ErasedResource> {
        self.resources.get(&ResourceId::of::<T>())
    }

    /// Debug-only: Clear the set of mutably-borrowed resources.
    ///
    /// Called by [`Engine::process_frame`] at the start of every frame so
    /// that the isolation check only guards against concurrent access
    /// within a single frame.
    /// Take one resource's debug write lock, reporting an overlapping holder.
    ///
    /// Acquired when a [`ResMut`](crate::query::ResMut) is built and released
    /// when it drops, so the lock spans exactly one system's access. Doing it
    /// per `get_mut()` call instead meant a system that took the resource
    /// twice in sequence tripped the assertion, and holding it to the end of
    /// the frame meant the second of two systems writing one resource did -
    /// both while the message blamed concurrency that was not happening.
    ///
    /// `&self`, not `&mut self`: the systems of a parallel batch hold aliasing
    /// `&mut World`, so this must be callable through a shared reference.
    #[cfg(debug_assertions)]
    pub(crate) fn debug_acquire_resource_lock(&self, id: ResourceId) {
        let newly_inserted = self.debug_resource_write_locks.lock().insert(id);
        debug_assert!(
            newly_inserted,
            "Resource {id:?} is already mutably borrowed by another live \
             `ResMut` - possible scheduler bug or concurrent system access"
        );
    }

    /// Release one resource's debug write lock; see
    /// [`Self::debug_acquire_resource_lock`].
    #[cfg(debug_assertions)]
    pub(crate) fn debug_release_resource_lock(&self, id: ResourceId) {
        self.debug_resource_write_locks.lock().remove(&id);
    }

    #[cfg(debug_assertions)]
    pub(crate) fn debug_clear_resource_locks(&mut self) {
        self.debug_resource_write_locks.lock().clear();
    }
}

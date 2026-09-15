//! Archetype-based component storage with SoA layout.
//!
//! An archetype is a unique combination of component types. All entities with
//! the same set of components are stored together in the same archetype for
//! cache-friendly iteration.
//!
//! # Responsibilities
//!
//! - Group entities by their exact component set into [`Archetype`] instances.
//! - Own the contiguous, type-erased component storage for each archetype,
//!   both native (`ComponentColumns`) and dynamically laid out
//!   (`DynamicColumn`).
//! - Track change-detection [`ComponentTicks`] for every component instance.
//!
//! # Design
//!
//! Archetypes use a Structure of Arrays (SoA) layout rather than an
//! Array of Structures (AoS). This means components of the same type are
//! stored contiguously in memory:
//!
//! ```text
//! Archetype [Position, Velocity]
//! ┌─────────────────────────────────────────────────┐
//! │ Entities:    [E1,     E2,     E3,     E4    ]   │
//! │ Positions:   [Pos1,   Pos2,   Pos3,   Pos4  ]   │
//! │ Velocities:  [Vel1,   Vel2,   Vel3,   Vel4  ]   │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! When iterating over all entities with Position+Velocity:
//! - SoA (this design): Sequential memory access, excellent cache utilization
//! - AoS alternative: Scattered access, poor cache performance
//!
//! The tradeoff is that accessing all components of a single entity requires
//! multiple array lookups, but this is rare compared to bulk iteration
//! in ECS-style approaches.
//!
//! Entity removal uses swap-remove to keep arrays dense: the removed entity
//! is swapped with the last entity in each component array, the last element
//! (now the removed entity's data) is popped, and the swapped entity's
//! location is updated in the `entity_locations` map. This keeps arrays
//! dense without gaps, maintaining O(1) removal.

// Standard library
use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::collections::HashMap;
use std::ptr::NonNull;

// External crates
use trait_type_map::{ErasedVecStorage, ErasedVecStorageInfo};

// Current crate
use crate::component::{Component, ComponentId, ComponentMask, ComponentTicks};
use crate::entity::Entity;
use crate::error::WorldError;

// =============================================================================
// StorageFactory
// =============================================================================

/// Strategy for creating component storage for a specific component type.
///
/// Registered per [`ComponentId`] and consulted when an [`Archetype`] is
/// created so the archetype can allocate storage without knowing the
/// concrete component types.
pub enum StorageFactory {
    /// Creates a type-erased native storage column inside the archetype's
    /// [`ComponentColumns`].
    ///
    /// Carries only data (type id, layout, per-type function table), never a
    /// closure: the column is stored as a concrete `Box<ErasedVecStorage>`
    /// with no trait-object vtable, so it survives module unloads; the engine
    /// refreshes its function table on every reload.
    Native(ErasedVecStorageInfo<dyn Component>),
    /// Carries the runtime layout of a component owned by another language.
    Dynamic(DynamicComponentLayout),
}

// =============================================================================
// DynamicComponentLayout
// =============================================================================

/// Runtime layout for a component whose concrete type is owned by another language.
///
/// Describes the memory footprint of an opaque component column so its rows
/// can be copied in and out as raw bytes without knowing the concrete type.
#[derive(Debug, Clone)]
pub struct DynamicComponentLayout {
    /// Size in bytes of a single component instance.
    pub size: usize,
    /// Alignment in bytes required by a single component instance.
    pub align: usize,
    /// Hash identifying the component's schema across language boundaries.
    pub schema_hash: u64,
}

/// Check that a size and alignment can describe dynamic storage.
///
/// Shared by registration and relayout so the two can never disagree about what
/// a usable layout is. The errors name the offending layout rather than the
/// caller, because both callers hand the same three facts to the same engine.
pub(crate) fn validate_dynamic_layout(size: usize, align: usize) -> Result<(), WorldError> {
    if size == 0 {
        return Err(WorldError::DynamicSizeZero);
    }
    if align == 0 || !align.is_power_of_two() {
        return Err(WorldError::DynamicAlignmentInvalid);
    }
    if Layout::from_size_align(size, align).is_err() {
        return Err(WorldError::DynamicLayoutInvalid);
    }
    Ok(())
}

// =============================================================================
// DynamicFieldPlan
// =============================================================================

/// One top-level field of a layout, as a migration plan sees it.
///
/// Only the top level, by design: a nested struct field moves as a single
/// block, so a change inside it cannot silently reinterpret its members. The
/// fields a plan maps are exactly the names the two layouts agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutField<'a> {
    /// Field name. Matching is by name, never by position.
    pub name: &'a str,
    /// Byte offset of the field inside a row.
    pub offset: usize,
    /// Byte size of the field.
    pub size: usize,
}

/// Where one field of a new layout takes its bytes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSource {
    /// Copy from this byte offset in the old row.
    OldOffset(usize),
    /// Leave the field zeroed. The relayout zeroes every row before it applies
    /// the plan, so a field with no source is defined rather than stale.
    ZeroFill,
}

/// One instruction in a migration plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedField {
    /// Byte offset of the field in the new row.
    pub offset: usize,
    /// Number of bytes written at `offset`.
    pub bytes: usize,
    /// Where those bytes come from.
    pub source: FieldSource,
}

/// A byte-level plan for moving rows from one layout of a component to another.
///
/// Data, not code. The caller computes it from the two field lists it already
/// has - the managed manifest carries field names, offsets and sizes, so the
/// C# path builds one per changed component - and the engine applies it to every
/// stored row. Anything the plan does not cover is left zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicFieldPlan {
    /// The instructions, in the order they are applied.
    fields: Vec<PlannedField>,
}

impl DynamicFieldPlan {
    /// An empty plan: every byte of every new row is zero.
    #[must_use]
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// Append one instruction.
    pub fn push(&mut self, offset: usize, bytes: usize, source: FieldSource) {
        self.fields.push(PlannedField {
            offset,
            bytes,
            source,
        });
    }

    /// Build a plan from the top-level fields of the old and new layout.
    ///
    /// Fields are matched **by name**, so a reorder keeps every value with its
    /// own name. A field only the new layout has becomes a zero fill; a field
    /// only the old layout had is left out, which is how a removed field's
    /// bytes leave the rows. A name repeated in one list matches its first
    /// occurrence and the rest are ignored, which keeps the plan deterministic
    /// for a malformed input rather than depending on iteration order.
    ///
    /// A matched pair whose sizes disagree copies the smaller size and leaves
    /// the remainder zeroed, so a field that grew keeps its bytes and gains a
    /// defined tail instead of reading past the old row.
    #[must_use]
    pub fn between(old: &[LayoutField<'_>], new: &[LayoutField<'_>]) -> Self {
        let mut plan = Self::new();
        for new_field in new {
            match old
                .iter()
                .find(|old_field| old_field.name == new_field.name)
            {
                Some(old_field) => plan.push(
                    new_field.offset,
                    old_field.size.min(new_field.size),
                    FieldSource::OldOffset(old_field.offset),
                ),
                None => plan.push(new_field.offset, new_field.size, FieldSource::ZeroFill),
            }
        }
        plan
    }

    /// The instructions, in application order.
    #[must_use]
    pub fn fields(&self) -> &[PlannedField] {
        &self.fields
    }

    /// Whether the plan carries no instructions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Check every instruction against the two row sizes.
    ///
    /// Callers run this before touching a row, so a plan that does not fit is
    /// refused while the storage is still untouched. The error is
    /// [`WorldError::DynamicRowInvalid`]: the plan is layout-level data and
    /// carries no component id for a richer message.
    pub fn validate(&self, old_size: usize, new_size: usize) -> Result<(), WorldError> {
        for field in &self.fields {
            if out_of_bounds(field.offset, field.bytes, new_size) {
                return Err(WorldError::DynamicRowInvalid);
            }
            if let FieldSource::OldOffset(offset) = field.source {
                if out_of_bounds(offset, field.bytes, old_size) {
                    return Err(WorldError::DynamicRowInvalid);
                }
            }
        }
        Ok(())
    }
}

/// Whether `bytes` written at `offset` leaves a row of `size` bytes.
///
/// A helper rather than two `checked_add` expressions inline: the same question
/// is asked of the source and the destination of every instruction.
fn out_of_bounds(offset: usize, bytes: usize, size: usize) -> bool {
    match offset.checked_add(bytes) {
        Some(end) => end > size,
        None => true,
    }
}

// =============================================================================
// ComponentColumns
// =============================================================================

/// The native component columns of one archetype, keyed by [`ComponentId`].
///
/// This replaced a `TraitTypeMap` keyed by [`TypeId`](std::any::TypeId). That
/// map was already storing a concrete `ErasedVecStorage` per entry rather than
/// a trait object, so the only thing it contributed was the key - and `TypeId`
/// is the wrong key here. A component type linked into two binaries has two
/// `TypeId`s, so the binary that did not create the column could not find it,
/// while the engine identifies the same component by one [`ComponentId`] every
/// other structure is already keyed on: the registry's bit, the archetype's
/// `component_types`, the tick vectors, the storage factories. Keying the
/// columns the same way makes a shared component reachable from either binary
/// and removes a translation step from every lookup.
#[derive(Default)]
pub struct ComponentColumns {
    /// One contiguous column per native component in the archetype.
    columns: HashMap<ComponentId, ErasedVecStorage<dyn Component>>,
}

impl ComponentColumns {
    /// Creates an empty set of columns.
    pub fn new() -> Self {
        Self {
            columns: HashMap::new(),
        }
    }

    /// Creates an empty set of columns sized for `capacity` component types.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            columns: HashMap::with_capacity(capacity),
        }
    }

    /// Returns the column for `component_id`, or `None` when the archetype
    /// does not store that component.
    #[inline]
    pub fn get(&self, component_id: ComponentId) -> Option<&ErasedVecStorage<dyn Component>> {
        self.columns.get(&component_id)
    }

    /// Returns the column for `component_id` mutably.
    #[inline]
    pub fn get_mut(
        &mut self,
        component_id: ComponentId,
    ) -> Option<&mut ErasedVecStorage<dyn Component>> {
        self.columns.get_mut(&component_id)
    }

    /// Returns the column storing `T`.
    ///
    /// # Panics
    ///
    /// Panics when the archetype does not store `T`, which means the caller
    /// reached a column the archetype's mask says is not there.
    #[inline]
    pub fn column_of<T: Component>(&self) -> &ErasedVecStorage<dyn Component> {
        self.get(ComponentId::of::<T>())
            .unwrap_or_else(|| missing_column::<T>())
    }

    /// Returns the column storing `T`, mutably.
    ///
    /// # Panics
    ///
    /// Panics under the same condition as [`Self::column_of`].
    #[inline]
    pub fn column_of_mut<T: Component>(&mut self) -> &mut ErasedVecStorage<dyn Component> {
        self.get_mut(ComponentId::of::<T>())
            .unwrap_or_else(|| missing_column::<T>())
    }

    /// Inserts a column for `component_id`.
    ///
    /// # Panics
    ///
    /// Panics when a column for that component already exists; an archetype
    /// builds each of its columns exactly once.
    pub fn insert(&mut self, component_id: ComponentId, column: ErasedVecStorage<dyn Component>) {
        let replaced = self.columns.insert(component_id, column);
        assert!(
            replaced.is_none(),
            "component {component_id:?} already has a column in this archetype"
        );
    }

    /// Removes and returns the column for `component_id`.
    pub fn remove(&mut self, component_id: ComponentId) -> Option<ErasedVecStorage<dyn Component>> {
        self.columns.remove(&component_id)
    }

    /// Whether a column exists for `component_id`.
    #[inline]
    pub fn contains(&self, component_id: ComponentId) -> bool {
        self.columns.contains_key(&component_id)
    }

    /// Number of columns stored.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether no columns are stored.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// Report a lookup for a column the archetype does not have.
///
/// Split out of the accessors so the panic path stays off their inlined fast
/// path, and `#[cold]` so the branch predictor is told which way this goes.
#[cold]
#[inline(never)]
fn missing_column<T: Component>() -> ! {
    panic!(
        "archetype has no column for component {} ({:?}); the archetype's mask \
         and its columns disagree, or the component was never registered",
        std::any::type_name::<T>(),
        ComponentId::of::<T>(),
    )
}

// =============================================================================
// DynamicColumn
// =============================================================================

/// Aligned, densely packed storage for a type-erased component column.
///
/// Owns a raw heap allocation whose element size and alignment come from a
/// [`DynamicComponentLayout`]. Rows are written and read as raw bytes, which
/// lets components defined in other languages share storage with native ones.
pub struct DynamicColumn {
    /// Runtime layout of the stored element type.
    layout: DynamicComponentLayout,
    /// Pointer to the heap allocation, or dangling before the first growth.
    data: NonNull<u8>,
    /// Number of initialized rows.
    len: usize,
    /// Number of rows the current allocation can hold.
    capacity: usize,
}

impl DynamicColumn {
    /// Creates an empty column for the given runtime layout.
    ///
    /// No heap allocation is made until the first row is pushed.
    ///
    /// This is a POD-only container: rows are copied as raw bytes and the
    /// buffer is freed without running element destructors, so the layout
    /// must describe a blittable value type. `pill_host::csharp::components`
    /// enforces that through `BLITTABLE_FIELD_TYPES` before any layout is
    /// registered; the debug assertion below is a second line of defense for
    /// any caller that constructs a layout directly.
    pub fn new(layout: DynamicComponentLayout) -> Self {
        debug_assert!(
            layout.size > 0
                && layout.align > 0
                && layout.align.is_power_of_two()
                && std::alloc::Layout::from_size_align(layout.size, layout.align).is_ok(),
            "invalid DynamicComponentLayout: size {} align {} (must be a valid, \
             non-zero POD layout)",
            layout.size,
            layout.align
        );
        Self {
            layout,
            data: NonNull::dangling(),
            len: 0,
            capacity: 0,
        }
    }

    /// Returns the number of initialized rows in this column.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether this column holds no rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the size in bytes of a single stored component instance.
    pub fn element_size(&self) -> usize {
        self.layout.size
    }

    /// Returns the alignment in bytes required by a single stored instance.
    pub fn alignment(&self) -> usize {
        self.layout.align
    }

    /// Returns the schema hash identifying the stored component type.
    pub fn schema_hash(&self) -> u64 {
        self.layout.schema_hash
    }

    /// Returns a raw pointer to the underlying buffer.
    ///
    /// The pointer is dangling before the first allocation and is invalidated
    /// by any later reallocation or by dropping the column.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_ptr()
    }

    /// Appends a zero-initialized row to this column.
    ///
    /// Grows the column when needed and leaves the new row's bytes zeroed.
    pub fn push_zeroed(&mut self) {
        self.reserve_one();
        // SAFETY: reserve_one guarantees one writable, correctly aligned slot.
        unsafe {
            std::ptr::write_bytes(
                self.data.as_ptr().add(self.len * self.layout.size),
                0,
                self.layout.size,
            )
        };
        self.len += 1;
    }

    /// Appends a row containing a copy of the given bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicSizeMismatch`] when `bytes` does not
    /// contain exactly `element_size()` bytes.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), WorldError> {
        if bytes.len() != self.layout.size {
            return Err(WorldError::DynamicSizeMismatch);
        }
        self.reserve_one();
        // SAFETY: source and destination are valid for exactly one element and
        // cannot overlap because the source is outside this column's spare slot.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.data.as_ptr().add(self.len * self.layout.size),
                self.layout.size,
            );
        }
        self.len += 1;
        Ok(())
    }

    /// Copies the row at `index` from another column and appends it here.
    ///
    /// # Panics
    ///
    /// Panics when the two columns have different element sizes or when
    /// `index` is out of bounds of `source`.
    pub fn push_from(&mut self, source: &Self, index: usize) {
        assert_eq!(self.layout.size, source.layout.size);
        assert!(index < source.len);
        self.reserve_one();
        // SAFETY: both slots are allocated, aligned, non-overlapping columns.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source.data.as_ptr().add(index * source.layout.size),
                self.data.as_ptr().add(self.len * self.layout.size),
                self.layout.size,
            );
        }
        self.len += 1;
    }

    /// Overwrites the row at `index` with a copy of the given bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicRowInvalid`] when `index` is out of
    /// bounds or `bytes` does not contain exactly `element_size()` bytes.
    pub fn set_bytes(&mut self, index: usize, bytes: &[u8]) -> Result<(), WorldError> {
        if index >= self.len || bytes.len() != self.layout.size {
            return Err(WorldError::DynamicRowInvalid);
        }
        // SAFETY: the checked row is initialized and bytes has one element's size.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.data.as_ptr().add(index * self.layout.size),
                self.layout.size,
            );
        }
        Ok(())
    }

    /// Returns the raw bytes of the row at `index`, or `None` when out of bounds.
    pub fn bytes(&self, index: usize) -> Option<&[u8]> {
        if index >= self.len {
            return None;
        }
        // SAFETY: the row is initialized and lives for the returned shared borrow.
        Some(unsafe {
            std::slice::from_raw_parts(
                self.data.as_ptr().add(index * self.layout.size),
                self.layout.size,
            )
        })
    }

    /// Rewrite every row into a new layout, following `plan`.
    ///
    /// The column keeps its row count and its row order, so indices callers
    /// hold - every `EntityLocation` that names a row among them - stay valid.
    /// Anything the plan does not cover is zeroed, which is how a field added to
    /// a foreign-language component starts at a defined value instead of a byte
    /// left over from the previous shape.
    ///
    /// Rows are transformed through a scratch copy, so a plan that moves fields
    /// inside a row cannot read a byte it has already overwritten. The buffer is
    /// rewritten in place when the element size and alignment are unchanged and
    /// reallocated at the same capacity otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicSizeZero`],
    /// [`WorldError::DynamicAlignmentInvalid`] or
    /// [`WorldError::DynamicLayoutInvalid`] when the new layout cannot describe
    /// storage, and [`WorldError::DynamicRowInvalid`] when a planned field reads
    /// or writes past the edge of a row. Nothing is modified in either case.
    pub fn relayout(
        &mut self,
        layout: DynamicComponentLayout,
        plan: &DynamicFieldPlan,
    ) -> Result<usize, WorldError> {
        validate_dynamic_layout(layout.size, layout.align)?;
        plan.validate(self.layout.size, layout.size)?;
        Ok(self.relayout_validated(layout, plan))
    }

    /// [`Self::relayout`] for a caller that has already validated.
    ///
    /// Infallible by construction: the only failure modes are the layout and
    /// plan checks above, and both are deterministic functions of data the
    /// caller can check once for every column it is about to rewrite. That is
    /// what lets a world-level relayout refuse a bad plan before it touches the
    /// first column, instead of leaving some columns migrated and some not.
    pub(crate) fn relayout_validated(
        &mut self,
        layout: DynamicComponentLayout,
        plan: &DynamicFieldPlan,
    ) -> usize {
        let rows = self.len;
        let old_size = self.layout.size;
        let old_align = self.layout.align;
        let old_data = self.data;
        let in_place = layout.size == old_size && layout.align == old_align;

        // Step 1: Prepare the destination buffer. An unchanged shape keeps the
        // buffer it has; anything else gets a fresh one at the same capacity,
        // because a relayout is not a reason to re-grow on the next push.
        let mut new_data = old_data;
        if !in_place {
            new_data = if self.capacity == 0 {
                NonNull::dangling()
            } else {
                let bytes = layout
                    .size
                    .checked_mul(self.capacity)
                    .expect("dynamic column too large");
                let allocation = Layout::from_size_align(bytes, layout.align)
                    .expect("dynamic layout validated by the caller");
                // SAFETY: the allocation has non-zero size, checked above.
                let pointer = unsafe { alloc(allocation) };
                NonNull::new(pointer).unwrap_or_else(|| handle_alloc_error(allocation))
            };
        }

        // Step 2: Transform one row at a time. The scratch copy is what makes
        // this correct in place as well as across buffers: every source byte is
        // read before the destination row is zeroed.
        if rows != 0 {
            let mut scratch = vec![0_u8; old_size.max(layout.size)];
            for row in 0..rows {
                // SAFETY: `row < rows` counts only initialized rows, and both
                // buffers are allocated for at least `rows` rows.
                unsafe {
                    let source = old_data.as_ptr().add(row * old_size);
                    std::ptr::copy_nonoverlapping(source, scratch.as_mut_ptr(), old_size);
                    let destination = new_data.as_ptr().add(row * layout.size);
                    std::ptr::write_bytes(destination, 0, layout.size);
                    for field in plan.fields() {
                        let FieldSource::OldOffset(offset) = field.source else {
                            continue;
                        };
                        std::ptr::copy_nonoverlapping(
                            scratch.as_ptr().add(offset),
                            destination.add(field.offset),
                            field.bytes,
                        );
                    }
                }
            }
        }

        // Step 3: Release the buffer that is no longer the column's, and adopt
        // the new shape. `capacity != 0` is the same "is there an allocation"
        // test `Drop` uses, so a column that never allocated never reaches
        // `dealloc`.
        if !in_place {
            if self.capacity != 0 {
                // SAFETY: this is the live allocation, created with this layout.
                unsafe {
                    dealloc(
                        old_data.as_ptr(),
                        Layout::from_size_align_unchecked(old_size * self.capacity, old_align),
                    );
                }
            }
            self.data = new_data;
        }
        self.layout = layout;
        rows
    }

    /// Removes the row at `index` by swapping in the last row.
    ///
    /// Keeps the column dense and runs in O(1), but does not preserve row
    /// ordering.
    ///
    /// # Panics
    ///
    /// Panics when `index` is out of bounds.
    pub fn swap_remove(&mut self, index: usize) {
        assert!(index < self.len);
        let last = self.len - 1;
        if index != last {
            // SAFETY: both rows are within this allocation; copy permits overlap.
            unsafe {
                std::ptr::copy(
                    self.data.as_ptr().add(last * self.layout.size),
                    self.data.as_ptr().add(index * self.layout.size),
                    self.layout.size,
                );
            }
        }
        self.len = last;
    }

    /// Ensures capacity for at least one more row, growing the buffer when full.
    fn reserve_one(&mut self) {
        // Step 1: Early-exit when the column still has a spare slot.
        if self.len < self.capacity {
            return;
        }

        // Step 2: Compute the doubled capacity and the layout it requires.
        //
        // The floor is applied *after* doubling. Applying it before made the
        // first allocation eight rows rather than four, over-allocating every
        // manifest-driven column on first use.
        let new_capacity = self.capacity.checked_mul(2).unwrap_or(4).max(4);
        let new_layout = Layout::from_size_align(
            self.layout
                .size
                .checked_mul(new_capacity)
                .expect("dynamic column too large"),
            self.layout.align,
        )
        .expect("invalid dynamic component layout");

        // Step 3: Allocate the new buffer and migrate the existing rows.
        // SAFETY: new_layout has non-zero size because component size is validated.
        let new_data = unsafe { alloc(new_layout) };
        let new_data = NonNull::new(new_data).unwrap_or_else(|| handle_alloc_error(new_layout));
        if self.len != 0 {
            // SAFETY: both allocations are valid and non-overlapping.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data.as_ptr(),
                    new_data.as_ptr(),
                    self.len * self.layout.size,
                );
                dealloc(
                    self.data.as_ptr(),
                    Layout::from_size_align_unchecked(
                        self.layout.size * self.capacity,
                        self.layout.align,
                    ),
                );
            }
        }

        // Step 4: Adopt the new allocation as the column's buffer.
        self.data = new_data;
        self.capacity = new_capacity;
    }
}

// SAFETY: Two premises, each enforced by named code rather than asserted here.
//
// 1. Every field of a dynamic component is a blittable value type, enforced by
//    `BLITTABLE_FIELD_TYPES` in `pill_host::csharp::components`, which rejects
//    any manifest declaring a managed reference. This is what makes the raw
//    `ptr::copy` in `swap_remove` and the destructor-free `Drop` below correct:
//    there is no ownership to duplicate or release.
// 2. Access is serialised by the same scheduler rules as native columns - see
//    `SystemAccess::conflicts_with`, which only takes its bitmask fast path when
//    both systems' access masks are complete.
unsafe impl Send for DynamicColumn {}
// SAFETY: Shared access to a column only permits reading rows, which is sound
// under the same two premises as `Send` above: rows are blittable value types
// and access is serialised by the scheduler, so no write can race a read.
unsafe impl Sync for DynamicColumn {}

impl Drop for DynamicColumn {
    /// Frees the buffer. **No element destructor runs, by design.**
    ///
    /// Every field of a dynamic component is a blittable value type - enforced
    /// by `BLITTABLE_FIELD_TYPES` in `pill_host::csharp::components`, which
    /// rejects any manifest declaring otherwise - so a row owns nothing that
    /// needs releasing. That is also what makes the raw `ptr::copy` in
    /// `swap_remove` correct: moving a row cannot duplicate ownership.
    ///
    /// If dynamic components ever need to own a resource, this is the first
    /// place that has to change: `DynamicComponentLayout` would need an
    /// optional `drop_fn`, called here and from `swap_remove`.
    fn drop(&mut self) {
        if self.capacity != 0 {
            // SAFETY: this is the live allocation created by reserve_one.
            unsafe {
                dealloc(
                    self.data.as_ptr(),
                    Layout::from_size_align_unchecked(
                        self.layout.size * self.capacity,
                        self.layout.align,
                    ),
                );
            }
        }
    }
}

// =============================================================================
// ArchetypeId
// =============================================================================

/// ArchetypeId uniquely identifies an archetype by its component mask.
///
/// Derived from the archetype's [`ComponentMask`], guaranteeing a 1:1
/// mapping without a separate lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchetypeId(
    /// The packed component mask bits identifying this archetype.
    pub u128,
);

// =============================================================================
// Archetype
// =============================================================================

/// Core storage unit grouping entities with the same component set.
///
/// Uses a Structure of Arrays (SoA) layout for cache-friendly bulk iteration.
pub struct Archetype {
    /// Unique identifier derived from this archetype's component mask.
    pub id: ArchetypeId,
    /// Component types stored in this archetype, in registration order.
    ///
    /// Still needed for iteration and lookup.
    pub component_types: Vec<ComponentId>,
    /// Bitmask of the stored component types for fast query matching.
    pub component_mask: ComponentMask,
    /// Type-erased native component storage, keyed by component id.
    pub component_storages: ComponentColumns,
    /// Byte-oriented storage for components owned by other languages.
    pub dynamic_component_storages: HashMap<ComponentId, DynamicColumn>,
    /// Entities currently stored in this archetype.
    pub entities: Vec<Entity>,
    /// Per-component-instance change-detection metadata.
    ///
    /// For each `ComponentId` in `component_types`, the matching
    /// `Vec<ComponentTicks>` is kept in lockstep with the underlying
    /// component storage: row `i` of the tick vec corresponds to row `i`
    /// of the component vec for the same entity. Maintenance happens in
    /// `World` whenever entities are inserted, moved between archetypes,
    /// or destroyed.
    pub component_ticks: HashMap<ComponentId, Vec<ComponentTicks>>,
}

impl Archetype {
    /// Creates a new archetype with storage for the specified component types.
    ///
    /// `storage_factories` provides a way to create storage for each component
    /// type by [`ComponentId`], allowing archetype creation without knowing the
    /// concrete types.
    ///
    /// # Panics
    ///
    /// Panics when a component type has no entry in `storage_factories`, which
    /// happens when `world.register_component::<T>()` was not called for it.
    pub fn new(
        id: ArchetypeId,
        component_types: Vec<ComponentId>,
        component_mask: ComponentMask,
        storage_factories: &HashMap<ComponentId, StorageFactory>,
    ) -> Self {
        // Step 1: Pre-size the storage maps for the component count.
        let component_count = component_types.len();
        let _zone = crate::profile_scope!(
            "archetype new",
            [("Component types in this archetype: {}", component_count)]
        );
        let mut component_storages = ComponentColumns::with_capacity(component_count);
        let mut dynamic_component_storages = HashMap::new();
        let mut component_ticks: HashMap<ComponentId, Vec<ComponentTicks>> =
            HashMap::with_capacity(component_count);

        // Step 2: Create storage for each component type using its factory.
        for &component_id in &component_types {
            let factory = storage_factories.get(&component_id)
                .unwrap_or_else(|| panic!(
                    "Component type {:?} not registered. Call world.register_component::<T>() first.",
                    component_id
                ));
            match factory {
                StorageFactory::Native(info) => {
                    // Build the erased column from the registered type
                    // description. It is stored as a concrete
                    // `ErasedVecStorage` (no trait-object vtable), so the
                    // column stays valid across module unloads, and under the
                    // component's id rather than its `TypeId`, so a component
                    // shared between binaries resolves to it from either one.
                    component_storages
                        .insert(component_id, ErasedVecStorage::<dyn Component>::new(*info));
                }
                StorageFactory::Dynamic(layout) => {
                    dynamic_component_storages
                        .insert(component_id, DynamicColumn::new(layout.clone()));
                }
            }
            component_ticks.insert(component_id, Vec::new());
        }

        // Step 3: Emit allocation telemetry and assemble the archetype.
        crate::profile_message!(
            "archetype {:?} allocated with {} component storage columns for up to 0 entities",
            id,
            component_count,
        );

        Self {
            id,
            component_types,
            component_mask,
            component_storages,
            dynamic_component_storages,
            entities: Vec::new(),
            component_ticks,
        }
    }

    /// Checks whether this archetype stores the component type at the given mask bit.
    ///
    /// Uses the bitmask for O(1) lookup instead of a linear search through
    /// the component types.
    #[inline]
    pub fn has_component_bit(&self, bit: u8) -> bool {
        self.component_mask.has_bit(bit)
    }

    /// Checks whether this archetype stores the specified component type.
    ///
    /// Note: This uses an O(n) linear search. Prefer `has_component_bit` with
    /// a pre-looked-up bit index for hot paths.
    pub fn has_component<T: Component>(&self) -> bool {
        self.component_types.contains(&ComponentId::of::<T>())
    }

    /// Checks whether this archetype contains every component a query requires.
    ///
    /// Called in the query setup hot path for every archetype.
    #[inline]
    pub fn matches_mask(&self, required_mask: &ComponentMask) -> bool {
        self.component_mask.contains_all(required_mask)
    }

    /// Returns the number of entities in this archetype.
    #[inline]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Returns the number of entities in this archetype (alias for [`Self::len`]).
    ///
    /// Provided for API consistency with other collection types.
    #[inline]
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Returns `true` when this archetype contains no entities.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Builds a human-readable summary of this archetype.
    ///
    /// Includes the archetype ID, the entity count, and the names of the
    /// stored component types. Unknown component IDs render as "Unknown".
    pub fn get_archetype_info(&self, registry: &crate::component::ComponentRegistry) -> String {
        let component_names: Vec<String> = self
            .component_types
            .iter()
            .map(|component_id| {
                registry
                    .get_name(component_id)
                    .unwrap_or("Unknown")
                    .to_string()
            })
            .collect();

        format!(
            "Archetype {:?}: {} entities, components: [{}]",
            self.id,
            self.entities.len(),
            component_names.join(", ")
        )
    }

    /// Prints information about this archetype (component names and entity count).
    #[cold]
    pub fn print_info(&self, registry: &crate::component::ComponentRegistry) {
        let info = self.get_archetype_info(registry);
        println!("{}", info);
    }

    /// Estimate the memory footprint of this archetype in bytes.
    ///
    /// Sums entity IDs, component column capacities x element sizes,
    /// and change-detection tick vectors.
    pub fn memory_estimate(&self, registry: &crate::component::ComponentRegistry) -> usize {
        // Step 1: Account for the entity ID array (16 bytes per entity).
        let mut total = self.entities.capacity() * std::mem::size_of::<crate::entity::Entity>();

        // Step 2: Account for each component column (entity count as a lower bound).
        for &component_id in &self.component_types {
            if let Some(size) = registry.get_size(&component_id) {
                // We can't inspect the Vec's capacity through the trait object,
                // so we use entity count as a lower bound. The actual Vec capacity
                // may be larger due to pre-allocation.
                total += self.entities.len() * size;
            }
        }

        // Step 3: Account for the change-detection tick vectors (8 bytes per tick).
        total += self.component_types.len() * self.entities.capacity() * 8;

        // Step 4: Approximate the HashMap overhead for the tick lookup.
        total += self.component_types.len() * 64;

        total
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// One row of a two-field layout: `a` at 0, `b` at 4.
    fn two_fields() -> DynamicComponentLayout {
        DynamicComponentLayout {
            size: 8,
            align: 4,
            schema_hash: 1,
        }
    }

    fn column_with_rows(rows: &[[u8; 8]]) -> DynamicColumn {
        let mut column = DynamicColumn::new(two_fields());
        for row in rows {
            column.push_bytes(row).expect("row matches the layout");
        }
        column
    }

    #[test]
    fn plan_between_matches_fields_by_name_not_by_position() {
        let old = [
            LayoutField {
                name: "a",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                offset: 4,
                size: 4,
            },
        ];
        // The same two fields, swapped in the new layout.
        let new = [
            LayoutField {
                name: "b",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "a",
                offset: 4,
                size: 4,
            },
        ];

        let plan = DynamicFieldPlan::between(&old, &new);

        assert_eq!(
            plan.fields(),
            &[
                PlannedField {
                    offset: 0,
                    bytes: 4,
                    source: FieldSource::OldOffset(4),
                },
                PlannedField {
                    offset: 4,
                    bytes: 4,
                    source: FieldSource::OldOffset(0),
                },
            ]
        );
    }

    #[test]
    fn plan_between_zeroes_the_new_fields_and_omits_the_removed_ones() {
        let old = [
            LayoutField {
                name: "kept",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "removed",
                offset: 4,
                size: 4,
            },
        ];
        let new = [
            LayoutField {
                name: "kept",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "added",
                offset: 4,
                size: 8,
            },
        ];

        let plan = DynamicFieldPlan::between(&old, &new);

        // The removed field leaves no instruction at all, and the added one is
        // an explicit zero fill rather than a missing entry.
        assert_eq!(
            plan.fields(),
            &[
                PlannedField {
                    offset: 0,
                    bytes: 4,
                    source: FieldSource::OldOffset(0),
                },
                PlannedField {
                    offset: 4,
                    bytes: 8,
                    source: FieldSource::ZeroFill,
                },
            ]
        );
    }

    #[test]
    fn plan_between_copies_the_smaller_of_two_sizes() {
        let old = [LayoutField {
            name: "grew",
            offset: 0,
            size: 4,
        }];
        let new = [LayoutField {
            name: "grew",
            offset: 0,
            size: 8,
        }];
        assert_eq!(
            DynamicFieldPlan::between(&old, &new).fields()[0].bytes,
            4,
            "the tail of a grown field has to come from the zero fill, not the old row"
        );

        let old = [LayoutField {
            name: "shrank",
            offset: 0,
            size: 8,
        }];
        let new = [LayoutField {
            name: "shrank",
            offset: 0,
            size: 4,
        }];
        assert_eq!(DynamicFieldPlan::between(&old, &new).fields()[0].bytes, 4);
    }

    #[test]
    fn relayout_moves_rows_through_the_plan() {
        // Two rows, `a` and `b` each holding a distinct u32.
        let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0], [3, 0, 0, 0, 4, 0, 0, 0]]);
        let old = [
            LayoutField {
                name: "a",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                offset: 4,
                size: 4,
            },
        ];
        // `b` first, then `a`, then a new eight-byte field.
        let new = [
            LayoutField {
                name: "b",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "a",
                offset: 4,
                size: 4,
            },
            LayoutField {
                name: "added",
                offset: 8,
                size: 8,
            },
        ];
        let plan = DynamicFieldPlan::between(&old, &new);

        let rows = column
            .relayout(
                DynamicComponentLayout {
                    size: 16,
                    align: 8,
                    schema_hash: 2,
                },
                &plan,
            )
            .expect("the plan fits both layouts");

        assert_eq!(rows, 2);
        assert_eq!(
            column.bytes(0).unwrap(),
            [2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].as_slice()
        );
        assert_eq!(
            column.bytes(1).unwrap(),
            [4, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].as_slice()
        );
        assert_eq!(column.element_size(), 16);
        assert_eq!(column.schema_hash(), 2);
    }

    #[test]
    fn relayout_of_an_equal_shape_rewrites_rows_in_place() {
        let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0]]);
        let before = column.as_mut_ptr();
        let plan = DynamicFieldPlan::between(
            &[
                LayoutField {
                    name: "a",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "b",
                    offset: 4,
                    size: 4,
                },
            ],
            &[
                LayoutField {
                    name: "b",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "a",
                    offset: 4,
                    size: 4,
                },
            ],
        );

        column
            .relayout(
                DynamicComponentLayout {
                    size: 8,
                    align: 4,
                    schema_hash: 3,
                },
                &plan,
            )
            .expect("an equal-shape relayout of a fitting plan");

        assert_eq!(
            column.as_mut_ptr(),
            before,
            "an unchanged shape must not reallocate: pointers into it may be live"
        );
        // The scratch copy is what makes this correct: `b` overwrites `a`'s
        // bytes before `a` has been read out of the same buffer.
        assert_eq!(
            column.bytes(0).unwrap(),
            [2, 0, 0, 0, 1, 0, 0, 0].as_slice()
        );
    }

    #[test]
    fn relayout_keeps_the_row_count_and_capacity() {
        let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0], [3, 0, 0, 0, 4, 0, 0, 0]]);
        let plan = DynamicFieldPlan::new();

        column
            .relayout(
                DynamicComponentLayout {
                    size: 4,
                    align: 4,
                    schema_hash: 4,
                },
                &plan,
            )
            .expect("an empty plan always fits");

        assert_eq!(column.len(), 2);
        assert_eq!(column.bytes(0).unwrap(), [0, 0, 0, 0].as_slice());
        assert_eq!(column.bytes(1).unwrap(), [0, 0, 0, 0].as_slice());
        // Capacity was kept, so the next push still has its spare slot.
        column
            .push_bytes(&[9, 0, 0, 0])
            .expect("fits the kept capacity");
        assert_eq!(column.len(), 3);
    }

    #[test]
    fn relayout_refuses_a_plan_that_leaves_a_row() {
        let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0]]);
        let mut plan = DynamicFieldPlan::new();
        plan.push(4, 8, FieldSource::OldOffset(0));

        let result = column.relayout(
            DynamicComponentLayout {
                size: 8,
                align: 4,
                schema_hash: 1,
            },
            &plan,
        );

        assert!(matches!(result, Err(WorldError::DynamicRowInvalid)));
        assert_eq!(
            column.bytes(0).unwrap(),
            [1, 0, 0, 0, 2, 0, 0, 0].as_slice(),
            "a refused plan must not touch the rows"
        );
    }

    #[test]
    fn relayout_refuses_a_layout_that_cannot_be_allocated() {
        let mut column = column_with_rows(&[]);
        assert!(matches!(
            column.relayout(
                DynamicComponentLayout {
                    size: 0,
                    align: 4,
                    schema_hash: 1
                },
                &DynamicFieldPlan::new()
            ),
            Err(WorldError::DynamicSizeZero)
        ));
        assert!(matches!(
            column.relayout(
                DynamicComponentLayout {
                    size: 4,
                    align: 3,
                    schema_hash: 1
                },
                &DynamicFieldPlan::new()
            ),
            Err(WorldError::DynamicAlignmentInvalid)
        ));
    }
}

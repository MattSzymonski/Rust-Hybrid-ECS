//! Singleton resources stored in the [`World`], not attached to entities.
//!
//! # Responsibilities
//!
//! - Defines the [`Resource`] marker trait for global/shared state types.
//! - Provides [`ResourceId`] for type-erased resource identification, stable
//!   across artifacts when the type declares a shared name.
//! - Implements [`ResHandle`] - a lightweight, copyable handle for deferred resource access.
//! - Owns [`ErasedResource`], the storage one resource value lives in.
//!
//! # Design
//!
//! Resources represent global state such as time, input, configuration,
//! and asset stores. They are accessed by systems through `Res<T>` (immutable)
//! and `ResMut<T>` (mutable) system parameters. [`ResHandle<T>`] provides a
//! zero-cost typed reference that can be stored and passed around without
//! borrowing the [`World`].
//!
//! ## Usage
//!
//! ```no_run
//! # use pill_engine::*;
//! // Define a resource
//! #[derive(Debug)]
//! struct ProjectTime { delta: f32, elapsed: f32 }
//! impl Resource for ProjectTime {}
//!
//! // Insert into world
//! # let mut world = World::new();
//! world.insert_resource(ProjectTime { delta: 0.016, elapsed: 0.0 });
//!
//! // Get a handle (cheap, copyable)
//! let handle = ResHandle::<ProjectTime>::new();
//!
//! // Use handle to access the resource later
//! let time = handle.get(&world).unwrap();
//!
//! // Use in systems via Res/ResMut
//! fn my_system(time: Res<ProjectTime>) {
//!     println!("Elapsed: {}", time.get().unwrap().elapsed);
//! }
//! ```

// Standard library
use std::alloc::Layout;
use std::any::TypeId;
use std::marker::PhantomData;
use std::ptr::NonNull;

// Current crate
use pill_core::error;

use crate::archetype::{FieldPlan, FieldSource};
use crate::error::WorldError;
use crate::world::World;

// =============================================================================
// Resource
// =============================================================================

/// Resource marker trait - resources are singleton data stored in the World.
///
/// Unlike components, resources are not attached to entities. They represent
/// global/shared state such as time, input, configuration, etc.
///
/// Resources must be Send + Sync to support parallel system access.
///
/// # Examples
///
/// ```
/// use pill_engine::Resource;
///
/// struct ProjectTime {
///     delta: f32,
///     elapsed: f32,
/// }
///
/// impl Resource for ProjectTime {}
/// ```
pub trait Resource: Send + Sync + 'static {
    /// Stable, cross-artifact name for this resource, or `None` for the
    /// ordinary per-binary identity.
    ///
    /// The resource counterpart of
    /// [`Component::shared_name`](crate::component::Component::shared_name),
    /// and it exists for the same reason: a type compiled into two artifacts
    /// with differing inputs gets a different [`TypeId`] in each, so
    /// identifying a resource by `TypeId` splits it into two resources that
    /// cannot see each other.
    ///
    /// The name must be unique across the whole process, so it should be
    /// namespaced - `"pill_spline::SplineSettings"`, not `"Settings"`.
    ///
    /// Most resources do not need this. A type defined in `pill_engine`, or in
    /// any crate every artifact compiles identically, already agrees on its
    /// `TypeId` everywhere - which is why `Time` and `AssetManager` work
    /// across the boundary untouched.
    fn shared_name() -> Option<&'static str>
    where
        Self: Sized,
    {
        None
    }

    /// The stable identity [`Self::shared_name`] hashes to, or `None`.
    ///
    /// Defaulted in terms of `shared_name`, so an implementor writes only the
    /// name and the two cannot disagree. Override it only with
    /// `shared_component_identity(Self::shared_name())`; any other value
    /// silently splits the resource's identity from the name every other
    /// artifact derives it from.
    fn shared_identity() -> Option<u128>
    where
        Self: Sized,
    {
        Self::shared_name().map(crate::component::shared_component_identity)
    }

    /// Structural hash of this resource's declared layout, or `None`.
    ///
    /// The resource counterpart of a component's schema hash: size and
    /// alignment cannot tell `{u32, u32}` from `{f32, f32}`, so a shared
    /// resource whose declaring types disagree only in field types is caught
    /// by this instead of being read through the wrong `T`.
    ///
    /// Defaulted to `None`, and only compared when **both** sides of a claim
    /// carry a value, so a type that does not need the extra check is not
    /// required to lie about having one. The natural source, when the type
    /// has a field vocabulary, is
    /// [`component_schema_hash`](crate::component::component_schema_hash).
    fn shared_schema_hash() -> Option<u64>
    where
        Self: Sized,
    {
        None
    }
}

// =============================================================================
// ResourceId
// =============================================================================

/// Uniquely identifies a resource type.
///
/// Either the Rust [`TypeId`], or - for a resource declaring
/// [`Resource::shared_name`] - a stable identity derived from that name, so
/// artifacts that compiled the type with different inputs still agree.
///
/// `Hash` is implemented by hand below rather than derived; see the impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourceId {
    /// Resource identified by its Rust [`TypeId`], which differs between
    /// artifacts that compiled the type with different inputs.
    Native(TypeId),
    /// Resource identified by the stable name it declares, which every
    /// artifact computes identically.
    Shared(u128),
}

impl ResourceId {
    /// Returns the [`ResourceId`] identifying the resource type `T`.
    ///
    /// For an ordinary resource this is its [`TypeId`]. For one declaring
    /// [`Resource::shared_name`] it is the stable identity derived from that
    /// name, so every artifact that links the type produces the same id.
    #[inline]
    pub fn of<T: Resource>() -> Self {
        match T::shared_identity() {
            Some(identity) => Self::Shared(identity),
            None => Self::Native(TypeId::of::<T>()),
        }
    }

    /// The stable identity behind a shared resource id, if this is one.
    pub const fn shared_identity(self) -> Option<u128> {
        match self {
            Self::Shared(identity) => Some(identity),
            Self::Native(_) => None,
        }
    }
}

/// `Hash` is written by hand so a 128-bit identity feeds the hasher as 64 bits,
/// exactly as [`ComponentId`](crate::component::ComponentId) does and for the
/// same measured reason: a map resolves collisions with `Eq`, which still
/// compares every bit, so the second 8-byte block buys no correctness and costs
/// a compression round.
impl std::hash::Hash for ResourceId {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            // `TypeId`'s own impl already narrows 128 bits to 64.
            Self::Native(type_id) => type_id.hash(state),
            Self::Shared(identity) => (*identity as u64 ^ SHARED_RESOURCE_HASH_SALT).hash(state),
        }
    }
}

/// Salt keeping a shared identity from colliding with a `TypeId`-derived hash
/// on an identical low half. Arbitrary odd constant.
const SHARED_RESOURCE_HASH_SALT: u64 = 0xd6e8_feb8_6659_fd93;

// =============================================================================
// ResHandle
// =============================================================================

/// A lightweight, typed handle to a resource in the World.
///
/// `ResHandle<T>` is a zero-cost abstraction that stores the resource's type
/// information. Handles are `Copy`, `Clone`, `Send`, and `Sync`, making them
/// easy to store and pass around without borrowing the World.
///
/// Use handles when you need to:
/// - Store a reference to a resource type for later access
/// - Pass resource type information between systems or phases
/// - Defer resource access to a later point
///
/// # Examples
/// ```no_run
/// # use pill_engine::*;
/// #[derive(Debug)]
/// struct Score(u32);
/// impl Resource for Score {}
///
/// // Create a handle
/// let handle = ResHandle::<Score>::new();
///
/// // Insert the resource
/// # let mut world = World::new();
/// world.insert_resource(Score(0));
///
/// // Use the handle to access the resource
/// let score = handle.get(&world).unwrap();
/// assert_eq!(score.0, 0);
///
/// // Mutably access via handle
/// let score = handle.get_mut(&mut world).unwrap();
/// score.0 += 10;
/// ```
pub struct ResHandle<T: Resource> {
    /// Type-level marker carrying no data; the handle is zero-sized at runtime.
    _phantom: PhantomData<T>,
}

impl<T: Resource> ResHandle<T> {
    /// Creates a new handle for a resource type.
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }

    /// Gets the [`ResourceId`] for this handle's resource type.
    pub fn id(&self) -> ResourceId {
        ResourceId::of::<T>()
    }

    /// Gets an immutable reference to the resource from the [`World`].
    ///
    /// Returns `None` if the resource has not been inserted.
    pub fn get<'w>(&self, world: &'w World) -> Option<&'w T> {
        world.get_resource::<T>()
    }

    /// Gets a mutable reference to the resource from the [`World`].
    ///
    /// Returns `None` if the resource has not been inserted.
    pub fn get_mut<'w>(&self, world: &'w mut World) -> Option<&'w mut T> {
        world.get_resource_mut::<T>()
    }

    /// Checks whether the resource exists in the [`World`].
    pub fn exists(&self, world: &World) -> bool {
        world.has_resource::<T>()
    }
}

impl<T: Resource> Default for ResHandle<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Resource> Clone for ResHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Resource> Copy for ResHandle<T> {}

// SAFETY: `ResHandle<T>` stores only `PhantomData<T>` and never owns or
// references a value of type `T`, so no `T` data is moved, shared, or
// aliased through a handle. Marking every handle `Send` and `Sync` is
// therefore sound for any `T`: handles are inert, zero-sized type-level
// markers with no data to race on, alias, or invalidate.
unsafe impl<T: Resource> Send for ResHandle<T> {}
// SAFETY: As for `Send` above - a handle carries no `T` data to share, so
// shared access across threads cannot race or alias anything.
unsafe impl<T: Resource> Sync for ResHandle<T> {}

impl<T: Resource> std::fmt::Debug for ResHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResHandle<{}>", std::any::type_name::<T>())
    }
}

// =============================================================================
// Tests
// =============================================================================

// =============================================================================
// ErasedResource
// =============================================================================

/// Per-type behaviour an [`ErasedResource`] needs, plus the layout facts that
/// go with it, as plain data.
///
/// Function pointers rather than a vtable, for the same reason
/// [`ColumnOps`](crate::archetype::ColumnOps) is on the component side: a
/// `Box<dyn Any>` carries its destructor in a vtable that lives in the image
/// of whichever artifact created the value, and resources outlive the module
/// that inserted them - nothing clears them on reload. A table that is plain
/// data can be replaced when the code it points at is about to be unmapped.
///
/// Only a drop is needed. Unlike a component column, a resource is never
/// upcast to a trait object, so there are no `up_ref`/`up_mut`/`take_boxed`
/// counterparts.
///
/// The layout fields moved here so one table answers every question about a
/// registration: a value's size and alignment (which a box needs for its own
/// allocation) and, for a declaration from another language, the schema hash
/// its manifest carried.
#[derive(Clone, Copy)]
pub struct ErasedResourceOps {
    /// Drop the initialized value at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must point at a live, correctly aligned value of the type this
    /// table was built for.
    pub drop_in_place: unsafe fn(*mut u8),
    /// Size in bytes of the values this table describes.
    pub size: usize,
    /// Alignment in bytes of the values this table describes.
    pub align: usize,
    /// Schema hash the declaring manifest carried, for a foreign declaration.
    /// `None` for a Rust type, whose layout *is* its type.
    pub schema_hash: Option<u64>,
    /// Whether this table drops nothing because its values are blittable bytes
    /// owned by another language.
    ///
    /// Read in two places: [`World::rehome_resources`] leaves such a box's
    /// table alone - there is no newer code to point it at, and no destructor
    /// to keep valid - and the raw byte accessors refuse to hand out anything
    /// else.
    pub foreign: bool,
}

impl ErasedResourceOps {
    /// Assemble the table for a concrete resource type.
    pub fn of<T: Resource>() -> Self {
        Self {
            drop_in_place: drop_in_place_of::<T>,
            size: std::mem::size_of::<T>(),
            align: std::mem::align_of::<T>(),
            schema_hash: None,
            foreign: false,
        }
    }

    /// Assemble the table for a foreign declaration.
    ///
    /// The drop is deliberately a no-op: a foreign resource is a run of
    /// blittable bytes, enforced by the caller (the C# manifest path allows
    /// only unmanaged value types), so there is nothing to release and no
    /// artifact whose code could go away.
    pub fn foreign(size: usize, align: usize, schema_hash: u64) -> Self {
        Self {
            drop_in_place: drop_nothing,
            size,
            align,
            schema_hash: Some(schema_hash),
            foreign: true,
        }
    }
}

/// Drop the `T` at `ptr`.
///
/// # Safety
///
/// `ptr` must point at a live, correctly aligned `T`.
unsafe fn drop_in_place_of<T>(ptr: *mut u8) {
    // SAFETY: guaranteed by the caller; the box upholds it.
    unsafe { std::ptr::drop_in_place(ptr.cast::<T>()) };
}

/// Drop nothing.
///
/// # Safety
///
/// Always sound: a foreign resource owns no allocation and holds no value that
/// needs releasing, which is the whole point of the blittable-only rule.
unsafe fn drop_nothing(_ptr: *mut u8) {}

/// One resource value, stored without a trait object.
///
/// Replaces `Box<dyn Any + Send + Sync>`. Two reasons, and the second is what
/// makes it necessary rather than tidy:
///
/// 1. `Any::downcast_ref` compares [`TypeId`], which differs between artifacts
///    that compiled the same type with different inputs, so a resource
///    inserted by one could not be read by another.
/// 2. A `Box<dyn Any>`'s vtable - including its destructor - lives in the image
///    of the artifact that created the value. Resources are never cleared on
///    reload, so retiring a module that owned one leaves a destructor pointing
///    into an unmapped image. The table here is plain data and can be re-homed.
///
/// The allocation is owned by this value and released on drop.
pub struct ErasedResource {
    /// Heap allocation holding the value, or dangling for a zero-sized type.
    data: NonNull<u8>,
    /// Runtime type identity of the stored value, as the creating artifact
    /// sees it, or `None` when the value has no Rust type at all.
    type_id: Option<TypeId>,
    /// Size in bytes of the stored value.
    size: usize,
    /// Alignment in bytes of the stored value.
    align: usize,
    /// Whether the stored value is identified by its layout rather than by
    /// `type_id`; set when the resource declares a shared name.
    shared_identity: bool,
    /// Per-type function table; replaceable via [`Self::refresh_ops`].
    ops: ErasedResourceOps,
}

// SAFETY: `Resource` is `Send + Sync`, and this box only ever holds a value of
// such a type. The raw pointer is owned exclusively by the box.
unsafe impl Send for ErasedResource {}
// SAFETY: as above.
unsafe impl Sync for ErasedResource {}

impl ErasedResource {
    /// Move `value` into a fresh erased box.
    pub fn new<T: Resource>(value: T) -> Self {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();
        // SAFETY: the allocation is `size` bytes at `align`, and the write
        // below fills it completely before it is read.
        let data = Self::allocate(size, align, |pointer| unsafe {
            std::ptr::write(pointer.cast::<T>(), value);
        });
        Self {
            data,
            type_id: Some(TypeId::of::<T>()),
            size,
            align,
            // A shared resource is reached from an artifact whose `TypeId` for
            // the type differs, so its identity check has to be layout-based;
            // see `holds`.
            shared_identity: T::shared_name().is_some(),
            ops: ErasedResourceOps::of::<T>(),
        }
    }

    /// Move another language's blittable bytes into a fresh erased box.
    ///
    /// The value has no Rust type: its identity is the shared name its
    /// declaration carried, so `shared_identity` is set and `holds` compares
    /// layout - which is all a foreign declaration can offer. The box owns a
    /// copy of `bytes` and releases nothing on drop, because blittable bytes
    /// own nothing and there is no artifact whose code could go away.
    pub fn new_foreign(bytes: &[u8], align: usize, schema_hash: u64) -> Self {
        let size = bytes.len();
        // SAFETY: the allocation is `bytes.len()` bytes, and the copy fills it
        // completely before it is read.
        let data = Self::allocate(size, align, |pointer| unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, size);
        });
        Self {
            data,
            // No Rust type names this value, which is exactly what the
            // layout-based `holds` branch above is for.
            type_id: None,
            size,
            align,
            shared_identity: true,
            ops: ErasedResourceOps::foreign(size, align, schema_hash),
        }
    }

    /// Allocate a value slot of `size` bytes at `align` and fill it.
    ///
    /// A zero-sized value needs no allocation: `drop_in_place` on a
    /// dangling-but-aligned pointer is well defined for one.
    fn allocate(size: usize, align: usize, fill: impl FnOnce(*mut u8)) -> NonNull<u8> {
        if size == 0 {
            return NonNull::dangling();
        }
        let layout = Layout::from_size_align(size, align).expect("valid resource layout");
        // SAFETY: `layout` has non-zero size, checked above.
        let pointer = unsafe { std::alloc::alloc(layout) };
        let Some(pointer) = NonNull::new(pointer) else {
            std::alloc::handle_alloc_error(layout);
        };
        fill(pointer.as_ptr());
        pointer
    }

    /// Runtime type identity of the stored value, or `None` for a foreign one.
    ///
    /// A foreign resource is a run of bytes with no Rust type anywhere in the
    /// process; its identity is the shared name it declared, reached through
    /// [`ResourceId`] instead.
    pub fn type_id(&self) -> Option<TypeId> {
        self.type_id
    }

    /// Size in bytes of the stored value.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Alignment in bytes of the stored value.
    pub fn align(&self) -> usize {
        self.align
    }

    /// Whether this box holds another language's blittable bytes.
    ///
    /// The answer comes from the box's own creation - a Rust value was stored
    /// with a `TypeId` and a foreign payload without one - never from the
    /// function table, which a re-home can replace. A table cannot redefine
    /// what the value is, which is what keeps this predicate honest after a
    /// mismatched re-home would otherwise have flipped it.
    pub fn is_foreign(&self) -> bool {
        self.type_id.is_none()
    }

    /// The stored value as raw bytes.
    ///
    /// The bytes are what another language reads and writes, and what a
    /// migration plan moves. They only mean anything to a caller that knows the
    /// registered layout: a Rust value's bytes are its fields at their offsets,
    /// and handing them out does not make the value any less the type it is.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the box owns a live byte run of this size, and the borrow is
        // tied to `&self`.
        unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.size) }
    }

    /// The stored value as raw bytes, mutably.
    ///
    /// A caller writing here is responsible for writing a value the stored type
    /// accepts - the same trust the managed mirror path places in its generated
    /// writers, which write component rows in place.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the borrow exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.data.as_ptr(), self.size) }
    }

    /// Rewrite the stored payload into a new shape, following a plan.
    ///
    /// Only a foreign payload may change size: a Rust value's size is its type,
    /// and `World::relayout_foreign_resource` refuses that before it gets here.
    /// The scratch copy is the discipline a descriptor column uses as well -
    /// every source byte is read before the destination is zeroed - so a plan that
    /// moves fields inside the value cannot read what it overwrote.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorRowInvalid`] when an instruction falls
    /// outside the old or the new payload. Nothing is modified in that case.
    pub(crate) fn migrate_bytes(
        &mut self,
        size: usize,
        align: usize,
        plan: &FieldPlan,
    ) -> Result<(), WorldError> {
        plan.validate(self.size, size)?;

        // Read the old payload out first: a plan is free to move a field onto
        // bytes the field it came from still occupies.
        let mut scratch = vec![0_u8; self.size.max(size)];
        // SAFETY: the box holds `self.size` initialized bytes, and the scratch
        // buffer is at least that long.
        unsafe {
            std::ptr::copy_nonoverlapping(self.data.as_ptr(), scratch.as_mut_ptr(), self.size);
        }

        if size != self.size || align != self.align {
            if self.size != 0 {
                let old_layout =
                    Layout::from_size_align(self.size, self.align).expect("valid resource layout");
                // SAFETY: this is the live allocation, created with this layout.
                unsafe { std::alloc::dealloc(self.data.as_ptr(), old_layout) };
            }
            // SAFETY: the allocation is `size` bytes at `align`, and the writes
            // below fill it before anything reads it.
            self.data = Self::allocate(size, align, |_pointer| {});
            self.size = size;
            self.align = align;
        }

        // SAFETY: the destination holds `size` bytes, the plan was checked
        // against both sizes, and the scratch buffer holds the old payload.
        unsafe {
            std::ptr::write_bytes(self.data.as_ptr(), 0, size);
            for field in plan.fields() {
                match field.source {
                    FieldSource::OldOffset(offset) => {
                        std::ptr::copy_nonoverlapping(
                            scratch.as_ptr().add(offset),
                            self.data.as_ptr().add(field.offset),
                            field.bytes,
                        );
                    }
                    // The payload was zeroed above, and a default is checked to
                    // fit its field when it is set.
                    FieldSource::Default { bytes, len } => {
                        std::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            self.data.as_ptr().add(field.offset),
                            usize::from(len),
                        );
                    }
                    FieldSource::ZeroFill => {}
                }
            }
        }

        // The box's own table describes the value it now holds. A foreign box's
        // table is never refreshed from the factories - that is what keeps a
        // Rust type sharing the name from handing its drop to these bytes - so
        // this is the one place it moves.
        self.ops = ErasedResourceOps::foreign(size, align, self.ops.schema_hash.unwrap_or(0));
        Ok(())
    }

    /// Replace the per-type function table.
    ///
    /// Lets the owner re-point the stored drop at code that is still mapped,
    /// after the artifact that supplied it has been reloaded or retired. A
    /// foreign box is not refreshed: see [`Self::is_foreign`] and
    /// `World::rehome_resources`.
    ///
    /// The table has to agree with the box's own ownership; a disagreement
    /// would install a drop the value never had. Debug builds assert it, and
    /// `World::rehome_resources` skips the case in release builds.
    pub fn refresh_ops(&mut self, ops: ErasedResourceOps) {
        // A table that disagrees with the box's ownership would install a drop
        // the value never had, or none where one is needed. `rehome_resources`
        // already filters this case out, so reaching it means a caller got it
        // wrong - refuse the swap and say so rather than corrupting the box.
        // Keeping the existing table is the safe half of the disagreement: the
        // value is still released by whatever produced it.
        if ops.foreign != self.type_id.is_none() {
            error!(
                target: pill_core::telemetry::telemetry_target::ECS,
                foreign_table = ops.foreign,
                foreign_box = self.type_id.is_none(),
                "a resource's function table disagrees with the ownership its box was created with; keeping the existing table"
            );
            return;
        }
        self.ops = ops;
    }

    /// Whether this box is identified by layout rather than by `TypeId`.
    pub fn has_shared_identity(&self) -> bool {
        self.shared_identity
    }

    /// Whether the stored value can be read as a `T`.
    ///
    /// An ordinary resource demands the exact `TypeId` it was stored under,
    /// which is the strict default. A shared one cannot: the whole point is
    /// that another artifact reaches it, and that artifact's `TypeId` for the
    /// same type differs. It compares layout instead, which still rejects an
    /// outright wrong `T` while accepting the other artifact's copy of the
    /// right one.
    ///
    /// Layout alone is weaker than `TypeId` - `{f32, f32}` and `{u32, u32}`
    /// agree on size and alignment - which is why a shared resource's name is
    /// guarded at registration (see `World::register_resource`).
    pub fn holds<T: Resource>(&self) -> bool {
        if self.shared_identity {
            self.size == std::mem::size_of::<T>() && self.align == std::mem::align_of::<T>()
        } else {
            self.type_id == Some(TypeId::of::<T>())
        }
    }

    /// Borrow the stored value as a `T`, or `None` when it is another type.
    pub fn get<T: Resource>(&self) -> Option<&T> {
        if !self.holds::<T>() {
            return None;
        }
        // SAFETY: the check above establishes that the allocation holds a live,
        // correctly aligned `T`.
        Some(unsafe { &*self.data.as_ptr().cast::<T>() })
    }

    /// Borrow the stored value mutably, or `None` when it is another type.
    pub fn get_mut<T: Resource>(&mut self) -> Option<&mut T> {
        if !self.holds::<T>() {
            return None;
        }
        // SAFETY: as for `get`, and `&mut self` makes the borrow exclusive.
        Some(unsafe { &mut *self.data.as_ptr().cast::<T>() })
    }

    /// Move the stored value out, consuming the box.
    ///
    /// Returns the box unchanged when it holds another type, so a refused take
    /// does not destroy the value.
    ///
    /// This is the only path where ownership leaves the box, and the only one
    /// that must **not** run the drop: the value goes to the caller, who
    /// becomes responsible for it.
    pub fn take<T: Resource>(self) -> Result<T, Self> {
        if !self.holds::<T>() {
            return Err(self);
        }
        // The value moves out, so this box must release its allocation without
        // dropping the contents. `ManuallyDrop` suppresses `Drop for
        // ErasedResource`; the deallocation below replaces it.
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: the check above establishes a live `T` at `data`, and
        // `ManuallyDrop` guarantees nothing else reads or drops it.
        let value = unsafe { std::ptr::read(this.data.as_ptr().cast::<T>()) };
        if this.size != 0 {
            let layout =
                Layout::from_size_align(this.size, this.align).expect("valid resource layout");
            // SAFETY: the allocation came from `alloc` with this layout in
            // `new`, and the value has been moved out, so nothing live is freed.
            unsafe { std::alloc::dealloc(this.data.as_ptr(), layout) };
        }
        Ok(value)
    }
}

impl std::fmt::Debug for ErasedResource {
    /// Reports what the box knows about the value, since the value itself is
    /// opaque here.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ErasedResource")
            .field("type_id", &self.type_id)
            .field("size", &self.size)
            .field("align", &self.align)
            .finish_non_exhaustive()
    }
}

impl Drop for ErasedResource {
    fn drop(&mut self) {
        // SAFETY: the box owns a live value of the type its table was built
        // for, and this runs exactly once.
        unsafe { (self.ops.drop_in_place)(self.data.as_ptr()) };
        if self.size != 0 {
            let layout =
                Layout::from_size_align(self.size, self.align).expect("valid resource layout");
            // SAFETY: the allocation came from `alloc` with this layout in
            // `new`, and the value was just dropped.
            unsafe { std::alloc::dealloc(self.data.as_ptr(), layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Score(u32);
    impl Resource for Score {}

    #[derive(Debug, PartialEq)]
    struct ProjectTime {
        delta: f32,
        elapsed: f32,
    }
    impl Resource for ProjectTime {}

    /// Verifies that `ResourceId` stores the correct TypeId internally.
    #[test]
    fn test_resource_id() {
        let id1 = ResourceId::of::<Score>();
        let id2 = ResourceId::of::<Score>();
        let id3 = ResourceId::of::<ProjectTime>();

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    /// Verifies that `ResHandle::new()` creates a handle for the correct type.
    #[test]
    fn test_res_handle_new() {
        let handle = ResHandle::<Score>::new();
        assert_eq!(handle.id(), ResourceId::of::<Score>());
    }

    /// Verifies that `ResHandle::default()` creates a valid zero-cost handle.
    #[test]
    fn test_res_handle_default() {
        let handle = ResHandle::<Score>::default();
        assert_eq!(handle.id(), ResourceId::of::<Score>());
    }

    /// Verifies that `ResHandle` implements Copy and Clone correctly.
    #[test]
    fn test_res_handle_copy_clone() {
        let handle = ResHandle::<Score>::new();
        let handle2 = handle;
        let handle3 = handle;
        // All handles refer to the same resource type
        assert_eq!(handle.id(), handle2.id());
        assert_eq!(handle.id(), handle3.id());
    }

    /// Verifies that `ResHandle` implements Debug for diagnostic output.
    #[test]
    fn test_res_handle_debug() {
        let handle = ResHandle::<Score>::new();
        let debug_str = format!("{:?}", handle);
        assert!(debug_str.contains("ResHandle"));
        assert!(debug_str.contains("Score"));
    }

    /// Verifies that `ResHandle::get()` returns the resource when it exists.
    #[test]
    fn test_res_handle_get() {
        let mut world = World::new();
        let handle = ResHandle::<Score>::new();

        // Resource doesn't exist yet
        assert!(!handle.exists(&world));
        assert!(handle.get(&world).is_none());

        // Insert resource
        world.insert_resource(Score(42));

        // Now accessible via handle
        assert!(handle.exists(&world));
        assert_eq!(handle.get(&world).unwrap().0, 42);
    }

    /// Verifies that `ResHandle::get_mut()` allows mutable access to an existing resource.
    #[test]
    fn test_res_handle_get_mut() {
        let mut world = World::new();
        let handle = ResHandle::<Score>::new();

        world.insert_resource(Score(0));

        // Mutate via handle
        handle.get_mut(&mut world).unwrap().0 += 10;
        assert_eq!(handle.get(&world).unwrap().0, 10);

        handle.get_mut(&mut world).unwrap().0 += 5;
        assert_eq!(handle.get(&world).unwrap().0, 15);
    }

    /// Verifies that `ResHandle::get()` returns None for a missing resource.
    #[test]
    fn test_res_handle_missing_resource() {
        let world = World::new();
        let handle = ResHandle::<Score>::new();

        assert!(!handle.exists(&world));
        assert!(handle.get(&world).is_none());
    }

    /// Verifies that handles for different resource types are independent.
    #[test]
    fn test_multiple_handles_different_types() {
        let mut world = World::new();

        let score_handle = ResHandle::<Score>::new();
        let time_handle = ResHandle::<ProjectTime>::new();

        world.insert_resource(Score(100));
        world.insert_resource(ProjectTime {
            delta: 0.016,
            elapsed: 0.0,
        });

        assert_eq!(score_handle.get(&world).unwrap().0, 100);
        assert_eq!(time_handle.get(&world).unwrap().delta, 0.016);

        // Different types, different IDs
        assert_ne!(score_handle.id(), time_handle.id());
    }

    // -------------------------------------------------------------------------
    // ErasedResource
    // -------------------------------------------------------------------------

    /// Counts its own drops, so a double drop or a leak is visible rather than
    /// merely suspected.
    ///
    /// The counter is owned per instance rather than being a `static`: the
    /// harness runs these tests in parallel, and a shared counter makes every
    /// `before + 1` assertion race with the others.
    #[derive(Debug)]
    struct Tracked {
        value: u32,
        drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Resource for Tracked {}

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A fresh `Tracked` and the counter watching it.
    fn tracked(value: u32) -> (Tracked, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Tracked {
                value,
                drops: std::sync::Arc::clone(&drops),
            },
            drops,
        )
    }

    /// Reads one probe's counter.
    fn drops_of(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A second resource type with the same layout as [`Tracked`], to show the
    /// type check is not merely a size comparison.
    #[derive(Debug)]
    struct Lookalike {
        _value: u32,
    }
    impl Resource for Lookalike {}

    /// A zero-sized resource, which takes the no-allocation path.
    #[derive(Debug)]
    struct ZeroSized;
    impl Resource for ZeroSized {}

    /// A value survives the round trip into the box and back out by reference.
    #[test]
    fn an_erased_value_can_be_stored_and_borrowed() {
        let mut erased = ErasedResource::new(tracked(7).0);

        assert_eq!(erased.get::<Tracked>().unwrap().value, 7);
        erased.get_mut::<Tracked>().unwrap().value = 9;
        assert_eq!(erased.get::<Tracked>().unwrap().value, 9);
    }

    /// The box holds one type and says so; a same-sized other type is refused.
    #[test]
    fn an_erased_box_refuses_another_type_with_an_identical_layout() {
        let mut erased = ErasedResource::new(tracked(1).0);

        assert!(erased.holds::<Tracked>());
        assert!(!erased.holds::<Lookalike>());
        assert!(erased.get::<Lookalike>().is_none());
        assert!(erased.get_mut::<Lookalike>().is_none());
    }

    /// Dropping the box drops the value exactly once.
    #[test]
    fn dropping_an_erased_box_drops_the_value_once() {
        let (probe, drops) = tracked(1);
        drop(ErasedResource::new(probe));
        assert_eq!(drops_of(&drops), 1);
    }

    /// Taking the value out moves it to the caller and must NOT also drop it.
    ///
    /// The delicate path: the box has to release its allocation without running
    /// the drop, or the value is dropped twice.
    #[test]
    fn taking_an_erased_value_out_moves_it_exactly_once() {
        let (probe, drops) = tracked(42);
        let erased = ErasedResource::new(probe);

        let taken = erased.take::<Tracked>().expect("the box holds a Tracked");
        assert_eq!(taken.value, 42);
        assert_eq!(
            drops_of(&drops),
            0,
            "the value is alive in the caller's hands; nothing dropped yet"
        );

        drop(taken);
        assert_eq!(
            drops_of(&drops),
            1,
            "the value must be dropped exactly once, by its new owner"
        );
    }

    /// A refused take gives the box back rather than destroying the value.
    #[test]
    fn a_refused_take_returns_the_erased_box_intact() {
        let (probe, drops) = tracked(3);
        let erased = ErasedResource::new(probe);

        let returned = erased
            .take::<Lookalike>()
            .expect_err("the box does not hold a Lookalike");
        assert_eq!(drops_of(&drops), 0, "a refused take must not drop anything");
        assert_eq!(returned.get::<Tracked>().unwrap().value, 3);

        drop(returned);
        assert_eq!(drops_of(&drops), 1);
    }

    /// A zero-sized resource takes the no-allocation path and still round trips.
    #[test]
    fn a_zero_sized_erased_resource_round_trips() {
        let erased = ErasedResource::new(ZeroSized);
        assert_eq!(erased.size(), 0);
        assert!(erased.holds::<ZeroSized>());
        erased.take::<ZeroSized>().expect("a ZeroSized comes back");
    }

    /// The box reports the stored type's layout, which is what re-homing and
    /// the identity checks are built on.
    #[test]
    fn an_erased_box_reports_the_stored_types_layout() {
        let erased = ErasedResource::new(tracked(0).0);
        assert_eq!(erased.size(), std::mem::size_of::<Tracked>());
        assert_eq!(erased.align(), std::mem::align_of::<Tracked>());
        assert_eq!(erased.type_id(), Some(TypeId::of::<Tracked>()));
    }

    /// Replacing the function table leaves the stored value alone, which is the
    /// whole point of re-homing across a reload.
    #[test]
    fn refreshing_an_erased_boxs_ops_leaves_the_value_alone() {
        let (probe, drops) = tracked(11);
        let mut erased = ErasedResource::new(probe);

        erased.refresh_ops(ErasedResourceOps::of::<Tracked>());

        assert_eq!(erased.get::<Tracked>().unwrap().value, 11);
        assert_eq!(drops_of(&drops), 0, "nothing was dropped");

        drop(erased);
        assert_eq!(drops_of(&drops), 1);
    }
}

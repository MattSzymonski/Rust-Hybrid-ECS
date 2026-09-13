//! Engine-owned native dynamic buffer.
//!
//! # Responsibilities
//!
//! - Provide [`DynamicBuffer<T>`], a growable run of plain (`Copy`) elements
//!   whose memory is owned by the engine's shared native allocation service
//!   instead of by a `Vec` on the heap of whichever DLL created it.
//! - Give Rust systems and generated C# mirrors the same live view of the
//!   data: a `#[repr(C)]` `(ptr, len, cap)` handle that managed code reads
//!   straight out of the component row.
//!
//! # Design
//!
//! ## Why not a `Vec` field
//!
//! The archetype copier is `Clone`-based: moving an entity between archetypes
//! deep-copies every component it keeps. A `Vec` field therefore reallocates
//! and copies its whole heap on every structural change, and the copy's
//! address has no stability a managed view could rely on - the source block is
//! freed as soon as the moved-from slot is dropped. A C#-visible `(ptr, len)`
//! read from such a field would be exactly the retained raw pointer the
//! interop rule forbids.
//!
//! ## Reference-counted handles, copy-on-write
//!
//! `DynamicBuffer` keeps its elements in a block owned by
//! [`pill_core::native_buffer`]: memory whose address never changes while the
//! block is alive, and whose free path is the shared allocation service rather
//! than whatever code happens to be current. `Clone` *retains* the block - the
//! clone shares it, address and all - so an archetype move copies the handle
//! and never the elements. Mutation while a block is shared copies it first,
//! so two handles never write through one another.
//!
//! ## The address-stability contract
//!
//! Callers may rely on all of the following; the internals exist to make them
//! true:
//!
//! - **A block's address is stable for as long as any handle to it exists.**
//!   Growth allocates a new block (and releases the old reference); it never
//!   relocates a live one, and nothing else moves elements at all.
//! - **Archetype migration moves the handle, never the block.** The copier's
//!   `Clone` is a reference count, so a component's elements stay exactly
//!   where they were across structural changes.
//! - **Resizing is the one operation that can retire a block**, and it is
//!   therefore the operation that ends any outstanding element view: take a
//!   view, use it, drop it before resizing. (Views taken through one handle
//!   remain valid while shared handles exist, because the block stays alive.)
//! - **A view never outlives the frame it was taken in.** Handles are
//!   re-read from the row each frame; nothing caches an element pointer
//!   across a reload or a frame boundary.
//!
//! Element writes through one handle while another exists are safe by
//! construction - the writer copies first - but they are not *observed* by a
//! `&[T]` borrowed from the other handle before that copy, exactly as with
//! `Cow::to_mut` or `Arc::make_mut`.

use std::mem::size_of;
use std::ops::{Deref, DerefMut};

use pill_core::native_buffer;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// =============================================================================
// DynamicBuffer
// =============================================================================

/// A growable run of plain elements living in engine-owned native memory.
///
/// The handle is `#[repr(C)]` with the words `(ptr, len, cap)` in that order:
/// the C# mirror reads them directly out of the component row, so managed code
/// iterates the live elements with zero boundary calls, and the editor reads
/// the count the same way.
///
/// `T: Copy` is a hard requirement, not a convenience: elements are memcpy'd
/// between blocks and the last release frees the block without running drop
/// glue, so a type owning anything would leak. Types with a destructor do not
/// compile into a buffer at all.
///
/// See the module documentation for the address-stability contract this type
/// upholds.
#[repr(C)]
pub struct DynamicBuffer<T: Copy> {
    /// Element area of the block this handle owns a reference to; null while
    /// the buffer is empty.
    ptr: *mut T,
    /// Initialised elements at `ptr`.
    len: usize,
    /// Elements the block can hold before it must grow.
    cap: usize,
}

impl<T: Copy> DynamicBuffer<T> {
    /// Alignment every block for this element type is allocated with.
    const ELEMENT_ALIGN: usize = std::mem::align_of::<T>();

    /// An empty buffer that owns no block.
    pub const fn new() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }

    /// An empty buffer with room for `capacity` elements before the first
    /// growth.
    pub fn with_capacity(capacity: usize) -> Self {
        if capacity == 0 {
            return Self::new();
        }
        assert!(
            size_of::<T>() > 0,
            "a DynamicBuffer element type must have a non-zero size"
        );
        // SAFETY: `capacity` is non-zero and the element size is non-zero, so
        // the block's byte capacity is non-zero; the returned pointer owns the
        // block's single reference, which this handle stores.
        let elements =
            unsafe { native_buffer::allocate(Self::element_bytes(capacity), Self::ELEMENT_ALIGN) };
        Self {
            ptr: elements.cast::<T>(),
            len: 0,
            cap: capacity,
        }
    }

    /// A buffer holding a copy of `values`.
    pub fn from_slice(values: &[T]) -> Self {
        let mut buffer = Self::with_capacity(values.len());
        buffer.extend_from_slice(values);
        buffer
    }

    /// Number of initialised elements.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer holds no elements.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Elements the block can hold before it must grow.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Pointer to the first element, for FFI trampolines and mirror views.
    ///
    /// Null while the buffer is empty; a caller that hands this to managed
    /// code must pair it with [`Self::len`] and treat a zero length as no
    /// view.
    pub fn as_ptr(&self) -> *const T {
        self.ptr
    }

    /// Writable pointer to the first element, copying the block first when it
    /// is shared.
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.ensure_unique();
        self.ptr
    }

    /// The elements, borrowed immutably.
    pub fn as_slice(&self) -> &[T] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `len` initialised elements follow `ptr` (len <= cap for the
        // whole lifetime of the block) and no `&mut` to them exists while this
        // shared borrow is alive.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// The elements, borrowed mutably.
    ///
    /// Copies the block first when another handle shares it, so a buffer is
    /// never observed changing under a reader.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.ensure_unique();
        if self.len == 0 {
            return &mut [];
        }
        // SAFETY: `len` initialised elements follow `ptr`, `ensure_unique`
        // left this handle the only referent, and the exclusive borrow lasts
        // as long as the returned slice.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Append one element, growing (or copying a shared block) as needed.
    pub fn push(&mut self, value: T) {
        if self.len == self.cap {
            self.grow_to(self.len + 1);
        } else {
            self.ensure_unique();
        }
        // SAFETY: the capacity now exceeds `len`, so `ptr + len` is inside the
        // block (and non-null), and the slot is uninitialised, which writing
        // over is fine.
        unsafe { self.ptr.add(self.len).write(value) };
        self.len += 1;
    }

    /// Append every element of `values`.
    pub fn extend_from_slice(&mut self, values: &[T]) {
        if values.is_empty() {
            return;
        }
        if self.len + values.len() > self.cap {
            self.grow_to(self.len + values.len());
        } else {
            self.ensure_unique();
        }
        // SAFETY: the capacity covers `len + values.len()`, so the destination
        // range is inside the block; `values` borrows a different allocation,
        // so the ranges cannot overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                values.as_ptr(),
                self.ptr.add(self.len),
                values.len(),
            );
        }
        self.len += values.len();
    }

    /// Rewrite the buffer to `new_len` elements.
    ///
    /// Growing fills the new elements with `value`; shrinking keeps the first
    /// `new_len`. Either direction may retire the current block (it is always
    /// retired when it is shared), which is what ends any view taken from it.
    pub fn resize(&mut self, new_len: usize, value: T) {
        if new_len > self.cap {
            self.grow_to(new_len);
        } else if new_len > self.len {
            self.ensure_unique();
        }
        for index in self.len..new_len {
            // SAFETY: `capacity >= new_len`, so every index in
            // `len..new_len` is inside the block; the slots are uninitialised,
            // which writing over is fine. A write path above made this handle
            // the only referent when anything was shared.
            unsafe { self.ptr.add(index).write(value) };
        }
        self.len = new_len;
    }

    /// Drop every element, keeping the block for reuse.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Keep only the first `new_len` elements.
    pub fn truncate(&mut self, new_len: usize) {
        if new_len < self.len {
            self.len = new_len;
        }
    }

    /// Ensure room for `additional` more elements.
    pub fn reserve(&mut self, additional: usize) {
        let wanted = self.len.saturating_add(additional);
        if wanted > self.cap {
            self.grow_to(wanted);
        }
    }

    /// Byte capacity of a block holding `capacity` elements.
    fn element_bytes(capacity: usize) -> usize {
        capacity * size_of::<T>()
    }

    /// Grow to hold at least `wanted` elements, keeping the existing ones.
    ///
    /// Always allocates a fresh block and copies: that is correct whether the
    /// current block is shared (the other referent keeps it) or not (the old
    /// reference is dropped and the block freed), and it never relocates a
    /// live block, which is what keeps the address-stability contract.
    fn grow_to(&mut self, wanted: usize) {
        assert!(
            size_of::<T>() > 0,
            "a DynamicBuffer element type must have a non-zero size"
        );
        // Doubling keeps repeated pushes amortised; small buffers start at
        // four elements so a push does not allocate for one.
        let new_capacity = wanted.max(self.cap.saturating_mul(2)).max(4);
        // SAFETY: the capacity is non-zero and the element size is non-zero,
        // so the allocation request is valid; the returned pointer owns the
        // new block's single reference.
        let new_ptr = unsafe {
            native_buffer::allocate(Self::element_bytes(new_capacity), Self::ELEMENT_ALIGN)
        }
        .cast::<T>();

        if self.len > 0 {
            // SAFETY: both blocks hold `len` initialised elements of the same
            // type, the source is alive (this handle holds a reference to it),
            // and the destination was just allocated, so they cannot overlap.
            unsafe { std::ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len) };
        }
        if !self.ptr.is_null() {
            // SAFETY: this handle owns one reference to the old block, whose
            // byte capacity matches the stored capacity and alignment; giving
            // the reference up leaves the block alive for any other referent.
            unsafe {
                native_buffer::release(
                    self.ptr.cast::<u8>(),
                    Self::element_bytes(self.cap),
                    Self::ELEMENT_ALIGN,
                );
            }
        }
        self.ptr = new_ptr;
        self.cap = new_capacity;
    }

    /// Make this handle the only referent before writing through it.
    fn ensure_unique(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // SAFETY: the pointer addresses a live block this handle holds a
        // reference to, allocated with this element alignment.
        if unsafe { native_buffer::reference_count(self.ptr.cast::<u8>(), Self::ELEMENT_ALIGN) } <= 1
        {
            return;
        }
        let bytes = Self::element_bytes(self.cap);
        // SAFETY: the capacity is non-zero (a block exists) and the element
        // size is non-zero, so the allocation request is valid.
        let new_ptr =
            unsafe { native_buffer::allocate(bytes, Self::ELEMENT_ALIGN) }.cast::<T>();
        if self.len > 0 {
            // SAFETY: as in `grow_to`: same element type, `len <= cap`
            // initialised elements on both sides, distinct allocations.
            unsafe { std::ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len) };
        }
        // SAFETY: this handle owns one reference to the shared block; the
        // reference count was above one, so the block survives for the other
        // referents and the stored capacity still describes it.
        unsafe {
            native_buffer::release(self.ptr.cast::<u8>(), bytes, Self::ELEMENT_ALIGN);
        }
        self.ptr = new_ptr;
    }
}

// =============================================================================
// Trait Impls
// =============================================================================

impl<T: Copy> Default for DynamicBuffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Cloning a buffer retains its block instead of copying it.
///
/// This is what makes an archetype move copy the handle and not the elements:
/// both the moved-from and the moved-to component refer to the same block
/// until one of them writes, at which point that one copies first.
impl<T: Copy> Clone for DynamicBuffer<T> {
    fn clone(&self) -> Self {
        if !self.ptr.is_null() {
            // SAFETY: the pointer addresses a live block this handle holds a
            // reference to; the clone takes ownership of the new reference.
            unsafe { native_buffer::retain(self.ptr.cast::<u8>(), Self::ELEMENT_ALIGN) };
        }
        Self {
            ptr: self.ptr,
            len: self.len,
            cap: self.cap,
        }
    }
}

impl<T: Copy> Drop for DynamicBuffer<T> {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // SAFETY: this handle owns exactly one reference to the block, whose
        // byte capacity matches the stored capacity and alignment. Elements
        // are `Copy`, so there is no drop glue to run before the block goes.
        unsafe {
            native_buffer::release(
                self.ptr.cast::<u8>(),
                Self::element_bytes(self.cap),
                Self::ELEMENT_ALIGN,
            );
        }
    }
}

// SAFETY: the handle owns one reference to a block of plain `Copy` elements
// and holds no thread-affine state; reference counts are atomic and every
// write path copies a shared block first, so moving the handle to another
// thread cannot race another referent's elements.
unsafe impl<T: Copy + Send> Send for DynamicBuffer<T> {}

// SAFETY: as above; sharing `&DynamicBuffer` hands out element reads only,
// which `T: Sync` makes safe across threads.
unsafe impl<T: Copy + Sync> Sync for DynamicBuffer<T> {}

impl<T: Copy> Deref for DynamicBuffer<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Copy> DerefMut for DynamicBuffer<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: Copy + std::fmt::Debug> std::fmt::Debug for DynamicBuffer<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.as_slice().iter()).finish()
    }
}

impl<T: Copy + PartialEq> PartialEq for DynamicBuffer<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Copy + Eq> Eq for DynamicBuffer<T> {}

/// Serializes as the sequence of its live elements, so a buffer round-trips
/// through the same shape a `Vec<T>` would and a schema change migrates it
/// through the ordinary persistable machinery.
impl<T: Copy + Serialize> Serialize for DynamicBuffer<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.as_slice().serialize(serializer)
    }
}

/// Deserializes a sequence into a freshly allocated block; nothing about an
/// address is ever carried across a reload.
impl<'de, T: Copy + Deserialize<'de>> Deserialize<'de> for DynamicBuffer<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values: Vec<T> = Vec::deserialize(deserializer)?;
        Ok(Self::from_slice(&values))
    }
}

// =============================================================================
// Tests
// =============================================================================

/// Unit tests for the handle's contract: pinned layout, reference-counted
/// clone, copy-on-write mutation, balanced block accounting, and serde.
///
/// The accounting statics are process-wide, so the tests that assert on them
/// take turns through a guard.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that assert on the shared block accounting.
    static ACCOUNTING_GUARD: Mutex<()> = Mutex::new(());

    fn guarded() -> std::sync::MutexGuard<'static, ()> {
        ACCOUNTING_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The mirror and the C# side both depend on the handle being exactly
    /// `(ptr, len, cap)` in that order, pointer-sized and pointer-aligned.
    #[test]
    fn handle_layout_is_pinned() {
        use std::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<DynamicBuffer<f32>>(), 3 * size_of::<usize>());
        assert_eq!(align_of::<DynamicBuffer<f32>>(), align_of::<usize>());
        assert_eq!(offset_of!(DynamicBuffer<f32>, ptr), 0);
        assert_eq!(offset_of!(DynamicBuffer<f32>, len), size_of::<usize>());
        assert_eq!(offset_of!(DynamicBuffer<f32>, cap), 2 * size_of::<usize>());
    }

    /// Pushes grow through the doubling policy and keep every element.
    #[test]
    fn push_and_resize_keep_contents() {
        let _guard = guarded();
        let bytes_before = native_buffer::live_bytes();

        let mut buffer = DynamicBuffer::<f32>::new();
        assert!(buffer.is_empty());
        for value in 0..10 {
            buffer.push(value as f32);
        }
        assert_eq!(buffer.len(), 10);
        assert!(buffer.capacity() >= 10);
        assert_eq!(buffer.as_slice(), (0..10).map(|v| v as f32).collect::<Vec<_>>().as_slice());

        buffer.resize(12, -1.0);
        assert_eq!(buffer.len(), 12);
        assert_eq!(buffer.as_slice()[10], -1.0);
        buffer.resize(2, 0.0);
        assert_eq!(buffer.as_slice(), &[0.0, 1.0]);

        drop(buffer);
        assert_eq!(native_buffer::live_bytes(), bytes_before);
    }

    /// A clone shares the block, and writing through either handle copies
    /// first so neither observes the other changing - the property an
    /// archetype move relies on.
    #[test]
    fn clone_shares_the_block_until_a_write() {
        let _guard = guarded();
        let bytes_before = native_buffer::live_bytes();

        let original = DynamicBuffer::from_slice(&[1u32, 2, 3]);
        let mut clone = original.clone();
        // Address stability: the clone is a retained reference, not a copy.
        assert_eq!(original.as_ptr(), clone.as_ptr());
        assert_eq!(
            // SAFETY: the block is live and both handles hold references to it.
            unsafe { native_buffer::reference_count(original.as_ptr().cast_mut().cast::<u8>(), 4) },
            2
        );

        clone.push(4);
        assert_ne!(original.as_ptr(), clone.as_ptr());
        assert_eq!(original.as_slice(), &[1, 2, 3]);
        assert_eq!(clone.as_slice(), &[1, 2, 3, 4]);

        drop(clone);
        drop(original);
        assert_eq!(native_buffer::live_bytes(), bytes_before);
    }

    /// A resize that only shrinks writes nothing, so a shared block stays
    /// shared.
    #[test]
    fn shrinking_resize_keeps_a_shared_block() {
        let _guard = guarded();

        let buffer = DynamicBuffer::from_slice(&[1i64, 2, 3, 4]);
        let mut shared = buffer.clone();
        let address = buffer.as_ptr();

        shared.truncate(2);
        assert_eq!(buffer.as_slice(), &[1, 2, 3, 4]);
        assert_eq!(shared.as_slice(), &[1, 2]);
        assert_eq!(buffer.as_ptr(), address);
        assert_eq!(shared.as_ptr(), address);

        drop(shared);
        drop(buffer);
    }

    /// Elements round-trip through serde, which is what carries buffer
    /// contents across a reload whose schema changed.
    #[test]
    fn serde_round_trip_preserves_elements() {
        let _guard = guarded();
        let bytes_before = native_buffer::live_bytes();

        let buffer = DynamicBuffer::from_slice(&[2.5f64, -1.0, 42.0]);
        let json = serde_json::to_string(&buffer).expect("serialize");
        assert_eq!(json, "[2.5,-1.0,42.0]");
        let restored: DynamicBuffer<f64> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.as_slice(), buffer.as_slice());

        let empty = DynamicBuffer::<u8>::new();
        let json = serde_json::to_string(&empty).expect("serialize empty");
        assert_eq!(json, "[]");
        let restored_empty: DynamicBuffer<u8> =
            serde_json::from_str(&json).expect("deserialize empty");
        assert!(restored_empty.is_empty());

        drop(restored);
        drop(restored_empty);
        drop(empty);
        drop(buffer);
        assert_eq!(native_buffer::live_bytes(), bytes_before);
    }

    /// The free path frees exactly once: dropping a buffer returns its bytes,
    /// and a retained reference keeps the block until the last drop.
    #[test]
    fn drops_release_blocks_exactly_once() {
        let _guard = guarded();
        let bytes_before = native_buffer::live_bytes();

        let buffer = DynamicBuffer::from_slice(&[0u8; 32]);
        let shared = buffer.clone();
        let held = native_buffer::live_bytes();
        drop(shared);
        assert_eq!(
            native_buffer::live_bytes(),
            held,
            "the block must outlive the first handle"
        );
        drop(buffer);
        assert_eq!(native_buffer::live_bytes(), bytes_before);
    }
}

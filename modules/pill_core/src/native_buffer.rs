//! Engine-owned native buffer allocation.
//!
//! # Responsibilities
//!
//! - Allocate, retain and release the element blocks behind `pill_engine`'s
//!   `DynamicBuffer<T>` handles.
//! - Keep live block accounting observable, so a leaked or double-freed block
//!   is a failing assertion rather than a mystery.
//!
//! # Design
//!
//! One block is one allocation: a reference-count header first, then the
//! element area. The returned pointer addresses the *elements*, so a handle
//! can be stored - and mirrored to C# - as plain `(ptr, len, cap)` words,
//! while the count lives in a place no handle can see or corrupt.
//!
//! This service lives in `pill_core` (the dylib shared by the host and every
//! loaded artifact) rather than in `pill_engine` (an rlib compiled into each
//! of them) on purpose: a block's lifetime then never depends on which DLL
//! allocated it, and a release executed by a reloaded generation runs the
//! same instance the original allocation did.
//!
//! # Address-stability contract
//!
//! The service guarantees, and callers may rely on:
//!
//! - a block's element pointer never moves while it is alive - growth
//!   allocates a *new* block rather than relocating an existing one;
//! - `retain`/`release` only move a count, so a block's address survives the
//!   handle being cloned (an archetype move copies the handle; it cannot move
//!   or copy the block);
//! - the block is freed exactly when the last reference is released, so an
//!   outstanding borrow seen through one handle stays valid as long as any
//!   handle to it exists.

use std::alloc::Layout;
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};

// =============================================================================
// Accounting
// =============================================================================

/// Element bytes currently held by live blocks, process-wide.
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Live blocks, process-wide.
static LIVE_BLOCKS: AtomicUsize = AtomicUsize::new(0);

/// Total element bytes currently held by live blocks.
///
/// Header bytes are excluded, so the number matches what the `(len, cap)`
/// words of the handles add up to. Useful for diagnostics and for tests that
/// assert a world releases everything it allocated.
pub fn live_bytes() -> usize {
    LIVE_BYTES.load(Ordering::Relaxed)
}

/// Number of live blocks.
pub fn live_blocks() -> usize {
    LIVE_BLOCKS.load(Ordering::Relaxed)
}

// =============================================================================
// Blocks
// =============================================================================

/// Distance in bytes between a block's elements and its reference-count
/// header.
///
/// At least one `usize`, so the header is aligned and the element area starts
/// at a multiple of the element alignment.
#[inline]
pub const fn header_size(element_align: usize) -> usize {
    if element_align > size_of::<usize>() {
        element_align
    } else {
        size_of::<usize>()
    }
}

/// Allocate one block of `byte_capacity` element bytes with the given element
/// alignment, returning a pointer to its uninitialised element area.
///
/// # Panics
///
/// Panics when `byte_capacity` is zero (an empty buffer has no block) or the
/// allocation request cannot be laid out.
///
/// # Safety
///
/// The returned pointer owns one reference. It must eventually be handed to
/// [`release`] exactly once - through whichever handle owns it - after every
/// [`retain`] taken from it has been balanced.
pub unsafe fn allocate(byte_capacity: usize, element_align: usize) -> *mut u8 {
    assert!(byte_capacity > 0, "an empty block has no allocation");
    assert!(
        element_align.is_power_of_two(),
        "element alignment must be a power of two"
    );
    let align = element_align.max(std::mem::align_of::<usize>());
    let layout = Layout::from_size_align(header_size(element_align) + byte_capacity, align)
        .expect("native buffer layout is valid");
    // SAFETY: `layout` has non-zero size (the header alone is at least one
    // word), which is the one precondition `alloc` imposes.
    let base = unsafe { std::alloc::alloc(layout) };
    if base.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: `base` addresses `header_size + byte_capacity` writable bytes at
    // an alignment of at least `align_of::<usize>()`, so an `AtomicUsize`
    // fits at offset zero.
    unsafe {
        base.cast::<AtomicUsize>().write(AtomicUsize::new(1));
    }
    LIVE_BYTES.fetch_add(byte_capacity, Ordering::Relaxed);
    LIVE_BLOCKS.fetch_add(1, Ordering::Relaxed);

    // SAFETY: the element area starts inside the same allocation and
    // `header_size` is a multiple of `element_align`, so the returned pointer
    // is aligned for the elements the caller is about to write.
    unsafe { base.add(header_size(element_align)) }
}

/// Take one additional reference to a block's elements.
///
/// # Safety
///
/// `elements` must point at the element area of a live block allocated by
/// [`allocate`] with the same `element_align`; the caller takes ownership of
/// the new reference and must balance it with one [`release`].
pub unsafe fn retain(elements: *mut u8, element_align: usize) {
    // SAFETY: the caller guarantees `elements` belongs to a live block, so the
    // header ahead of it is a live, initialised `AtomicUsize`.
    let header = unsafe {
        elements
            .sub(header_size(element_align))
            .cast::<AtomicUsize>()
    };
    // SAFETY: same guarantee as above.
    unsafe { (*header).fetch_add(1, Ordering::Relaxed) };
}

/// Drop one reference to a block's elements, freeing the block when the last
/// reference goes away.
///
/// # Safety
///
/// `elements` must point at the element area of a live block allocated by
/// [`allocate`] with the same `byte_capacity` and `element_align`, and this
/// call must balance exactly one earlier [`allocate`] or [`retain`]. After the
/// call the pointer must not be used unless another reference is still held.
pub unsafe fn release(elements: *mut u8, byte_capacity: usize, element_align: usize) {
    // SAFETY: the caller guarantees `elements` belongs to a live block, so the
    // header ahead of it is a live, initialised `AtomicUsize`.
    let header = unsafe {
        elements
            .sub(header_size(element_align))
            .cast::<AtomicUsize>()
    };
    // SAFETY: same guarantee as above; `AcqRel` pairs the count with every
    // other retain/release so the last releaser sees all prior writes.
    let previous = unsafe { (*header).fetch_sub(1, Ordering::AcqRel) };
    debug_assert!(
        previous > 0,
        "a native buffer was released more times than it was retained"
    );
    if previous != 1 {
        return;
    }

    let align = element_align.max(std::mem::align_of::<usize>());
    let layout = Layout::from_size_align(header_size(element_align) + byte_capacity, align)
        .expect("native buffer layout is valid");
    // SAFETY: the caller guarantees `elements` belongs to a block allocated
    // with exactly this layout, and the count above just reached zero, so this
    // is the only remaining reference and no new one can be taken.
    unsafe { std::alloc::dealloc(elements.sub(header_size(element_align)), layout) };
    LIVE_BYTES.fetch_sub(byte_capacity, Ordering::Relaxed);
    LIVE_BLOCKS.fetch_sub(1, Ordering::Relaxed);
}

/// How many references the block behind `elements` currently holds.
///
/// # Safety
///
/// `elements` must point at the element area of a live block allocated by
/// [`allocate`] with the same `element_align`.
pub unsafe fn reference_count(elements: *mut u8, element_align: usize) -> usize {
    // SAFETY: the caller guarantees `elements` belongs to a live block, so the
    // header ahead of it is a live, initialised `AtomicUsize`.
    let header = unsafe {
        elements
            .sub(header_size(element_align))
            .cast::<AtomicUsize>()
    };
    // SAFETY: same guarantee as above.
    unsafe { (*header).load(Ordering::Acquire) }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The accounting statics are process-wide, so tests that assert on them
    /// take turns.
    static ACCOUNTING_GUARD: Mutex<()> = Mutex::new(());

    fn guarded() -> std::sync::MutexGuard<'static, ()> {
        ACCOUNTING_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The header is at least one word and always a multiple of the element
    /// alignment, which is what keeps the element area aligned.
    #[test]
    fn header_size_keeps_elements_aligned() {
        assert_eq!(header_size(1), size_of::<usize>());
        assert_eq!(header_size(4), size_of::<usize>());
        assert_eq!(header_size(8), size_of::<usize>());
        assert_eq!(header_size(16), 16);
        for align in [1usize, 2, 4, 8, 16] {
            assert_eq!(header_size(align) % align, 0);
        }
    }

    /// A block reports the alignment it was allocated with and counts as one
    /// live block until released.
    #[test]
    fn allocate_reports_aligned_elements_and_accounting() {
        let _guard = guarded();
        let bytes_before = live_bytes();
        let blocks_before = live_blocks();

        // SAFETY: the pointer owns the block's single reference and is
        // released exactly once below.
        let elements = unsafe { allocate(64, 16) };
        assert_eq!(elements as usize % 16, 0);
        // SAFETY: the block is live and we hold its reference.
        assert_eq!(unsafe { reference_count(elements, 16) }, 1);
        assert_eq!(live_bytes(), bytes_before + 64);
        assert_eq!(live_blocks(), blocks_before + 1);

        // SAFETY: balances the allocate above.
        unsafe { release(elements, 64, 16) };
        assert_eq!(live_bytes(), bytes_before);
        assert_eq!(live_blocks(), blocks_before);
    }

    /// A retained reference keeps the block alive until both references are
    /// released, and the address is identical for both.
    #[test]
    fn retain_keeps_the_block_alive_until_the_last_release() {
        let _guard = guarded();
        let bytes_before = live_bytes();

        // SAFETY: the pointer owns the block's first reference.
        let elements = unsafe { allocate(32, 4) };
        // SAFETY: the block is live; the retained reference is released below.
        unsafe { retain(elements, 4) };

        // SAFETY: the block is live and now holds two references.
        assert_eq!(unsafe { reference_count(elements, 4) }, 2);

        // SAFETY: balances the first reference; the block stays alive because
        // the retained one is still outstanding.
        unsafe { release(elements, 32, 4) };
        assert_eq!(live_bytes(), bytes_before + 32);
        // SAFETY: the block is still live through the second reference.
        assert_eq!(unsafe { reference_count(elements, 4) }, 1);

        // SAFETY: balances the retain above; this is the last reference.
        unsafe { release(elements, 32, 4) };
        assert_eq!(live_bytes(), bytes_before);
    }
}

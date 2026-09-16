//! Integration tests for the dynamic-component layout witness.
//!
//! A typeless dynamic component is stored as raw bytes: `DynamicColumn` copies
//! rows with `ptr::copy`, frees them without running element destructors, and
//! is `Send + Sync` on the strength of the `Blittability` its layout carries.
//! These tests pin the shapes the engine must refuse, and pin that the refusal
//! is a typed error rather than a debug-only assertion or a panic deep inside
//! the first growth.
//!
//! # Responsibilities
//!
//! - Pin the layout shapes the engine must refuse, as typed errors.
//! - Keep the refusals at registration rather than at first growth.

use pill_engine::archetype::{Blittability, ComponentColumn, ComponentLayout};
use pill_engine::world::WorldError;

/// A layout the constructor refuses is refused with the error that names the
/// check; a layout built around the constructor is refused by the column.
#[test]
fn degenerate_layouts_are_typed_errors() {
    // SAFETY: every layout below would describe at most one row of a plain
    // value type; only the size and alignment are degenerate, which is what
    // the checks under test refuse.
    let witness = unsafe { Blittability::assume() };
    assert!(matches!(
        ComponentLayout::new(0, 4, 1, witness),
        Err(WorldError::DescriptorSizeZero)
    ));
    assert!(matches!(
        ComponentLayout::new(4, 3, 1, witness),
        Err(WorldError::DescriptorAlignmentInvalid)
    ));
    assert!(matches!(
        ComponentLayout::new(usize::MAX, 1, 1, witness),
        Err(WorldError::DescriptorLayoutInvalid)
    ));

    // The fields are public, so a column can be handed a layout the
    // constructor would have refused. It validates again, in every build
    // profile: a debug-only assertion used to leave release builds accepting
    // these until the first push ran off the end of the buffer.
    let zero_sized = ComponentLayout {
        size: 0,
        align: 4,
        schema_hash: 1,
        blittability: Blittability::from_manifest_fields(),
    };
    assert!(matches!(
        ComponentColumn::new(zero_sized),
        Err(WorldError::DescriptorSizeZero)
    ));

    let misaligned = ComponentLayout {
        size: 4,
        align: 3,
        schema_hash: 1,
        blittability: Blittability::from_manifest_fields(),
    };
    assert!(matches!(
        ComponentColumn::new(misaligned),
        Err(WorldError::DescriptorAlignmentInvalid)
    ));
}

/// An element whose size cannot be multiplied by the first growth's capacity
/// is reported as a layout error rather than tripping an `expect`.
#[test]
fn an_unrepresentable_growth_is_a_layout_error() {
    // One element of this size still describes a layout; four of them overflow
    // `usize`, so the refusal happens in `reserve_one` before any allocation.
    let unallocatable_growth = ComponentLayout {
        size: usize::MAX / 4 + 8,
        align: 8,
        schema_hash: 1,
        blittability: Blittability::from_manifest_fields(),
    };
    let mut column = ComponentColumn::new(unallocatable_growth).expect("one element still fits");
    assert!(matches!(
        column.push_zeroed(),
        Err(WorldError::DescriptorLayoutInvalid)
    ));
    assert_eq!(column.len(), 0, "a refused growth leaves the rows alone");
}

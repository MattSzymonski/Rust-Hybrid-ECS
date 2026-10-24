//! Unit tests for [`Archetype`](super::Archetype) and the
//! byte-storage columns it owns.
//!
//! # Responsibilities
//!
//! - Exercises column layout validation, byte-row moves and tick
//!   migration across the archetype's unsafe storage paths.
//!
//! # Design
//!
//! Kept beside the code they exercise: the storage paths are where
//! coverage is worth the most, and a sibling file lets
//! `archetype.rs` read as implementation while these tests keep
//! private access through `super::*`.

use super::*;
use crate::component::Tick;

/// One row of a two-field layout: `a` at 0, `b` at 4.
fn two_fields() -> ColumnLayout {
    ColumnLayout {
        size: 8,
        align: 4,
        schema_hash: 1,
        blittability: Blittability::engine_verified(),
    }
}

fn column_with_rows(rows: &[[u8; 8]]) -> ComponentColumn {
    let mut column = ComponentColumn::new(two_fields()).expect("a valid layout");
    for row in rows {
        column.push_bytes(row).expect("row matches the layout");
    }
    column
}

/// A layout carries its witness, and a layout with a release hook has that
/// hook run once per live row - by `Drop` and by `swap_remove`.
#[test]
fn a_layout_keeps_its_witness_and_a_column_releases_through_its_ops() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let constructed =
        ColumnLayout::new(4, 4, 7, Blittability::from_manifest_fields()).expect("a plain layout");
    assert_eq!(
        constructed.blittability,
        Blittability::from_manifest_fields(),
        "the constructor stores the witness it was handed"
    );

    // Release is the ops table's job now, not a hook on the layout: one
    // path serves a descriptor column (which reports `trivial_drop` and is
    // skipped) and a native one (which runs the element type's glue).
    // Counting calls holds both call sites - `Drop` and
    // `swap_remove_discard` - to one release per row.
    static RELEASES: AtomicUsize = AtomicUsize::new(0);
    unsafe fn count_releases(_rows: *mut u8, count: usize) {
        RELEASES.fetch_add(count, Ordering::SeqCst);
    }

    let layout =
        ColumnLayout::new(4, 4, 7, Blittability::from_manifest_fields()).expect("a valid layout");
    let mut column = ComponentColumn::new(layout).expect("a column");
    // Rows go in while the column still reports plain data, then the
    // counting table is installed. The byte mutators refuse a column whose
    // rows own resources, so populating first is now the only order that
    // works - and it is the honest one: a real column reaches a
    // non-trivial table by re-homing, never by being born with one and
    // then filled with zeroed bytes.
    column
        .push_zeroed()
        .expect("the first row grows the column");
    column.push_zeroed().expect("the second row fits");
    column.refresh_ops(ColumnOps {
        drop_range: count_releases,
        trivial_drop: false,
    });
    column.swap_remove_discard(0);
    assert_eq!(
        RELEASES.load(Ordering::SeqCst),
        1,
        "swap_remove_discard releases the row it overwrites"
    );

    drop(column);
    assert_eq!(
        RELEASES.load(Ordering::SeqCst),
        2,
        "Drop releases the one row still live"
    );
}

#[test]
fn plan_between_matches_fields_by_name_not_by_position() {
    let old = [
        LayoutField {
            name: "a",
            type_tag: "u32",
            offset: 0,
            size: 4,
        },
        LayoutField {
            name: "b",
            type_tag: "u32",
            offset: 4,
            size: 4,
        },
    ];
    // The same two fields, swapped in the new layout.
    let new = [
        LayoutField {
            name: "b",
            type_tag: "u32",
            offset: 0,
            size: 4,
        },
        LayoutField {
            name: "a",
            type_tag: "u32",
            offset: 4,
            size: 4,
        },
    ];

    let plan = FieldPlan::between(&old, &new);

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
            type_tag: "u32",
            offset: 0,
            size: 4,
        },
        LayoutField {
            name: "removed",
            type_tag: "u32",
            offset: 4,
            size: 4,
        },
    ];
    let new = [
        LayoutField {
            name: "kept",
            type_tag: "u32",
            offset: 0,
            size: 4,
        },
        LayoutField {
            name: "added",
            type_tag: "u32",
            offset: 4,
            size: 8,
        },
    ];

    let plan = FieldPlan::between(&old, &new);

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
        type_tag: "u32",
        offset: 0,
        size: 4,
    }];
    let new = [LayoutField {
        name: "grew",
        type_tag: "u32",
        offset: 0,
        size: 8,
    }];
    assert_eq!(
        FieldPlan::between(&old, &new).fields()[0].bytes,
        4,
        "the tail of a grown field has to come from the zero fill, not the old row"
    );

    let old = [LayoutField {
        name: "shrank",
        type_tag: "u32",
        offset: 0,
        size: 8,
    }];
    let new = [LayoutField {
        name: "shrank",
        type_tag: "u32",
        offset: 0,
        size: 4,
    }];
    assert_eq!(FieldPlan::between(&old, &new).fields()[0].bytes, 4);
}

#[test]
fn relayout_moves_rows_through_the_plan() {
    // Two rows, `a` and `b` each holding a distinct u32.
    let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0], [3, 0, 0, 0, 4, 0, 0, 0]]);
    let old = [
        LayoutField {
            name: "a",
            type_tag: "u32",
            offset: 0,
            size: 4,
        },
        LayoutField {
            name: "b",
            type_tag: "u32",
            offset: 4,
            size: 4,
        },
    ];
    // `b` first, then `a`, then a new eight-byte field.
    let new = [
        LayoutField {
            name: "b",
            type_tag: "u32",
            offset: 0,
            size: 4,
        },
        LayoutField {
            name: "a",
            type_tag: "u32",
            offset: 4,
            size: 4,
        },
        LayoutField {
            name: "added",
            type_tag: "u32",
            offset: 8,
            size: 8,
        },
    ];
    let plan = FieldPlan::between(&old, &new);

    let rows = column
        .relayout(
            ColumnLayout {
                size: 16,
                align: 8,
                schema_hash: 2,
                blittability: Blittability::engine_verified(),
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
    let plan = FieldPlan::between(
        &[
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ],
        &[
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ],
    );

    column
        .relayout(
            ColumnLayout {
                size: 8,
                align: 4,
                schema_hash: 3,
                blittability: Blittability::engine_verified(),
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
    let plan = FieldPlan::new();

    column
        .relayout(
            ColumnLayout {
                size: 4,
                align: 4,
                schema_hash: 4,
                blittability: Blittability::engine_verified(),
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
    let mut plan = FieldPlan::new();
    plan.push(4, 8, FieldSource::OldOffset(0));

    let result = column.relayout(
        ColumnLayout {
            size: 8,
            align: 4,
            schema_hash: 1,
            blittability: Blittability::engine_verified(),
        },
        &plan,
    );

    assert!(matches!(result, Err(WorldError::DescriptorRowInvalid)));
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
            ColumnLayout {
                size: 0,
                align: 4,
                schema_hash: 1,
                blittability: Blittability::engine_verified()
            },
            &FieldPlan::new()
        ),
        Err(WorldError::DescriptorSizeZero)
    ));
    assert!(matches!(
        column.relayout(
            ColumnLayout {
                size: 4,
                align: 3,
                schema_hash: 1,
                blittability: Blittability::engine_verified()
            },
            &FieldPlan::new()
        ),
        Err(WorldError::DescriptorAlignmentInvalid)
    ));
}
// =========================================================================
// Unified column: identity, ops and typed access
// =========================================================================

/// A native element type with real drop glue, so a POD column and a typed
/// one can be told apart by behaviour rather than by construction.
#[derive(Debug, PartialEq)]
struct DropCounted(u32);

thread_local! {
    static DROPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Drop for DropCounted {
    fn drop(&mut self) {
        DROPS.with(|count| count.set(count.get() + 1));
    }
}

/// A native column round-trips typed values through the byte buffer.
#[test]
fn a_native_column_pushes_and_reads_typed_rows() {
    let mut column = ComponentColumn::new_native::<u64>(7, false).expect("layout");
    column.push::<u64>(11);
    column.push::<u64>(22);

    assert_eq!(column.len(), 2);
    assert_eq!(*column.get::<u64>(0), 11);
    assert_eq!(*column.get::<u64>(1), 22);
    assert_eq!(column.as_slice::<u64>(), &[11, 22]);

    *column.get_mut::<u64>(0) = 33;
    assert_eq!(*column.get::<u64>(0), 33);
    assert_eq!(column.swap_remove::<u64>(0), 33);
    assert_eq!(column.as_slice::<u64>(), &[22]);
}

/// The element-type check refuses a `T` the column does not hold.
#[test]
#[should_panic(expected = "different component type")]
fn a_native_column_refuses_a_foreign_type() {
    let column = ComponentColumn::new_native::<u64>(7, false).expect("layout");
    let _ = column.as_slice::<u32>();
}

/// A descriptor-only column has no typed reading at all.
#[test]
#[should_panic(expected = "descriptor-only column has no Rust type")]
fn a_descriptor_column_refuses_typed_access() {
    let layout = ColumnLayout::new(8, 4, 7, Blittability::engine_verified()).expect("layout");
    let column = ComponentColumn::new(layout).expect("column");
    let _ = column.as_slice::<u64>();
}

/// A shared column checks layout instead of `TypeId`, because the two
/// binaries that reach it have different ids for one type.
#[test]
fn a_shared_column_accepts_a_layout_compatible_type() {
    let mut column = ComponentColumn::new_native::<u64>(7, true).expect("layout");
    column.push::<u64>(5);
    // A different type of the same shape is what the second binary's `T`
    // looks like from here.
    assert_eq!(*column.get::<i64>(0), 5);
}

/// ...and still refuses one whose layout disagrees.
#[test]
#[should_panic(expected = "shared column layout")]
fn a_shared_column_refuses_a_layout_mismatch() {
    let column = ComponentColumn::new_native::<u64>(7, true).expect("layout");
    let _ = column.as_slice::<u32>();
}

/// The generic ops table reports that there is nothing to drop, and the
/// generated one reports the truth about its element type.
#[test]
fn ops_report_the_drop_behaviour_of_their_lane() {
    let layout = ColumnLayout::new(4, 4, 7, Blittability::engine_verified()).expect("layout");
    assert!(
        ComponentColumn::new(layout)
            .expect("column")
            .ops()
            .trivial_drop
    );
    assert!(
        ComponentColumn::new_native::<u64>(7, false)
            .expect("column")
            .ops()
            .trivial_drop
    );
    assert!(
        !ComponentColumn::new_native::<DropCounted>(7, false)
            .expect("column")
            .ops()
            .trivial_drop
    );
}

/// The generated table drops exactly the rows it is given, once each.
#[test]
fn generated_ops_drop_each_row_once() {
    let mut column = ComponentColumn::new_native::<DropCounted>(7, false).expect("column");
    column.push(DropCounted(1));
    column.push(DropCounted(2));
    DROPS.with(|count| count.set(0));

    let ops = column.ops();
    // SAFETY: the column holds exactly two initialized `DropCounted` rows,
    // and nothing reads them afterwards.
    unsafe { (ops.drop_range)(column.as_mut_ptr(), 2) };
    assert_eq!(DROPS.with(std::cell::Cell::get), 2);

    // The rows were consumed by hand, so the column must not drop them again.
    std::mem::forget(column);
}

/// A column's ticks track its rows through every mutator, by construction.
///
/// This is the invariant the parallel `HashMap<ComponentId,
/// Vec<ComponentTicks>>` used to carry by hand across 21 maintenance sites
/// in `World`, guarded by a panic and two debug assertions whose only job
/// was to catch the desync. With the ticks owned by the column there is one
/// length to maintain, so the desync is unrepresentable rather than
/// checked - and this test is what pins that every mutator maintains it.
#[test]
fn a_columns_ticks_track_its_rows_through_every_mutator() {
    let mut column = ComponentColumn::new_native::<u32>(7, false).expect("column");
    assert_eq!(column.ticks().len(), column.len());

    column.push::<u32>(1);
    column.push::<u32>(2);
    column.push::<u32>(3);
    assert_eq!(column.ticks().len(), 3);
    assert_eq!(column.ticks().len(), column.len());

    column.set_row_ticks(1, ComponentTicks::new(Tick(42)));
    assert_eq!(column.row_ticks(1).expect("row 1 exists").added, Tick(42));

    // A swap-remove moves the tail tick over the hole exactly as it moves
    // the tail row, so row 1 keeps carrying row 1's metadata.
    column.swap_remove_discard(0);
    assert_eq!(column.ticks().len(), 2);
    assert_eq!(column.ticks().len(), column.len());
    assert_eq!(column.row_ticks(1).expect("row 1 exists").added, Tick(42));

    assert_eq!(column.swap_remove::<u32>(0), 3);
    assert_eq!(column.ticks().len(), column.len());

    // A move hands the row and its metadata to the destination together.
    let mut destination = ComponentColumn::new_native::<u32>(7, false).expect("column");
    destination
        .take_row_from(&mut column, 0)
        .expect("the two columns have one shape");
    assert_eq!(column.ticks().len(), column.len());
    assert_eq!(destination.ticks().len(), destination.len());
    assert_eq!(
        destination.row_ticks(0).expect("the moved row").added,
        Tick(42),
        "a moved row keeps the ticks it was carrying"
    );

    // The byte lanes maintain it too.
    let mut descriptor = ComponentColumn::new(two_fields()).expect("column");
    descriptor.push_zeroed().expect("push");
    descriptor.push_bytes(&[0; 8]).expect("push");
    assert_eq!(descriptor.ticks().len(), 2);
    assert_eq!(descriptor.ticks().len(), descriptor.len());
    descriptor.swap_remove_discard(0);
    assert_eq!(descriptor.ticks().len(), descriptor.len());
}

/// A relayout reshapes rows without disturbing their change ticks.
///
/// The column reallocates its buffer, and the ticks it carries have to come
/// across with it: a reshaped component has not been added or changed, so a
/// `Changed<T>` system must not see the whole world light up after a hot
/// reload that only moved a field.
#[test]
fn a_relayout_carries_the_column_ticks_across_the_reallocation() {
    let mut column = ComponentColumn::new(two_fields()).expect("column");
    column.push_bytes(&[1, 0, 0, 0, 2, 0, 0, 0]).expect("push");
    column.push_bytes(&[3, 0, 0, 0, 4, 0, 0, 0]).expect("push");
    column.set_row_ticks(0, ComponentTicks::new(Tick(11)));
    column.set_row_ticks(1, ComponentTicks::new(Tick(22)));

    // A wider, more strongly aligned shape forces a fresh allocation.
    let widened =
        ColumnLayout::new(16, 8, 77, Blittability::engine_verified()).expect("a valid layout");
    column
        .relayout(widened, &FieldPlan::new())
        .expect("an empty plan zeroes every row");

    assert_eq!(column.ticks().len(), column.len());
    assert_eq!(column.row_ticks(0).expect("row 0").added, Tick(11));
    assert_eq!(column.row_ticks(1).expect("row 1").added, Tick(22));
}

/// Reserving space does not change what the column contains.
#[test]
fn reserving_rows_preserves_contents() {
    let mut column = ComponentColumn::new_native::<u32>(7, false).expect("column");
    column.push::<u32>(9);
    column.reserve_rows(64).expect("reserve");
    assert!(column.capacity >= 65);
    assert_eq!(column.as_slice::<u32>(), &[9]);
    column.push::<u32>(10);
    assert_eq!(column.as_slice::<u32>(), &[9, 10]);
}
/// A descriptor column that gains alignment reallocates rather than
/// reinterpreting its old stride.
///
/// This is the descriptor-lane counterpart of audit item 4.18, and it is
/// why that lane needs no registration-time refusal of its own. The native
/// hazard was a column re-homed onto a newer ops table and then *read
/// through the incoming type* at the old stride. A descriptor column has no
/// typed reading at all, and a relayout that changes size or alignment
/// moves every row into a freshly allocated, correctly aligned buffer, so
/// the misaligned read the guard exists to prevent is unrepresentable here.
#[test]
fn a_descriptor_relayout_that_widens_alignment_reallocates() {
    // Two 4-byte fields, align 4 - the shape audit 4.18 widened.
    let old = ColumnLayout::new(12, 4, 1, Blittability::engine_verified()).expect("layout");
    let mut column = ComponentColumn::new(old).expect("column");
    column
        .push_bytes(&[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0])
        .expect("push");
    column
        .push_bytes(&[4, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0])
        .expect("push");

    // Widen to align 8, the case the native lane refuses.
    let new = ColumnLayout::new(16, 8, 2, Blittability::engine_verified()).expect("layout");
    let mut plan = FieldPlan::new();
    plan.push(0, 4, FieldSource::OldOffset(0));
    plan.push(8, 4, FieldSource::OldOffset(4));
    let migrated = column.relayout(new, &plan).expect("relayout");

    assert_eq!(migrated, 2, "both rows migrate");
    assert_eq!(column.len(), 2, "row count and order survive");
    assert_eq!(column.alignment(), 8);
    assert_eq!(
        column.as_mut_ptr() as usize % 8,
        0,
        "the new buffer really is 8-aligned, which is what the native lane could not promise"
    );
    assert_eq!(&column.bytes(0).expect("row")[0..4], &[1, 0, 0, 0]);
    assert_eq!(&column.bytes(0).expect("row")[8..12], &[2, 0, 0, 0]);
    assert_eq!(&column.bytes(1).expect("row")[0..4], &[4, 0, 0, 0]);
}
/// A field that keeps its name but changes type is reset, not reinterpreted.
#[test]
fn plan_between_resets_a_field_whose_type_changed() {
    let old = [LayoutField {
        name: "health",
        type_tag: "i32",
        offset: 0,
        size: 4,
    }];
    let new = [LayoutField {
        name: "health",
        type_tag: "f32",
        offset: 0,
        size: 4,
    }];

    let plan = FieldPlan::between(&old, &new);

    // Copying would have carried the bits across: `5i32` read as `f32` is
    // 7e-45, a different number wearing the same name.
    assert_eq!(
        plan.fields(),
        &[PlannedField {
            offset: 0,
            bytes: 4,
            source: FieldSource::ZeroFill,
        }],
        "a retyped field takes the zero fill, not the old bytes"
    );
    assert_eq!(
        plan.retyped_fields(),
        &["health".to_string()],
        "and the reset is recorded so the caller can report it"
    );
}

/// A field that keeps its name and type still carries its bytes over.
#[test]
fn plan_between_copies_a_field_whose_type_is_unchanged() {
    let old = [LayoutField {
        name: "health",
        type_tag: "i32",
        offset: 8,
        size: 4,
    }];
    let new = [LayoutField {
        name: "health",
        type_tag: "i32",
        offset: 0,
        size: 4,
    }];

    let plan = FieldPlan::between(&old, &new);

    assert_eq!(
        plan.fields(),
        &[PlannedField {
            offset: 0,
            bytes: 4,
            source: FieldSource::OldOffset(8),
        }],
        "a moved field follows its name"
    );
    assert!(
        plan.retyped_fields().is_empty(),
        "nothing was reset, so nothing is reported"
    );
}

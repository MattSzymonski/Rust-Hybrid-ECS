//! Field decorators on `#[derive(PillComponent)]` and `#[derive(PillMirror)]`.
//!
//! # Responsibilities
//!
//! - Show that every `#[pill(...)]` field decorator the Inspector understands
//!   round-trips from the derive's compile-time layout, in declaration order.
//! - Show that an undecorated field keeps an empty decorator slice, so the
//!   common case costs nothing.
//! - Show that a decorated field of a nested value type keeps its decorations
//!   on its dotted rows after registration, which is what lets the Inspector
//!   style a nested colour channel.

use pill_engine::component_registry::{
    register_all_components, ComponentFieldDescriptor, FieldDecorator,
};
use pill_engine::{ComponentId, PillComponent, PillMirror, World};

/// Carries one field per decorator, plus one undecorated field, so the layout
/// proves the whole vocabulary reaches the descriptor.
#[repr(C)]
#[derive(Debug, Clone, Default, PillComponent)]
struct DecoratedProbe {
    /// No decorators: the row draws exactly as it did before this feature.
    plain: f32,
    /// Verified through the decorator set below.
    #[pill(hide_in_inspector)]
    hidden: usize,
    /// A header and a gap on the same field, in the order declared.
    #[pill(header("Timing"), space(10.0))]
    elapsed: f32,
    /// A range and a tooltip together, the shape a health bar takes.
    #[pill(tooltip("Current hit points"), range(0.0, 100.0))]
    health: f32,
    /// The parameterless text area: Unity's three-row default.
    #[pill(text_area)]
    notes: String,
    /// A bounded text area.
    #[pill(text_area(3, 10))]
    description: String,
    /// A fixed-height multi-line box, declared as its own attribute.
    #[pill(multiline(5))]
    biography: String,
    /// A drag value that never goes below zero.
    #[pill(min(0.0))]
    ammo: i32,
}

/// A mirrored value type whose fields are decorated at their own declaration.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillMirror)]
struct DecoratedRange {
    /// Open end of the range, decorated for the dotted row.
    #[pill(range(0.0, 1.0))]
    start: f32,
    /// Closed end, with a minimum too.
    #[pill(min(0.0), tooltip("Where the range ends"))]
    end: f32,
}

/// A component nesting the decorated value type.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillComponent)]
struct RangedCarrier {
    /// Ordinary scalar, so the expansion has something to leave alone.
    lead: f32,
    /// Expands into `range.start` and `range.end`.
    range: DecoratedRange,
}

/// The decorators a component field declares reach its layout unchanged, in
/// declaration order, and an undecorated field carries none.
#[test]
fn field_decorators_round_trip_through_the_layout() {
    let layout = DecoratedProbe::FIELD_LAYOUT;
    assert_eq!(layout.len(), 8);

    assert_eq!(layout[0].name, "plain");
    assert!(
        layout[0].decorators.is_empty(),
        "an undecorated field keeps an empty decorator slice"
    );

    assert_eq!(
        layout[1].decorators.as_ref(),
        &[FieldDecorator::HideInInspector]
    );

    // Header and gap are separate decorators, so the Inspector can draw the
    // line and the space independently.
    assert_eq!(
        layout[2].decorators.as_ref(),
        &[
            FieldDecorator::Header("Timing".into()),
            FieldDecorator::Space(10.0),
        ]
    );

    assert_eq!(
        layout[3].decorators.as_ref(),
        &[
            FieldDecorator::Tooltip("Current hit points".into()),
            FieldDecorator::Range {
                min: 0.0,
                max: 100.0,
            },
        ]
    );

    assert_eq!(
        layout[4].decorators.as_ref(),
        &[FieldDecorator::TextArea {
            min_lines: 3,
            max_lines: 3,
        }]
    );

    assert_eq!(
        layout[5].decorators.as_ref(),
        &[FieldDecorator::TextArea {
            min_lines: 3,
            max_lines: 10,
        }]
    );

    assert_eq!(
        layout[6].decorators.as_ref(),
        &[FieldDecorator::Multiline(5)]
    );

    assert_eq!(layout[7].decorators.as_ref(), &[FieldDecorator::Min(0.0)]);
}

/// A nested value type's decorated fields keep their decorators when the world
/// expands the struct field into dotted rows.
#[test]
fn a_decorated_value_type_field_keeps_its_decorators_on_dotted_rows() {
    let mut world = World::new();
    register_all_components(&mut world).expect("registration succeeds");

    let layout = world
        .component_field_layout(ComponentId::of::<RangedCarrier>())
        .expect("the derive registers a field layout");
    let rows: Vec<(&str, &[FieldDecorator])> = layout
        .iter()
        .map(|field: &ComponentFieldDescriptor| (field.name, field.decorators.as_ref()))
        .collect();
    assert_eq!(
        rows,
        [
            ("lead", &[][..]),
            (
                "range.start",
                &[FieldDecorator::Range { min: 0.0, max: 1.0 }][..]
            ),
            (
                "range.end",
                &[
                    FieldDecorator::Min(0.0),
                    FieldDecorator::Tooltip("Where the range ends".into()),
                ][..],
            ),
        ]
    );
}

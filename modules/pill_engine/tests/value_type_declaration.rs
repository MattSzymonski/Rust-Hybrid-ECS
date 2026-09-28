//! Declaring a foreign type's layout with `pill_value_type!`.
//!
//! # Responsibilities
//!
//! - Exercise the macro on a path spelled across segments, and on two
//!   declarations whose paths share a trailing segment.
//! - Pin the generated item naming that lets the two coexist.
//!
//! The macro walks the fields through a synthetic struct and takes every
//! offset from the compiler, so a path spelled across segments has to work the
//! same as an imported name - and two declarations whose paths share a
//! trailing segment have to coexist, which pins the generated item naming.

mod probe {
    /// A `repr(C)` stand-in for a foreign vector type.
    #[repr(C)]
    pub struct Vec3Probe {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }
}

mod other {
    /// A second type whose path ends in the same segment.
    #[repr(C)]
    pub struct Vec3Probe {
        pub x: f32,
        pub y: f32,
    }
}

pill_engine::pill_value_type! {
    probe::Vec3Probe {
        x: f32,
        y: f32,
        z: f32,
    }
}

pill_engine::pill_value_type! {
    other::Vec3Probe {
        x: f32,
        y: f32,
    }
}

/// Both declarations land in the registry under the path as written, with
/// compiler-derived offsets and sizes.
#[test]
fn declared_types_are_registered_under_their_written_paths() {
    // Touch the fields so the stand-in structs are not dead code.
    let _ = probe::Vec3Probe {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let _ = other::Vec3Probe { x: 1.0, y: 2.0 };

    let descriptors = pill_engine::component_registry::value_type_descriptors();

    let probe = descriptors
        .iter()
        .find(|descriptor| descriptor.type_name == "probe::Vec3Probe")
        .expect("probe::Vec3Probe must be registered");
    assert_eq!(probe.size, 12);
    assert_eq!(probe.align, 4);
    let fields: Vec<(&str, &str, usize)> = probe
        .fields
        .iter()
        .map(|field| (field.name, field.type_tag, field.offset))
        .collect();
    assert_eq!(fields, [("x", "f32", 0), ("y", "f32", 4), ("z", "f32", 8)]);

    let other = descriptors
        .iter()
        .find(|descriptor| descriptor.type_name == "other::Vec3Probe")
        .expect("other::Vec3Probe must be registered");
    assert_eq!(other.size, 8);
    assert_eq!(other.align, 4);
    assert_eq!(other.fields.len(), 2);
}

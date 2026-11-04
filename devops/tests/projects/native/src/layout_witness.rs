//! A resource whose declared layout proves the keep-or-rebuild decision.
//!
//! # Responsibilities
//!
//! - Holds heap state, so a value that was dropped and one that was leaked can
//!   be told apart, and prints one line when it is dropped.
//! - Declares the file that defines its layout, and nothing else, so the
//!   keep-or-rebuild suites can edit exactly one source and know what the host
//!   should decide.
//!
//! # Design
//!
//! The suites need three observable outcomes for one resource across a reload:
//! kept, dropped through the retiring image, and rebuilt. The declaration is
//! what makes the first and the second different, and the drop line is what
//! proves the value was dropped by the image that owned its drop function
//! rather than leaked.
//!
//! [`WITNESS_BUILD_VALUE`] is part of the declared file, so an edit to it is an
//! edit to the layout. The value inserted at build time is printed both when it
//! is created and when it is dropped, which is how a suite proves that the
//! rebuild produced the new value rather than restoring the old one.

// Standard library
use std::sync::atomic::{AtomicU32, Ordering};

// External crates
use pill_engine::{Engine, Resource};

/// The witness's build-time value. Editing this line is editing the layout.
const WITNESS_BUILD_VALUE: u32 = 1;

/// The single source this resource's layout is made of.
const WITNESS_SOURCE: &str = "src/layout_witness.rs";

/// Counts witness values dropped in this process, so a suite can tell "the
/// value was dropped" from "the value never existed".
static DROPPED_WITNESS_COUNT: AtomicU32 = AtomicU32::new(0);

/// A resource that owns heap state and reports its own drop.
pub struct LayoutWitness {
    /// Opaque payload naming the build that created this value.
    payload: String,
}

impl LayoutWitness {
    /// A witness carrying the value it was built with.
    fn new() -> Self {
        Self {
            payload: format!("layout-witness-{WITNESS_BUILD_VALUE}"),
        }
    }

    /// The payload this value was created with.
    pub fn payload(&self) -> &str {
        &self.payload
    }
}

impl Resource for LayoutWitness {
    /// The witness is a shared resource, and it has to be: an id built from
    /// `TypeId` differs in every generation, so a reload would retire the old
    /// value instead of ever keeping it. A declared name gives both generations
    /// one identity, which is the only arrangement the decision can be about.
    fn shared_name() -> Option<&'static str> {
        Some("native::layout_witness::LayoutWitness")
    }

    /// Only this file, so the suites can edit the layout without touching
    /// anything else the module compiles.
    fn layout_sources() -> Option<&'static [&'static str]> {
        Some(&[WITNESS_SOURCE])
    }
}

impl Drop for LayoutWitness {
    fn drop(&mut self) {
        let dropped = DROPPED_WITNESS_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        // The suite matches this text, and `test_log_contract.py` requires it
        // to be a literal in the source rather than a constant.
        pill_engine::tracing::info!(
            "[test] layout witness dropped payload={} dropped_total={dropped}",
            self.payload
        );
    }
}

/// Registers the witness, keeping a live value across a reload.
///
/// The value is inserted only when the world has none: a kept value has to
/// survive, and a value the layout decision dropped has to come back. The
/// registration itself runs on every generation so the kept value's drop
/// function points at code that is still mapped.
pub fn register(engine: &mut Engine) {
    if engine.world().get_resource::<LayoutWitness>().is_none() {
        let witness = LayoutWitness::new();
        // Literal for the same reason as the drop line below.
        pill_engine::tracing::info!(
            "[test] layout witness inserted payload={} build_value={WITNESS_BUILD_VALUE}",
            witness.payload()
        );
        engine.world_mut().insert_resource(witness);
    }
    engine.world_mut().register_resource::<LayoutWitness>();
}

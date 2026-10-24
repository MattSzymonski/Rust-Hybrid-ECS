//! Integration tests for `#[pill_hot_fn]` on a method with parameters.
//!
//! # Responsibilities
//!
//! - Pin that a method annotated `#[pill_hot_fn]` compiles when it declares
//!   parameters of its own, which the dispatcher forwards by the names the
//!   body uses.
//! - Pin that such a method answers with its own body, follows an installed
//!   replacement, and returns to its body on a reset - the same lifecycle a
//!   no-parameter method has.

use pill_engine::hot_patch::{
    install_plain_function, plain_function_signature, reset_plain_function,
};
use pill_engine::pill_hot_fn;

/// A receiver whose methods take arguments, so the attribute has values to
/// forward rather than only a receiver.
struct Scaler {
    /// Multiplier [`Scaler::scale`] applies to its argument.
    factor: f32,
}

impl Scaler {
    /// Multiplies `value` by the receiver's factor.
    ///
    /// The plain-parameter case: the body names `value` directly, and the
    /// dispatcher has to forward it under that same name.
    #[pill_hot_fn]
    fn scale(&self, value: f32) -> f32 {
        self.factor * value
    }

    /// Adds half the receiver's factor to `delta`, through a `mut` binding.
    ///
    /// Pins that a `mut` pattern survives: the declaration keeps it and the
    /// forwarded name is the one it binds.
    #[pill_hot_fn]
    fn nudge(&self, mut delta: f32) -> f32 {
        delta += self.factor * 0.5;
        delta
    }
}

/// Replacement for [`Scaler::scale`], answering a constant that no arrangement
/// of the receiver's state could produce.
fn scale_replacement(_scaler: &Scaler, _value: f32) -> f32 {
    777.0
}

/// The shape a generated method patch takes: the same declaration, filed under
/// a patch name and told the concrete receiver type.
///
/// The host builds every method patch this way, and the two copies have to
/// record the same signature text for the install gate to accept the
/// replacement, which is what the assertion below pins.
mod method_patch {
    use pill_engine::pill_hot_fn;

    use super::Scaler;

    /// The patch copy of [`super::Scaler::nudge`], carrying its body into a
    /// local trait as the host's generated source does.
    #[pill_hot_fn(name = "test_patch::nudge", self_type = Scaler)]
    fn nudge(&self, mut delta: f32) -> f32 {
        delta += self.factor * 0.5;
        delta
    }
}

// =============================================================================
// Dispatch
// =============================================================================

/// A method with a plain parameter follows the install/reset lifecycle: its
/// own body first, the replacement once installed, and its own body again
/// after a reset.
#[test]
fn a_method_with_a_plain_parameter_dispatches_through_its_slot() {
    let scaler = Scaler { factor: 2.0 };
    assert_eq!(scaler.scale(3.0), 6.0, "the method's own body first");

    let name = concat!(module_path!(), "::scale");
    let signature = plain_function_signature(name)
        .expect("the attribute registers the method under its module path");
    install_plain_function(name, scale_replacement as *const () as usize, signature)
        .expect("a replacement of the recorded shape is accepted");
    assert_eq!(scaler.scale(3.0), 777.0, "callers see the replacement");

    reset_plain_function(name).expect("a reset finds the registered method");
    assert_eq!(scaler.scale(3.0), 6.0, "a reset returns it to its own body");
}

/// A `mut` parameter binding compiles and keeps its own computation.
#[test]
fn a_method_with_a_mut_parameter_keeps_its_computation() {
    let scaler = Scaler { factor: 4.0 };
    assert_eq!(scaler.nudge(1.0), 3.0, "delta plus half the factor");
}

/// A generated method patch records the same signature text as the running
/// method, which is what lets its replacement pass the gate at install time.
#[test]
fn a_generated_method_patch_records_the_running_signature() {
    let running = plain_function_signature(concat!(module_path!(), "::nudge"))
        .expect("the running method is registered");
    let patch = plain_function_signature("test_patch::nudge")
        .expect("the patch copy is registered under the patch name");
    assert_eq!(
        patch, running,
        "both copies must derive one signature from one declaration"
    );
}

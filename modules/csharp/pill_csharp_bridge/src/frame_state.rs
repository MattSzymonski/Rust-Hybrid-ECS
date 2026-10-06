//! Read-only frame state for the managed runtime: input and time.
//!
//! # Responsibilities
//!
//! - Answer the managed runtime's input queries (keys and mouse buttons held,
//!   mouse motion since the previous frame) from the active invocation's
//!   `Input` resource.
//! - Report the engine clock's frame delta and elapsed time from the active
//!   invocation's `Time` resource.
//!
//! # Design
//!
//! Every function runs inside an active managed invocation, whose world the
//! host owns, and reports the values the frame's systems see - so a managed
//! system and a native one agree about the frame they process. Outside an
//! invocation the answers are "nothing held" and zero: these are reads of
//! per-frame state, so there is no cached copy that could go stale.
//!
//! Keys and mouse buttons cross as `u8` discriminants: [`KeyCode::ALL`] is
//! the wire order, and the managed mirror lists the same keys in the same
//! order. An unknown discriminant reports "not held".

// External crates
use pill_core::math::Vector2f;
use pill_engine::{Input, KeyCode, MouseButton, Time};

// Current crate
use crate::context::with_active_world;

/// Whether the key with discriminant `key` is held this frame.
///
/// `1` while held, `0` otherwise - outside an active invocation included, and
/// for a discriminant no [`KeyCode`] has.
pub(super) extern "C" fn ffi_input_key_held(key: u8) -> u8 {
    let Some(key) = KeyCode::ALL.get(key as usize).copied() else {
        return 0;
    };
    let held = with_active_world(|world| {
        world
            .get_resource::<Input>()
            .is_some_and(|input| input.key_held(key))
    })
    .unwrap_or(false);
    u8::from(held)
}

/// Whether the mouse button with discriminant `button` is held this frame;
/// shaped like [`ffi_input_key_held`].
pub(super) extern "C" fn ffi_input_mouse_button_held(button: u8) -> u8 {
    let Some(button) = MouseButton::ALL.get(button as usize).copied() else {
        return 0;
    };
    let held = with_active_world(|world| {
        world
            .get_resource::<Input>()
            .is_some_and(|input| input.mouse_button_held(button))
    })
    .unwrap_or(false);
    u8::from(held)
}

/// Writes the mouse motion since the previous frame, in physical pixels, to
/// `out_x`/`out_y`, and returns `1`.
///
/// Returns `0` without writing when either output is null. Outside an active
/// invocation the written delta is zero.
///
/// # Safety
///
/// `out_x` and `out_y` must be null or point at writable `f32` values for the
/// call's duration.
pub(super) extern "C" fn ffi_input_mouse_delta(out_x: *mut f32, out_y: *mut f32) -> u8 {
    if out_x.is_null() || out_y.is_null() {
        return 0;
    }
    let delta = with_active_world(|world| {
        world
            .get_resource::<Input>()
            .map_or(Vector2f::ZERO, |input| input.mouse_delta())
    })
    .unwrap_or(Vector2f::ZERO);
    // SAFETY: checked non-null above; the caller's contract makes both
    // writable.
    unsafe {
        *out_x = delta.x;
        *out_y = delta.y;
    }
    1
}

/// The frame's clamped delta in seconds, or `0.0` with no active clock.
///
/// This is the value gameplay integrates with, not wall clock, so a stalled
/// frame cannot teleport a managed simulation either.
pub(super) extern "C" fn ffi_time_delta_seconds() -> f32 {
    with_active_world(|world| world.get_resource::<Time>().map(Time::delta_seconds))
        .flatten()
        .unwrap_or(0.0)
}

/// Seconds since the engine started, or `0.0` with no active clock:
/// wall-clock time, for animation that must not tick with the delta.
pub(super) extern "C" fn ffi_time_elapsed_seconds() -> f32 {
    with_active_world(|world| world.get_resource::<Time>().map(Time::elapsed_seconds))
        .flatten()
        .unwrap_or(0.0)
}

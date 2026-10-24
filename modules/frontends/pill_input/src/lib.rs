//! Input glue shared by the frontends.
//!
//! # Responsibilities
//!
//! - Translate winit window and device events into the engine's
//!   [`InputEvent`](pill_engine::InputEvent)s ([`winit_events`], feature
//!   `winit`).
//! - Own the gamepads: poll them into input events and play the rumble systems
//!   request ([`gamepads`], feature `gamepad`, native only).
//!
//! # Design
//!
//! The engine defines what input is (`pill_engine::input`) and knows no
//! platform library; `pill_runtime` must not link winit either. Each frontend
//! gets its platform's events in its own loop, so translation belongs to the
//! frontends - and two of them (`pill_standalone`, `pill_web`) are built on the
//! same winit, so the translation lives here once instead of in each. The
//! editor receives DOM events instead and maps their W3C key names through
//! `KeyCode::from_code_name`, but it shares the gamepad half.
//!
//! Frontends forward the results through `FrameDriver::push_input` between
//! frames, and hand `FrameDriver::take_rumble_requests` to
//! [`gamepads::Gamepads::play_rumble`] after them.

/// Gamepads through gilrs: polled into input events, rumbled on request.
#[cfg(all(feature = "gamepad", not(target_arch = "wasm32")))]
pub mod gamepads;

/// winit window and device events as engine input events.
#[cfg(feature = "winit")]
pub mod winit_events;

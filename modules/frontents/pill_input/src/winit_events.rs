//! winit events as engine input events.
//!
//! # Responsibilities
//!
//! - Translate the input-carrying [`WindowEvent`]s and [`DeviceEvent`]s into
//!   [`InputEvent`]s ([`translate_window_event`], [`translate_device_event`]).
//! - Map winit's key codes and mouse buttons onto the engine's ([`key_code`],
//!   [`mouse_button`]).
//!
//! # Design
//!
//! The engine's [`KeyCode`] mirrors winit's variant for variant, so the key
//! mapping is one list of names; a name the engine lacks fails to compile, and
//! a count check catches a key left out. Positions and deltas stay in physical
//! pixels, as winit reports them; the window is the game's whole area in both
//! winit frontends.

// External crates
use pill_core::math::Vector2f;
use pill_engine::{ButtonState, InputEvent, KeyCode, MouseButton, ScrollDelta};
use winit::event::{DeviceEvent, ElementState, MouseScrollDelta, WindowEvent};
use winit::keyboard::PhysicalKey;

// =============================================================================
// Free Functions
// =============================================================================

/// The input event a window event carries, or `None` for any other event.
///
/// Key repeats are dropped: the engine does not count them as presses.
pub fn translate_window_event(event: &WindowEvent) -> Option<InputEvent> {
    match event {
        WindowEvent::KeyboardInput { event, .. } => {
            if event.repeat {
                return None;
            }
            let PhysicalKey::Code(code) = event.physical_key else {
                return None;
            };
            Some(InputEvent::Key {
                key: key_code(code)?,
                state: button_state(event.state),
            })
        }
        WindowEvent::MouseInput { state, button, .. } => Some(InputEvent::MouseButton {
            button: mouse_button(*button)?,
            state: button_state(*state),
        }),
        WindowEvent::CursorMoved { position, .. } => Some(InputEvent::CursorMoved {
            position: Vector2f::new(position.x as f32, position.y as f32),
        }),
        WindowEvent::CursorLeft { .. } => Some(InputEvent::CursorLeft),
        WindowEvent::MouseWheel { delta, .. } => Some(InputEvent::MouseWheel {
            delta: match *delta {
                MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines(Vector2f::new(x, y)),
                MouseScrollDelta::PixelDelta(position) => {
                    ScrollDelta::Pixels(Vector2f::new(position.x as f32, position.y as f32))
                }
            },
        }),
        WindowEvent::Focused(false) => Some(InputEvent::FocusLost),
        _ => None,
    }
}

/// The input event a device event carries: raw mouse motion, or `None`.
pub fn translate_device_event(event: &DeviceEvent) -> Option<InputEvent> {
    match event {
        DeviceEvent::MouseMotion { delta } => Some(InputEvent::MouseMotion {
            delta: Vector2f::new(delta.0 as f32, delta.1 as f32),
        }),
        _ => None,
    }
}

/// The engine's button state for winit's.
fn button_state(state: ElementState) -> ButtonState {
    match state {
        ElementState::Pressed => ButtonState::Pressed,
        ElementState::Released => ButtonState::Released,
    }
}

/// The engine's mouse button for winit's, or `None` for a numbered extra
/// button the engine does not track.
pub fn mouse_button(button: winit::event::MouseButton) -> Option<MouseButton> {
    match button {
        winit::event::MouseButton::Left => Some(MouseButton::Left),
        winit::event::MouseButton::Right => Some(MouseButton::Right),
        winit::event::MouseButton::Middle => Some(MouseButton::Middle),
        winit::event::MouseButton::Back => Some(MouseButton::Back),
        winit::event::MouseButton::Forward => Some(MouseButton::Forward),
        winit::event::MouseButton::Other(_) => None,
    }
}

/// Declares [`key_code`] from the key names winit and the engine share.
macro_rules! shared_key_codes {
    ($($name:ident),* $(,)?) => {
        /// The engine's key for winit's, or `None` for a key winit adds later.
        pub fn key_code(code: winit::keyboard::KeyCode) -> Option<KeyCode> {
            match code {
                $(winit::keyboard::KeyCode::$name => Some(KeyCode::$name),)*
                _ => None,
            }
        }

        /// How many keys [`key_code`] maps.
        const MAPPED_KEY_COUNT: usize = [$(stringify!($name)),*].len();
    };
}

shared_key_codes! {
    Backquote, Backslash, BracketLeft, BracketRight, Comma, Digit0, Digit1, Digit2, Digit3,
    Digit4, Digit5, Digit6, Digit7, Digit8, Digit9, Equal, IntlBackslash, IntlRo, IntlYen, KeyA,
    KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP,
    KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ, Minus, Period, Quote, Semicolon,
    Slash, AltLeft, AltRight, Backspace, CapsLock, ContextMenu, ControlLeft, ControlRight, Enter,
    SuperLeft, SuperRight, ShiftLeft, ShiftRight, Space, Tab, Convert, KanaMode, Lang1, Lang2,
    Lang3, Lang4, Lang5, NonConvert, Delete, End, Help, Home, Insert, PageDown, PageUp, ArrowDown,
    ArrowLeft, ArrowRight, ArrowUp, NumLock, Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5,
    Numpad6, Numpad7, Numpad8, Numpad9, NumpadAdd, NumpadBackspace, NumpadClear,
    NumpadClearEntry, NumpadComma, NumpadDecimal, NumpadDivide, NumpadEnter, NumpadEqual,
    NumpadHash, NumpadMemoryAdd, NumpadMemoryClear, NumpadMemoryRecall, NumpadMemoryStore,
    NumpadMemorySubtract, NumpadMultiply, NumpadParenLeft, NumpadParenRight, NumpadStar,
    NumpadSubtract, Escape, Fn, FnLock, PrintScreen, ScrollLock, Pause, BrowserBack,
    BrowserFavorites, BrowserForward, BrowserHome, BrowserRefresh, BrowserSearch, BrowserStop,
    Eject, LaunchApp1, LaunchApp2, LaunchMail, MediaPlayPause, MediaSelect, MediaStop,
    MediaTrackNext, MediaTrackPrevious, Power, Sleep, AudioVolumeDown, AudioVolumeMute,
    AudioVolumeUp, WakeUp, Meta, Hyper, Turbo, Abort, Resume, Suspend, Again, Copy, Cut, Find,
    Open, Paste, Props, Select, Undo, Hiragana, Katakana, F1, F2, F3, F4, F5, F6, F7, F8, F9, F10,
    F11, F12, F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, F23, F24, F25, F26, F27, F28, F29,
    F30, F31, F32, F33, F34, F35,
}

// Every engine key has a winit counterpart in the list above.
const _: () = assert!(MAPPED_KEY_COUNT == KeyCode::ALL.len());

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_by_name() {
        assert_eq!(
            key_code(winit::keyboard::KeyCode::KeyW),
            Some(KeyCode::KeyW)
        );
        assert_eq!(
            key_code(winit::keyboard::KeyCode::SuperLeft),
            Some(KeyCode::SuperLeft)
        );
        assert_eq!(key_code(winit::keyboard::KeyCode::F35), Some(KeyCode::F35));
    }

    #[test]
    fn numbered_extra_mouse_buttons_are_not_tracked() {
        assert_eq!(
            mouse_button(winit::event::MouseButton::Back),
            Some(MouseButton::Back)
        );
        assert_eq!(mouse_button(winit::event::MouseButton::Other(9)), None);
    }

    #[test]
    fn focus_loss_and_raw_motion_translate() {
        assert_eq!(
            translate_window_event(&WindowEvent::Focused(false)),
            Some(InputEvent::FocusLost)
        );
        assert_eq!(translate_window_event(&WindowEvent::Focused(true)), None);
        assert_eq!(
            translate_device_event(&DeviceEvent::MouseMotion { delta: (3.0, -4.0) }),
            Some(InputEvent::MouseMotion {
                delta: Vector2f::new(3.0, -4.0),
            })
        );
    }
}

//! winit events as engine input events.
//!
//! # Responsibilities
//!
//! - Translate the input-carrying [`WindowEvent`]s and [`DeviceEvent`]s into
//!   [`InputEvent`]s ([`translate_window_event`], [`translate_device_event`]).
//! - Map winit's key codes, mouse buttons, modifier state and IME events onto
//!   the engine's ([`key_code`], [`mouse_button`], [`modifier_state`],
//!   [`ime_event`]).
//!
//! # Design
//!
//! The engine's [`KeyCode`] mirrors winit's variant for variant, so the key
//! mapping is one list of names; a name the engine lacks fails to compile, and
//! a count check catches a key left out. Positions and deltas stay in physical
//! pixels, as winit reports them, and a frontend gives the frame driver the
//! window's whole inner size, so input and the drawn surface share one space.
//! One window event can carry two engine events - a pressed key reports both
//! its key change and its committed text - so the translation yields a small
//! sequence rather than one optional event.

// External crates
use pill_core::math::Vector2f;
use pill_engine::{ButtonState, InputEvent, KeyCode, Modifiers, MouseButton, ScrollDelta};
use winit::event::{DeviceEvent, ElementState, Ime, MouseScrollDelta, WindowEvent};
use winit::keyboard::{ModifiersState, PhysicalKey};

// =============================================================================
// Free Functions
// =============================================================================

/// The input events a window event carries, in the order to apply them.
///
/// Zero events for an event that carries no input, one for most carriers,
/// two for a key that was both pressed or repeated and typed something: the
/// key change first, its committed text second.
pub fn translate_window_event(event: &WindowEvent) -> impl Iterator<Item = InputEvent> {
    let events = match event {
        WindowEvent::KeyboardInput { event, .. } => {
            let PhysicalKey::Code(code) = event.physical_key else {
                return [None, None].into_iter().flatten();
            };
            key_code(code)
                .map(|key| key_events(key, event.state, event.repeat, event.text.as_deref()))
                .unwrap_or([None, None])
        }
        WindowEvent::ModifiersChanged(modifiers) => [
            Some(InputEvent::ModifiersChanged {
                modifiers: modifier_state(modifiers.state()),
            }),
            None,
        ],
        WindowEvent::Ime(ime) => [ime_event(ime), None],
        WindowEvent::MouseInput { state, button, .. } => [
            mouse_button(*button).map(|button| InputEvent::MouseButton {
                button,
                state: button_state(*state),
            }),
            None,
        ],
        WindowEvent::CursorMoved { position, .. } => [
            Some(InputEvent::CursorMoved {
                position: Vector2f::new(position.x as f32, position.y as f32),
            }),
            None,
        ],
        WindowEvent::CursorLeft { .. } => [Some(InputEvent::CursorLeft), None],
        WindowEvent::MouseWheel { delta, .. } => [
            Some(InputEvent::MouseWheel {
                delta: match *delta {
                    MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines(Vector2f::new(x, y)),
                    MouseScrollDelta::PixelDelta(position) => {
                        ScrollDelta::Pixels(Vector2f::new(position.x as f32, position.y as f32))
                    }
                },
            }),
            None,
        ],
        WindowEvent::Focused(false) => [Some(InputEvent::FocusLost), None],
        _ => [None, None],
    };
    events.into_iter().flatten()
}

/// The events one keyboard input carries: the key change, and the text the
/// key committed when it typed something printable.
///
/// Repeat keeps its own kind - a repeat is not a new press - and types again,
/// which is what a held key in a text field does. Control-only text (Enter is
/// `"\r"`, Tab is `"\t"`) belongs to the key event: those keys act, they do
/// not type. Split out of [`translate_window_event`] because winit's key event
/// cannot be constructed outside winit, so this is the part tests drive
/// directly.
fn key_events(
    key: KeyCode,
    state: ElementState,
    repeat: bool,
    text: Option<&str>,
) -> [Option<InputEvent>; 2] {
    let mut events = [None, None];
    events[0] = Some(if repeat {
        InputEvent::KeyRepeat { key }
    } else {
        InputEvent::Key {
            key,
            state: button_state(state),
        }
    });
    if state == ElementState::Pressed {
        if let Some(text) = text {
            if text.chars().any(|character| !character.is_control()) {
                events[1] = Some(InputEvent::TextInput {
                    text: text.to_string(),
                });
            }
        }
    }
    events
}

/// The engine's modifier state for winit's.
fn modifier_state(state: ModifiersState) -> Modifiers {
    Modifiers {
        shift: state.shift_key(),
        control: state.control_key(),
        alt: state.alt_key(),
        super_key: state.super_key(),
    }
}

/// The input event an IME event carries, or `None` while the IME toggles.
fn ime_event(ime: &Ime) -> Option<InputEvent> {
    match ime {
        Ime::Preedit(text, cursor) => Some(InputEvent::ImePreedit {
            text: text.clone(),
            cursor: *cursor,
        }),
        Ime::Commit(text) => Some(InputEvent::ImeCommit { text: text.clone() }),
        Ime::Enabled | Ime::Disabled => None,
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
            translate_window_event(&WindowEvent::Focused(false)).collect::<Vec<_>>(),
            [InputEvent::FocusLost]
        );
        assert!(translate_window_event(&WindowEvent::Focused(true))
            .next()
            .is_none());
        assert_eq!(
            translate_device_event(&DeviceEvent::MouseMotion { delta: (3.0, -4.0) }),
            Some(InputEvent::MouseMotion {
                delta: Vector2f::new(3.0, -4.0),
            })
        );
    }

    #[test]
    fn a_typed_key_yields_its_key_change_and_its_text() {
        assert_eq!(
            key_events(KeyCode::KeyA, ElementState::Pressed, false, Some("a")),
            [
                Some(InputEvent::Key {
                    key: KeyCode::KeyA,
                    state: ButtonState::Pressed,
                }),
                Some(InputEvent::TextInput {
                    text: "a".to_string(),
                }),
            ]
        );
    }

    #[test]
    fn a_repeat_is_its_own_kind_and_types_again() {
        assert_eq!(
            key_events(KeyCode::KeyA, ElementState::Pressed, true, Some("a")),
            [
                Some(InputEvent::KeyRepeat { key: KeyCode::KeyA }),
                Some(InputEvent::TextInput {
                    text: "a".to_string(),
                }),
            ]
        );
    }

    #[test]
    fn control_only_text_stays_with_the_key_event() {
        assert_eq!(
            key_events(KeyCode::Enter, ElementState::Pressed, false, Some("\r")),
            [
                Some(InputEvent::Key {
                    key: KeyCode::Enter,
                    state: ButtonState::Pressed,
                }),
                None,
            ]
        );
    }

    #[test]
    fn a_released_key_never_types() {
        assert_eq!(
            key_events(KeyCode::KeyA, ElementState::Released, false, Some("a")),
            [
                Some(InputEvent::Key {
                    key: KeyCode::KeyA,
                    state: ButtonState::Released,
                }),
                None,
            ]
        );
    }

    #[test]
    fn the_modifier_state_translates() {
        let change = WindowEvent::ModifiersChanged(winit::event::Modifiers::from(
            ModifiersState::SHIFT | ModifiersState::CONTROL,
        ));
        assert_eq!(
            translate_window_event(&change).collect::<Vec<_>>(),
            [InputEvent::ModifiersChanged {
                modifiers: Modifiers {
                    shift: true,
                    control: true,
                    ..Modifiers::default()
                },
            }]
        );
    }

    #[test]
    fn ime_events_translate() {
        assert_eq!(
            translate_window_event(&WindowEvent::Ime(Ime::Preedit(
                "ka".to_string(),
                Some((0, 2)),
            )))
            .collect::<Vec<_>>(),
            [InputEvent::ImePreedit {
                text: "ka".to_string(),
                cursor: Some((0, 2)),
            }]
        );
        assert_eq!(
            translate_window_event(&WindowEvent::Ime(Ime::Commit("ka".to_string())))
                .collect::<Vec<_>>(),
            [InputEvent::ImeCommit {
                text: "ka".to_string(),
            }]
        );
        assert!(translate_window_event(&WindowEvent::Ime(Ime::Enabled))
            .next()
            .is_none());
    }
}

//! The shell's input bridge: its own window events to egui's per-frame input.
//!
//! # Responsibilities
//!
//! - [`raw_input`]: one frame of [`RawInput`] from the events the frontend
//!   collected for the shell this frame, plus the shell's geometry.
//! - [`map_key`] and [`modifiers`]: the pure conversions it composes, kept
//!   public so they stay testable.
//!
//! # Design
//!
//! The shell reads [`EguiShellInput::events`], never the game's `Input`: a
//! key typed into an Inspector field belongs to the UI, and a project system
//! must not see it (W-0038 Stage 1). The frontend routes window events by
//! window - the editor's own windows feed this list, the game's window feeds
//! the engine - and this bridge replays the list as egui events, carrying
//! the pointer position and the modifiers forward so every event reports the
//! state that was current where it sits in the stream.

// External crates
use egui::{Event, ImeEvent, Modifiers, MouseWheelUnit, Pos2, RawInput, Rect, Vec2};
use pill_core::math::Vector2f;
use pill_engine_core::input::{
    ButtonState, InputEvent, KeyCode, Modifiers as EngineModifiers, MouseButton, ScrollDelta,
};

// Current crate
use crate::boundary::EguiShellInput;

/// One frame of egui input, from the shell's event list.
pub fn raw_input(shell_input: &EguiShellInput) -> RawInput {
    let scale = if shell_input.scale_factor > 0.0 {
        shell_input.scale_factor
    } else {
        1.0
    };
    let mut walk = EventWalk::default();
    let mut events = Vec::new();
    for event in &shell_input.events {
        walk.push(event, scale, &mut events);
    }

    RawInput {
        screen_rect: Some(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(shell_input.screen_size.x, shell_input.screen_size.y),
        )),
        time: Some(shell_input.time),
        focused: shell_input.focused,
        modifiers: walk.modifiers,
        events,
        ..Default::default()
    }
}

/// The pointer position and modifier state an event stream carries forward.
///
/// egui's pointer events name their own position and modifiers, while the
/// engine reports those as separate events: the walk remembers the last one
/// of each so a button press reports where it landed and with which chords.
#[derive(Default)]
struct EventWalk {
    /// The modifier state the last `ModifiersChanged` reported.
    modifiers: Modifiers,
    /// The pointer's last known position, in logical points.
    cursor: Option<Pos2>,
}

impl EventWalk {
    /// Append the egui events `event` stands for; events the shell has no
    /// use for (raw motion, gamepads, focus) append nothing.
    fn push(&mut self, event: &InputEvent, scale: f32, out: &mut Vec<Event>) {
        let to_logical = |position: Vector2f| Pos2::new(position.x / scale, position.y / scale);
        match event {
            InputEvent::ModifiersChanged { modifiers: state } => {
                self.modifiers = modifiers(*state);
            }
            InputEvent::CursorMoved { position } => {
                let position = to_logical(*position);
                self.cursor = Some(position);
                out.push(Event::PointerMoved(position));
            }
            InputEvent::CursorLeft => {
                self.cursor = None;
                out.push(Event::PointerGone);
            }
            InputEvent::MouseButton { button, state } => {
                // Without a position egui cannot attribute the press to a
                // widget, so a button event before any move is dropped.
                let (Some(pos), Some(button)) = (self.cursor, pointer_button(*button)) else {
                    return;
                };
                out.push(Event::PointerButton {
                    pos,
                    button,
                    pressed: *state == ButtonState::Pressed,
                    modifiers: self.modifiers,
                });
            }
            InputEvent::MouseWheel { delta } => {
                let (unit, delta) = match delta {
                    ScrollDelta::Lines(lines) => {
                        (MouseWheelUnit::Line, Vec2::new(lines.x, lines.y))
                    }
                    ScrollDelta::Pixels(pixels) => (
                        MouseWheelUnit::Point,
                        Vec2::new(pixels.x / scale, pixels.y / scale),
                    ),
                };
                out.push(Event::MouseWheel {
                    unit,
                    delta,
                    modifiers: self.modifiers,
                });
            }
            InputEvent::Key { key, state } => {
                if let Some(key) = map_key(*key) {
                    out.push(Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: *state == ButtonState::Pressed,
                        repeat: false,
                        modifiers: self.modifiers,
                    });
                }
            }
            InputEvent::KeyRepeat { key } => {
                if let Some(key) = map_key(*key) {
                    out.push(Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed: true,
                        repeat: true,
                        modifiers: self.modifiers,
                    });
                }
            }
            InputEvent::TextInput { text } => out.push(Event::Text(text.clone())),
            InputEvent::ImePreedit { text, .. } => {
                out.push(Event::Ime(ImeEvent::Preedit(text.clone())));
            }
            InputEvent::ImeCommit { text } => out.push(Event::Ime(ImeEvent::Commit(text.clone()))),
            // Raw motion, focus and gamepads steer the game and the editor's
            // cameras, not the shell.
            InputEvent::MouseMotion { .. }
            | InputEvent::FocusLost
            | InputEvent::GamepadConnected { .. }
            | InputEvent::GamepadDisconnected { .. }
            | InputEvent::GamepadButton { .. }
            | InputEvent::GamepadAxis { .. } => {}
        }
    }
}

/// Map an engine mouse button to egui's pointer button.
pub fn pointer_button(button: MouseButton) -> Option<egui::PointerButton> {
    Some(match button {
        MouseButton::Left => egui::PointerButton::Primary,
        MouseButton::Right => egui::PointerButton::Secondary,
        MouseButton::Middle => egui::PointerButton::Middle,
        _ => return None,
    })
}

/// Convert the engine's modifier state.
pub fn modifiers(engine: EngineModifiers) -> Modifiers {
    Modifiers {
        alt: engine.alt,
        ctrl: engine.control,
        shift: engine.shift,
        mac_cmd: false,
        command: engine.control,
    }
}

/// Map an engine key to the egui key, or `None` for keys egui does not name.
pub fn map_key(key: KeyCode) -> Option<egui::Key> {
    use egui::Key;
    Some(match key {
        KeyCode::KeyA => Key::A,
        KeyCode::KeyB => Key::B,
        KeyCode::KeyC => Key::C,
        KeyCode::KeyD => Key::D,
        KeyCode::KeyE => Key::E,
        KeyCode::KeyF => Key::F,
        KeyCode::KeyG => Key::G,
        KeyCode::KeyH => Key::H,
        KeyCode::KeyI => Key::I,
        KeyCode::KeyJ => Key::J,
        KeyCode::KeyK => Key::K,
        KeyCode::KeyL => Key::L,
        KeyCode::KeyM => Key::M,
        KeyCode::KeyN => Key::N,
        KeyCode::KeyO => Key::O,
        KeyCode::KeyP => Key::P,
        KeyCode::KeyQ => Key::Q,
        KeyCode::KeyR => Key::R,
        KeyCode::KeyS => Key::S,
        KeyCode::KeyT => Key::T,
        KeyCode::KeyU => Key::U,
        KeyCode::KeyV => Key::V,
        KeyCode::KeyW => Key::W,
        KeyCode::KeyX => Key::X,
        KeyCode::KeyY => Key::Y,
        KeyCode::KeyZ => Key::Z,
        KeyCode::Digit0 => Key::Num0,
        KeyCode::Digit1 => Key::Num1,
        KeyCode::Digit2 => Key::Num2,
        KeyCode::Digit3 => Key::Num3,
        KeyCode::Digit4 => Key::Num4,
        KeyCode::Digit5 => Key::Num5,
        KeyCode::Digit6 => Key::Num6,
        KeyCode::Digit7 => Key::Num7,
        KeyCode::Digit8 => Key::Num8,
        KeyCode::Digit9 => Key::Num9,
        KeyCode::Numpad0 => Key::Num0,
        KeyCode::Numpad1 => Key::Num1,
        KeyCode::Numpad2 => Key::Num2,
        KeyCode::Numpad3 => Key::Num3,
        KeyCode::Numpad4 => Key::Num4,
        KeyCode::Numpad5 => Key::Num5,
        KeyCode::Numpad6 => Key::Num6,
        KeyCode::Numpad7 => Key::Num7,
        KeyCode::Numpad8 => Key::Num8,
        KeyCode::Numpad9 => Key::Num9,
        KeyCode::F1 => Key::F1,
        KeyCode::F2 => Key::F2,
        KeyCode::F3 => Key::F3,
        KeyCode::F4 => Key::F4,
        KeyCode::F5 => Key::F5,
        KeyCode::F6 => Key::F6,
        KeyCode::F7 => Key::F7,
        KeyCode::F8 => Key::F8,
        KeyCode::F9 => Key::F9,
        KeyCode::F10 => Key::F10,
        KeyCode::F11 => Key::F11,
        KeyCode::F12 => Key::F12,
        KeyCode::F13 => Key::F13,
        KeyCode::F14 => Key::F14,
        KeyCode::F15 => Key::F15,
        KeyCode::F16 => Key::F16,
        KeyCode::F17 => Key::F17,
        KeyCode::F18 => Key::F18,
        KeyCode::F19 => Key::F19,
        KeyCode::F20 => Key::F20,
        KeyCode::ArrowUp => Key::ArrowUp,
        KeyCode::ArrowDown => Key::ArrowDown,
        KeyCode::ArrowLeft => Key::ArrowLeft,
        KeyCode::ArrowRight => Key::ArrowRight,
        KeyCode::Enter | KeyCode::NumpadEnter => Key::Enter,
        KeyCode::Space => Key::Space,
        KeyCode::Tab => Key::Tab,
        KeyCode::Escape => Key::Escape,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Minus | KeyCode::NumpadSubtract => Key::Minus,
        KeyCode::Equal => Key::Equals,
        KeyCode::Comma | KeyCode::NumpadDecimal => Key::Comma,
        KeyCode::Period => Key::Period,
        KeyCode::Slash => Key::Slash,
        KeyCode::Backslash => Key::Backslash,
        KeyCode::Semicolon => Key::Semicolon,
        KeyCode::Quote => Key::Quote,
        KeyCode::BracketLeft => Key::OpenBracket,
        KeyCode::BracketRight => Key::CloseBracket,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_egui() {
        assert_eq!(map_key(KeyCode::KeyA), Some(egui::Key::A));
        assert_eq!(map_key(KeyCode::Digit7), Some(egui::Key::Num7));
        assert_eq!(map_key(KeyCode::ArrowUp), Some(egui::Key::ArrowUp));
        assert_eq!(map_key(KeyCode::F12), Some(egui::Key::F12));
        assert_eq!(map_key(KeyCode::LaunchMail), None);
    }

    #[test]
    fn modifiers_map_to_egui() {
        let engine = EngineModifiers {
            shift: true,
            control: true,
            alt: false,
            super_key: true,
        };
        let mapped = modifiers(engine);
        assert!(mapped.shift);
        assert!(mapped.ctrl);
        assert!(mapped.command);
        assert!(!mapped.alt);
    }

    #[test]
    fn raw_input_carries_geometry_and_replays_the_events() {
        let mut shell_input = EguiShellInput {
            screen_size: Vector2f::new(800.0, 600.0),
            scale_factor: 2.0,
            focused: true,
            time: 12.5,
            frames_per_second: 117.0,
            ..Default::default()
        };
        let raw = raw_input(&shell_input);
        assert_eq!(
            raw.screen_rect,
            Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)))
        );
        assert_eq!(raw.time, Some(12.5));
        assert!(raw.focused);
        // A frame without events is a frame without input: the shell reads
        // its own list, so nothing is invented from the game's state.
        assert!(raw.events.is_empty());

        shell_input.events = vec![InputEvent::CursorLeft];
        let raw = raw_input(&shell_input);
        assert!(matches!(raw.events.as_slice(), [Event::PointerGone]));
    }

    #[test]
    fn a_key_carries_the_modifiers_in_force_when_it_happened() {
        let shell_input = EguiShellInput {
            events: vec![
                InputEvent::ModifiersChanged {
                    modifiers: EngineModifiers {
                        control: true,
                        ..Default::default()
                    },
                },
                InputEvent::Key {
                    key: KeyCode::KeyD,
                    state: ButtonState::Pressed,
                },
                InputEvent::ModifiersChanged {
                    modifiers: EngineModifiers::default(),
                },
                InputEvent::Key {
                    key: KeyCode::KeyD,
                    state: ButtonState::Released,
                },
            ],
            ..Default::default()
        };
        let raw = raw_input(&shell_input);
        let [Event::Key {
            modifiers, pressed, ..
        }, Event::Key {
            modifiers: released_modifiers,
            pressed: released,
            ..
        }] = raw.events.as_slice()
        else {
            panic!("expected two key events, got {:?}", raw.events);
        };
        assert!(modifiers.ctrl);
        assert!(pressed);
        assert!(!released_modifiers.ctrl);
        assert!(!released);
        // The frame reports the state the stream ended in.
        assert!(!raw.modifiers.ctrl);
    }

    #[test]
    fn a_button_press_lands_where_the_pointer_was() {
        let shell_input = EguiShellInput {
            scale_factor: 2.0,
            events: vec![
                InputEvent::CursorMoved {
                    position: Vector2f::new(200.0, 100.0),
                },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: ButtonState::Pressed,
                },
            ],
            ..Default::default()
        };
        let raw = raw_input(&shell_input);
        let [Event::PointerMoved(moved), Event::PointerButton {
            pos,
            button,
            pressed,
            ..
        }] = raw.events.as_slice()
        else {
            panic!("expected a move and a press, got {:?}", raw.events);
        };
        assert_eq!(*moved, Pos2::new(100.0, 50.0));
        assert_eq!(*pos, Pos2::new(100.0, 50.0));
        assert_eq!(*button, egui::PointerButton::Primary);
        assert!(pressed);
    }

    #[test]
    fn a_button_before_any_move_is_dropped() {
        let shell_input = EguiShellInput {
            events: vec![InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ButtonState::Released,
            }],
            ..Default::default()
        };
        assert!(raw_input(&shell_input).events.is_empty());
    }

    #[test]
    fn text_and_ime_commit_forward_to_egui() {
        let shell_input = EguiShellInput {
            events: vec![
                InputEvent::TextInput {
                    text: "zzz".to_owned(),
                },
                InputEvent::ImePreedit {
                    text: "zo".to_owned(),
                    cursor: None,
                },
                InputEvent::ImeCommit {
                    text: "zoo".to_owned(),
                },
            ],
            ..Default::default()
        };
        let raw = raw_input(&shell_input);
        assert_eq!(
            raw.events,
            vec![
                Event::Text("zzz".to_owned()),
                Event::Ime(ImeEvent::Preedit("zo".to_owned())),
                Event::Ime(ImeEvent::Commit("zoo".to_owned())),
            ]
        );
    }
}

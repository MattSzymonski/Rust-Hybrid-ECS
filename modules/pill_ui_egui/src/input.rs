//! The engine-input to egui bridge.
//!
//! # Responsibilities
//!
//! - [`raw_input`]: one frame of [`RawInput`] from the engine's per-frame
//!   [`Input`] state and the shell's geometry.
//! - [`map_key`] and [`modifiers`]: the pure conversions it composes, kept
//!   public so they stay testable.
//!
//! # Design
//!
//! egui consumes a per-frame event stream; the engine keeps per-frame state
//! (what was held, pressed or released since the previous frame). The
//! bridge translates state into events the way `pill_ui`'s input routing
//! does - which drops key repeats and pointer deltas egui does not need.

// External crates
use egui::{Event, ImeEvent, Modifiers, MouseWheelUnit, Pos2, RawInput, Rect, Vec2};
use pill_core::math::Vector2f;
use pill_engine_core::input::{Input, KeyCode, Modifiers as EngineModifiers, MouseButton};

// Current crate
use crate::boundary::EguiShellInput;

/// One frame of egui input, from the engine's input state.
pub fn raw_input(engine: &Input, shell_input: &EguiShellInput) -> RawInput {
    let scale = if shell_input.scale_factor > 0.0 {
        shell_input.scale_factor
    } else {
        1.0
    };
    let to_logical = |position: Vector2f| Pos2::new(position.x / scale, position.y / scale);
    let modifiers = modifiers(engine.modifiers());
    let mut events = Vec::new();

    match engine.cursor_position() {
        Some(position) => events.push(Event::PointerMoved(to_logical(position))),
        None => events.push(Event::PointerGone),
    }
    if let Some(position) = engine.cursor_position().map(to_logical) {
        for (engine_button, button) in [
            (MouseButton::Left, egui::PointerButton::Primary),
            (MouseButton::Right, egui::PointerButton::Secondary),
            (MouseButton::Middle, egui::PointerButton::Middle),
        ] {
            if engine.mouse_button_pressed(engine_button) {
                events.push(Event::PointerButton {
                    pos: position,
                    button,
                    pressed: true,
                    modifiers,
                });
            }
            if engine.mouse_button_released(engine_button) {
                events.push(Event::PointerButton {
                    pos: position,
                    button,
                    pressed: false,
                    modifiers,
                });
            }
        }
    }
    let lines = engine.scroll_lines();
    if lines != Vector2f::ZERO {
        events.push(Event::MouseWheel {
            unit: MouseWheelUnit::Line,
            delta: Vec2::new(lines.x, lines.y),
            modifiers,
        });
    }
    let pixels = engine.scroll_pixels();
    if pixels != Vector2f::ZERO {
        events.push(Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: Vec2::new(pixels.x / scale, pixels.y / scale),
            modifiers,
        });
    }
    for key in engine.keys_pressed() {
        if let Some(key) = map_key(key) {
            events.push(Event::Key {
                key,
                physical_key: Some(key),
                pressed: true,
                repeat: false,
                modifiers,
            });
        }
    }
    for key in engine.keys_released() {
        if let Some(key) = map_key(key) {
            events.push(Event::Key {
                key,
                physical_key: Some(key),
                pressed: false,
                repeat: false,
                modifiers,
            });
        }
    }
    if !engine.text_entered().is_empty() {
        events.push(Event::Text(engine.text_entered().to_owned()));
    }
    if let Some(preedit) = engine.ime_preedit() {
        events.push(Event::Ime(ImeEvent::Preedit(preedit.to_owned())));
    }

    RawInput {
        screen_rect: Some(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(shell_input.screen_size.x, shell_input.screen_size.y),
        )),
        time: Some(shell_input.time),
        focused: shell_input.focused,
        modifiers,
        events,
        ..Default::default()
    }
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
    fn raw_input_carries_geometry_and_pointer_gone() {
        let engine = Input::new();
        let geometry = EguiShellInput {
            screen_size: Vector2f::new(800.0, 600.0),
            scale_factor: 2.0,
            focused: true,
            time: 12.5,
        };
        let raw = raw_input(&engine, &geometry);
        assert_eq!(
            raw.screen_rect,
            Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)))
        );
        assert_eq!(raw.time, Some(12.5));
        assert!(raw.focused);
        assert!(matches!(raw.events.as_slice(), [Event::PointerGone]));
    }
}

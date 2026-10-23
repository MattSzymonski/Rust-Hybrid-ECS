//! The engine's input snapshot, captured and turned into egui's input.
//!
//! # Responsibilities
//!
//! - [`InputFrame`]: one frame of engine input, owned, so a system can capture
//!   it before borrowing the UI resource mutably.
//! - [`raw_input`]: that frame as egui's [`RawInput`], against the viewport the
//!   UI draws into.
//! - [`map_key`], [`pointer_button`] and [`modifiers`]: the pure conversions it
//!   composes.
//! - [`InputMemory`]: the two things a snapshot cannot state - the modifiers it
//!   last reported and where the cursor was.
//!
//! # Design
//!
//! The engine reports input as a *snapshot* (what is held, what went down or
//! up this frame, the accumulated text and scroll), not as an event list, so
//! the bridge derives egui's events from the snapshot's edges and remembers
//! the previous value of the two that only exist as "the current state".
//!
//! Order matters to egui: modifiers change before the keys they apply to, and
//! the pointer moves before a button press that names its position, so events
//! are emitted in that order rather than the snapshot's field order.
//!
//! The editor's equivalent (`pill_editor_core_egui::input`) is the shape this mirrors,
//! deliberately not a dependency: that crate carries `egui_dock` and the
//! editor's boundary types, and a game build must not link the docking crate
//! (plan decision D2).
//!
//! [`InputFrame`] exists because the resource borrows do not overlap: a system
//! holds `&Input` and `&mut GameUi` from the same world at once, so the input
//! is captured first and the borrow released.

// External crates
use egui::{Event, ImeEvent, Modifiers, MouseWheelUnit, Pos2, RawInput, Rect, Vec2};
use pill_core::math::Vector2f;
use pill_engine_core::input::{Input, KeyCode, Modifiers as EngineModifiers, MouseButton};

// =============================================================================
// InputFrame
// =============================================================================

/// One frame of engine input, owned.
///
/// Captured from the engine's [`Input`] snapshot at the top of a system; the
/// fields are the snapshot's own values, not a summary, so the bridge below
/// sees exactly what the engine saw.
#[derive(Clone, Debug, Default)]
pub struct InputFrame {
    /// Where the cursor is, in physical pixels, when it is over the window.
    pub cursor: Option<Vector2f>,
    /// Which modifiers are held.
    pub modifiers: EngineModifiers,
    /// The keys that went down this frame.
    pub keys_pressed: Vec<KeyCode>,
    /// The keys that went up this frame.
    pub keys_released: Vec<KeyCode>,
    /// The buttons that went down this frame.
    pub buttons_pressed: Vec<MouseButton>,
    /// The buttons that went up this frame.
    pub buttons_released: Vec<MouseButton>,
    /// The text typed (or pasted) this frame.
    pub text: String,
    /// The IME composition string, when one is active.
    pub preedit: Option<String>,
    /// The scroll, in lines.
    pub scroll_lines: Vector2f,
    /// The scroll, in physical pixels.
    pub scroll_pixels: Vector2f,
}

impl InputFrame {
    /// Capture one frame of `input`.
    pub fn capture(input: &Input) -> InputFrame {
        InputFrame {
            cursor: input.cursor_position(),
            modifiers: input.modifiers(),
            keys_pressed: input.keys_pressed().collect(),
            keys_released: input.keys_released().collect(),
            buttons_pressed: [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
                .into_iter()
                .filter(|button| input.mouse_button_pressed(*button))
                .collect(),
            buttons_released: [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
                .into_iter()
                .filter(|button| input.mouse_button_released(*button))
                .collect(),
            text: input.text_entered().to_owned(),
            preedit: input.ime_preedit().map(str::to_owned),
            scroll_lines: input.scroll_lines(),
            scroll_pixels: input.scroll_pixels(),
        }
    }
}

// =============================================================================
// InputMemory
// =============================================================================

/// What the previous frame told the bridge, so the next frame can report the
/// changes a snapshot states only as "the current value".
#[derive(Debug, Default)]
pub struct InputMemory {
    /// The modifier state the previous frame reported.
    modifiers: EngineModifiers,
    /// The cursor position the previous frame reported, in logical points.
    cursor: Option<Pos2>,
}

impl InputMemory {
    /// Fresh memory: no modifiers held and no cursor seen yet.
    pub fn new() -> InputMemory {
        InputMemory::default()
    }

    /// The modifier state the last frame reported.
    pub fn modifiers(&self) -> EngineModifiers {
        self.modifiers
    }

    /// The cursor position the last frame reported, in logical points.
    pub fn cursor(&self) -> Option<Pos2> {
        self.cursor
    }
}

// =============================================================================
// raw_input
// =============================================================================

/// One frame of egui input from `frame`.
///
/// `viewport` is the UI's logical size (the surface's, divided by
/// `scale_factor`), `scale_factor` the monitor scale, and `time` the seconds
/// since the UI's first pass, which egui uses for animation.
pub fn raw_input(
    frame: &InputFrame,
    viewport: Vector2f,
    scale_factor: f32,
    time: f64,
    memory: &mut InputMemory,
) -> RawInput {
    let scale = if scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let mut events = Vec::new();

    // Step 1: the modifier state, when it changed, before anything it applies
    // to.
    let modifiers = modifiers(frame.modifiers);
    if frame.modifiers != memory.modifiers {
        memory.modifiers = frame.modifiers;
        events.push(Event::ModifiersChanged(modifiers));
    }

    // Step 2: where the pointer is, and whether it left.
    let cursor = frame
        .cursor
        .map(|position| Pos2::new(position.x / scale, position.y / scale));
    match cursor {
        Some(position) if memory.cursor != Some(position) => {
            memory.cursor = Some(position);
            events.push(Event::PointerMoved(position));
        }
        None if memory.cursor.is_some() => {
            memory.cursor = None;
            events.push(Event::PointerGone);
        }
        _ => {}
    }

    // Step 3: buttons, each naming the position it landed at. A press with no
    // known pointer position is dropped: egui could not attribute it to a
    // widget.
    if let Some(position) = memory.cursor {
        for button in &frame.buttons_pressed {
            if let Some(pointer) = pointer_button(*button) {
                events.push(Event::PointerButton {
                    pos: position,
                    button: pointer,
                    pressed: true,
                    modifiers,
                });
            }
        }
        for button in &frame.buttons_released {
            if let Some(pointer) = pointer_button(*button) {
                events.push(Event::PointerButton {
                    pos: position,
                    button: pointer,
                    pressed: false,
                    modifiers,
                });
            }
        }
    }

    // Step 4: scrolling, in whatever unit the platform reported.
    if frame.scroll_lines.x != 0.0 || frame.scroll_lines.y != 0.0 {
        events.push(Event::MouseWheel {
            unit: MouseWheelUnit::Line,
            delta: Vec2::new(frame.scroll_lines.x, frame.scroll_lines.y),
            modifiers,
            phase: egui::TouchPhase::Move,
        });
    }
    if frame.scroll_pixels.x != 0.0 || frame.scroll_pixels.y != 0.0 {
        events.push(Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: Vec2::new(frame.scroll_pixels.x / scale, frame.scroll_pixels.y / scale),
            modifiers,
            phase: egui::TouchPhase::Move,
        });
    }

    // Step 5: keys.
    for key in &frame.keys_pressed {
        if let Some(key) = map_key(*key) {
            events.push(Event::Key {
                key,
                physical_key: Some(key),
                pressed: true,
                repeat: false,
                modifiers,
            });
        }
    }
    for key in &frame.keys_released {
        if let Some(key) = map_key(*key) {
            events.push(Event::Key {
                key,
                physical_key: Some(key),
                pressed: false,
                repeat: false,
                modifiers,
            });
        }
    }

    // Step 6: composed text and the IME's composition. The engine reports a
    // commit as text, which is what egui inserts; a composition arrives as the
    // preedit string, without the character range a platform may not report.
    if !frame.text.is_empty() {
        events.push(Event::Text(frame.text.clone()));
    }
    if let Some(preedit) = &frame.preedit {
        events.push(Event::Ime(ImeEvent::Preedit {
            text: preedit.clone(),
            active_range_chars: None,
        }));
    }

    RawInput {
        screen_rect: Some(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(viewport.x, viewport.y),
        )),
        time: Some(time),
        // A game window only reports input while it is focused.
        focused: true,
        events,
        ..Default::default()
    }
}

// =============================================================================
// Conversions
// =============================================================================

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
        mac_cmd: engine.super_key,
        command: engine.control,
    }
}

/// Map an engine key code onto egui's.
///
/// The mapping covers the letters, digits, function keys, arrows and the
/// editing keys a game UI uses; anything else is dropped rather than guessed
/// at.
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
        KeyCode::Enter => Key::Enter,
        KeyCode::Escape => Key::Escape,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Tab => Key::Tab,
        KeyCode::Space => Key::Space,
        KeyCode::Delete => Key::Delete,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::ArrowUp => Key::ArrowUp,
        KeyCode::ArrowDown => Key::ArrowDown,
        KeyCode::ArrowLeft => Key::ArrowLeft,
        KeyCode::ArrowRight => Key::ArrowRight,
        KeyCode::Minus => Key::Minus,
        KeyCode::Equal => Key::Equals,
        KeyCode::BracketLeft => Key::OpenBracket,
        KeyCode::BracketRight => Key::CloseBracket,
        KeyCode::Backslash => Key::Backslash,
        KeyCode::Semicolon => Key::Semicolon,
        KeyCode::Quote => Key::Quote,
        KeyCode::Comma => Key::Comma,
        KeyCode::Period => Key::Period,
        KeyCode::Slash => Key::Slash,
        KeyCode::Backquote => Key::Backtick,
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
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine_core::input::{ButtonState, InputEvent, ScrollDelta};
    use pill_engine_core::Engine;

    /// The engine's input snapshot after one frame with `events` queued.
    fn captured(events: &[InputEvent]) -> InputFrame {
        let mut engine = Engine::new();
        for event in events {
            engine.push_input_event(event.clone());
        }
        engine.process_frame().expect("the frame runs");
        let input = engine
            .world()
            .get_resource::<Input>()
            .expect("the engine installs its input");
        InputFrame::capture(input)
    }

    #[test]
    fn a_key_press_and_release_become_egui_key_events() {
        let frame = captured(&[
            InputEvent::Key {
                key: KeyCode::KeyW,
                state: ButtonState::Pressed,
            },
            InputEvent::Key {
                key: KeyCode::KeyW,
                state: ButtonState::Released,
            },
        ]);
        assert_eq!(frame.keys_pressed, vec![KeyCode::KeyW]);
        let mut memory = InputMemory::new();
        let raw = raw_input(&frame, Vector2f::new(320.0, 200.0), 1.0, 0.5, &mut memory);
        let keys: Vec<_> = raw
            .events
            .iter()
            .filter_map(|event| match event {
                Event::Key { key, pressed, .. } => Some((*key, *pressed)),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec![(egui::Key::W, true), (egui::Key::W, false)]);
        assert_eq!(
            raw.screen_rect,
            Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(320.0, 200.0)))
        );
        assert_eq!(raw.time, Some(0.5));
    }

    #[test]
    fn a_modifier_change_is_reported_once() {
        let frame = captured(&[InputEvent::ModifiersChanged {
            modifiers: EngineModifiers {
                control: true,
                ..Default::default()
            },
        }]);
        let mut memory = InputMemory::new();
        let first = raw_input(&frame, Vector2f::new(64.0, 64.0), 1.0, 0.0, &mut memory);
        assert!(first
            .events
            .iter()
            .any(|event| matches!(event, Event::ModifiersChanged(state) if state.ctrl)));
        // The next frame reports the same state: nothing to say.
        let second = raw_input(&frame, Vector2f::new(64.0, 64.0), 1.0, 0.1, &mut memory);
        assert!(!second
            .events
            .iter()
            .any(|event| matches!(event, Event::ModifiersChanged(_))));
    }

    #[test]
    fn a_press_needs_a_pointer_position_and_scales_it() {
        let frame = captured(&[
            InputEvent::CursorMoved {
                position: Vector2f::new(200.0, 100.0),
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                state: ButtonState::Pressed,
            },
        ]);
        assert_eq!(frame.buttons_pressed, vec![MouseButton::Left]);
        let mut memory = InputMemory::new();
        let raw = raw_input(&frame, Vector2f::new(160.0, 100.0), 2.0, 0.0, &mut memory);
        let (position, pressed, button) = raw
            .events
            .iter()
            .find_map(|event| match event {
                Event::PointerButton {
                    pos,
                    pressed,
                    button,
                    ..
                } => Some((*pos, *pressed, *button)),
                _ => None,
            })
            .expect("the press is reported");
        assert!(pressed);
        assert_eq!(button, egui::PointerButton::Primary);
        // 200 engine pixels over a 2x monitor is 100 logical points.
        assert_eq!(position, Pos2::new(100.0, 50.0));
        assert_eq!(
            memory.cursor(),
            Some(Pos2::new(100.0, 50.0)),
            "the memory carries the position into the next frame"
        );
    }

    #[test]
    fn a_press_with_no_position_yet_is_dropped() {
        let frame = InputFrame {
            buttons_pressed: vec![MouseButton::Left],
            ..Default::default()
        };
        let mut memory = InputMemory::new();
        let raw = raw_input(&frame, Vector2f::new(64.0, 64.0), 1.0, 0.0, &mut memory);
        assert!(!raw
            .events
            .iter()
            .any(|event| matches!(event, Event::PointerButton { .. })));
    }

    #[test]
    fn a_cursor_leaving_reports_pointer_gone_once() {
        let mut memory = InputMemory::new();
        let present = InputFrame {
            cursor: Some(Vector2f::new(4.0, 4.0)),
            ..Default::default()
        };
        raw_input(&present, Vector2f::new(64.0, 64.0), 1.0, 0.0, &mut memory);
        let gone = InputFrame::default();
        let raw = raw_input(&gone, Vector2f::new(64.0, 64.0), 1.0, 0.1, &mut memory);
        assert_eq!(
            raw.events
                .iter()
                .filter(|event| matches!(event, Event::PointerGone))
                .count(),
            1
        );
        let again = raw_input(&gone, Vector2f::new(64.0, 64.0), 1.0, 0.2, &mut memory);
        assert!(!again
            .events
            .iter()
            .any(|event| matches!(event, Event::PointerGone)));
    }

    #[test]
    fn typed_text_and_a_preedit_reach_egui() {
        let frame = captured(&[InputEvent::TextInput {
            text: "zoo".to_owned(),
        }]);
        assert_eq!(frame.text, "zoo");
        let mut memory = InputMemory::new();
        let raw = raw_input(&frame, Vector2f::new(64.0, 64.0), 1.0, 0.0, &mut memory);
        assert!(raw
            .events
            .iter()
            .any(|event| matches!(event, Event::Text(text) if text == "zoo")));

        let composing = InputFrame {
            preedit: Some("zo".to_owned()),
            ..Default::default()
        };
        let raw = raw_input(&composing, Vector2f::new(64.0, 64.0), 1.0, 0.1, &mut memory);
        assert!(raw.events.iter().any(|event| matches!(
            event,
            Event::Ime(ImeEvent::Preedit { text, .. }) if text == "zo"
        )));
    }

    #[test]
    fn wheel_units_survive_and_pixels_scale() {
        let frame = captured(&[
            InputEvent::MouseWheel {
                delta: ScrollDelta::Lines(Vector2f::new(0.0, -3.0)),
            },
            InputEvent::MouseWheel {
                delta: ScrollDelta::Pixels(Vector2f::new(0.0, 40.0)),
            },
        ]);
        let mut memory = InputMemory::new();
        let raw = raw_input(&frame, Vector2f::new(64.0, 64.0), 2.0, 0.0, &mut memory);
        let wheels: Vec<_> = raw
            .events
            .iter()
            .filter_map(|event| match event {
                Event::MouseWheel { unit, delta, .. } => Some((*unit, *delta)),
                _ => None,
            })
            .collect();
        assert_eq!(wheels.len(), 2);
        assert_eq!(wheels[0], (MouseWheelUnit::Line, Vec2::new(0.0, -3.0)));
        // 40 physical pixels over a 2x monitor is 20 logical points.
        assert_eq!(wheels[1], (MouseWheelUnit::Point, Vec2::new(0.0, 20.0)));
    }
}

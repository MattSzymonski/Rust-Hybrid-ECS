//! The engine's input events, kept the way UI reads them.
//!
//! # Responsibilities
//!
//! - [`UiInput`]: remembers the pointer, mouse buttons, modifiers, scroll,
//!   keys and committed text across a frame, fed by engine [`InputEvent`]s
//!   ([`UiInput::apply`]) and cleared at the frame boundary
//!   ([`UiInput::begin_frame`]).
//! - Tracks edges the way the engine does - `pressed` and `released` are true
//!   for exactly one frame - plus the ordered lists UI navigation needs.
//!
//! # Design
//!
//! `pill_ui` never reads a platform library; a frontend translates winit (or
//! the browser) into [`InputEvent`]s and hands each one to both the engine's
//! [`Input`](pill_engine_core::Input) resource and this type. What UI adds over
//! the engine resource is ordering (which keys were pressed, in which order),
//! repeats as a first-class list for held-key navigation, and text entry
//! (committed characters and the IME composition). Gamepad input is not part
//! of the UI contract.

// External crates
use pill_core::math::Vector2f;
use pill_engine_core::{ButtonState, InputEvent, KeyCode, Modifiers, MouseButton, ScrollDelta};

// =============================================================================
// UiInput
// =============================================================================

/// One frame's worth of input, as UI reads it.
///
/// Values that describe an instant (`pointer`, `modifiers`, the IME
/// composition) persist until replaced; values that describe the frame
/// (`text`, the edges, scroll, motion) are cleared by
/// [`Self::begin_frame`].
#[derive(Clone, Debug, Default)]
pub struct UiInput {
    /// Where the pointer is, or `None` while it is outside the UI.
    pointer: Option<Vector2f>,
    /// Pointer motion accumulated since the previous frame, in physical
    /// pixels, from the platform's raw mouse reports.
    pointer_motion: Vector2f,
    /// Mouse button bits down at the start of the frame.
    mouse_held: u8,
    /// Mouse button bits that went down since the previous frame.
    mouse_pressed: u8,
    /// Mouse button bits that came up since the previous frame.
    mouse_released: u8,
    /// Line scrolling accumulated since the previous frame.
    scroll_lines: Vector2f,
    /// Pixel scrolling accumulated since the previous frame.
    scroll_pixels: Vector2f,
    /// The modifier keys' state, as last reported.
    modifiers: Modifiers,
    /// Text committed since the previous frame: typed characters and IME
    /// commits, in order.
    text: String,
    /// The IME's active composition text, while one is composing.
    ime_preedit: Option<String>,
    /// The composing cursor range within [`Self::ime_preedit`], as byte
    /// offsets, when the platform reports one.
    ime_cursor: Option<(usize, usize)>,
    /// Keys down at the start of the frame, in the order pressed.
    keys_held: Vec<KeyCode>,
    /// Keys that went down since the previous frame, in order.
    keys_pressed: Vec<KeyCode>,
    /// The modifier state in effect when each of `keys_pressed` arrived.
    /// Text fields read these rather than the live modifiers: a key and
    /// its modifier release can land in the same frame (injected chords),
    /// and the press still carried the modifier.
    keys_pressed_modifiers: Vec<(KeyCode, Modifiers)>,
    /// Keys that came up since the previous frame, in order.
    keys_released: Vec<KeyCode>,
    /// Keys that repeated since the previous frame, in order.
    keys_repeated: Vec<KeyCode>,
    /// The modifier state in effect when each of `keys_repeated` arrived.
    keys_repeated_modifiers: Vec<(KeyCode, Modifiers)>,
    /// Events applied at the next [`Self::begin_frame`], in arrival order.
    pending: Vec<InputEvent>,
}

impl UiInput {
    /// Input with nothing held.
    pub fn new() -> UiInput {
        UiInput::default()
    }

    /// Queue one engine input event for the next frame.
    ///
    /// Called by the frontend between frames, in arrival order. The events
    /// are applied by [`Self::begin_frame`], so a UI pass sees the same
    /// edges the engine's [`Input`](pill_engine_core::Input) resource reports.
    pub fn apply(&mut self, event: &InputEvent) {
        self.pending.push(event.clone());
    }

    /// Apply one queued event to the frame being built.
    fn apply_event(&mut self, event: InputEvent) {
        match event {
            InputEvent::Key { key, state } => self.apply_key(key, state),
            InputEvent::KeyRepeat { key } => {
                self.keys_repeated.push(key);
                self.keys_repeated_modifiers.push((key, self.modifiers));
            }
            InputEvent::ModifiersChanged { modifiers } => self.modifiers = modifiers,
            InputEvent::TextInput { text } => self.text.push_str(&text),
            InputEvent::ImePreedit { text, cursor } => {
                if text.is_empty() {
                    self.ime_preedit = None;
                    self.ime_cursor = None;
                } else {
                    self.ime_preedit = Some(text);
                    self.ime_cursor = cursor;
                }
            }
            InputEvent::ImeCommit { text } => {
                self.text.push_str(&text);
                self.ime_preedit = None;
                self.ime_cursor = None;
            }
            InputEvent::MouseButton { button, state } => self.apply_mouse_button(button, state),
            InputEvent::CursorMoved { position } => self.pointer = Some(position),
            InputEvent::CursorLeft => self.pointer = None,
            InputEvent::MouseMotion { delta } => self.pointer_motion += delta,
            InputEvent::MouseWheel {
                delta: ScrollDelta::Lines(delta),
            } => self.scroll_lines += delta,
            InputEvent::MouseWheel {
                delta: ScrollDelta::Pixels(delta),
            } => self.scroll_pixels += delta,
            InputEvent::FocusLost => {
                // The releases will go to whoever holds the focus now; UI
                // needs no release edges for keys it no longer sees.
                self.keys_held.clear();
                self.modifiers = Modifiers::default();
                self.ime_preedit = None;
                self.ime_cursor = None;
            }
            // Gamepads are not part of the UI contract.
            InputEvent::GamepadConnected { .. }
            | InputEvent::GamepadDisconnected { .. }
            | InputEvent::GamepadButton { .. }
            | InputEvent::GamepadAxis { .. } => {}
        }
    }

    /// Start a frame: forget the previous frame's edges and accumulation,
    /// then apply every queued event in arrival order.
    ///
    /// The pointer, the modifier state, held keys and the IME composition
    /// are not edges; they persist until something replaces them.
    pub fn begin_frame(&mut self) {
        self.pointer_motion = Vector2f::ZERO;
        self.mouse_pressed = 0;
        self.mouse_released = 0;
        self.scroll_lines = Vector2f::ZERO;
        self.scroll_pixels = Vector2f::ZERO;
        self.text.clear();
        self.keys_pressed.clear();
        self.keys_pressed_modifiers.clear();
        self.keys_released.clear();
        self.keys_repeated.clear();
        self.keys_repeated_modifiers.clear();

        // Taken out and put back so the buffer's capacity is reused.
        let mut events = std::mem::take(&mut self.pending);
        for event in events.drain(..) {
            self.apply_event(event);
        }
        self.pending = events;
    }

    /// Where the pointer is, or `None` while it is outside the UI.
    pub fn pointer(&self) -> Option<Vector2f> {
        self.pointer
    }

    /// Pointer motion since the previous frame, in physical pixels.
    pub fn pointer_motion(&self) -> Vector2f {
        self.pointer_motion
    }

    /// Whether `button` is down.
    pub fn mouse_button_held(&self, button: MouseButton) -> bool {
        self.mouse_held & button_bit(button) != 0
    }

    /// Whether `button` went down since the previous frame.
    pub fn mouse_button_pressed(&self, button: MouseButton) -> bool {
        self.mouse_pressed & button_bit(button) != 0
    }

    /// Whether `button` came up since the previous frame.
    pub fn mouse_button_released(&self, button: MouseButton) -> bool {
        self.mouse_released & button_bit(button) != 0
    }

    /// Line scrolling since the previous frame; positive `y` is up.
    pub fn scroll_lines(&self) -> Vector2f {
        self.scroll_lines
    }

    /// Pixel scrolling since the previous frame; positive `y` is up.
    pub fn scroll_pixels(&self) -> Vector2f {
        self.scroll_pixels
    }

    /// The modifier keys' state.
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// Text committed since the previous frame, in order.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The IME's active composition text, or `None` while not composing.
    pub fn ime_preedit(&self) -> Option<&str> {
        self.ime_preedit.as_deref()
    }

    /// The composing cursor range within [`Self::ime_preedit`], as byte
    /// offsets, when the platform reports one.
    pub fn ime_cursor(&self) -> Option<(usize, usize)> {
        self.ime_cursor
    }

    /// Whether `key` is down.
    pub fn key_held(&self, key: KeyCode) -> bool {
        self.keys_held.contains(&key)
    }

    /// Whether `key` went down since the previous frame.
    pub fn key_pressed(&self, key: KeyCode) -> bool {
        self.keys_pressed.contains(&key)
    }

    /// Whether `key` repeated since the previous frame.
    pub fn key_repeated(&self, key: KeyCode) -> bool {
        self.keys_repeated.contains(&key)
    }

    /// Every key down, in the order pressed.
    pub fn keys_held(&self) -> &[KeyCode] {
        &self.keys_held
    }

    /// Every key that went down since the previous frame, in order.
    pub fn keys_pressed(&self) -> &[KeyCode] {
        &self.keys_pressed
    }

    /// Every key that went down since the previous frame with the modifiers
    /// in effect at its arrival, in order.
    pub fn keys_pressed_modifiers(&self) -> &[(KeyCode, Modifiers)] {
        &self.keys_pressed_modifiers
    }

    /// Every key that repeated since the previous frame with the modifiers
    /// in effect at its arrival, in order.
    pub fn keys_repeated_modifiers(&self) -> &[(KeyCode, Modifiers)] {
        &self.keys_repeated_modifiers
    }

    /// Every key that came up since the previous frame, in order.
    pub fn keys_released(&self) -> &[KeyCode] {
        &self.keys_released
    }

    /// Every key that repeated since the previous frame, in order.
    pub fn keys_repeated(&self) -> &[KeyCode] {
        &self.keys_repeated
    }

    /// Apply a key change: a press while already held changes nothing (the
    /// engine reports repeats separately).
    fn apply_key(&mut self, key: KeyCode, state: ButtonState) {
        match state {
            ButtonState::Pressed => {
                if !self.keys_held.contains(&key) {
                    self.keys_held.push(key);
                    self.keys_pressed.push(key);
                    self.keys_pressed_modifiers.push((key, self.modifiers));
                }
            }
            ButtonState::Released => {
                if let Some(index) = self.keys_held.iter().position(|held| *held == key) {
                    self.keys_held.remove(index);
                    self.keys_released.push(key);
                }
            }
        }
    }

    /// Apply a mouse button change, with the same rules as [`Self::apply_key`].
    fn apply_mouse_button(&mut self, button: MouseButton, state: ButtonState) {
        let bit = button_bit(button);
        match state {
            ButtonState::Pressed if self.mouse_held & bit == 0 => {
                self.mouse_held |= bit;
                self.mouse_pressed |= bit;
            }
            ButtonState::Released if self.mouse_held & bit != 0 => {
                self.mouse_held &= !bit;
                self.mouse_released |= bit;
            }
            ButtonState::Pressed | ButtonState::Released => {}
        }
    }
}

/// The bit one mouse button occupies, matching the engine's layout.
fn button_bit(button: MouseButton) -> u8 {
    1 << (button as u8)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: KeyCode, state: ButtonState) -> InputEvent {
        InputEvent::Key { key, state }
    }

    fn next_frame(input: &mut UiInput) {
        input.begin_frame();
    }

    #[test]
    fn a_click_is_an_edge_for_one_frame_and_held_until_released() {
        let mut input = UiInput::new();
        input.apply(&InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Pressed,
        });
        next_frame(&mut input);
        assert!(input.mouse_button_pressed(MouseButton::Left));
        assert!(input.mouse_button_held(MouseButton::Left));

        next_frame(&mut input);
        assert!(!input.mouse_button_pressed(MouseButton::Left));
        assert!(input.mouse_button_held(MouseButton::Left));

        input.apply(&InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Released,
        });
        next_frame(&mut input);
        assert!(input.mouse_button_released(MouseButton::Left));
        assert!(!input.mouse_button_held(MouseButton::Left));
        assert!(!input.mouse_button_pressed(MouseButton::Left));
    }

    #[test]
    fn buttons_other_than_the_pressed_one_stay_quiet() {
        let mut input = UiInput::new();
        input.apply(&InputEvent::MouseButton {
            button: MouseButton::Middle,
            state: ButtonState::Pressed,
        });
        next_frame(&mut input);
        assert!(input.mouse_button_pressed(MouseButton::Middle));
        assert!(!input.mouse_button_pressed(MouseButton::Left));
        assert!(!input.mouse_button_held(MouseButton::Right));
    }

    #[test]
    fn keys_keep_their_order_and_repeats_are_listed() {
        let mut input = UiInput::new();
        input.apply(&key(KeyCode::ArrowDown, ButtonState::Pressed));
        input.apply(&key(KeyCode::ArrowRight, ButtonState::Pressed));
        input.apply(&InputEvent::KeyRepeat {
            key: KeyCode::ArrowDown,
        });
        next_frame(&mut input);
        assert_eq!(
            input.keys_pressed(),
            [KeyCode::ArrowDown, KeyCode::ArrowRight]
        );
        assert_eq!(input.keys_repeated(), [KeyCode::ArrowDown]);
        assert!(input.key_repeated(KeyCode::ArrowDown));
        assert!(!input.key_repeated(KeyCode::ArrowRight));

        next_frame(&mut input);
        assert!(input.keys_repeated().is_empty());
        assert_eq!(input.keys_held(), [KeyCode::ArrowDown, KeyCode::ArrowRight]);

        input.apply(&key(KeyCode::ArrowDown, ButtonState::Released));
        next_frame(&mut input);
        assert_eq!(input.keys_released(), [KeyCode::ArrowDown]);
        assert_eq!(input.keys_held(), [KeyCode::ArrowRight]);
    }

    #[test]
    fn a_press_of_a_held_key_is_not_a_second_edge() {
        let mut input = UiInput::new();
        input.apply(&key(KeyCode::KeyW, ButtonState::Pressed));
        next_frame(&mut input);
        input.apply(&key(KeyCode::KeyW, ButtonState::Pressed));
        next_frame(&mut input);
        assert!(!input.key_pressed(KeyCode::KeyW));
        assert!(input.key_held(KeyCode::KeyW));
    }

    #[test]
    fn text_and_ime_commits_add_up_and_the_preedit_clears() {
        let mut input = UiInput::new();
        input.apply(&InputEvent::ImePreedit {
            text: "ka".to_string(),
            cursor: Some((0, 2)),
        });
        next_frame(&mut input);
        assert_eq!(input.ime_preedit(), Some("ka"));
        assert_eq!(input.ime_cursor(), Some((0, 2)));
        assert_eq!(input.text(), "");

        input.apply(&InputEvent::TextInput {
            text: "a".to_string(),
        });
        input.apply(&InputEvent::ImeCommit {
            text: "ka".to_string(),
        });
        next_frame(&mut input);
        assert_eq!(input.text(), "aka");
        next_frame(&mut input);
        assert_eq!(input.text(), "");
        assert_eq!(input.ime_preedit(), None);
    }

    #[test]
    fn focus_loss_drops_held_keys_and_the_composition() {
        let mut input = UiInput::new();
        input.apply(&key(KeyCode::ShiftLeft, ButtonState::Pressed));
        input.apply(&InputEvent::ModifiersChanged {
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        });
        input.apply(&InputEvent::ImePreedit {
            text: "wa".to_string(),
            cursor: None,
        });
        input.apply(&InputEvent::FocusLost);
        next_frame(&mut input);
        assert!(input.keys_held().is_empty());
        assert_eq!(input.modifiers(), Modifiers::default());
        assert_eq!(input.ime_preedit(), None);
    }

    #[test]
    fn motion_scroll_and_the_pointer_persist_and_reset_per_frame() {
        let mut input = UiInput::new();
        input.apply(&InputEvent::CursorMoved {
            position: Vector2f::new(3.0, 4.0),
        });
        input.apply(&InputEvent::MouseMotion {
            delta: Vector2f::new(1.0, -1.0),
        });
        input.apply(&InputEvent::MouseWheel {
            delta: ScrollDelta::Lines(Vector2f::new(0.0, 2.0)),
        });
        input.apply(&InputEvent::MouseWheel {
            delta: ScrollDelta::Pixels(Vector2f::new(0.0, -20.0)),
        });
        next_frame(&mut input);
        assert_eq!(input.pointer(), Some(Vector2f::new(3.0, 4.0)));
        assert_eq!(input.pointer_motion(), Vector2f::new(1.0, -1.0));
        assert_eq!(input.scroll_lines(), Vector2f::new(0.0, 2.0));
        assert_eq!(input.scroll_pixels(), Vector2f::new(0.0, -20.0));

        next_frame(&mut input);
        assert_eq!(input.pointer(), Some(Vector2f::new(3.0, 4.0)));
        assert_eq!(input.pointer_motion(), Vector2f::ZERO);
        assert_eq!(input.scroll_lines(), Vector2f::ZERO);
        assert_eq!(input.scroll_pixels(), Vector2f::ZERO);

        input.apply(&InputEvent::CursorLeft);
        next_frame(&mut input);
        assert_eq!(input.pointer(), None);
    }
}

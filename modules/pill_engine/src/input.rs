//! Keyboard, mouse and gamepad state, maintained by the engine and readable
//! from any system.
//!
//! # Responsibilities
//!
//! - Defines [`Input`], the engine-owned input resource, and the plain-data
//!   vocabulary it is fed with: [`InputEvent`], [`KeyCode`], [`MouseButton`],
//!   [`GamepadButton`], [`GamepadAxis`], [`PlayerId`].
//! - Tracks, per frame, what is held and what was pressed or released since the
//!   previous frame, plus the cursor, mouse motion and scroll accumulated over
//!   it.
//! - Queues rumble requests from systems for the frontend that owns the
//!   gamepads to play.
//!
//! # Design
//!
//! The engine knows no windowing or gamepad library. A frontend translates its
//! platform's events (winit, the editor's DOM, gilrs) into [`InputEvent`]s and
//! queues them between frames through
//! [`Engine::push_input_event`](crate::engine::Engine::push_input_event). The
//! engine applies the queue at the top of the next frame, right after advancing
//! [`Time`](crate::time::Time) and for the same reason: the scheduler orders
//! systems independently of registration, so an "input system" could run after
//! its readers. Every system in a frame therefore sees the same input.
//!
//! Key codes name physical keys (the W3C `KeyboardEvent.code` vocabulary, as
//! winit does), not the characters a layout produces: `KeyW` is the key left of
//! `KeyE` on every layout, which is what movement bindings want. Text entry is
//! not covered.
//!
//! The resource is defined in `pill_engine`, which every artifact compiles
//! identically, so a project DLL reads the host's instance through `Res<Input>`
//! across a reload, exactly like `Time`.
//!
//! Frame semantics:
//! - `*_held`: down at the start of this frame.
//! - `*_pressed` / `*_released`: changed since the previous frame. A press and
//!   a release inside one frame report both, so a short tap is never lost.
//!   Operating-system key repeat does not produce new presses.
//! - Mouse motion and scroll add up over the frame; the cursor position is the
//!   latest one.

// Standard library
use std::time::Duration;

// External crates
use pill_core::math::Vector2f;

// Current crate
use crate::resource::Resource;

// =============================================================================
// Constants
// =============================================================================

/// Number of gamepads tracked at once, one per [`PlayerId`].
pub const GAMEPAD_SLOT_COUNT: usize = 4;

/// Axis values closer to zero than this read as exactly zero.
///
/// Resting sticks rarely report a clean zero, and without a deadzone a
/// character drifts while nobody touches the pad.
pub const GAMEPAD_AXIS_DEADZONE: f32 = 0.05;

/// Words in the key bit set: 256 bits, room for every [`KeyCode`].
const KEY_SET_WORDS: usize = 4;

// =============================================================================
// KeyCode
// =============================================================================

/// Declares [`KeyCode`] with each key's W3C `KeyboardEvent.code` name.
///
/// One list produces the enum, [`KeyCode::ALL`], [`KeyCode::code_name`] and
/// [`KeyCode::from_code_name`], so the four cannot drift apart.
macro_rules! key_codes {
    ($($variant:ident => $code_name:literal),* $(,)?) => {
        /// A physical key, named after its position on a US layout.
        ///
        /// The variants match winit's `KeyCode`, and their
        /// [`code_name`](Self::code_name)s are the W3C `KeyboardEvent.code`
        /// values a browser reports.
        #[repr(u8)]
        #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum KeyCode {
            $($variant),*
        }

        impl KeyCode {
            /// Every key, in declaration order.
            pub const ALL: &'static [KeyCode] = &[$(KeyCode::$variant),*];

            /// The W3C `KeyboardEvent.code` name of this key.
            pub const fn code_name(self) -> &'static str {
                match self {
                    $(KeyCode::$variant => $code_name),*
                }
            }

            /// The key a W3C `KeyboardEvent.code` value names, or `None` for
            /// a code the engine does not know.
            ///
            /// Also accepts `OSLeft`/`OSRight`, which older Firefox versions
            /// report for the Windows/Command keys.
            pub fn from_code_name(code_name: &str) -> Option<KeyCode> {
                match code_name {
                    $($code_name => Some(KeyCode::$variant),)*
                    "OSLeft" => Some(KeyCode::SuperLeft),
                    "OSRight" => Some(KeyCode::SuperRight),
                    _ => None,
                }
            }
        }
    };
}

key_codes! {
    // Writing system keys.
    Backquote => "Backquote", Backslash => "Backslash", BracketLeft => "BracketLeft",
    BracketRight => "BracketRight", Comma => "Comma",
    Digit0 => "Digit0", Digit1 => "Digit1", Digit2 => "Digit2", Digit3 => "Digit3",
    Digit4 => "Digit4", Digit5 => "Digit5", Digit6 => "Digit6", Digit7 => "Digit7",
    Digit8 => "Digit8", Digit9 => "Digit9",
    Equal => "Equal", IntlBackslash => "IntlBackslash", IntlRo => "IntlRo", IntlYen => "IntlYen",
    KeyA => "KeyA", KeyB => "KeyB", KeyC => "KeyC", KeyD => "KeyD", KeyE => "KeyE",
    KeyF => "KeyF", KeyG => "KeyG", KeyH => "KeyH", KeyI => "KeyI", KeyJ => "KeyJ",
    KeyK => "KeyK", KeyL => "KeyL", KeyM => "KeyM", KeyN => "KeyN", KeyO => "KeyO",
    KeyP => "KeyP", KeyQ => "KeyQ", KeyR => "KeyR", KeyS => "KeyS", KeyT => "KeyT",
    KeyU => "KeyU", KeyV => "KeyV", KeyW => "KeyW", KeyX => "KeyX", KeyY => "KeyY",
    KeyZ => "KeyZ",
    Minus => "Minus", Period => "Period", Quote => "Quote", Semicolon => "Semicolon",
    Slash => "Slash",
    // Functional keys.
    AltLeft => "AltLeft", AltRight => "AltRight", Backspace => "Backspace",
    CapsLock => "CapsLock", ContextMenu => "ContextMenu", ControlLeft => "ControlLeft",
    ControlRight => "ControlRight", Enter => "Enter", SuperLeft => "MetaLeft",
    SuperRight => "MetaRight", ShiftLeft => "ShiftLeft", ShiftRight => "ShiftRight",
    Space => "Space", Tab => "Tab",
    Convert => "Convert", KanaMode => "KanaMode", Lang1 => "Lang1", Lang2 => "Lang2",
    Lang3 => "Lang3", Lang4 => "Lang4", Lang5 => "Lang5", NonConvert => "NonConvert",
    // Control pad and arrows.
    Delete => "Delete", End => "End", Help => "Help", Home => "Home", Insert => "Insert",
    PageDown => "PageDown", PageUp => "PageUp",
    ArrowDown => "ArrowDown", ArrowLeft => "ArrowLeft", ArrowRight => "ArrowRight",
    ArrowUp => "ArrowUp",
    // Numpad.
    NumLock => "NumLock",
    Numpad0 => "Numpad0", Numpad1 => "Numpad1", Numpad2 => "Numpad2", Numpad3 => "Numpad3",
    Numpad4 => "Numpad4", Numpad5 => "Numpad5", Numpad6 => "Numpad6", Numpad7 => "Numpad7",
    Numpad8 => "Numpad8", Numpad9 => "Numpad9",
    NumpadAdd => "NumpadAdd", NumpadBackspace => "NumpadBackspace",
    NumpadClear => "NumpadClear", NumpadClearEntry => "NumpadClearEntry",
    NumpadComma => "NumpadComma", NumpadDecimal => "NumpadDecimal",
    NumpadDivide => "NumpadDivide", NumpadEnter => "NumpadEnter", NumpadEqual => "NumpadEqual",
    NumpadHash => "NumpadHash", NumpadMemoryAdd => "NumpadMemoryAdd",
    NumpadMemoryClear => "NumpadMemoryClear", NumpadMemoryRecall => "NumpadMemoryRecall",
    NumpadMemoryStore => "NumpadMemoryStore", NumpadMemorySubtract => "NumpadMemorySubtract",
    NumpadMultiply => "NumpadMultiply", NumpadParenLeft => "NumpadParenLeft",
    NumpadParenRight => "NumpadParenRight", NumpadStar => "NumpadStar",
    NumpadSubtract => "NumpadSubtract",
    // Function section.
    Escape => "Escape", Fn => "Fn", FnLock => "FnLock", PrintScreen => "PrintScreen",
    ScrollLock => "ScrollLock", Pause => "Pause",
    // Media and browser keys.
    BrowserBack => "BrowserBack", BrowserFavorites => "BrowserFavorites",
    BrowserForward => "BrowserForward", BrowserHome => "BrowserHome",
    BrowserRefresh => "BrowserRefresh", BrowserSearch => "BrowserSearch",
    BrowserStop => "BrowserStop", Eject => "Eject", LaunchApp1 => "LaunchApp1",
    LaunchApp2 => "LaunchApp2", LaunchMail => "LaunchMail", MediaPlayPause => "MediaPlayPause",
    MediaSelect => "MediaSelect", MediaStop => "MediaStop", MediaTrackNext => "MediaTrackNext",
    MediaTrackPrevious => "MediaTrackPrevious", Power => "Power", Sleep => "Sleep",
    AudioVolumeDown => "AudioVolumeDown", AudioVolumeMute => "AudioVolumeMute",
    AudioVolumeUp => "AudioVolumeUp", WakeUp => "WakeUp",
    // Legacy and non-standard keys.
    Meta => "Meta", Hyper => "Hyper", Turbo => "Turbo", Abort => "Abort", Resume => "Resume",
    Suspend => "Suspend", Again => "Again", Copy => "Copy", Cut => "Cut", Find => "Find",
    Open => "Open", Paste => "Paste", Props => "Props", Select => "Select", Undo => "Undo",
    Hiragana => "Hiragana", Katakana => "Katakana",
    // Function keys.
    F1 => "F1", F2 => "F2", F3 => "F3", F4 => "F4", F5 => "F5", F6 => "F6", F7 => "F7",
    F8 => "F8", F9 => "F9", F10 => "F10", F11 => "F11", F12 => "F12", F13 => "F13",
    F14 => "F14", F15 => "F15", F16 => "F16", F17 => "F17", F18 => "F18", F19 => "F19",
    F20 => "F20", F21 => "F21", F22 => "F22", F23 => "F23", F24 => "F24", F25 => "F25",
    F26 => "F26", F27 => "F27", F28 => "F28", F29 => "F29", F30 => "F30", F31 => "F31",
    F32 => "F32", F33 => "F33", F34 => "F34", F35 => "F35",
}

// Every key needs a bit in the key set.
const _: () = assert!(KeyCode::ALL.len() <= KEY_SET_WORDS * 64);

// =============================================================================
// Buttons, Axes and Players
// =============================================================================

/// Whether a key or button went down or came up.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ButtonState {
    /// The key or button went down.
    Pressed,
    /// The key or button came up.
    Released,
}

/// A mouse button.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// The primary button, usually the left one.
    Left,
    /// The secondary button, usually the right one.
    Right,
    /// The wheel button.
    Middle,
    /// The "browser back" side button.
    Back,
    /// The "browser forward" side button.
    Forward,
}

impl MouseButton {
    /// Every mouse button, in declaration order.
    pub const ALL: [MouseButton; 5] = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ];

    /// This button's bit in a button mask.
    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// A gamepad button, named after the Xbox layout.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GamepadButton {
    /// The bottom face button (Xbox A, PlayStation Cross).
    A,
    /// The right face button (Xbox B, PlayStation Circle).
    B,
    /// The left face button (Xbox X, PlayStation Square).
    X,
    /// The top face button (Xbox Y, PlayStation Triangle).
    Y,
    /// The left shoulder button.
    LeftBumper,
    /// The right shoulder button.
    RightBumper,
    /// The left trigger, as a button; its travel is [`GamepadAxis::LeftTrigger`].
    LeftTrigger,
    /// The right trigger, as a button; its travel is [`GamepadAxis::RightTrigger`].
    RightTrigger,
    /// The left menu button (Back, View, Select, Share).
    Back,
    /// The right menu button (Start, Menu, Options).
    Start,
    /// The guide button (Xbox, PlayStation).
    Mode,
    /// Pressing the left stick in.
    LeftStick,
    /// Pressing the right stick in.
    RightStick,
    /// D-pad up.
    DPadUp,
    /// D-pad down.
    DPadDown,
    /// D-pad left.
    DPadLeft,
    /// D-pad right.
    DPadRight,
}

impl GamepadButton {
    /// This button's bit in a button mask.
    const fn bit(self) -> u32 {
        1 << self as u8
    }
}

/// A gamepad axis. Sticks range over `-1.0..=1.0` (up and right are positive),
/// triggers over `0.0..=1.0`.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GamepadAxis {
    /// Left stick, horizontal.
    LeftStickX,
    /// Left stick, vertical.
    LeftStickY,
    /// Right stick, horizontal.
    RightStickX,
    /// Right stick, vertical.
    RightStickY,
    /// Left trigger travel.
    LeftTrigger,
    /// Right trigger travel.
    RightTrigger,
    /// D-pad as an axis, on pads that report it as one.
    DPadX,
    /// D-pad as an axis, on pads that report it as one.
    DPadY,
}

impl GamepadAxis {
    /// Number of axes.
    pub const COUNT: usize = GamepadAxis::DPadY as usize + 1;
}

/// The player a gamepad belongs to.
///
/// Gamepads take the lowest free slot when they connect, and keep it until
/// they disconnect.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PlayerId {
    /// The first slot.
    Player1,
    /// The second slot.
    Player2,
    /// The third slot.
    Player3,
    /// The fourth slot.
    Player4,
}

impl PlayerId {
    /// Every slot, in order.
    pub const ALL: [PlayerId; GAMEPAD_SLOT_COUNT] = [
        PlayerId::Player1,
        PlayerId::Player2,
        PlayerId::Player3,
        PlayerId::Player4,
    ];

    /// The slot at `index`, or `None` past the last one.
    pub const fn from_index(index: usize) -> Option<PlayerId> {
        if index < GAMEPAD_SLOT_COUNT {
            Some(Self::ALL[index])
        } else {
            None
        }
    }

    /// This slot's zero-based index.
    pub const fn index(self) -> usize {
        self as usize
    }
}

// =============================================================================
// Events and Requests
// =============================================================================

/// How far a mouse wheel turned.
///
/// Positive `y` scrolls up (away from the user), positive `x` scrolls right.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ScrollDelta {
    /// Notched wheels: lines (or rows) to scroll.
    Lines(Vector2f),
    /// Touchpads and smooth wheels: physical pixels to scroll.
    Pixels(Vector2f),
}

/// One input change, as a frontend reports it.
///
/// Positions and deltas are in physical pixels; positions are relative to the
/// top-left corner of the area the game is drawn in.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// A key went down or came up.
    Key {
        /// Which key.
        key: KeyCode,
        /// Down or up.
        state: ButtonState,
    },
    /// A mouse button went down or came up.
    MouseButton {
        /// Which button.
        button: MouseButton,
        /// Down or up.
        state: ButtonState,
    },
    /// The cursor moved over the game.
    CursorMoved {
        /// The new position.
        position: Vector2f,
    },
    /// The cursor left the game's area.
    CursorLeft,
    /// The mouse moved, independent of the cursor: unaccelerated where the
    /// platform reports it, and not stopped by the window border.
    MouseMotion {
        /// The movement.
        delta: Vector2f,
    },
    /// The mouse wheel or touchpad scrolled.
    MouseWheel {
        /// How far.
        delta: ScrollDelta,
    },
    /// The game lost keyboard focus: everything held is released, because the
    /// matching releases will go to someone else.
    FocusLost,
    /// A gamepad connected and took a slot.
    GamepadConnected {
        /// The slot it took.
        player: PlayerId,
    },
    /// A gamepad disconnected and freed its slot.
    GamepadDisconnected {
        /// The slot it freed.
        player: PlayerId,
    },
    /// A gamepad button went down or came up.
    GamepadButton {
        /// Whose gamepad.
        player: PlayerId,
        /// Which button.
        button: GamepadButton,
        /// Down or up.
        state: ButtonState,
    },
    /// A gamepad axis moved.
    GamepadAxis {
        /// Whose gamepad.
        player: PlayerId,
        /// Which axis.
        axis: GamepadAxis,
        /// The raw value, before the deadzone.
        value: f32,
    },
}

/// A request to rumble a player's gamepad, queued by a system for the
/// frontend to play.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RumbleRequest {
    /// Whose gamepad.
    pub player: PlayerId,
    /// High-frequency motor strength, `0.0..=1.0`.
    pub weak: f32,
    /// Low-frequency motor strength, `0.0..=1.0`.
    pub strong: f32,
    /// How long to rumble.
    pub duration: Duration,
}

// =============================================================================
// KeySet
// =============================================================================

/// One bit per [`KeyCode`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct KeySet([u64; KEY_SET_WORDS]);

impl KeySet {
    /// Whether `key` is in the set.
    fn contains(&self, key: KeyCode) -> bool {
        let index = key as usize;
        self.0[index / 64] & (1 << (index % 64)) != 0
    }

    /// Add or remove `key`.
    fn set(&mut self, key: KeyCode, present: bool) {
        let index = key as usize;
        let bit = 1 << (index % 64);
        if present {
            self.0[index / 64] |= bit;
        } else {
            self.0[index / 64] &= !bit;
        }
    }

    /// Every key in the set, in [`KeyCode`] order.
    fn iter(&self) -> impl Iterator<Item = KeyCode> + '_ {
        KeyCode::ALL
            .iter()
            .copied()
            .filter(|key| self.contains(*key))
    }
}

// =============================================================================
// GamepadState
// =============================================================================

/// One slot's gamepad.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
struct GamepadState {
    /// Whether a gamepad holds this slot.
    connected: bool,
    /// [`GamepadButton`] bits down at the start of the frame.
    held: u32,
    /// Bits that went down since the previous frame.
    pressed: u32,
    /// Bits that came up since the previous frame.
    released: u32,
    /// Latest value per [`GamepadAxis`], after the deadzone.
    axes: [f32; GamepadAxis::COUNT],
}

// =============================================================================
// Input
// =============================================================================

/// Keyboard, mouse and gamepad state for the current frame.
///
/// Inserted automatically by the engine; a project never constructs one.
/// Read it from any system:
///
/// ```no_run
/// # use pill_engine::*;
/// fn jump(input: Res<Input>) {
///     let Some(input) = input.get() else { return };
///     if input.key_pressed(KeyCode::Space)
///         || input.gamepad_button_pressed(PlayerId::Player1, GamepadButton::A)
///     {
///         // Start the jump.
///     }
/// }
/// ```
///
/// Every value describes the frame being processed and stays fixed for its
/// whole duration. Headless runs never receive events, so everything reads as
/// released and zero.
#[derive(Debug, Default)]
pub struct Input {
    /// Keys down at the start of the frame.
    keys_held: KeySet,
    /// Keys that went down since the previous frame.
    keys_pressed: KeySet,
    /// Keys that came up since the previous frame.
    keys_released: KeySet,
    /// [`MouseButton`] bits down at the start of the frame.
    mouse_held: u8,
    /// Mouse button bits that went down since the previous frame.
    mouse_pressed: u8,
    /// Mouse button bits that came up since the previous frame.
    mouse_released: u8,
    /// The latest cursor position over the game, or `None` when it is not
    /// over it.
    cursor_position: Option<Vector2f>,
    /// Mouse motion accumulated since the previous frame.
    mouse_delta: Vector2f,
    /// Line scrolling accumulated since the previous frame.
    scroll_lines: Vector2f,
    /// Pixel scrolling accumulated since the previous frame.
    scroll_pixels: Vector2f,
    /// One state per [`PlayerId`].
    gamepads: [GamepadState; GAMEPAD_SLOT_COUNT],
    /// Events queued since the previous frame, applied at the next one.
    pending_events: Vec<InputEvent>,
    /// Rumble requests not yet taken by the frontend.
    rumble_requests: Vec<RumbleRequest>,
}

impl Resource for Input {}

impl Input {
    /// Input with nothing held and no gamepad connected.
    pub fn new() -> Self {
        Self::default()
    }

    // -------------------------------------------------------------------------
    // Keyboard
    // -------------------------------------------------------------------------

    /// Whether `key` is down this frame.
    pub fn key_held(&self, key: KeyCode) -> bool {
        self.keys_held.contains(key)
    }

    /// Whether `key` went down since the previous frame.
    pub fn key_pressed(&self, key: KeyCode) -> bool {
        self.keys_pressed.contains(key)
    }

    /// Whether `key` came up since the previous frame.
    pub fn key_released(&self, key: KeyCode) -> bool {
        self.keys_released.contains(key)
    }

    /// Every key down this frame.
    pub fn keys_held(&self) -> impl Iterator<Item = KeyCode> + '_ {
        self.keys_held.iter()
    }

    /// Every key that went down since the previous frame.
    pub fn keys_pressed(&self) -> impl Iterator<Item = KeyCode> + '_ {
        self.keys_pressed.iter()
    }

    // -------------------------------------------------------------------------
    // Mouse
    // -------------------------------------------------------------------------

    /// Whether `button` is down this frame.
    pub fn mouse_button_held(&self, button: MouseButton) -> bool {
        self.mouse_held & button.bit() != 0
    }

    /// Whether `button` went down since the previous frame.
    pub fn mouse_button_pressed(&self, button: MouseButton) -> bool {
        self.mouse_pressed & button.bit() != 0
    }

    /// Whether `button` came up since the previous frame.
    pub fn mouse_button_released(&self, button: MouseButton) -> bool {
        self.mouse_released & button.bit() != 0
    }

    /// The cursor position in physical pixels from the top-left corner of the
    /// game's area, or `None` while the cursor is elsewhere.
    pub fn cursor_position(&self) -> Option<Vector2f> {
        self.cursor_position
    }

    /// Mouse movement since the previous frame, in physical pixels.
    ///
    /// Keeps reporting at the window border, so it is the value for mouse-look.
    pub fn mouse_delta(&self) -> Vector2f {
        self.mouse_delta
    }

    /// Wheel lines scrolled since the previous frame; positive `y` is up.
    pub fn scroll_lines(&self) -> Vector2f {
        self.scroll_lines
    }

    /// Touchpad pixels scrolled since the previous frame; positive `y` is up.
    pub fn scroll_pixels(&self) -> Vector2f {
        self.scroll_pixels
    }

    // -------------------------------------------------------------------------
    // Gamepads
    // -------------------------------------------------------------------------

    /// Whether `player` has a gamepad connected.
    pub fn gamepad_connected(&self, player: PlayerId) -> bool {
        self.gamepads[player.index()].connected
    }

    /// Every player with a gamepad connected.
    pub fn connected_gamepads(&self) -> impl Iterator<Item = PlayerId> + '_ {
        PlayerId::ALL
            .into_iter()
            .filter(|player| self.gamepad_connected(*player))
    }

    /// Whether `player`'s `button` is down this frame.
    pub fn gamepad_button_held(&self, player: PlayerId, button: GamepadButton) -> bool {
        self.gamepads[player.index()].held & button.bit() != 0
    }

    /// Whether `player`'s `button` went down since the previous frame.
    pub fn gamepad_button_pressed(&self, player: PlayerId, button: GamepadButton) -> bool {
        self.gamepads[player.index()].pressed & button.bit() != 0
    }

    /// Whether `player`'s `button` came up since the previous frame.
    pub fn gamepad_button_released(&self, player: PlayerId, button: GamepadButton) -> bool {
        self.gamepads[player.index()].released & button.bit() != 0
    }

    /// `player`'s `axis`, after the deadzone; zero without a gamepad.
    pub fn gamepad_axis(&self, player: PlayerId, axis: GamepadAxis) -> f32 {
        self.gamepads[player.index()].axes[axis as usize]
    }

    /// Ask the frontend to rumble `player`'s gamepad.
    ///
    /// Strengths are clamped to `0.0..=1.0`. Played after the frame by the
    /// frontend that owns the gamepads; ignored where there is none (headless,
    /// the browser) or the gamepad cannot rumble.
    pub fn request_rumble(&mut self, player: PlayerId, weak: f32, strong: f32, duration: Duration) {
        self.rumble_requests.push(RumbleRequest {
            player,
            weak: weak.clamp(0.0, 1.0),
            strong: strong.clamp(0.0, 1.0),
            duration,
        });
    }

    /// Take every rumble request queued so far; the frontend's half of
    /// [`Self::request_rumble`].
    pub fn take_rumble_requests(&mut self) -> Vec<RumbleRequest> {
        std::mem::take(&mut self.rumble_requests)
    }

    // -------------------------------------------------------------------------
    // Feeding
    // -------------------------------------------------------------------------

    /// Queue `event` for the next frame.
    ///
    /// Frontends call this between frames, through
    /// [`Engine::push_input_event`](crate::engine::Engine::push_input_event).
    /// A system may also queue events, to simulate input; they take effect
    /// next frame.
    pub fn push_event(&mut self, event: InputEvent) {
        self.pending_events.push(event);
    }

    /// Start a frame: forget the previous frame's edges and motion, then
    /// apply every queued event in order.
    ///
    /// Called by the engine at the top of each frame, before systems run.
    pub(crate) fn begin_frame(&mut self) {
        self.keys_pressed = KeySet::default();
        self.keys_released = KeySet::default();
        self.mouse_pressed = 0;
        self.mouse_released = 0;
        self.mouse_delta = Vector2f::ZERO;
        self.scroll_lines = Vector2f::ZERO;
        self.scroll_pixels = Vector2f::ZERO;
        for gamepad in &mut self.gamepads {
            gamepad.pressed = 0;
            gamepad.released = 0;
        }

        // Taken out and put back so the buffer's capacity is reused.
        let mut events = std::mem::take(&mut self.pending_events);
        for event in events.drain(..) {
            self.apply(event);
        }
        self.pending_events = events;
    }

    /// Apply one event to the current frame's state.
    fn apply(&mut self, event: InputEvent) {
        match event {
            InputEvent::Key { key, state } => self.apply_key(key, state),
            InputEvent::MouseButton { button, state } => self.apply_mouse_button(button, state),
            InputEvent::CursorMoved { position } => self.cursor_position = Some(position),
            InputEvent::CursorLeft => self.cursor_position = None,
            InputEvent::MouseMotion { delta } => self.mouse_delta += delta,
            InputEvent::MouseWheel {
                delta: ScrollDelta::Lines(delta),
            } => self.scroll_lines += delta,
            InputEvent::MouseWheel {
                delta: ScrollDelta::Pixels(delta),
            } => self.scroll_pixels += delta,
            InputEvent::FocusLost => self.release_keyboard_and_mouse(),
            InputEvent::GamepadConnected { player } => {
                self.gamepads[player.index()] = GamepadState {
                    connected: true,
                    ..GamepadState::default()
                };
            }
            InputEvent::GamepadDisconnected { player } => {
                self.gamepads[player.index()] = GamepadState::default();
            }
            InputEvent::GamepadButton {
                player,
                button,
                state,
            } => {
                let gamepad = &mut self.gamepads[player.index()];
                let bit = button.bit();
                match state {
                    // Held already: a repeat, not a new press.
                    ButtonState::Pressed if gamepad.held & bit == 0 => {
                        gamepad.held |= bit;
                        gamepad.pressed |= bit;
                    }
                    ButtonState::Pressed => {}
                    ButtonState::Released if gamepad.held & bit != 0 => {
                        gamepad.held &= !bit;
                        gamepad.released |= bit;
                    }
                    ButtonState::Released => {}
                }
            }
            InputEvent::GamepadAxis {
                player,
                axis,
                value,
            } => {
                let value = if value.abs() < GAMEPAD_AXIS_DEADZONE {
                    0.0
                } else {
                    value
                };
                self.gamepads[player.index()].axes[axis as usize] = value;
            }
        }
    }

    /// Apply a key change. A press of a held key is the operating system's
    /// repeat and changes nothing; a release of a key that is not held (one
    /// already released by [`InputEvent::FocusLost`]) changes nothing either.
    fn apply_key(&mut self, key: KeyCode, state: ButtonState) {
        let held = self.keys_held.contains(key);
        match state {
            ButtonState::Pressed if !held => {
                self.keys_held.set(key, true);
                self.keys_pressed.set(key, true);
            }
            ButtonState::Released if held => {
                self.keys_held.set(key, false);
                self.keys_released.set(key, true);
            }
            ButtonState::Pressed | ButtonState::Released => {}
        }
    }

    /// Apply a mouse button change, with the same rules as [`Self::apply_key`].
    fn apply_mouse_button(&mut self, button: MouseButton, state: ButtonState) {
        let bit = button.bit();
        let held = self.mouse_held & bit != 0;
        match state {
            ButtonState::Pressed if !held => {
                self.mouse_held |= bit;
                self.mouse_pressed |= bit;
            }
            ButtonState::Released if held => {
                self.mouse_held &= !bit;
                self.mouse_released |= bit;
            }
            ButtonState::Pressed | ButtonState::Released => {}
        }
    }

    /// Release every held key and mouse button, reporting each as released.
    fn release_keyboard_and_mouse(&mut self) {
        for word in 0..KEY_SET_WORDS {
            self.keys_released.0[word] |= self.keys_held.0[word];
        }
        self.keys_held = KeySet::default();
        self.mouse_released |= self.mouse_held;
        self.mouse_held = 0;
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Queue `events` and start a frame.
    fn frame(input: &mut Input, events: &[InputEvent]) {
        for event in events {
            input.push_event(*event);
        }
        input.begin_frame();
    }

    fn key(key: KeyCode, state: ButtonState) -> InputEvent {
        InputEvent::Key { key, state }
    }

    #[test]
    fn a_press_is_an_edge_for_one_frame_and_held_until_released() {
        let mut input = Input::new();
        frame(&mut input, &[key(KeyCode::KeyW, ButtonState::Pressed)]);
        assert!(input.key_pressed(KeyCode::KeyW));
        assert!(input.key_held(KeyCode::KeyW));

        frame(&mut input, &[]);
        assert!(!input.key_pressed(KeyCode::KeyW));
        assert!(input.key_held(KeyCode::KeyW));

        frame(&mut input, &[key(KeyCode::KeyW, ButtonState::Released)]);
        assert!(input.key_released(KeyCode::KeyW));
        assert!(!input.key_held(KeyCode::KeyW));

        frame(&mut input, &[]);
        assert!(!input.key_released(KeyCode::KeyW));
    }

    #[test]
    fn a_tap_inside_one_frame_reports_both_edges() {
        let mut input = Input::new();
        frame(
            &mut input,
            &[
                key(KeyCode::Space, ButtonState::Pressed),
                key(KeyCode::Space, ButtonState::Released),
            ],
        );
        assert!(input.key_pressed(KeyCode::Space));
        assert!(input.key_released(KeyCode::Space));
        assert!(!input.key_held(KeyCode::Space));
    }

    #[test]
    fn key_repeat_is_not_a_new_press() {
        let mut input = Input::new();
        frame(&mut input, &[key(KeyCode::KeyA, ButtonState::Pressed)]);
        frame(&mut input, &[key(KeyCode::KeyA, ButtonState::Pressed)]);
        assert!(!input.key_pressed(KeyCode::KeyA));
        assert!(input.key_held(KeyCode::KeyA));
    }

    #[test]
    fn a_repeat_in_the_frame_of_the_press_keeps_the_press() {
        let mut input = Input::new();
        frame(
            &mut input,
            &[
                key(KeyCode::KeyA, ButtonState::Pressed),
                key(KeyCode::KeyA, ButtonState::Pressed),
            ],
        );
        assert!(input.key_pressed(KeyCode::KeyA));
    }

    #[test]
    fn focus_loss_releases_everything_held() {
        let mut input = Input::new();
        frame(
            &mut input,
            &[
                key(KeyCode::ShiftLeft, ButtonState::Pressed),
                key(KeyCode::F35, ButtonState::Pressed),
                InputEvent::MouseButton {
                    button: MouseButton::Right,
                    state: ButtonState::Pressed,
                },
            ],
        );
        frame(&mut input, &[InputEvent::FocusLost]);
        assert!(input.key_released(KeyCode::ShiftLeft));
        assert!(input.key_released(KeyCode::F35));
        assert!(!input.key_held(KeyCode::ShiftLeft));
        assert!(input.mouse_button_released(MouseButton::Right));
        assert!(!input.mouse_button_held(MouseButton::Right));

        // The real release arriving later is not a second edge.
        frame(
            &mut input,
            &[key(KeyCode::ShiftLeft, ButtonState::Released)],
        );
        assert!(!input.key_released(KeyCode::ShiftLeft));
    }

    #[test]
    fn motion_and_scroll_add_up_over_a_frame_and_reset_after_it() {
        let mut input = Input::new();
        frame(
            &mut input,
            &[
                InputEvent::MouseMotion {
                    delta: Vector2f::new(3.0, -1.0),
                },
                InputEvent::MouseMotion {
                    delta: Vector2f::new(2.0, -1.0),
                },
                InputEvent::MouseWheel {
                    delta: ScrollDelta::Lines(Vector2f::new(0.0, 1.0)),
                },
                InputEvent::MouseWheel {
                    delta: ScrollDelta::Lines(Vector2f::new(0.0, 2.0)),
                },
                InputEvent::MouseWheel {
                    delta: ScrollDelta::Pixels(Vector2f::new(0.0, -40.0)),
                },
            ],
        );
        assert_eq!(input.mouse_delta(), Vector2f::new(5.0, -2.0));
        assert_eq!(input.scroll_lines(), Vector2f::new(0.0, 3.0));
        assert_eq!(input.scroll_pixels(), Vector2f::new(0.0, -40.0));

        frame(&mut input, &[]);
        assert_eq!(input.mouse_delta(), Vector2f::ZERO);
        assert_eq!(input.scroll_lines(), Vector2f::ZERO);
        assert_eq!(input.scroll_pixels(), Vector2f::ZERO);
    }

    #[test]
    fn the_cursor_keeps_its_latest_position_until_it_leaves() {
        let mut input = Input::new();
        assert_eq!(input.cursor_position(), None);
        frame(
            &mut input,
            &[
                InputEvent::CursorMoved {
                    position: Vector2f::new(1.0, 2.0),
                },
                InputEvent::CursorMoved {
                    position: Vector2f::new(10.0, 20.0),
                },
            ],
        );
        assert_eq!(input.cursor_position(), Some(Vector2f::new(10.0, 20.0)));
        frame(&mut input, &[]);
        assert_eq!(input.cursor_position(), Some(Vector2f::new(10.0, 20.0)));
        frame(&mut input, &[InputEvent::CursorLeft]);
        assert_eq!(input.cursor_position(), None);
    }

    #[test]
    fn gamepads_track_buttons_and_axes_per_player() {
        let mut input = Input::new();
        frame(
            &mut input,
            &[
                InputEvent::GamepadConnected {
                    player: PlayerId::Player2,
                },
                InputEvent::GamepadButton {
                    player: PlayerId::Player2,
                    button: GamepadButton::A,
                    state: ButtonState::Pressed,
                },
                InputEvent::GamepadAxis {
                    player: PlayerId::Player2,
                    axis: GamepadAxis::LeftStickX,
                    value: 0.75,
                },
                InputEvent::GamepadAxis {
                    player: PlayerId::Player2,
                    axis: GamepadAxis::LeftStickY,
                    value: 0.01,
                },
            ],
        );
        assert!(input.gamepad_connected(PlayerId::Player2));
        assert!(!input.gamepad_connected(PlayerId::Player1));
        assert_eq!(
            input.connected_gamepads().collect::<Vec<_>>(),
            [PlayerId::Player2]
        );
        assert!(input.gamepad_button_pressed(PlayerId::Player2, GamepadButton::A));
        assert!(!input.gamepad_button_pressed(PlayerId::Player1, GamepadButton::A));
        assert_eq!(
            input.gamepad_axis(PlayerId::Player2, GamepadAxis::LeftStickX),
            0.75
        );
        // Inside the deadzone.
        assert_eq!(
            input.gamepad_axis(PlayerId::Player2, GamepadAxis::LeftStickY),
            0.0
        );

        frame(&mut input, &[]);
        assert!(!input.gamepad_button_pressed(PlayerId::Player2, GamepadButton::A));
        assert!(input.gamepad_button_held(PlayerId::Player2, GamepadButton::A));

        frame(
            &mut input,
            &[InputEvent::GamepadDisconnected {
                player: PlayerId::Player2,
            }],
        );
        assert!(!input.gamepad_connected(PlayerId::Player2));
        assert!(!input.gamepad_button_held(PlayerId::Player2, GamepadButton::A));
        assert_eq!(
            input.gamepad_axis(PlayerId::Player2, GamepadAxis::LeftStickX),
            0.0
        );
    }

    #[test]
    fn rumble_requests_are_clamped_and_taken_once() {
        let mut input = Input::new();
        input.request_rumble(PlayerId::Player1, 2.0, -1.0, Duration::from_millis(200));
        let requests = input.take_rumble_requests();
        assert_eq!(
            requests,
            [RumbleRequest {
                player: PlayerId::Player1,
                weak: 1.0,
                strong: 0.0,
                duration: Duration::from_millis(200),
            }]
        );
        assert!(input.take_rumble_requests().is_empty());
    }

    #[test]
    fn every_key_code_name_round_trips() {
        for key in KeyCode::ALL {
            assert_eq!(KeyCode::from_code_name(key.code_name()), Some(*key));
        }
        assert_eq!(KeyCode::from_code_name("OSLeft"), Some(KeyCode::SuperLeft));
        assert_eq!(KeyCode::from_code_name("NoSuchKey"), None);
    }

    #[test]
    fn keys_held_lists_keys_in_order() {
        let mut input = Input::new();
        frame(
            &mut input,
            &[
                key(KeyCode::KeyD, ButtonState::Pressed),
                key(KeyCode::KeyA, ButtonState::Pressed),
            ],
        );
        assert_eq!(
            input.keys_held().collect::<Vec<_>>(),
            [KeyCode::KeyA, KeyCode::KeyD]
        );
        assert_eq!(
            input.keys_pressed().collect::<Vec<_>>(),
            [KeyCode::KeyA, KeyCode::KeyD]
        );
    }
}

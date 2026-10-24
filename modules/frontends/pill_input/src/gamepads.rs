//! Gamepads through gilrs: polled into input events, rumbled on request.
//!
//! # Responsibilities
//!
//! - Own the process's [`Gilrs`] context and assign each gamepad a
//!   [`PlayerId`] slot as it connects ([`Gamepads::poll`]).
//! - Translate gilrs buttons and axes into the engine's.
//! - Play the rumble systems request, keeping each effect alive while it runs
//!   ([`Gamepads::play_rumble`]).
//!
//! # Design
//!
//! Gamepads are not window events: gilrs reads them from the operating system
//! on its own, so a frontend polls once per frame, before running it, and pushes
//! what it gets like any other input. Slots go to gamepads in connection order,
//! the lowest free one first; a fifth gamepad is ignored until a slot frees.
//!
//! Dropping a gilrs `Effect` stops it, so playing rumble means keeping the
//! effect until its duration has passed; a gamepad that never reports
//! completion (some XInput pads) is covered by that deadline.

// Standard library
use std::time::Duration;

// External crates
use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Replay, Ticks};
use gilrs::{Axis, Button, EventType, GamepadId, Gilrs};
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target;
use pill_core::{info, warn};
use pill_engine::input::GAMEPAD_SLOT_COUNT;
use pill_engine::{ButtonState, GamepadAxis, GamepadButton, InputEvent, PlayerId, RumbleRequest};

// =============================================================================
// Gamepads
// =============================================================================

/// A rumble effect that is still playing.
struct ActiveRumble {
    /// The gamepad it plays on.
    gamepad: GamepadId,
    /// Kept alive while it plays; dropping it stops it.
    _effect: Effect,
    /// When it started.
    started: Instant,
    /// How long it plays.
    duration: Duration,
}

/// The gamepads, their player slots and the rumble playing on them.
pub struct Gamepads {
    /// The gilrs context, or `None` when the platform has no gamepad support.
    gilrs: Option<Gilrs>,
    /// The gamepad holding each [`PlayerId`] slot.
    slots: [Option<GamepadId>; GAMEPAD_SLOT_COUNT],
    /// Gamepads connected before the first poll, announced by it.
    connected_at_start: Vec<GamepadId>,
    /// Effects still playing.
    active_rumble: Vec<ActiveRumble>,
}

impl Default for Gamepads {
    fn default() -> Self {
        Self::new()
    }
}

impl Gamepads {
    /// Open the platform's gamepad support.
    ///
    /// Never fails: without gamepad support the result reports no gamepads,
    /// and the reason is logged once.
    pub fn new() -> Self {
        let gilrs = match Gilrs::new() {
            Ok(gilrs) => Some(gilrs),
            // A working context that will never see a gamepad.
            Err(gilrs::Error::NotImplemented(gilrs)) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    "gamepads are not supported on this platform"
                );
                Some(gilrs)
            }
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "gamepads unavailable: gilrs failed to start"
                );
                None
            }
        };
        // gilrs reports a gamepad that was already plugged in through
        // `gamepads()`, not as a connection event.
        let connected_at_start = gilrs
            .as_ref()
            .map(|gilrs| gilrs.gamepads().map(|(id, _)| id).collect())
            .unwrap_or_default();
        Self {
            gilrs,
            slots: [None; GAMEPAD_SLOT_COUNT],
            connected_at_start,
            active_rumble: Vec::new(),
        }
    }

    /// Hand every gamepad change since the last poll to `push`.
    ///
    /// Call once per frame, before running it.
    pub fn poll(&mut self, mut push: impl FnMut(InputEvent)) {
        // Step 1: Announce the gamepads that were connected at startup.
        for gamepad in std::mem::take(&mut self.connected_at_start) {
            self.connect(gamepad, &mut push);
        }

        // Step 2: Translate what happened since the last poll.
        while let Some(event) = self.gilrs.as_mut().and_then(Gilrs::next_event) {
            let gamepad = event.id;
            match event.event {
                EventType::Connected => self.connect(gamepad, &mut push),
                EventType::Disconnected => self.disconnect(gamepad, &mut push),
                EventType::ButtonPressed(button, _) => {
                    self.push_button(gamepad, button, ButtonState::Pressed, &mut push);
                }
                EventType::ButtonReleased(button, _) => {
                    self.push_button(gamepad, button, ButtonState::Released, &mut push);
                }
                // The triggers' travel arrives as a button value on most pads.
                EventType::ButtonChanged(Button::LeftTrigger2, value, _) => {
                    self.push_axis(gamepad, GamepadAxis::LeftTrigger, value, &mut push);
                }
                EventType::ButtonChanged(Button::RightTrigger2, value, _) => {
                    self.push_axis(gamepad, GamepadAxis::RightTrigger, value, &mut push);
                }
                EventType::AxisChanged(axis, value, _) => {
                    if let Some(axis) = gamepad_axis(axis) {
                        self.push_axis(gamepad, axis, value, &mut push);
                    }
                }
                EventType::ForceFeedbackEffectCompleted => {
                    self.active_rumble
                        .retain(|rumble| rumble.gamepad != gamepad);
                }
                // Repeats are not presses, other button values are not axes,
                // and a dropped event has nothing to report.
                _ => {}
            }
        }

        // Step 3: Let effects that have run their course go.
        self.active_rumble
            .retain(|rumble| rumble.started.elapsed() < rumble.duration);
    }

    /// Play `requests` on the gamepads that can rumble.
    ///
    /// Call after a frame, with what its systems requested. A request for an
    /// empty slot or a gamepad without force feedback is dropped.
    pub fn play_rumble(&mut self, requests: Vec<RumbleRequest>) {
        let Some(gilrs) = self.gilrs.as_mut() else {
            return;
        };
        for request in requests {
            let Some(gamepad) = self.slots[request.player.index()] else {
                continue;
            };
            if !gilrs.gamepad(gamepad).is_ff_supported() {
                continue;
            }
            match rumble_effect(gilrs, gamepad, &request).and_then(|effect| {
                effect.play()?;
                Ok(effect)
            }) {
                Ok(effect) => self.active_rumble.push(ActiveRumble {
                    gamepad,
                    _effect: effect,
                    started: Instant::now(),
                    duration: request.duration,
                }),
                Err(error) => warn!(
                    target: telemetry_target::ENGINE,
                    player = ?request.player,
                    error = %error,
                    "gamepad rumble failed"
                ),
            }
        }
    }

    /// Give `gamepad` the lowest free slot and announce it.
    fn connect(&mut self, gamepad: GamepadId, push: &mut impl FnMut(InputEvent)) {
        if self.player_of(gamepad).is_some() {
            return;
        }
        let Some(index) = self.slots.iter().position(Option::is_none) else {
            warn!(
                target: telemetry_target::ENGINE,
                slots = GAMEPAD_SLOT_COUNT,
                "gamepad connected, but every player slot is taken; ignoring it"
            );
            return;
        };
        self.slots[index] = Some(gamepad);
        let player = PlayerId::ALL[index];
        if let Some(gilrs) = &self.gilrs {
            info!(
                target: telemetry_target::ENGINE,
                player = ?player,
                name = gilrs.gamepad(gamepad).name(),
                "gamepad connected"
            );
        }
        push(InputEvent::GamepadConnected { player });
    }

    /// Free `gamepad`'s slot and announce it.
    fn disconnect(&mut self, gamepad: GamepadId, push: &mut impl FnMut(InputEvent)) {
        let Some(player) = self.player_of(gamepad) else {
            return;
        };
        self.slots[player.index()] = None;
        self.active_rumble
            .retain(|rumble| rumble.gamepad != gamepad);
        info!(target: telemetry_target::ENGINE, player = ?player, "gamepad disconnected");
        push(InputEvent::GamepadDisconnected { player });
    }

    /// Push a button change for a gamepad that holds a slot.
    fn push_button(
        &self,
        gamepad: GamepadId,
        button: Button,
        state: ButtonState,
        push: &mut impl FnMut(InputEvent),
    ) {
        if let (Some(player), Some(button)) = (self.player_of(gamepad), gamepad_button(button)) {
            push(InputEvent::GamepadButton {
                player,
                button,
                state,
            });
        }
    }

    /// Push an axis change for a gamepad that holds a slot.
    fn push_axis(
        &self,
        gamepad: GamepadId,
        axis: GamepadAxis,
        value: f32,
        push: &mut impl FnMut(InputEvent),
    ) {
        if let Some(player) = self.player_of(gamepad) {
            push(InputEvent::GamepadAxis {
                player,
                axis,
                value,
            });
        }
    }

    /// The slot `gamepad` holds, if any.
    fn player_of(&self, gamepad: GamepadId) -> Option<PlayerId> {
        self.slots
            .iter()
            .position(|slot| *slot == Some(gamepad))
            .and_then(PlayerId::from_index)
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Build a two-motor rumble for `request` on `gamepad`.
///
/// # Errors
///
/// Returns gilrs's error when the gamepad refuses the effect.
fn rumble_effect(
    gilrs: &mut Gilrs,
    gamepad: GamepadId,
    request: &RumbleRequest,
) -> Result<Effect, gilrs::ff::Error> {
    let duration_milliseconds = u32::try_from(request.duration.as_millis()).unwrap_or(u32::MAX);
    let scheduling = Replay {
        play_for: Ticks::from_ms(duration_milliseconds),
        ..Default::default()
    };
    // Motor strengths are 16-bit magnitudes.
    let magnitude = |strength: f32| (strength.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
    EffectBuilder::new()
        .add_effect(BaseEffect {
            kind: BaseEffectType::Weak {
                magnitude: magnitude(request.weak),
            },
            scheduling,
            ..Default::default()
        })
        .add_effect(BaseEffect {
            kind: BaseEffectType::Strong {
                magnitude: magnitude(request.strong),
            },
            scheduling,
            ..Default::default()
        })
        .gamepads(&[gamepad])
        .finish(gilrs)
}

/// The engine's button for gilrs's, or `None` for one the engine does not
/// track (the extra `C`/`Z` buttons, unknown ones).
///
/// gilrs names face buttons by position; the engine names them after the Xbox
/// layout, where X is on the left and Y on top.
fn gamepad_button(button: Button) -> Option<GamepadButton> {
    match button {
        Button::South => Some(GamepadButton::A),
        Button::East => Some(GamepadButton::B),
        Button::West => Some(GamepadButton::X),
        Button::North => Some(GamepadButton::Y),
        Button::LeftTrigger => Some(GamepadButton::LeftBumper),
        Button::RightTrigger => Some(GamepadButton::RightBumper),
        Button::LeftTrigger2 => Some(GamepadButton::LeftTrigger),
        Button::RightTrigger2 => Some(GamepadButton::RightTrigger),
        Button::Select => Some(GamepadButton::Back),
        Button::Start => Some(GamepadButton::Start),
        Button::Mode => Some(GamepadButton::Mode),
        Button::LeftThumb => Some(GamepadButton::LeftStick),
        Button::RightThumb => Some(GamepadButton::RightStick),
        Button::DPadUp => Some(GamepadButton::DPadUp),
        Button::DPadDown => Some(GamepadButton::DPadDown),
        Button::DPadLeft => Some(GamepadButton::DPadLeft),
        Button::DPadRight => Some(GamepadButton::DPadRight),
        _ => None,
    }
}

/// The engine's axis for gilrs's, or `None` for one the engine does not track.
fn gamepad_axis(axis: Axis) -> Option<GamepadAxis> {
    match axis {
        Axis::LeftStickX => Some(GamepadAxis::LeftStickX),
        Axis::LeftStickY => Some(GamepadAxis::LeftStickY),
        Axis::RightStickX => Some(GamepadAxis::RightStickX),
        Axis::RightStickY => Some(GamepadAxis::RightStickY),
        Axis::LeftZ => Some(GamepadAxis::LeftTrigger),
        Axis::RightZ => Some(GamepadAxis::RightTrigger),
        Axis::DPadX => Some(GamepadAxis::DPadX),
        Axis::DPadY => Some(GamepadAxis::DPadY),
        _ => None,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn face_buttons_follow_the_xbox_layout() {
        assert_eq!(gamepad_button(Button::South), Some(GamepadButton::A));
        assert_eq!(gamepad_button(Button::West), Some(GamepadButton::X));
        assert_eq!(gamepad_button(Button::North), Some(GamepadButton::Y));
        assert_eq!(gamepad_button(Button::C), None);
    }

    #[test]
    fn trigger_axes_map_to_trigger_travel() {
        assert_eq!(gamepad_axis(Axis::LeftZ), Some(GamepadAxis::LeftTrigger));
        assert_eq!(gamepad_axis(Axis::Unknown), None);
    }
}

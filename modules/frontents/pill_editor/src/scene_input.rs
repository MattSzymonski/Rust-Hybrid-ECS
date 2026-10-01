//! The Scene panel's input: DOM events over the viewport, as engine input.
//!
//! # Responsibilities
//!
//! - Render the element that covers the Scene viewport and receives its
//!   keyboard, pointer and wheel events ([`SceneViewport`]), docked or
//!   detached.
//! - Translate those DOM events into engine
//!   [`InputEvent`](pill_engine::InputEvent)s and queue them on the editor's
//!   host.
//!
//! # Design
//!
//! The engine draws behind a transparent region of the WebView, so the input
//! over the scene goes to the DOM, not to the native window: the WebView
//! takes the keyboard and mouse before tao could see them. The viewport
//! element is focusable and receives keys only while focused, which a click
//! into the scene gives it, so typing into the Inspector never moves the game.
//!
//! Positions are the element's own coordinates, scaled from CSS pixels to
//! physical ones - the frame of reference the engine uses everywhere, since
//! the element and the render viewport cover the same rectangle. The DOM
//! reports no raw motion, so mouse motion is the difference between pointer
//! positions and stops at the element's edge. A button released outside the
//! element would never report its release, so leaving the element releases
//! every button.

// Standard library
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

// External crates
use dioxus::desktop::window;
use dioxus::html::geometry::{ElementPoint, WheelDelta};
use dioxus::html::input_data::MouseButton as DomMouseButton;
use dioxus::prelude::*;
use pill_core::math::Vector2f;
use pill_engine::{ButtonState, InputEvent, KeyCode, MouseButton, ScrollDelta};

// Current crate
use crate::{EditorContext, Stats};

// =============================================================================
// SceneViewport
// =============================================================================

/// The Scene panel: the FPS overlay, over an element that forwards input.
#[component]
pub(crate) fn SceneViewport(stats: Signal<Stats>, editor: Arc<EditorContext>) -> Element {
    // Where the pointer last was over this element and which buttons it holds.
    let tracker = use_hook(|| Rc::new(PointerTracker::default()));
    // The window this element is in - the main one, or a detached Scene
    // window - for its scale factor.
    let desktop = window();
    let stats = stats.read();

    let key_down_editor = Arc::clone(&editor);
    let key_up_editor = Arc::clone(&editor);
    let blur_editor = Arc::clone(&editor);
    let (down_editor, down_tracker, down_desktop) =
        (Arc::clone(&editor), Rc::clone(&tracker), desktop.clone());
    let (up_editor, up_tracker, up_desktop) =
        (Arc::clone(&editor), Rc::clone(&tracker), desktop.clone());
    let (move_editor, move_tracker, move_desktop) =
        (Arc::clone(&editor), Rc::clone(&tracker), desktop.clone());
    let (leave_editor, leave_tracker) = (Arc::clone(&editor), Rc::clone(&tracker));
    let (wheel_editor, wheel_desktop) = (Arc::clone(&editor), desktop);

    rsx! {
        div {
            class: "dock-scene-input",
            tabindex: "0",
            onkeydown: move |event| {
                // Keep the WebView from acting on game keys (Space scrolling,
                // Tab moving focus, F5 reloading the editor's page).
                event.prevent_default();
                if event.is_auto_repeating() {
                    return;
                }
                if let Some(key) = KeyCode::from_code_name(&event.code().to_string()) {
                    key_down_editor.push_input(InputEvent::Key { key, state: ButtonState::Pressed });
                }
            },
            onkeyup: move |event| {
                event.prevent_default();
                if let Some(key) = KeyCode::from_code_name(&event.code().to_string()) {
                    key_up_editor.push_input(InputEvent::Key { key, state: ButtonState::Released });
                }
            },
            onblur: move |_| blur_editor.push_input(InputEvent::FocusLost),
            oncontextmenu: move |event| {
                // Right-click belongs to the game here, not to the editor menu.
                event.prevent_default();
                event.stop_propagation();
            },
            onpointerdown: move |event| {
                let scale = down_desktop.window.scale_factor();
                down_tracker.move_to(&down_editor, physical_position(event.element_coordinates(), scale));
                if let Some(button) = event.trigger_button().and_then(mouse_button) {
                    down_tracker.press(&down_editor, button);
                }
            },
            onpointerup: move |event| {
                let scale = up_desktop.window.scale_factor();
                up_tracker.move_to(&up_editor, physical_position(event.element_coordinates(), scale));
                if let Some(button) = event.trigger_button().and_then(mouse_button) {
                    up_tracker.release(&up_editor, button);
                }
            },
            onpointermove: move |event| {
                let scale = move_desktop.window.scale_factor();
                move_tracker.move_to(&move_editor, physical_position(event.element_coordinates(), scale));
            },
            onpointerleave: move |_| leave_tracker.leave(&leave_editor),
            onwheel: move |event| {
                event.prevent_default();
                let scale = wheel_desktop.window.scale_factor();
                wheel_editor.push_input(InputEvent::MouseWheel { delta: scroll_delta(event.delta(), scale) });
            },
            div {
                class: "dock-viewport-fps",
                "{stats.fps:.0} FPS"
            }
        }
    }
}

// =============================================================================
// PointerTracker
// =============================================================================

/// What one viewport element knows about the pointer over it.
#[derive(Default)]
struct PointerTracker {
    /// The last position, or `None` while the pointer is outside.
    position: Cell<Option<Vector2f>>,
    /// Engine mouse buttons pressed over the element and not yet released,
    /// indexed by [`MouseButton`].
    held: Cell<[bool; MouseButton::ALL.len()]>,
}

impl PointerTracker {
    /// Report the pointer at `position`, and its motion since the last one.
    fn move_to(&self, editor: &EditorContext, position: Vector2f) {
        if let Some(previous) = self.position.replace(Some(position)) {
            let delta = position - previous;
            if delta != Vector2f::ZERO {
                editor.push_input(InputEvent::MouseMotion { delta });
            }
        }
        editor.push_input(InputEvent::CursorMoved { position });
    }

    /// Report `button` pressed.
    fn press(&self, editor: &EditorContext, button: MouseButton) {
        self.set_held(button, true);
        editor.push_input(InputEvent::MouseButton {
            button,
            state: ButtonState::Pressed,
        });
    }

    /// Report `button` released.
    fn release(&self, editor: &EditorContext, button: MouseButton) {
        self.set_held(button, false);
        editor.push_input(InputEvent::MouseButton {
            button,
            state: ButtonState::Released,
        });
    }

    /// Report the pointer gone, releasing every button it still holds.
    fn leave(&self, editor: &EditorContext) {
        for button in MouseButton::ALL {
            if self.held.get()[button as usize] {
                self.release(editor, button);
            }
        }
        self.position.set(None);
        editor.push_input(InputEvent::CursorLeft);
    }

    /// Record whether `button` is held.
    fn set_held(&self, button: MouseButton, held: bool) {
        let mut buttons = self.held.get();
        buttons[button as usize] = held;
        self.held.set(buttons);
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// The engine mouse button for a DOM one, or `None` for an unknown button.
fn mouse_button(button: DomMouseButton) -> Option<MouseButton> {
    match button {
        DomMouseButton::Primary => Some(MouseButton::Left),
        DomMouseButton::Secondary => Some(MouseButton::Right),
        DomMouseButton::Auxiliary => Some(MouseButton::Middle),
        DomMouseButton::Fourth => Some(MouseButton::Back),
        DomMouseButton::Fifth => Some(MouseButton::Forward),
        DomMouseButton::Unknown => None,
    }
}

/// An element-relative position in CSS pixels, in physical pixels.
fn physical_position(point: ElementPoint, scale_factor: f64) -> Vector2f {
    Vector2f::new(
        (point.x * scale_factor) as f32,
        (point.y * scale_factor) as f32,
    )
}

/// A DOM wheel delta as the engine's.
///
/// The DOM counts positive `y` as scrolling down; the engine, like winit,
/// counts it as up, so both axes flip. Pixels are scaled to physical ones, and
/// a page step is reported as that many lines.
fn scroll_delta(delta: WheelDelta, scale_factor: f64) -> ScrollDelta {
    match delta {
        WheelDelta::Pixels(pixels) => ScrollDelta::Pixels(Vector2f::new(
            (-pixels.x * scale_factor) as f32,
            (-pixels.y * scale_factor) as f32,
        )),
        WheelDelta::Lines(lines) => {
            ScrollDelta::Lines(Vector2f::new(-lines.x as f32, -lines.y as f32))
        }
        WheelDelta::Pages(pages) => {
            ScrollDelta::Lines(Vector2f::new(-pages.x as f32, -pages.y as f32))
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dom_buttons_map_by_role_not_index() {
        // The DOM numbers the middle button 1 and the right one 2.
        assert_eq!(
            mouse_button(DomMouseButton::Auxiliary),
            Some(MouseButton::Middle)
        );
        assert_eq!(
            mouse_button(DomMouseButton::Secondary),
            Some(MouseButton::Right)
        );
        assert_eq!(mouse_button(DomMouseButton::Unknown), None);
    }

    #[test]
    fn wheel_deltas_flip_to_the_engine_direction() {
        assert_eq!(
            scroll_delta(WheelDelta::lines(0.0, 3.0, 0.0), 2.0),
            ScrollDelta::Lines(Vector2f::new(0.0, -3.0))
        );
        assert_eq!(
            scroll_delta(WheelDelta::pixels(4.0, 10.0, 0.0), 2.0),
            ScrollDelta::Pixels(Vector2f::new(-8.0, -20.0))
        );
    }

    #[test]
    fn positions_scale_to_physical_pixels() {
        assert_eq!(
            physical_position(ElementPoint::new(10.0, 5.5), 1.5),
            Vector2f::new(15.0, 8.25)
        );
    }
}

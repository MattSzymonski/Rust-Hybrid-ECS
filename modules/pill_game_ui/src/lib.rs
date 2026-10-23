//! The game UI: an egui context a project draws through the main renderer.
//!
//! # Responsibilities
//!
//! - [`GameUi`]: the resource holding the egui context, the published pointer
//!   and keyboard claims, and the texture mirror.
//! - [`GameUi::run`]: one pass - the engine's input in, the frame's draw list
//!   and textures out, published into the world's
//!   [`UiFrameSlot`](pill_draw::UiFrameSlot).
//! - [`install`]: puts the resource into a project's world, registering its
//!   type first so a reload finds the same one.
//! - [`convert`], [`input`], [`textures`]: the three pieces it composes.
//!
//! # Design
//!
//! This crate is a shared `dylib` (plan decision D2) so the egui context
//! outlives a project reload: a rebuilt project finds its fonts, its widget
//! state and its textures still there. The same reload contract as
//! `pill_editor_core_egui` binds - nothing module-defined may be stored in the context
//! - and [`GameUi`] itself lives here, so the type is one across artifacts.
//!
//! The frame it produces is neutral (`pill_draw`) and travels through the
//! draw crate's frame slot (`pill_draw::UiFrameSlot`): the renderer and `pill_renderer_api` never see
//! egui (D1), and neither side names the other's crate - which is also what
//! keeps a renderer from having to link this crate, since one link unit cannot
//! hold two copies of a third-party graph in different formats. A game's own
//! systems read [`GameUi::wants_pointer`] / [`GameUi::wants_keyboard`] to gate
//! their input (D4). The context is exposed
//! for the one thing a facade cannot wrap: a project's UI closure needs
//! `&mut egui::Ui`.
//!
//! # Usage
//!
//! ```ignore
//! // Once, in the project's init:
//! pill_game_ui::install(engine);
//!
//! // In a system, once per frame:
//! let input = pill_game_ui::InputFrame::capture(world.get_resource::<Input>().unwrap());
//! let mut ui = world.get_resource_mut::<GameUi>().unwrap();
//! let mut slot = world.get_resource_mut::<UiFrameSlot>().unwrap();
//! ui.run(&mut slot, &input, viewport, scale_factor, pill_game_ui::PRIMARY_SURFACE, |ui| {
//!     ui.heading("Tuning");
//!     ui.label(format!("fps {fps}"));
//! });
//! ```

/// The tessellated output to `pill_draw`.
pub mod convert;
/// The engine's input snapshot and the bridge to egui's input.
pub mod input;
/// The texture mirror a rebuilt painter can upload from.
pub mod textures;

/// The egui this crate pins, re-exported so a project names it through the
/// engine rather than depending on a second copy of its own.
pub use egui;
/// The slot a pass publishes into, re-exported so a project's system names it
/// without depending on the draw-data crate itself.
pub use pill_draw::UiFrameSlot;

// External crates
use egui::Context;
use pill_core::math::Vector2f;
use pill_core::platform::Instant;
use pill_draw::{DrawList, UiDraw, UiFrame};
use pill_engine_core::{Engine, Resource};

// Current crate
pub use convert::{convert, external_id, straight_color};
pub use input::{raw_input, InputFrame, InputMemory};
use textures::TextureMirror;

/// The surface id of the window a game draws into.
///
/// A pass names the surface whose frame it belongs in, so a game hands this to
/// [`GameUi::run`] and the renderer draws it with everything else bound to the
/// primary window.
pub const PRIMARY_SURFACE: u32 = 0;

/// The game's UI: one egui context, and what the last pass drew.
pub struct GameUi {
    /// The context every project system draws into.
    context: Context,
    /// What the previous frame told the input bridge.
    input_memory: InputMemory,
    /// Every texture egui handed out, kept whole.
    textures: TextureMirror,
    /// When the UI's first pass ran, for egui's animation clock.
    started: Instant,
    /// How many draws the last pass produced, for a caller's own logging.
    draws: usize,
    /// Whether the last pass wanted the pointer.
    wants_pointer: bool,
    /// Whether the last pass wanted the keyboard.
    wants_keyboard: bool,
}

impl Resource for GameUi {}

impl Default for GameUi {
    fn default() -> Self {
        GameUi::new()
    }
}

impl GameUi {
    /// A UI with a fresh context, egui's default fonts and no textures.
    pub fn new() -> GameUi {
        GameUi {
            context: Context::default(),
            input_memory: InputMemory::new(),
            textures: TextureMirror::new(),
            started: Instant::now(),
            draws: 0,
            wants_pointer: false,
            wants_keyboard: false,
        }
    }

    /// The egui context a project's UI closure runs in.
    ///
    /// Exposed for the callers a facade cannot wrap - a project that installs
    /// its own style, or a test that inspects egui's own state.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Run one pass: `build` draws the UI, and the frame it produced is
    /// published into `slot` for the renderer to drain.
    ///
    /// `input` is this frame's engine snapshot (see [`InputFrame::capture`]),
    /// `viewport` the UI's logical size, `scale_factor` the monitor scale and
    /// `surface` the surface id the renderer draws the frame into. A frame the
    /// previous pass published and nobody drained is replaced: the newest pass
    /// is what the renderer wants.
    pub fn run(
        &mut self,
        slot: &mut UiFrameSlot,
        input: &InputFrame,
        viewport: Vector2f,
        scale_factor: f32,
        surface: u32,
        build: impl FnMut(&mut egui::Ui),
    ) {
        // Step 1: the pass itself, on the engine's input.
        let time = self.started.elapsed().as_secs_f64();
        let raw_input = raw_input(input, viewport, scale_factor, time, &mut self.input_memory);
        let output = self.context.run_ui(raw_input, build);

        // Step 2: the textures egui produced, folded into the mirror. epaint
        // panics on a delta dropped with unapplied content, so the mirror is
        // the one consumer and the delta is cleared once folded.
        let mut delta = output.textures_delta;
        self.textures.apply(&delta);
        delta.clear();

        // Step 3: the tessellated output, as the neutral frame the renderer
        // drains.
        let primitives = self
            .context
            .tessellate(output.shapes, output.pixels_per_point);
        let mut list = DrawList::new();
        self.draws = convert(&primitives, &mut list);
        let mut frame = UiFrame::default();
        self.textures.carry(&mut frame.textures);
        if !list.is_empty() {
            frame.draws.push(UiDraw {
                surface,
                scale_factor,
                list,
            });
        }

        // Step 4: what the UI claimed this pass, for the game's own gating.
        self.wants_pointer = self.context.egui_wants_pointer_input();
        self.wants_keyboard = self.context.egui_wants_keyboard_input();
        slot.publish(frame);
    }

    /// How many draws the last pass produced.
    pub fn draws(&self) -> usize {
        self.draws
    }

    /// Whether the last pass wants the pointer: a game gates its own pointer
    /// input on this (D4).
    pub fn wants_pointer(&self) -> bool {
        self.wants_pointer
    }

    /// Whether the last pass wants the keyboard: a game gates its own key
    /// input on this (D4).
    pub fn wants_keyboard(&self) -> bool {
        self.wants_keyboard
    }

    /// How many textures the UI currently mirrors.
    pub fn texture_count(&self) -> usize {
        self.textures.len()
    }
}

/// Put a [`GameUi`] into `engine`'s world, with the frame slot a renderer
/// drains, registering both types first.
///
/// A project's `init` calls this and nothing else. An existing UI is kept, so a
/// project reload does not reset the context, its fonts or the state the player
/// typed into it; the slot is what tells a renderer that this world's UI is the
/// game's.
pub fn install(engine: &mut Engine) {
    let world = engine.world_mut();
    world.register_resource::<GameUi>();
    world.register_resource::<UiFrameSlot>();
    if world.get_resource::<GameUi>().is_none() {
        world.insert_resource(GameUi::new());
    }
    if world.get_resource::<UiFrameSlot>().is_none() {
        world.insert_resource(UiFrameSlot::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine_core::input::{ButtonState, Input, InputEvent, KeyCode};

    /// An engine with a `GameUi` installed that ran one frame with `events`
    /// queued.
    fn engine_with_ui(events: &[InputEvent]) -> Engine {
        let mut engine = Engine::new();
        install(&mut engine);
        for event in events {
            engine.push_input_event(event.clone());
        }
        engine.process_frame().expect("the frame runs");
        engine
    }

    /// The input frame a system would capture this frame.
    fn capture(engine: &Engine) -> InputFrame {
        InputFrame::capture(
            engine
                .world()
                .get_resource::<Input>()
                .expect("the engine installs its input"),
        )
    }

    /// Run one pass into a fresh slot and return what it published.
    fn run_once(engine: &mut Engine) -> UiFrame {
        let input = capture(engine);
        let mut slot = UiFrameSlot::new();
        let world = engine.world_mut();
        let ui = world.get_resource_mut::<GameUi>().expect("installed above");
        ui.run(
            &mut slot,
            &input,
            Vector2f::new(320.0, 200.0),
            1.0,
            PRIMARY_SURFACE,
            |ui| {
                ui.heading("Pill");
                ui.label("game ui");
            },
        );
        slot.take()
    }

    #[test]
    fn a_headless_pass_produces_a_draw_list_and_carries_the_atlas() {
        let mut engine = engine_with_ui(&[]);
        let frame = run_once(&mut engine);
        assert!(!frame.draws.is_empty(), "the pass drew no draw list");
        assert!(
            !frame.draws[0].list.vertices.is_empty(),
            "the draw list carries no geometry"
        );
        assert_eq!(frame.draws[0].surface, 0);
        // egui's first managed texture is its font atlas; it arrives whole.
        assert!(
            frame
                .textures
                .textures
                .iter()
                .any(|texture| texture.id == pill_draw::TextureId(1 << 63)),
            "the font atlas was not carried"
        );
        let ui = engine
            .world()
            .get_resource::<GameUi>()
            .expect("still installed");
        assert!(ui.draws() > 0);
    }

    #[test]
    fn a_second_pass_carries_no_textures_that_did_not_change() {
        let mut engine = engine_with_ui(&[]);
        let first = run_once(&mut engine);
        assert!(!first.textures.textures.is_empty());
        let second = run_once(&mut engine);
        assert!(
            second.textures.textures.is_empty(),
            "unchanged textures were carried again"
        );
        assert!(!second.is_empty(), "the geometry is still drawn");
    }

    #[test]
    fn a_pass_publishes_its_frame_once() {
        let mut engine = engine_with_ui(&[]);
        let input = capture(&engine);
        let mut slot = UiFrameSlot::new();
        assert!(slot.is_empty());
        let world = engine.world_mut();
        let ui = world.get_resource_mut::<GameUi>().expect("installed above");
        ui.run(
            &mut slot,
            &input,
            Vector2f::new(320.0, 200.0),
            1.0,
            PRIMARY_SURFACE,
            |ui| {
                ui.label("published");
            },
        );
        assert!(!slot.is_empty(), "the pass published nothing");
        assert!(!slot.take().is_empty());
        assert!(slot.is_empty(), "the take drained the slot");
    }

    #[test]
    fn installing_again_keeps_the_context_and_its_textures() {
        let mut engine = engine_with_ui(&[]);
        run_once(&mut engine);
        let textures = engine
            .world()
            .get_resource::<GameUi>()
            .expect("installed")
            .texture_count();
        assert!(textures > 0);
        install(&mut engine);
        let after = engine
            .world()
            .get_resource::<GameUi>()
            .expect("installed")
            .texture_count();
        assert_eq!(after, textures, "a reload reset the UI");
    }

    #[test]
    fn a_key_press_reaches_the_ui() {
        let mut engine = engine_with_ui(&[InputEvent::Key {
            key: KeyCode::Tab,
            state: ButtonState::Pressed,
        }]);
        let input = capture(&engine);
        let mut seen_tab = false;
        let mut slot = UiFrameSlot::new();
        let world = engine.world_mut();
        let ui = world.get_resource_mut::<GameUi>().expect("installed above");
        ui.run(
            &mut slot,
            &input,
            Vector2f::new(320.0, 200.0),
            1.0,
            PRIMARY_SURFACE,
            |ui| {
                seen_tab = ui.input(|state| state.key_pressed(egui::Key::Tab));
            },
        );
        assert!(seen_tab, "the bridge did not deliver the key press");
        // A label claims neither the pointer nor the keyboard.
        assert!(!ui.wants_pointer());
        assert!(!ui.wants_keyboard());
    }

    #[test]
    fn wants_pointer_follows_a_window_under_the_cursor() {
        let mut ui = GameUi::new();
        let mut slot = UiFrameSlot::new();
        let viewport = Vector2f::new(320.0, 200.0);
        // The window sits at (10, 10) and fills 120 x 80 points, so (40, 40)
        // is inside it and (300, 190) is not.
        let mut pass = |cursor: Option<Vector2f>| {
            let input = InputFrame {
                cursor,
                ..Default::default()
            };
            ui.run(&mut slot, &input, viewport, 1.0, PRIMARY_SURFACE, |ui| {
                egui::Window::new("tuning")
                    .fixed_pos(egui::pos2(10.0, 10.0))
                    .fixed_size(egui::vec2(120.0, 80.0))
                    .show(ui.ctx(), |ui| {
                        ui.label("volume");
                    });
            });
            ui.wants_pointer()
        };
        // The first pass lays the window out; egui hit-tests from the areas it
        // recorded, so the claim is read on the passes after it.
        pass(None);
        assert!(
            pass(Some(Vector2f::new(40.0, 40.0))),
            "the window is under the cursor"
        );
        assert!(
            !pass(Some(Vector2f::new(300.0, 190.0))),
            "the cursor left the window"
        );
    }
}

//! The game's UI context: one world resource a project's systems build
//! immediate-mode UI through.
//!
//! # Responsibilities
//!
//! - [`UiContext`]: the resource owning the persistent widget state (values,
//!   focus, scroll), the text system and glyph atlas, the design tokens and
//!   the project's stylesheet.
//! - [`UiContext::begin_frame`] routes the engine's [`Input`] into the UI and
//!   starts the frame; [`UiContext::draw`] builds a panel and appends its
//!   draw list for a surface; [`UiContext::inject`] appends a list built
//!   outside the widget state (the editor's overlay); [`UiContext::take_frame`]
//!   hands everything built, plus the atlas changes, to the renderer's frame
//!   builder.
//! - [`UiContext::sync_stylesheet`]: load a `.css` source through the engine's
//!   asset store and re-parse it whenever the store reports it changed, so a
//!   project's UI restyles live when the stylesheet is edited.
//!
//! # Design
//!
//! Everything that must survive a project reload lives in this resource, and
//! the resource lives in the world: values, scroll offsets, the atlas and the
//! shaped-glyph caches are plain data or shared-`dylib` state, never statics
//! in the project's image. That is also why this type sits in `pill_ui` and
//! not the engine core: `pill_ui` already depends on the engine (it consumes
//! [`InputEvent`]), a dependency the engine core cannot take in reverse, and
//! as a shared library it gives the project that fills the resource and the
//! host that outlives the reload one type identity.
//!
//! Inserting it is a per-generation contract: a reload releases exactly the
//! resources the arriving generation did not re-register, so a project's
//! `init` calls `World::register_resource::<UiContext>()` on every
//! generation and inserts the value only when no earlier generation left one
//! behind. A context that skips the re-registration is dropped out from
//! under the systems using it at the next reload.
//!
//! The engine folds its events into per-frame state ([`Input`] keeps no
//! event list), so the routing rebuilds the UI-facing event stream from that
//! state every frame: press and release edges, the cursor, text and IME,
//! modifiers, and the wheel.

// Standard library
use std::path::Path;

// External crates
use pill_core::math::Vector2f;
use pill_engine_core::{
    AssetImport, AssetManager, ButtonState, Input, InputEvent, MetadataPolicy, MouseButton,
    Resource, ScrollDelta,
};

// Current crate
use crate::asset::StylesheetAsset;
use crate::atlas::{Atlas, AtlasUpdate};
use crate::css::Stylesheet;
use crate::draw::DrawList;
use crate::frame::{UiDraw, UiFrame};
use crate::layout::run_frame;
use crate::paint::paint;
use crate::panel::PanelBuilder;
use crate::state::UiState;
use crate::text::TextSystem;
use crate::theme::Theme;

/// The game's immediate-mode UI: state, input routing, and this frame's
/// output.
///
/// Insert it once as a world resource (a project's `init` does) and every
/// system that draws UI takes `ResMut<UiContext>`. The resource is the reload
/// boundary for the UI: a project DLL swap leaves it - and the values, focus
/// and scroll it holds - untouched.
pub struct UiContext {
    /// The persistent widget state: values, focus, scroll, text editing.
    pub state: UiState,
    /// Fonts, shaping caches and glyph rasterization.
    pub text: TextSystem,
    /// The glyph and icon image the draw lists sample.
    pub atlas: Atlas,
    /// The built-in design: what a UI without a stylesheet looks like.
    pub theme: Theme,
    /// The project's stylesheet; an empty one means the built-in look.
    pub stylesheet: Stylesheet,
    /// The frame being built; drained by [`Self::take_frame`].
    frame: UiFrame,
    /// The area this frame lays out against, in logical pixels.
    viewport: Vector2f,
    /// The monitor scale logical pixels are drawn at.
    scale_factor: f32,
    /// The stylesheet text the current parse came from, to skip re-parsing
    /// equal text.
    stylesheet_source: String,
    /// The store revision [`Self::sync_stylesheet`] last looked at.
    stylesheet_revision: Option<u64>,
    /// The stylesheet asset's content version last parsed.
    stylesheet_version: Option<u64>,
    /// Whether the last import attempt failed and was reported, so a missing
    /// file logs once instead of every frame.
    stylesheet_import_reported: bool,
    /// The atlas revision the last frame carried; the texture set uploads by
    /// revision (W-0028 D3).
    atlas_revision: u64,
    /// Whether the atlas was ever carried, so the first frame publishes the
    /// whole image even when no texel changed yet.
    atlas_published: bool,
}

impl Default for UiContext {
    fn default() -> UiContext {
        UiContext {
            state: UiState::new(),
            text: TextSystem::new(),
            atlas: Atlas::default(),
            theme: Theme::default(),
            stylesheet: Stylesheet::empty(),
            frame: UiFrame::default(),
            viewport: Vector2f::new(640.0, 480.0),
            scale_factor: 1.0,
            stylesheet_source: String::new(),
            stylesheet_revision: None,
            stylesheet_version: None,
            stylesheet_import_reported: false,
            atlas_revision: 0,
            atlas_published: false,
        }
    }
}

impl Resource for UiContext {}

impl UiContext {
    /// A context with the bundled font, the built-in theme and an empty
    /// stylesheet.
    pub fn new() -> UiContext {
        UiContext::default()
    }

    /// Route this frame's input into the UI and start the frame.
    ///
    /// `viewport` is the logical size panels lay out against - the surface's
    /// size for a full-window UI, or the overlay's own size for a fixed
    /// corner overlay; `scale_factor` converts the engine's physical-pixel
    /// cursor and scroll values.
    pub fn begin_frame(&mut self, input: &Input, viewport: Vector2f, scale_factor: f32) {
        // The previous frame closes: a release it saw has already been
        // offered to every widget's build, so nothing may stay active.
        self.state.end_frame();
        self.viewport = viewport;
        self.scale_factor = if scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        route_input(&mut self.state, input, self.scale_factor);
        self.state.input.begin_frame();
        self.frame.clear();
    }

    /// Build one panel over the frame's viewport and append its draw list for
    /// `surface` (the primary surface is `0`).
    pub fn draw(&mut self, surface: u32, panel: impl FnOnce(&mut PanelBuilder<'_>)) {
        let (output, layout) = run_frame(
            &mut self.state,
            &mut self.text,
            &self.theme,
            &mut self.stylesheet,
            self.viewport,
            panel,
        );
        let mut list = DrawList::new();
        paint(
            &output.tree,
            &layout,
            &self.theme,
            &self.state,
            &mut self.text,
            &mut self.atlas,
            &mut list,
        );
        self.frame.draws.push(UiDraw {
            surface,
            scale_factor: self.scale_factor,
            list,
        });
    }

    /// Append an already-built draw list for `surface` to this frame (Stage
    /// 21, D24).
    ///
    /// The executable uses this to fold the editor's overlay list into the
    /// renderer's UI pass when no composition overlay could be created: the
    /// list is built by the shell, outside the project's widget state, so it
    /// cannot go through [`Self::draw`]. `scale_factor` is the monitor scale
    /// the list's logical pixels are in, exactly as a built panel records.
    pub fn inject(&mut self, surface: u32, scale_factor: f32, list: DrawList) {
        self.frame.draws.push(UiDraw {
            surface,
            scale_factor: if scale_factor > 0.0 {
                scale_factor
            } else {
                1.0
            },
            list,
        });
    }

    /// Everything built since the frame began, plus the textures it samples;
    /// drains both.
    ///
    /// The renderer's frame builder calls this once per update; without it
    /// the atlas keeps accumulating its dirty list. A nearly full atlas is
    /// grown a page here, between frames, so the size change never lands
    /// under a quad that was already pushed against the old UV scale.
    ///
    /// The atlas is carried as the whole image at a new revision whenever a
    /// texel changed (or on the first take), so a backend that was rebuilt
    /// mid-run can rebuild its texture from one frame instead of waiting for
    /// a full image that a patch-only stream would never send (W-0028 D3).
    pub fn take_frame(&mut self) -> UiFrame {
        self.atlas.ensure_headroom();
        let mut frame = std::mem::take(&mut self.frame);
        let changed = self.atlas.take_updated_regions();
        if !changed.is_empty() || !self.atlas_published {
            self.atlas_revision += 1;
            self.atlas_published = true;
            let width = self.atlas.width();
            let height = self.atlas.height();
            let pixels: std::sync::Arc<[u8]> = std::sync::Arc::from(self.atlas.pixels());
            frame.textures.push_full(
                pill_draw::TextureId::Atlas,
                width,
                height,
                self.atlas_revision,
                pill_draw::UiTextureFilter::Nearest,
                pixels,
            );
        }
        frame
    }

    /// Fold another atlas's additions into this context's (Stage 21, D24).
    ///
    /// The editor's overlay list is built against the shell's atlas while the
    /// renderer's UI pass uploads the atlas of the world's context; copying
    /// the written regions keeps the list's texture coordinates pointing at
    /// the same glyphs, so the fallback draws the same content the overlay
    /// would have.
    pub fn adopt_atlas(&mut self, update: &AtlasUpdate) {
        for region in &update.regions {
            self.atlas.write_region(region.region, &region.pixels);
        }
    }

    /// Parse `source` as the project's stylesheet; an empty source goes back
    /// to the built-in look. Handing it the same text again is a no-op.
    ///
    /// Unsupported properties are reported through the log by name and line
    /// and never guessed at; the rest of the sheet still applies.
    pub fn set_stylesheet_source(&mut self, source: &str) {
        if self.stylesheet_source == source {
            return;
        }
        self.stylesheet = Stylesheet::parse(source);
        self.stylesheet_source = source.to_owned();
        for report in self.stylesheet.reports() {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::UI,
                "[ui] stylesheet: {report}"
            );
        }
    }

    /// Load the `.css` source at `path` (relative to the project's `res`)
    /// through the asset store and re-parse it whenever the store reports it
    /// changed - the dev host's asset watcher reimports an edited file, and
    /// the next call notices.
    ///
    /// The caller must have registered the type once per live generation
    /// (`World::register_imported_asset::<StylesheetAsset>()`). A path that
    /// cannot be loaded is reported once and otherwise ignored.
    pub fn sync_stylesheet(&mut self, assets: &mut AssetManager, path: &str) {
        if self.stylesheet_revision == Some(assets.revision()) {
            return;
        }
        self.stylesheet_revision = Some(assets.revision());

        if assets.get_by_name::<StylesheetAsset>(path).is_none() {
            let source = Path::new(path);
            let request =
                AssetImport::<StylesheetAsset>::new(source, MetadataPolicy::CreateIfMissing);
            match assets.import(request) {
                Ok(_) => self.stylesheet_import_reported = false,
                Err(_) if self.stylesheet_import_reported => return,
                Err(error) => {
                    self.stylesheet_import_reported = true;
                    pill_core::warn!(
                        target: pill_core::telemetry::telemetry_target::UI,
                        "[ui] stylesheet `{path}` did not load: {error}"
                    );
                    return;
                }
            }
        }
        let Some(handle) = assets.handle_by_name::<StylesheetAsset>(path) else {
            return;
        };
        let version = assets.content_version(handle).unwrap_or(0);
        if self.stylesheet_version == Some(version) {
            return;
        }
        self.stylesheet_version = Some(version);
        if let Some(sheet) = assets.get(handle) {
            self.set_stylesheet_source(sheet.source());
        }
    }
}

// =============================================================================
// Input routing
// =============================================================================

/// Rebuild the UI's event stream from the engine's per-frame input state.
fn route_input(state: &mut UiState, input: &Input, scale_factor: f32) {
    let logical =
        |position: Vector2f| Vector2f::new(position.x / scale_factor, position.y / scale_factor);
    let mut send = |event: InputEvent| state.input.apply(&event);

    match input.cursor_position() {
        Some(cursor) => send(InputEvent::CursorMoved {
            position: logical(cursor),
        }),
        None => send(InputEvent::CursorLeft),
    }
    for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
        if input.mouse_button_pressed(button) {
            send(InputEvent::MouseButton {
                button,
                state: ButtonState::Pressed,
            });
        }
        if input.mouse_button_released(button) {
            send(InputEvent::MouseButton {
                button,
                state: ButtonState::Released,
            });
        }
    }
    let lines = input.scroll_lines();
    if lines != Vector2f::ZERO {
        send(InputEvent::MouseWheel {
            delta: ScrollDelta::Lines(lines),
        });
    }
    let pixels = input.scroll_pixels();
    if pixels != Vector2f::ZERO {
        send(InputEvent::MouseWheel {
            delta: ScrollDelta::Pixels(logical(pixels)),
        });
    }
    for key in input.keys_pressed() {
        send(InputEvent::Key {
            key,
            state: ButtonState::Pressed,
        });
    }
    for key in input.keys_released() {
        send(InputEvent::Key {
            key,
            state: ButtonState::Released,
        });
    }
    if !input.text_entered().is_empty() {
        send(InputEvent::TextInput {
            text: input.text_entered().to_owned(),
        });
    }
    if let Some(preedit) = input.ime_preedit() {
        send(InputEvent::ImePreedit {
            text: preedit.to_owned(),
            cursor: input.ime_cursor(),
        });
    }
    send(InputEvent::ModifiersChanged {
        modifiers: input.modifiers(),
    });
}

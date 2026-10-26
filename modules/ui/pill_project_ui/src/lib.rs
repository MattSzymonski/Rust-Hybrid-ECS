//! The game UI: an egui context a project draws through the main renderer.
//!
//! # Responsibilities
//!
//! - [`GameUi`]: the resource holding the egui context, the published pointer
//!   and keyboard claims, and the texture mirror.
//! - [`GameUi::run`]: one pass - the engine's input in, the frame's draw list
//!   and textures out, published into the world's
//!   [`UiFrameSlot`](pill_ui_frame::UiFrameSlot).
//! - [`install`]: puts the resource into a project's world, registering its
//!   type first so a reload finds the same one.
//! - [`install_with_theme`] and [`ThemeInstall`]: the project's code default
//!   merged with an optional `.theme` override, plus the system that
//!   re-applies it when the file is reimported (W-0127).
//! - [`GameUi::theme`] and [`GameUi::apply_theme`]: the tokens the context
//!   paints with, and the one install entry point.
//! - [`convert`], [`input`], [`textures`]: the three pieces it composes.
//!
//! # Design
//!
//! This crate is a shared `dylib` (plan decision D2) so the egui context
//! outlives a project reload: a rebuilt project finds its fonts, its widget
//! state and its textures still there. The same reload contract as
//! `pill_editor_core` binds - nothing module-defined may be stored in the context
//! - and [`GameUi`] itself lives here, so the type is one across artifacts.
//!
//! The frame it produces is neutral (`pill_ui_frame`) and travels through the
//! draw crate's frame slot (`pill_ui_frame::UiFrameSlot`): the renderer and `pill_renderer_api` never see
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
//! pill_project_ui::install(engine);
//!
//! // In a system, once per frame:
//! let input = pill_project_ui::InputFrame::capture(world.get_resource::<Input>().unwrap());
//! let mut ui = world.get_resource_mut::<GameUi>().unwrap();
//! let mut slot = world.get_resource_mut::<UiFrameSlot>().unwrap();
//! ui.run(&mut slot, &input, viewport, scale_factor, pill_project_ui::PRIMARY_SURFACE, |ui| {
//!     ui.heading("Tuning");
//!     ui.label(format!("fps {fps}"));
//! });
//! ```

/// The tessellated output to `pill_ui_frame`.
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
pub use pill_ui_frame::UiFrameSlot;
/// The UI kit: the theme-to-egui translator, the font installer and the
/// scoped style variants (W-0127). Re-exported so a project never names the
/// rlib directly.
pub use pill_ui_kit as kit;

// External crates
use egui::Context;
use pill_core::math::Vector2f;
use pill_core::platform::Instant;
use pill_engine_core::{
    AssetManager, Engine, Handle, Res, ResMut, Resource, SystemError, SystemOwner,
};
use pill_ui_frame::{DrawList, UiDraw, UiFrame};

// Current crate
pub use convert::{convert, external_id, straight_color};
pub use input::{raw_input, InputFrame, InputMemory};
/// The theme data a project's UI installs (W-0127).
pub use pill_ui_theme::{Theme, ThemeOverrides, ThemePathScope};
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
    /// The painter's resend request this UI last served.
    ///
    /// A rebuilt renderer starts with an empty texture map and asks for every
    /// image again; the request is a number so the producer can tell a fresh
    /// one from the one it already answered.
    served_resend_generation: u64,
    /// The project's code default, before any override.
    base: Theme,
    /// The theme the context currently paints with.
    applied: Theme,
    /// How many themes this UI has applied.
    generation: u64,
    /// The override path the project named, when one loaded.
    override_path: Option<String>,
    /// The loaded override, when one loaded.
    override_handle: Option<Handle<ThemeOverrides>>,
    /// The override's content version the applied theme was merged from.
    override_version: Option<u64>,
    /// The three font families' handles, in [`ThemeFonts`] order (body,
    /// display, monospace).
    font_handles: [Option<Handle<pill_ui_theme::FontAsset>>; 3],
    /// Those handles' content versions when the fonts were installed.
    font_versions: [Option<u64>; 3],
    /// The bytes the kit installs.
    fonts: kit::style::ThemeFonts,
    /// The font set installed in the context.
    font_signature: kit::style::FontSignature,
}

impl Resource for GameUi {}

impl Default for GameUi {
    fn default() -> Self {
        GameUi::new()
    }
}

impl GameUi {
    /// A UI with a fresh context, the fallback theme installed and no
    /// textures.
    pub fn new() -> GameUi {
        let theme = Theme::fallback();
        let mut game_ui = GameUi {
            context: Context::default(),
            input_memory: InputMemory::new(),
            textures: TextureMirror::new(),
            started: Instant::now(),
            draws: 0,
            wants_pointer: false,
            wants_keyboard: false,
            served_resend_generation: 0,
            base: theme.clone(),
            applied: theme.clone(),
            generation: 1,
            override_path: None,
            override_handle: None,
            override_version: None,
            font_handles: [None; 3],
            font_versions: [None; 3],
            fonts: kit::style::ThemeFonts::default(),
            font_signature: kit::style::FontSignature::default(),
        };
        // The first install is silent: a project's own theme logs the line
        // that matters.
        kit::style::install(
            &game_ui.context,
            &theme,
            &game_ui.fonts,
            |_| {},
            &mut game_ui.font_signature,
        );
        game_ui
    }

    /// The egui context a project's UI closure runs in.
    ///
    /// Exposed for the callers a facade cannot wrap - a project that installs
    /// its own style, or a test that inspects egui's own state.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// The theme the context currently paints with.
    pub fn theme(&self) -> &Theme {
        &self.applied
    }

    /// The code default the active theme was merged from.
    pub fn base_theme(&self) -> &Theme {
        &self.base
    }

    /// How many themes this UI has applied.
    pub fn theme_generation(&self) -> u64 {
        self.generation
    }

    /// Install `theme` on this UI's context.
    ///
    /// This is the one non-generic install entry point (D11), so no closure
    /// from a reloadable image ends up in the context's style.
    pub fn apply_theme(&mut self, theme: &Theme) {
        self.install_theme(theme);
    }

    /// Install `theme` and record it, logging the application.
    fn install_theme(&mut self, theme: &Theme) {
        let fonts_installed = kit::style::install(
            &self.context,
            theme,
            &self.fonts,
            |_| {},
            &mut self.font_signature,
        );
        if fonts_installed {
            pill_core::info!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                signature = %self.font_signature,
                "[ui] fonts installed"
            );
        }
        self.applied = theme.clone();
        self.generation = self.generation.wrapping_add(1);
        let layers = if self.override_handle.is_some() {
            "code,file"
        } else {
            "code"
        };
        let path = self.override_path.as_deref().unwrap_or("none");
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            layers = %layers,
            path = %path,
            generation = self.generation,
            accent = %theme.palette.accent.hex(),
            corner = theme.shape.corner_radius,
            "[ui] theme applied"
        );
    }

    /// Re-merge the base with the override when the override's content moved.
    ///
    /// A refused merge warns and keeps the theme that is already applied. An
    /// edited font re-installs the fonts on its own: the store's content
    /// version of the font moved, even when the theme did not.
    fn refresh_from_assets(&mut self, assets: &AssetManager) {
        let fonts_moved = self.refresh_font_bytes(assets);
        let Some(handle) = self.override_handle else {
            if fonts_moved {
                let theme = self.applied.clone();
                self.install_theme(&theme);
            }
            return;
        };
        let version = assets.content_version(handle);
        if version == self.override_version && !fonts_moved {
            return;
        }
        self.override_version = version;
        let Some(overrides) = assets.get(handle) else {
            return;
        };
        match self.base.merged_with(overrides) {
            Ok(theme) => {
                if theme != self.applied || fonts_moved {
                    self.install_theme(&theme);
                }
            }
            Err(error) => {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    "[ui] theme refused: {error}"
                );
            }
        }
    }

    /// Re-read the loaded fonts whose content moved, and report whether one
    /// did.
    ///
    /// The bytes are copied out of the store again; the handle, the name and
    /// the guid stay as they were, so the context sees an edited file as a new
    /// font set and nothing else.
    fn refresh_font_bytes(&mut self, assets: &AssetManager) -> bool {
        let mut moved = false;
        for index in 0..FONT_FAMILIES {
            let Some(handle) = self.font_handles[index] else {
                continue;
            };
            let version = assets.content_version(handle);
            if version == self.font_versions[index] {
                continue;
            }
            self.font_versions[index] = version;
            let bytes = assets.get(handle).map(|font| font.bytes.clone());
            set_font_family(&mut self.fonts, index, bytes);
            moved = true;
        }
        moved
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
        // A painter that was rebuilt lost every texture it had and asks for
        // them again; nothing else would re-send an image whose only change
        // was the painter's own reload.
        let resend_generation = slot.texture_resend_generation();
        if resend_generation != self.served_resend_generation {
            self.textures.resend_all();
            self.served_resend_generation = resend_generation;
        }
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

/// A project's UI theme: its code default and an optional override file.
///
/// The project builds the base in its own code (the builder, or
/// [`Theme::brutalist`]) and names a `.theme` file below `res`; the file
/// states only the items it changes.
#[derive(Clone, Debug)]
pub struct ThemeInstall {
    /// The project's code default.
    pub base: Theme,
    /// A `.theme` override below `res`, when the project names one.
    pub override_path: Option<String>,
}

impl ThemeInstall {
    /// A theme install with no override file.
    pub fn new(base: Theme) -> Self {
        Self {
            base,
            override_path: None,
        }
    }

    /// Name the `.theme` file merged over the base.
    pub fn with_override(mut self, path: impl Into<String>) -> Self {
        self.override_path = Some(path.into());
        self
    }
}

/// Put a [`GameUi`] into `engine`'s world with the fallback theme, the frame
/// slot a renderer drains, and the theme re-apply system.
///
/// A project that wants its own look calls [`install_with_theme`] instead.
/// An existing UI is kept, so a project reload does not reset the context, its
/// fonts or the state the player typed into it; the slot is what tells a
/// renderer that this world's UI is the game's.
pub fn install(engine: &mut Engine) {
    install_with_theme(engine, ThemeInstall::new(Theme::fallback()));
}

/// Install a project's theme: the code default, merged with an optional
/// `.theme` override, plus the system that re-applies it on a reimport.
///
/// A refused override warns and leaves the code default in place; an
/// `editor/` path is refused because editor content never reaches a game. An
/// existing [`GameUi`] keeps its context and textures; only the base and the
/// override are replaced, and the theme is applied again only when the merge
/// differs from the one already applied.
pub fn install_with_theme(engine: &mut Engine, install: ThemeInstall) {
    let ThemeInstall {
        base,
        override_path,
    } = install;
    {
        let world = engine.world_mut();
        pill_ui_theme::register(world);
        world.register_resource::<GameUi>();
        world.register_resource::<UiFrameSlot>();
        if world.get_resource::<GameUi>().is_none() {
            world.insert_resource(GameUi::new());
        }
        if world.get_resource::<UiFrameSlot>().is_none() {
            world.insert_resource(UiFrameSlot::new());
        }

        // Resolve the override before borrowing the UI.
        let mut path = override_path;
        let mut handle = None;
        if let Some(named) = path.clone() {
            match pill_ui_theme::load_overrides(world, &named, ThemePathScope::Game) {
                Ok(loaded) => handle = Some(loaded),
                Err(error) => {
                    pill_core::warn!(
                        target: pill_core::telemetry::telemetry_target::ENGINE,
                        "[ui] theme refused: {error}"
                    );
                    path = None;
                }
            }
        }

        // Merge and read the version before the UI borrow.
        let (merged, version) = match handle {
            Some(handle) => {
                let assets = world
                    .get_resource::<AssetManager>()
                    .expect("the engine owns an asset manager");
                let version = assets.content_version(handle);
                let merged = match assets.get(handle) {
                    Some(overrides) => match base.merged_with(overrides) {
                        Ok(theme) => Some(theme),
                        Err(error) => {
                            pill_core::warn!(
                                target: pill_core::telemetry::telemetry_target::ENGINE,
                                "[ui] theme refused: {error}"
                            );
                            None
                        }
                    },
                    None => None,
                };
                (merged, version)
            }
            None => (None, None),
        };
        let merged = merged.unwrap_or_else(|| base.clone());
        let resolved = resolve_fonts(world, &merged);

        let game_ui = world.get_resource_mut::<GameUi>().expect("inserted above");
        game_ui.base = base.clone();
        game_ui.override_path = path;
        game_ui.override_handle = handle;
        game_ui.override_version = version;
        let fonts_changed = game_ui.fonts != resolved.fonts;
        game_ui.font_handles = resolved.handles;
        game_ui.font_versions = resolved.versions;
        game_ui.fonts = resolved.fonts;
        if merged != game_ui.applied || fonts_changed {
            game_ui.install_theme(&merged);
        }
    }
    register_reload_system(engine);
}

/// How many families a theme may name, in [`kit::style::ThemeFonts`] order.
const FONT_FAMILIES: usize = 3;

/// The fonts a theme's paths resolved to.
#[derive(Default)]
struct ResolvedFonts {
    /// Each family's handle in the store.
    handles: [Option<Handle<pill_ui_theme::FontAsset>>; FONT_FAMILIES],
    /// Those handles' content versions when the bytes were read.
    versions: [Option<u64>; FONT_FAMILIES],
    /// The bytes the kit installs.
    fonts: kit::style::ThemeFonts,
}

/// Resolve a theme's font paths through the project's asset store.
///
/// A refused font warns and leaves its family to the built-in ones; the other
/// families and the theme's own values still apply.
fn resolve_fonts(world: &mut pill_engine_core::World, theme: &Theme) -> ResolvedFonts {
    let paths = [
        theme.fonts.body.clone(),
        theme.fonts.display.clone(),
        theme.fonts.monospace.clone(),
    ];
    let mut resolved = ResolvedFonts::default();
    for (index, path) in paths.into_iter().enumerate() {
        let Some(path) = path else {
            continue;
        };
        match pill_ui_theme::load_font(world, &path, ThemePathScope::Game) {
            Ok(loaded) => {
                resolved.handles[index] = Some(loaded.handle);
                resolved.versions[index] = loaded.version;
            }
            Err(message) => pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::ENGINE,
                "[ui] font refused: {message}"
            ),
        }
    }
    if let Some(assets) = world.get_resource::<AssetManager>() {
        for (index, handle) in resolved.handles.iter().enumerate() {
            let bytes = handle
                .and_then(|handle| assets.get(handle))
                .map(|font| font.bytes.clone());
            set_font_family(&mut resolved.fonts, index, bytes);
        }
    }
    resolved
}

/// Put one family's bytes where [`ThemeFonts`] holds them.
fn set_font_family(fonts: &mut kit::style::ThemeFonts, index: usize, bytes: Option<Vec<u8>>) {
    match index {
        0 => fonts.body = bytes,
        1 => fonts.display = bytes,
        _ => fonts.monospace = bytes,
    }
}

/// The system `install_with_theme` registers.
const THEME_SYSTEM_NAME: &str = "ui_theme";

/// Register the re-apply system once per project generation.
///
/// The engine retires a generation's systems on reload, so the call after a
/// reload registers a fresh one; the guard only stops a second identical
/// `install_with_theme` inside one `init` from stacking a duplicate.
fn register_reload_system(engine: &mut Engine) {
    let registered = engine
        .system_snapshots()
        .iter()
        .any(|system| system.name == THEME_SYSTEM_NAME && system.owner == SystemOwner::PROJECT);
    if !registered {
        engine.register_system(THEME_SYSTEM_NAME, theme_reload_system);
    }
}

/// Re-merge and re-apply when the override's content version moved.
fn theme_reload_system(
    mut game_ui: ResMut<GameUi>,
    assets: Res<AssetManager>,
) -> Result<(), SystemError> {
    let (Some(mut game_ui), Some(assets)) = (game_ui.get_mut(), assets.get()) else {
        return Ok(());
    };
    game_ui.refresh_from_assets(assets);
    Ok(())
}

#[cfg(test)]
mod tests {
    // Standard library
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    // External crates
    use pill_engine_core::asset_store::mount_directory;
    use pill_engine_core::input::{ButtonState, Input, InputEvent, KeyCode};
    use pill_ui_theme::Rgba;

    // Current crate
    use super::*;

    /// Serializes this crate's tests that mount the process-wide asset
    /// directory.
    static MOUNT_LOCK: Mutex<()> = Mutex::new(());

    /// A scratch `res` directory mounted under the mount lock.
    struct ScratchRes {
        root: PathBuf,
        _mounted: MutexGuard<'static, ()>,
    }

    impl ScratchRes {
        fn new(label: &str) -> Self {
            let mounted = MOUNT_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = std::env::temp_dir().join(format!(
                "pill-project-ui-theme-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("ui")).unwrap();
            mount_directory(&root);
            Self {
                root,
                _mounted: mounted,
            }
        }

        /// Write a theme file with the standard header and `body` under
        /// `asset`.
        fn write_theme(&self, relative: &str, body: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!(
                    r#"{{"format_version": 1, "asset_type": "pill_ui_theme::ThemeOverrides", "guid": "000000000000000000000000000000a1", "asset": {body}}}"#
                ),
            )
            .unwrap();
        }

        /// Write a font file's bytes at `relative`.
        fn write_font(&self, relative: &str, bytes: &[u8]) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
    }

    impl Drop for ScratchRes {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Install a brutalist base with `ui/accent.theme` holding `body`.
    fn install_with_override(body: &str) -> (Engine, ScratchRes) {
        let res = ScratchRes::new("install");
        res.write_theme("ui/accent.theme", body);
        let mut engine = Engine::new();
        install_with_theme(
            &mut engine,
            ThemeInstall::new(Theme::brutalist()).with_override("ui/accent.theme"),
        );
        (engine, res)
    }

    fn generation(engine: &Engine) -> u64 {
        engine
            .world()
            .get_resource::<GameUi>()
            .unwrap()
            .theme_generation()
    }

    fn reimport(engine: &mut Engine) {
        let assets = engine
            .world_mut()
            .get_resource_mut::<AssetManager>()
            .unwrap();
        let _ = assets
            .reimport_standalone::<ThemeOverrides>(Path::new("ui/accent.theme"))
            .unwrap();
    }

    /// A theme whose code default names `body` as its body font.
    fn theme_with_body_font(body: &str) -> Theme {
        let mut theme = Theme::brutalist();
        theme.fonts.body = Some(body.to_string());
        theme
    }

    /// A font a theme names is loaded, copied into the UI and recorded, and an
    /// edit to it re-installs the fonts without a theme change.
    #[test]
    fn a_theme_font_loads_and_an_edit_reinstalls_it() {
        let res = ScratchRes::new("fonts");
        res.write_font("fonts/body.ttf", b"\x00\x01\x00\x00first");
        let mut engine = Engine::new();
        install_with_theme(
            &mut engine,
            ThemeInstall::new(theme_with_body_font("fonts/body.ttf")),
        );
        let before = generation(&engine);
        {
            let ui = engine.world().get_resource::<GameUi>().unwrap();
            assert_eq!(
                ui.fonts.body.as_deref(),
                Some(&b"\x00\x01\x00\x00first"[..])
            );
            assert!(ui.font_handles[0].is_some(), "the font has a handle");
        }

        res.write_font("fonts/body.ttf", b"\x00\x01\x00\x00second");
        {
            let assets = engine
                .world_mut()
                .get_resource_mut::<AssetManager>()
                .unwrap();
            let outcome = assets
                .reimport::<pill_ui_theme::FontAsset>(Path::new("fonts/body.ttf"))
                .expect("the edited font reimports");
            assert!(outcome.replaced, "the store replaced the font");
        }
        // The reload system is what catches the store's new version in a real
        // run; one frame runs it.
        engine.process_frame().expect("the frame runs");

        let ui = engine.world().get_resource::<GameUi>().unwrap();
        assert_eq!(
            ui.fonts.body.as_deref(),
            Some(&b"\x00\x01\x00\x00second"[..]),
            "the edited font's bytes replaced the old ones"
        );
        assert!(generation(&engine) > before, "the fonts re-installed");
    }

    /// A font a theme names but the project does not ship is refused, and the
    /// families the built-ins provide still answer: the install happens, the
    /// theme's other values apply, and the family name stays registered.
    #[test]
    fn a_missing_font_is_refused_and_the_family_falls_back() {
        let _res = ScratchRes::new("missing-font");
        let mut engine = Engine::new();
        install_with_theme(
            &mut engine,
            ThemeInstall::new(theme_with_body_font("fonts/missing.ttf")),
        );
        let ui = engine.world().get_resource::<GameUi>().unwrap();
        assert_eq!(ui.fonts.body, None, "the refused font installed nothing");
        assert!(ui.font_handles[0].is_none());
        assert_eq!(ui.theme().palette.accent, Theme::brutalist().palette.accent);
        let definitions = kit::style::font_definitions(&ui.fonts, |_| {});
        assert!(definitions
            .families
            .get(&egui::FontFamily::Proportional)
            .is_some_and(|family| !family.is_empty()));
    }

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
                .any(|texture| texture.id == pill_ui_frame::TextureId(1 << 63)),
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

    #[test]
    fn plain_install_applies_the_fallback() {
        let mut engine = Engine::new();
        install(&mut engine);
        let ui = engine.world().get_resource::<GameUi>().unwrap();
        assert_eq!(ui.theme(), &Theme::fallback());
        assert!(ui.override_handle.is_none());
    }

    #[test]
    fn an_override_changes_only_the_items_it_names() {
        let (engine, _res) = install_with_override(
            r##"{"palette": {"accent": "#33d17a"}, "text_styles": {"heading": {"size": 24.0}}}"##,
        );
        let ui = engine.world().get_resource::<GameUi>().unwrap();
        assert_eq!(ui.theme().palette.accent, Rgba::rgb(0x33, 0xd1, 0x7a));
        assert_eq!(ui.theme().text_styles["heading"].size, 24.0);
        assert_eq!(ui.theme().palette.panel, Theme::brutalist().palette.panel);
        assert!(ui.override_handle.is_some());
    }

    #[test]
    fn a_reimport_raises_the_generation() {
        let (mut engine, res) = install_with_override(r##"{"palette": {"accent": "#111111"}}"##);
        let before = generation(&engine);
        res.write_theme("ui/accent.theme", r##"{"palette": {"accent": "#222222"}}"##);
        reimport(&mut engine);
        engine.process_frame().expect("the frame runs");
        let ui = engine.world().get_resource::<GameUi>().unwrap();
        assert!(ui.theme_generation() > before);
        assert_eq!(ui.theme().palette.accent, Rgba::rgb(0x22, 0x22, 0x22));
    }

    #[test]
    fn an_unchanged_version_does_not_re_apply() {
        let (mut engine, _res) = install_with_override(r##"{"palette": {"accent": "#111111"}}"##);
        let before = generation(&engine);
        engine.process_frame().expect("the frame runs");
        assert_eq!(generation(&engine), before);
    }

    #[test]
    fn a_refused_override_keeps_the_previous_theme() {
        let (mut engine, res) = install_with_override(r##"{"palette": {"accent": "#111111"}}"##);
        let before = generation(&engine);
        res.write_theme("ui/accent.theme", r##"{"palette": {"accent": "nope"}}"##);
        reimport(&mut engine);
        engine.process_frame().expect("the frame runs");
        let ui = engine.world().get_resource::<GameUi>().unwrap();
        assert_eq!(ui.theme_generation(), before);
        assert_eq!(ui.theme().palette.accent, Rgba::rgb(0x11, 0x11, 0x11));
    }

    #[test]
    fn an_editor_path_is_refused() {
        let mut engine = Engine::new();
        install_with_theme(
            &mut engine,
            ThemeInstall::new(Theme::brutalist()).with_override("editor/a.theme"),
        );
        let ui = engine.world().get_resource::<GameUi>().unwrap();
        assert_eq!(ui.theme(), &Theme::brutalist());
        assert!(ui.override_handle.is_none());
        assert!(ui.override_path.is_none());
    }

    #[test]
    fn installing_twice_with_the_same_input_does_not_re_apply() {
        let (mut engine, _res) = install_with_override(r##"{"palette": {"accent": "#111111"}}"##);
        let before = generation(&engine);
        install_with_theme(
            &mut engine,
            ThemeInstall::new(Theme::brutalist()).with_override("ui/accent.theme"),
        );
        assert_eq!(generation(&engine), before);
        let systems = engine
            .system_snapshots()
            .into_iter()
            .filter(|system| system.name == "ui_theme")
            .count();
        assert_eq!(systems, 1);
    }
}

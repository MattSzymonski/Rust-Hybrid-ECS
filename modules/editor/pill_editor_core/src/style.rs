//! The editor's egui look: one `Theme` built in code, and the palette the
//! shell reads.
//!
//! # Responsibilities
//!
//! - [`default_theme`]: the editor's look as theme tokens, built in code.
//! - [`ThemeLayer`] and [`UserThemeSelection`]: where a theme layer comes
//!   from - the code default, the user's file, the project's - and the merge
//!   that folds them in order.
//! - [`apply_theme`] and [`apply`]: the one install entry point, and the
//!   re-install a pop-out window or the no-shell fallback needs.
//! - [`palette`] and the role accessors ([`text`], [`panel`], [`accent`], ...):
//!   the colours the shell needs beyond egui's own visuals.
//! - [`dock_style`]: the `egui_dock` styling (flat tabs merged with their
//!   bodies, thin separators) built from the same tokens.
//!
//! # Design
//!
//! Styling is egui `Style`/`Visuals` data, never CSS: the shell asks for a role
//! and holds no colour value of its own. [`apply_theme`] is the only writer of
//! the recorded palette, and the record lives in this crate - a shared dylib
//! that never reloads - so a shell reload leaves it untouched and the reloaded
//! shell reads the same roles. It holds `Color32` copies only: nothing a module
//! image owns outlives its image.

// Standard library
use std::sync::{Arc, OnceLock, RwLock};

// External crates
use egui::{Color32, Context, CornerRadius, Stroke};
use pill_ui_theme::{
    Rgba, Shadow as TokenShadow, Stroke as TokenStroke, Theme, WidgetStates, WidgetStyle,
};

// Current crate
use crate::kit;

/// The UI font: Inter, vendored under `assets/` with its OFL text.
const BODY_FONT: &[u8] = include_bytes!("../assets/Inter-Variable.ttf");
/// The code font: JetBrains Mono, vendored under `assets/` with its OFL text.
const MONOSPACE_FONT: &[u8] = include_bytes!("../assets/JetBrainsMono-Variable.ttf");
/// The keys the two families are registered under.
const BODY_KEY: &str = "editor.body";
const MONOSPACE_KEY: &str = "editor.monospace";

// =============================================================================
// The recorded palette
// =============================================================================

/// The editor's colour roles as egui colours, recorded from the last installed
/// theme.
///
/// The shell reads these through the role accessors below; it never names the
/// theme's `Rgba` values, so a restyle moves every call site at once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// The window frame and tab-strip background.
    pub frame: Color32,
    /// A panel's and tab body's background.
    pub panel: Color32,
    /// A raised surface: menus, hovered rows, tooltips.
    pub panel_raised: Color32,
    /// A flat control's fill on a panel.
    pub control: Color32,
    /// Inputs and wells.
    pub well: Color32,
    /// Borders and separators.
    pub line: Color32,
    /// Normal text.
    pub text: Color32,
    /// Emphasized text: active tab titles, hovered labels.
    pub text_strong: Color32,
    /// Muted text: inactive tab titles, secondary labels.
    pub text_muted: Color32,
    /// The accent: links and highlights.
    pub accent: Color32,
    /// A brighter accent for hovered accents.
    pub accent_hot: Color32,
    /// Success and running states.
    pub success: Color32,
    /// Warnings.
    pub warning: Color32,
    /// Errors and destructive actions.
    pub danger: Color32,
    /// A selected row's tint.
    pub selection: Color32,
    /// A selected row's left bar.
    pub selection_bar: Color32,
    /// A floating panel's fill over the scene.
    pub overlay: Color32,
    /// A floating panel's border.
    pub overlay_border: Color32,
    /// The ink a drop shadow is cast in.
    pub shadow: Color32,
    /// A log line at `info`.
    pub log_info: Color32,
    /// A log line at `debug`.
    pub log_debug: Color32,
    /// A log line at `warn`.
    pub log_warn: Color32,
    /// A log line at `error`.
    pub log_error: Color32,
}

impl Palette {
    /// The roles of `theme`.
    fn from_theme(theme: &Theme) -> Self {
        let palette = &theme.palette;
        Self {
            frame: color(palette.frame),
            panel: color(palette.panel),
            panel_raised: color(palette.panel_raised),
            control: color(palette.control),
            well: color(palette.well),
            line: color(palette.line),
            text: color(palette.text),
            text_strong: color(palette.text_strong),
            text_muted: color(palette.text_muted),
            accent: color(palette.accent),
            accent_hot: color(palette.accent_hot),
            success: color(palette.success),
            warning: color(palette.warning),
            danger: color(palette.danger),
            selection: color(palette.selection),
            selection_bar: color(palette.selection_bar),
            overlay: color(palette.overlay),
            overlay_border: color(palette.overlay_border),
            shadow: color(theme.shape.window_shadow.color),
            log_info: color(palette.log_info),
            log_debug: color(palette.log_debug),
            log_warn: color(palette.log_warn),
            log_error: color(palette.log_error),
        }
    }
}

/// The theme the editor last resolved, with the palette it records.
struct Resolved {
    /// The tokens every context paints with.
    theme: Theme,
    /// [`Resolved::theme`] as egui colours.
    palette: Palette,
    /// The layers [`Resolved::theme`] was merged from, in apply order.
    layers: Vec<ThemeLayerInfo>,
    /// The font bytes the layers resolved.
    fonts: kit::style::ThemeFonts,
    /// The executable's layer generation; moving it means a new theme to
    /// install.
    generation: u64,
    /// How many context installs have run; the log line's `contexts`.
    contexts: u64,
    /// Whether a theme has ever been installed, so the first one logs.
    installed: bool,
}

/// The one record every accessor reads.
static RESOLVED: OnceLock<RwLock<Resolved>> = OnceLock::new();

/// The record, seeded with the code default so a read before the first install
/// still answers.
fn resolved() -> &'static RwLock<Resolved> {
    RESOLVED.get_or_init(|| {
        let theme = default_theme();
        let palette = Palette::from_theme(&theme);
        RwLock::new(Resolved {
            theme,
            palette,
            layers: vec![ThemeLayerInfo::code()],
            fonts: kit::style::ThemeFonts::default(),
            generation: 1,
            contexts: 0,
            installed: false,
        })
    })
}

/// Read the record, ignoring a panic another thread took while writing it.
fn read_resolved<T>(read: impl FnOnce(&Resolved) -> T) -> T {
    let record = resolved()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    read(&record)
}

/// The palette the last installed theme recorded.
pub fn palette() -> Palette {
    read_resolved(|resolved| resolved.palette)
}

/// The theme the editor last installed.
pub fn current_theme() -> Theme {
    read_resolved(|resolved| resolved.theme.clone())
}

/// The generation of the theme the editor last installed: what a shell reads
/// to decide whether the executable's next resolution is a theme it already
/// paints.
pub fn generation() -> u64 {
    read_resolved(|resolved| resolved.generation)
}

/// The layers the installed theme was merged from.
pub fn current_layers() -> Vec<ThemeLayerInfo> {
    read_resolved(|resolved| resolved.layers.clone())
}

/// The window frame and tab-strip background.
pub fn frame() -> Color32 {
    palette().frame
}

/// A panel's and tab body's background.
pub fn panel() -> Color32 {
    palette().panel
}

/// A raised surface: menus, hovered rows, tooltips.
pub fn panel_raised() -> Color32 {
    palette().panel_raised
}

/// A flat control's fill on a panel.
pub fn control() -> Color32 {
    palette().control
}

/// Inputs and wells.
pub fn well() -> Color32 {
    palette().well
}

/// Borders and separators.
pub fn line() -> Color32 {
    palette().line
}

/// Normal text.
pub fn text() -> Color32 {
    palette().text
}

/// Emphasized text.
pub fn text_strong() -> Color32 {
    palette().text_strong
}

/// Muted text.
pub fn text_muted() -> Color32 {
    palette().text_muted
}

/// The accent: links and highlights.
pub fn accent() -> Color32 {
    palette().accent
}

/// A brighter accent for hovered accents.
pub fn accent_hot() -> Color32 {
    palette().accent_hot
}

/// Success and running states.
pub fn success() -> Color32 {
    palette().success
}

/// Warnings.
pub fn warning() -> Color32 {
    palette().warning
}

/// Errors and destructive actions.
pub fn danger() -> Color32 {
    palette().danger
}

/// A selected row's tint.
pub fn selection() -> Color32 {
    palette().selection
}

/// A selected row's left bar.
pub fn selection_bar() -> Color32 {
    palette().selection_bar
}

/// A floating panel's fill over the scene.
pub fn overlay() -> Color32 {
    palette().overlay
}

/// A floating panel's border.
pub fn overlay_border() -> Color32 {
    palette().overlay_border
}

/// The ink a drop shadow is cast in: the theme's window-shadow colour, so a
/// mark floating over the scene shadows itself in the same ink as every
/// other floating surface.
pub fn shadow() -> Color32 {
    palette().shadow
}

/// A log line at `info`.
pub fn log_info() -> Color32 {
    palette().log_info
}

/// A log line at `debug`.
pub fn log_debug() -> Color32 {
    palette().log_debug
}

/// A log line at `warn`.
pub fn log_warn() -> Color32 {
    palette().log_warn
}

/// A log line at `error`.
pub fn log_error() -> Color32 {
    palette().log_error
}

/// Paint the selected-row wash and its left accent bar over `rect`.
///
/// The Hierarchy, Entities and Assets rows call this - or [`selected_row_bar`]
/// when the widget already fills the row with the `selection` role - so one
/// selected row looks the same in every panel: the wash is the `selection`
/// colour and the bar the `selection_bar` one, three points wide as the
/// reference draws it.
pub fn selected_row(ui: &egui::Ui, rect: egui::Rect) {
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, palette().selection);
    selected_row_bar(ui, rect);
}

/// Paint only the selected row's left accent bar, for a widget that already
/// fills its row with the `selection` colour.
pub fn selected_row_bar(ui: &egui::Ui, rect: egui::Rect) {
    let bar = egui::Rect::from_min_size(rect.left_top(), egui::vec2(3.0, rect.height()));
    ui.painter()
        .rect_filled(bar, egui::CornerRadius::ZERO, palette().selection_bar);
}

// =============================================================================
// The editor's theme
// =============================================================================

/// The editor's look as a complete theme: one flat, compact dark style.
///
/// The values follow the editor's reference look: near-black panels, a crimson
/// accent on links, the active tab and the selected row's bar, and compact
/// controls drawn in Inter and JetBrains Mono.
pub fn default_theme() -> Theme {
    let mut theme = Theme::fallback();
    theme.name = "editor".to_owned();
    theme.dark_mode = true;
    theme.palette.frame = Rgba::rgb(18, 18, 20);
    theme.palette.panel = Rgba::rgb(26, 26, 29);
    theme.palette.panel_raised = Rgba::rgb(32, 32, 36);
    theme.palette.control = Rgba::rgb(36, 36, 41);
    theme.palette.well = Rgba::rgb(15, 15, 17);
    theme.palette.line = Rgba::rgb(46, 46, 52);
    theme.palette.text = Rgba::rgb(201, 201, 208);
    theme.palette.text_strong = Rgba::rgb(240, 240, 244);
    theme.palette.text_muted = Rgba::rgb(138, 138, 147);
    // The crimson accent: links, the active tab's underline, the selected
    // row's bar.
    theme.palette.accent = Rgba::rgb(200, 53, 63);
    theme.palette.accent_hot = Rgba::rgb(224, 69, 80);
    theme.palette.success = Rgba::rgb(111, 191, 115);
    theme.palette.warning = Rgba::rgb(224, 176, 64);
    theme.palette.danger = Rgba::rgb(229, 72, 77);
    // The selected row: a dark crimson wash under the accent bar.
    theme.palette.selection = Rgba::rgb(58, 28, 32);
    theme.palette.selection_bar = Rgba::rgb(200, 53, 63);
    theme.palette.overlay = Rgba::with_alpha(26, 26, 30, 236);
    theme.palette.overlay_border = Rgba::rgb(52, 52, 58);
    theme.palette.log_info = Rgba::rgb(106, 168, 224);
    theme.palette.log_debug = Rgba::rgb(138, 138, 147);
    theme.palette.log_warn = Rgba::rgb(224, 176, 64);
    theme.palette.log_error = Rgba::rgb(229, 72, 77);

    theme.shape.corner_radius = 3;
    theme.shape.window_corner_radius = 5;
    theme.shape.menu_corner_radius = 5;
    theme.shape.window_stroke = TokenStroke::new(1.0, Rgba::rgb(46, 46, 52));
    theme.shape.window_shadow = TokenShadow::new([0, 8], 24, 0, Rgba::with_alpha(0, 0, 0, 160));
    theme.shape.popup_shadow = TokenShadow::new([0, 6], 16, 0, Rgba::with_alpha(0, 0, 0, 160));
    theme.shape.selection_stroke = TokenStroke::new(1.0, Rgba::rgb(240, 240, 244));
    theme.shape.tab_underline_width = 2.0;
    theme.shape.selected_row_bar_width = 3.0;

    // The five widget states: fills carry the state, the stroke stays subtle,
    // the corners stay tight.
    theme.widgets = WidgetStates {
        noninteractive: widget(
            Rgba::rgb(26, 26, 29),
            Rgba::rgb(26, 26, 29),
            Some((1.0, Rgba::rgb(46, 46, 52))),
            Rgba::rgb(201, 201, 208),
        ),
        inactive: widget(
            Rgba::rgb(38, 38, 43),
            Rgba::rgb(38, 38, 43),
            None,
            Rgba::rgb(201, 201, 208),
        ),
        hovered: widget(
            Rgba::rgb(46, 46, 52),
            Rgba::rgb(46, 46, 52),
            Some((1.0, Rgba::rgb(70, 70, 78))),
            Rgba::rgb(240, 240, 244),
        ),
        active: widget(
            Rgba::rgb(54, 54, 61),
            Rgba::rgb(54, 54, 61),
            Some((1.0, Rgba::rgb(90, 90, 99))),
            Rgba::rgb(240, 240, 244),
        ),
        open: widget(
            Rgba::rgb(46, 46, 52),
            Rgba::rgb(46, 46, 52),
            Some((1.0, Rgba::rgb(70, 70, 78))),
            Rgba::rgb(240, 240, 244),
        ),
    };

    // Compact metrics: tight item spacing and a compact control size.
    theme.spacing.item_spacing = [6.0, 3.0];
    theme.spacing.button_padding = [8.0, 4.0];
    theme.spacing.interact_size = [24.0, 20.0];
    theme.spacing.window_margin = 8.0;
    theme.spacing.menu_margin = 6.0;
    theme.spacing.indent = 16.0;
    theme.spacing.slider_width = 120.0;
    theme.spacing.slider_rail_height = 4.0;
    theme.spacing.icon_width = 14.0;
    theme.spacing.icon_width_inner = 8.0;
    theme.spacing.icon_spacing = 6.0;
    theme.spacing.menu_width = 0.0;
    theme.spacing.tooltip_width = 0.0;
    theme.spacing.combo_width = 0.0;
    theme.spacing.combo_height = 220.0;
    theme.spacing.text_edit_width = 220.0;
    theme.spacing.menu_spacing = 2.0;
    theme.spacing.scroll_bar_width = 8.0;
    theme.spacing.scroll_floating = true;

    theme.text_styles = [
        ("small", 11.0, false),
        ("body", 12.5, false),
        ("button", 12.5, false),
        ("monospace", 12.0, true),
        ("heading", 14.0, false),
    ]
    .into_iter()
    .map(|(name, size, monospace)| {
        let family = if monospace {
            pill_ui_theme::FontFamilyName::Monospace
        } else {
            pill_ui_theme::FontFamilyName::Proportional
        };
        (name.to_owned(), pill_ui_theme::TextStyle::new(size, family))
    })
    .collect();
    theme
}

// =============================================================================
// The theme layers
// =============================================================================

/// Where one layer of the editor's theme came from.
///
/// The executable merges the layers in this order and the shell installs the
/// result; a launch stamps the generation so a shell that was reloaded knows
/// whether it still paints the theme the executable resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThemeLayer {
    /// The editor's own default, built in code.
    Code,
    /// The user's file, named by the `theme` preference.
    User,
    /// The project's `res/editor/` theme, named by `editor_settings.yaml`.
    Project,
}

impl ThemeLayer {
    /// The layer's name, as the logs and `dump theme` spell it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

impl std::fmt::Display for ThemeLayer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One layer of a resolved theme: its provenance and, for a file, its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeLayerInfo {
    /// Which layer this is.
    pub layer: ThemeLayer,
    /// The file the layer loaded, when it loaded one.
    pub path: Option<String>,
}

impl ThemeLayerInfo {
    /// The code default, which has no file.
    pub fn code() -> Self {
        Self {
            layer: ThemeLayer::Code,
            path: None,
        }
    }

    /// The user's file at `path`.
    pub fn user(path: impl Into<String>) -> Self {
        Self {
            layer: ThemeLayer::User,
            path: Some(path.into()),
        }
    }

    /// The project's file at `path`.
    pub fn project(path: impl Into<String>) -> Self {
        Self {
            layer: ThemeLayer::Project,
            path: Some(path.into()),
        }
    }

    /// The path this layer loaded, or `none` for the code default.
    pub fn path_text(&self) -> &str {
        self.path.as_deref().unwrap_or("none")
    }
}

/// The layer names joined as the logs and `dump theme` print them
/// (`code,user,project`).
pub fn layers_text(layers: &[ThemeLayerInfo]) -> String {
    layers
        .iter()
        .map(|info| info.layer.name())
        .collect::<Vec<_>>()
        .join(",")
}

/// What the user's `theme` preference selected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UserThemeSelection {
    /// The preference is absent or blank: no user layer.
    #[default]
    None,
    /// A bare theme name, as preferences written before theme files spell it
    /// (`dark`, `light`): kept, but adds no layer.
    Legacy(String),
    /// A theme file, absolute or relative to the editor's state directory.
    File(std::path::PathBuf),
}

impl UserThemeSelection {
    /// Read the preference's value.
    ///
    /// A value ending in `.json` or `.theme` is a file; anything else is a
    /// bare theme name, which adds no layer. A relative path resolves against
    /// `state_directory`, where the preferences file itself lives.
    pub fn from_preference(value: Option<&str>, state_directory: &std::path::Path) -> Self {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Self::None;
        };
        if value.ends_with(".json") || value.ends_with(".theme") {
            let path = std::path::Path::new(value);
            return Self::File(if path.is_absolute() {
                path.to_path_buf()
            } else {
                state_directory.join(path)
            });
        }
        Self::Legacy(value.to_string())
    }
}

/// The result of merging the layers over the code default.
#[derive(Clone, Debug)]
pub struct ThemeMerge {
    /// The merged theme.
    pub theme: Theme,
    /// The layers that fed [`ThemeMerge::theme`], in apply order; the code
    /// default is always first.
    pub layers: Vec<ThemeLayerInfo>,
    /// The layers a refused override dropped, with the reason.
    pub refused: Vec<(ThemeLayer, String)>,
}

/// Merge `user` and then `project` over `code`, refusing individually.
///
/// A layer the merge refuses is skipped and named in
/// [`ThemeMerge::refused`], so one bad item never costs the other layer's
/// values; the layers that did apply are what [`ThemeMerge::layers`] lists.
pub fn merge_layers(
    code: &Theme,
    user: Option<(&ThemeLayerInfo, &pill_ui_theme::ThemeOverrides)>,
    project: Option<(&ThemeLayerInfo, &pill_ui_theme::ThemeOverrides)>,
) -> ThemeMerge {
    let mut merge = ThemeMerge {
        theme: code.clone(),
        layers: vec![ThemeLayerInfo::code()],
        refused: Vec::new(),
    };
    for (info, overrides) in [user, project].into_iter().flatten() {
        match merge.theme.merged_with(overrides) {
            Ok(theme) => {
                merge.theme = theme;
                merge.layers.push(info.clone());
            }
            Err(error) => merge.refused.push((info.layer, error.to_string())),
        }
    }
    merge
}

/// One widget state's look: fills, an optional outline and a text colour.
fn widget(
    bg_fill: Rgba,
    weak_bg_fill: Rgba,
    outline: Option<(f32, Rgba)>,
    fg_fill: Rgba,
) -> WidgetStyle {
    WidgetStyle {
        bg_fill,
        weak_bg_fill,
        bg_stroke: match outline {
            Some((width, color)) => TokenStroke::new(width, color),
            None => TokenStroke::new(0.0, Rgba::with_alpha(0, 0, 0, 0)),
        },
        fg_stroke: TokenStroke::new(1.0, fg_fill),
        corner_radius: 3,
        expansion: 0.0,
    }
}

/// An egui colour from a theme colour (straight sRGB alpha).
fn color(value: Rgba) -> Color32 {
    let [red, green, blue, alpha] = value.0;
    Color32::from_rgba_unmultiplied(red, green, blue, alpha)
}

// =============================================================================
// Applying the theme
// =============================================================================

/// Install `theme` on `context` and record it as the editor's look.
///
/// The executable resolves the layers and the shell installs the merged theme
/// with the layers and the generation it came from; a context created later - a
/// pop-out window, the no-shell fallback - calls [`apply`] to install whatever
/// is recorded. The install goes through the UI kit with `append_fonts` as
/// its callback, so a theme layer's font leads each UI family, the editor's own
/// fonts follow it, and each icon family leads with its Phosphor face.
pub fn apply_theme(
    context: &Context,
    theme: &Theme,
    layers: &[ThemeLayerInfo],
    generation: u64,
    fonts: &kit::style::ThemeFonts,
) {
    let (contexts, first) = {
        let mut record = resolved()
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = record.theme != *theme
            || record.layers != layers
            || record.generation != generation
            || record.fonts != *fonts;
        if changed {
            record.theme = theme.clone();
            record.palette = Palette::from_theme(theme);
            record.layers = layers.to_vec();
            record.generation = generation;
            record.fonts = fonts.clone();
        }
        record.contexts += 1;
        let first = !record.installed || changed;
        record.installed = true;
        (record.contexts, first)
    };
    // One line per resolved theme, not per context: a pop-out that registers
    // the same theme joins the count without logging again.
    if first {
        let user = layers
            .iter()
            .find(|info| info.layer == ThemeLayer::User)
            .map_or("none", ThemeLayerInfo::path_text);
        let project = layers
            .iter()
            .find(|info| info.layer == ThemeLayer::Project)
            .map_or("none", ThemeLayerInfo::path_text);
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            layers = %layers_text(layers),
            user = %user,
            project = %project,
            generation,
            contexts,
            accent = %theme.palette.accent.hex(),
            corner = theme.shape.corner_radius,
            "[editor] theme applied"
        );
    }
    // The signature is fresh per context: fonts belong to the context, so a
    // pop-out always gets them even when the main one already had them.
    let mut signature = kit::style::FontSignature::default();
    let fonts_installed = kit::style::install(context, theme, fonts, append_fonts, &mut signature);
    if fonts_installed {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            signature = %signature,
            "[editor] fonts installed"
        );
    }
}

/// Append the editor's own families to `fonts`: Inter for the UI, JetBrains
/// Mono for code and numbers, then the icon families. Phosphor joins no UI
/// family, because its regular font claims the lowercase Latin range with
/// empty glyphs.
pub(crate) fn append_fonts(fonts: &mut egui::FontDefinitions) {
    fonts.font_data.insert(
        BODY_KEY.to_owned(),
        Arc::new(egui::FontData::from_static(BODY_FONT)),
    );
    fonts.font_data.insert(
        MONOSPACE_KEY.to_owned(),
        Arc::new(egui::FontData::from_static(MONOSPACE_FONT)),
    );
    for (family, key) in [
        (egui::FontFamily::Proportional, BODY_KEY),
        (egui::FontFamily::Monospace, MONOSPACE_KEY),
    ] {
        let names = fonts.families.entry(family).or_default();
        // A theme layer's font is what the layer asked for; Inter and
        // JetBrains Mono are the fallback the editor ships, so they go behind
        // it and ahead of egui's defaults.
        let index = names
            .iter()
            .take_while(|name| kit::style::is_theme_font(name))
            .count();
        names.insert(index, key.to_owned());
    }
    crate::icons::append_font_definitions(fonts);
}

/// Install the theme the editor last resolved on `context`.
///
/// A pop-out window and the no-shell fallback call this, so every context
/// paints with the same tokens as the main one.
pub fn apply(context: &Context) {
    let (theme, layers, generation, fonts) = read_resolved(|resolved| {
        (
            resolved.theme.clone(),
            resolved.layers.clone(),
            resolved.generation,
            resolved.fonts.clone(),
        )
    });
    apply_theme(context, &theme, &layers, generation, &fonts);
}

// =============================================================================
// The dock's style
// =============================================================================

/// The `egui_dock` styling for the shell's `DockArea`, derived from
/// `egui_style` and overridden to the editor's look: flat tabs that merge
/// with their active bodies, a dark tab strip, and thin separators between
/// panels. Colours come from `palette`; only an inactive tab's fill is
/// transparent.
pub fn dock_style(egui_style: &egui::Style, palette: &Palette) -> egui_dock::Style {
    let mut style = egui_dock::Style::from_egui(egui_style);
    style.main_surface_border_stroke = Stroke::NONE;
    style.dock_area_padding = None;
    // The tab strip: a darker band; the active tab takes the body's fill and
    // carries the accent underline, so it reads as one surface with its
    // panel. The strip's own line under the tabs is the neutral separator:
    // the accent underline is the selected tab's alone and the shell paints
    // it, because the dock uses one line colour for every tab.
    style.tab_bar.bg_fill = palette.frame;
    style.tab_bar.height = 26.0;
    style.tab_bar.corner_radius = CornerRadius::ZERO;
    style.tab_bar.hline_color = palette.line;
    style.tab_bar.inner_margin = egui::Margin::symmetric(6, 3);
    style.tab.active = tab_faces(palette.panel, palette.text_strong);
    style.tab.focused = tab_faces(palette.panel, palette.text_strong);
    style.tab.active_with_kb_focus = tab_faces(palette.panel, palette.text_strong);
    style.tab.inactive = tab_faces(Color32::TRANSPARENT, palette.text_muted);
    style.tab.inactive_with_kb_focus = tab_faces(Color32::TRANSPARENT, palette.text_muted);
    style.tab.hovered = tab_faces(palette.panel_raised, palette.text);
    style.tab.focused_with_kb_focus = tab_faces(palette.panel_raised, palette.text);
    // The active tab merges with its body: the shell paints the accent
    // underline there itself, so the dock must not run the neutral line
    // under it.
    style.tab.hline_below_active_tab_name = false;
    style.tab.spacing = 0.0;
    style.tab.tab_body.bg_fill = palette.panel;
    style.tab.tab_body.inner_margin = egui::Margin::same(6);
    style.tab.tab_body.corner_radius = CornerRadius::ZERO;
    style.tab.tab_body.stroke = Stroke::NONE;
    // Panel boundaries: one thin line, with a grab band several times its
    // width so the splitter is easy to catch, brightened while
    // interacted with - the line stepped towards the text colours rather than
    // a second pair of literals.
    style.separator.width = 1.0;
    style.separator.extra_interact_width = 8.0;
    style.separator.color_idle = palette.line;
    style.separator.color_hovered = palette.line.lerp_to_gamma(palette.text_muted, 0.44);
    style.separator.color_dragged = palette.line.lerp_to_gamma(palette.text, 0.47);
    // The drag-and-drop preview uses the selection tint.
    style.overlay.selection_color = palette.selection;
    // Icon-ish buttons: muted, brightening on hover.
    let buttons = &mut style.buttons;
    buttons.add_tab_align = egui_dock::TabAddAlign::Left;
    for (color, active) in [
        (
            &mut buttons.close_tab_color,
            &mut buttons.close_tab_active_color,
        ),
        (
            &mut buttons.add_tab_color,
            &mut buttons.add_tab_active_color,
        ),
        (
            &mut buttons.close_all_tabs_color,
            &mut buttons.close_all_tabs_active_color,
        ),
        (
            &mut buttons.collapse_tabs_color,
            &mut buttons.collapse_tabs_active_color,
        ),
        (
            &mut buttons.minimize_window_color,
            &mut buttons.minimize_window_active_color,
        ),
    ] {
        *color = palette.text_muted;
        *active = palette.text_strong;
    }
    style
}

/// One `egui_dock` tab face: square corners and no outline, so the fill
/// alone separates the states.
fn tab_faces(bg_fill: Color32, text_color: Color32) -> egui_dock::TabInteractionStyle {
    egui_dock::TabInteractionStyle {
        bg_fill,
        text_color,
        corner_radius: CornerRadius::ZERO,
        outline_color: Color32::TRANSPARENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record is process-wide, so the tests that install into it take
    /// turns: a parallel test's install would otherwise move the state the
    /// one below is asserting on.
    static RECORD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn record() -> std::sync::MutexGuard<'static, ()> {
        RECORD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A fresh context with the code default installed.
    fn themed() -> Context {
        let context = Context::default();
        apply_theme(
            &context,
            &default_theme(),
            &[ThemeLayerInfo::code()],
            1,
            &kit::style::ThemeFonts::default(),
        );
        context
    }

    #[test]
    fn the_installed_style_carries_the_theme_tokens() {
        let _record = record();
        let context = themed();
        let theme = default_theme();
        let style = context.style_of(egui::Theme::Dark);
        let visuals = &style.visuals;
        assert_eq!(visuals.panel_fill, color(theme.palette.panel));
        assert_eq!(visuals.window_fill, color(theme.palette.panel));
        assert_eq!(visuals.window_stroke.width, 1.0);
        assert_eq!(visuals.window_stroke.color, color(theme.palette.line));
        assert_eq!(
            visuals.window_corner_radius,
            CornerRadius::same(theme.shape.window_corner_radius)
        );
        assert_eq!(visuals.extreme_bg_color, color(theme.palette.well));
        assert_eq!(visuals.faint_bg_color, color(theme.palette.panel_raised));
        assert_eq!(visuals.selection.bg_fill, color(theme.palette.selection));
        assert_eq!(
            visuals.selection.stroke.color,
            color(theme.shape.selection_stroke.color)
        );
        assert_eq!(visuals.hyperlink_color, color(theme.palette.accent_hot));
        assert!(visuals.slider_trailing_fill);
        assert_eq!(
            visuals.window_shadow.offset,
            theme.shape.window_shadow.offset
        );
        assert_eq!(visuals.window_shadow.blur, theme.shape.window_shadow.blur);
    }

    #[test]
    fn the_installed_style_carries_the_widget_states() {
        let _record = record();
        let context = themed();
        let theme = default_theme();
        let style = context.style_of(egui::Theme::Dark);
        let widgets = &style.visuals.widgets;
        assert_eq!(
            widgets.noninteractive.bg_fill,
            color(theme.widgets.noninteractive.bg_fill)
        );
        assert_eq!(
            widgets.noninteractive.fg_stroke.color,
            color(theme.widgets.noninteractive.fg_stroke.color)
        );
        assert_eq!(
            widgets.inactive.bg_fill,
            color(theme.widgets.inactive.bg_fill)
        );
        assert_eq!(
            widgets.inactive.bg_stroke.width,
            theme.widgets.inactive.bg_stroke.width
        );
        assert_eq!(
            widgets.hovered.bg_fill,
            color(theme.widgets.hovered.bg_fill)
        );
        assert_eq!(
            widgets.hovered.bg_stroke.color,
            color(theme.widgets.hovered.bg_stroke.color)
        );
        assert_eq!(widgets.active.bg_fill, color(theme.widgets.active.bg_fill));
        assert_eq!(
            widgets.active.bg_stroke.color,
            color(theme.widgets.active.bg_stroke.color)
        );
        assert_eq!(widgets.open.bg_fill, color(theme.widgets.open.bg_fill));
        for state in [
            &widgets.noninteractive,
            &widgets.inactive,
            &widgets.hovered,
            &widgets.active,
            &widgets.open,
        ] {
            assert_eq!(
                state.corner_radius,
                CornerRadius::same(theme.widgets.inactive.corner_radius)
            );
        }
    }

    #[test]
    fn the_installed_style_carries_the_metrics_and_text() {
        let _record = record();
        let context = themed();
        let theme = default_theme();
        let style = context.style_of(egui::Theme::Dark);
        assert_eq!(
            style.spacing.item_spacing,
            egui::vec2(theme.spacing.item_spacing[0], theme.spacing.item_spacing[1])
        );
        assert_eq!(
            style.spacing.button_padding,
            egui::vec2(
                theme.spacing.button_padding[0],
                theme.spacing.button_padding[1]
            )
        );
        assert_eq!(
            style.spacing.interact_size,
            egui::vec2(
                theme.spacing.interact_size[0],
                theme.spacing.interact_size[1]
            )
        );
        assert_eq!(style.spacing.scroll.floating, theme.spacing.scroll_floating);
        assert_eq!(
            style.spacing.scroll.bar_width,
            theme.spacing.scroll_bar_width
        );
        for (name, key) in [
            ("body", egui::TextStyle::Body),
            ("monospace", egui::TextStyle::Monospace),
            ("heading", egui::TextStyle::Heading),
            ("small", egui::TextStyle::Small),
        ] {
            let token = &theme.text_styles[name];
            assert_eq!(style.text_styles[&key].size, token.size, "{name}");
        }
        assert_eq!(context.theme(), egui::Theme::Dark);
    }

    /// The default theme's signature values: a crimson accent on near-black
    /// chrome, with tight corners.
    #[test]
    fn the_default_theme_samples_the_reference() {
        let theme = default_theme();
        assert_eq!(theme.palette.accent, Rgba::rgb(200, 53, 63));
        assert_eq!(theme.palette.selection_bar, theme.palette.accent);
        assert!(theme.palette.frame.0[0] < 24, "the frame is near-black");
        assert_eq!(theme.shape.corner_radius, 3);
    }

    #[test]
    fn the_recorded_palette_is_the_installed_theme() {
        let _record = record();
        let context = Context::default();
        let theme = default_theme();
        apply_theme(
            &context,
            &theme,
            &[ThemeLayerInfo::code()],
            1,
            &kit::style::ThemeFonts::default(),
        );
        let palette = palette();
        assert_eq!(palette.panel, color(theme.palette.panel));
        assert_eq!(text(), color(theme.palette.text));
        assert_eq!(text_muted(), color(theme.palette.text_muted));
        assert_eq!(accent(), color(theme.palette.accent));
        assert_eq!(overlay(), color(theme.palette.overlay));
        assert_eq!(shadow(), color(theme.shape.window_shadow.color));
        assert_eq!(log_warn(), color(theme.palette.log_warn));
    }

    /// The preference's value: a file (absolute or beside the state files), a
    /// bare theme name, or nothing.
    #[test]
    fn the_user_layer_reads_a_preference_value() {
        let directory = std::path::Path::new("C:/state");
        assert_eq!(
            UserThemeSelection::from_preference(None, directory),
            UserThemeSelection::None
        );
        assert_eq!(
            UserThemeSelection::from_preference(Some("   "), directory),
            UserThemeSelection::None
        );
        assert_eq!(
            UserThemeSelection::from_preference(Some("dark"), directory),
            UserThemeSelection::Legacy("dark".to_string())
        );
        assert_eq!(
            UserThemeSelection::from_preference(Some("theme.json"), directory),
            UserThemeSelection::File(directory.join("theme.json"))
        );
        assert_eq!(
            UserThemeSelection::from_preference(Some("mine.theme"), directory),
            UserThemeSelection::File(directory.join("mine.theme"))
        );
        assert_eq!(
            UserThemeSelection::from_preference(Some("C:/elsewhere/mine.theme"), directory),
            UserThemeSelection::File(std::path::PathBuf::from("C:/elsewhere/mine.theme"))
        );
    }

    /// The layers fold in order - user first, project last - so the project's
    /// value wins where both name the same item and the user's survives where
    /// only it spoke.
    #[test]
    fn the_layers_merge_user_then_project() {
        let code = default_theme();
        let user = pill_ui_theme::ThemeOverrides::from_json_str(
            "{\"palette\": {\"accent\": \"#ff488c\", \"panel\": \"#101820\"}}",
        )
        .expect("user document");
        let project = pill_ui_theme::ThemeOverrides::from_json_str(
            "{\"palette\": {\"accent\": \"#33d17a\"}}",
        )
        .expect("project document");
        let user_info = ThemeLayerInfo::user("theme.json");
        let project_info = ThemeLayerInfo::project("editor/editor.theme");

        let merged = merge_layers(
            &code,
            Some((&user_info, &user)),
            Some((&project_info, &project)),
        );

        assert_eq!(merged.theme.palette.accent, Rgba::parse("#33d17a").unwrap());
        assert_eq!(merged.theme.palette.panel, Rgba::parse("#101820").unwrap());
        assert!(merged.refused.is_empty());
        assert_eq!(
            merged.layers,
            vec![
                ThemeLayerInfo::code(),
                ThemeLayerInfo::user("theme.json"),
                ThemeLayerInfo::project("editor/editor.theme"),
            ]
        );
        assert_eq!(layers_text(&merged.layers), "code,user,project");
    }

    /// A layer whose document the merge refuses is named and skipped: the
    /// other layer still applies, and the code default it fell back to is
    /// still there.
    #[test]
    fn a_refused_layer_is_skipped_and_named() {
        let code = default_theme();
        let user = pill_ui_theme::ThemeOverrides::from_json_str(
            "{\"palette\": {\"accent\": \"not-a-colour\"}}",
        )
        .expect("the document parses; the colour does not");
        let project = pill_ui_theme::ThemeOverrides::from_json_str(
            "{\"palette\": {\"accent\": \"#33d17a\"}}",
        )
        .expect("project document");
        let user_info = ThemeLayerInfo::user("broken.theme");
        let project_info = ThemeLayerInfo::project("editor/editor.theme");

        let merged = merge_layers(
            &code,
            Some((&user_info, &user)),
            Some((&project_info, &project)),
        );

        assert_eq!(merged.theme.palette.accent, Rgba::parse("#33d17a").unwrap());
        assert_eq!(merged.refused.len(), 1);
        assert_eq!(merged.refused[0].0, ThemeLayer::User);
        assert!(merged.refused[0].1.contains("palette.accent"));
        assert_eq!(merged.layers.len(), 2);
    }

    /// The record follows the layer set: a new generation with the same tokens
    /// is still a theme to install, and `apply` re-installs what is recorded.
    #[test]
    fn the_record_follows_the_layer_set() {
        let _record = record();
        let context = Context::default();
        let theme = default_theme();
        let layers = vec![ThemeLayerInfo::code(), ThemeLayerInfo::user("theme.json")];
        apply_theme(
            &context,
            &theme,
            &layers,
            7,
            &kit::style::ThemeFonts::default(),
        );
        let recorded = read_resolved(|resolved| (resolved.generation, resolved.layers.clone()));
        assert_eq!(recorded.0, 7);
        assert_eq!(recorded.1, layers);
        // A second context installs the same record without moving it.
        let popout = Context::default();
        apply(&popout);
        let after = read_resolved(|resolved| resolved.generation);
        assert_eq!(after, 7);
    }

    #[test]
    fn the_dock_style_reads_the_palette_roles() {
        let palette = Palette::from_theme(&default_theme());
        let dock = dock_style(&egui::Style::default(), &palette);
        assert_eq!(dock.tab_bar.bg_fill, palette.frame);
        assert_eq!(dock.tab_bar.hline_color, palette.line);
        assert!(
            !dock.tab.hline_below_active_tab_name,
            "the shell paints the active tab's accent underline itself"
        );
        assert_eq!(dock.tab.active.bg_fill, palette.panel);
        assert_eq!(dock.tab.active.text_color, palette.text_strong);
        assert_eq!(dock.tab.inactive.text_color, palette.text_muted);
        assert_eq!(dock.tab.hovered.bg_fill, palette.panel_raised);
        assert_eq!(dock.tab.tab_body.bg_fill, palette.panel);
        assert_eq!(dock.separator.color_idle, palette.line);
        assert_ne!(dock.separator.color_hovered, palette.line);
    }

    #[test]
    fn the_built_in_families_are_inter_jetbrains_mono_then_phosphor() {
        let definitions =
            kit::style::font_definitions(&kit::style::ThemeFonts::default(), append_fonts);
        let proportional = &definitions.families[&egui::FontFamily::Proportional];
        assert_eq!(
            proportional.first().map(String::as_str),
            Some(BODY_KEY),
            "Inter leads the proportional family"
        );
        let monospace = &definitions.families[&egui::FontFamily::Monospace];
        assert_eq!(
            monospace.first().map(String::as_str),
            Some(MONOSPACE_KEY),
            "JetBrains Mono leads the monospace family"
        );
        let duotone =
            &definitions.families[&egui::FontFamily::Name(crate::icons::DUOTONE_FAMILY.into())];
        assert_eq!(
            duotone.first().map(String::as_str),
            Some(crate::icons::DUOTONE_KEY),
            "Phosphor's duotone face leads its own family"
        );
        assert!(
            duotone.iter().any(|name| name == BODY_KEY),
            "Inter follows it as the label's fallback"
        );
    }

    /// A theme layer's font leads the UI families, the editor's own fonts
    /// follow it as the fallback, and an icon family leads with its Phosphor
    /// face - the UI fonts claim the icons' private-use codepoints with glyphs
    /// of their own, so an icon listed behind them would draw as a letter.
    #[test]
    fn a_theme_font_leads_the_editors_own_families() {
        let fonts = kit::style::ThemeFonts {
            body: Some(vec![0, 1, 2, 3]),
            monospace: Some(vec![4, 5, 6, 7]),
            ..kit::style::ThemeFonts::default()
        };
        let definitions = kit::style::font_definitions(&fonts, append_fonts);

        let proportional = &definitions.families[&egui::FontFamily::Proportional];
        assert_eq!(
            proportional.first().map(String::as_str),
            Some(kit::style::THEME_BODY_KEY),
            "the theme's body font leads"
        );
        assert_eq!(
            proportional.get(1).map(String::as_str),
            Some(BODY_KEY),
            "Inter is the fallback behind it"
        );
        let monospace = &definitions.families[&egui::FontFamily::Monospace];
        assert_eq!(
            monospace.first().map(String::as_str),
            Some(kit::style::THEME_MONOSPACE_KEY)
        );
        assert_eq!(monospace.get(1).map(String::as_str), Some(MONOSPACE_KEY));
        let icons = &definitions.families[&egui::FontFamily::Name(crate::icons::FAMILY.into())];
        assert_eq!(
            icons.first().map(String::as_str),
            Some(crate::icons::REGULAR_KEY),
            "Phosphor's regular face leads the icon family"
        );
        assert_eq!(
            icons.get(1).map(String::as_str),
            Some(kit::style::THEME_BODY_KEY),
            "the theme's body font follows it as the fallback"
        );
    }
}

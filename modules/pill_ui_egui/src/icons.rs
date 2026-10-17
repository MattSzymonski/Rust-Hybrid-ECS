//! Phosphor's icons for the editor's chrome: the regular weight inline and
//! duotone icons as two painted layers.
//!
//! # Responsibilities
//!
//! - [`install`]: register the vendored Phosphor fonts as families on an
//!   egui context; [`crate::style::apply`] calls it with the theme.
//! - [`Icon`]: the icons the editor uses, as the fonts' codepoints.
//! - [`rich`]: an icon and a label as one text run, for tab titles and the
//!   widgets that only take text.
//! - [`paint_duotone`] and [`IconButton`]: Phosphor's duotone form - a fill
//!   under the line art - for the editor's buttons, menus and tool rows.
//!
//! # Design
//!
//! The vendored files are the `@phosphor-icons/web` package's
//! `Phosphor.ttf` (regular) and `Phosphor-Duotone.ttf`. The duotone font
//! carries every icon twice: its fill shape at the icon's codepoint and its
//! line art one codepoint further - the pairing the package's stylesheet
//! overlays with `:before`/`:after`, the fill at a fifth of the opacity.
//! Painting both glyphs from one origin at [`DUOTONE_FILL_ALPHA`] gives the
//! duotone look with no image atlas: the icons tint with the theme,
//! rasterize at any size, and cost no texture memory.
//!
//! Both families list the context's proportional fonts first and Phosphor
//! after them: Phosphor's own character map also claims the lowercase
//! Latin range (with empty glyphs), so a letter must always be answered by
//! the UI font, and only an icon's private-use codepoint reaches Phosphor.

// Standard library
use std::sync::Arc;

// External crates
use egui::{
    Color32, Context, FontData, FontDefinitions, FontFamily, FontId, Painter, Rect, Response,
    Sense, StrokeKind, TextStyle, Ui, Vec2, Widget,
};

/// The family of the regular-weight icons, falling back to the UI font.
pub const FAMILY: &str = "phosphor";

/// The family of the duotone icons, falling back to the UI font.
pub const DUOTONE_FAMILY: &str = "phosphor.duotone";

/// How strongly a duotone icon's fill shows under its line art.
pub const DUOTONE_FILL_ALPHA: f32 = 0.2;

/// An icon's height relative to the button text it sits beside.
const ICON_TEXT_RATIO: f32 = 1.15;

/// The gap between an icon and its label, in points.
const ICON_LABEL_GAP: f32 = 6.0;

/// How much the hovered fill of a row floating over a scene brightens.
const HOVER_BRIGHTEN: u8 = 14;

/// The regular-weight font, as the package shipped it.
const REGULAR_FONT: &[u8] = include_bytes!("../assets/Phosphor.ttf");

/// The duotone font, as the package shipped it.
const DUOTONE_FONT: &[u8] = include_bytes!("../assets/Phosphor-Duotone.ttf");

/// The font definitions' keys for the two files.
const REGULAR_KEY: &str = "phosphor";
const DUOTONE_KEY: &str = "phosphor.duotone";

/// Register the Phosphor fonts on `context` as [`FAMILY`] and
/// [`DUOTONE_FAMILY`], each falling back to the context's proportional
/// fonts so a label beside an icon still draws.
pub fn install(context: &Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        REGULAR_KEY.to_owned(),
        Arc::new(FontData::from_static(REGULAR_FONT)),
    );
    fonts.font_data.insert(
        DUOTONE_KEY.to_owned(),
        Arc::new(FontData::from_static(DUOTONE_FONT)),
    );
    let fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    // Phosphor goes LAST: its character map claims the lowercase Latin range
    // (with empty glyphs), so a letter in a mixed run has to be answered by
    // the UI fonts first, and only an icon's codepoint reaches Phosphor.
    let family = |icon_font: &str| {
        let mut names = fallbacks.clone();
        names.push(icon_font.to_owned());
        names
    };
    fonts
        .families
        .insert(FontFamily::Name(FAMILY.into()), family(REGULAR_KEY));
    fonts
        .families
        .insert(FontFamily::Name(DUOTONE_FAMILY.into()), family(DUOTONE_KEY));
    context.set_fonts(fonts);
}

/// One Phosphor icon, named the way the package names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    /// A solid cube: the first scene view.
    Cube,
    /// A wireframe cube: the second scene view.
    CubeTransparent,
    /// A display with a play mark: the project's own view.
    MonitorPlay,
    /// A hierarchy of nodes: the entity list.
    TreeStructure,
    /// A cog: the system list.
    Gear,
    /// Three sliders: the selected entity's detail.
    SlidersHorizontal,
    /// An open folder: the asset browser.
    FolderOpen,
    /// The fader bank: the preferences.
    Faders,
    /// Arrows out of a centre: the translate tool.
    ArrowsOutCardinal,
    /// The clockwise arc: the rotate tool.
    ArrowsClockwise,
    /// Arrows out of a box: the scale tool.
    ArrowsOut,
    /// A globe: world space.
    GlobeHemisphereWest,
    /// A map pin: local space.
    MapPin,
    /// A horseshoe magnet: snapping.
    Magnet,
    /// A four-cell grid: the scene grid.
    GridFour,
    /// A plus: create.
    Plus,
    /// A bin: delete.
    Trash,
    /// A pencil: rename.
    PencilSimple,
    /// A file with a down arrow: write the asset's metadata.
    FileArrowDown,
    /// The power symbol: a system's on/off switch.
    Power,
    /// Two bars: pause the project.
    Pause,
    /// A triangle: resume the project.
    Play,
    /// A bar and a triangle: advance one frame.
    SkipForward,
    /// A circled plus: add an entity or a component.
    PlusCircle,
    /// A circled minus: remove a component.
    MinusCircle,
    /// A circled tick: confirm a dialog.
    CheckCircle,
    /// A circled cross: cancel a dialog.
    XCircle,
    /// A floppy disk: save.
    FloppyDisk,
    /// A box with an arrow: move a tab back into the dock.
    ArrowSquareOut,
}

impl Icon {
    /// Every icon, in declaration order.
    pub const ALL: [Icon; 29] = [
        Icon::Cube,
        Icon::CubeTransparent,
        Icon::MonitorPlay,
        Icon::TreeStructure,
        Icon::Gear,
        Icon::SlidersHorizontal,
        Icon::FolderOpen,
        Icon::Faders,
        Icon::ArrowsOutCardinal,
        Icon::ArrowsClockwise,
        Icon::ArrowsOut,
        Icon::GlobeHemisphereWest,
        Icon::MapPin,
        Icon::Magnet,
        Icon::GridFour,
        Icon::Plus,
        Icon::Trash,
        Icon::PencilSimple,
        Icon::FileArrowDown,
        Icon::Power,
        Icon::Pause,
        Icon::Play,
        Icon::SkipForward,
        Icon::PlusCircle,
        Icon::MinusCircle,
        Icon::CheckCircle,
        Icon::XCircle,
        Icon::FloppyDisk,
        Icon::ArrowSquareOut,
    ];

    /// The icon's codepoint in the vendored fonts: the regular font draws
    /// its line art there, and the duotone font its fill.
    pub fn codepoint(self) -> u32 {
        match self {
            Icon::Cube => 0xe1da,
            Icon::CubeTransparent => 0xec7c,
            Icon::MonitorPlay => 0xe58c,
            Icon::TreeStructure => 0xe67c,
            Icon::Gear => 0xe270,
            Icon::SlidersHorizontal => 0xe434,
            Icon::FolderOpen => 0xe256,
            Icon::Faders => 0xe228,
            Icon::ArrowsOutCardinal => 0xe0a4,
            Icon::ArrowsClockwise => 0xe094,
            Icon::ArrowsOut => 0xe0a2,
            Icon::GlobeHemisphereWest => 0xe28c,
            Icon::MapPin => 0xe316,
            Icon::Magnet => 0xe680,
            Icon::GridFour => 0xe296,
            Icon::Plus => 0xe3d4,
            Icon::Trash => 0xe4a6,
            Icon::PencilSimple => 0xe3b4,
            Icon::FileArrowDown => 0xe232,
            Icon::Power => 0xe3da,
            Icon::Pause => 0xe39e,
            Icon::Play => 0xe3d0,
            Icon::SkipForward => 0xe5a6,
            Icon::PlusCircle => 0xe3d6,
            Icon::MinusCircle => 0xe32c,
            Icon::CheckCircle => 0xe184,
            Icon::XCircle => 0xe4f8,
            Icon::FloppyDisk => 0xe248,
            Icon::ArrowSquareOut => 0xe5de,
        }
    }

    /// The icon's Phosphor name, as the package spells it.
    pub fn name(self) -> &'static str {
        match self {
            Icon::Cube => "cube",
            Icon::CubeTransparent => "cube-transparent",
            Icon::MonitorPlay => "monitor-play",
            Icon::TreeStructure => "tree-structure",
            Icon::Gear => "gear",
            Icon::SlidersHorizontal => "sliders-horizontal",
            Icon::FolderOpen => "folder-open",
            Icon::Faders => "faders",
            Icon::ArrowsOutCardinal => "arrows-out-cardinal",
            Icon::ArrowsClockwise => "arrows-clockwise",
            Icon::ArrowsOut => "arrows-out",
            Icon::GlobeHemisphereWest => "globe-hemisphere-west",
            Icon::MapPin => "map-pin",
            Icon::Magnet => "magnet",
            Icon::GridFour => "grid-four",
            Icon::Plus => "plus",
            Icon::Trash => "trash",
            Icon::PencilSimple => "pencil-simple",
            Icon::FileArrowDown => "file-arrow-down",
            Icon::Power => "power",
            Icon::Pause => "pause",
            Icon::Play => "play",
            Icon::SkipForward => "skip-forward",
            Icon::PlusCircle => "plus-circle",
            Icon::MinusCircle => "minus-circle",
            Icon::CheckCircle => "check-circle",
            Icon::XCircle => "x-circle",
            Icon::FloppyDisk => "floppy-disk",
            Icon::ArrowSquareOut => "arrow-square-out",
        }
    }

    /// The glyph the regular font draws for the icon.
    pub fn glyph(self) -> char {
        glyph(self.codepoint())
    }

    /// The glyphs the duotone font draws for the icon: its fill shape, and
    /// the line art one codepoint further.
    pub fn duotone(self) -> (char, char) {
        (glyph(self.codepoint()), glyph(self.codepoint() + 1))
    }
}

/// The glyph at `codepoint`, which the vendored fonts always carry.
fn glyph(codepoint: u32) -> char {
    char::from_u32(codepoint).expect("a Phosphor codepoint is a scalar value")
}

/// `icon` and `label` as one text run: the icon from the regular font and
/// the letters from the UI font the family falls back to.
pub fn rich(icon: Icon, label: &str) -> egui::RichText {
    egui::RichText::new(format!("{}  {label}", icon.glyph()))
        .family(FontFamily::Name(FAMILY.into()))
}

/// `color` at `factor` of its opacity.
pub fn with_alpha(color: Color32, factor: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (color.a() as f32 * factor.clamp(0.0, 1.0)) as u8,
    )
}

/// Paint `icon`'s duotone form centred in `rect`, tinted `color`: the fill
/// shape at [`DUOTONE_FILL_ALPHA`] with the line art over it.
pub fn paint_duotone(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let size = rect.width().min(rect.height());
    if size <= 0.0 {
        return;
    }
    let font = FontId::new(size, FontFamily::Name(DUOTONE_FAMILY.into()));
    let (fill, line) = icon.duotone();
    // The line art's layout box anchors both layers: the two glyphs share a
    // baseline, so drawing them from one origin keeps the fill exactly under
    // the line whatever their ink extents are.
    let line_galley = painter.layout_no_wrap(line.to_string(), font.clone(), color);
    let origin = rect.center() - line_galley.size() * 0.5;
    let faded = with_alpha(color, DUOTONE_FILL_ALPHA);
    let fill_galley = painter.layout_no_wrap(fill.to_string(), font, faded);
    painter.galley(origin, fill_galley, faded);
    painter.galley(origin, line_galley, color);
}

/// A button carrying a duotone icon and a label: the one button the egui
/// editor's toolbars, menus and floating tool rows draw.
///
/// It wears the theme's button visuals - the hover and press feedback, the
/// selected fill a current choice takes - and lays the icon out before the
/// label. A row whose rectangle a panel has already fixed takes exactly that
/// rectangle through [`IconButton::fixed_size`].
pub struct IconButton<'a> {
    icon: Icon,
    label: &'a str,
    active: bool,
    fill: Option<Color32>,
    size: Option<Vec2>,
}

impl<'a> IconButton<'a> {
    /// A button for `icon` and `label`; an empty label leaves the icon
    /// alone.
    pub fn new(icon: Icon, label: &'a str) -> Self {
        Self {
            icon,
            label,
            active: false,
            fill: None,
            size: None,
        }
    }

    /// Draw the button as the current choice (a checked menu item, a tool
    /// that is on).
    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    /// Use `fill` instead of the theme's button fill: a panel floating over
    /// the scene keeps its own palette.
    pub fn fill(mut self, fill: Color32) -> Self {
        self.fill = Some(fill);
        self
    }

    /// Take exactly `size` rather than growing to the icon and label.
    pub fn fixed_size(mut self, size: Vec2) -> Self {
        self.size = Some(size);
        self
    }
}

impl Widget for IconButton<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let style = ui.style();
        let font = style
            .text_styles
            .get(&TextStyle::Button)
            .cloned()
            .unwrap_or_else(|| FontId::proportional(13.0));
        let padding = style.spacing.button_padding;
        let color = style.visuals.text_color();
        let icon_size = font.size * ICON_TEXT_RATIO;
        let label_galley = ui
            .painter()
            .layout_no_wrap(self.label.to_owned(), font, color);
        let gap = if self.label.is_empty() {
            0.0
        } else {
            ICON_LABEL_GAP
        };
        let content = Vec2::new(
            icon_size + gap + label_galley.size().x,
            label_galley.size().y.max(icon_size),
        );
        let size = self.size.unwrap_or(content + padding * 2.0);
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        if ui.is_rect_visible(rect) {
            let visuals = ui.style().interact_selectable(&response, self.active);
            let fill = self.fill.map_or(visuals.weak_bg_fill, |fill| {
                if response.hovered() {
                    brighten(fill, HOVER_BRIGHTEN)
                } else {
                    fill
                }
            });
            let corner = ui.style().visuals.widgets.inactive.corner_radius;
            ui.painter()
                .rect(rect, corner, fill, visuals.bg_stroke, StrokeKind::Inside);
            let icon_rect = Rect::from_center_size(
                egui::pos2(
                    rect.center().x - content.x * 0.5 + icon_size * 0.5,
                    rect.center().y,
                ),
                Vec2::splat(icon_size),
            );
            paint_duotone(ui.painter(), icon_rect, self.icon, visuals.fg_stroke.color);
            if !self.label.is_empty() {
                ui.painter().galley(
                    egui::pos2(
                        icon_rect.right() + gap,
                        rect.center().y - label_galley.size().y * 0.5,
                    ),
                    label_galley,
                    visuals.fg_stroke.color,
                );
            }
        }
        response
    }
}

/// `color` lifted `amount` towards white on every channel.
fn brighten(color: Color32, amount: u8) -> Color32 {
    let lift = |channel: u8| channel.saturating_add(amount);
    Color32::from_rgba_unmultiplied(lift(color.r()), lift(color.g()), lift(color.b()), color.a())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon's codepoint, and the duotone font's line art one further,
    /// are in both vendored fonts' character maps: a typo or a package
    /// change fails here instead of drawing a blank.
    #[test]
    fn every_icon_is_in_the_vendored_fonts() {
        let regular = codepoints(REGULAR_FONT);
        let duotone = codepoints(DUOTONE_FONT);
        for icon in Icon::ALL {
            assert!(
                regular.contains(&icon.codepoint()),
                "the regular font has no glyph for {} (U+{:04X})",
                icon.name(),
                icon.codepoint()
            );
            assert!(
                duotone.contains(&icon.codepoint()),
                "the duotone font has no fill glyph for {} (U+{:04X})",
                icon.name(),
                icon.codepoint()
            );
            assert!(
                duotone.contains(&(icon.codepoint() + 1)),
                "the duotone font has no line glyph for {} (U+{:04X})",
                icon.name(),
                icon.codepoint() + 1
            );
        }
    }

    /// The inline form puts the icon's glyph first and keeps the label
    /// after it, whatever the icon.
    #[test]
    fn a_rich_label_starts_with_its_icon() {
        let grid = rich(Icon::GridFour, "Grid");
        assert!(grid.text().starts_with(Icon::GridFour.glyph()));
        assert!(grid.text().ends_with("Grid"));
        let magnet = rich(Icon::Magnet, "Grid");
        assert!(magnet.text().starts_with(Icon::Magnet.glyph()));
        assert_ne!(grid.text(), magnet.text());
    }

    /// The fade scales the colour's opacity and leaves a solid colour alone.
    #[test]
    fn the_fill_fades_the_colour() {
        let solid = with_alpha(Color32::from_rgb(200, 100, 50), 1.0);
        assert_eq!(solid, Color32::from_rgb(200, 100, 50));
        let faded = with_alpha(Color32::from_rgb(200, 100, 50), 0.5);
        assert!(
            (faded.a() as i32 - 128).abs() <= 1,
            "half opacity gave {}",
            faded.a()
        );
        assert_ne!(faded, solid);
    }

    /// Every codepoint a font maps, read from its `cmap` table: the format 4
    /// and format 12 subtables a TrueType font carries.
    fn codepoints(font: &[u8]) -> std::collections::HashSet<u32> {
        let mut found = std::collections::HashSet::new();
        let table_count = read_u16(font, 4) as usize;
        for index in 0..table_count {
            let entry = 12 + index * 16;
            if &font[entry..entry + 4] != b"cmap" {
                continue;
            }
            let cmap = read_u32(font, entry + 8) as usize;
            for subtable in 0..read_u16(font, cmap + 2) as usize {
                let record = cmap + 4 + subtable * 8;
                let offset = cmap + read_u32(font, record + 4) as usize;
                match read_u16(font, offset) {
                    4 => read_format_4(font, offset, &mut found),
                    12 => read_format_12(font, offset, &mut found),
                    _ => {}
                }
            }
        }
        found
    }

    /// The `cmap` format 4 segments: a run of codepoints per segment.
    fn read_format_4(font: &[u8], offset: usize, found: &mut std::collections::HashSet<u32>) {
        let segments = read_u16(font, offset + 6) as usize / 2;
        let ends = offset + 14;
        let starts = ends + segments * 2 + 2;
        for segment in 0..segments {
            let end = read_u16(font, ends + segment * 2);
            let start = read_u16(font, starts + segment * 2);
            found.extend((start..=end).map(u32::from));
        }
    }

    /// The `cmap` format 12 groups: a run of codepoints per group.
    fn read_format_12(font: &[u8], offset: usize, found: &mut std::collections::HashSet<u32>) {
        for group in 0..read_u32(font, offset + 12) as usize {
            let entry = offset + 16 + group * 12;
            let start = read_u32(font, entry);
            let end = read_u32(font, entry + 4);
            found.extend(start..=end);
        }
    }

    fn read_u16(font: &[u8], offset: usize) -> u16 {
        u16::from_be_bytes([font[offset], font[offset + 1]])
    }

    fn read_u32(font: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes([
            font[offset],
            font[offset + 1],
            font[offset + 2],
            font[offset + 3],
        ])
    }
}
